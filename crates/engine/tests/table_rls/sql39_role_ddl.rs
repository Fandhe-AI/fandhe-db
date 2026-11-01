//! `CREATE ROLE`／`CREATE USER`／`DROP ROLE`（SQL-39 のポインタ、Issue #1962）の SQL 水準の結合テスト。
//! 設計記録は `docs/design/role-privilege-model.md`。
//!
//! 固定する契約: 権限ゲートが対象の有無より前にあること（`42501`）、`42710`・`42704`・`42939`・`42601`・
//! `0A000` の分類、テナント境界（同名のロールをテナントごとに独立に持てる・他テナントのロールは不在と
//! 同じ応答）、明示トランザクション内の拒否、複数名の `DROP` の原子性、パスワードの平文が応答にもデータ
//! ベースファイルにも出ないこと。検証子の中身・メンバーシップ・ACL 依存は crate 内テスト
//! （`sql::role_ddl::tests`）が担う。

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
fn ddl_permission_gate_precedes_everything() {
    let (_g, _p, core) = open_core("sql39-gate");
    let mut allowed = allowed_session();
    ok(&core, "alice", &mut allowed, "CREATE ROLE exists_r");
    let mut denied = SessionState::default();
    for sql in [
        "CREATE ROLE new_r",
        "CREATE ROLE exists_r",
        "CREATE USER new_u PASSWORD 'x'",
        "DROP ROLE exists_r",
        "DROP ROLE nosuch",
        "DROP ROLE IF EXISTS nosuch",
    ] {
        assert_eq!(code(&core, "alice", &mut denied, sql), "42501", "{sql}");
    }
    // 拒否されたセッションの試行は何も変えない。
    assert_eq!(
        code(&core, "alice", &mut allowed, "CREATE ROLE exists_r"),
        "42710"
    );
    ok(&core, "alice", &mut allowed, "DROP ROLE exists_r");
}

#[test]
fn create_duplicate_and_recreate_after_drop() {
    let (_g, _p, core) = open_core("sql39-dup");
    let mut s = allowed_session();
    assert!(matches!(
        ok(&core, "alice", &mut s, "CREATE ROLE r1"),
        SqlOutcome::CreateRole(_)
    ));
    assert_eq!(code(&core, "alice", &mut s, "CREATE ROLE r1"), "42710");
    assert_eq!(code(&core, "alice", &mut s, "CREATE USER r1"), "42710");
    assert!(matches!(
        ok(
            &core,
            "alice",
            &mut s,
            "CREATE USER u1 WITH LOGIN PASSWORD 'pw'"
        ),
        SqlOutcome::CreateRole(_)
    ));
    assert!(matches!(
        ok(&core, "alice", &mut s, "DROP ROLE r1"),
        SqlOutcome::DropRole(o) if !o.skipped()
    ));
    ok(&core, "alice", &mut s, "CREATE ROLE r1");
    assert!(matches!(
        ok(&core, "alice", &mut s, "DROP USER IF EXISTS nosuch"),
        SqlOutcome::DropRole(o) if o.skipped()
    ));
    assert_eq!(code(&core, "alice", &mut s, "DROP ROLE nosuch"), "42704");
}

#[test]
fn tenant_boundary_is_not_an_existence_oracle() {
    let (_g, _p, core) = open_core("sql39-tenant");
    let mut s = allowed_session();
    ok(&core, "alice", &mut s, "CREATE ROLE shared");
    ok(&core, "bob", &mut s, "CREATE ROLE shared");
    ok(&core, "alice", &mut s, "CREATE ROLE only_alice");
    let cross = run(&core, "bob", &mut s, "DROP ROLE only_alice").expect_err("cross");
    assert_eq!(cross.wire_code(), "42704");
    assert_eq!(cross.client_message(), "role \"only_alice\" does not exist");
    let absent = run(&core, "bob", &mut s, "DROP ROLE nobody").expect_err("absent");
    assert_eq!(absent.wire_code(), "42704");
    assert!(matches!(
        ok(&core, "bob", &mut s, "DROP ROLE IF EXISTS only_alice"),
        SqlOutcome::DropRole(o) if o.skipped()
    ));
    // alice 側は無傷。
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE ROLE only_alice"),
        "42710"
    );
}

#[test]
fn rejected_forms_map_to_documented_sqlstates() {
    let (_g, _p, core) = open_core("sql39-forms");
    let mut s = allowed_session();
    for (sql, want) in [
        ("CREATE ROLE r SUPERUSER", "42501"),
        ("CREATE ROLE r BYPASSRLS", "42501"),
        ("CREATE ROLE r REPLICATION", "42501"),
        ("CREATE ROLE r IN ROLE other", "0A000"),
        ("CREATE ROLE r VALID UNTIL '2030-01-01'", "0A000"),
        ("CREATE ROLE r CONNECTION LIMIT 3", "0A000"),
        ("CREATE ROLE r UNENCRYPTED PASSWORD 'x'", "0A000"),
        ("CREATE ROLE r LOGIN NOLOGIN", "42601"),
        ("CREATE ROLE r BOGUS", "42601"),
        ("CREATE ROLE s.r", "42601"),
        ("CREATE ROLE pg_x", "42939"),
        ("CREATE ROLE public", "42939"),
        ("CREATE ROLE none", "42939"),
        ("CREATE ROLE r PASSWORD 'pässword'", "22023"),
        ("DROP ROLE", "42601"),
    ] {
        assert_eq!(code(&core, "alice", &mut s, sql), want, "{sql}");
    }
    // 拒否された文はロールを作らない。
    ok(&core, "alice", &mut s, "CREATE ROLE r");
}

#[test]
fn explicit_transaction_rejects_role_ddl() {
    let (_g, _p, core) = open_core("sql39-tx");
    let mut allowed = allowed_session();
    for sql in ["CREATE ROLE r", "DROP ROLE r"] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut allowed, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut allowed, &mut txn, sql)
            .expect_err("role DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        core.execute_sql_in_txn(&ctx("alice"), &mut allowed, &mut txn, "ROLLBACK")
            .expect("rollback");

        let mut denied = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut denied, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut denied, &mut txn, sql)
            .expect_err("denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
        core.execute_sql_in_txn(&ctx("alice"), &mut denied, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    // Tx 内の試行は何も作っていない。
    let mut s = allowed_session();
    ok(&core, "alice", &mut s, "CREATE ROLE r");
}

#[test]
fn multi_name_drop_is_atomic() {
    let (_g, _p, core) = open_core("sql39-multi");
    let mut s = allowed_session();
    ok(&core, "alice", &mut s, "CREATE ROLE a");
    ok(&core, "alice", &mut s, "CREATE ROLE b");
    assert_eq!(
        code(&core, "alice", &mut s, "DROP ROLE a, missing, b"),
        "42704"
    );
    assert_eq!(code(&core, "alice", &mut s, "CREATE ROLE a"), "42710");
    assert_eq!(code(&core, "alice", &mut s, "CREATE ROLE b"), "42710");
    ok(&core, "alice", &mut s, "DROP ROLE a, b");
    ok(&core, "alice", &mut s, "CREATE ROLE a");
}

#[test]
fn password_plaintext_never_leaks() {
    let marker = "PLAINTEXT-MARKER-4471";
    let (_g, path, core) = open_core("sql39-leak");
    let mut s = allowed_session();
    let mut surfaces: Vec<String> = Vec::new();
    for sql in [
        format!("CREATE ROLE ok_r PASSWORD '{marker}'"),
        format!("CREATE ROLE ok_r PASSWORD '{marker}'"),
        format!("CREATE ROLE bad_r PASSWORD '{marker}' LOGIN NOLOGIN"),
        format!("CREATE ROLE bad_r PASSWORD '{marker}' SUPERUSER"),
        format!("CREATE ROLE bad_r UNENCRYPTED PASSWORD '{marker}'"),
        format!("CREATE ROLE bad_r PASSWORD '{marker}' BOGUS"),
        format!("CREATE ROLE pg_bad PASSWORD '{marker}'"),
        format!("CREATE ROLE bad_r PASSWORD '{marker}é'"),
    ] {
        match run(&core, "alice", &mut s, &sql) {
            Ok(o) => surfaces.push(format!("{o:?}")),
            Err(e) => {
                surfaces.push(e.to_string());
                surfaces.push(format!("{e:?}"));
                surfaces.push(e.client_message());
            }
        }
    }
    let mut denied = SessionState::default();
    let e = run(
        &core,
        "alice",
        &mut denied,
        &format!("CREATE ROLE x PASSWORD '{marker}'"),
    )
    .expect_err("denied");
    surfaces.push(format!("{e:?}"));
    for text in &surfaces {
        assert!(!text.contains(marker), "{text}");
    }
    drop(core);
    let bytes = std::fs::read(&path).expect("read db file");
    assert!(!bytes.windows(marker.len()).any(|w| w == marker.as_bytes()));
}
