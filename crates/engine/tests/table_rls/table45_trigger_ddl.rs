//! `CREATE [OR REPLACE] TRIGGER` / `DROP TRIGGER`（TABLE-45・Issue #1722）の SQL 水準の結合テスト。
//! ポインタ: `docs/spec/04-behavior/` の TABLE-45、設計記録は `docs/design/trigger-catalog.md`・
//! `docs/design/trigger-execution-model.md`（D8・D12）。
//!
//! 本ファイルはエラー契約の判定順・テナント境界・fail-closed を固定する。共有 `public` 表への定義は
//! 定義時に `0A000`（Issue #3361）で、トリガの対象は各テナントの SCHEMA 配下の表（`sa.t`・`sb.t`）に置く。
//! 再起動後の保持など Storage 水準の成功経路は `catalog::trigger` の単体テストが担う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
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

fn open_fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("b", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> SqlOutcome {
    run_as(core, tenant, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code_as(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> &'static str {
    match run_as(core, tenant, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

const CREATE: &str = "CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()";
/// SCHEMA 配下の表（[`schema_tables`] が作る `sa.t`・`sb.t`）への定義。
const CREATE_A: &str = "CREATE TRIGGER g BEFORE INSERT ON sa.t FOR EACH ROW EXECUTE FUNCTION f()";
const CREATE_B: &str = "CREATE TRIGGER g BEFORE INSERT ON sb.t FOR EACH ROW EXECUTE FUNCTION f()";

/// alice に SCHEMA `sa`・bob に SCHEMA `sb` を作り、それぞれ表 `t` を置く（セッションは呼び出し側が渡す）。
fn schema_tables(core: &EngineCore, alice: &mut SessionState, bob: &mut SessionState) {
    for (tenant, s, schema) in [("alice", alice, "sa"), ("bob", bob, "sb")] {
        ok_as(core, tenant, s, &format!("CREATE SCHEMA {schema}"));
        ok_as(
            core,
            tenant,
            s,
            &format!("CREATE TABLE {schema}.t (a TEXT, b TEXT)"),
        );
    }
}

const TRIGGER_FN: &str =
    "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$";

/// 権限の無い主体には、対象表・関数・トリガの有無を問わず常に `42501`（存在オラクルにしない）。
#[test]
fn ddl_permission_gate_precedes_every_catalog_lookup() {
    let (_g, core) = open_fixture("trg-perm");
    let mut s = SessionState::default();
    for sql in [
        CREATE,
        "CREATE TRIGGER g BEFORE INSERT ON nosuch EXECUTE FUNCTION nosuch()",
        "DROP TRIGGER g ON t",
        "DROP TRIGGER IF EXISTS g ON nosuch",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "42501", "{sql}");
    }
}

#[test]
fn unsupported_and_malformed_forms_are_rejected_before_the_catalog() {
    let (_g, core) = open_fixture("trg-syntax");
    let mut s = allowed_session();
    for (sql, want) in [
        (
            "CREATE TRIGGER g INSTEAD OF INSERT ON t EXECUTE FUNCTION f()",
            "0A000",
        ),
        (
            "CREATE TRIGGER g AFTER INSERT ON t REFERENCING NEW TABLE AS n EXECUTE FUNCTION f()",
            "0A000",
        ),
        (
            "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW WHEN (NEW.a = (SELECT 1)) EXECUTE FUNCTION f()",
            "0A000",
        ),
        (
            "CREATE TRIGGER g AFTER TRUNCATE ON t FOR EACH ROW EXECUTE FUNCTION f()",
            "0A000",
        ),
        ("CREATE TRIGGER g AFTER INSERT ON t", "42601"),
        (
            "CREATE TRIGGER s.g AFTER INSERT ON t EXECUTE FUNCTION f()",
            "42601",
        ),
        ("DROP TRIGGER g", "42601"),
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), want, "{sql}");
    }
}

#[test]
fn explicit_transaction_rejects_trigger_ddl() {
    let (_g, core) = open_fixture("trg-tx");
    let mut s = allowed_session();
    for sql in [CREATE, "DROP TRIGGER g ON t"] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, sql)
            .expect_err("trigger DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

/// 判定順: 表の不在 `42P01`・列の不在 `42703`・関数の不在 `42883`（対象は SCHEMA 配下の表）。
#[test]
fn judgement_order_table_then_column_then_function() {
    let (_g, core) = open_fixture("trg-order");
    let mut s = allowed_session();
    let mut b = allowed_session();
    schema_tables(&core, &mut s, &mut b);
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "CREATE TRIGGER g BEFORE INSERT ON nosuch EXECUTE FUNCTION f()"
        ),
        "42P01"
    );
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "CREATE TRIGGER g BEFORE UPDATE OF zzz ON sa.t EXECUTE FUNCTION f()"
        ),
        "42703"
    );
    assert_eq!(code_as(&core, "alice", &mut s, CREATE_A), "42883");
}

/// ビュー・マテリアライズドビューへのトリガは `42809`。
#[test]
fn views_are_rejected_with_wrong_object_type() {
    let (_g, core) = open_fixture("trg-view");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE VIEW v AS SELECT a FROM t");
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "CREATE TRIGGER g BEFORE INSERT ON v FOR EACH ROW EXECUTE FUNCTION f()"
        ),
        "42809"
    );
}

/// 関数はあるが `RETURNS trigger` ではない → `42P17`。他テナントの関数は不在と同じ `42883`。
#[test]
fn existing_function_is_not_a_trigger_function() {
    let (_g, core) = open_fixture("trg-fn");
    let mut alice = allowed_session();
    let mut bob = allowed_session();
    schema_tables(&core, &mut alice, &mut bob);
    ok_as(&core, "alice", &mut alice, "CREATE FUNCTION f() AS 1");
    assert_eq!(code_as(&core, "alice", &mut alice, CREATE_A), "42P17");
    assert_eq!(code_as(&core, "bob", &mut bob, CREATE_B), "42883");
}

/// 共有 `public` 表への `CREATE [OR REPLACE] TRIGGER`（`public.` 修飾を含む）は、実在するトリガ関数があっても
/// 定義時に `0A000`（Issue #3361）。カタログには何も残らず、`DROP TRIGGER` は不在の `42704`。
///
/// intentional: docs/design/trigger-catalog.md
/// 共有 public 表のトリガは、作成後に書き込みだけが失敗する状態を作るため定義時に拒否する（オーナー判断 2026-11-13）。
#[test]
fn shared_public_table_is_rejected_at_definition() {
    let (_g, core) = open_fixture("trg-public");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, TRIGGER_FN);
    for sql in [
        "CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE OR REPLACE TRIGGER g BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE TRIGGER g BEFORE INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION f()",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "0A000", "{sql}");
        assert_eq!(
            code_as(&core, "alice", &mut s, "DROP TRIGGER g ON t"),
            "42704",
            "{sql}"
        );
    }
}

/// テナントの SCHEMA 配下の表へは作成でき、`DROP TRIGGER` で消せる。
#[test]
fn schema_table_accepts_triggers() {
    let (_g, core) = open_fixture("trg-schema-ok");
    let mut alice = allowed_session();
    let mut bob = allowed_session();
    schema_tables(&core, &mut alice, &mut bob);
    ok_as(&core, "alice", &mut alice, TRIGGER_FN);
    assert!(matches!(
        ok_as(&core, "alice", &mut alice, CREATE_A),
        SqlOutcome::CreateTrigger(_)
    ));
    assert!(matches!(
        ok_as(&core, "alice", &mut alice, "DROP TRIGGER g ON sa.t"),
        SqlOutcome::DropTrigger(_)
    ));
}

/// 共有 public 表への拒否の応答は、他テナントの存在・定義の有無で変わらない（code・message がバイト単位で同じ）。
///
/// intentional: docs/design/trigger-catalog.md
/// 判定は表キーの形だけで決まり、他テナントのトリガを読まない（同名の `42710` に化けない）。
#[test]
fn shared_public_rejection_does_not_reveal_other_tenants() {
    let (_g, core) = open_fixture("trg-public-oracle");
    let mut alice = allowed_session();
    let mut bob = allowed_session();
    schema_tables(&core, &mut alice, &mut bob);
    ok_as(&core, "alice", &mut alice, TRIGGER_FN);
    ok_as(&core, "bob", &mut bob, TRIGGER_FN);
    let reject = |core: &EngineCore, s: &mut SessionState| {
        let e = run_as(core, "bob", s, CREATE).expect_err("shared public table");
        (e.wire_code(), e.client_message())
    };
    let before = reject(&core, &mut bob);
    assert_eq!(before.0, "0A000");
    // alice も同じ名前で拒否され（何も作られない）、alice の SCHEMA にトリガがあっても bob の応答は同じ。
    assert_eq!(code_as(&core, "alice", &mut alice, CREATE), "0A000");
    ok_as(&core, "alice", &mut alice, CREATE_A);
    assert_eq!(reject(&core, &mut bob), before);
}

/// `DROP TRIGGER`: 不在は `42704`、`IF EXISTS` は no-op、表の不在は `42P01`（`IF EXISTS` は no-op）。
#[test]
fn drop_trigger_absence_contract() {
    let (_g, core) = open_fixture("trg-drop");
    let mut s = allowed_session();
    assert_eq!(
        code_as(&core, "alice", &mut s, "DROP TRIGGER g ON t"),
        "42704"
    );
    assert_eq!(
        code_as(&core, "alice", &mut s, "DROP TRIGGER g ON nosuch"),
        "42P01"
    );
    for sql in [
        "DROP TRIGGER IF EXISTS g ON t",
        "DROP TRIGGER IF EXISTS g ON nosuch CASCADE",
    ] {
        match ok_as(&core, "alice", &mut s, sql) {
            SqlOutcome::DropTrigger(o) => assert!(o.skipped(), "{sql}"),
            other => panic!("{other:?}"),
        }
    }
}

/// 他テナントの SCHEMA の表は不在と同じ `3F000`（存在を漏らさない）。
#[test]
fn other_tenants_schema_is_indistinguishable_from_missing() {
    let (_g, core) = open_fixture("trg-tenant");
    let mut alice = allowed_session();
    let mut bob = allowed_session();
    ok_as(&core, "alice", &mut alice, "CREATE SCHEMA sa");
    for sql in [
        "CREATE TRIGGER g BEFORE INSERT ON sa.t EXECUTE FUNCTION f()",
        "DROP TRIGGER g ON sa.t",
        "DROP TRIGGER IF EXISTS g ON sa.t",
    ] {
        assert_eq!(code_as(&core, "bob", &mut bob, sql), "3F000", "{sql}");
        assert_eq!(
            code_as(&core, "bob", &mut bob, sql.replace("sa.", "zz.").as_str()),
            "3F000"
        );
    }
}
