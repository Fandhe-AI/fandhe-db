//! `ALTER SEQUENCE`（TABLE-24・SQL-23、Issue #1689）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-24、
//! `docs/spec/04-behavior/error-format.md` ERR-6。設計記録は
//! `docs/design/sequence-namespace-and-ledger.md` の「Issue #1689 実装記録」節。
//!
//! `table24_sequence_ddl.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。
//! `nextval` は #1690 の範囲のため、変更後の採番値そのものは Storage 水準の単体テスト
//! （`catalog::sequence::tests`）で格納値として検証し、ここでは SQL 表層の契約
//! （構文・名前解決・範囲 `22023`・権限・トランザクション・テナント独立性）を固定する。
//!
//! PostgreSQL 17 で ALTER 後に最初の `nextval` が返す値（#1690 が SQL 水準へ持ち上げる期待値）:
//!
//! | 手順 | 最初の nextval |
//! | ---- | -------------- |
//! | `CREATE SEQUENCE s; ALTER SEQUENCE s INCREMENT BY 5` | 1（次は 6） |
//! | `CREATE SEQUENCE s START 10; ALTER SEQUENCE s RESTART WITH 3` | 3 |
//! | `CREATE SEQUENCE s START 10; ALTER SEQUENCE s START WITH 20` | 10 |
//! | 続けて `ALTER SEQUENCE s RESTART` | 20 |
//! | `CREATE SEQUENCE s MAXVALUE 3; ALTER SEQUENCE s CYCLE` | 1, 2, 3, 1 |

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

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    run_as(core, "alice", session, sql)
}

fn ok(core: &EngineCore, session: &mut SessionState, sql: &str) {
    run(core, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn reopen(core: EngineCore, path: &std::path::Path) -> EngineCore {
    drop(core);
    core_over(Storage::open(path).expect("reopen storage"))
}

#[test]
fn accepted_forms_return_alter_sequence_outcome() {
    let (_p, _g, core) = open_fixture("alt-accept");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE s");
    for sql in [
        "ALTER SEQUENCE s RESTART",
        "ALTER SEQUENCE s RESTART 5",
        "ALTER SEQUENCE s RESTART WITH 7;",
        "alter sequence s start with 3",
        "ALTER SEQUENCE s START 1",
        "ALTER SEQUENCE s INCREMENT BY 2",
        "ALTER SEQUENCE s INCREMENT 3",
        "ALTER SEQUENCE s MINVALUE 1 MAXVALUE 1000",
        "ALTER SEQUENCE s NO MINVALUE NO MAXVALUE",
        "ALTER SEQUENCE s CYCLE",
        "ALTER SEQUENCE s NO CYCLE",
        "ALTER SEQUENCE s CYCLE INCREMENT BY 1 RESTART WITH 2",
    ] {
        assert!(
            matches!(run(&core, &mut s, sql), Ok(SqlOutcome::AlterSequence(_))),
            "{sql}"
        );
    }
    assert!(matches!(
        run(&core, &mut s, "ALTER SEQUENCE s RENAME TO s2"),
        Ok(SqlOutcome::AlterSequence(_))
    ));
}

#[test]
fn altered_definition_survives_reopen() {
    let (p, _g, core) = open_fixture("alt-persist");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE s");
    ok(&core, &mut s, "ALTER SEQUENCE s MAXVALUE 100");
    let core = reopen(core, &p);
    // MAXVALUE 100 が永続化されていれば、101 への RESTART は範囲外。
    assert_eq!(code(&core, &mut s, "ALTER SEQUENCE s RESTART 101"), "22023");
    ok(&core, &mut s, "ALTER SEQUENCE s RESTART 100");
}

#[test]
fn syntax_table() {
    let (_p, _g, core) = open_fixture("alt-syntax");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE s");
    for (sql, expect) in [
        ("ALTER SEQUENCE s", "42601"),
        ("ALTER SEQUENCE s;", "42601"),
        ("ALTER SEQUENCE s RENAME TO", "42601"),
        ("ALTER SEQUENCE s RENAME TO x CYCLE", "42601"),
        ("ALTER SEQUENCE s CYCLE RENAME TO x", "42601"),
        ("ALTER SEQUENCE s CYCLE NO CYCLE", "42601"),
        ("ALTER SEQUENCE s INCREMENT 1 INCREMENT 2", "42601"),
        ("ALTER SEQUENCE s MINVALUE 1 NO MINVALUE", "42601"),
        ("ALTER SEQUENCE s MAXVALUE 9 MAXVALUE 8", "42601"),
        ("ALTER SEQUENCE s RESTART RESTART", "42601"),
        ("ALTER SEQUENCE s START 1.5", "42601"),
        ("ALTER SEQUENCE s RESTART WITH", "42601"),
        ("ALTER SEQUENCE s BOGUS", "42601"),
        ("ALTER SEQUENCE s NO BOGUS", "42601"),
        ("ALTER SEQUENCE s CYCLE; SELECT 1", "42601"),
        ("ALTER SEQUENCE s START 9223372036854775808", "22003"),
        ("ALTER SEQUENCE s INCREMENT -9223372036854775809", "22003"),
        ("ALTER SEQUENCE IF EXISTS s CYCLE", "0A000"),
        ("ALTER SEQUENCE s AS integer", "0A000"),
        ("ALTER SEQUENCE s CACHE 5", "0A000"),
        ("ALTER SEQUENCE s OWNED BY t.embedding", "0A000"),
        ("ALTER SEQUENCE s OWNER TO someone", "0A000"),
        ("ALTER SEQUENCE s SET SCHEMA other", "0A000"),
        ("ALTER SEQUENCE s SET LOGGED", "0A000"),
        ("ALTER SEQUENCE s SET UNLOGGED", "0A000"),
        ("ALTER SEQUENCE s RENAME TO public.x", "0A000"),
    ] {
        assert_eq!(code(&core, &mut s, sql), expect, "{sql}");
    }
}

#[test]
fn range_violations_are_invalid_parameter_value() {
    let (_p, _g, core) = open_fixture("alt-range");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE s");
    for sql in [
        "ALTER SEQUENCE s INCREMENT 0",
        "ALTER SEQUENCE s MINVALUE 10 MAXVALUE 5",
        "ALTER SEQUENCE s MINVALUE 10 MAXVALUE 10",
        // 現行の START 1 が新しい範囲の外。
        "ALTER SEQUENCE s MINVALUE 5",
        // START を併記しても現在値 1 が範囲外。
        "ALTER SEQUENCE s MINVALUE 5 START 5",
        "ALTER SEQUENCE s MAXVALUE 0",
        "ALTER SEQUENCE s RESTART 0",
        "ALTER SEQUENCE s MAXVALUE 10 RESTART 11",
        // 降順の既定（MAXVALUE -1）では START 1 が範囲外。
        "ALTER SEQUENCE s INCREMENT BY -1 NO MINVALUE NO MAXVALUE",
    ] {
        assert_eq!(code(&core, &mut s, sql), "22023", "{sql}");
    }
    // RESTART を併記すれば範囲を変えられる。
    ok(&core, &mut s, "ALTER SEQUENCE s MINVALUE 5 START 5 RESTART");
    // 失敗した ALTER は定義を変えない（MAXVALUE は既定のまま）。
    ok(
        &core,
        &mut s,
        "ALTER SEQUENCE s RESTART 9223372036854775807",
    );
}

#[test]
fn name_resolution_and_kinds() {
    let (_p, _g, core) = open_fixture("alt-names");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE seq_a");
    ok(&core, &mut s, "CREATE SEQUENCE seq_b");
    ok(&core, &mut s, "CREATE VIEW v1 AS SELECT embedding FROM t");
    ok(
        &core,
        &mut s,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, lang TEXT)",
    );
    ok(&core, &mut s, "CREATE INDEX idx1 ON u (lang)");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW mv1 AS SELECT embedding FROM t",
    );
    assert_eq!(code(&core, &mut s, "ALTER SEQUENCE nothere CYCLE"), "42P01");
    assert_eq!(
        code(&core, &mut s, "ALTER SEQUENCE nothere RENAME TO x"),
        "42P01"
    );
    for kind in ["t", "u", "v1", "idx1", "mv1"] {
        assert_eq!(
            code(&core, &mut s, &format!("ALTER SEQUENCE {kind} CYCLE")),
            "42809",
            "{kind}"
        );
        // RENAME 先が 5 種の relation と衝突する。
        assert_eq!(
            code(
                &core,
                &mut s,
                &format!("ALTER SEQUENCE seq_a RENAME TO {kind}")
            ),
            "42P07",
            "{kind}"
        );
    }
    assert_eq!(
        code(&core, &mut s, "ALTER SEQUENCE seq_a RENAME TO seq_b"),
        "42P07"
    );
    // 同名への RENAME も 42P07。
    assert_eq!(
        code(&core, &mut s, "ALTER SEQUENCE seq_a RENAME TO seq_a"),
        "42P07"
    );
    ok(&core, &mut s, "ALTER SEQUENCE seq_a RENAME TO seq_c");
    assert_eq!(code(&core, &mut s, "DROP SEQUENCE seq_a"), "42P01");
    ok(&core, &mut s, "DROP SEQUENCE seq_c");
    // 旧名は再利用できる。
    ok(&core, &mut s, "CREATE SEQUENCE seq_a");
}

#[test]
fn ddl_without_permission_is_rejected_regardless_of_existence() {
    let (p, _g, core) = open_fixture("alt-perm");
    ok(&core, &mut allowed_session(), "CREATE SEQUENCE seeded");
    let mut denied = SessionState::default();
    for sql in [
        "ALTER SEQUENCE seeded CYCLE",
        "ALTER SEQUENCE nothere CYCLE",
        "ALTER SEQUENCE seeded RENAME TO x",
        "ALTER SEQUENCE nothere RENAME TO x",
    ] {
        assert_eq!(code(&core, &mut denied, sql), "42501", "{sql}");
    }
    // 拒否された文はカタログを変えない。
    let core = reopen(core, &p);
    let mut s = allowed_session();
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE seeded"), "42P07");
    ok(&core, &mut s, "CREATE SEQUENCE x");
}

#[test]
fn alter_sequence_inside_explicit_transaction_is_rejected() {
    let (p, _g, core) = open_fixture("alt-txn");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE existing");
    for sql in [
        "ALTER SEQUENCE existing MAXVALUE 10",
        "ALTER SEQUENCE existing RENAME TO renamed",
    ] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, sql)
            .expect_err("ALTER SEQUENCE inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed);
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    // ROLLBACK 後も定義は不変（MAXVALUE は既定のまま・改名されていない）。
    let core = reopen(core, &p);
    ok(
        &core,
        &mut s,
        "ALTER SEQUENCE existing RESTART 9223372036854775807",
    );
    ok(&core, &mut s, "DROP SEQUENCE existing");
}

#[test]
fn extended_protocol_rejects_params() {
    let (_p, _g, core) = open_fixture("alt-extended");
    for sql in [
        "ALTER SEQUENCE s RESTART WITH $1",
        "ALTER SEQUENCE s INCREMENT $1",
        "ALTER SEQUENCE $1 CYCLE",
    ] {
        let err = core
            .parse_sql_prepared(sql)
            .expect_err("$n must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

#[test]
fn results_do_not_depend_on_other_tenants() {
    let (_p, _g, core) = open_fixture("alt-tenant");
    let mut s = allowed_session();
    // 未知の名前の応答はテナントによらず同一。
    let err_a = run_as(&core, "alice", &mut s, "ALTER SEQUENCE ghost CYCLE").expect_err("unknown");
    let err_b = run_as(&core, "bob", &mut s, "ALTER SEQUENCE ghost CYCLE").expect_err("unknown");
    assert_eq!(err_a.wire_code(), "42P01");
    assert_eq!(err_b.wire_code(), "42P01");
    assert_eq!(err_a.client_message(), err_b.client_message());
    assert!(!err_a.client_message().contains("alice"));

    // 定義は全テナント共有。alice の ALTER 結果は bob の操作の有無に依存しない。
    run_as(&core, "alice", &mut s, "CREATE SEQUENCE shared").expect("create");
    run_as(&core, "bob", &mut s, "ALTER SEQUENCE shared MAXVALUE 10").expect("bob alter");
    let err = run_as(&core, "alice", &mut s, "ALTER SEQUENCE shared RESTART 11")
        .expect_err("out of range");
    assert_eq!(err.wire_code(), "22023");
    assert!(!err.client_message().contains("bob"));
    run_as(&core, "alice", &mut s, "ALTER SEQUENCE shared RESTART 10").expect("in range");
}
