//! `schema.table` のテーブル参照の名前解決（TABLE-27・Issue #1714）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27、`docs/spec/04-behavior/error-format.md`
//! ERR-6。設計記録は `docs/design/schema-tenant-namespace.md`（D2・D4。実装記録は §7）。
//!
//! `table27_schema_ddl.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - `public.t` は未修飾の `t` と同じ結果になる（DDL・DML・SELECT・JOIN・サブクエリ・COPY・ビュー）
//! - 存在しない SCHEMA は `3F000`（他テナントにしか無い名前も同じ応答）、自テナントの SCHEMA は
//!   参照系 `42P01`・作成系 `0A000`（内部キー・内部 ID を文言に含まない）
//! - `pg_catalog`／`information_schema`／`pg_temp` は `0A000`、3 部名・引用付きは従来のコード
//! - `public.<一時表>` は永続側へも一時表へも作用せず拒否される
//! - 拡張クエリ（Parse／Bind）でも同じ解決になる

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "alice";
const COLUMNS: &str = "(name TEXT NOT NULL, qty INTEGER DEFAULT 7, code TEXT UNIQUE)";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger),
        guard,
    )
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(TENANT), s, sql) {
        Ok(o) => format!("{o:?}"),
        Err(e) => format!("ERR {}", e.wire_code()),
    }
}

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(TENANT), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
}

fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> (String, String) {
    match core.execute_sql_in_session(&ctx(TENANT), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    err(core, s, sql).0
}

fn count(core: &EngineCore, s: &mut SessionState, table: &str) -> String {
    match ok(core, s, &format!("SELECT COUNT(*) FROM {table}")) {
        SqlOutcome::Query(r) => format!("{:?}", r.rows[0].cells[0]),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `{Q}` を修飾子（空文字または `public.`）へ置換して実行する、修飾の有無で結果が一致するべき台本。
const SCRIPT: &[&str] = &[
    "CREATE TABLE {Q}t (name TEXT NOT NULL, qty INTEGER DEFAULT 7, code TEXT UNIQUE)",
    "CREATE TABLE IF NOT EXISTS {Q}t (name TEXT NOT NULL)",
    "CREATE TABLE {Q}p (name TEXT)",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (1, 'a', 10, 'c1')",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (2, 'b', 20, 'c2'), (3, 'c', 30, 'c3')",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (9, 'x', 1, 'c1')",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (1, 'a2', 11, 'c1') ON CONFLICT (id) DO UPDATE SET name = 'a2'",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (5, 'e', 50, 'c5') RETURNING id, name",
    "INSERT INTO {Q}p (id, name) VALUES (1, 'a2'), (2, 'b')",
    "SELECT id, name, qty FROM {Q}t ORDER BY id LIMIT 10",
    "SELECT t.id, t.name FROM {Q}t WHERE t.qty > 15 ORDER BY t.id LIMIT 10",
    "SELECT x.id, x.name FROM {Q}t AS x WHERE x.qty > 15 ORDER BY x.id LIMIT 10",
    "SELECT COUNT(*), SUM(qty) FROM {Q}t",
    "SELECT a.id, b.name FROM {Q}t a JOIN {Q}p b ON a.name = b.name LIMIT 10",
    "SELECT id FROM {Q}t WHERE name IN (SELECT name FROM {Q}p LIMIT 10) ORDER BY id LIMIT 10",
    "SELECT id FROM {Q}t WHERE EXISTS (SELECT id FROM {Q}p LIMIT 10) ORDER BY id LIMIT 10",
    "WITH c AS (SELECT id, name FROM {Q}t LIMIT 10) SELECT id FROM c LIMIT 10",
    "SELECT id FROM {Q}t UNION SELECT id FROM {Q}p",
    "EXPLAIN SELECT id FROM {Q}t WHERE qty = 20 LIMIT 5",
    "UPDATE {Q}t SET qty = 51 WHERE id = 5 RETURNING id, qty",
    "DELETE FROM {Q}t WHERE id = 5 RETURNING id, name",
    "CREATE INDEX t_qty ON {Q}t (qty)",
    "CREATE TABLE {Q}c (pid BIGINT REFERENCES {Q}p (id))",
    "INSERT INTO {Q}c (id, pid) VALUES (1, 1)",
    "INSERT INTO {Q}c (id, pid) VALUES (2, 99)",
    "ALTER TABLE {Q}t ADD COLUMN extra TEXT",
    "SELECT id, extra FROM {Q}t ORDER BY id LIMIT 10",
    "CREATE VIEW v AS SELECT id, name FROM {Q}t",
    "SELECT id, name FROM v ORDER BY id LIMIT 10",
    "TRUNCATE {Q}c, {Q}p",
    "SELECT COUNT(*) FROM {Q}p",
    "DROP TABLE IF EXISTS {Q}missing",
    "DROP VIEW v",
    "DROP TABLE {Q}c",
    "DROP TABLE {Q}p",
    "DROP TABLE {Q}t",
    "SELECT id FROM {Q}t LIMIT 1",
];

fn script(qualifier: &str) -> Vec<String> {
    let (core, _g) = new_core("t27-qualified-script");
    let mut s = ddl_session();
    SCRIPT
        .iter()
        .map(|q| run(&core, &mut s, &q.replace("{Q}", qualifier)))
        .collect()
}

#[test]
fn public_qualified_names_behave_like_unqualified_names() {
    let plain = script("");
    let qualified = script("public.");
    assert_eq!(plain.len(), qualified.len());
    for (i, (p, q)) in plain.iter().zip(&qualified).enumerate() {
        assert_eq!(p, q, "script step {i}: {}", SCRIPT[i]);
    }
    // 台本が空振りしていないこと（成功する文・失敗する文の両方がある）。
    let errors = plain.iter().filter(|r| r.starts_with("ERR")).count();
    assert!(errors >= 3 && errors < plain.len() - 10, "{plain:#?}");
}

#[test]
fn public_prefix_is_case_insensitive_and_mixes_with_unqualified() {
    let (core, _g) = new_core("t27-qualified-mixed");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE PUBLIC.t {COLUMNS}"));
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (1, 'a', 'c1')",
    );
    assert_eq!(count(&core, &mut s, "Public.t"), "Integer(1)");
    assert_eq!(count(&core, &mut s, "t"), "Integer(1)");
}

#[test]
fn qualified_copy_is_resolved() {
    let (core, _g) = new_core("t27-qualified-copy");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (1, 'a', 'c1')",
    );
    let plan = core
        .begin_copy(
            &ctx(TENANT),
            &s,
            "COPY (SELECT id, name FROM public.t LIMIT 10) TO STDOUT",
        )
        .expect("copy to");
    assert!(matches!(plan, engine::core::CopyPlan::To(..)));
    let plan = core
        .begin_copy(&ctx(TENANT), &s, "COPY public.t (id, name) FROM STDIN")
        .expect("copy from");
    assert!(matches!(plan, engine::core::CopyPlan::From(..)));
}

#[test]
fn unknown_schema_is_3f000_for_every_statement_kind() {
    let (core, _g) = new_core("t27-qualified-unknown");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    for sql in [
        "SELECT id FROM nope.t LIMIT 1",
        "INSERT INTO nope.t (id, name) VALUES (1, 'a')",
        "UPDATE nope.t SET qty = 1 WHERE id = 1",
        "DELETE FROM nope.t WHERE id = 1",
        "TRUNCATE nope.t",
        "CREATE TABLE nope.t (name TEXT)",
        "DROP TABLE nope.t",
        "ALTER TABLE nope.t ADD COLUMN c TEXT",
        "CREATE INDEX i ON nope.t (qty)",
    ] {
        assert_eq!(code(&core, &mut s, sql), "3F000", "{sql}");
    }
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
}

#[test]
fn schema_of_another_tenant_is_indistinguishable_from_a_missing_one() {
    let (core, _g) = new_core("t27-qualified-oracle");
    let mut bob = ddl_session();
    let bob_ctx = ctx("bob");
    core.execute_sql_in_session(&bob_ctx, &mut bob, "CREATE SCHEMA secret")
        .expect("bob creates schema");
    let mut s = ddl_session();
    let theirs = err(&core, &mut s, "SELECT id FROM secret.t LIMIT 1");
    let missing = err(&core, &mut s, "SELECT id FROM nosuch.t LIMIT 1");
    assert_eq!(theirs.0, "3F000");
    assert_eq!(missing.0, "3F000");
    assert_eq!(
        theirs.1.replace("secret", "X"),
        missing.1.replace("nosuch", "X")
    );
    // bob 自身には「SCHEMA はあるが表が無い」ので 42P01。
    let e = core
        .execute_sql_in_session(&bob_ctx, &mut bob, "SELECT id FROM secret.t LIMIT 1")
        .expect_err("42P01");
    assert_eq!(e.wire_code(), "42P01");
}

#[test]
fn own_schema_gives_42p01_for_references_and_0a000_for_create() {
    let (core, _g) = new_core("t27-qualified-own");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s1");
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    for sql in [
        "SELECT id FROM s1.t LIMIT 1",
        "INSERT INTO s1.t (id, name) VALUES (1, 'a')",
        "UPDATE s1.t SET qty = 1 WHERE id = 1",
        "DELETE FROM s1.t WHERE id = 1",
        "TRUNCATE s1.t",
        "DROP TABLE s1.t",
        "ALTER TABLE s1.t ADD COLUMN c TEXT",
    ] {
        let (c, msg) = err(&core, &mut s, sql);
        assert_eq!(c, "42P01", "{sql}");
        assert!(msg.contains("s1.t"), "{msg}");
        assert!(!msg.contains('#'), "{msg}");
    }
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE s1.t (name TEXT)"),
        "0A000"
    );
    // 修飾した名前が裸の `t` に化けていない（表は無変更）。
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
}

#[test]
fn system_schemas_temp_three_part_and_quoted_forms_are_rejected() {
    let (core, _g) = new_core("t27-qualified-system");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    for sql in [
        "SELECT id FROM pg_catalog.t LIMIT 1",
        "SELECT id FROM information_schema.t LIMIT 1",
        "INSERT INTO pg_catalog.t (id, name) VALUES (1, 'a')",
        "SELECT id FROM pg_temp.t LIMIT 1",
        "DROP TABLE pg_temp.t",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    assert_eq!(code(&core, &mut s, "SELECT id FROM a.b.c LIMIT 1"), "42601");
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM \"public\".\"t\" LIMIT 1"),
        "42601"
    );
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
}

#[test]
fn rls_still_applies_through_public_qualification() {
    let (core, _g) = new_core("t27-qualified-rls");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (1, 'a', 'c1')",
    );
    let mut other = SessionState::default();
    let seen = core
        .execute_sql_in_session(&ctx("bob"), &mut other, "SELECT id FROM public.t LIMIT 10")
        .expect("select");
    match seen {
        SqlOutcome::Query(r) => assert!(r.rows.is_empty(), "other tenant sees no rows"),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn qualified_names_resolve_in_prepared_statements() {
    let (core, _g) = new_core("t27-qualified-prepared");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (1, 'a', 'c1')",
    );
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(
            &s,
            "SELECT id, name FROM public.t WHERE name = $1 LIMIT 10",
            &[],
        )
        .expect("prepare");
    assert_eq!(prepared.param_count(), 1);
    let e = core
        .parse_sql_prepared_with_declared_in_session(
            &s,
            "SELECT id FROM nope.t WHERE name = $1 LIMIT 10",
            &[],
        )
        .expect_err("unknown schema");
    assert_eq!(e.wire_code(), "3F000");
}

// ---- 一時表との相互作用（P0: `public.t` は一時表 `t` に解決させない） ---------------------

#[test]
fn public_qualified_name_never_resolves_to_a_temp_table() {
    let (core, _g) = new_core("t27-qualified-temp");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (1, 'perm', 'p1')",
    );
    ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (7, 'temp', 't1')",
    );
    for sql in [
        "SELECT id FROM public.t LIMIT 10",
        "SELECT id FROM t JOIN public.t u ON t.id = u.id LIMIT 10",
        "DROP TABLE public.t",
        "DROP TABLE IF EXISTS public.t",
        "DELETE FROM public.t WHERE id = 1",
        "INSERT INTO public.t (id, name, code) VALUES (2, 'x', 'x')",
        "TRUNCATE public.t",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    // 一時表は残り、永続表も無変更。
    assert_eq!(count(&core, &mut s, "t"), "Integer(1)");
    ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(count(&core, &mut s, "public.t"), "Integer(1)");
}

#[test]
fn create_temp_table_in_public_is_42p16_and_pg_temp_is_not_wired() {
    let (core, _g) = new_core("t27-qualified-temp-create");
    let mut s = ddl_session();
    assert_eq!(
        code(
            &core,
            &mut s,
            &format!("CREATE TEMP TABLE public.t {COLUMNS}")
        ),
        "42P16"
    );
    for kw in [
        "GLOBAL TEMP",
        "LOCAL TEMP",
        "GLOBAL TEMPORARY",
        "LOCAL TEMPORARY",
    ] {
        assert_eq!(
            code(
                &core,
                &mut s,
                &format!("CREATE {kw} TABLE public.t {COLUMNS}")
            ),
            "42P16",
            "{kw}"
        );
    }
    ok(&core, &mut s, &format!("CREATE TABLE x {COLUMNS}"));
    ok(&core, &mut s, &format!("CREATE TEMP TABLE y {COLUMNS}"));
    // `pg_temp.x` は永続の `x` に落ちない。
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM pg_temp.x LIMIT 1"),
        "0A000"
    );
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM pg_temp.y LIMIT 1"),
        "0A000"
    );
}

#[test]
fn prepared_public_qualified_name_rejected_after_same_name_temp_table_created() {
    let (core, _g) = new_core("t27-qualified-temp-prepared");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    // 一時表が無い状態で Parse する（`public.` 修飾は Parse 時に外れる）。
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(&s, "DROP TABLE public.t", &[])
        .expect("prepare");
    ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    let e = core
        .describe_prepared_in_session(&s, &prepared)
        .map(|_| ())
        .expect_err("describe must reject");
    assert_eq!(e.wire_code(), "0A000");
    let e = core
        .bind_prepared_in_session(&s, &prepared, &[])
        .expect_err("bind must reject");
    assert_eq!(e.wire_code(), "0A000");
    // 一時表は残る。
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
}

#[test]
fn bound_public_qualified_drop_is_rejected_when_temp_table_created_before_execute() {
    let (core, _g) = new_core("t27-qualified-temp-bound");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(&s, "DROP TABLE public.t", &[])
        .expect("prepare");
    // 一時表が無い状態で Bind 済み（ポータル相当）にしてから、同名の一時表を作る。
    let bound = core
        .bind_prepared_in_session(&s, &prepared, &[])
        .expect("bind");
    ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    let e = core
        .execute_parsed_in_session(&ctx(TENANT), &mut s, &bound)
        .map(|_| ())
        .expect_err("execute must reject");
    assert_eq!(e.wire_code(), "0A000");
    // 一時表も永続表も残る。
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
    ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(count(&core, &mut s, "public.t"), "Integer(0)");
}

#[test]
fn table_identifier_in_select_list_does_not_start_relation_list() {
    let (core, _g) = new_core("t27-qualified-table-ident");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    // 選択リストの `t.id`・`public.id` は列修飾のまま扱われ、表参照として解決されない
    // （スキーマ `t`／`public` の解決エラーにならない）。
    for sql in [
        "SELECT table, t.id FROM t LIMIT 10",
        "SELECT table, public.id FROM t LIMIT 10",
    ] {
        let r = run(&core, &mut s, sql);
        assert!(!r.contains("3F000"), "{sql}: {r}");
    }
}

#[test]
fn public_alias_column_reference_is_not_a_temp_collision() {
    let (core, _g) = new_core("t27-qualified-public-alias");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE base {COLUMNS}"));
    ok(&core, &mut s, &format!("CREATE TEMP TABLE name {COLUMNS}"));
    // `public` を別名にした列参照は、一時表 `name` との衝突として拒否されない。
    let r = run(
        &core,
        &mut s,
        "SELECT public.name FROM base AS public LIMIT 1",
    );
    assert!(!r.contains("0A000"), "{r}");
}

#[test]
fn parse_sql_public_qualified_drop_is_rejected_for_session_with_same_name_temp_table() {
    let (core, _g) = new_core("t27-qualified-parse-sql");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    // セッション非依存の parse_sql（`public.` 修飾は Parse 時に外れる）。
    let parsed = core.parse_sql("DROP TABLE public.t").expect("parse");
    ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    let e = core
        .execute_parsed_in_session(&ctx(TENANT), &mut s, &parsed)
        .map(|_| ())
        .expect_err("execute must reject");
    assert_eq!(e.wire_code(), "0A000");
    // 一時表も永続表も残る。
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
    ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(count(&core, &mut s, "public.t"), "Integer(0)");
}

#[test]
fn create_temp_table_allows_public_qualified_reference_side() {
    let (core, _g) = new_core("t27-qualified-temp-reference-side");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE TABLE u (k INTEGER PRIMARY KEY)");
    // 作成対象の `public.` は 42P16、参照側の `public.u` は修飾子の検査で拒否されない。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TEMP TABLE public.x (a INTEGER REFERENCES public.u (k))"
        ),
        "42P16"
    );
    // 未修飾の参照側と修飾した参照側で同じ結果（修飾子だけで拒否が変わらない）。
    let plain = err(
        &core,
        &mut s,
        "CREATE TEMP TABLE x (a INTEGER REFERENCES u (k))",
    );
    let qualified = err(
        &core,
        &mut s,
        "CREATE TEMP TABLE x (a INTEGER REFERENCES public.u (k))",
    );
    assert_eq!(plain, qualified);
}

#[test]
fn unparameterized_public_drop_via_dummy_parsed_is_rejected_after_temp_table_created() {
    let (core, _g) = new_core("t27-qualified-temp-dummy");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    // wire-server の `parsed_if_unparameterized` 経路（Bind を経ず複製して実行）を再現する。
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(&s, "DROP TABLE public.t", &[])
        .expect("prepare");
    let parsed = prepared
        .parsed_if_unparameterized()
        .expect("unparameterized")
        .clone();
    ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    let e = core
        .execute_parsed_in_session(&ctx(TENANT), &mut s, &parsed)
        .map(|_| ())
        .expect_err("execute must reject");
    assert_eq!(e.wire_code(), "0A000");
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
}

#[test]
fn public_qualified_relation_is_not_shadowed_by_same_name_cte() {
    let (core, _g) = new_core("t27-qualified-cte");
    let mut s = ddl_session();
    ok(&core, &mut s, &format!("CREATE TABLE u {COLUMNS}"));
    assert_eq!(
        code(
            &core,
            &mut s,
            "WITH t AS (SELECT name FROM u) SELECT name FROM public.t LIMIT 10"
        ),
        "0A000"
    );
}
