//! セッション変数 `hnsw.ef_search` の `SET`／`SHOW`／`RESET`（Issue #2030・SQL-73 のポインタ）の
//! 結合テスト。値の往復・表示・拒否・Tx 巻き戻しを決定的に固定する（HNSW 探索への反映と
//! EXPLAIN 表示は `sql_named` の `sql_explain.rs`）。

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
    match run(core, session, "SHOW hnsw.ef_search").expect("show") {
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

/// SQL-73: SET → SHOW → RESET の往復（`=`・`TO`・文字列・`DEFAULT`・大文字小文字・`SESSION`）。
#[test]
fn set_show_reset_roundtrip() {
    let (core, _g) = open("sql2030-roundtrip");
    let mut s = SessionState::default();
    assert_eq!(show(&core, &mut s), "64");
    for (sql, expect) in [
        ("SET hnsw.ef_search = 200", "200"),
        ("SET hnsw.ef_search TO 300", "300"),
        ("SET hnsw.ef_search = '100'", "100"),
        ("SET SESSION hnsw.ef_search = 1", "1"),
        ("set HNSW.EF_SEARCH to 1000", "1000"),
    ] {
        let out = run(&core, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert!(matches!(out, SqlOutcome::SetHnswEfSearch { reset: false }));
        assert_eq!(show(&core, &mut s), expect, "{sql}");
    }
    assert_eq!(s.hnsw_ef_search(), Some(1000));
    assert!(matches!(
        run(&core, &mut s, "SET hnsw.ef_search TO DEFAULT").expect("default"),
        SqlOutcome::SetHnswEfSearch { reset: false }
    ));
    assert_eq!(s.hnsw_ef_search(), None);
    run(&core, &mut s, "SET hnsw.ef_search = 5").expect("set");
    assert!(matches!(
        run(&core, &mut s, "RESET hnsw.ef_search").expect("reset"),
        SqlOutcome::SetHnswEfSearch { reset: true }
    ));
    assert_eq!(show(&core, &mut s), "64");
}

/// SQL-73: 範囲外・不正値は 22023（直前の値は変わらない）、`SET LOCAL` は 0A000、値の欠落は 42601。
#[test]
fn invalid_values_are_rejected_without_changing_state() {
    let (core, _g) = open("sql2030-invalid");
    let mut s = SessionState::default();
    run(&core, &mut s, "SET hnsw.ef_search = 42").expect("set");
    for sql in [
        "SET hnsw.ef_search = 0",
        "SET hnsw.ef_search = 1001",
        "SET hnsw.ef_search = -1",
        "SET hnsw.ef_search = 1.5",
        "SET hnsw.ef_search = 'abc'",
        "SET hnsw.ef_search = ''",
        "SET hnsw.ef_search = 99999999999999999999",
    ] {
        let e = run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(e.wire_code(), "22023", "{sql}");
        let text = format!("{e:?}{e}");
        assert!(
            !text.contains("abc") && !text.contains("1001"),
            "{sql}: {text}"
        );
        assert_eq!(show(&core, &mut s), "42", "{sql}");
    }
    let e = run(&core, &mut s, "SET LOCAL hnsw.ef_search = 1").expect_err("local");
    assert_eq!(e.wire_code(), "0A000");
    let e = run(&core, &mut s, "SET hnsw.ef_search =").expect_err("missing value");
    assert_eq!(e.wire_code(), "42601");
    let e = run(&core, &mut s, "SET hnsw.iterative_scan = 'off'").expect_err("unsupported name");
    assert_eq!(e.wire_code(), "42601");
    assert_eq!(show(&core, &mut s), "42");
}

/// SQL-73: BEGIN／ROLLBACK と SAVEPOINT／ROLLBACK TO で値が戻る。
#[test]
fn set_is_rolled_back_with_transaction_and_savepoint() {
    let (core, _g) = open("sql2030-rollback");
    let mut s = SessionState::default();
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let c = ctx();
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "SET hnsw.ef_search = 100");
    exec(&mut s, "BEGIN");
    exec(&mut s, "SET hnsw.ef_search = 200");
    exec(&mut s, "ROLLBACK");
    assert_eq!(s.hnsw_ef_search(), Some(100));
    exec(&mut s, "BEGIN");
    exec(&mut s, "SAVEPOINT a");
    exec(&mut s, "SET hnsw.ef_search = 400");
    exec(&mut s, "ROLLBACK TO a");
    assert_eq!(s.hnsw_ef_search(), Some(100));
    exec(&mut s, "COMMIT");
}

/// SQL-73: セッションを持たない入口は固定文言で拒否する。
#[test]
fn session_less_entry_rejects() {
    let (core, _g) = open("sql2030-sessionless");
    let e = core
        .execute_sql(&ctx(), "SET hnsw.ef_search = 5")
        .expect_err("session-less entry");
    assert_eq!(e.wire_code(), "42601");
    let e = core
        .execute_sql(&ctx(), "SHOW hnsw.ef_search")
        .expect_err("session-less entry");
    assert_eq!(e.wire_code(), "42601");
}
