//! `CREATE FUNCTION`／`DROP FUNCTION` の UDF カタログ書き込み経路の結合テスト（Issue #1920、
//! 対象ビヘイビア: SQL-66・TASK-309 のポインタ。設計記録: `docs/design/udf-catalog.md`）。
//!
//! 固定する契約: 作成した定義は再オープン後も残り、DROP した定義は再オープン後に消えている／テナント
//! 境界（他テナントの定義は不在と同じ `42883`・同名の独立作成が可能）／DDL 権限の無いセッションは
//! カタログに書かない／DDL 権限ありの明示トランザクション内は `0A000`／DROP FUNCTION の受理形
//! （複数名は `0A000`。`IF EXISTS`・`CASCADE`・`RESTRICT` は #1917 で受理。置換・IF EXISTS の
//! 結合テストは `sql66_udf_drop_replace.rs`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

/// 成功なら `Ok(outcome)`、失敗なら wire_code と文言。
fn run(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, (String, String)> {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .map_err(|e| (e.wire_code().to_string(), e.to_string()))
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    run(core, tenant, s, sql).unwrap_or_else(|(c, m)| panic!("{sql}: {c} {m}"))
}

fn code(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match run(core, tenant, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err((c, _)) => c,
    }
}

const CALL_DBL: &str = "SELECT dbl(n) AS d FROM t LIMIT 1";

fn setup_table(core: &EngineCore, tenant: &str, s: &mut SessionState) {
    ok(
        core,
        tenant,
        s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    ok(core, tenant, s, "INSERT INTO t (id, n) VALUES (1, 5)");
}

/// SQL-66: CREATE した定義は再オープン（＝再起動）後も残り、DROP した定義は消えている。
#[test]
fn create_persists_and_drop_removes_across_reopen() {
    let path = unique_db_path("sql66-write-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        setup_table(&core, "alice", &mut s);
        ok(&core, "alice", &mut s, "CREATE FUNCTION dbl(x) AS x * 2");
        ok(&core, "alice", &mut s, "CREATE FUNCTION keep(x) AS x + 1");
    }
    {
        // 再起動後: 同名の CREATE は既存扱い（永続化されている）。
        let core = open(&path);
        let mut s = ddl_session();
        assert_eq!(
            code(&core, "alice", &mut s, "CREATE FUNCTION dbl(x) AS x * 2"),
            "42723"
        );
        let out = ok(&core, "alice", &mut s, "DROP FUNCTION dbl");
        assert!(matches!(out, SqlOutcome::DropFunction(_)), "{out:?}");
    }
    {
        // 再起動後: DROP した定義は消え、残した定義は残る。
        let core = open(&path);
        let mut s = ddl_session();
        ok(&core, "alice", &mut s, "CREATE FUNCTION dbl(x) AS x * 3");
        assert_eq!(
            code(&core, "alice", &mut s, "CREATE FUNCTION keep(x) AS x"),
            "42723"
        );
    }
}

/// DROP 後の同一セッションでの呼び出しは 42883。DROP 済みの名前は再 CREATE できる。
#[test]
fn dropped_function_is_gone_in_the_same_session() {
    let path = unique_db_path("sql66-write-drop-session");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    setup_table(&core, "alice", &mut s);
    ok(&core, "alice", &mut s, "CREATE FUNCTION dbl(x) AS x * 2");
    ok(&core, "alice", &mut s, CALL_DBL);
    ok(&core, "alice", &mut s, "DROP FUNCTION dbl(integer)");
    assert_eq!(code(&core, "alice", &mut s, CALL_DBL), "42883");
    assert_eq!(code(&core, "alice", &mut s, "DROP FUNCTION dbl"), "42883");
    ok(&core, "alice", &mut s, "CREATE FUNCTION dbl(x) AS x * 3");
}

/// テナント境界（P0）: 他テナントの定義は DROP できず（不在と同じ 42883）、同名の独立した CREATE が
/// 互いに干渉しない。エラー文言に tenant や内部キーを含めない。
#[test]
fn tenants_are_isolated() {
    let path = unique_db_path("sql66-write-tenant");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    let mut b = ddl_session();
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    let (c, m) = run(&core, "bob", &mut b, "DROP FUNCTION dbl").expect_err("bob has no dbl");
    assert_eq!(c, "42883");
    assert!(!m.contains("alice") && !m.contains('#'), "{m}");
    ok(&core, "bob", &mut b, "CREATE FUNCTION dbl(x) AS x * 5");
    // alice の定義は bob の DROP の影響を受けない（再オープン後も 42723）。
    ok(&core, "bob", &mut b, "DROP FUNCTION dbl");
    drop(core);
    let core = open(&path);
    let mut a2 = ddl_session();
    assert_eq!(
        code(&core, "alice", &mut a2, "CREATE FUNCTION dbl(x) AS x"),
        "42723"
    );
    let mut b2 = ddl_session();
    ok(&core, "bob", &mut b2, "CREATE FUNCTION dbl(x) AS x");
}

/// DDL 権限の無いセッションは従来どおりセッション内だけ（カタログへ書かない・カタログを消さない）。
#[test]
fn session_without_ddl_permission_does_not_touch_the_catalog() {
    let path = unique_db_path("sql66-write-noddl");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut d = ddl_session();
        ok(&core, "alice", &mut d, "CREATE FUNCTION persisted(x) AS x");
        let mut s = SessionState::default();
        ok(&core, "alice", &mut s, "CREATE FUNCTION local_only(x) AS x");
        // 権限の無いセッションから永続関数は見えず消せない（存在情報を出さない 42883）。
        assert_eq!(
            code(&core, "alice", &mut s, "DROP FUNCTION persisted"),
            "42883"
        );
        ok(&core, "alice", &mut s, "DROP FUNCTION local_only");
    }
    let core = open(&path);
    let mut d = ddl_session();
    // local_only は永続化されていない。persisted は残っている。
    ok(&core, "alice", &mut d, "CREATE FUNCTION local_only(x) AS x");
    assert_eq!(
        code(&core, "alice", &mut d, "CREATE FUNCTION persisted(x) AS x"),
        "42723"
    );
}

/// DDL 権限ありの明示トランザクション内の CREATE／DROP FUNCTION は 0A000（取り消せない永続書き込みを
/// しない）。カタログは変わらない。
#[test]
fn ddl_inside_explicit_transaction_is_rejected() {
    let path = unique_db_path("sql66-write-txn");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        ok(&core, "alice", &mut s, "CREATE FUNCTION pre(x) AS x");
        let alice = ctx("alice");
        for sql in ["CREATE FUNCTION inner_f(x) AS x", "DROP FUNCTION pre"] {
            let mut txn = core.new_session_transaction();
            core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
                .expect("begin");
            let err = core
                .execute_sql_in_txn(&alice, &mut s, &mut txn, sql)
                .expect_err("persistent DDL inside a transaction must be rejected");
            assert_eq!(err.wire_code(), "0A000", "{sql}");
            core.execute_sql_in_txn(&alice, &mut s, &mut txn, "ROLLBACK")
                .expect("rollback");
        }
    }
    let core = open(&path);
    let mut s = ddl_session();
    ok(&core, "alice", &mut s, "CREATE FUNCTION inner_f(x) AS x");
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION pre(x) AS x"),
        "42723"
    );
}

/// DROP FUNCTION の受理形: 型の並び（`f(<型>)`・`f()`）と SCHEMA 修飾（括弧なし）。複数名は 0A000
/// （`IF EXISTS`・`CASCADE`・`RESTRICT` は #1917 で受理するため `sql66_udf_drop_replace.rs` が担う）。
#[test]
fn drop_function_forms() {
    let path = unique_db_path("sql66-write-forms");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    ok(&core, "alice", &mut s, "CREATE SCHEMA s");
    ok(&core, "alice", &mut s, "CREATE FUNCTION s.f(x) AS x");
    ok(&core, "alice", &mut s, "CREATE FUNCTION g() AS 1");
    ok(&core, "alice", &mut s, "CREATE FUNCTION h(x) AS x");
    assert_eq!(
        code(&core, "alice", &mut s, "DROP FUNCTION h, g"),
        "0A000",
        "multiple names stay unsupported"
    );
    // 引数の数が合わない型の並びは不在と同じ 42883。
    assert_eq!(
        code(&core, "alice", &mut s, "DROP FUNCTION h(integer, integer)"),
        "42883"
    );
    ok(&core, "alice", &mut s, "DROP FUNCTION g()");
    ok(&core, "alice", &mut s, "DROP FUNCTION h(integer)");
    // 配下に関数がある SCHEMA は RESTRICT で 2BP01。修飾名の DROP の後は削除できる。
    assert_eq!(code(&core, "alice", &mut s, "DROP SCHEMA s"), "2BP01");
    ok(&core, "alice", &mut s, "DROP FUNCTION s.f");
    ok(&core, "alice", &mut s, "DROP SCHEMA s");
}

/// DROP SCHEMA ... CASCADE は配下の関数行も消し、再作成した SCHEMA に古い定義は残らない。
/// 他の SCHEMA・public の同名関数には影響しない。
#[test]
fn drop_schema_cascade_removes_functions_in_the_schema() {
    let path = unique_db_path("sql66-write-cascade");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        ok(&core, "alice", &mut s, "CREATE SCHEMA s");
        ok(&core, "alice", &mut s, "CREATE SCHEMA keep");
        ok(&core, "alice", &mut s, "CREATE FUNCTION s.f(x) AS x");
        ok(&core, "alice", &mut s, "CREATE FUNCTION keep.f(x) AS x");
        ok(&core, "alice", &mut s, "CREATE FUNCTION f(x) AS x");
        ok(&core, "alice", &mut s, "DROP SCHEMA s CASCADE");
    }
    let core = open(&path);
    let mut s = ddl_session();
    ok(&core, "alice", &mut s, "CREATE SCHEMA s");
    ok(&core, "alice", &mut s, "CREATE FUNCTION s.f(x) AS x");
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION keep.f(x) AS x"),
        "42723"
    );
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION f(x) AS x"),
        "42723"
    );
}
