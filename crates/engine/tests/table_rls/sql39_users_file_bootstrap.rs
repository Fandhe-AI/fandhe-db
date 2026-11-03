//! users ファイルの初期ブートストラップ（SQL-39 (3)(5)・WIRE-2、Issue #2238）の結合テスト。
//!
//! 固定する契約: 投入後はテナントが「カタログが正」になりマーカーが立つこと、初期ロールの全権限が
//! 「セッションのロール本人のオブジェクト権限」だけに作用し DDL を通さないこと、全権限フラグを持たない
//! 束縛セッションは ACL で拒否されること（フラグは認証層だけが立てる）。認証の往復・起動ログは
//! wire-server 側の結合テストが担う。

use engine::core::{BootstrapUser, EngineCore, UsersFileRoleBinding};
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

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn user(line: usize, name: &str, tenant: &str, ddl: bool) -> BootstrapUser {
    BootstrapUser {
        line,
        username: name.to_string(),
        tenant_id: tenant.to_string(),
        phc: "$argon2id$dummy".to_string(),
        scram_verifier: None,
        ddl_listed: ddl,
    }
}

fn code(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(tenant), s, sql) {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 認証層が行う束縛の写し（ロールの束縛・全権限フラグ）。
fn bound_session(core: &EngineCore, tenant: &str, name: &str, with_flag: bool) -> SessionState {
    let UsersFileRoleBinding::Role(c) = core.resolve_users_file_role(tenant, name).expect("find")
    else {
        panic!("{name} must be bound");
    };
    let mut s = SessionState::default();
    s.bind_role_ids(c.role_id, c.role_id);
    if with_flag && c.initial_privileges.all_object_privileges {
        s.grant_bootstrap_all_privileges();
    }
    s
}

#[test]
fn bootstrap_makes_catalog_authoritative_and_is_idempotent() {
    let (_g, core) = open_core("sql39-boot-auth");
    assert_eq!(
        core.resolve_users_file_role("t1", "alice").expect("pre"),
        UsersFileRoleBinding::CatalogNotAuthoritative
    );
    let users = [user(1, "alice", "t1", true), user(2, "bob", "t1", false)];
    let r = core.bootstrap_users_file(&users).expect("boot");
    assert_eq!(r.tenants_bootstrapped, 1);
    let UsersFileRoleBinding::Role(a) = core.resolve_users_file_role("t1", "alice").expect("a")
    else {
        panic!("alice must exist");
    };
    assert!(a.bootstrapped && a.login && a.initial_privileges.ddl);
    assert_eq!(a.legacy_phc, "$argon2id$dummy");
    // 他のテナントはカタログが正にならない。
    assert_eq!(
        core.resolve_users_file_role("t2", "alice").expect("t2"),
        UsersFileRoleBinding::CatalogNotAuthoritative
    );
    let again = core.bootstrap_users_file(&users).expect("again");
    assert_eq!(again.tenants_bootstrapped, 0);
    assert!(again.entries.is_empty());
}

#[test]
fn bootstrapped_role_has_all_object_privileges_but_no_ddl() {
    let (_g, core) = open_core("sql39-boot-privs");
    core.bootstrap_users_file(&[user(1, "bob", "t1", false)])
        .expect("boot");
    // 運用者の DDL セッションで表を作る（束縛なし）。
    let mut admin = SessionState::default();
    admin.allow_ddl();
    assert_eq!(
        code(&core, "t1", &mut admin, "CREATE TABLE t (id BIGINT)"),
        "ok"
    );
    // 全権限のあるセッションは ACL なしで読み書きできる。
    let mut bob = bound_session(&core, "t1", "bob", true);
    assert_eq!(
        code(&core, "t1", &mut bob, "INSERT INTO t VALUES (1)"),
        "ok"
    );
    assert_eq!(code(&core, "t1", &mut bob, "SELECT * FROM t"), "ok");
    // DDL は全権限では通らない。
    assert_eq!(
        code(&core, "t1", &mut bob, "CREATE TABLE u (id BIGINT)"),
        "42501"
    );
    // フラグを持たない束縛セッションは ACL で拒否される（フラグは認証層だけが立てる）。
    let mut plain = bound_session(&core, "t1", "bob", false);
    assert_eq!(code(&core, "t1", &mut plain, "SELECT * FROM t"), "42501");
}
