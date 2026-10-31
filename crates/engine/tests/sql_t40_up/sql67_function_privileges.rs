//! `LANGUAGE sql` 関数の権限境界（SQL-67 (5)・Issue #2221。ポインタ: RLS-10）の結合テスト。
//!
//! 関数の定義・削除は DDL 権限（`42501`）が要り、呼び出しは権限の無いセッションでも本体を
//! 「呼び出し元の権限・可視性」で実行する（`SECURITY DEFINER` は受理しない）ことを固定する。
//! - DDL 権限の無いセッションの `CREATE`／`CREATE OR REPLACE`／`DROP`（各 LANGUAGE sql 形）は `42501`
//!   で、カタログも既存の関数も変わらない（明示 Tx 内でも `0A000` より先に `42501`）
//! - 権限の無いセッションでもカタログ由来の関数は呼べる（スカラー位置・`FROM` 位置）
//! - 定義者の Private 可視性は呼び出し元へ引き継がれない（本体は呼び出し元の可視性で素の SQL と一致）
//! - 本体に DDL・トランザクション制御・`SET`・`USING OPERATION_ID` を置けず、`EXECUTE` 経由の DDL も
//!   権限を迂回できない（権限の無いセッションからは表が作られない）
//! - `SECURITY DEFINER` はすべての形（SETOF・TABLE・plpgsql・`OR REPLACE`）で `0A000`
//! - 他テナントの `DROP`／`OR REPLACE` は自テナントの定義に影響せず、同名の `CREATE` は
//!   存在を漏らす `42723` にならない

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn public_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn open(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

/// DDL 権限なしでカタログから関数をロードしたセッション。
fn caller_session(core: &EngineCore, tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    core.load_session_udfs(&ctx(tenant), &mut s).expect("load");
    s
}

fn run(core: &EngineCore, c: &PolicyContext, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(c, s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn err(core: &EngineCore, c: &PolicyContext, s: &mut SessionState, sql: &str) -> SqlSurfaceError {
    core.execute_sql_in_session(c, s, sql).expect_err(sql)
}

fn code(core: &EngineCore, c: &PolicyContext, s: &mut SessionState, sql: &str) -> &'static str {
    err(core, c, s, sql).wire_code()
}

fn rows(core: &EngineCore, c: &PolicyContext, s: &mut SessionState, sql: &str) -> Vec<Vec<Cell>> {
    match run(core, c, s, sql) {
        SqlOutcome::Query(q) => q.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn int_cell(c: &Cell) -> i64 {
    match c {
        Cell::Integer(v) => i64::try_from(*v).expect("fits"),
        Cell::SignedInteger(v) => *v,
        other => panic!("unexpected cell {other:?}"),
    }
}

fn scalar(core: &EngineCore, c: &PolicyContext, s: &mut SessionState, sql: &str) -> i64 {
    let r = rows(core, c, s, sql);
    assert_eq!(r.len(), 1, "{sql}");
    int_cell(&r[0][0])
}

/// 表 `t(v)` と DDL セッション。`alice` が `f` を `base` として定義済み。
fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = ddl_session();
    run(&core, &ctx("alice"), &mut s, "CREATE TABLE t (v INTEGER)");
    run(
        &core,
        &ctx("alice"),
        &mut s,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 41 $$",
    );
    (core, guard)
}

/// DDL 権限の無いセッションの定義・削除は、LANGUAGE sql のどの形でも `42501`。カタログも既存の
/// 関数も変わらず、明示 Tx 内でも権限エラーが先に返る。
#[test]
fn session_without_ddl_permission_cannot_define_or_drop_sql_functions() {
    let (core, _g) = setup("sql67_priv_noddl");
    let a = ctx("alice");
    let mut s = caller_session(&core, "alice");
    let before = s.udfs().len();
    for sql in [
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql AS $tag$ SELECT 1 $tag$",
        "CREATE FUNCTION g() RETURNS SETOF int LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION g() RETURNS TABLE(x int) LANGUAGE sql AS $$ SELECT 1 $$",
        "CREATE FUNCTION g() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
        "CREATE OR REPLACE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 0 $$",
        "CREATE OR REPLACE FUNCTION g() RETURNS int LANGUAGE sql AS $$ SELECT 0 $$",
        "DROP FUNCTION f",
        "DROP FUNCTION IF EXISTS f",
        "DROP FUNCTION IF EXISTS nosuch",
    ] {
        assert_eq!(code(&core, &a, &mut s, sql), "42501", "{sql}");
        // 明示 Tx 内でも 42501 が先（0A000 ではない）。
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let e = core
            .execute_sql_in_txn(&a, &mut s, &mut txn, sql)
            .expect_err(sql);
        assert_eq!(e.wire_code(), "42501", "{sql} (txn)");
        let _ = core.execute_sql_in_txn(&a, &mut s, &mut txn, "ROLLBACK");
    }
    assert_eq!(s.udfs().len(), before);
    // 既存の関数は変わらず呼べ、新しい名前は作られていない（新しい DDL セッションで作成できる）。
    assert_eq!(scalar(&core, &a, &mut s, "SELECT f()"), 41);
    let mut fresh = caller_session(&core, "alice");
    assert_eq!(scalar(&core, &a, &mut fresh, "SELECT f()"), 41);
    assert_eq!(code(&core, &a, &mut fresh, "SELECT g()"), "42883");
    let mut d = ddl_session();
    run(
        &core,
        &a,
        &mut d,
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
    );
}

/// DDL 権限の無いセッションでも、カタログ由来の関数はスカラー位置・`FROM` 位置から呼べる。
#[test]
fn session_without_ddl_permission_can_call_catalog_functions() {
    let (core, _g) = setup("sql67_priv_call");
    let a = ctx("alice");
    let mut d = ddl_session();
    run(
        &core,
        &a,
        &mut d,
        "CREATE FUNCTION sf() RETURNS SETOF int LANGUAGE sql AS $$ SELECT 1 $$",
    );
    run(
        &core,
        &a,
        &mut d,
        "CREATE FUNCTION tf() RETURNS TABLE(x int) LANGUAGE sql AS $$ SELECT 2 $$",
    );
    let mut s = caller_session(&core, "alice");
    assert!(!s.ddl_allowed());
    assert_eq!(scalar(&core, &a, &mut s, "SELECT f()"), 41);
    assert_eq!(rows(&core, &a, &mut s, "SELECT * FROM sf()").len(), 1);
    assert_eq!(rows(&core, &a, &mut s, "SELECT x FROM tf()").len(), 1);
}

/// 本体は定義者ではなく呼び出し元の可視性で実行される。定義者（Private 込み）が作った関数を
/// Public のみの呼び出し元が呼んでも、見える行・書ける行は素の SQL と同じ。
#[test]
fn body_runs_with_the_callers_visibility_not_the_definers() {
    let (core, _g) = setup("sql67_priv_invoker");
    let a_priv = ctx("alice");
    let a_pub = public_ctx("alice");
    let mut d = ddl_session();
    run(&core, &a_priv, &mut d, "INSERT INTO t VALUES (1), (2), (3)");
    run(
        &core,
        &a_priv,
        &mut d,
        "CREATE FUNCTION n() RETURNS bigint LANGUAGE sql AS $$ SELECT count(*) FROM t $$",
    );
    run(
        &core,
        &a_priv,
        &mut d,
        "CREATE FUNCTION bump() RETURNS SETOF int LANGUAGE sql AS \
         $$ UPDATE t SET v = v + 10 RETURNING v $$",
    );
    for c in [&a_priv, &a_pub] {
        let mut s = caller_session(&core, "alice");
        let via_fn = scalar(&core, c, &mut s, "SELECT n()");
        let plain = scalar(&core, c, &mut s, "SELECT count(*) FROM t");
        assert_eq!(via_fn, plain);
    }
    // 他テナントの呼び出し元は、同じ表でも自分の行（0 件）しか見えない。
    let mut b = caller_session(&core, "bob");
    let r = core.execute_sql_in_session(&ctx("bob"), &mut b, "SELECT n()");
    // bob にはカタログ上 n が無い（不在と同じ 42883）。
    assert_eq!(r.expect_err("bob").wire_code(), "42883");
    // 書き込みも呼び出し元の可視範囲だけ（素の UPDATE の影響行数と一致）。
    let mut s = caller_session(&core, "alice");
    let changed = rows(&core, &a_pub, &mut s, "SELECT * FROM bump()").len();
    let mut s2 = caller_session(&core, "alice");
    let visible_after = scalar(
        &core,
        &a_pub,
        &mut s2,
        "SELECT count(*) FROM t WHERE v > 10",
    );
    assert_eq!(changed as i64, visible_after);
}

/// 本体には DDL・トランザクション制御・`SET`・`USING OPERATION_ID` を置けない（定義者が DDL 権限を
/// 持っていても `0A000`）。
#[test]
fn bodies_cannot_smuggle_privileged_statements() {
    let (core, _g) = setup("sql67_priv_smuggle");
    let a = ctx("alice");
    let mut d = ddl_session();
    for (i, body) in [
        "CREATE TABLE smuggled (v INTEGER)",
        "DROP TABLE t",
        "CREATE FUNCTION inner_f() RETURNS int LANGUAGE sql AS $x$ SELECT 1 $x$",
        "BEGIN",
        "COMMIT",
        "SET search_mode = 'balanced'",
        "INSERT INTO t VALUES (1) USING OPERATION_ID 'smuggled-op'",
    ]
    .iter()
    .enumerate()
    {
        let e = err(
            &core,
            &a,
            &mut d,
            &format!("CREATE FUNCTION bad{i}() RETURNS int LANGUAGE sql AS $${body}; SELECT 1$$"),
        );
        assert_eq!(e.wire_code(), "0A000", "{body}");
    }
    // どれも作られていない。
    let mut s = caller_session(&core, "alice");
    for i in 0..7 {
        assert_eq!(
            code(&core, &a, &mut s, &format!("SELECT bad{i}()")),
            "42883"
        );
    }
}

/// `EXECUTE` で DDL を撃つ PL/pgSQL 関数を、LANGUAGE sql 関数経由で権限の無いセッションが呼んでも、
/// 表は作られない（権限を迂回できない）。本体内の DDL は `0A000` で拒否され、副作用が無いことを確かめる。
#[test]
fn execute_ddl_through_a_function_does_not_bypass_the_permission() {
    let (core, _g) = setup("sql67_priv_execute");
    let a = ctx("alice");
    let mut d = ddl_session();
    run(
        &core,
        &a,
        &mut d,
        "CREATE FUNCTION mk() RETURNS int LANGUAGE plpgsql AS \
         $$ BEGIN EXECUTE 'CREATE TABLE smuggled (v INTEGER)'; RETURN 1; END $$",
    );
    run(
        &core,
        &a,
        &mut d,
        "CREATE FUNCTION via_sql() RETURNS int LANGUAGE sql AS $$ SELECT mk() $$",
    );
    let mut s = caller_session(&core, "alice");
    for sql in ["SELECT mk()", "SELECT via_sql()"] {
        let e = err(&core, &a, &mut s, sql);
        // 本体内の DDL は権限の有無にかかわらず 0A000（迂回経路が無い）。
        assert_eq!(e.wire_code(), "0A000", "{sql}");
    }
    // 表は作られていない（DDL セッションからも見えない）。
    assert_eq!(
        code(&core, &a, &mut d, "SELECT count(*) FROM smuggled"),
        "42P01"
    );
}

/// `SECURITY DEFINER` は LANGUAGE sql のすべての形（スカラー・SETOF・TABLE・plpgsql・`OR REPLACE`）で
/// `0A000`。カタログへは何も書かれない。
#[test]
fn security_definer_is_rejected_in_every_form() {
    let (core, _g) = setup("sql67_priv_definer");
    let a = ctx("alice");
    let mut d = ddl_session();
    for sql in [
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
        "CREATE FUNCTION g() RETURNS SETOF int LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
        "CREATE FUNCTION g() RETURNS TABLE(x int) LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
        "CREATE FUNCTION g() RETURNS int LANGUAGE plpgsql SECURITY DEFINER AS \
         $$ BEGIN RETURN 1; END $$",
        "CREATE OR REPLACE FUNCTION g() RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
        "CREATE OR REPLACE FUNCTION f() RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ SELECT 0 $$",
    ] {
        assert_eq!(code(&core, &a, &mut d, sql), "0A000", "{sql}");
    }
    let mut s = caller_session(&core, "alice");
    assert_eq!(code(&core, &a, &mut s, "SELECT g()"), "42883");
    // 既存の f は置き換えられていない。
    assert_eq!(scalar(&core, &a, &mut s, "SELECT f()"), 41);
}

/// 他テナントの `DROP`／`CREATE OR REPLACE` は自テナントの定義に影響せず、同名の `CREATE` は
/// 存在を漏らす `42723` にならない。
#[test]
fn other_tenants_ddl_does_not_affect_this_tenants_definitions() {
    let (core, _g) = setup("sql67_priv_tenant_ddl");
    let a = ctx("alice");
    let b = ctx("bob");
    let mut bd = ddl_session();
    // alice の f の存在を bob の CREATE が漏らさない（同名を独立に作れる）。
    run(
        &core,
        &b,
        &mut bd,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 7 $$",
    );
    run(
        &core,
        &b,
        &mut bd,
        "CREATE OR REPLACE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 8 $$",
    );
    let e = err(&core, &b, &mut bd, "DROP FUNCTION nosuch_for_bob");
    assert_eq!(e.wire_code(), "42883");
    run(&core, &b, &mut bd, "DROP FUNCTION f");
    // alice 側は変わらない（新しいセッションのロードでも）。
    let mut s = caller_session(&core, "alice");
    assert_eq!(scalar(&core, &a, &mut s, "SELECT f()"), 41);
    // bob は DROP 後は f を呼べない（alice の定義へフォールバックしない）。
    let mut bs = caller_session(&core, "bob");
    assert_eq!(code(&core, &b, &mut bs, "SELECT f()"), "42883");
}
