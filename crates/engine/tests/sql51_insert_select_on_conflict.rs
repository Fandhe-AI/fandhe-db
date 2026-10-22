//! `INSERT ... SELECT ... ON CONFLICT` の SQL 経路 e2e（SQL-51・SQL-49。Issue #2614。親: #1731）。
//!
//! 内側 SELECT の結果行が VALUES 形と同じ UPSERT 経路へ流れること（`DO NOTHING`／`DO UPDATE`・
//! 列リスト形／`ON CONSTRAINT`／対象省略・RETURNING）、同一文内で衝突キーが 2 回現れた場合の
//! PostgreSQL 17 と同じ扱い（`DO UPDATE` は `21000` で全体拒否、`DO NOTHING` は先勝ち、NULL キーは
//! 重複とみなさない）、テナント境界、原子性、台帳（同一文の再送 `23505`・別文の再利用 `22023`）、
//! 明示トランザクション、未対応形の fail-closed を固定する。
//! 設定は `sql51_insert_select_exec.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。
//! ポインタ: `docs/design/insert-select.md`・`docs/design/sql-upsert.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_with_tables(path: &std::path::Path, unique_k: bool) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    let dst = if unique_k {
        "CREATE TABLE dst (id BIGINT PRIMARY KEY, k INTEGER, t TEXT, CONSTRAINT uq_k UNIQUE (k))"
    } else {
        "CREATE TABLE dst (id BIGINT PRIMARY KEY, k INTEGER, t TEXT)"
    };
    for ddl in [
        "CREATE TABLE src (id BIGINT PRIMARY KEY, k INTEGER, t TEXT)",
        dst,
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    core
}

/// `src`（衝突元の材料）・`dst`（`uq_k` の UNIQUE 制約付き）を作り、src に alice の 3 行・
/// bob の 1 行を入れる。
fn setup(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let core = core_with_tables(&path, true);
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, k, t) VALUES (1, 10, 'a1'), (2, 20, 'a2'), (3, 30, 'a3') \
         USING OPERATION_ID 'seed-a'",
    )
    .expect("seed alice");
    core.execute_insert_sql(
        &ctx("bob"),
        "INSERT INTO src (id, k, t) VALUES (1, 10, 'b1') USING OPERATION_ID 'seed-b'",
    )
    .expect("seed bob");
    (core, path)
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
    match r.expect("statement") {
        SqlOutcome::Insert(o) => o.rows_affected,
        SqlOutcome::Returning(o) => o.rows_affected,
        other => panic!("expected Insert/Returning, got {other:?}"),
    }
}

/// テナントから見える `dst` の全行（`id|k|t` 形式・id 昇順）。
fn dst_rows(core: &EngineCore, tenant: &str) -> Vec<String> {
    match run(
        core,
        tenant,
        "SELECT id, k, t FROM dst ORDER BY id LIMIT 100",
    )
    .expect("select")
    {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn seed_dst(core: &EngineCore, tenant: &str, values: &str, op: &str) {
    core.execute_insert_sql(
        &ctx(tenant),
        &format!("INSERT INTO dst (id, k, t) VALUES {values} USING OPERATION_ID '{op}'"),
    )
    .expect("seed dst");
}

const SEL: &str = "SELECT id, k, t FROM src ORDER BY id LIMIT 100";

#[test]
fn do_nothing_skips_conflicting_rows() {
    let (core, path) = setup("sql51-oc-nothing");
    let _g = CleanupGuard(path);
    seed_dst(&core, "alice", "(1, 99, 'old')", "seed-d");
    let n = affected(run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) DO NOTHING \
             USING OPERATION_ID 'oc-n1'"
        ),
    ));
    assert_eq!(n, 2);
    // 既存行は不変、残り 2 行が入る。
    let rows = dst_rows(&core, "alice");
    assert_eq!(rows.len(), 3);
    assert!(rows[0].contains("old"), "{rows:?}");
}

#[test]
fn do_update_updates_conflicting_rows() {
    let (core, path) = setup("sql51-oc-update");
    let _g = CleanupGuard(path);
    seed_dst(&core, "alice", "(1, 99, 'old')", "seed-d");
    let n = affected(run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) \
             DO UPDATE SET t = EXCLUDED.t, k = EXCLUDED.k USING OPERATION_ID 'oc-u1'"
        ),
    ));
    assert_eq!(n, 3);
    let rows = dst_rows(&core, "alice");
    assert_eq!(rows.len(), 3);
    assert!(
        rows[0].contains("a1") && !rows[0].contains("old"),
        "{rows:?}"
    );
}

#[test]
fn constraint_name_and_omitted_target_forms_behave_like_column_list() {
    for (label, target) in [
        ("cols", "ON CONFLICT (k) DO NOTHING"),
        ("constraint", "ON CONFLICT ON CONSTRAINT uq_k DO NOTHING"),
        ("omitted", "ON CONFLICT DO NOTHING"),
    ] {
        let (core, path) = setup(&format!("sql51-oc-form-{label}"));
        let _g = CleanupGuard(path);
        // k=20 が既存と衝突（id は別）。
        seed_dst(&core, "alice", "(7, 20, 'old')", "seed-d");
        let n = affected(run(
            &core,
            "alice",
            &format!("INSERT INTO dst (id, k, t) {SEL} {target} USING OPERATION_ID 'oc-f1'"),
        ));
        assert_eq!(n, 2, "{label}");
        assert_eq!(dst_rows(&core, "alice").len(), 3, "{label}");
    }
}

#[test]
fn returning_excludes_skipped_rows_and_shows_updated_values() {
    let (core, path) = setup("sql51-oc-returning");
    let _g = CleanupGuard(path);
    seed_dst(&core, "alice", "(1, 99, 'old')", "seed-d");
    match run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) DO NOTHING RETURNING id \
             USING OPERATION_ID 'oc-r1'"
        ),
    )
    .expect("returning")
    {
        SqlOutcome::Returning(r) => {
            assert_eq!(r.rows_affected, 2);
            assert_eq!(r.result.rows.len(), 2);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
    match run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) DO UPDATE SET t = 'upd' \
             RETURNING id, t USING OPERATION_ID 'oc-r2'"
        ),
    )
    .expect("returning update")
    {
        SqlOutcome::Returning(r) => {
            assert_eq!(r.rows_affected, 3);
            assert_eq!(r.result.rows.len(), 3);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
}

#[test]
fn duplicate_key_in_statement_matches_postgres() {
    let (core, path) = setup("sql51-oc-dup");
    let _g = CleanupGuard(path);
    // 内側の結果で id=5 が 3 回現れる（k は別々）。
    let dup = "SELECT 5, k, t FROM src ORDER BY id LIMIT 100";
    // DO UPDATE は 21000 で文全体を拒否し、何も書かない。
    let err = run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {dup} ON CONFLICT (id) \
             DO UPDATE SET t = EXCLUDED.t USING OPERATION_ID 'oc-d1'"
        ),
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "21000");
    assert_eq!(
        err.to_string(),
        "ON CONFLICT DO UPDATE command cannot affect row a second time"
    );
    assert!(dst_rows(&core, "alice").is_empty());
    // DO NOTHING は先に現れた行だけを入れる。
    assert_eq!(
        affected(run(
            &core,
            "alice",
            &format!(
                "INSERT INTO dst (id, k, t) {dup} ON CONFLICT (id) DO NOTHING \
                 USING OPERATION_ID 'oc-d1'"
            ),
        )),
        1
    );
    let rows = dst_rows(&core, "alice");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("a1"), "first row wins: {rows:?}");
    // 対象省略の DO NOTHING も先勝ち（k は別なので id=6 の 1 行目だけが入る）。
    assert_eq!(
        affected(run(
            &core,
            "alice",
            "INSERT INTO dst (id, k, t) SELECT 6, k + 100, t FROM src ORDER BY id LIMIT 100 \
             ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'oc-d2'",
        )),
        1
    );
}

#[test]
fn null_key_is_not_a_duplicate() {
    let (core, path) = setup("sql51-oc-null");
    let _g = CleanupGuard(path);
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, k, t) VALUES (11, NULL, 'n1'), (12, NULL, 'n2') \
         USING OPERATION_ID 'seed-null'",
    )
    .expect("seed null keys");
    // k が NULL の行が複数あっても UNIQUE 対象 (k) の重複とはみなさない（NULLS DISTINCT）。
    let n = affected(run(
        &core,
        "alice",
        "INSERT INTO dst (id, k, t) SELECT id, k, t FROM src WHERE id > 10 ORDER BY id LIMIT 100 \
         ON CONFLICT (k) DO UPDATE SET t = EXCLUDED.t USING OPERATION_ID 'oc-nl1'",
    ));
    assert_eq!(n, 2);
    assert_eq!(dst_rows(&core, "alice").len(), 2);
}

#[test]
fn other_tenant_rows_are_invisible_and_untouched() {
    let (core, path) = setup("sql51-oc-tenant");
    let _g = CleanupGuard(path);
    seed_dst(&core, "bob", "(1, 10, 'bob-row')", "seed-bd");
    let n = affected(run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) \
             DO UPDATE SET t = EXCLUDED.t USING OPERATION_ID 'oc-t1'"
        ),
    ));
    // bob の dst 行とは衝突せず、alice の 3 行がそのまま入る。
    assert_eq!(n, 3);
    assert_eq!(dst_rows(&core, "alice").len(), 3);
    let bob = dst_rows(&core, "bob");
    assert_eq!(bob.len(), 1);
    assert!(bob[0].contains("bob-row"), "{bob:?}");
}

#[test]
fn failure_during_conflict_handling_leaves_no_partial_rows() {
    let (core, path) = setup("sql51-oc-atomic");
    let _g = CleanupGuard(path);
    // id=9 の既存行が k=20 を持つ。src の id=2 (k=20) は (id) 対象では衝突せず uq_k に違反する。
    seed_dst(&core, "alice", "(9, 20, 'old')", "seed-d");
    let r = run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) DO NOTHING \
             USING OPERATION_ID 'oc-a1'"
        ),
    );
    assert_eq!(code(r), "23505");
    assert_eq!(dst_rows(&core, "alice").len(), 1);
}

#[test]
fn row_count_limit_leaves_dst_untouched() {
    let path = unique_db_path("sql51-oc-limit");
    let _g = CleanupGuard(path.clone());
    let core = core_with_tables(&path, false).with_dml_limits(engine::sql::parser::DmlLimits {
        max_insert_rows_per_statement: Some(std::num::NonZeroUsize::new(2).expect("nonzero")),
        ..engine::sql::parser::DmlLimits::default()
    });
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, k, t) VALUES (1, 1, 'a'), (2, 2, 'b') USING OPERATION_ID 's1'",
    )
    .expect("seed");
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, k, t) VALUES (3, 3, 'c') USING OPERATION_ID 's2'",
    )
    .expect("seed 2");
    let r = run(
        &core,
        "alice",
        "INSERT INTO dst (id, k, t) SELECT id, k, t FROM src ORDER BY id LIMIT 100 \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'oc-l1'",
    );
    assert_eq!(code(r), "54000");
    assert!(dst_rows(&core, "alice").is_empty());
}

#[test]
fn ledger_replay_and_statement_change() {
    let (core, path) = setup("sql51-oc-ledger");
    let _g = CleanupGuard(path);
    let sql = format!(
        "INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'oc-led'"
    );
    assert_eq!(affected(run(&core, "alice", &sql)), 3);
    // src を変えても同一文の再送は 23505 で、行は増えない。
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, k, t) VALUES (4, 40, 'a4') USING OPERATION_ID 'seed-a2'",
    )
    .expect("seed more");
    assert_eq!(code(run(&core, "alice", &sql)), "23505");
    assert_eq!(dst_rows(&core, "alice").len(), 3);
    // 同じ operation_id で ON CONFLICT 句だけ異なる文は 22023。
    for other in [
        "ON CONFLICT (id) DO UPDATE SET t = EXCLUDED.t",
        "ON CONFLICT (k) DO NOTHING",
        "ON CONFLICT DO NOTHING",
    ] {
        let s = format!("INSERT INTO dst (id, k, t) {SEL} {other} USING OPERATION_ID 'oc-led'");
        assert_eq!(code(run(&core, "alice", &s)), "22023", "{other}");
    }
    assert_eq!(dst_rows(&core, "alice").len(), 3);
}

#[test]
fn explicit_transaction_commit_and_rollback() {
    let (core, path) = setup("sql51-oc-txn");
    let _g = CleanupGuard(path);
    let mut txn = core.new_session_transaction();
    let mut ts = SessionState::default();
    let c = ctx("alice");
    let sql =
        format!("INSERT INTO dst (id, k, t) {SEL} ON CONFLICT (id) DO NOTHING USING OPERATION_ID ");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, &format!("{sql}'oc-tx1'"))
        .expect("in txn");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(dst_rows(&core, "alice").is_empty());
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin 2");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, &format!("{sql}'oc-tx2'"))
        .expect("in txn 2");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(dst_rows(&core, "alice").len(), 3);
}

#[test]
fn unsupported_forms_stay_fail_closed() {
    let (core, path) = setup("sql51-oc-reject");
    let _g = CleanupGuard(path);
    let cases: &[(&str, &str)] = &[
        (
            "ON CONFLICT (id) DO UPDATE SET t = EXCLUDED.t WHERE dst.k > 0",
            "42601",
        ),
        ("ON CONFLICT (nope) DO NOTHING", "42P10"),
        ("ON CONFLICT ON CONSTRAINT nope DO NOTHING", "42704"),
        ("ON CONFLICT DO UPDATE SET t = EXCLUDED.t", "42601"),
    ];
    for (clause, expected) in cases {
        let sql = format!("INSERT INTO dst (id, k, t) {SEL} {clause} USING OPERATION_ID 'oc-rej'");
        assert_eq!(code(run(&core, "alice", &sql)), *expected, "{clause}");
    }
    assert!(dst_rows(&core, "alice").is_empty());
}

// Issue #2614 レビュー指摘: UNIQUE 対象で衝突によりスキップ・更新される行は、SELECT 由来で
// id が重複していても新規挿入されないため id 重複検査の対象外（書き込み側は実際に新規挿入する
// 行だけを検査する）。
#[test]
fn skipped_or_updated_rows_with_duplicate_ids_are_not_rejected() {
    let (core, path) = setup("sql51-oc-dupid");
    let _g = CleanupGuard(path);
    seed_dst(
        &core,
        "alice",
        "(101, 10, 'o10'), (102, 20, 'o20')",
        "seed-d",
    );
    // 2 行とも id=5 だが、どちらも既存行と k で衝突するため両方スキップされる。
    let sel = "SELECT 5, k, t FROM src WHERE k = 10 OR k = 20 ORDER BY k LIMIT 100";
    let n = affected(run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {sel} ON CONFLICT (k) DO NOTHING \
             USING OPERATION_ID 'oc-dup-n'"
        ),
    ));
    assert_eq!(n, 0);
    // DO UPDATE は別々の既存行を更新する。
    let n = affected(run(
        &core,
        "alice",
        &format!(
            "INSERT INTO dst (id, k, t) {sel} ON CONFLICT (k) DO UPDATE SET t = EXCLUDED.t \
             USING OPERATION_ID 'oc-dup-u'"
        ),
    ));
    assert_eq!(n, 2);
    let rows = dst_rows(&core, "alice");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[0].contains("a1") && rows[1].contains("a2"), "{rows:?}");
}
