//! `SUM`／`AVG`／`MIN`／`MAX(DISTINCT)` の受理（Issue #3263。SQL-25 のポインタ。
//! 設計記録: `docs/design/aggregate-scalar-types.md` の Issue #3263 追記節）の結合テスト。
//!
//! 期待値は fixture から手計算した値（重複を除いた値の和・平均）。DISTINCT の状態は
//! `COUNT(DISTINCT)` と同じクエリ共有の予算で縛られ、超過は `54000`、VECTOR などキーを
//! 定義できない入力・未対応の形は fail-closed で拒否する。RLS 可視判定の後段でのみ観測する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx_of("alice"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, g INTEGER, i INTEGER, s SMALLINT, b BIGINT, \
         r REAL, d DOUBLE PRECISION, n NUMERIC(10,2), iv INTERVAL, tx TEXT, embedding VECTOR(2))",
    )
    .expect("ddl");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<String, SqlSurfaceError> {
    let out = core.execute_sql_in_session(&ctx_of(tenant), &mut SessionState::default(), sql)?;
    match out {
        SqlOutcome::Query(r) => Ok(r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect::<Vec<_>>()
            .join("|")),
        other => Ok(format!("{other:?}")),
    }
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    run(core, tenant, sql).expect_err(sql).wire_code()
}

/// 6 行。g=1: i=10,10,20（d は 0.0,-0.0,2.5）、g=2: i=NULL,10,10。
fn seed(core: &EngineCore, tenant: &str, base: u64) {
    let sql = format!(
        "INSERT INTO t (id, g, i, s, b, r, d, n, iv, tx) VALUES \
         ({a}, 1, 10, 2, 100, 1.5, 0.0, 3.25, '1 mon', 'x'), \
         ({b}, 1, 10, 2, 100, 1.5, -0.0, 3.25, '30 days', 'x'), \
         ({c}, 1, 20, 3, 200, 2.5, 2.5, 4.75, '2 days', 'y'), \
         ({d}, 2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
         ({e}, 2, 10, 2, 100, 1.5, 2.5, 3.25, '1 day', 'x'), \
         ({f}, 2, 10, 2, 100, 1.5, 2.5, 3.25, '1 day', 'x')",
        a = base,
        b = base + 1,
        c = base + 2,
        d = base + 3,
        e = base + 4,
        f = base + 5
    );
    core.execute_insert_sql(&ctx_of(tenant), &sql)
        .expect("seed");
}

fn same_as(core: &EngineCore, a: &str, b: &str) {
    assert_eq!(
        run(core, "alice", a).expect(a),
        run(core, "alice", b).expect(b),
        "{a} == {b}"
    );
}

/// 重複を除いた値の和・平均が、重複の無い同値集合の通常集計と一致する（独立オラクル）。
#[test]
fn sql3263_sum_avg_distinct_match_deduplicated_oracle() {
    let (core, _g) = open("sql3263-values");
    seed(&core, "alice", 1);
    // 重複を除いた i は {10, 20}（id 1 と 3 の行が代表）。
    for (col, ids) in [
        ("i", "1, 3"),
        ("s", "1, 3"),
        ("b", "1, 3"),
        ("r", "1, 3"),
        ("n", "1, 3"),
        // d は {0.0(= -0.0), 2.5}。
        ("d", "1, 3"),
    ] {
        same_as(
            &core,
            &format!("SELECT SUM(DISTINCT {col}), AVG(DISTINCT {col}) FROM t"),
            &format!("SELECT SUM({col}), AVG({col}) FROM t WHERE id IN ({ids})"),
        );
    }
    // INTERVAL は正規化キーで同一視する（`1 mon` = `30 days`）。残る値は {1 mon, 2 days, 1 day}。
    same_as(
        &core,
        "SELECT SUM(DISTINCT iv) FROM t",
        "SELECT SUM(iv) FROM t WHERE id IN (1, 3, 5)",
    );
    // 結果型は非 DISTINCT と同じ（`SUM(int)` は整数）。
    assert_eq!(
        run(&core, "alice", "SELECT SUM(DISTINCT i) FROM t").unwrap(),
        "[SignedInteger(30)]"
    );
}

#[test]
fn sql3263_group_by_filter_having_and_mixed_calls() {
    let (core, _g) = open("sql3263-group");
    seed(&core, "alice", 1);
    let all = |sql: &str| run(&core, "alice", sql).expect(sql);
    // g=1: {10, 20} → 30、g=2: {10} → 10。
    assert_eq!(
        all("SELECT g, SUM(DISTINCT i) FROM t GROUP BY g ORDER BY g"),
        "[SignedInteger(1), SignedInteger(30)]|[SignedInteger(2), SignedInteger(10)]"
    );
    same_as(
        &core,
        "SELECT g, AVG(DISTINCT i) FROM t GROUP BY g ORDER BY g",
        "SELECT g, AVG(i) FROM t WHERE id IN (1, 3, 5) GROUP BY g ORDER BY g",
    );
    // 同一文の SUM(i) と SUM(DISTINCT i) は別々に計算される（60 と 30）。
    assert_eq!(
        all("SELECT SUM(i), SUM(DISTINCT i) FROM t"),
        "[SignedInteger(60), SignedInteger(30)]"
    );
    // FILTER・HAVING（SELECT に無い集計）。
    assert_eq!(
        all("SELECT SUM(DISTINCT i) FILTER (WHERE g = 2) FROM t"),
        "[SignedInteger(10)]"
    );
    assert_eq!(
        all("SELECT g FROM t GROUP BY g HAVING SUM(DISTINCT i) > 20"),
        "[SignedInteger(1)]"
    );
    // MIN／MAX(DISTINCT) は非 DISTINCT と同値。
    same_as(
        &core,
        "SELECT MIN(DISTINCT i), MAX(DISTINCT i) FROM t",
        "SELECT MIN(i), MAX(i) FROM t",
    );
    // 式引数は受理される。
    assert!(!all("SELECT SUM(DISTINCT i + 1) FROM t").is_empty());
    // 空集合・全 NULL は NULL。
    assert_eq!(
        all("SELECT SUM(DISTINCT i), AVG(DISTINCT i) FROM t WHERE id = 4"),
        "[Null, Null]"
    );
    assert_eq!(
        all("SELECT SUM(DISTINCT i) FROM t WHERE id > 1000"),
        "[Null]"
    );
}

/// 他テナントの行は集計にも重複判定にも入らない。エラー文言に値・テナントは出ない。
#[test]
fn sql3263_tenant_boundary_is_kept() {
    let (core, _g) = open("sql3263-rls");
    seed(&core, "alice", 1);
    // bob の Private 行（i = 999, 20）。20 は alice の値と重なる。
    core.execute_insert_sql(
        &ctx_of("bob"),
        "INSERT INTO t (id, g, i) VALUES (101, 1, 999), (102, 1, 20)",
    )
    .expect("bob seed");
    assert_eq!(
        run(&core, "alice", "SELECT SUM(DISTINCT i) FROM t").unwrap(),
        "[SignedInteger(30)]"
    );
    let err = run(&core, "alice", "SELECT SUM(DISTINCT embedding) FROM t").unwrap_err();
    let msg = format!("{err:?}");
    assert!(!msg.contains("bob") && !msg.contains("999"), "{msg}");
}

#[test]
fn sql3263_unsupported_shapes_fail_closed() {
    let (core, _g) = open("sql3263-reject");
    seed(&core, "alice", 1);
    for (sql, expected) in [
        ("SELECT SUM(DISTINCT embedding) FROM t", "22000"),
        ("SELECT AVG(DISTINCT embedding) FROM t", "22000"),
        ("SELECT SUM(DISTINCT tx) FROM t", "42883"),
        ("SELECT SUM(DISTINCT *) FROM t", "42601"),
        ("SELECT SUM(DISTINCT i) OVER () FROM t", "42601"),
        ("SELECT variance(DISTINCT i) FROM t", "42601"),
    ] {
        assert_eq!(code(&core, "alice", sql), expected, "{sql}");
    }
}

/// DISTINCT 値が上限（100,000 件）を超えたら `54000`（`COUNT(DISTINCT)` と同じ予算）。
#[test]
fn sql3263_budget_overflow_is_54000() {
    let (core, _g) = open("sql3263-budget");
    let mut start = 1u64;
    while start <= 100_001 {
        let end = (start + 63).min(100_001);
        let values = (start..=end)
            .map(|id| format!("({id}, 1)"))
            .collect::<Vec<_>>()
            .join(", ");
        core.execute_insert_sql(
            &ctx_of("alice"),
            &format!("INSERT INTO t (id, g) VALUES {values}"),
        )
        .expect("bulk");
        start = end + 1;
    }
    assert_eq!(
        code(&core, "alice", "SELECT SUM(DISTINCT id) FROM t"),
        "54000"
    );
}
