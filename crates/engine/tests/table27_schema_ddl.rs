//! `CREATE SCHEMA` / `DROP SCHEMA`（TABLE-27・TABLE-36・SQL-23、Issue #1713）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27・TABLE-36、
//! `docs/spec/04-behavior/error-format.md` ERR-6。設計記録は
//! `docs/design/schema-tenant-namespace.md`（D1・D2・D8・D9・D10）。
//!
//! `table24_sequence_ddl.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - 再起動（Storage の再オープン）後も SCHEMA が残り、DROP 後は同名を再作成できる
//! - SCHEMA はテナント単位で独立し、他テナントにしか無い名前の DROP は存在しない名前と同一応答
//! - `IF NOT EXISTS`／`IF EXISTS` の no-op、予約名・組み込み名、DDL 実行権限ゲート
//! - 未対応の句は `0A000`、構文違反は `42601`、明示トランザクション内は `0A000`
//! - 既存の表との名前空間は別で、既存の挙動を変えない

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

fn core_over(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `t` 表を持つ DB とコアを開く。
fn open_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    (path, guard, core_over(storage))
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) {
    run_as(core, tenant, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

fn err_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> SqlSurfaceError {
    match run_as(core, tenant, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

fn code_as(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> &'static str {
    err_as(core, tenant, session, sql).wire_code()
}

fn reopen(core: EngineCore, path: &std::path::Path) -> EngineCore {
    drop(core);
    core_over(Storage::open(path).expect("reopen storage"))
}

#[test]
fn schema_survives_reopen_and_drop_frees_the_name() {
    let (p, _g, core) = open_fixture("sch-persist");
    let mut s = allowed_session();
    assert!(matches!(
        run_as(&core, "alice", &mut s, "CREATE SCHEMA s1;"),
        Ok(SqlOutcome::CreateSchema(_))
    ));
    let core = reopen(core, &p);
    assert_eq!(code_as(&core, "alice", &mut s, "CREATE SCHEMA s1"), "42P06");
    assert!(matches!(
        run_as(&core, "alice", &mut s, "DROP SCHEMA s1"),
        Ok(SqlOutcome::DropSchema(_))
    ));
    let core = reopen(core, &p);
    assert_eq!(code_as(&core, "alice", &mut s, "DROP SCHEMA s1"), "3F000");
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA s1");
}

#[test]
fn schemas_are_independent_per_tenant() {
    let (_p, _g, core) = open_fixture("sch-tenant");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA s");
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA s");
    ok_as(&core, "bob", &mut s, "DROP SCHEMA s");
    // bob の削除は alice の同名に影響しない。
    assert_eq!(code_as(&core, "alice", &mut s, "CREATE SCHEMA s"), "42P06");
    assert_eq!(code_as(&core, "bob", &mut s, "DROP SCHEMA s"), "3F000");
    ok_as(&core, "alice", &mut s, "DROP SCHEMA s");
}

#[test]
fn drop_of_other_tenants_schema_is_indistinguishable_from_unknown_name() {
    let (_p, _g, core) = open_fixture("sch-hidden");
    let mut s = allowed_session();
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA bobs");
    let ghost = err_as(&core, "alice", &mut s, "DROP SCHEMA ghost");
    let other = err_as(&core, "alice", &mut s, "DROP SCHEMA bobs");
    assert_eq!(ghost.wire_code(), "3F000");
    assert_eq!(other.wire_code(), "3F000");
    assert_eq!(
        ghost.to_string().replace("ghost", "X"),
        other.to_string().replace("bobs", "X")
    );
    for e in [ghost, other] {
        let text = e.to_string();
        assert!(!text.contains('#'), "internal key leaked: {text}");
        assert!(
            !text.chars().any(|c| c.is_ascii_digit()),
            "internal id leaked: {text}"
        );
    }
    // IF EXISTS は存在の有無を問わず成功し、bob の SCHEMA は残る。
    ok_as(&core, "alice", &mut s, "DROP SCHEMA IF EXISTS bobs");
    assert_eq!(code_as(&core, "bob", &mut s, "CREATE SCHEMA bobs"), "42P06");
}

#[test]
fn if_not_exists_and_if_exists_are_noops() {
    let (_p, _g, core) = open_fixture("sch-ifs");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA IF NOT EXISTS s");
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA IF NOT EXISTS s");
    assert_eq!(code_as(&core, "alice", &mut s, "CREATE SCHEMA s"), "42P06");
    ok_as(&core, "alice", &mut s, "DROP SCHEMA IF EXISTS s");
    ok_as(&core, "alice", &mut s, "DROP SCHEMA IF EXISTS s");
    ok_as(
        &core,
        "alice",
        &mut s,
        "DROP SCHEMA IF EXISTS nothere RESTRICT",
    );
    assert_eq!(code_as(&core, "alice", &mut s, "DROP SCHEMA s"), "3F000");
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA IF NOT EXISTS public");
    assert_eq!(
        code_as(&core, "alice", &mut s, "CREATE SCHEMA public"),
        "42P06"
    );
}

#[test]
fn reserved_and_builtin_names() {
    let (_p, _g, core) = open_fixture("sch-reserved");
    let mut s = allowed_session();
    for name in ["pg_x", "PG_X", "pg_catalog", "Pg_Temp"] {
        for sql in [
            format!("CREATE SCHEMA {name}"),
            format!("CREATE SCHEMA IF NOT EXISTS {name}"),
        ] {
            assert_eq!(code_as(&core, "alice", &mut s, &sql), "42939", "{sql}");
        }
    }
    assert_eq!(
        code_as(&core, "alice", &mut s, "CREATE SCHEMA information_schema"),
        "42P06"
    );
    for name in ["public", "PUBLIC", "pg_catalog", "information_schema"] {
        for sql in [
            format!("DROP SCHEMA {name}"),
            format!("DROP SCHEMA IF EXISTS {name}"),
        ] {
            assert_eq!(code_as(&core, "alice", &mut s, &sql), "42501", "{sql}");
        }
    }
}

#[test]
fn ddl_permission_gate_applies_regardless_of_existence() {
    let (_p, _g, core) = open_fixture("sch-perm");
    let mut allowed = allowed_session();
    ok_as(&core, "alice", &mut allowed, "CREATE SCHEMA exists1");
    let mut denied = SessionState::default();
    for sql in [
        "CREATE SCHEMA s",
        "CREATE SCHEMA exists1",
        "CREATE SCHEMA pg_x",
        "DROP SCHEMA exists1",
        "DROP SCHEMA ghost",
        "DROP SCHEMA IF EXISTS ghost",
        "DROP SCHEMA public",
    ] {
        assert_eq!(code_as(&core, "alice", &mut denied, sql), "42501", "{sql}");
    }
    // 拒否された文は何も変えない。
    ok_as(&core, "alice", &mut allowed, "DROP SCHEMA exists1");
}

#[test]
fn syntax_table() {
    let (_p, _g, core) = open_fixture("sch-syntax");
    let mut s = allowed_session();
    for sql in [
        "CREATE SCHEMA AUTHORIZATION bob",
        "CREATE SCHEMA s AUTHORIZATION bob",
        "CREATE SCHEMA IF NOT EXISTS s AUTHORIZATION bob",
        "CREATE SCHEMA s CREATE TABLE x (a INT)",
        "DROP SCHEMA s CASCADE",
        "DROP SCHEMA IF EXISTS s CASCADE",
        "DROP SCHEMA a, b",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "0A000", "{sql}");
    }
    for sql in [
        "CREATE SCHEMA",
        "CREATE SCHEMA a.b",
        "CREATE SCHEMA 1x",
        "CREATE SCHEMA s extra",
        "DROP SCHEMA",
        "DROP SCHEMA a.b",
        "DROP SCHEMA 1x",
        "DROP SCHEMA s RESTRICT extra",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "42601", "{sql}");
    }
    // RESTRICT は既定と同義で受理される。
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA s");
    ok_as(&core, "alice", &mut s, "DROP SCHEMA s RESTRICT");
}

#[test]
fn schema_ddl_inside_explicit_transaction_is_rejected() {
    let (p, _g, core) = open_fixture("sch-txn");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA existing");
    for sql in ["CREATE SCHEMA s9", "DROP SCHEMA existing"] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, sql)
            .expect_err("schema DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed);
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    let core = reopen(core, &p);
    assert_eq!(
        code_as(&core, "alice", &mut s, "CREATE SCHEMA existing"),
        "42P06"
    );
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA s9");
}

#[test]
fn schema_names_do_not_collide_with_relations_and_existing_behavior_is_unchanged() {
    let (_p, _g, core) = open_fixture("sch-ns");
    let mut s = allowed_session();
    // 表名と同じ SCHEMA 名は別の名前空間で衝突しない。
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA t");
    ok_as(&core, "alice", &mut s, "CREATE TABLE s (a TEXT)");
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA s");
    ok_as(&core, "alice", &mut s, "SELECT a FROM s ORDER BY a LIMIT 1");
    assert_eq!(
        code_as(&core, "alice", &mut s, "CREATE TABLE s (a TEXT)"),
        "42P07"
    );
    ok_as(&core, "alice", &mut s, "DROP TABLE s");
    ok_as(&core, "alice", &mut s, "DROP SCHEMA s");
}

/// 一時表を持つセッションでも `CREATE SCHEMA`／`DROP SCHEMA` が通る（Issue #1713 の Bugbot 指摘）。
/// SCHEMA 名は関係名と別の名前空間のため、一時表名（`create`／`schema`／`drop` や SCHEMA 名と同名）と
/// トークンが一致しても 0A000 にならない。simple query（トークン走査）と extended
/// （`execute_parsed_in_session` の粗いゲート）の両経路を覆う。
#[test]
fn schema_ddl_is_not_blocked_by_session_temp_tables() {
    let (_path, _guard, core) = open_fixture("schema-with-temp");
    let mut session = allowed_session();
    for tmp in ["create", "schema", "drop", "s1"] {
        ok_as(
            &core,
            "tenant-a",
            &mut session,
            &format!("CREATE TEMP TABLE {tmp} (a INTEGER)"),
        );
    }
    ok_as(&core, "tenant-a", &mut session, "CREATE SCHEMA s1");
    ok_as(&core, "tenant-a", &mut session, "DROP SCHEMA s1");

    let parsed = core.parse_sql("CREATE SCHEMA s2").expect("parse");
    core.execute_parsed_in_session(&ctx("tenant-a"), &mut session, &parsed)
        .expect("extended CREATE SCHEMA must pass the coarse temp gate");
    let parsed = core.parse_sql("DROP SCHEMA s2").expect("parse");
    core.execute_parsed_in_session(&ctx("tenant-a"), &mut session, &parsed)
        .expect("extended DROP SCHEMA must pass the coarse temp gate");
}
