//! `INSERT ... SELECT` の内側 SELECT の `LIMIT` 省略・自己参照・PostgreSQL 照合（SQL-51・SQL-34。Issue #2615）。
//!
//! `LIMIT` 省略時の入力行数の上限は `--max-result-rows`（`EngineCore::with_result_limits`）で、超過は
//! `54000`（1 行も挿入しない）。自己参照の `INSERT INTO t SELECT ... FROM t` は文の開始時点の行だけを読む。
//! PG 照合の期待値は PostgreSQL 17 の挙動を固定値で書く（CI に実 PG は無い）。`ORDER BY` 無しの
//! `LIMIT` は PG でも選ばれる行が未規定なので件数だけを照合する。ポインタ: `docs/design/insert-select.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str, max_rows: Option<usize>) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(n) = max_rows {
        core = core.with_result_limits(ResultLimits::new(n));
    }
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE src (id BIGINT PRIMARY KEY, a INTEGER, t TEXT)",
        "CREATE TABLE dst (id BIGINT PRIMARY KEY, a BIGINT, t TEXT)",
        "CREATE TABLE self_t (id BIGINT PRIMARY KEY, a INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    (core, path)
}

fn seed_src(core: &EngineCore, tenant: &str, ids: std::ops::RangeInclusive<i64>) {
    let values: Vec<String> = ids.map(|i| format!("({i}, {}, 't{i}')", i * 10)).collect();
    core.execute_insert_sql(
        &ctx(tenant),
        &format!("INSERT INTO src (id, a, t) VALUES {}", values.join(", ")),
    )
    .expect("seed");
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn code(r: Result<SqlOutcome, SqlSurfaceError>) -> String {
    match r {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn affected(r: Result<SqlOutcome, SqlSurfaceError>) -> u64 {
    match r.expect("insert select") {
        SqlOutcome::Insert(o) => o.rows_affected,
        SqlOutcome::Returning(o) => o.rows_affected,
        other => panic!("expected Insert, got {other:?}"),
    }
}

/// SELECT の結果行を `Debug` 文字列で返す。
fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match run(core, tenant, sql).expect("select") {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn count(core: &EngineCore, tenant: &str, table: &str) -> usize {
    count_lim(core, tenant, table, 1000)
}

/// 結果行数の上限を絞った設定でも数えられるよう `LIMIT` を指定する版。
fn count_lim(core: &EngineCore, tenant: &str, table: &str, lim: usize) -> usize {
    rows(
        core,
        tenant,
        &format!("SELECT id FROM {table} ORDER BY id LIMIT {lim}"),
    )
    .len()
}

fn clear_dst(core: &EngineCore) {
    run(core, "alice", "DELETE FROM dst WHERE id > 0").expect("clear");
}

#[test]
fn omitted_limit_is_accepted_for_row_returning_shapes() {
    let (core, path) = open("sql51-lo-shapes", None);
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=3);
    let cases = [
        ("INSERT INTO dst (id, a, t) SELECT id, a, t FROM src", 3),
        (
            "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src WHERE a > 10",
            2,
        ),
        (
            "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src ORDER BY a",
            3,
        ),
        ("INSERT INTO dst SELECT id, a, t FROM src", 3),
    ];
    for (sql, n) in cases {
        assert_eq!(affected(run(&core, "alice", sql)), n, "{sql}");
        clear_dst(&core);
        assert_eq!(count(&core, "alice", "dst"), 0, "{sql}");
    }
}

#[test]
fn omitted_limit_returning_and_operation_id() {
    let (core, path) = open("sql51-lo-ret", None);
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=2);
    let same = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src RETURNING id, a \
                USING OPERATION_ID 'r-1'";
    match run(&core, "alice", same).expect("returning") {
        SqlOutcome::Returning(r) => {
            assert_eq!(r.rows_affected, 2);
            assert_eq!(r.result.rows.len(), 2);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
    // 同一文の再送は 23505、LIMIT を明示した別の文に同じ operation_id を使うと 22023。
    assert_eq!(code(run(&core, "alice", same)), "23505");
    let explicit = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 \
                    RETURNING id, a USING OPERATION_ID 'r-1'";
    assert_eq!(code(run(&core, "alice", explicit)), "22023");
    assert_eq!(count(&core, "alice", "dst"), 2);
}

#[test]
fn row_cap_boundary_and_no_partial_insert() {
    let (core, path) = open("sql51-lo-cap", Some(5));
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=5);
    let sql = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src";
    assert_eq!(affected(run(&core, "alice", sql)), 5);
    clear_dst(&core);
    seed_src(&core, "alice", 6..=6);
    assert_eq!(code(run(&core, "alice", sql)), "54000");
    assert_eq!(count_lim(&core, "alice", "dst", 5), 0);
    // 超過した文は台帳に残らない（条件を満たした後の同じ operation_id の再実行が成功する）。
    let with_op = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src USING OPERATION_ID 'cap-1'";
    assert_eq!(code(run(&core, "alice", with_op)), "54000");
    run(&core, "alice", "DELETE FROM src WHERE id = 6").expect("shrink");
    assert_eq!(affected(run(&core, "alice", with_op)), 5);
}

#[test]
fn row_cap_counts_only_visible_rows() {
    let (core, path) = open("sql51-lo-rls", Some(5));
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=3);
    core.execute_insert_sql(
        &ctx("bob"),
        "INSERT INTO src (id, a, t) VALUES (101, 1, 'b1'), (102, 1, 'b2'), (103, 1, 'b3'), \
         (104, 1, 'b4')",
    )
    .expect("seed bob");
    let sql = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src";
    assert_eq!(affected(run(&core, "alice", sql)), 3);
}

#[test]
fn explicit_limit_is_unchanged() {
    let (core, path) = open("sql51-lo-explicit", Some(5));
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=5);
    let over = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 6";
    assert_eq!(code(run(&core, "alice", over)), "22000");
    let three = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src ORDER BY a LIMIT 3";
    assert_eq!(affected(run(&core, "alice", three)), 3);
    assert_eq!(count_lim(&core, "alice", "dst", 5), 3);
}

#[test]
fn describe_of_omitted_limit_does_not_hit_range_error() {
    let (core, path) = open("sql51-lo-describe", Some(5));
    let _g = CleanupGuard(path);
    for sql in [
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src",
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src RETURNING id, a",
    ] {
        let parsed = core.parse_sql(sql).expect("parse");
        let s = SessionState::default();
        core.describe_parsed_in_session(&s, &parsed)
            .unwrap_or_else(|e| panic!("describe {sql}: {e:?}"));
    }
}

#[test]
fn self_reference_reads_statement_snapshot_only() {
    let (core, path) = open("sql51-lo-self", None);
    let _g = CleanupGuard(path);
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO self_t (id, a) VALUES (1, 10), (2, 20)",
    )
    .expect("seed");
    // LIMIT 省略形・明示形のどちらも、挿入した行を再び読まない（件数はちょうど 2 倍）。
    let omitted = "INSERT INTO self_t (id, a) SELECT id + 100, a FROM self_t";
    assert_eq!(affected(run(&core, "alice", omitted)), 2);
    assert_eq!(count(&core, "alice", "self_t"), 4);
    let explicit = "INSERT INTO self_t (id, a) SELECT id + 1000, a FROM self_t LIMIT 100";
    assert_eq!(affected(run(&core, "alice", explicit)), 4);
    assert_eq!(count(&core, "alice", "self_t"), 8);
}

#[test]
fn self_reference_in_explicit_transaction() {
    let (core, path) = open("sql51-lo-self-txn", None);
    let _g = CleanupGuard(path);
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO self_t (id, a) VALUES (1, 10), (2, 20)",
    )
    .expect("seed");
    let sql = "INSERT INTO self_t (id, a) SELECT id + 100, a FROM self_t";
    let mut txn = core.new_session_transaction();
    let mut ts = SessionState::default();
    let c = ctx("alice");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin");
    match core
        .execute_sql_in_txn(&c, &mut ts, &mut txn, sql)
        .expect("self insert")
    {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected Insert, got {other:?}"),
    }
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, "alice", "self_t"), 2);
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin 2");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, sql)
        .expect("self insert 2");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count(&core, "alice", "self_t"), 4);
}

#[test]
fn pg_parity_full_copy_order_limit_and_empty() {
    let (core, path) = open("sql51-lo-pg", None);
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=4);
    // 全件コピー: 件数と行の中身が一致する。
    let all = "INSERT INTO dst SELECT id, a, t FROM src";
    assert_eq!(affected(run(&core, "alice", all)), 4);
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT id, a, t FROM dst ORDER BY id LIMIT 100"
        ),
        rows(
            &core,
            "alice",
            "SELECT id, a, t FROM src ORDER BY id LIMIT 100"
        )
    );
    clear_dst(&core);
    // ORDER BY a LIMIT 2 は a の小さい 2 行（id 1, 2）。
    let ordered = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src ORDER BY a LIMIT 2";
    assert_eq!(affected(run(&core, "alice", ordered)), 2);
    let got = rows(&core, "alice", "SELECT id FROM dst ORDER BY id LIMIT 100");
    let want = rows(
        &core,
        "alice",
        "SELECT id FROM src WHERE id <= 2 ORDER BY id LIMIT 100",
    );
    assert_eq!(got, want);
    clear_dst(&core);
    // ORDER BY 無しの LIMIT 2 は件数だけを照合する。
    let limited = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 2";
    assert_eq!(affected(run(&core, "alice", limited)), 2);
    // 0 行の SELECT は INSERT 0 0 相当。
    let none = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src WHERE a > 100000";
    assert_eq!(affected(run(&core, "alice", none)), 0);
}

#[test]
fn pg_parity_on_conflict_with_omitted_limit() {
    let (core, path) = open("sql51-lo-oc", None);
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=3);
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO dst (id, a, t) VALUES (2, 999, 'old')",
    )
    .expect("seed dst");
    // DO NOTHING: 衝突行 (id 2) を飛ばして 2 件。
    let nothing = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src ON CONFLICT DO NOTHING";
    assert_eq!(affected(run(&core, "alice", nothing)), 2);
    assert_eq!(count_lim(&core, "alice", "dst", 5), 3);
    // DO UPDATE: 衝突行が EXCLUDED の値（a = 20）で更新される。
    let update = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src \
                  ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a";
    assert_eq!(affected(run(&core, "alice", update)), 3);
    assert_eq!(
        rows(&core, "alice", "SELECT a FROM dst WHERE id = 2 LIMIT 1"),
        rows(&core, "alice", "SELECT a FROM src WHERE id = 2 LIMIT 1")
    );
}

#[test]
fn pg_parity_self_reference_conflict() {
    let (core, path) = open("sql51-lo-self-oc", None);
    let _g = CleanupGuard(path);
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO self_t (id, a) VALUES (1, 10), (2, 20)",
    )
    .expect("seed");
    // ON CONFLICT 無しの自己コピーは主キー衝突で 23505、1 行も増えない。
    let plain = "INSERT INTO self_t SELECT id, a FROM self_t";
    assert_eq!(code(run(&core, "alice", plain)), "23505");
    assert_eq!(count(&core, "alice", "self_t"), 2);
    // DO NOTHING なら 0 件（PG と同じ）。
    let nothing = "INSERT INTO self_t SELECT id, a FROM self_t ON CONFLICT DO NOTHING";
    assert_eq!(affected(run(&core, "alice", nothing)), 0);
    assert_eq!(count(&core, "alice", "self_t"), 2);
}

// 対象ビヘイビア: SQL-34・SQL-51（Issue #2212）。`--max-result-rows` を変えると入力側の境界も動く。
#[test]
fn row_cap_follows_the_setting() {
    let sql = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src";
    for cap in [3usize, 6] {
        let (core, path) = open(&format!("sql51-lo-follow-{cap}"), Some(cap));
        let _g = CleanupGuard(path);
        seed_src(&core, "alice", 1..=cap as i64);
        assert_eq!(affected(run(&core, "alice", sql)), cap as u64);
        clear_dst(&core);
        seed_src(&core, "alice", (cap as i64 + 1)..=(cap as i64 + 1));
        assert_eq!(code(run(&core, "alice", sql)), "54000");
        assert_eq!(count_lim(&core, "alice", "dst", cap), 0);
    }
}

// 対象ビヘイビア: SQL-34・SQL-51（Issue #2212）。上限をハード上限にしても少量の入力は成功する。
#[test]
fn hard_cap_setting_still_inserts_small_input() {
    let (core, path) = open("sql51-lo-hard", Some(usize::MAX));
    let _g = CleanupGuard(path);
    seed_src(&core, "alice", 1..=4);
    let sql = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src";
    assert_eq!(affected(run(&core, "alice", sql)), 4);
}
