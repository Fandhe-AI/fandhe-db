//! セッション開始時の UDF ロード・refresh の結合テスト（Issue #1921、対象ビヘイビア: SQL-66・TASK-309 の
//! ポインタ。設計記録: `docs/design/udf-catalog.md`「ロード経路」）。
//!
//! 固定する契約: 再オープン後の新しいセッションがカタログの UDF を呼べる（DDL 権限の有無を問わない）／
//! 他セッションの作成・DROP が refresh で反映される／他テナントの定義は見えない／カタログ由来の関数は
//! 権限の無いセッションから DROP できない（42501）／依存順序が逆転した定義・消えた呼び出し先を持つ
//! 定義の扱い／SCHEMA 配下の関数。

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

/// 新しいセッション（権限の有無を指定）を作り、開始時のロードを行う。
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

/// 呼び出しの結果（1 行目の `d` 列）を文字列化して返す。
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

/// SQL-66: 再オープン後の新しいセッションが、DDL 権限の有無を問わず作成済みの UDF を呼べる。
#[test]
fn new_session_after_reopen_can_call_persisted_function() {
    let path = unique_db_path("sql66-load-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        setup_table(&core, "alice", &mut s);
        ok(&core, "alice", &mut s, "CREATE FUNCTION dbl(x) AS x * 2");
    }
    let core = open(&path);
    // ロード前は呼べない（ロードしない経路の既定挙動は変わらない）。
    let mut unloaded = ddl_session();
    assert_eq!(code(&core, "alice", &mut unloaded, CALL_DBL), "42883");
    for ddl in [true, false] {
        let mut s = loaded(&core, "alice", ddl);
        let out = call(&core, "alice", &mut s, CALL_DBL);
        assert!(out.contains("10"), "{out}");
    }
}

/// 同じプロセスの別セッション（別接続相当）が、作成済みの関数を呼べる。他テナントには見えない。
#[test]
fn other_session_sees_function_and_other_tenant_does_not() {
    let path = unique_db_path("sql66-load-other-session");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    let mut b = loaded(&core, "alice", false);
    assert!(call(&core, "alice", &mut b, CALL_DBL).contains("10"));
    let mut other = loaded(&core, "bob", true);
    let (c, m) = run(&core, "bob", &mut other, CALL_DBL).expect_err("bob has no dbl");
    assert_eq!(c, "42883");
    assert!(!m.contains("alice") && !m.contains('#'), "{m}");
}

/// カタログ由来の関数は権限の無いセッションから DROP できず（42501）、CREATE も 42501（権限検査が
/// 重複判定より先。SQL-66・Issue #1922）。ロード済みの関数の呼び出しは権限によらず可能。
#[test]
fn catalog_origin_function_cannot_be_dropped_without_ddl_permission() {
    let path = unique_db_path("sql66-load-privilege");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    let mut b = loaded(&core, "alice", false);
    assert_eq!(code(&core, "alice", &mut b, "DROP FUNCTION dbl"), "42501");
    assert_eq!(
        code(&core, "alice", &mut b, "CREATE FUNCTION dbl(x) AS x"),
        "42501"
    );
    assert_eq!(
        code(&core, "alice", &mut b, "CREATE FUNCTION local(x) AS x + 1"),
        "42501"
    );
    assert!(call(&core, "alice", &mut b, CALL_DBL).contains("10"));
}

/// 呼び出し先を DROP して作り直しても（呼び出し先の id の方が大きくても）、再オープン後のロードで
/// 呼び出し元が呼べて、新しい呼び出し先が使われる。
#[test]
fn dependency_order_inversion_is_resolved() {
    let path = unique_db_path("sql66-load-order");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        setup_table(&core, "alice", &mut s);
        ok(&core, "alice", &mut s, "CREATE FUNCTION b(x) AS x * 2");
        ok(&core, "alice", &mut s, "CREATE FUNCTION a(x) AS b(x) + 1");
        ok(&core, "alice", &mut s, "DROP FUNCTION b");
        ok(&core, "alice", &mut s, "CREATE FUNCTION b(x) AS x * 3");
    }
    let core = open(&path);
    let mut s = loaded(&core, "alice", false);
    // n = 5: 新しい b なら 5 * 3 + 1 = 16（古い b なら 11）。
    let out = call(&core, "alice", &mut s, "SELECT a(n) AS d FROM t LIMIT 1");
    assert!(out.contains("16"), "{out}");
}

/// 呼び出し先が消えた関数は skip され（呼ぶと 42883）、行はカタログに残るため DDL セッションが
/// DROP FUNCTION で消せる。
#[test]
fn function_with_missing_callee_is_skipped_and_droppable() {
    let path = unique_db_path("sql66-load-orphan");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        setup_table(&core, "alice", &mut s);
        ok(&core, "alice", &mut s, "CREATE FUNCTION b(x) AS x * 2");
        ok(&core, "alice", &mut s, "CREATE FUNCTION a(x) AS b(x) + 1");
        ok(&core, "alice", &mut s, "DROP FUNCTION b");
    }
    let core = open(&path);
    let mut s = loaded(&core, "alice", true);
    assert_eq!(
        code(&core, "alice", &mut s, "SELECT a(n) AS d FROM t LIMIT 1"),
        "42883"
    );
    ok(&core, "alice", &mut s, "DROP FUNCTION a");
}

/// SCHEMA 配下の関数と、それを呼ぶ public の関数が、再オープン後に呼べる。
#[test]
fn schema_qualified_function_loads_after_reopen() {
    let path = unique_db_path("sql66-load-schema");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        setup_table(&core, "alice", &mut s);
        ok(&core, "alice", &mut s, "CREATE SCHEMA s");
        ok(&core, "alice", &mut s, "CREATE FUNCTION s.f(x) AS x * 2");
        ok(&core, "alice", &mut s, "CREATE FUNCTION g(x) AS s.f(x) + 1");
    }
    let core = open(&path);
    let mut s = loaded(&core, "alice", false);
    let out = call(&core, "alice", &mut s, "SELECT s.f(n) AS d FROM t LIMIT 1");
    assert!(out.contains("10"), "{out}");
    let out = call(&core, "alice", &mut s, "SELECT g(n) AS d FROM t LIMIT 1");
    assert!(out.contains("11"), "{out}");
}

/// refresh: 別セッションの DROP が反映され、別セッションの新しい関数が呼べる。
#[test]
fn refresh_reflects_other_sessions_ddl() {
    let path = unique_db_path("sql66-load-refresh");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut y = ddl_session();
    setup_table(&core, "alice", &mut y);
    ok(&core, "alice", &mut y, "CREATE FUNCTION dbl(x) AS x * 2");
    let mut x = loaded(&core, "alice", false);
    assert!(call(&core, "alice", &mut x, CALL_DBL).contains("10"));

    ok(&core, "alice", &mut y, "DROP FUNCTION dbl");
    ok(&core, "alice", &mut y, "CREATE FUNCTION trp(x) AS x * 3");
    ok(&core, "alice", &mut y, "CREATE FUNCTION mine(x) AS x + 100");
    core.refresh_session_udfs(&ctx("alice"), &mut x);
    assert_eq!(code(&core, "alice", &mut x, CALL_DBL), "42883");
    let out = call(&core, "alice", &mut x, "SELECT trp(n) AS d FROM t LIMIT 1");
    assert!(out.contains("15"), "{out}");
    let out = call(&core, "alice", &mut x, "SELECT mine(n) AS d FROM t LIMIT 1");
    assert!(out.contains("105"), "{out}");
}

/// 1 つのセッションを別テナントの ctx で refresh すると、カタログ由来の登録が消える。
#[test]
fn refresh_with_other_tenant_drops_catalog_entries() {
    let path = unique_db_path("sql66-load-rebind");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    ok(&core, "alice", &mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    let mut s = loaded(&core, "alice", false);
    assert!(call(&core, "alice", &mut s, CALL_DBL).contains("10"));
    core.refresh_session_udfs(&ctx("bob"), &mut s);
    assert_eq!(code(&core, "alice", &mut s, CALL_DBL), "42883");
}
/// SQL-66・#2798: 型付き引数と RETURNS つきの関数が再オープン後の新しいセッション（DDL 権限の有無を
/// 問わない）で呼べる。引数 0 個の RETURNS・text・配列・numeric の定義も永続化される。
#[test]
fn typed_function_with_returns_survives_reopen() {
    let path = unique_db_path("sql66-load-typed-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = ddl_session();
        setup_table(&core, "alice", &mut s);
        for sql in [
            "CREATE FUNCTION inc(x integer) RETURNS bigint AS x + 1",
            "CREATE FUNCTION seven() RETURNS integer AS 7",
            "CREATE FUNCTION shout(x text) RETURNS text AS upper(x)",
            "CREATE FUNCTION num(x numeric) AS x",
        ] {
            ok(&core, "alice", &mut s, sql);
        }
    }
    let core = open(&path);
    for ddl in [true, false] {
        let mut s = loaded(&core, "alice", ddl);
        assert!(call(&core, "alice", &mut s, "SELECT inc(n) AS d FROM t LIMIT 1").contains('6'));
        assert!(call(&core, "alice", &mut s, "SELECT seven() AS d FROM t LIMIT 1").contains('7'));
        assert!(call(
            &core,
            "alice",
            &mut s,
            "SELECT shout('ab') AS d FROM t LIMIT 1"
        )
        .contains("AB"));
        // 型が復元されている証拠: 型違いの引数は黙って受理されない。
        assert!(run(
            &core,
            "alice",
            &mut s,
            "SELECT inc('abc') AS d FROM t LIMIT 1"
        )
        .is_err());
    }
    let mut s = loaded(&core, "alice", true);
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE FUNCTION num(x numeric) AS x"
        ),
        "42723"
    );
}

/// SQL-66・#2798: 型付き DROP は型が一致するときだけ消える（不一致は 42883 で行もセッション登録も残る）。
#[test]
fn typed_drop_requires_matching_types() {
    let path = unique_db_path("sql66-load-typed-drop");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    setup_table(&core, "alice", &mut s);
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE FUNCTION inc(x integer) RETURNS integer AS x + 1",
    );
    assert_eq!(
        code(&core, "alice", &mut s, "DROP FUNCTION inc(text)"),
        "42883"
    );
    assert!(call(&core, "alice", &mut s, "SELECT inc(n) AS d FROM t LIMIT 1").contains('6'));
    let mut other = loaded(&core, "alice", false);
    assert!(call(
        &core,
        "alice",
        &mut other,
        "SELECT inc(n) AS d FROM t LIMIT 1"
    )
    .contains('6'));
    ok(&core, "alice", &mut s, "DROP FUNCTION inc(integer)");
    assert_eq!(
        code(&core, "alice", &mut s, "SELECT inc(n) AS d FROM t LIMIT 1"),
        "42883"
    );
}

/// SQL-66・RLS-12・#2798: 型付き関数は他テナントに見えず、他セッションの作成が refresh で反映され、
/// カタログ由来の型付き関数は権限の無いセッションから DROP できない（42501）。
#[test]
fn typed_function_respects_tenant_and_privilege_boundaries() {
    let path = unique_db_path("sql66-load-typed-boundary");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let mut a = ddl_session();
    setup_table(&core, "alice", &mut a);
    let mut reader = loaded(&core, "alice", false);
    ok(
        &core,
        "alice",
        &mut a,
        "CREATE FUNCTION inc(x integer) RETURNS integer AS x + 1",
    );
    core.refresh_session_udfs(&ctx("alice"), &mut reader);
    assert!(call(
        &core,
        "alice",
        &mut reader,
        "SELECT inc(n) AS d FROM t LIMIT 1"
    )
    .contains('6'));
    assert_eq!(
        code(&core, "alice", &mut reader, "DROP FUNCTION inc(integer)"),
        "42501"
    );
    let mut bob = loaded(&core, "bob", true);
    assert_eq!(
        code(&core, "bob", &mut bob, "SELECT inc(n) AS d FROM t LIMIT 1"),
        "42883"
    );
}
