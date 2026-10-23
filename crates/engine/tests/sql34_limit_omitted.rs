//! `LIMIT` 省略・`LIMIT ALL` の受理と結果行数上限（SQL-34。Issue #1796）の結合テスト。
//!
//! 行を返す SELECT は `LIMIT` を省略でき、`LIMIT ALL` と同じく件数の制限なしとして扱う。
//! 安全弁は結果行数の上限（`EngineCore::with_result_limits`。CLI の `--max-result-rows` 相当）で、
//! 超過は黙って切り詰めず `54000` で文全体を拒否する。ここでは cap を小さく注入して境界を固定する。
//! 距離 ORDER BY・サブクエリ内側の省略は従来どおり `42601`（#2242／#1852／#1853 の範囲）。
//! cap + 1 件での走査の早期中断・wire の分割送出は #1797／#1798 の範囲でここでは扱わない。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const CAP: usize = 3;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(CAP));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, k INTEGER, w INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    (core, CleanupGuard(path))
}

/// `t` に `ids` の行を入れる（`k = id`・`v = id * 10`。`k` は全行で異なる）。
fn seed_t(core: &EngineCore, tenant: &str, ids: std::ops::RangeInclusive<i64>) {
    let values: Vec<String> = ids.map(|i| format!("({i}, {i}, {})", i * 10)).collect();
    core.execute_insert_sql(
        &ctx(tenant),
        &format!("INSERT INTO t (id, k, v) VALUES {}", values.join(", ")),
    )
    .expect("seed t");
}

fn seed_u(core: &EngineCore, tenant: &str, ids: std::ops::RangeInclusive<i64>) {
    let values: Vec<String> = ids.map(|i| format!("({i}, {i}, {})", i * 100)).collect();
    core.execute_insert_sql(
        &ctx(tenant),
        &format!("INSERT INTO u (id, k, w) VALUES {}", values.join(", ")),
    )
    .expect("seed u");
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), &mut SessionState::default(), sql)
}

fn rows(core: &EngineCore, sql: &str) -> usize {
    match run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, sql) {
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 広域取得: 省略・`LIMIT ALL` は cap ちょうどで成功、cap + 1 で `54000`。
#[test]
fn scan_omitted_and_all_enforce_the_cap_boundary() {
    let (core, _g) = open("sql34-scan");
    seed_t(&core, "alice", 1..=3);
    for sql in [
        "SELECT * FROM t",
        "SELECT * FROM t LIMIT ALL",
        "SELECT * FROM t limit all",
        "SELECT * FROM t ORDER BY v",
        "SELECT * FROM t ORDER BY v DESC LIMIT ALL",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t",
    ] {
        assert_eq!(rows(&core, sql), CAP, "{sql}");
    }
    seed_t(&core, "alice", 4..=4);
    for sql in [
        "SELECT * FROM t",
        "SELECT * FROM t LIMIT ALL",
        "SELECT * FROM t ORDER BY v",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t",
    ] {
        assert_eq!(code(&core, sql), "54000", "{sql}");
    }
}

/// OFFSET は適用後の件数で数える（OFFSET 単独・`LIMIT ALL OFFSET`）。
#[test]
fn offset_is_applied_before_counting() {
    let (core, _g) = open("sql34-offset");
    seed_t(&core, "alice", 1..=4);
    assert_eq!(rows(&core, "SELECT * FROM t OFFSET 1"), CAP);
    assert_eq!(rows(&core, "SELECT * FROM t LIMIT ALL OFFSET 2"), 2);
    assert_eq!(rows(&core, "SELECT * FROM t ORDER BY v OFFSET 4"), 0);
    assert_eq!(code(&core, "SELECT * FROM t OFFSET 0"), "54000");
}

/// 明示 `LIMIT n` の契約は不変（`n > cap` と `LIMIT 0` は `22000`）。
#[test]
fn explicit_limit_contract_is_unchanged() {
    let (core, _g) = open("sql34-explicit");
    seed_t(&core, "alice", 1..=2);
    assert_eq!(rows(&core, "SELECT * FROM t LIMIT 2"), 2);
    assert_eq!(code(&core, "SELECT * FROM t LIMIT 4"), "22000");
    assert_eq!(code(&core, "SELECT * FROM t LIMIT 0"), "22000");
}

/// 他テナントの行は数えない（cap を超えて存在しても可視行が cap 以内なら成功）。
#[test]
fn invisible_rows_are_not_counted() {
    let (core, _g) = open("sql34-rls");
    seed_t(&core, "alice", 1..=2);
    seed_t(&core, "bob", 11..=20);
    assert_eq!(rows(&core, "SELECT * FROM t"), 2);
    assert_eq!(rows(&core, "SELECT * FROM t LIMIT ALL"), 2);
}

/// GROUP BY・DISTINCT: HAVING と OFFSET の適用後の件数で判定する。
#[test]
fn group_by_counts_rows_after_having_and_offset() {
    let (core, _g) = open("sql34-group");
    seed_t(&core, "alice", 1..=5);
    let base = "SELECT k, SUM(v) AS s FROM t GROUP BY k";
    assert_eq!(code(&core, base), "54000");
    assert_eq!(code(&core, &format!("{base} LIMIT ALL")), "54000");
    // HAVING で最終的に cap 以内になるなら成功（HAVING 前の件数では拒否しない）。
    assert_eq!(rows(&core, &format!("{base} HAVING s <= 30")), CAP);
    // OFFSET 適用後の件数で数える。
    assert_eq!(rows(&core, &format!("{base} OFFSET 2")), CAP);
    assert_eq!(rows(&core, &format!("{base} LIMIT ALL OFFSET 2")), CAP);
    assert_eq!(code(&core, &format!("{base} OFFSET 1")), "54000");
    // 単一行の集計は対象外。
    assert_eq!(rows(&core, "SELECT COUNT(*) FROM t"), 1);
    // DISTINCT。
    assert_eq!(code(&core, "SELECT DISTINCT k FROM t"), "54000");
    assert_eq!(code(&core, "SELECT DISTINCT k FROM t LIMIT ALL"), "54000");
    assert_eq!(rows(&core, "SELECT DISTINCT k FROM t LIMIT 3"), 3);
}

/// JOIN（非集計・集計）の省略・`LIMIT ALL`。
#[test]
fn join_omitted_and_all_enforce_the_cap() {
    let (core, _g) = open("sql34-join");
    seed_t(&core, "alice", 1..=3);
    seed_u(&core, "alice", 1..=3);
    let plain = "SELECT t.id, u.w FROM t JOIN u ON t.k = u.k";
    assert_eq!(rows(&core, plain), CAP);
    assert_eq!(rows(&core, &format!("{plain} LIMIT ALL")), CAP);
    assert_eq!(rows(&core, &format!("{plain} ORDER BY t.id OFFSET 1")), 2);
    let agg = "SELECT t.k, COUNT(*) FROM t JOIN u ON t.k = u.k GROUP BY t.k";
    assert_eq!(rows(&core, agg), CAP);
    assert_eq!(rows(&core, &format!("{agg} LIMIT ALL")), CAP);
    seed_t(&core, "alice", 4..=4);
    seed_u(&core, "alice", 4..=4);
    assert_eq!(code(&core, plain), "54000");
    assert_eq!(code(&core, &format!("{plain} LIMIT ALL")), "54000");
    assert_eq!(code(&core, agg), "54000");
    assert_eq!(code(&core, &format!("{agg} LIMIT ALL")), "54000");
    assert_eq!(rows(&core, &format!("{plain} LIMIT 2")), 2);
}

/// 集合演算全体の `LIMIT ALL` と省略。
#[test]
fn set_operation_all_enforces_the_cap() {
    let (core, _g) = open("sql34-setop");
    seed_t(&core, "alice", 1..=2);
    seed_u(&core, "alice", 3..=4);
    let sql = "SELECT k FROM t UNION ALL SELECT k FROM u";
    assert_eq!(code(&core, sql), "54000");
    assert_eq!(code(&core, &format!("{sql} LIMIT ALL")), "54000");
    assert_eq!(rows(&core, &format!("{sql} LIMIT 3")), 3);
    assert_eq!(
        rows(&core, "SELECT k FROM t UNION SELECT k FROM t LIMIT ALL"),
        2
    );
}

/// ビュー畳み込み・CTE の主クエリは広域取得と同じ経路で上限が効く。
#[test]
fn view_and_cte_share_the_scan_path() {
    let (core, _g) = open("sql34-view");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE VIEW tv AS SELECT * FROM t",
    )
    .expect("create view");
    seed_t(&core, "alice", 1..=3);
    assert_eq!(rows(&core, "SELECT * FROM tv"), CAP);
    assert_eq!(
        rows(&core, "WITH c AS (SELECT id, k FROM t) SELECT * FROM c"),
        CAP
    );
    seed_t(&core, "alice", 4..=4);
    assert_eq!(code(&core, "SELECT * FROM tv"), "54000");
    assert_eq!(code(&core, "SELECT * FROM tv LIMIT ALL"), "54000");
    assert_eq!(
        code(&core, "WITH c AS (SELECT id, k FROM t) SELECT * FROM c"),
        "54000"
    );
    assert_eq!(rows(&core, "SELECT * FROM tv LIMIT 2"), 2);
}

/// 評価後射影形ビュー（本文が LIMIT 付き）の外側の省略・`LIMIT ALL`。
#[test]
fn buffered_view_outer_omitted_enforces_the_cap() {
    let (core, _g) = open("sql34-buffered");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE VIEW bv AS SELECT id, k FROM t ORDER BY id LIMIT 3",
    )
    .expect("create buffered view");
    seed_t(&core, "alice", 1..=5);
    // 本文が 3 行に絞るので cap ちょうど。
    assert_eq!(rows(&core, "SELECT * FROM bv"), CAP);
    assert_eq!(rows(&core, "SELECT * FROM bv LIMIT ALL"), CAP);
    assert_eq!(rows(&core, "SELECT * FROM bv WHERE k <= 2"), 2);
}

/// 評価後射影形ビューの外側 `GROUP BY` の省略・`LIMIT ALL` も cap を超えたら `54000`。
#[test]
fn buffered_view_outer_group_by_enforces_the_cap() {
    let (core, _g) = open("sql34-buffered-agg");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE VIEW bv AS SELECT id, k FROM t ORDER BY id LIMIT 4",
    )
    .expect("create buffered view");
    seed_t(&core, "alice", 1..=5);
    assert_eq!(
        code(&core, "SELECT k, COUNT(*) FROM bv GROUP BY k LIMIT ALL"),
        "54000"
    );
    assert_eq!(
        code(&core, "SELECT k, COUNT(*) FROM bv GROUP BY k"),
        "54000"
    );
    // OFFSET 適用後に cap 内へ収まれば成功する。
    assert_eq!(
        rows(&core, "SELECT k, COUNT(*) FROM bv GROUP BY k OFFSET 1"),
        CAP
    );
}

/// ビュー本文の集計は本文用の上限で評価され、外側が cap 内へ絞れば成功する。
#[test]
fn buffered_view_body_group_by_uses_body_limits() {
    let (core, _g) = open("sql34-buffered-body-agg");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE VIEW gv AS SELECT k, COUNT(*) AS n FROM t GROUP BY k",
    )
    .expect("create buffered view");
    seed_t(&core, "alice", 1..=4);
    assert_eq!(rows(&core, "SELECT * FROM gv LIMIT 2"), 2);
}

/// `DECLARE` は全件を数え、超過なら `54000` で拒否してカーソルを登録しない。
#[test]
fn declare_rejects_over_cap_and_registers_nothing() {
    let (core, _g) = open("sql34-cursor");
    seed_t(&core, "alice", 1..=4);
    let caller = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "DECLARE c CURSOR FOR SELECT id FROM t",
        )
        .expect_err("declare over cap");
    assert_eq!(err.wire_code(), "54000");

    // 上限以内なら FETCH で全件読める。
    let (core2, _g2) = open("sql34-cursor-ok");
    seed_t(&core2, "alice", 1..=3);
    let mut session2 = SessionState::default();
    let mut txn2 = core2.new_session_transaction();
    core2
        .execute_sql_in_txn(&caller, &mut session2, &mut txn2, "BEGIN")
        .expect("begin");
    core2
        .execute_sql_in_txn(
            &caller,
            &mut session2,
            &mut txn2,
            "DECLARE c CURSOR FOR SELECT id FROM t LIMIT ALL",
        )
        .expect("declare within cap");
    let fetched = core2
        .execute_sql_in_txn(&caller, &mut session2, &mut txn2, "FETCH 3 FROM c")
        .expect("fetch");
    let SqlOutcome::Fetch(result) = fetched else {
        panic!("expected Fetch outcome");
    };
    assert_eq!(result.rows.len(), CAP);
}

/// 距離 ORDER BY・`USING PLAN` の省略、サブクエリ内側の省略・`ALL` は従来どおり `42601`。
#[test]
fn ranked_and_subquery_forms_still_require_limit() {
    let (core, _g) = open("sql34-regress");
    seed_t(&core, "alice", 1..=2);
    for sql in [
        "SELECT id FROM t WHERE k IN (SELECT k FROM u)",
        "SELECT id FROM t WHERE k IN (SELECT k FROM u LIMIT ALL)",
        "SELECT id FROM t WHERE EXISTS (SELECT k FROM u)",
        "SELECT id FROM t WHERE k = (SELECT k FROM u)",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

/// Describe（拡張クエリプロトコル）でも省略形が `22000` にならない。
#[test]
fn describe_accepts_omitted_forms() {
    let (core, _g) = open("sql34-describe");
    let session = SessionState::default();
    for sql in [
        "SELECT * FROM t",
        "SELECT * FROM t LIMIT ALL",
        "SELECT k, COUNT(*) FROM t GROUP BY k",
        "SELECT t.id FROM t JOIN u ON t.k = u.k",
    ] {
        let prepared = core
            .parse_sql_prepared(sql)
            .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
        core.describe_prepared_in_session(&session, &prepared)
            .unwrap_or_else(|e| panic!("describe {sql}: {}", e.wire_code()));
    }
}

/// INSERT ... SELECT・CTAS の入力 SELECT の省略は同じ上限で `54000`（副作用なし）。
#[test]
fn insert_select_omitted_stays_within_the_same_cap() {
    let (core, _g) = open("sql34-insert-select");
    seed_t(&core, "alice", 1..=3);
    run(
        &core,
        "INSERT INTO u (id, k, w) SELECT id, k, v FROM t USING OPERATION_ID 'op-34-a'",
    )
    .expect("within cap");
    seed_t(&core, "alice", 4..=4);
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, k, w) SELECT id + 100, k, v FROM t USING OPERATION_ID 'op-34-b'"
        ),
        "54000"
    );
}

/// 集合演算全体の `OFFSET`（`LIMIT ALL OFFSET n`・`OFFSET n` 単独・`LIMIT n OFFSET m`）。
/// OFFSET を先に適用し、読み飛ばし後の件数で結果行数上限を判定する。
#[test]
fn set_operation_offset_is_applied_before_the_cap() {
    let (core, _g) = open("sql34-setop-offset");
    seed_t(&core, "alice", 1..=2);
    seed_u(&core, "alice", 3..=4);
    let sql = "SELECT k FROM t UNION ALL SELECT k FROM u";
    // 4 行 - OFFSET 1 = 3 行 = CAP（ちょうど許容）。
    assert_eq!(rows(&core, &format!("{sql} LIMIT ALL OFFSET 1")), 3);
    assert_eq!(rows(&core, &format!("{sql} OFFSET 1")), 3);
    assert_eq!(rows(&core, &format!("{sql} OFFSET 4")), 0);
    // OFFSET 0 では 4 行 > CAP。
    assert_eq!(code(&core, &format!("{sql} OFFSET 0")), "54000");
    assert_eq!(code(&core, &format!("{sql} LIMIT ALL OFFSET 0")), "54000");
    assert_eq!(rows(&core, &format!("{sql} LIMIT 2 OFFSET 3")), 1);
}
