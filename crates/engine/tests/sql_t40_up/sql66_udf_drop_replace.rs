//! `DROP FUNCTION [IF EXISTS] ... [CASCADE | RESTRICT]` と `CREATE OR REPLACE FUNCTION` の結合テスト
//! （Issue #1917、対象ビヘイビア: SQL-66 (2)(3)・TASK-309 のポインタ。設計記録:
//! `docs/design/udf-catalog.md`）。
//!
//! 固定する契約: 削除した関数の呼び出しは `42883`（再オープン後も）／`IF EXISTS` は対象が無くても
//! 成功し、他テナントの定義も不在と同じ応答で消えない（P0）／置換は呼び出し結果へ反映され再オープン後・
//! 別セッションの refresh 後も残る／引数名・戻り値型の変更は `42P13`／引数の型の並びが違う同名の再定義
//! （多重定義）は未対応のため `42723`／組み込み名・予約名は `42723`／置換による循環は拒否／
//! DDL 権限ありの明示トランザクション内の置換は `0A000`／複数名の DROP は `0A000`のまま。
//!
//! `DROP FUNCTION ... CASCADE` による依存トリガの連鎖削除（`2BP01` との対比）は、`RETURNS trigger` の
//! SQL 経路が未結線のため Storage 水準の単体テスト（`catalog::trigger::tests`）が担う。

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

fn loaded(core: &EngineCore, tenant: &str, ddl: bool) -> SessionState {
    let mut s = if ddl {
        ddl_session()
    } else {
        SessionState::default()
    };
    core.load_session_udfs(&ctx(tenant), &mut s)
        .expect("load session udfs");
    s
}

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

fn call(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    format!("{:?}", ok(core, tenant, s, sql))
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

fn dropped(out: &SqlOutcome) -> bool {
    match out {
        SqlOutcome::DropFunction(o) => !o.skipped(),
        other => panic!("expected DropFunction, got {other:?}"),
    }
}

fn skipped(out: &SqlOutcome) -> bool {
    match out {
        SqlOutcome::DropFunction(o) => o.skipped(),
        other => panic!("expected DropFunction, got {other:?}"),
    }
}

/// SQL-66 (3): 削除した関数の呼び出しは 42883。DDL セッション・非 DDL セッション・再オープン後で同じ。
#[test]
fn dropped_function_call_is_42883_in_every_session_kind_and_after_reopen() {
    let path = unique_db_path("sql66-dr-drop-call");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut a = ddl_session();
        setup_table(&core, "alice", &mut a);
        ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
        let mut b = loaded(&core, "alice", true);
        assert!(call(&core, "alice", &mut b, CALL_DBL).contains("10"));
        assert!(dropped(&ok(&core, "alice", &mut a, "DROP FUNCTION dbl")));
        assert_eq!(code(&core, "alice", &mut a, CALL_DBL), "42883");
        // 別セッションは refresh で削除を知る。
        core.refresh_session_udfs(&ctx("alice"), &mut b);
        assert_eq!(code(&core, "alice", &mut b, CALL_DBL), "42883");
    }
    let core = open(&path);
    for ddl in [true, false] {
        let mut s = loaded(&core, "alice", ddl);
        assert_eq!(code(&core, "alice", &mut s, CALL_DBL), "42883");
    }
}

/// SQL-66 (3): `IF EXISTS` は対象が無くても成功し（skipped）、あれば削除する。`RESTRICT`／`CASCADE` を
/// 付けた形（依存トリガが無い場合）も同じ。複数名は 0A000 のまま。
#[test]
fn if_exists_cascade_restrict_forms() {
    let path = unique_db_path("sql66-dr-if-exists");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    assert!(skipped(&ok(
        &core,
        "alice",
        &mut s,
        "DROP FUNCTION IF EXISTS nope"
    )));
    assert!(skipped(&ok(
        &core,
        "alice",
        &mut s,
        "DROP FUNCTION IF EXISTS nope(integer) CASCADE"
    )));
    assert_eq!(code(&core, "alice", &mut s, "DROP FUNCTION nope"), "42883");
    ok(&core, "alice", &mut s, "CREATE FUNCTION f1(x) AS x");
    ok(&core, "alice", &mut s, "CREATE FUNCTION f2(x) AS x");
    ok(&core, "alice", &mut s, "CREATE FUNCTION f3(x) AS x");
    assert!(dropped(&ok(
        &core,
        "alice",
        &mut s,
        "DROP FUNCTION IF EXISTS f1"
    )));
    assert!(dropped(&ok(
        &core,
        "alice",
        &mut s,
        "DROP FUNCTION f2 CASCADE"
    )));
    assert!(dropped(&ok(
        &core,
        "alice",
        &mut s,
        "DROP FUNCTION f3(integer) RESTRICT"
    )));
    assert_eq!(
        code(&core, "alice", &mut s, "DROP FUNCTION f1, f2"),
        "0A000"
    );
    // 型の並びが合わない場合は、IF EXISTS なら不在扱い（skipped）。
    ok(&core, "alice", &mut s, "CREATE FUNCTION g(x) AS x");
    assert!(skipped(&ok(
        &core,
        "alice",
        &mut s,
        "DROP FUNCTION IF EXISTS g(integer, integer)"
    )));
    assert!(dropped(&ok(&core, "alice", &mut s, "DROP FUNCTION g")));
}

/// テナント境界（P0）: `IF EXISTS` は他テナントの定義に対しても不在と同じ skipped で、その定義は消えない。
/// 非 DDL セッションは不在の IF EXISTS でも 42501（SQL-66・Issue #1922）。
#[test]
fn if_exists_does_not_reveal_or_drop_other_tenants_function() {
    let path = unique_db_path("sql66-dr-if-exists-tenant");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut a = ddl_session();
        ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
        let mut b = ddl_session();
        assert!(skipped(&ok(
            &core,
            "bob",
            &mut b,
            "DROP FUNCTION IF EXISTS dbl"
        )));
        // 権限の無いセッションは対象の有無によらず 42501（存在情報を区別させない）。
        let mut c = loaded(&core, "bob", false);
        assert_eq!(
            code(&core, "bob", &mut c, "DROP FUNCTION IF EXISTS dbl"),
            "42501"
        );
    }
    let core = open(&path);
    let mut a = ddl_session();
    assert_eq!(
        code(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x"),
        "42723",
        "alice's function must survive bob's DROP IF EXISTS"
    );
}

/// 権限の無いセッションがカタログ由来の関数を消そうとすると、`IF EXISTS` でも 42501（呼べる関数に
/// 42883 を返す食い違いを作らない）。関数は消えない。
#[test]
fn if_exists_still_requires_privilege_for_catalog_origin_function() {
    let path = unique_db_path("sql66-dr-if-exists-priv");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    let mut b = loaded(&core, "alice", false);
    assert_eq!(
        code(&core, "alice", &mut b, "DROP FUNCTION IF EXISTS dbl"),
        "42501"
    );
    assert!(call(&core, "alice", &mut b, CALL_DBL).contains("10"));
}

/// SQL-66 (2): `CREATE OR REPLACE` は呼び出し結果へ反映され、再オープン後・別セッションの refresh 後も
/// 新しい本体が使われる。対象が無ければ新規作成になる。
#[test]
fn replace_is_reflected_and_persisted() {
    let path = unique_db_path("sql66-dr-replace");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut a = ddl_session();
        setup_table(&core, "alice", &mut a);
        // 対象が無い OR REPLACE は新規作成。
        ok(
            &core,
            "alice",
            &mut a,
            "CREATE OR REPLACE FUNCTION dbl(x) AS x * 2",
        );
        assert!(call(&core, "alice", &mut a, CALL_DBL).contains("10"));
        let mut b = loaded(&core, "alice", false);
        assert!(call(&core, "alice", &mut b, CALL_DBL).contains("10"));
        let out = ok(
            &core,
            "alice",
            &mut a,
            "CREATE OR REPLACE FUNCTION dbl(x) AS x * 3",
        );
        assert!(matches!(out, SqlOutcome::CreateFunction { .. }), "{out:?}");
        assert!(call(&core, "alice", &mut a, CALL_DBL).contains("15"));
        core.refresh_session_udfs(&ctx("alice"), &mut b);
        assert!(call(&core, "alice", &mut b, CALL_DBL).contains("15"));
    }
    let core = open(&path);
    for ddl in [true, false] {
        let mut s = loaded(&core, "alice", ddl);
        assert!(call(&core, "alice", &mut s, CALL_DBL).contains("15"));
    }
}

/// 呼び出し元が置換した呼び出し先を使う（インライン展開は置換後の本体）。
#[test]
fn replacing_a_callee_changes_its_callers() {
    let path = unique_db_path("sql66-dr-replace-callee");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    ok(&core, "alice", &mut a, "CREATE FUNCTION g(x) AS x + 1");
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS g(x) * 2");
    assert!(call(&core, "alice", &mut a, CALL_DBL).contains("12"));
    ok(
        &core,
        "alice",
        &mut a,
        "CREATE OR REPLACE FUNCTION g(x) AS x + 10",
    );
    assert!(call(&core, "alice", &mut a, CALL_DBL).contains("30"));
}

/// 非 DDL セッションの OR REPLACE は 42501（Issue #1922）。レジストリもカタログも変わらない。
#[test]
fn replace_in_session_without_ddl_permission_is_rejected() {
    let path = unique_db_path("sql66-dr-replace-noddl");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut d = ddl_session();
    setup_table(&core, "alice", &mut d);
    ok(&core, "alice", &mut d, "CREATE FUNCTION dbl(x) AS x * 2");
    let mut s = loaded(&core, "alice", false);
    let before = s.udfs().len();
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION dbl(x) AS x * 7"
        ),
        "42501"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION fresh(x) AS x * 7"
        ),
        "42501"
    );
    assert_eq!(s.udfs().len(), before);
    assert!(call(&core, "alice", &mut s, CALL_DBL).contains("10"));
    // カタログも変わらない: 別セッションから見ても元の本体で、fresh は無い。
    let mut other = loaded(&core, "alice", true);
    assert!(call(&core, "alice", &mut other, CALL_DBL).contains("10"));
    assert_eq!(
        code(&core, "alice", &mut other, "SELECT fresh(n) FROM t LIMIT 1"),
        "42883"
    );
}

/// カタログ由来の関数を権限の無いセッションが置換すると 42501（DROP と同じ。関数は変わらない）。
#[test]
fn replace_of_catalog_origin_function_requires_privilege() {
    let path = unique_db_path("sql66-dr-replace-priv");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    let mut b = loaded(&core, "alice", false);
    assert_eq!(
        code(
            &core,
            "alice",
            &mut b,
            "CREATE OR REPLACE FUNCTION dbl(x) AS x * 9"
        ),
        "42501"
    );
    assert!(call(&core, "alice", &mut b, CALL_DBL).contains("10"));
}

/// 引数名の変更は 42P13。引数の数が違う同名の再定義（多重定義）は未対応のため 42723（既存の定義は
/// 変わらない）。
#[test]
fn replace_rejects_param_name_change_and_overloads() {
    let path = unique_db_path("sql66-dr-replace-reject");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut a,
            "CREATE OR REPLACE FUNCTION dbl(z) AS z * 3"
        ),
        "42P13"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut a,
            "CREATE OR REPLACE FUNCTION dbl(x, y) AS x * y"
        ),
        "42723"
    );
    assert!(call(&core, "alice", &mut a, CALL_DBL).contains("10"));
}

/// 戻り値型・型注釈の変更（権限ありセッションの型付き・RETURNS 付き定義は #2798 までセッション限定）。
/// 戻り値型の変更・RETURNS の有無の変更は 42P13、引数の型の違いは多重定義扱いで 42723、
/// 同じシグネチャなら置換できる。
#[test]
fn replace_rejects_return_type_and_signature_changes_in_session_only_definitions() {
    let path = unique_db_path("sql66-dr-replace-returns");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE FUNCTION tf(x integer) RETURNS integer AS x",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION tf(x integer) RETURNS bigint AS x"
        ),
        "42P13"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION tf(x integer) AS x"
        ),
        "42P13"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION tf(x text) RETURNS integer AS 1"
        ),
        "42723"
    );
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE OR REPLACE FUNCTION tf(x integer) RETURNS integer AS x + 1",
    );
}

/// 組み込み名・予約名に対する OR REPLACE は 42723（組み込み関数を置換できない）。権限の無い
/// セッションは名前の検証より先に 42501。
#[test]
fn replace_cannot_shadow_builtin_or_reserved_names() {
    let path = unique_db_path("sql66-dr-replace-builtin");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    for ddl in [true, false] {
        let mut s = if ddl {
            ddl_session()
        } else {
            SessionState::default()
        };
        for name in ["abs", "count", "nextval"] {
            assert_eq!(
                code(
                    &core,
                    "alice",
                    &mut s,
                    &format!("CREATE OR REPLACE FUNCTION {name}(x) AS x")
                ),
                if ddl { "42723" } else { "42501" },
                "{name} ddl={ddl}"
            );
        }
    }
}

/// 置換による呼び出しの循環（自己参照・相互参照）は拒否され、レジストリ・カタログは変わらない
/// （インライン展開の無限再帰を作らない。DoS 対策）。
#[test]
fn replace_rejects_cycles_and_leaves_state_unchanged() {
    let path = unique_db_path("sql66-dr-replace-cycle");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut a = ddl_session();
        setup_table(&core, "alice", &mut a);
        ok(&core, "alice", &mut a, "CREATE FUNCTION g(x) AS x + 1");
        ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS g(x) * 2");
        // 自己参照
        assert_eq!(
            code(
                &core,
                "alice",
                &mut a,
                "CREATE OR REPLACE FUNCTION dbl(x) AS dbl(x)"
            ),
            "0A000"
        );
        // 相互参照: g が dbl（= g を呼ぶ）を呼ぶ
        assert_eq!(
            code(
                &core,
                "alice",
                &mut a,
                "CREATE OR REPLACE FUNCTION g(x) AS dbl(x)"
            ),
            "0A000"
        );
        assert!(call(&core, "alice", &mut a, CALL_DBL).contains("12"));
    }
    let core = open(&path);
    let mut s = loaded(&core, "alice", true);
    assert!(call(&core, "alice", &mut s, CALL_DBL).contains("12"));
    // 追加の関数でも同じ（ロード済みの DDL セッション）。
    let mut n = ddl_session();
    // 本体が自身だけを呼ぶ関数の自己再帰置換も 0A000（未定義関数 42883 にならない）。
    ok(&core, "alice", &mut n, "CREATE FUNCTION selfref(x) AS x");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut n,
            "CREATE OR REPLACE FUNCTION selfref(x) AS selfref(x)"
        ),
        "0A000"
    );
    ok(&core, "alice", &mut n, "CREATE FUNCTION a1(x) AS x");
    ok(&core, "alice", &mut n, "CREATE FUNCTION b1(x) AS a1(x)");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut n,
            "CREATE OR REPLACE FUNCTION a1(x) AS b1(x)"
        ),
        "0A000"
    );
}

/// DDL 権限ありの明示トランザクション内の OR REPLACE は 0A000（取り消せない永続書き込みをしない）。
#[test]
fn replace_inside_explicit_transaction_is_rejected_for_ddl_sessions() {
    let path = unique_db_path("sql66-dr-replace-txn");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    ok(&core, "alice", &mut s, "CREATE FUNCTION pre(x) AS x");
    let alice = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &alice,
            &mut s,
            &mut txn,
            "CREATE OR REPLACE FUNCTION pre(x) AS x + 1",
        )
        .expect_err("persistent DDL inside a transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// SQL-66・#2798: DDL セッションの型付き・RETURNS つきの OR REPLACE は永続化される。
/// 同じシグネチャなら置換でき、戻り値型の変更・RETURNS の有無の変更は 42P13、引数の型の変更は 42723。
#[test]
fn replace_with_typed_signature_is_persisted_in_ddl_sessions() {
    let path = unique_db_path("sql66-dr-replace-typed-ddl");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE FUNCTION tf(x integer) RETURNS integer AS x",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE OR REPLACE FUNCTION tf(x integer) RETURNS integer AS x + 1",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION tf(x integer) RETURNS bigint AS x"
        ),
        "42P13"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION tf(x integer) AS x"
        ),
        "42P13"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION tf(x text) RETURNS text AS x"
        ),
        "42723"
    );
    let mut other = loaded(&core, "alice", true);
    for name in ["tf", "tr"] {
        assert_eq!(
            code(&core, "alice", &mut other, &format!("DROP FUNCTION {name}")),
            "42883"
        );
    }
}
