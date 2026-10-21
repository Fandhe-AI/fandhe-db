//! `current_schema()`・`current_schemas()`（TABLE-27 (7)・Issue #1718）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27 (7)。設計記録は
//! `docs/design/schema-tenant-namespace.md`（D2・D4・D6・D9・D12。実装記録は §11）。
//!
//! `table27_search_path.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - セッションの `search_path` に従った値（実在する SCHEMA だけ。他テナントの SCHEMA は不在と同じ）
//! - `current_schemas(true)` の暗黙要素（`pg_catalog`・一時スキーマ）と順序
//! - FROM なし SELECT 以外の文脈は fail-closed、内部キーを出さない、UDF 名の予約

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "alice";

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

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(TENANT), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(TENANT), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 1 行の結果の全セルを文字列化する（NULL は `<null>`、配列は `Debug` 表示）。
fn row(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    match ok(core, s, sql) {
        SqlOutcome::Query(r) => r.rows[0]
            .cells
            .iter()
            .map(|c| match c {
                Cell::Text(t) => t.clone(),
                Cell::Null => "<null>".to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn one(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    row(core, s, sql).remove(0)
}

fn columns(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    match ok(core, s, sql) {
        SqlOutcome::Query(r) => r
            .columns
            .iter()
            .map(|c| match c {
                engine::sql::exec::ColumnMeta::Computed { name, .. } => name.clone(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `current_schemas(..)` の配列セルの `Debug` 表示（NULL は `<null>`）。
fn schemas(core: &EngineCore, s: &mut SessionState, arg: &str) -> String {
    let sql = format!("SELECT current_schemas({arg})");
    match ok(core, s, &sql) {
        SqlOutcome::Query(r) => match &r.rows[0].cells[0] {
            Cell::Null => "<null>".to_string(),
            Cell::Array(a) => format!("{a:?}"),
            other => panic!("expected array, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn default_session_without_user_resolves_to_public() {
    let (core, _g) = new_core("cs-default");
    let mut s = ddl_session();
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "public");
    assert_eq!(one(&core, &mut s, "SELECT current_schema"), "public");
    let f = schemas(&core, &mut s, "false");
    let t = schemas(&core, &mut s, "true");
    assert!(f.contains("public") && !f.contains("pg_catalog"), "{f}");
    assert!(t.contains("pg_catalog") && t.contains("public"), "{t}");
    assert!(t.find("pg_catalog") < t.find("public"), "{t}");
    assert!(!t.contains("pg_temp"), "{t}");
}

#[test]
fn column_names_match_postgres() {
    let (core, _g) = new_core("cs-colnames");
    let mut s = ddl_session();
    assert_eq!(
        columns(&core, &mut s, "SELECT current_schema()"),
        vec!["current_schema"]
    );
    assert_eq!(
        columns(&core, &mut s, "SELECT current_schema"),
        vec!["current_schema"]
    );
    assert_eq!(
        columns(&core, &mut s, "SELECT current_schemas(true)"),
        vec!["current_schemas"]
    );
}

#[test]
fn session_user_schema_wins_only_in_own_tenant() {
    let (core, _g) = new_core("cs-user");
    let mut s = ddl_session();
    s.set_session_user("alice");
    // 同名 SCHEMA が無ければ `public`。
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "public");
    // 他テナントにだけ同名があっても不在と同じ。
    core.execute_sql_in_session(&ctx("bob"), &mut ddl_session(), "CREATE SCHEMA alice")
        .expect("bob creates schema");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "public");
    // 自テナントに作ると `$user` が優先される。
    ok(&core, &mut s, "CREATE SCHEMA alice");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "alice");
    let f = schemas(&core, &mut s, "false");
    assert!(f.find("alice") < f.find("public"), "{f}");
}

#[test]
fn path_that_hides_public_yields_null_or_empty() {
    let (core, _g) = new_core("cs-hide");
    let mut s = ddl_session();
    ok(&core, &mut s, "SET search_path = nosuch");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "<null>");
    let f = schemas(&core, &mut s, "false");
    assert!(!f.contains("public") && !f.contains("nosuch"), "{f}");
    let t = schemas(&core, &mut s, "true");
    assert!(t.contains("pg_catalog") && !t.contains("public"), "{t}");
}

#[test]
fn other_tenant_schema_is_indistinguishable_from_missing() {
    let (core, _g) = new_core("cs-other");
    core.execute_sql_in_session(&ctx("bob"), &mut ddl_session(), "CREATE SCHEMA theirs")
        .expect("bob creates schema");
    let mut s = ddl_session();
    ok(&core, &mut s, "SET search_path = theirs, public");
    let with_theirs = (
        one(&core, &mut s, "SELECT current_schema()"),
        schemas(&core, &mut s, "false"),
        schemas(&core, &mut s, "true"),
    );
    ok(&core, &mut s, "SET search_path = missing, public");
    let with_missing = (
        one(&core, &mut s, "SELECT current_schema()"),
        schemas(&core, &mut s, "false"),
        schemas(&core, &mut s, "true"),
    );
    assert_eq!(with_theirs, with_missing);
    assert_eq!(with_theirs.0, "public");
}

#[test]
fn own_schema_listed_before_public() {
    let (core, _g) = new_core("cs-own");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s1");
    ok(&core, &mut s, "SET search_path = s1, public");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "s1");
    let f = schemas(&core, &mut s, "false");
    assert!(f.find("s1") < f.find("public"), "{f}");
    // 引用符つきの名前も利用者が書いた名前のまま返る（内部キーを含まない）。
    ok(&core, &mut s, "CREATE SCHEMA \"MySchema\"");
    ok(&core, &mut s, "SET search_path = \"MySchema\"");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "MySchema");
}

#[test]
fn builtin_schemas_and_no_duplicate_implicit_entries() {
    let (core, _g) = new_core("cs-builtin");
    let mut s = ddl_session();
    ok(&core, &mut s, "SET search_path = pg_catalog, public");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "pg_catalog");
    let t = schemas(&core, &mut s, "true");
    assert_eq!(t.matches("pg_catalog").count(), 1, "{t}");
    ok(&core, &mut s, "SET search_path = information_schema");
    assert_eq!(
        one(&core, &mut s, "SELECT current_schema()"),
        "information_schema"
    );
}

#[test]
fn temp_schema_is_visible_only_with_temp_tables() {
    let (core, _g) = new_core("cs-temp");
    let mut s = ddl_session();
    ok(&core, &mut s, "SET search_path = pg_temp, public");
    // 一時表が無ければ明示した `pg_temp` も飛ばされる。
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "public");
    assert!(!schemas(&core, &mut s, "true").contains("pg_temp"));
    ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY)",
    );
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "pg_temp");
    ok(&core, &mut s, "SET search_path = public");
    let t = schemas(&core, &mut s, "true");
    assert!(t.contains("pg_temp"), "{t}");
    assert!(t.find("pg_temp") < t.find("pg_catalog"), "{t}");
    assert!(t.find("pg_catalog") < t.find("public"), "{t}");
}

#[test]
fn values_follow_set_reset_and_rollback() {
    let (core, _g) = new_core("cs-follow");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s1");
    ok(&core, &mut s, "SET search_path = s1");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "s1");
    ok(&core, &mut s, "RESET search_path");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "public");
    let c = ctx(TENANT);
    let mut txn = core.new_session_transaction();
    let mut in_txn = |s: &mut SessionState, sql: &str| -> Vec<String> {
        match core
            .execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
        {
            SqlOutcome::Query(r) => r.rows[0]
                .cells
                .iter()
                .map(|c| match c {
                    Cell::Text(t) => t.clone(),
                    other => format!("{other:?}"),
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    in_txn(&mut s, "BEGIN");
    in_txn(&mut s, "SET search_path = s1");
    assert_eq!(in_txn(&mut s, "SELECT current_schema()"), vec!["s1"]);
    in_txn(&mut s, "ROLLBACK");
    assert_eq!(one(&core, &mut s, "SELECT current_schema()"), "public");
}

#[test]
fn expression_forms_and_argument_checks() {
    let (core, _g) = new_core("cs-expr");
    let mut s = ddl_session();
    assert_eq!(schemas(&core, &mut s, "NULL"), "<null>");
    assert_eq!(
        one(&core, &mut s, "SELECT coalesce(current_schema(), 'none')"),
        "public"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT current_schema() WHERE current_schema() = 'public'"
        ),
        "public"
    );
    for sql in [
        "SELECT current_schemas()",
        "SELECT current_schemas(1)",
        "SELECT current_schema(1)",
        "SELECT current_schemas(true, false)",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42883", "{sql}");
    }
    assert_eq!(code(&core, &mut s, "SELECT current_schemas(x)"), "42703");
}

#[test]
fn never_exposes_internal_keys_after_schema_churn() {
    let (core, _g) = new_core("cs-ids");
    let mut s = ddl_session();
    for i in 0..5 {
        ok(&core, &mut s, &format!("CREATE SCHEMA churn{i}"));
        ok(&core, &mut s, &format!("DROP SCHEMA churn{i}"));
    }
    ok(&core, &mut s, "CREATE SCHEMA keep");
    ok(&core, &mut s, "SET search_path = keep, public");
    let v = row(
        &core,
        &mut s,
        "SELECT current_schema(), current_schemas(true)",
    );
    assert!(v.iter().all(|c| !c.contains('#')), "{v:?}");
    assert_eq!(v[0], "keep");
}

#[test]
fn other_contexts_fail_closed() {
    let (core, _g) = new_core("cs-closed");
    let mut s = ddl_session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (1, 'a')");
    for sql in [
        "SELECT current_schema() FROM t",
        "SELECT id FROM t WHERE name = current_schema()",
        "INSERT INTO t (id, name) VALUES (2, current_schema())",
        "CREATE TABLE d (id BIGINT PRIMARY KEY, n TEXT DEFAULT current_schema())",
        "CREATE TABLE c (id BIGINT PRIMARY KEY, n TEXT CHECK (n = current_schema()))",
        "CREATE VIEW v AS SELECT current_schema() AS s",
    ] {
        let c = code(&core, &mut s, sql);
        assert!(
            ["0A000", "42601", "42883", "42703"].contains(&c.as_str()),
            "{sql}: {c}"
        );
    }
    // 拒否された文は何も作らない。
    assert_eq!(code(&core, &mut s, "SELECT s FROM v LIMIT 5"), "42P01");
}

#[test]
fn udf_cannot_shadow_schema_functions() {
    let (core, _g) = new_core("cs-udf");
    let mut s = ddl_session();
    for name in ["current_schema", "current_schemas"] {
        let c = code(
            &core,
            &mut s,
            &format!("CREATE FUNCTION {name}() RETURNS float AS 1"),
        );
        assert!(c == "42723" || c == "42601" || c == "0A000", "{name}: {c}");
    }
}

#[test]
fn session_less_entry_uses_the_default_path() {
    let (core, _g) = new_core("cs-nosession");
    let r = core
        .execute_sql(&ctx(TENANT), "SELECT current_schema()")
        .expect("session-less");
    assert_eq!(r.rows[0].cells[0], Cell::Text("public".to_string()));
}
