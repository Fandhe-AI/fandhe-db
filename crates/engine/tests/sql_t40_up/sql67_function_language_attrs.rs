//! `CREATE FUNCTION` の `LANGUAGE` 句と関数属性（SQL-67 (3)・Issue #2217）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-67 (3)、`indexing.md` INDEX-10・INDEX-11。
//! 言語・属性の受理と再起動後の保持、揮発性による式索引・部分索引の判定（`42P17`）、
//! テナント境界（他テナントの関数が結果を変えない）を production 経路で固定する。
//! `LANGUAGE sql|plpgsql` の関数の呼び出しは #2218 が結線した（本体の実行は `sql67_sql_function_exec`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
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

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx_of(tenant), &mut ddl_session(), sql)
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    run_as(core, "alice", sql)
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    run(core, sql).expect_err(sql).wire_code()
}

fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let core = open(&path);
    run(&core, "CREATE TABLE t (a INT, b TEXT)").expect("create table");
    drop(core);
    (path.clone(), CleanupGuard(path))
}

#[test]
fn language_sql_and_plpgsql_are_accepted_with_dollar_and_single_quoted_bodies() {
    let path = unique_db_path("sql67_lang_ok");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    for sql in [
        "CREATE FUNCTION f1(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$",
        "CREATE FUNCTION f2(a int) RETURNS int AS 'SELECT a' LANGUAGE sql IMMUTABLE",
        "CREATE FUNCTION f3(a int) RETURNS int IMMUTABLE STRICT LANGUAGE plpgsql \
         AS $b$ BEGIN RETURN a; END $b$",
        "CREATE FUNCTION f4(a int) RETURNS int LANGUAGE 'plpgsql' STABLE \
         RETURNS NULL ON NULL INPUT PARALLEL SAFE COST 10 AS $$ BEGIN RETURN a; END $$",
        "CREATE FUNCTION f5(a int) RETURNS int LANGUAGE sql CALLED ON NULL INPUT \
         EXTERNAL SECURITY INVOKER VOLATILE AS $$ SELECT a $$",
    ] {
        let out = run(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert!(matches!(out, SqlOutcome::CreateFunction { .. }), "{sql}");
    }
}

#[test]
fn invalid_option_combinations_are_rejected_with_fixed_codes() {
    let path = unique_db_path("sql67_lang_bad");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    let cases = [
        // 未知の言語は応答に言語名を含めない。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE c AS $$ x $$", "42704"),
        // `LANGUAGE wasm` は SQL 定義の永続化設計が未確定のため両ビルドで 0A000（Issue #1926）。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE wasm AS $$ x $$", "0A000"),
        // 重複・矛盾。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql LANGUAGE sql AS $$ x $$", "42601"),
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql IMMUTABLE VOLATILE AS $$ x $$", "42601"),
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql STRICT CALLED ON NULL INPUT AS $$ x $$", "42601"),
        // 言語付きの本体は文字列リテラル 1 個。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS a + 1", "42601"),
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS 'a' || 'b'", "42601"),
        // 本体・RETURNS の欠落。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql", "42P13"),
        ("CREATE FUNCTION f(a int) LANGUAGE sql AS $$ x $$", "42P13"),
        // 型なし引数。
        ("CREATE FUNCTION f(a) RETURNS int LANGUAGE sql AS $$ x $$", "42601"),
        // SECURITY DEFINER・未対応の句・宣言的 UDF の STRICT。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ x $$", "0A000"),
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql LEAKPROOF AS $$ x $$", "0A000"),
        ("CREATE FUNCTION f(a) STRICT AS a + 1", "0A000"),
        // trigger は plpgsql・引数 0 個のみ。
        ("CREATE FUNCTION f() RETURNS trigger LANGUAGE sql AS $$ x $$", "42P13"),
        ("CREATE FUNCTION f(a int) RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$", "42P13"),
        // plpgsql の定義時検証。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN ( ; END $$", "42601"),
        // COST・ROWS・PARALLEL。
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql COST 0 AS $$ x $$", "22000"),
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql ROWS 10 AS $$ x $$", "22000"),
        ("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql PARALLEL MAYBE AS $$ x $$", "22000"),
    ];
    for (sql, want) in cases {
        let e = run(&core, sql).expect_err(sql);
        assert_eq!(e.wire_code(), want, "{sql}");
        assert!(!format!("{e:?}").contains("'c'"), "{sql}");
    }
    // 拒否された文は何も作らない。
    assert!(run(
        &core,
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$"
    )
    .is_ok());
}

#[test]
fn definitions_and_attributes_survive_reopen() {
    let (path, _g) = setup("sql67_reopen");
    let core = open(&path);
    run(
        &core,
        "CREATE FUNCTION imm(a int) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT a $$",
    )
    .expect("imm");
    run(
        &core,
        "CREATE FUNCTION vol(a int) RETURNS int LANGUAGE plpgsql VOLATILE \
         AS $$ BEGIN RETURN a; END $$",
    )
    .expect("vol");
    run(
        &core,
        "CREATE FUNCTION dflt(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$",
    )
    .expect("dflt");
    drop(core);

    let core = open(&path);
    // 再 CREATE は 42723、引数名の変更は 42P13、置換（本体・揮発性の変更）は成功する。
    assert_eq!(
        code(
            &core,
            "CREATE FUNCTION imm(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$"
        ),
        "42723"
    );
    assert_eq!(
        code(
            &core,
            "CREATE OR REPLACE FUNCTION imm(z int) RETURNS int LANGUAGE sql AS $$ SELECT z $$"
        ),
        "42P13"
    );
    run(
        &core,
        "CREATE OR REPLACE FUNCTION dflt(a int) RETURNS int LANGUAGE sql STABLE AS $$ SELECT a $$",
    )
    .expect("replace");
    // 揮発性が保存されている観測点: IMMUTABLE は 0A000、それ以外は 42P17。
    assert_eq!(code(&core, "CREATE INDEX i1 ON t ((imm(a)))"), "0A000");
    assert_eq!(code(&core, "CREATE INDEX i2 ON t ((vol(a)))"), "42P17");
    assert_eq!(code(&core, "CREATE INDEX i3 ON t ((dflt(a)))"), "42P17");
    run(&core, "DROP FUNCTION imm").expect("drop");
    run(&core, "DROP FUNCTION vol").expect("drop");
}

#[test]
fn volatile_and_stable_functions_are_rejected_in_key_and_predicate_with_42p17() {
    let (path, _g) = setup("sql67_index");
    let core = open(&path);
    run(
        &core,
        "CREATE FUNCTION v(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$",
    )
    .expect("v");
    run(
        &core,
        "CREATE FUNCTION s(a int) RETURNS int LANGUAGE sql STABLE AS $$ SELECT a $$",
    )
    .expect("s");
    run(
        &core,
        "CREATE FUNCTION i(a int) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT a $$",
    )
    .expect("i");
    for sql in [
        "CREATE INDEX x1 ON t ((v(a)))",
        "CREATE INDEX x2 ON t ((s(a)))",
        "CREATE INDEX x3 ON t (a) WHERE v(a) > 0",
        "CREATE INDEX x4 ON t (a) WHERE s(a) > 0",
    ] {
        let e = run(&core, sql).expect_err(sql);
        assert_eq!(e.wire_code(), "42P17", "{sql}");
    }
    // IMMUTABLE の sql／plpgsql 関数も、実行・維持できないため受け入れない（fail-closed）。
    for sql in [
        "CREATE INDEX y1 ON t ((i(a)))",
        "CREATE INDEX y2 ON t (a) WHERE i(a) > 0",
        // 未知の名前・引数の数が違う呼び出しは従来どおり 0A000。
        "CREATE INDEX y3 ON t ((nosuch(a)))",
        "CREATE INDEX y4 ON t ((v(a, a)))",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    // 権限の無いセッションはカタログを引かず、従来どおり（構文段と同じ）0A000。関数の有無・揮発性を
    // 区別できない（存在オラクルにならない）。
    let e = core
        .execute_sql_in_session(
            &ctx_of("alice"),
            &mut SessionState::default(),
            "CREATE INDEX z1 ON t ((v(a)))",
        )
        .expect_err("no ddl");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn other_tenant_function_does_not_change_the_result() {
    let (path, _g) = setup("sql67_tenant");
    let core = open(&path);
    run_as(
        &core,
        "bob",
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$",
    )
    .expect("bob f");
    // alice には f が無い: 従来どおり 0A000（bob の VOLATILE な f は見えず、42P17 にならない）。
    assert_eq!(code(&core, "CREATE INDEX x ON t ((f(a)))"), "0A000");
    // alice 側に同名を作れる（bob の定義と衝突しない）。
    run(
        &core,
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$",
    )
    .expect("alice f");
    assert_eq!(code(&core, "CREATE INDEX x ON t ((f(a)))"), "42P17");
}

#[test]
fn declarative_functions_keep_their_behavior() {
    let (path, _g) = setup("sql67_declarative");
    let core = open(&path);
    run(&core, "CREATE FUNCTION inc(x) AS x + 1").expect("inc");
    run(&core, "CREATE FUNCTION dq(x) AS $$ x $$").ok();
    run(
        &core,
        "CREATE FUNCTION g(x int) RETURNS int IMMUTABLE AS x + 1",
    )
    .expect("g");
    // 宣言的 UDF の索引は従来どおり 0A000（揮発性は判定に使わない）。
    assert_eq!(code(&core, "CREATE INDEX x ON t ((inc(a)))"), "0A000");
    assert_eq!(code(&core, "CREATE INDEX x ON t ((g(a)))"), "0A000");
    // 属性付きの宣言的 UDF も従来どおり呼べる（同じ接続で作った定義の展開）。
    let mut sess = ddl_session();
    let c = ctx_of("alice");
    core.execute_sql_in_session(&c, &mut sess, "CREATE FUNCTION inc2(x) AS x + 1")
        .expect("inc2");
    let r = core
        .execute_sql_in_session(&c, &mut sess, "SELECT inc2(1)")
        .expect("select");
    assert!(matches!(r, SqlOutcome::Query(_)));
}

#[test]
fn routine_calls_run_in_the_defining_session_and_in_loaded_sessions() {
    // Issue #2218: sql／plpgsql の関数は、作成した同じセッションと、カタログからロードした別セッションの
    // 両方で式から呼べる。ロードしていない（カタログを引かない）入口 `execute_sql` では未定義（42883）。
    let (path, _g) = setup("sql67_call");
    let core = open(&path);
    let mut s = ddl_session();
    let c = ctx_of("alice");
    for sql in [
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$",
        "CREATE FUNCTION g(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a + 1; END $$",
    ] {
        core.execute_sql_in_session(&c, &mut s, sql).expect(sql);
    }
    for (sql, want) in [("SELECT f(1)", 1), ("SELECT g(1)", 2)] {
        let r = core.execute_sql_in_session(&c, &mut s, sql).expect(sql);
        let SqlOutcome::Query(q) = r else {
            panic!("{sql}: expected a query result");
        };
        let cell = q.rows.first().and_then(|r| r.cells.first()).cloned();
        assert_eq!(
            cell,
            Some(engine::sql::exec::Cell::SignedInteger(want)),
            "{sql}"
        );
    }
    let mut loaded = SessionState::default();
    core.load_session_udfs(&c, &mut loaded).expect("load");
    assert!(core
        .execute_sql_in_session(&c, &mut loaded, "SELECT f(2)")
        .is_ok());
    let e = core.execute_sql(&c, "SELECT f(1)").expect_err("call");
    assert_eq!(e.wire_code(), "42883");
}

#[test]
fn cross_language_name_collision_is_fail_closed() {
    let (path, _g) = setup("sql67_collide");
    let core = open(&path);
    run(&core, "CREATE FUNCTION f(x) AS x + 1").expect("declarative f");
    // 同名の別言語 CREATE は 42723。
    assert_eq!(
        code(
            &core,
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$"
        ),
        "42723"
    );
    run(
        &core,
        "CREATE FUNCTION h(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$",
    )
    .expect("h");
    assert_eq!(code(&core, "CREATE FUNCTION h(x) AS x + 1"), "42723");
}

#[test]
fn plpgsql_definition_limits_and_scope_are_checked_at_definition_time() {
    let path = unique_db_path("sql67_plpgsql_def");
    let _g = CleanupGuard(path.clone());
    let core = open(&path);
    // 範囲外の構文（EXCEPTION ブロック）は 0A000。
    assert_eq!(
        code(
            &core,
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; \
             EXCEPTION WHEN OTHERS THEN RETURN 0; END $$"
        ),
        "0A000"
    );
    // 埋め込み DDL は 0A000。
    assert_eq!(
        code(
            &core,
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN \
             CREATE TABLE zz (a int); RETURN a; END $$"
        ),
        "0A000"
    );
    // 本体長の超過は 54000（確保前に拒否される）。
    let big = "x".repeat(100_000);
    let sql = format!("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS '{big}'");
    assert_eq!(code(&core, &sql), "54000");
}

#[test]
fn trigger_function_can_be_created_and_used_by_create_trigger() {
    let (path, _g) = setup("sql67_trigger");
    let core = open(&path);
    run(
        &core,
        "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    )
    .expect("trigger fn");
    run(
        &core,
        "CREATE TRIGGER tt BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg()",
    )
    .expect("create trigger");
}
