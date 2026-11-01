//! `LOCK [TABLE]`（SQL-38・Issue #1958）の結合テスト。ポインタ: `docs/spec/04-behavior/`
//! SQL-38 (3)(4)(5)(7)・SQL-31・RLS-12・TABLE-3。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として検証する。NOWAIT の取得失敗（`55P03`）の
//! SQL レベル E2E は、現状 `BEGIN` が writer を取得するため 2 つ目のセッションが `LOCK` へ到達できず、
//! 遅延取得（#2400）の後に追加する（取得失敗の単体検証は `sql::transaction` の単体テスト）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger),
        guard,
    )
}

fn ddl(core: &EngineCore, tenant: &str, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()));
}

fn run_as<'a>(
    core: &'a EngineCore,
    tenant: &str,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, (String, String)> {
    core.execute_sql_in_txn(&ctx(tenant), session, txn, sql)
        .map_err(|e| (e.wire_code().to_string(), e.to_string()))
}

fn code<T: std::fmt::Debug>(r: Result<T, (String, String)>) -> String {
    r.expect_err("must fail").0
}

fn count(core: &EngineCore, tenant: &str, table: &str) -> String {
    let mut s = SessionState::default();
    match core.execute_sql_in_session(
        &ctx(tenant),
        &mut s,
        &format!("SELECT id FROM {table} ORDER BY id LIMIT 100"),
    ) {
        Ok(o) => format!("{o:?}"),
        Err(e) => format!("ERR {}", e.wire_code()),
    }
}

const MODES: [&str; 8] = [
    "ACCESS SHARE",
    "ROW SHARE",
    "ROW EXCLUSIVE",
    "SHARE UPDATE EXCLUSIVE",
    "SHARE",
    "SHARE ROW EXCLUSIVE",
    "EXCLUSIVE",
    "ACCESS EXCLUSIVE",
];

#[test]
fn all_forms_acquire_and_hold_until_commit() {
    let (core, _g) = new_core("sql38-lock-forms");
    ddl(
        &core,
        "alice",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ddl(&core, "alice", "CREATE TABLE u (id BIGINT PRIMARY KEY)");
    let mut forms: Vec<String> = Vec::new();
    for mode in MODES {
        for kw in ["", "TABLE "] {
            for nowait in ["", " NOWAIT"] {
                forms.push(format!("LOCK {kw}t IN {mode} MODE{nowait}"));
            }
        }
    }
    forms.push("LOCK t".to_string());
    forms.push("LOCK TABLE t".to_string());
    forms.push("LOCK TABLE ONLY t".to_string());
    forms.push("LOCK TABLE t, u IN SHARE MODE NOWAIT".to_string());
    forms.push("lock table t * , u".to_string());
    let mut id = 0;
    for sql in &forms {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
        assert_eq!(
            run_as(&core, "alice", &mut session, &mut txn, sql),
            Ok(SqlOutcome::Lock),
            "{sql}"
        );
        assert_eq!(txn.status(), TransactionStatus::InTransaction, "{sql}");
        id += 1;
        run_as(
            &core,
            "alice",
            &mut session,
            &mut txn,
            &format!("INSERT INTO t (id, name) VALUES ({id}, 'x')"),
        )
        .expect("insert");
        run_as(&core, "alice", &mut session, &mut txn, "COMMIT").expect("commit");
    }
    assert!(count(&core, "alice", "t").contains(&format!("{id}")));
}

#[test]
fn rollback_discards_work_after_lock() {
    let (core, _g) = new_core("sql38-lock-rollback");
    ddl(&core, "alice", "CREATE TABLE t (id BIGINT PRIMARY KEY)");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
    run_as(&core, "alice", &mut session, &mut txn, "LOCK TABLE t").expect("lock");
    run_as(
        &core,
        "alice",
        &mut session,
        &mut txn,
        "INSERT INTO t (id) VALUES (1)",
    )
    .expect("insert");
    run_as(&core, "alice", &mut session, &mut txn, "ROLLBACK").expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    // writer が解放されているので次の Tx が取得できる。
    run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin again");
    run_as(&core, "alice", &mut session, &mut txn, "ROLLBACK").expect("rollback");
}

#[test]
fn outside_transaction_is_25p01_and_failed_state_is_25p02() {
    let (core, _g) = new_core("sql38-lock-state");
    ddl(&core, "alice", "CREATE TABLE t (id BIGINT PRIMARY KEY)");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    assert_eq!(
        code(run_as(
            &core,
            "alice",
            &mut session,
            &mut txn,
            "LOCK TABLE t"
        )),
        "25P01"
    );
    let single =
        core.execute_sql_in_session(&ctx("alice"), &mut SessionState::default(), "LOCK TABLE t");
    assert_eq!(single.expect_err("25P01").wire_code(), "25P01");

    run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
    assert_eq!(
        code(run_as(
            &core,
            "alice",
            &mut session,
            &mut txn,
            "LOCK TABLE nosuch"
        )),
        "42P01"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        code(run_as(
            &core,
            "alice",
            &mut session,
            &mut txn,
            "LOCK TABLE t"
        )),
        "25P02"
    );
    run_as(&core, "alice", &mut session, &mut txn, "ROLLBACK").expect("rollback");
}

#[test]
fn missing_view_and_malformed_names_are_rejected() {
    let (core, _g) = new_core("sql38-lock-names");
    ddl(&core, "alice", "CREATE TABLE t (id BIGINT PRIMARY KEY)");
    ddl(&core, "alice", "CREATE VIEW v AS SELECT id FROM t");
    let cases = [
        ("LOCK TABLE nosuch", "42P01"),
        ("LOCK TABLE t, nosuch", "42P01"),
        ("LOCK TABLE nosuch, t", "42P01"),
        ("LOCK TABLE v", "42809"),
        ("LOCK TABLE IN SHARE MODE", "42601"),
        ("LOCK TABLE t IN BOGUS MODE", "42601"),
        ("LOCK TABLE t IN SHARE", "42601"),
        ("LOCK TABLE t NOWAIT junk", "42601"),
        ("LOCK TABLE t USING OPERATION_ID 'x'", "42601"),
    ];
    for (sql, want) in cases {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
        let got = code(run_as(&core, "alice", &mut session, &mut txn, sql));
        assert_eq!(got, want, "{sql}");
        let _ = run_as(&core, "alice", &mut session, &mut txn, "ROLLBACK");
    }
}

#[test]
fn other_tenants_and_missing_schemas_are_indistinguishable() {
    let (core, _g) = new_core("sql38-lock-tenant");
    ddl(&core, "bob", "CREATE SCHEMA sb");
    ddl(&core, "bob", "CREATE TABLE sb.t (id BIGINT PRIMARY KEY)");
    ddl(&core, "bob", "INSERT INTO sb.t (id) VALUES (7)");
    ddl(&core, "alice", "CREATE TABLE a (id BIGINT PRIMARY KEY)");
    let before_b = count(&core, "bob", "sb.t");
    let before_a = count(&core, "alice", "a");

    let probe = |sql: &str| -> (String, String) {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
        let r = run_as(&core, "alice", &mut session, &mut txn, sql).expect_err("must fail");
        let _ = run_as(&core, "alice", &mut session, &mut txn, "ROLLBACK");
        r
    };
    let other = probe("LOCK TABLE sb.t");
    let missing = probe("LOCK TABLE nosuch.t");
    // 文言は名前の写しだけが異なる（他テナントの存在・内部キーは含まない）。
    assert_eq!(other.0, missing.0);
    assert_eq!(other.1.replace("sb", "nosuch"), missing.1);
    assert!(!other.1.contains('#'), "{other:?}");

    // 未修飾で不在の表と、search_path が他所を指す場合も同一。
    let unqualified = probe("LOCK TABLE t");
    let unqualified2 = probe("LOCK TABLE zzz");
    assert_eq!(unqualified.0, "42P01");
    assert_eq!(unqualified.0, unqualified2.0);

    assert_eq!(count(&core, "bob", "sb.t"), before_b);
    assert_eq!(count(&core, "alice", "a"), before_a);
}
#[test]
fn quoted_keyword_named_tables_are_lockable() {
    let (core, _g) = new_core("sql38-lock-quoted");
    ddl(
        &core,
        "alice",
        "CREATE TABLE \"default\" (id BIGINT PRIMARY KEY)",
    );
    ddl(
        &core,
        "alice",
        "CREATE TABLE \"current_date\" (id BIGINT PRIMARY KEY)",
    );
    for sql in [
        "LOCK TABLE \"default\"",
        "LOCK TABLE ONLY \"default\" *, \"current_date\" IN SHARE MODE",
    ] {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
        assert_eq!(
            run_as(&core, "alice", &mut session, &mut txn, sql),
            Ok(SqlOutcome::Lock),
            "{sql}"
        );
        run_as(&core, "alice", &mut session, &mut txn, "ROLLBACK").expect("rollback");
    }
}

/// キーワードと同名の一時表（`lock`・`table`・`mode` 等）を持つセッションでも、永続表への
/// `LOCK TABLE` は通り、一時表そのものへの `LOCK` は固定の `0A000` で拒否される（fail-closed）。
#[test]
fn keyword_named_temp_tables_do_not_block_persistent_lock() {
    let (core, _g) = new_core("sql38-lock-temp-kw");
    ddl(&core, "alice", "CREATE TABLE t (id BIGINT PRIMARY KEY)");
    let mut session = SessionState::default();
    session.allow_ddl();
    for name in ["\"lock\"", "\"table\"", "\"mode\"", "\"share\"", "\"in\""] {
        core.execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            &format!("CREATE TEMP TABLE {name} (id BIGINT PRIMARY KEY)"),
        )
        .unwrap_or_else(|e| panic!("{name}: {} {e}", e.wire_code()));
    }
    let mut txn = core.new_session_transaction();
    run_as(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
    assert_eq!(
        run_as(
            &core,
            "alice",
            &mut session,
            &mut txn,
            "LOCK TABLE t IN SHARE MODE NOWAIT"
        ),
        Ok(SqlOutcome::Lock)
    );
    assert_eq!(
        code(run_as(
            &core,
            "alice",
            &mut session,
            &mut txn,
            "LOCK TABLE \"lock\""
        )),
        "0A000"
    );
}
