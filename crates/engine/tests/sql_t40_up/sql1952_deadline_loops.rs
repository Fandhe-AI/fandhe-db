//! 走査後の実行ループ（JOIN・集約・ソート・集合演算・サブクエリ・CTE・派生表・索引集約）への
//! デッドライン確認点（Issue #1952・SQL-71 のポインタ。網羅表: `docs/design/statement-interrupt.md`）
//! の結合テスト。
//!
//! 既に過ぎた期限（`StatementDeadline::at(now)`）なら各経路が `57014` で中断され、1 時間先の期限なら
//! 期限なしと同じ結果になる。sleep・実測時間に依存しないため決定的に動く。

use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::interrupt::{DeadlineScope, StatementDeadline};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TIMEOUT_MSG: &str = "canceling statement due to statement timeout";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn expired() -> StatementDeadline {
    StatementDeadline::at(Instant::now())
}

fn far() -> StatementDeadline {
    StatementDeadline::after(Instant::now(), Duration::from_secs(3600))
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, k INTEGER, w INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx(), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO t (id, k, v) VALUES (1, 1, 10), (2, 2, 20), (3, 2, 30)",
    )
    .expect("seed t");
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO u (id, k, w) VALUES (1, 1, 100), (2, 2, 200)",
    )
    .expect("seed u");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
}

/// 経路ごとの SQL（JOIN の残余・ORDER BY・NULL 補完・集計、GROUP BY の HAVING・ORDER BY、集合演算、
/// サブクエリ、CTE、派生表）。
const QUERIES: &[&str] = &[
    "SELECT t.id, u.w FROM t JOIN u ON t.k = u.k WHERE t.v < u.w ORDER BY t.id",
    "SELECT t.id, u.w FROM t LEFT JOIN u ON t.k = u.k ORDER BY u.w, t.id",
    "SELECT t.id, u.id FROM t CROSS JOIN u ORDER BY t.id, u.id",
    "SELECT t.k, COUNT(*) AS c FROM t JOIN u ON t.k = u.k GROUP BY t.k ORDER BY t.k",
    "SELECT k, SUM(v) AS s FROM t GROUP BY k HAVING SUM(v) > 5 ORDER BY k",
    "SELECT DISTINCT k FROM t ORDER BY k",
    "SELECT id FROM t UNION SELECT id FROM u",
    "SELECT id FROM t INTERSECT SELECT id FROM u",
    "SELECT id FROM t EXCEPT SELECT id FROM u",
    "SELECT id FROM t WHERE k IN (SELECT k FROM u)",
    "WITH c AS (SELECT id, v FROM t) SELECT id FROM c WHERE v > 5 ORDER BY v",
    "SELECT id FROM (SELECT id, v FROM t) d WHERE v > 5 ORDER BY v",
];

/// SQL-71: 期限切れなら各経路が `57014`・statement timeout 文言で中断される（部分結果なし）。
#[test]
fn sql71_expired_deadline_stops_every_read_path() {
    let (core, _g) = open("sql1952-expired");
    let _scope = DeadlineScope::install(expired());
    for sql in QUERIES {
        let e = run(&core, sql).expect_err(sql);
        assert_eq!(e.wire_code(), "57014", "{sql}");
        assert_eq!(e.to_string(), TIMEOUT_MSG, "{sql}");
    }
}

/// SQL-71: 期限が先なら結果は期限なしと同じ（確認点は挙動を変えない）。
#[test]
fn sql71_far_deadline_matches_no_deadline_results() {
    let (core, _g) = open("sql1952-far");
    let baseline: Vec<String> = QUERIES
        .iter()
        .map(|sql| format!("{:?}", run(&core, sql).expect(sql)))
        .collect();
    let _scope = DeadlineScope::install(far());
    for (sql, expected) in QUERIES.iter().zip(&baseline) {
        let got = format!("{:?}", run(&core, sql).expect(sql));
        assert_eq!(&got, expected, "{sql}");
    }
}

/// 明示トランザクション内の中断（JOIN 経路）は `Failed`（`25P02`）にし、`ROLLBACK` で復帰できる。
#[test]
fn sql71_expired_join_fails_the_explicit_transaction() {
    let (core, _g) = open("sql1952-txn");
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    {
        let _scope = DeadlineScope::install(expired());
        let err = core
            .execute_sql_in_txn(&caller, &mut session, &mut txn, QUERIES[0])
            .expect_err("timed out");
        assert_eq!(err.wire_code(), "57014");
    }
    let err = core
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT * FROM t")
        .expect_err("failed transaction");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}
