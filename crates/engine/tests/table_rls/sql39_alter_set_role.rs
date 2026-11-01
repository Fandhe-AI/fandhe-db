//! `ALTER ROLE`／`ALTER USER`／`SET ROLE`／`RESET ROLE`（SQL-39 のポインタ、Issue #1963）の SQL 水準の結合テスト。
//! 設計記録は `docs/design/role-privilege-model.md`。
//!
//! 固定する契約: `ALTER ROLE` の DDL 権限ゲート（対象の有無より前の `42501`）と SQLSTATE の分類、改名後の
//! 名前解決、パスワード平文の非漏えい、`SET ROLE` の受入基準（メンバーシップの無いロールへは `42501`・他テナント
//! にだけ在るロールは不在と同じ `42704`・`SET ROLE` はテナント境界と DDL 権限を変えない）。
//! メンバーシップを書く API は crate 内限定のため、SET 真の経路の成否は `sql::role_ddl::tests` が固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn open_core(label: &str) -> (CleanupGuard, std::path::PathBuf, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        path,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    run(core, tenant, s, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> &'static str {
    match run(core, tenant, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

#[test]
fn alter_role_ddl_gate_precedes_everything() {
    let (_g, _p, core) = open_core("sql39-alter-gate");
    let mut allowed = allowed_session();
    ok(&core, "alice", &mut allowed, "CREATE ROLE r1");
    let mut denied = SessionState::default();
    for sql in [
        "ALTER ROLE r1 LOGIN",
        "ALTER ROLE nosuch LOGIN",
        "ALTER ROLE r1 RENAME TO r2",
        "ALTER USER nosuch PASSWORD 'x'",
        "ALTER ROLE r1 SUPERUSER",
    ] {
        assert_eq!(code(&core, "alice", &mut denied, sql), "42501", "{sql}");
    }
    // 拒否されたセッションの試行は何も変えない。
    assert_eq!(
        code(&core, "alice", &mut allowed, "ALTER ROLE r2 LOGIN"),
        "42704"
    );
    assert!(matches!(
        ok(&core, "alice", &mut allowed, "ALTER ROLE r1 LOGIN"),
        SqlOutcome::AlterRole(_)
    ));
}

#[test]
fn alter_role_sqlstate_classification() {
    let (_g, _p, core) = open_core("sql39-alter-codes");
    let mut s = allowed_session();
    ok(&core, "alice", &mut s, "CREATE ROLE a");
    ok(&core, "alice", &mut s, "CREATE ROLE b");
    ok(&core, "bob", &mut s, "CREATE ROLE only_bob");
    for (sql, expected) in [
        ("ALTER ROLE nosuch NOLOGIN", "42704"),
        // 他テナントにだけ在るロールは不在と同じ。
        ("ALTER ROLE only_bob NOLOGIN", "42704"),
        ("ALTER ROLE only_bob RENAME TO x", "42704"),
        ("ALTER ROLE a RENAME TO b", "42710"),
        ("ALTER ROLE a RENAME TO pg_x", "42939"),
        ("ALTER ROLE a RENAME TO public", "42939"),
        ("ALTER ROLE a RENAME TO current_user", "42601"),
        ("ALTER ROLE a LOGIN NOLOGIN", "42601"),
        ("ALTER ROLE a BOGUS", "42601"),
        ("ALTER ROLE a SUPERUSER", "42501"),
        ("ALTER ROLE a BYPASSRLS", "42501"),
        ("ALTER ROLE a VALID UNTIL 'infinity'", "0A000"),
        ("ALTER ROLE a CONNECTION LIMIT 1", "0A000"),
        ("ALTER ROLE a SET search_path = x", "0A000"),
        ("ALTER ROLE ALL SET search_path = x", "0A000"),
        ("ALTER ROLE current_user NOLOGIN", "0A000"),
    ] {
        assert_eq!(code(&core, "alice", &mut s, sql), expected, "{sql}");
    }
}

#[test]
fn rename_moves_the_name_for_later_statements() {
    let (_g, _p, core) = open_core("sql39-rename");
    let mut s = allowed_session();
    ok(&core, "alice", &mut s, "CREATE ROLE old_name");
    ok(
        &core,
        "alice",
        &mut s,
        "ALTER ROLE old_name RENAME TO new_name",
    );
    assert_eq!(code(&core, "alice", &mut s, "DROP ROLE old_name"), "42704");
    ok(&core, "alice", &mut s, "CREATE ROLE old_name");
    ok(&core, "alice", &mut s, "DROP ROLE new_name");
}

#[test]
fn rename_of_the_session_role_is_rejected() {
    let (_g, _p, core) = open_core("sql39-rename-self");
    let mut setup = allowed_session();
    ok(&core, "alice", &mut setup, "CREATE ROLE me");
    // 新しい DB の ID 採番は 1 始まり（`sql40_grant_revoke` と同じ前提）。
    let mut s = allowed_session();
    s.bind_role_ids(1, 1);
    assert_eq!(
        code(&core, "alice", &mut s, "ALTER ROLE me RENAME TO other"),
        "0A000"
    );
    ok(&core, "alice", &mut s, "ALTER ROLE me NOLOGIN");
}

#[test]
fn alter_role_is_rejected_inside_explicit_transactions() {
    let (_g, _p, core) = open_core("sql39-alter-txn");
    let mut setup = allowed_session();
    ok(&core, "alice", &mut setup, "CREATE ROLE r1");
    for (allowed, expected) in [(false, "42501"), (true, "0A000")] {
        let mut s = if allowed {
            allowed_session()
        } else {
            SessionState::default()
        };
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let e = core
            .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ALTER ROLE r1 LOGIN")
            .expect_err("rejected");
        assert_eq!(e.wire_code(), expected);
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

#[test]
fn alter_role_password_plaintext_never_leaks() {
    let (_g, path, core) = open_core("sql39-alter-pw");
    let marker = "ZZ-alter-pw-marker-ZZ";
    let mut s = allowed_session();
    ok(&core, "alice", &mut s, "CREATE USER u1");
    ok(
        &core,
        "alice",
        &mut s,
        &format!("ALTER USER u1 PASSWORD '{marker}'"),
    );
    for sql in [
        format!("ALTER ROLE u1 PASSWORD '{marker}' BOGUS"),
        format!("ALTER ROLE nosuch PASSWORD '{marker}'"),
        format!("ALTER ROLE u1 PASSWORD '{marker}' SUPERUSER"),
    ] {
        let e = run(&core, "alice", &mut s, &sql).expect_err("rejected");
        assert!(!e.to_string().contains(marker), "{e}");
        assert!(!e.client_message().contains(marker));
    }
    drop(core);
    let bytes = std::fs::read(&path).expect("read db file");
    assert!(
        !bytes.windows(marker.len()).any(|w| w == marker.as_bytes()),
        "plaintext password must not be stored"
    );
}

/// 受入基準 1: ロールが未束縛のセッション（現状の wire の全セッション）の `SET ROLE <名前>` は、存在する
/// ロール・存在しないロール・他テナントのロールのどれでも同じ `42501`。`RESET ROLE`・`SET ROLE NONE` は成功する。
#[test]
fn set_role_on_unbound_session_is_denied_uniformly() {
    let (_g, _p, core) = open_core("sql39-set-unbound");
    let mut admin = allowed_session();
    ok(&core, "alice", &mut admin, "CREATE ROLE r1");
    ok(&core, "bob", &mut admin, "CREATE ROLE only_bob");
    let mut s = SessionState::default();
    for sql in [
        "SET ROLE r1",
        "SET ROLE nosuch",
        "SET ROLE only_bob",
        "SET SESSION ROLE r1",
        "SET role TO r1",
        "SET ROLE 'r1'",
    ] {
        assert_eq!(code(&core, "alice", &mut s, sql), "42501", "{sql}");
    }
    assert!(matches!(
        ok(&core, "alice", &mut s, "RESET ROLE"),
        SqlOutcome::SetRole { reset: true }
    ));
    assert!(matches!(
        ok(&core, "alice", &mut s, "SET ROLE NONE"),
        SqlOutcome::SetRole { reset: false }
    ));
    assert!(matches!(
        ok(&core, "alice", &mut s, "SET role TO DEFAULT"),
        SqlOutcome::SetRole { reset: false }
    ));
    assert_eq!(s.current_role_id(), None);
    assert_eq!(code(&core, "alice", &mut s, "SET LOCAL ROLE r1"), "0A000");
    assert_eq!(code(&core, "alice", &mut s, "SET ROLE s.r1"), "42601");
    assert_eq!(code(&core, "alice", &mut s, "SET ROLE"), "42601");
}

/// 受入基準 1（束縛済み）: メンバーシップを持たないロールへの `SET ROLE` は `42501`、他テナントにだけ在る
/// ロール・不在のロールは同じ `42704`。自分自身への `SET ROLE` は成功し、セッションは変わらない。
#[test]
fn set_role_without_membership_is_denied_for_bound_session() {
    let (_g, _p, core) = open_core("sql39-set-bound");
    let mut admin = allowed_session();
    ok(&core, "alice", &mut admin, "CREATE ROLE me");
    ok(&core, "alice", &mut admin, "CREATE ROLE other");
    ok(&core, "bob", &mut admin, "CREATE ROLE only_bob");
    // 新しい DB の ID 採番は 1 始まり（`me` = 1）。
    let mut s = SessionState::default();
    s.bind_role_ids(1, 1);
    assert_eq!(code(&core, "alice", &mut s, "SET ROLE other"), "42501");
    assert_eq!(code(&core, "alice", &mut s, "SET ROLE only_bob"), "42704");
    assert_eq!(code(&core, "alice", &mut s, "SET ROLE nosuch"), "42704");
    assert_eq!(s.current_role_id(), Some(1));
    ok(&core, "alice", &mut s, "SET ROLE me");
    assert_eq!(s.current_role_id(), Some(1));
    ok(&core, "alice", &mut s, "RESET ROLE");
    assert_eq!(s.session_role_id(), Some(1));
}

/// 受入基準 2: `SET ROLE` はテナント境界・DDL 権限・セッションのロールを変えない。
#[test]
fn set_role_does_not_change_tenant_boundary_or_ddl_permission() {
    let (_g, _p, core) = open_core("sql39-set-boundary");
    let mut admin = allowed_session();
    ok(
        &core,
        "alice",
        &mut admin,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        "alice",
        &mut admin,
        "INSERT INTO t (id, name) VALUES (1, 'a')",
    );
    ok(&core, "alice", &mut admin, "CREATE ROLE me");
    ok(&core, "bob", &mut admin, "CREATE ROLE bobs");
    let mut s = SessionState::default();
    s.bind_role_ids(1, 1);
    let read = |core: &EngineCore, tenant: &str, s: &mut SessionState| match core
        .execute_sql_in_session(&ctx(tenant), s, "SELECT id FROM t")
    {
        Ok(SqlOutcome::Query(r)) => r.rows.len(),
        Err(_) => 0,
        other => panic!("unexpected: {other:?}"),
    };
    let before = read(&core, "alice", &mut s);
    assert_eq!(before, 1);
    ok(&core, "alice", &mut s, "SET ROLE me");
    assert_eq!(read(&core, "alice", &mut s), before);
    // 他テナントのセッションが SET ROLE しても alice の行は見えない。
    let mut bob = SessionState::default();
    bob.bind_role_ids(2, 2);
    ok(&core, "bob", &mut bob, "SET ROLE bobs");
    assert_eq!(read(&core, "bob", &mut bob), 0);
    // DDL 権限は SET ROLE で緩まない。
    assert_eq!(code(&core, "alice", &mut s, "CREATE ROLE sneaky"), "42501");
    assert_eq!(
        code(&core, "alice", &mut s, "ALTER ROLE me NOLOGIN"),
        "42501"
    );
}

/// `SET ROLE`／`RESET ROLE` は明示トランザクション内・READ ONLY でも実行でき、拒否はトランザクションを
/// 壊さない種類（`42501` で Failed へ遷移するのは通常どおり）。未束縛の `RESET ROLE` は成功する。
#[test]
fn set_role_inside_explicit_transaction() {
    let (_g, _p, core) = open_core("sql39-set-txn");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN READ ONLY")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "RESET ROLE")
        .expect("reset role in read only tx");
    assert!(matches!(out, SqlOutcome::SetRole { reset: true }));
    let e = core
        .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "SET ROLE nosuch")
        .expect_err("unbound");
    assert_eq!(e.wire_code(), "42501");
    core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
}
