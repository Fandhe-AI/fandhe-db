//! セッション変数 `statement_timeout` の `SET`／`SHOW`／`RESET`（Issue #1954・SQL-71 のポインタ）の
//! 結合テスト。時間に依存せず、値の往復・表示・拒否・Tx 巻き戻し・既定値の取り込みを決定的に固定する
//! （期限の発火そのものは wire-server 側の単独バイナリ `wire1954_statement_timeout.rs`）。

use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn show(core: &EngineCore, session: &mut SessionState) -> String {
    match run(core, session, "SHOW statement_timeout").expect("show") {
        SqlOutcome::Show(result) => {
            assert_eq!(result.columns.len(), 1);
            match &result.rows[0].cells[0] {
                Cell::Text(t) => t.clone(),
                other => panic!("expected text, got {other:?}"),
            }
        }
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn set_show_reset_roundtrip_and_units() {
    let (core, _g) = open("sql1954-roundtrip");
    let mut s = SessionState::default();
    assert_eq!(show(&core, &mut s), "0");
    for (sql, expect) in [
        ("SET statement_timeout = 1500", "1500ms"),
        ("SET statement_timeout TO '5s'", "5s"),
        ("SET SESSION statement_timeout = '1min'", "1min"),
        ("SET statement_timeout = '1h'", "1h"),
        ("SET statement_timeout = '1d'", "1d"),
        ("SET statement_timeout = '90 s'", "90s"),
        ("SET statement_timeout = 0", "0"),
    ] {
        let out = run(&core, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert!(matches!(
            out,
            SqlOutcome::SetStatementTimeout { reset: false }
        ));
        assert_eq!(show(&core, &mut s), expect, "{sql}");
    }
    run(&core, &mut s, "SET statement_timeout = 7").expect("set");
    assert_eq!(s.statement_timeout(), Duration::from_millis(7));
    assert!(matches!(
        run(&core, &mut s, "SET statement_timeout TO DEFAULT").expect("default"),
        SqlOutcome::SetStatementTimeout { reset: false }
    ));
    assert_eq!(show(&core, &mut s), "0");
    run(&core, &mut s, "SET statement_timeout = 7").expect("set");
    assert!(matches!(
        run(&core, &mut s, "RESET statement_timeout").expect("reset"),
        SqlOutcome::SetStatementTimeout { reset: true }
    ));
    assert_eq!(show(&core, &mut s), "0");
}

#[test]
fn invalid_values_are_rejected_without_changing_the_value() {
    let (core, _g) = open("sql1954-invalid");
    let mut s = SessionState::default();
    run(&core, &mut s, "SET statement_timeout = 42").expect("set");
    for sql in [
        "SET statement_timeout = -1",
        "SET statement_timeout = 2147483648",
        "SET statement_timeout = '1.5s'",
        "SET statement_timeout = '1us'",
        "SET statement_timeout = 'abc'",
        "SET statement_timeout = '99999999999999999999d'",
    ] {
        let e = run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(e.wire_code(), "22023", "{sql}");
        let text = format!("{e:?}{e}");
        assert!(
            !text.contains("abc") && !text.contains("2147483648"),
            "{sql}: {text}"
        );
        assert_eq!(show(&core, &mut s), "42ms", "{sql}");
    }
    let e = run(&core, &mut s, "SET LOCAL statement_timeout = 1").expect_err("local");
    assert_eq!(e.wire_code(), "0A000");
    assert_eq!(show(&core, &mut s), "42ms");
    run(&core, &mut s, "SET statement_timeout = 2147483647").expect("max accepted");
}

#[test]
fn set_is_rolled_back_with_transaction_and_savepoint() {
    let (core, _g) = open("sql1954-rollback");
    let mut s = SessionState::default();
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let c = ctx();
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "SET statement_timeout = 100");
    exec(&mut s, "BEGIN");
    exec(&mut s, "SET statement_timeout = 200");
    exec(&mut s, "ROLLBACK");
    assert_eq!(s.statement_timeout(), Duration::from_millis(100));
    exec(&mut s, "BEGIN READ ONLY");
    exec(&mut s, "SET statement_timeout = 300");
    assert_eq!(s.statement_timeout(), Duration::from_millis(300));
    exec(&mut s, "ROLLBACK");
    exec(&mut s, "BEGIN");
    exec(&mut s, "SAVEPOINT a");
    exec(&mut s, "SET statement_timeout = 400");
    exec(&mut s, "ROLLBACK TO a");
    assert_eq!(s.statement_timeout(), Duration::from_millis(100));
    exec(&mut s, "COMMIT");
}

#[test]
fn session_less_entry_rejects_and_default_applies() {
    let (core, _g) = open("sql1954-default");
    let e = core
        .execute_sql(&ctx(), "SET statement_timeout = 5")
        .expect_err("session-less entry");
    assert_eq!(e.wire_code(), "42601");
    assert_eq!(core.default_statement_timeout(), Duration::ZERO);
    let core = core.with_default_statement_timeout(Duration::from_millis(1500));
    assert_eq!(
        core.default_statement_timeout(),
        Duration::from_millis(1500)
    );
    let mut s = SessionState::default();
    s.apply_default_statement_timeout(core.default_statement_timeout());
    assert_eq!(show(&core, &mut s), "1500ms");
    run(&core, &mut s, "SET statement_timeout = 9").expect("set");
    assert_eq!(show(&core, &mut s), "9ms");
    run(&core, &mut s, "RESET statement_timeout").expect("reset");
    assert_eq!(show(&core, &mut s), "1500ms");
}
