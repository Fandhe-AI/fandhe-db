//! セッション変数 `hnsw.iterative_scan`・`hnsw.max_scan_tuples` の `SET`／`SHOW`／`RESET`
//! （Issue #2033・SQL-73 のポインタ）の結合テスト。値の往復・表示・拒否・Tx 巻き戻しを
//! 決定的に固定する（探索経路への反映は `sql2033_hnsw_iterative_scan_behavior`）。

use engine::core::EngineCore;
use engine::hnsw::HnswIterativeScan;
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

fn show(core: &EngineCore, session: &mut SessionState, name: &str) -> String {
    match run(core, session, &format!("SHOW {name}")).expect("show") {
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
    let (core, _g) = open("sql2033-roundtrip");
    let mut s = SessionState::default();
    assert_eq!(show(&core, &mut s, "hnsw.iterative_scan"), "strict_order");
    assert_eq!(show(&core, &mut s, "hnsw.max_scan_tuples"), "20000");
    for (sql, name, expect) in [
        (
            "SET hnsw.iterative_scan = off",
            "hnsw.iterative_scan",
            "off",
        ),
        (
            "SET hnsw.iterative_scan TO relaxed_order",
            "hnsw.iterative_scan",
            "relaxed_order",
        ),
        (
            "SET SESSION hnsw.iterative_scan = 'STRICT_ORDER'",
            "hnsw.iterative_scan",
            "strict_order",
        ),
        (
            "set HNSW.ITERATIVE_SCAN to Off",
            "hnsw.iterative_scan",
            "off",
        ),
        (
            "SET hnsw.max_scan_tuples = 50",
            "hnsw.max_scan_tuples",
            "50",
        ),
        (
            "SET hnsw.max_scan_tuples TO '7'",
            "hnsw.max_scan_tuples",
            "7",
        ),
        (
            "SET hnsw.max_scan_tuples = 2147483647",
            "hnsw.max_scan_tuples",
            "2147483647",
        ),
    ] {
        let out = run(&core, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert!(matches!(
            out,
            SqlOutcome::SetHnswScanSetting { reset: false }
        ));
        assert_eq!(show(&core, &mut s, name), expect, "{sql}");
    }
    assert_eq!(s.hnsw_iterative_scan(), Some(HnswIterativeScan::Off));
    assert_eq!(s.hnsw_max_scan_tuples(), Some(2_147_483_647));
    run(&core, &mut s, "SET hnsw.iterative_scan TO DEFAULT").expect("default");
    run(&core, &mut s, "SET hnsw.max_scan_tuples TO DEFAULT").expect("default");
    assert_eq!(s.hnsw_iterative_scan(), None);
    assert_eq!(s.hnsw_max_scan_tuples(), None);
    run(&core, &mut s, "SET hnsw.iterative_scan = off").expect("set");
    run(&core, &mut s, "SET hnsw.max_scan_tuples = 5").expect("set");
    for sql in ["RESET hnsw.iterative_scan", "RESET hnsw.max_scan_tuples"] {
        assert!(matches!(
            run(&core, &mut s, sql).expect("reset"),
            SqlOutcome::SetHnswScanSetting { reset: true }
        ));
    }
    assert_eq!(show(&core, &mut s, "hnsw.iterative_scan"), "strict_order");
    assert_eq!(show(&core, &mut s, "hnsw.max_scan_tuples"), "20000");
}

/// SQL-73: 範囲外・不正値は 22023（直前の値は変わらない・入力値を応答に含めない）、
/// `SET LOCAL` の不正値も 22023、値の欠落は 42601。
#[test]
fn invalid_values_are_rejected_without_changing_state() {
    let (core, _g) = open("sql2033-invalid");
    let mut s = SessionState::default();
    run(&core, &mut s, "SET hnsw.iterative_scan = relaxed_order").expect("set");
    run(&core, &mut s, "SET hnsw.max_scan_tuples = 42").expect("set");
    for sql in [
        "SET hnsw.iterative_scan = abc",
        "SET hnsw.iterative_scan = on",
        "SET hnsw.iterative_scan = 1",
        "SET hnsw.iterative_scan = ''",
        "SET hnsw.max_scan_tuples = 0",
        "SET hnsw.max_scan_tuples = 2147483648",
        "SET hnsw.max_scan_tuples = -1",
        "SET hnsw.max_scan_tuples = 1.5",
        "SET hnsw.max_scan_tuples = 'abc'",
        "SET hnsw.max_scan_tuples = 99999999999999999999",
    ] {
        let e = run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(e.wire_code(), "22023", "{sql}");
        let text = format!("{e:?}{e}");
        assert!(
            !text.contains("abc") && !text.contains("2147483648"),
            "{sql}: {text}"
        );
        assert_eq!(show(&core, &mut s, "hnsw.iterative_scan"), "relaxed_order");
        assert_eq!(show(&core, &mut s, "hnsw.max_scan_tuples"), "42");
    }
    for sql in [
        "SET LOCAL hnsw.iterative_scan = abc",
        "SET LOCAL hnsw.max_scan_tuples = 0",
    ] {
        assert_eq!(run(&core, &mut s, sql).expect_err(sql).wire_code(), "22023");
    }
    for sql in ["SET hnsw.iterative_scan =", "SET hnsw.max_scan_tuples ="] {
        assert_eq!(run(&core, &mut s, sql).expect_err(sql).wire_code(), "42601");
    }
    assert_eq!(show(&core, &mut s, "hnsw.max_scan_tuples"), "42");
}

/// SQL-73: BEGIN／ROLLBACK と SAVEPOINT／ROLLBACK TO で値が戻る。
#[test]
fn set_is_rolled_back_with_transaction_and_savepoint() {
    let (core, _g) = open("sql2033-rollback");
    let mut s = SessionState::default();
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let c = ctx();
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "SET hnsw.iterative_scan = off");
    exec(&mut s, "SET hnsw.max_scan_tuples = 100");
    exec(&mut s, "BEGIN");
    exec(&mut s, "SET hnsw.iterative_scan = strict_order");
    exec(&mut s, "SET hnsw.max_scan_tuples = 200");
    exec(&mut s, "ROLLBACK");
    assert_eq!(s.hnsw_iterative_scan(), Some(HnswIterativeScan::Off));
    assert_eq!(s.hnsw_max_scan_tuples(), Some(100));
    exec(&mut s, "BEGIN");
    exec(&mut s, "SAVEPOINT a");
    exec(&mut s, "SET hnsw.max_scan_tuples = 400");
    exec(&mut s, "ROLLBACK TO a");
    assert_eq!(s.hnsw_max_scan_tuples(), Some(100));
    exec(&mut s, "COMMIT");
}

/// SQL-73: セッションを持たない入口は固定文言で拒否する。
#[test]
fn session_less_entry_rejects() {
    let (core, _g) = open("sql2033-sessionless");
    for sql in [
        "SET hnsw.iterative_scan = off",
        "SHOW hnsw.iterative_scan",
        "SET hnsw.max_scan_tuples = 5",
        "SHOW hnsw.max_scan_tuples",
    ] {
        let e = core.execute_sql(&ctx(), sql).expect_err(sql);
        assert_eq!(e.wire_code(), "42601", "{sql}");
    }
}

/// SQL-73: `SET LOCAL` は明示トランザクション内だけ効き、COMMIT／ROLLBACK で LOCAL 直前の値へ戻る。
/// トランザクション外の `SET LOCAL` は効果を持たず、LOCAL 後のセッション値 `SET` は COMMIT 後も残る。
#[test]
fn set_local_is_restored_at_transaction_end() {
    let (core, _g) = open("sql2033-local");
    let mut s = SessionState::default();
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let c = ctx();
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    // 単独文の LOCAL は効果なし。
    exec(&mut s, "SET LOCAL hnsw.max_scan_tuples = 5");
    assert_eq!(get(&mut exec, &mut s, "hnsw.max_scan_tuples"), "20000");
    // COMMIT で LOCAL 直前（セッション値）へ戻る。
    exec(&mut s, "SET hnsw.max_scan_tuples = 42");
    exec(&mut s, "BEGIN");
    exec(&mut s, "SET LOCAL hnsw.max_scan_tuples = 7");
    exec(&mut s, "SET LOCAL hnsw.iterative_scan = off");
    exec(&mut s, "SET LOCAL hnsw.max_scan_tuples = 8");
    assert_eq!(get(&mut exec, &mut s, "hnsw.max_scan_tuples"), "8");
    assert_eq!(get(&mut exec, &mut s, "hnsw.iterative_scan"), "off");
    exec(&mut s, "COMMIT");
    assert_eq!(get(&mut exec, &mut s, "hnsw.max_scan_tuples"), "42");
    assert_eq!(
        get(&mut exec, &mut s, "hnsw.iterative_scan"),
        "strict_order"
    );
    // ROLLBACK でも戻り、LOCAL は以後のトランザクション外へ漏れない。
    exec(&mut s, "BEGIN");
    exec(&mut s, "SET LOCAL hnsw.max_scan_tuples = 9");
    exec(&mut s, "ROLLBACK");
    assert_eq!(get(&mut exec, &mut s, "hnsw.max_scan_tuples"), "42");
    exec(&mut s, "SET LOCAL hnsw.max_scan_tuples = 11");
    assert_eq!(get(&mut exec, &mut s, "hnsw.max_scan_tuples"), "42");
    // LOCAL の後のセッション値 SET は COMMIT 後も残る。
    exec(&mut s, "BEGIN");
    exec(&mut s, "SET LOCAL hnsw.max_scan_tuples = 3");
    exec(&mut s, "SET hnsw.max_scan_tuples = 99");
    exec(&mut s, "COMMIT");
    assert_eq!(get(&mut exec, &mut s, "hnsw.max_scan_tuples"), "99");
}

fn get(
    exec: &mut impl FnMut(&mut SessionState, &str) -> SqlOutcome,
    s: &mut SessionState,
    name: &str,
) -> String {
    match exec(s, &format!("SHOW {name}")) {
        SqlOutcome::Show(r) => match &r.rows[0].cells[0] {
            Cell::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}
