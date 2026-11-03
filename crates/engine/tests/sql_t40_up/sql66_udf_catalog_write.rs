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

/// DDL 権限の無いセッションの CREATE／DROP FUNCTION は `42501`（SQL-66・Issue #1922）。カタログは
/// 読みも書きもされず、永続関数は消えない。
#[test]
fn session_without_ddl_permission_is_rejected_and_does_not_touch_the_catalog() {
    let path = unique_db_path("sql66-write-noddl");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut d = ddl_session();
        ok(&core, "alice", &mut d, "CREATE FUNCTION persisted(x) AS x");
        let mut s = SessionState::default();
        assert_eq!(
            code(&core, "alice", &mut s, "CREATE FUNCTION local_only(x) AS x"),
            "42501"
        );
        assert_eq!(
            code(&core, "alice", &mut s, "DROP FUNCTION persisted"),
            "42501"
        );
        assert_eq!(s.udfs().len(), 0);
    }
    let core = open(&path);
    let mut d = ddl_session();
    // local_only は作られていない。persisted は残っている。
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
// ---- Issue #2882: 複数文メッセージ（暗黙トランザクション）内の CREATE [OR REPLACE] FUNCTION ----
// ポインタ: SQL-66（UDF カタログ）・WIRE-16（複数文メッセージ）・RECOVER-12（複数文単位の Tx）。
// wire 層（`run_implicit_transaction`）が行う手順を engine API だけで再現する: 先頭で暗黙 Tx を開始し、
// 文の失敗時は受信前のセッションスナップショットへ戻す。

/// 暗黙 Tx で `stmts` を順に実行する。途中で失敗したらセッションを受信前へ戻して wire_code を返す。
/// 全文成功なら commit して `Ok`（commit の失敗も wire_code）。
fn run_message(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    stmts: &[&str],
) -> Result<Vec<SqlOutcome>, String> {
    let before = s.clone();
    let mut txn = core.new_session_transaction();
    core.begin_implicit_transaction(&mut txn, s)
        .map_err(|e| e.wire_code().to_string())?;
    let mut outs = Vec::new();
    for sql in stmts {
        match core.execute_sql_in_txn(&ctx(tenant), s, &mut txn, sql) {
            Ok(o) => outs.push(o),
            Err(e) => {
                txn.abort_implicit();
                *s = before;
                return Err(e.wire_code().to_string());
            }
        }
    }
    match txn.commit_implicit(s) {
        Ok(()) => Ok(outs),
        Err(e) => {
            txn.abort_implicit();
            *s = before;
            Err(e.wire_code().to_string())
        }
    }
}

/// 別接続（新しいセッション）を想定し、カタログを読み直した DDL セッションを返す。
fn fresh_loaded(core: &EngineCore, tenant: &str) -> SessionState {
    let mut s = ddl_session();
    core.load_session_udfs(&ctx(tenant), &mut s)
        .expect("load session udfs");
    s
}

/// 後続文が失敗したメッセージでは、CREATE FUNCTION はカタログにもセッションにも残らない。
#[test]
fn failed_message_rolls_back_create_function_from_catalog_and_session() {
    let path = unique_db_path("sql66-implicit-create-fail");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    setup_table(&core, "alice", &mut s);
    core.load_session_udfs(&ctx("alice"), &mut s).expect("load");
    let err = run_message(
        &core,
        "alice",
        &mut s,
        &[
            "CREATE FUNCTION dbl(x) AS x * 2",
            "SELECT id FROM no_such_table LIMIT 1",
        ],
    )
    .expect_err("later statement fails");
    assert_eq!(err, "42P01");
    // 同じセッション: 次のメッセージでもロードし直されず呼べない（世代が進んでいない）。
    core.refresh_session_udfs(&ctx("alice"), &mut s);
    assert_eq!(code(&core, "alice", &mut s, CALL_DBL), "42883");
    // 別接続からも見えない。同名の CREATE が成功する（カタログに残っていない）。
    let mut other = fresh_loaded(&core, "alice");
    assert_eq!(code(&core, "alice", &mut other, CALL_DBL), "42883");
    ok(
        &core,
        "alice",
        &mut other,
        "CREATE FUNCTION dbl(x) AS x * 3",
    );
}

/// 全文成功なら作成され、同じメッセージ内で呼べる。別接続・再オープン後も呼べる。
#[test]
fn successful_message_creates_function_visible_to_other_sessions_and_after_reopen() {
    let path = unique_db_path("sql66-implicit-create-ok");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        setup_table(&core, "alice", &mut s);
        let outs = run_message(
            &core,
            "alice",
            &mut s,
            &["CREATE FUNCTION dbl(x) AS x * 2", CALL_DBL],
        )
        .expect("message succeeds");
        assert!(matches!(outs[0], SqlOutcome::CreateFunction { .. }));
        assert!(matches!(outs[1], SqlOutcome::Query(_)));
        let mut other = fresh_loaded(&core, "alice");
        assert!(matches!(
            ok(&core, "alice", &mut other, CALL_DBL),
            SqlOutcome::Query(_)
        ));
    }
    let core = open(&path);
    let mut again = fresh_loaded(&core, "alice");
    assert!(matches!(
        ok(&core, "alice", &mut again, CALL_DBL),
        SqlOutcome::Query(_)
    ));
}

/// 他セッションは commit 前の途中状態を見ない。commit 後に再ロードすると見える。
#[test]
fn other_session_does_not_observe_uncommitted_create_function() {
    let path = unique_db_path("sql66-implicit-create-invisible");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    setup_table(&core, "alice", &mut s);
    let mut txn = core.new_session_transaction();
    core.begin_implicit_transaction(&mut txn, &s)
        .expect("begin");
    core.execute_sql_in_txn(
        &ctx("alice"),
        &mut s,
        &mut txn,
        "CREATE FUNCTION dbl(x) AS x * 2",
    )
    .expect("create in txn");
    let mut other = fresh_loaded(&core, "alice");
    assert_eq!(code(&core, "alice", &mut other, CALL_DBL), "42883");
    txn.commit_implicit(&mut s).expect("commit");
    core.refresh_session_udfs(&ctx("alice"), &mut other);
    assert!(matches!(
        ok(&core, "alice", &mut other, CALL_DBL),
        SqlOutcome::Query(_)
    ));
}

/// CREATE OR REPLACE が後続文の失敗で巻き戻ると旧い本体のまま。成功すると新しい本体になる。
#[test]
fn failed_message_keeps_old_body_on_create_or_replace() {
    let path = unique_db_path("sql66-implicit-replace");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    setup_table(&core, "alice", &mut s);
    ok(&core, "alice", &mut s, "CREATE FUNCTION dbl(x) AS x * 2");
    let value = |core: &EngineCore| {
        let mut o = fresh_loaded(core, "alice");
        format!("{:?}", ok(core, "alice", &mut o, CALL_DBL))
    };
    let before = value(&core);
    run_message(
        &core,
        "alice",
        &mut s,
        &[
            "CREATE OR REPLACE FUNCTION dbl(x) AS x * 3",
            "SELECT id FROM no_such_table LIMIT 1",
        ],
    )
    .expect_err("later statement fails");
    assert_eq!(value(&core), before);
    run_message(
        &core,
        "alice",
        &mut s,
        &["CREATE OR REPLACE FUNCTION dbl(x) AS x * 3", "SELECT 1"],
    )
    .expect("message succeeds");
    assert_ne!(value(&core), before);
}

/// 明示トランザクション内の DDL セッションの CREATE FUNCTION は従来どおり 0A000。暗黙 Tx 内の
/// DROP FUNCTION も 0A000 で、同じメッセージの先行する CREATE FUNCTION も残らない（fail-closed）。
#[test]
fn explicit_txn_create_and_implicit_txn_drop_stay_unsupported() {
    let path = unique_db_path("sql66-implicit-unsupported");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    setup_table(&core, "alice", &mut s);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &ctx("alice"),
            &mut s,
            &mut txn,
            "CREATE FUNCTION dbl(x) AS x * 2",
        )
        .expect_err("explicit txn");
    assert_eq!(e.wire_code(), "0A000");
    let mut s = ddl_session();
    ok(&core, "alice", &mut s, "CREATE FUNCTION keep(x) AS x");
    let err = run_message(
        &core,
        "alice",
        &mut s,
        &["CREATE FUNCTION dbl(x) AS x * 2", "DROP FUNCTION keep"],
    )
    .expect_err("drop in implicit txn");
    assert_eq!(err, "0A000");
    let mut other = fresh_loaded(&core, "alice");
    assert_eq!(code(&core, "alice", &mut other, CALL_DBL), "42883");
    ok(&core, "alice", &mut other, "CREATE FUNCTION dbl(x) AS x");
    assert_eq!(
        code(&core, "alice", &mut other, "CREATE FUNCTION keep(x) AS x"),
        "42723"
    );
}

/// テナント境界（P0）: alice の成功したメッセージの定義は bob から見えず、bob は同名を独立に作れる。
#[test]
fn implicit_create_function_does_not_cross_tenants() {
    let path = unique_db_path("sql66-implicit-tenant");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    run_message(
        &core,
        "alice",
        &mut a,
        &["CREATE FUNCTION dbl(x) AS x * 2", "SELECT 1"],
    )
    .expect("alice message");
    let mut b2 = fresh_loaded(&core, "bob");
    assert_eq!(code(&core, "bob", &mut b2, CALL_DBL), "42883");
    ok(&core, "bob", &mut b2, "CREATE FUNCTION dbl(x) AS x * 5");
}

/// 同一メッセージ内の標準形の関数が、先行する未 commit の関数への依存辺を記録する（Issue #2882 の
/// レビュー指摘）。commit 後の `DROP FUNCTION` は RESTRICT・CASCADE とも `2BP01` で止まる。
#[test]
fn standard_body_in_same_message_records_dependency_on_uncommitted_function() {
    let path = unique_db_path("sql66-implicit-std-dep");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    run_message(
        &core,
        "alice",
        &mut s,
        &[
            "CREATE FUNCTION f(x int) RETURNS int RETURN x + 1",
            "CREATE FUNCTION g(x int) RETURNS int RETURN f(x)",
        ],
    )
    .expect("message commits");
    for sql in ["DROP FUNCTION f", "DROP FUNCTION f CASCADE"] {
        assert_eq!(code(&core, "alice", &mut s, sql), "2BP01", "{sql}");
    }
}
