//! 文単位のデッドライン（`engine::interrupt::DeadlineScope`。Issue #1951・SQL-71 のポインタ）の結合テスト。
//!
//! sql1798_interrupt と同じ「毒行」パターンを使う。既に過ぎた期限（`StatementDeadline::at(now)`）
//! なら毒行へ到達する前に `57014`、1 時間先の期限なら通常どおり毒行の `22012`／結果が返る。
//! 時間経過を待たない（sleep なし）ので決定的に動く。

use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::interrupt::{DeadlineScope, InterruptReason, StatementDeadline};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const CAP: usize = 5;
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(CAP));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, k INTEGER, w INTEGER)",
        "CREATE TABLE big (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx(), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO t (id, k, v) VALUES (1, 1, 10), (2, 2, 20), (100, 0, 0)",
    )
    .expect("seed t");
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO u (id, k, w) VALUES (1, 1, 100), (2, 2, 200)",
    )
    .expect("seed u");
    let values: Vec<String> = (1..=10).map(|n| format!("({n}, {n}, {n})")).collect();
    core.execute_insert_sql(
        &ctx(),
        &format!("INSERT INTO big (id, k, v) VALUES {}", values.join(", ")),
    )
    .expect("seed big");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
}

fn err_of(core: &EngineCore, sql: &str) -> SqlSurfaceError {
    match run(core, sql) {
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
        Err(e) => e,
    }
}

const QUERIES: &[&str] = &[
    "SELECT * FROM t WHERE 100 / k > 0",
    "SELECT * FROM t WHERE 100 / k > 0 ORDER BY id",
    "SELECT * FROM t WHERE 100 / k > 0 ORDER BY v",
    "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t WHERE 100 / k > 0",
    "SELECT k, SUM(v) AS s FROM t WHERE 100 / k > 0 GROUP BY k",
    "SELECT SUM(v) AS s FROM t WHERE 100 / k > 0",
];

const JOIN: &str = "SELECT t.id, u.w FROM t JOIN u ON t.k = u.k";

/// SQL-71: 期限切れなら毒行へ到達せず `57014`・statement timeout 文言で止まる。
#[test]
fn sql71_expired_deadline_stops_before_the_poison_row() {
    let (core, _g) = open("sql1951-stop");
    let _scope = DeadlineScope::install(expired());
    for sql in QUERIES.iter().copied().chain([JOIN]) {
        let e = err_of(&core, sql);
        assert_eq!(e.wire_code(), "57014", "{sql}");
        assert_eq!(e.to_string(), TIMEOUT_MSG, "{sql}");
    }
}

/// SQL-71: 期限が十分先なら挙動は変わらない（毒行の `22012`・JOIN は結果を返す）。
#[test]
fn sql71_far_deadline_does_not_change_behavior() {
    let (core, _g) = open("sql1951-far");
    let _scope = DeadlineScope::install(far());
    for sql in QUERIES {
        assert_eq!(err_of(&core, sql).wire_code(), "22012", "{sql}");
    }
    match run(&core, JOIN).expect("join") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// ガードを drop すれば期限は残らない。
#[test]
fn sql71_dropping_the_guard_restores_normal_behavior() {
    let (core, _g) = open("sql1951-drop");
    {
        let _scope = DeadlineScope::install(expired());
        assert_eq!(err_of(&core, "SELECT * FROM t").wire_code(), "57014");
    }
    match run(&core, "SELECT * FROM t WHERE id < 100").expect("select") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 明示トランザクション内の期限切れは `Failed`（`25P02`）にし、`ROLLBACK` で復帰できる。
#[test]
fn sql71_expired_deadline_fails_the_explicit_transaction() {
    let (core, _g) = open("sql1951-txn");
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    {
        let _scope = DeadlineScope::install(expired());
        let err = core
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT * FROM t")
            .expect_err("timed out");
        assert_eq!(err.wire_code(), "57014");
        assert_eq!(err.to_string(), TIMEOUT_MSG);
    }
    let err = core
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT * FROM t")
        .expect_err("failed transaction");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    match core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SELECT * FROM t WHERE id < 100",
        )
        .expect("usable again")
    {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// デッドラインは既存の結果件数上限（`54000`）と独立に働く。期限が先なら従来の上限が先に効く。
#[test]
fn sql71_far_deadline_keeps_the_result_cap_independent() {
    let (core, _g) = open("sql1951-cap");
    let _scope = DeadlineScope::install(far());
    assert_eq!(err_of(&core, "SELECT * FROM big").wire_code(), "54000");
}

/// `QueryCanceled` の文言は理由ごとの固定文言（切断・取り消しは従来のまま）。
#[test]
fn sql71_query_canceled_messages_are_fixed_per_reason() {
    let msg = |reason| SqlSurfaceError::QueryCanceled { reason }.to_string();
    assert_eq!(msg(InterruptReason::StatementTimeout), TIMEOUT_MSG);
    for r in [InterruptReason::ClientGone, InterruptReason::UserCancel] {
        assert_eq!(msg(r), "canceling statement due to user request");
    }
}
