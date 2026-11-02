//! users ファイルのユーザーとカタログのロールの対応づけ（SQL-39・WIRE-2、Issue #1971）の結合テスト。
//!
//! 固定する契約: 対応は自テナント内の同名ロールだけ（バイト一致・他テナントは引けない）、ロールが 0 件の
//! テナントは「カタログが正ではない」、`NOLOGIN`・期限切れのロールは属性つきで返す（拒否は wire 側）、
//! `DROP`／`RENAME` に追随すること。認証の往復と SQLSTATE は wire-server 側の結合テストが担う。

use engine::core::{EngineCore, UsersFileRoleBinding};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_core(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

fn bind(core: &EngineCore, tenant: &str, user: &str) -> UsersFileRoleBinding {
    core.resolve_users_file_role(tenant, user).expect("lookup")
}

#[test]
fn zero_roles_means_catalog_is_not_authoritative() {
    let (_g, core) = open_core("sql39-ufr-empty");
    assert_eq!(
        bind(&core, "t1", "alice"),
        UsersFileRoleBinding::CatalogNotAuthoritative
    );
}

#[test]
fn same_name_role_in_own_tenant_is_found_with_attributes() {
    let (_g, core) = open_core("sql39-ufr-found");
    ok(&core, "t1", "CREATE ROLE alice LOGIN CONNECTION LIMIT 3");
    ok(&core, "t1", "CREATE ROLE nologin NOLOGIN");
    ok(
        &core,
        "t1",
        "CREATE ROLE old LOGIN VALID UNTIL '2000-01-01 00:00:00+00'",
    );
    let UsersFileRoleBinding::Role(a) = bind(&core, "t1", "alice") else {
        panic!("alice must be found");
    };
    assert_eq!(a.tenant_id, "t1");
    assert_eq!(a.role_name, "alice");
    assert!(a.login);
    assert_eq!(a.connection_limit, 3);
    let UsersFileRoleBinding::Role(n) = bind(&core, "t1", "nologin") else {
        panic!("nologin must be found");
    };
    assert!(!n.login);
    let UsersFileRoleBinding::Role(o) = bind(&core, "t1", "old") else {
        panic!("old must be found");
    };
    assert!(o.is_expired());
    assert!(!format!("{a:?}").contains("alice"));
}

#[test]
fn other_tenant_role_and_case_variants_are_not_matched() {
    let (_g, core) = open_core("sql39-ufr-tenant");
    ok(&core, "t1", "CREATE ROLE bob LOGIN");
    ok(&core, "t2", "CREATE ROLE alice LOGIN");
    assert_eq!(bind(&core, "t1", "alice"), UsersFileRoleBinding::Missing);
    assert_eq!(bind(&core, "t1", "Bob"), UsersFileRoleBinding::Missing);
    assert_eq!(bind(&core, "t1", ""), UsersFileRoleBinding::Missing);
    assert_eq!(
        bind(&core, "t3", "alice"),
        UsersFileRoleBinding::CatalogNotAuthoritative
    );
}

#[test]
fn drop_and_rename_are_followed() {
    let (_g, core) = open_core("sql39-ufr-drop");
    ok(&core, "t1", "CREATE ROLE alice LOGIN");
    ok(&core, "t1", "CREATE ROLE bob LOGIN");
    ok(&core, "t1", "ALTER ROLE alice RENAME TO carol");
    assert_eq!(bind(&core, "t1", "alice"), UsersFileRoleBinding::Missing);
    assert!(matches!(
        bind(&core, "t1", "carol"),
        UsersFileRoleBinding::Role(_)
    ));
    ok(&core, "t1", "DROP ROLE carol");
    assert_eq!(bind(&core, "t1", "carol"), UsersFileRoleBinding::Missing);
    ok(&core, "t1", "DROP ROLE bob");
    assert_eq!(
        bind(&core, "t1", "bob"),
        UsersFileRoleBinding::CatalogNotAuthoritative
    );
}
