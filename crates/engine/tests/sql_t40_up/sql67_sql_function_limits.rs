//! `LANGUAGE sql` 関数の資源上限（呼び出し深さ `54001`・文数／返却行数 `54000`）と台帳必須構成の
//! 境界テスト（SQL-67 (6)(7)・Issue #2221。ポインタ: RLS-10）。
//!
//! 既存の `sql67_sql_function_exec.rs` はスカラー位置の上限ちょうど・`sql67_setof_from.rs` は
//! `--max-result-rows` の基本形を固定している。本ファイルは未固定だった次の経路を足す。
//! - 集合返却（`FROM` 位置）経由の深さ・文数の境界（ちょうど成功・+1 で失敗・副作用ゼロ）
//! - 相互再帰（sql↔sql・sql↔plpgsql）が既定上限で止まる
//! - 明示 Tx 内の上限超過（以降は `25P02`・`ROLLBACK` で先行する書き込みも消える）
//! - 失敗後に上限カウンタが漏れない（同一セッション・別テナントのセッションとも次の文が成功）
//! - 上限エラーの文面が他テナントの行・関数・テナント名・本体の文字列に依存しない
//! - 返却行数の上限は呼び出し元が見える行だけを数える（他テナントの大量の行で誤発火しない）
//! - 台帳必須（`Ledgered`）構成の `FROM` 位置の書き込みは `23502`（副作用ゼロ）
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`execute_sql_in_session`・
//! `execute_sql_in_txn`）で検証する。各テストは固有の一時 DB とスレッドローカルの予算だけを使い、
//! プロセス共有状態に依存しない。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::plpgsql_limits::PlpgsqlLimits;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::result_limits::ResultLimits;
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

fn open_with(
    path: &std::path::Path,
    limits: Option<PlpgsqlLimits>,
    mode: LedgerMode,
) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let mut core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode);
    if let Some(l) = limits {
        core = core.with_plpgsql_limits(l);
    }
    core
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn err(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> SqlSurfaceError {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .expect_err(sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> &'static str {
    err(core, s, "alice", sql).wire_code()
}

fn rows(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match run(core, s, tenant, sql) {
        SqlOutcome::Query(q) => q.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn count(core: &EngineCore, s: &mut SessionState, tenant: &str, table: &str) -> i64 {
    let r = rows(core, s, tenant, &format!("SELECT count(*) FROM {table}"));
    match r.first().and_then(|c| c.first()) {
        Some(Cell::SignedInteger(n)) => *n,
        Some(Cell::Integer(n)) => i64::try_from(*n).expect("count"),
        other => panic!("count: {other:?}"),
    }
}

fn create(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) {
    run(core, s, tenant, sql);
}

/// 上限つきの DB と DDL セッション（`log` 表つき）。
fn setup(label: &str, limits: Option<PlpgsqlLimits>) -> (EngineCore, SessionState, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_with(&path, limits, LedgerMode::default());
    let mut s = session();
    run(&core, &mut s, "alice", "CREATE TABLE log (n INTEGER)");
    (core, s, guard)
}

/// 呼び出しの連鎖 `c1 -> c2 -> c3 -> c4` を作る（各関数が自分の番号を `log` へ書く）。
fn create_chain(core: &EngineCore, s: &mut SessionState, tenant: &str) {
    for (name, body) in [
        ("c4", "INSERT INTO log VALUES (4); SELECT 4"),
        ("c3", "INSERT INTO log VALUES (3); SELECT c4()"),
        ("c2", "INSERT INTO log VALUES (2); SELECT c3()"),
        ("c1", "INSERT INTO log VALUES (1); SELECT c2()"),
    ] {
        create(
            core,
            s,
            tenant,
            &format!("CREATE FUNCTION {name}() RETURNS int LANGUAGE sql AS $${body}$$"),
        );
    }
}

/// `FROM` 位置の集合返却関数が呼ぶ深さも、スカラー位置と同じ上限（`max_call_depth`）に従う。
/// 集合返却関数自身を 1 段と数え、ちょうどの深さは成功・+1 は `54001`（副作用ゼロ）。
#[test]
fn call_depth_limit_is_exact_through_from_setof() {
    let limits = PlpgsqlLimits::new(100_000, 100_000, 32, 100_000, 3);
    let (core, mut s, _g) = setup("sql67_lim_depth_from", Some(limits));
    create_chain(&core, &mut s, "alice");
    for (name, callee) in [("sf3", "c4"), ("sf4", "c3"), ("sf5", "c2")] {
        create(
            &core,
            &mut s,
            "alice",
            &format!(
                "CREATE FUNCTION {name}() RETURNS SETOF int LANGUAGE sql AS \
                 $$ INSERT INTO log VALUES (0); SELECT {callee}() $$"
            ),
        );
    }
    // sf3 -> c4 は深さ 2、sf4 -> c3 -> c4 は深さ 3（ちょうど）で成功する。
    assert_eq!(rows(&core, &mut s, "alice", "SELECT * FROM sf3()").len(), 1);
    assert_eq!(rows(&core, &mut s, "alice", "SELECT * FROM sf4()").len(), 1);
    let ok_rows = count(&core, &mut s, "alice", "log");
    assert_eq!(ok_rows, 2 + 3);
    // sf5 -> c2 -> c3 -> c4 は深さ 4 で 54001、副作用はゼロ。
    assert_eq!(code(&core, &mut s, "SELECT * FROM sf5()"), "54001");
    assert_eq!(count(&core, &mut s, "alice", "log"), ok_rows);
}

/// 相互再帰（sql↔sql・sql↔plpgsql）は既定の深さ上限で `54001` になり、副作用が残らない。
#[test]
fn mutual_recursion_stops_at_the_default_depth_limit() {
    let (core, mut s, _g) = setup("sql67_lim_mutual", None);
    // 定義時は本体を解決しないので、相互参照する関数を順に定義できる。
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION fa() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1); SELECT fb() $$",
    );
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION fb() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (2); SELECT fa() $$",
    );
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION pa() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (3); SELECT pb() $$",
    );
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION pb() RETURNS int LANGUAGE plpgsql AS \
         $$ DECLARE r int; BEGIN INSERT INTO log VALUES (4); SELECT pa() INTO r; RETURN r; END $$",
    );
    for sql in ["SELECT fa()", "SELECT pa()", "SELECT pb()"] {
        assert_eq!(code(&core, &mut s, sql), "54001", "{sql}");
        assert_eq!(count(&core, &mut s, "alice", "log"), 0, "{sql}");
    }
}

/// 文数の上限は `FROM` 位置の集合返却関数にも効き、入れ子のスカラー関数の文数を合算する。
/// ちょうどは成功し、+1 で `54000`（副作用ゼロ）。
#[test]
fn statement_limit_is_exact_through_from_setof() {
    let limits = PlpgsqlLimits::new(3, 100_000, 32, 100_000, 32);
    let (core, mut s, _g) = setup("sql67_lim_stmts_from", Some(limits));
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION sf3() RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1); INSERT INTO log VALUES (2); SELECT 3 $$",
    );
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION sf4() RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1); INSERT INTO log VALUES (2); \
            INSERT INTO log VALUES (3); SELECT 4 $$",
    );
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION sc2() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (9); SELECT 9 $$",
    );
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION sfn() RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1); SELECT sc2() $$",
    );
    assert_eq!(rows(&core, &mut s, "alice", "SELECT * FROM sf3()").len(), 1);
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
    assert_eq!(code(&core, &mut s, "SELECT * FROM sf4()"), "54000");
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
    // 外側 2 文 + 内側 2 文 = 4 > 3（入れ子の文数が合算される）。
    assert_eq!(code(&core, &mut s, "SELECT * FROM sfn()"), "54000");
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
}

/// 明示 Tx 内で上限を超えると、その文は失敗し Tx は失敗状態（`25P02`）になる。`ROLLBACK` で先行する
/// 書き込みも本体の書き込みも消える（スカラー位置・`FROM` 位置の両方）。
#[test]
fn limit_failure_inside_an_explicit_transaction_rolls_everything_back() {
    let limits = PlpgsqlLimits::new(100_000, 100_000, 32, 100_000, 3);
    let (core, mut s, _g) = setup("sql67_lim_txn", Some(limits));
    create_chain(&core, &mut s, "alice");
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION sf5() RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (0); SELECT c2() $$",
    );
    let c = ctx("alice");
    for over_limit in ["SELECT c1()", "SELECT * FROM sf5()"] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO log VALUES (100)")
            .expect("prior write");
        let e = core
            .execute_sql_in_txn(&c, &mut s, &mut txn, over_limit)
            .expect_err(over_limit);
        assert_eq!(e.wire_code(), "54001", "{over_limit}");
        let e = core
            .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT 1")
            .expect_err("failed txn");
        assert_eq!(e.wire_code(), "25P02", "{over_limit}");
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
        assert_eq!(count(&core, &mut s, "alice", "log"), 0, "{over_limit}");
    }
}

/// 上限エラーの後も予算カウンタは漏れない。同一セッションの次の文・別テナントのセッション・
/// 上限内の関数がいずれも成功する。
#[test]
fn counters_do_not_leak_after_a_limit_failure() {
    let limits = PlpgsqlLimits::new(100_000, 100_000, 32, 100_000, 3);
    let (core, mut s, _g) = setup("sql67_lim_reset", Some(limits));
    create_chain(&core, &mut s, "alice");
    for _ in 0..3 {
        assert_eq!(code(&core, &mut s, "SELECT c1()"), "54001");
        // 直後の上限内の呼び出し・素の文が成功する（深さカウンタが戻っている）。
        assert_eq!(rows(&core, &mut s, "alice", "SELECT c2()").len(), 1);
        assert_eq!(rows(&core, &mut s, "alice", "SELECT 1").len(), 1);
    }
    let mut bob = session();
    create(
        &core,
        &mut bob,
        "bob",
        "CREATE FUNCTION only_bob() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (7); SELECT 7 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT c1()"), "54001");
    assert_eq!(rows(&core, &mut bob, "bob", "SELECT only_bob()").len(), 1);
    assert_eq!(count(&core, &mut bob, "bob", "log"), 1);
}

/// 上限エラー（`54001`・`54000`）の `wire_code` と文面は、他テナントの行・同名関数・テナント名・
/// 本体の文字列に依存しない。
#[test]
fn limit_error_text_is_independent_of_other_tenants_and_bodies() {
    let limits = PlpgsqlLimits::new(20, 100_000, 32, 100_000, 3);
    let (core, mut s, _g) = setup("sql67_lim_text", Some(limits));
    create_chain(&core, &mut s, "alice");
    // 文数の上限（20）を 1 つ超える本体（INSERT 20 文 + SELECT 1 文）。
    let many = "INSERT INTO log VALUES (1); ".repeat(20);
    create(
        &core,
        &mut s,
        "alice",
        &format!("CREATE FUNCTION s4() RETURNS int LANGUAGE sql AS $${many}SELECT 4$$"),
    );
    let depth_before = err(&core, &mut s, "alice", "SELECT c1()").to_string();
    let stmts_before = err(&core, &mut s, "alice", "SELECT s4()").to_string();
    // 他テナントが同名の表・関数・大量の行を持つ状態にする。
    let mut bob = session();
    run(
        &core,
        &mut bob,
        "bob",
        "INSERT INTO log VALUES (1), (2), (3)",
    );
    create_chain(&core, &mut bob, "bob");
    let depth_after = err(&core, &mut s, "alice", "SELECT c1()");
    let stmts_after = err(&core, &mut s, "alice", "SELECT s4()");
    assert_eq!(depth_after.wire_code(), "54001");
    assert_eq!(stmts_after.wire_code(), "54000");
    assert_eq!(depth_after.to_string(), depth_before);
    assert_eq!(stmts_after.to_string(), stmts_before);
    for t in [&depth_before, &stmts_before] {
        for needle in ["alice", "bob", "INSERT", "SELECT", "log"] {
            assert!(!t.contains(needle), "{needle} leaked: {t}");
        }
    }
    // 他テナントの関数は他テナントの上限で同じ文面になる（呼び出し元に依存しない）。
    let bob_depth = err(&core, &mut bob, "bob", "SELECT c1()");
    assert_eq!(bob_depth.to_string(), depth_before);
}

/// 陽性対照: 既定の上限では、中程度の深さ・文数の関数がそのまま成功する（上限が何でも拒否して
/// いるわけではない）。
#[test]
fn moderate_depth_and_statement_counts_succeed_under_default_limits() {
    let (core, mut s, _g) = setup("sql67_lim_control", None);
    create_chain(&core, &mut s, "alice");
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION sf() RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (0); SELECT c1() $$",
    );
    assert_eq!(rows(&core, &mut s, "alice", "SELECT c1()").len(), 1);
    assert_eq!(rows(&core, &mut s, "alice", "SELECT * FROM sf()").len(), 1);
    assert_eq!(count(&core, &mut s, "alice", "log"), 4 + 5);
}

/// 返却行数の上限（`--max-result-rows`）は呼び出し元が見える行だけを数える。他テナントが大量の行を
/// 持っていても誤発火せず、自分の行が上限を超えたときだけ `54000`（切り詰めず副作用なし）。
#[test]
fn row_limit_counts_only_rows_visible_to_the_caller() {
    let path = unique_db_path("sql67_lim_rows");
    let _g = CleanupGuard(path.clone());
    let core =
        open_with(&path, None, LedgerMode::default()).with_result_limits(ResultLimits::new(3));
    let mut a = session();
    let mut b = session();
    // 表はテナント共有のカタログ（行がテナントで分離される）。作成は 1 回。
    run(&core, &mut a, "alice", "CREATE TABLE t (a INTEGER)");
    for (s, t) in [(&mut a, "alice"), (&mut b, "bob")] {
        create(
            &core,
            s,
            t,
            "CREATE FUNCTION many() RETURNS SETOF int LANGUAGE sql STABLE AS \
             $$ SELECT a FROM t $$",
        );
    }
    run(&core, &mut a, "alice", "INSERT INTO t VALUES (1), (2), (3)");
    for chunk in 0..10 {
        run(
            &core,
            &mut b,
            "bob",
            &format!(
                "INSERT INTO t VALUES ({}), ({}), ({})",
                chunk * 3,
                chunk * 3 + 1,
                chunk * 3 + 2
            ),
        );
    }
    // alice の可視行はちょうど 3 件で成功する。bob の 30 行は数えない。
    assert_eq!(
        rows(&core, &mut a, "alice", "SELECT * FROM many()").len(),
        3
    );
    run(&core, &mut a, "alice", "INSERT INTO t VALUES (4)");
    let e = err(&core, &mut a, "alice", "SELECT * FROM many()");
    assert_eq!(e.wire_code(), "54000");
    // 文面は他テナントの行数を含まない。
    let text = e.to_string();
    for needle in ["30", "bob", "33"] {
        assert!(!text.contains(needle), "{needle} leaked: {text}");
    }
    assert_eq!(count(&core, &mut a, "alice", "t"), 4);
    assert_eq!(count(&core, &mut b, "bob", "t"), 30);
}

/// 台帳必須（`Ledgered`）構成: operation_id の無い `FROM` 位置から本体が書き込むと `23502`
/// （副作用ゼロ）、読み取りだけの集合返却関数は成功する（SQL-67 (6)）。
#[test]
fn ledgered_configuration_rejects_writes_from_a_from_clause() {
    let path = unique_db_path("sql67_lim_ledgered_from");
    let _g = CleanupGuard(path.clone());
    let core = open_with(&path, None, LedgerMode::Ledgered);
    let mut s = session();
    run(&core, &mut s, "alice", "CREATE TABLE log (n INTEGER)");
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION w() RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1) RETURNING n $$",
    );
    create(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION r() RETURNS SETOF int LANGUAGE sql STABLE AS $$ SELECT 5 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT * FROM w()"), "23502");
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
    assert_eq!(rows(&core, &mut s, "alice", "SELECT * FROM r()").len(), 1);
    // 明示 Tx 内でも同じ（失敗後は 25P02・ROLLBACK で副作用ゼロ）。
    let c = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT * FROM w()")
        .expect_err("ledgered write");
    assert_eq!(e.wire_code(), "23502");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
}
