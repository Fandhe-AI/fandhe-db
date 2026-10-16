//! `jsonb_path_query_array`・`jsonb_path_query_first`・`jsonb_path_exists`・`jsonb_path_match`
//! （対象ビヘイビア: TABLE-25。Issue #2185）の結合テスト。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-25。
//!
//! `tests/json_functions.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql` を production 経路として検証）。固定する契約: strict な NULL の扱い、
//! 結果の形、lax／strict と `silent` によるエラー抑止、抑止できないエラー（`42704`・`22023`・
//! `0A000`・`54000`）、束縛時の path 検査（行が無くても発生）、使える位置、起動時上限の反映、
//! テナント境界。期待値は PostgreSQL 17 の挙動に合わせたが、実機との突き合わせは未実施（推定を含む）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::jsonpath_limits::JsonPathLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "jpdocs";
const DOC: &str = r#"{"a":[1,2,3],"b":{"c":"x"},"n":null,"t":true}"#;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    )
}

fn open_core(tag: &str, limits: Option<JsonPathLimits>) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_jsonpath_limits(l),
        None => core,
    };
    (core, path)
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `docs` の各要素を id = 1.. で挿入する（`None` は docb が NULL）。
fn seed(core: &EngineCore, ctx: &PolicyContext, docs: &[Option<&str>]) {
    for (i, d) in docs.iter().enumerate() {
        let id = i + 1;
        let docb = match d {
            Some(d) => format!("'{d}'"),
            None => "NULL".to_string(),
        };
        core.execute_sql_in_session(
            ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, doc, docb, n) \
                 VALUES ({id}, '[0.1,0.2]', 'k{id}', '{{}}', {docb}, {id}) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .expect("insert should succeed");
    }
}

fn setup() -> (EngineCore, CleanupGuard, PolicyContext) {
    let (core, path) = open_core("json-path-functions", None);
    let guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(DOC), None]);
    (core, guard, alice)
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    core.execute_sql(ctx, sql)
        .expect_err("statement should fail")
        .wire_code()
        .to_string()
}

fn cell(core: &EngineCore, ctx: &PolicyContext, expr: &str, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .unwrap_or_else(|e| panic!("select {expr} should succeed: {}", e.wire_code()));
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

fn err_code(core: &EngineCore, ctx: &PolicyContext, expr: &str, id: u64) -> String {
    code(
        core,
        ctx,
        &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
    )
}

fn json(s: &str) -> Cell {
    Cell::Json(s.to_string())
}

fn matching(core: &EngineCore, ctx: &PolicyContext, pred: &str) -> Vec<i64> {
    let r = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("{pred} should succeed: {}", e.wire_code()));
    let mut v: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Integer(x) => i64::try_from(*x).expect("fits"),
            Cell::SignedInteger(x) => *x,
            other => panic!("unexpected id cell {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn query_array_and_first_shape_results() {
    let (core, _g, alice) = setup();
    let c = |e: &str| cell(&core, &alice, e, 1);
    assert_eq!(c("jsonb_path_query_array(docb, '$.a[*]')"), json("[1,2,3]"));
    assert_eq!(c("jsonb_path_query_array(docb, '$.missing')"), json("[]"));
    assert_eq!(
        c("jsonb_path_query_array(docb, '$.a[*] ? (@ > 1)')"),
        json("[2,3]")
    );
    assert_eq!(c("jsonb_path_query_first(docb, '$.a[*]')"), json("1"));
    assert_eq!(
        c("jsonb_path_query_first(docb, '$.b')"),
        json(r#"{"c":"x"}"#)
    );
    assert_eq!(c("jsonb_path_query_first(docb, '$.missing')"), Cell::Null);
}

#[test]
fn exists_and_match_return_booleans() {
    let (core, _g, alice) = setup();
    let c = |e: &str| cell(&core, &alice, e, 1);
    assert_eq!(c("jsonb_path_exists(docb, '$.a')"), Cell::Bool(true));
    assert_eq!(c("jsonb_path_exists(docb, '$.zzz')"), Cell::Bool(false));
    assert_eq!(c("jsonb_path_match(docb, '$.a[0] == 1')"), Cell::Bool(true));
    assert_eq!(c("jsonb_path_match(docb, '$.a[0] > 5')"), Cell::Bool(false));
    // 単一の null は NULL。
    assert_eq!(c("jsonb_path_match(docb, '$.n')"), Cell::Null);
    // 単一の真偽値以外は 22038、silent なら NULL。
    assert_eq!(
        err_code(&core, &alice, "jsonb_path_match(docb, '$.a[*]')", 1),
        "22038"
    );
    assert_eq!(
        c("jsonb_path_match(docb, '$.a[*]', '{}', true)"),
        Cell::Null
    );
}

#[test]
fn null_arguments_make_every_function_return_null() {
    let (core, _g, alice) = setup();
    for f in [
        "jsonb_path_query_array",
        "jsonb_path_query_first",
        "jsonb_path_exists",
        "jsonb_path_match",
    ] {
        let c = |e: String, id: u64| cell(&core, &alice, &e, id);
        assert_eq!(c(format!("{f}(docb, '$.a')"), 2), Cell::Null, "{f} column");
        assert_eq!(c(format!("{f}(NULL, '$.a')"), 1), Cell::Null, "{f} target");
        assert_eq!(c(format!("{f}(docb, NULL)"), 1), Cell::Null, "{f} path");
        assert_eq!(
            c(format!("{f}(docb, '$.a', NULL)"), 1),
            Cell::Null,
            "{f} vars"
        );
        assert_eq!(
            c(format!("{f}(docb, '$.a', '{{}}', NULL)"), 1),
            Cell::Null,
            "{f} silent"
        );
    }
}

#[test]
fn lax_strict_and_silent_control_suppressible_errors() {
    let (core, _g, alice) = setup();
    let c = |e: &str| cell(&core, &alice, e, 1);
    assert_eq!(
        c("jsonb_path_query_array(docb, 'lax $.missing')"),
        json("[]")
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_path_query_array(docb, 'strict $.missing')",
            1
        ),
        "2203A"
    );
    assert_eq!(
        c("jsonb_path_query_array(docb, 'strict $.missing', '{}', true)"),
        json("[]")
    );
    for silent in ["true", "TRUE", "'yes'", "'t'"] {
        assert_eq!(
            c(&format!(
                "jsonb_path_exists(docb, 'strict $.missing', '{{}}', {silent})"
            )),
            Cell::Null,
            "silent = {silent}"
        );
    }
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_path_exists(docb, 'strict $.missing', '{}', false)",
            1
        ),
        "2203A"
    );
}

#[test]
fn representative_error_codes_and_their_suppression() {
    let (core, _g, alice) = setup();
    for (path_lit, wire) in [
        ("strict $.b[0]", "22039"),
        ("strict $.a.keyvalue()", "2203C"),
        ("strict $.a[10]", "22033"),
        ("-$.b", "2203B"),
        ("$.a[*] + 1", "22038"),
        ("1 / 0", "22012"),
        ("$.b.abs()", "22036"),
    ] {
        let sql = format!("jsonb_path_query_array(docb, '{path_lit}')");
        assert_eq!(err_code(&core, &alice, &sql, 1), wire, "{path_lit}");
        let silent = format!("jsonb_path_query_array(docb, '{path_lit}', '{{}}', true)");
        let r = core.execute_sql(
            &alice,
            &format!("SELECT {silent} FROM {TABLE} WHERE id = 1 LIMIT 1"),
        );
        assert!(r.is_ok(), "{path_lit} should be suppressed by silent");
    }
}

#[test]
fn unsuppressible_errors_ignore_silent() {
    let (core, _g, alice) = setup();
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_path_exists(docb, '$x', '{}', true)",
            1
        ),
        "42704"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_path_exists(docb, '$x', '[1]', true)",
            1
        ),
        "22023"
    );
    assert_eq!(
        cell(
            &core,
            &alice,
            r#"jsonb_path_exists(docb, '$.a[0] == $x', '{"x":1}')"#,
            1
        ),
        Cell::Bool(true)
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_path_exists(docb, '$.t.datetime()', '{}', true)",
            1
        ),
        "0A000"
    );
}

#[test]
fn path_literal_errors_are_raised_at_bind_time_even_without_rows() {
    let (core, path) = open_core("json-path-functions-bind", None);
    let _g = CleanupGuard(path);
    let alice = ctx_for("alice");
    for (lit, wire) in [
        ("$.", "42601"),
        ("", "22P02"),
        (r#""\u0000""#, "22P05"),
        (r#"$ ? (@ like_regex "a")"#, "0A000"),
    ] {
        let sql = format!("SELECT jsonb_path_exists(docb, '{lit}') FROM {TABLE} LIMIT 1");
        assert_eq!(code(&core, &alice, &sql), wire, "{lit:?}");
    }
    for expr in [
        "jsonb_path_exists(docb, lang)",
        "jsonb_path_exists(docb, lang || 'x')",
        "jsonb_path_exists(doc, '$')",
        "jsonb_path_exists(docb)",
        "jsonb_path_exists(docb, '$', '{}', true, 1)",
    ] {
        assert_eq!(
            code(
                &core,
                &alice,
                &format!("SELECT {expr} FROM {TABLE} LIMIT 1")
            ),
            "42883",
            "{expr}"
        );
    }
}

#[test]
fn silent_argument_accepts_only_constants() {
    let (core, _g, alice) = setup();
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_path_exists(docb, '$', '{}', 'maybe')",
            1
        ),
        "22P02"
    );
    for expr in [
        "jsonb_path_exists(docb, '$', '{}', n > 0)",
        "jsonb_path_exists(docb, '$', '{}', lang)",
    ] {
        let c = err_code(&core, &alice, expr, 1);
        assert!(
            matches!(c.as_str(), "0A000" | "42883" | "42804" | "42601"),
            "{expr}: {c}"
        );
    }
}

#[test]
fn set_returning_jsonb_path_query_is_reserved_with_0a000() {
    let (core, _g, alice) = setup();
    assert_eq!(
        err_code(&core, &alice, "jsonb_path_query(docb, '$.a[*]')", 1),
        "0A000"
    );
}

#[test]
fn usable_in_where_and_order_by_but_not_check() {
    let (core, _g, alice) = setup();
    assert_eq!(
        matching(&core, &alice, "jsonb_path_exists(docb, '$.a')"),
        vec![1]
    );
    assert_eq!(
        matching(&core, &alice, "jsonb_path_match(docb, '$.t')"),
        vec![1]
    );
    let e = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE NOT jsonb_path_exists(docb, '$.a') LIMIT 5"),
        )
        .expect_err("negated bare predicate");
    assert_eq!(e.wire_code(), "42601");
    let r = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} ORDER BY jsonb_path_query_first(docb, '$.a[0]') LIMIT 5"
            ),
        )
        .expect("order by a jsonb result");
    assert_eq!(r.rows.len(), 2);
    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT jsonb_path_exists('{{}}', '$') FROM {TABLE} LIMIT 1"),
        )
        .expect("literal target is a jsonb literal");
    assert_eq!(r.rows[0].cells[0], Cell::Bool(true));
    let e = core
        .execute_sql(
            &alice,
            "CREATE TABLE chk (id BIGINT, v INTEGER, CHECK (jsonb_path_exists('{}', '$')))",
        )
        .expect_err("CHECK with jsonb_path");
    assert!(matches!(e.wire_code(), "0A000" | "42601"));
}

#[test]
fn result_larger_than_the_json_limit_is_54000() {
    let (core, path) = open_core("json-path-functions-big", None);
    let _g = CleanupGuard(path);
    let alice = ctx_for("alice");
    // 約 100KB の文字列 1 個を持つ配列（文書自体は上限内）。同じ要素を 50 回返す path で
    // 結果配列が 4MiB 超になり、直列化上限で 54000 になる。
    let big = "x".repeat(100_000);
    let doc = format!("[\"{big}\"]");
    seed(&core, &alice, &[Some(&doc)]);
    let ok = cell(&core, &alice, "jsonb_path_query_array(docb, '$[*]')", 1);
    assert!(matches!(ok, Cell::Json(_)));
    let idx = vec!["0"; 50].join(", ");
    let c = err_code(
        &core,
        &alice,
        &format!("jsonb_path_query_array(docb, '$[{idx}]')"),
        1,
    );
    assert_eq!(c, "54000");
}

#[test]
fn startup_limits_apply_to_sql_evaluation() {
    let alice = ctx_for("alice");
    let q = "jsonb_path_query_array(docb, '$.**.**.**')";
    let (core, path) = open_core("json-path-functions-default", None);
    let _g = CleanupGuard(path);
    seed(&core, &alice, &[Some(DOC)]);
    assert!(core
        .execute_sql(
            &alice,
            &format!("SELECT {q} FROM {TABLE} WHERE id = 1 LIMIT 1")
        )
        .is_ok());
    let (small, path2) = open_core("json-path-functions-small", Some(JsonPathLimits::new(5, 4)));
    let _g2 = CleanupGuard(path2);
    seed(&small, &alice, &[Some(DOC)]);
    assert_eq!(err_code(&small, &alice, q, 1), "54000");
    // silent でも抑止されない。
    assert_eq!(
        err_code(
            &small,
            &alice,
            "jsonb_path_query_array(docb, '$.**.**.**', '{}', true)",
            1
        ),
        "54000"
    );
}

#[test]
fn other_tenants_rows_never_leak_into_results_or_errors() {
    let (core, _g, alice) = setup();
    let bob = ctx_for("bob");
    assert_eq!(
        matching(&core, &bob, "jsonb_path_exists(docb, '$.a')"),
        Vec::<i64>::new()
    );
    let e = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT jsonb_path_query_array(docb, 'strict $.zzz') FROM {TABLE} WHERE id = 1 LIMIT 5"
            ),
        )
        .expect_err("strict missing key");
    assert_eq!(e.wire_code(), "2203A");
    assert!(!e.to_string().contains("zzz"));
}

/// INSERT VALUES・UPDATE SET・CREATE VIEW 本文での扱いは、既存の jsonb 関数（Issue #1599）と
/// 同じ受理／SQLSTATE になる（新しい受理経路を黙って増やさない）。
#[test]
fn unsupported_contexts_match_existing_jsonb_functions() {
    let (core, _g, alice) = setup();
    let run = |sql: &str| {
        core.execute_sql_in_session(&alice, &mut SessionState::default(), sql)
            .map(|_| ())
            .map_err(|e| e.wire_code().to_string())
    };
    let pairs = [
        (
            format!("INSERT INTO {TABLE} (id, embedding, docb) VALUES (9, '[0.1,0.2]', jsonb_path_query_first('{{}}', '$')) USING OPERATION_ID 'v1'"),
            format!("INSERT INTO {TABLE} (id, embedding, docb) VALUES (9, '[0.1,0.2]', jsonb_build_array('{{}}')) USING OPERATION_ID 'v1'"),
        ),
        (
            format!("UPDATE {TABLE} SET docb = jsonb_path_query_first(docb, '$') WHERE id = 1 USING OPERATION_ID 'u1'"),
            format!("UPDATE {TABLE} SET docb = jsonb_build_array(docb) WHERE id = 1 USING OPERATION_ID 'u1'"),
        ),
        (
            format!("CREATE VIEW vv AS SELECT id FROM {TABLE} WHERE jsonb_path_exists(docb, '$.a')"),
            format!("CREATE VIEW vv2 AS SELECT id FROM {TABLE} WHERE jsonb_array_length(docb) > 0"),
        ),
    ];
    for (new, old) in pairs {
        let a = run(&new);
        let b = run(&old);
        assert_eq!(a.is_ok(), b.is_ok(), "{new}: {a:?} vs {b:?}");
        if let (Err(x), Err(y)) = (&a, &b) {
            assert_eq!(x, y, "{new}");
        }
    }
}
