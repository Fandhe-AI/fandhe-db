//! カタログのロールのログイン情報（SQL-39・WIRE-18、Issue #2233）の SQL 水準の結合テスト。
//!
//! 固定する契約: `VALID UNTIL`・`CONNECTION LIMIT` の保存と拒否の SQLSTATE、ログイン識別子が
//! `CREATE`／`DROP`／`RENAME` に追随すること、テナント境界（他テナントのロールは識別子から引けない）、
//! 検証子が `Debug` に出ないこと。認証の往復そのものは wire-server 側の結合テストが担う。

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

fn open_core(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = allowed_session();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

fn lookup(
    core: &EngineCore,
    tenant: &str,
    role: &str,
) -> Option<engine::core::LoginRoleCredentials> {
    let ident = EngineCore::login_identifier(tenant, role).expect("identifier");
    core.resolve_login_role(&ident).expect("lookup")
}

#[test]
fn login_info_is_stored_and_resolved_with_its_tenant() {
    let (_g, core) = open_core("sql39-login-basic");
    ok(
        &core,
        "tenant-a",
        "CREATE ROLE app LOGIN PASSWORD 'pw-app' VALID UNTIL '2999-01-01 00:00:00+00' CONNECTION LIMIT 2",
    );
    let c = lookup(&core, "tenant-a", "app").expect("registered");
    assert_eq!(c.tenant_id, "tenant-a");
    assert_eq!(c.role_name, "app");
    assert!(c.login);
    assert_eq!(c.connection_limit, 2);
    assert!(c.valid_until.is_some());
    assert!(!c.is_expired());
    assert!(c.scram_verifier.starts_with("SCRAM-SHA-256$"));
    assert!(!format!("{c:?}").contains("SCRAM-SHA-256"));
}

#[test]
fn expiry_infinity_and_past_values() {
    let (_g, core) = open_core("sql39-login-expiry");
    ok(
        &core,
        "t",
        "CREATE ROLE past LOGIN VALID UNTIL '2000-01-01 00:00:00+00'",
    );
    ok(&core, "t", "CREATE ROLE inf LOGIN VALID UNTIL 'infinity'");
    assert!(lookup(&core, "t", "past").expect("past").is_expired());
    let inf = lookup(&core, "t", "inf").expect("inf");
    assert_eq!(inf.valid_until, None);
    assert!(!inf.is_expired());
    // ALTER は指定した項目だけを変える。
    ok(
        &core,
        "t",
        "ALTER ROLE inf VALID UNTIL '2000-01-01 00:00:00+00'",
    );
    assert!(lookup(&core, "t", "inf").expect("inf").is_expired());
    ok(&core, "t", "ALTER ROLE inf CONNECTION LIMIT 0");
    let inf = lookup(&core, "t", "inf").expect("inf");
    assert_eq!(inf.connection_limit, 0);
    assert!(inf.is_expired());
    ok(&core, "t", "ALTER ROLE inf VALID UNTIL 'infinity'");
    assert!(!lookup(&core, "t", "inf").expect("inf").is_expired());
}

#[test]
fn nologin_role_is_resolvable_but_flagged() {
    let (_g, core) = open_core("sql39-login-nologin");
    ok(&core, "t", "CREATE ROLE grp NOLOGIN PASSWORD 'pw'");
    let c = lookup(&core, "t", "grp").expect("resolvable");
    assert!(!c.login);
}

#[test]
fn identifier_follows_rename_and_drop() {
    let (_g, core) = open_core("sql39-login-follow");
    ok(&core, "t", "CREATE ROLE old_name LOGIN");
    ok(&core, "t", "ALTER ROLE old_name RENAME TO new_name");
    assert!(lookup(&core, "t", "old_name").is_none());
    assert!(lookup(&core, "t", "new_name").is_some());
    ok(&core, "t", "DROP ROLE new_name");
    assert!(lookup(&core, "t", "new_name").is_none());
}

#[test]
fn identifiers_never_cross_tenants() {
    let (_g, core) = open_core("sql39-login-tenant");
    ok(&core, "t1", "CREATE ROLE shared LOGIN");
    ok(&core, "t2", "CREATE ROLE only_t2 LOGIN");
    assert_eq!(lookup(&core, "t1", "shared").expect("t1").tenant_id, "t1");
    // t1 の修飾子と t2 にしか無いロール名の組み合わせは解決できない。
    assert!(lookup(&core, "t1", "only_t2").is_none());
    // 形が不正な識別子は未登録と同じ None。
    assert!(core.resolve_login_role("alice").expect("shape").is_none());
    assert!(!EngineCore::is_login_identifier("alice"));
}

#[test]
fn sync_login_names_is_idempotent_on_current_db() {
    let (_g, core) = open_core("sql39-login-sync");
    ok(&core, "t", "CREATE ROLE a LOGIN");
    assert_eq!(core.sync_login_names().expect("sync"), 0);
}

#[test]
fn date_only_valid_until_is_accepted() {
    let (_g, core) = open_core("sql39-login-date-only");
    ok(
        &core,
        "t",
        "CREATE ROLE past LOGIN VALID UNTIL '2000-01-01'",
    );
    ok(
        &core,
        "t",
        "CREATE ROLE future LOGIN VALID UNTIL '2999-01-01'",
    );
    let past = lookup(&core, "t", "past").expect("past");
    assert!(past.valid_until.is_some());
    assert!(past.is_expired());
    assert!(!lookup(&core, "t", "future").expect("future").is_expired());
    ok(&core, "t", "ALTER ROLE future VALID UNTIL '2000-01-01'");
    assert!(lookup(&core, "t", "future").expect("future").is_expired());
}

#[test]
fn timestamptz_valid_until_with_surrounding_spaces_is_accepted() {
    let (_g, core) = open_core("sql39-login-trim-tz");
    ok(
        &core,
        "t",
        "CREATE ROLE spaced LOGIN VALID UNTIL '  2999-01-01 00:00:00+00  '",
    );
    assert!(!lookup(&core, "t", "spaced").expect("spaced").is_expired());
}

#[test]
fn unregistered_login_resolves_to_none_even_if_dummy_tenant_exists() {
    let (_g, core) = open_core("sql39-login-dummy-tenant");
    // ダミー参照用のテナント名 "-" が実在しても、未登録の識別子は None のまま。
    ok(&core, "-", "CREATE ROLE ghost LOGIN");
    ok(&core, "t", "CREATE ROLE real LOGIN");
    let ident = EngineCore::login_identifier("t", "ghost").expect("identifier");
    assert!(core.resolve_login_role(&ident).expect("lookup").is_none());
}
