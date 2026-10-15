//! `json`（入力テキストを原文のまま保存する型）の演算子・関数（対象ビヘイビア: TABLE-25。
//! Issue #2180）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25
//! （関連: TABLE-14・NOSQL-8・ERR-4）。
//!
//! 実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql` を production 経路として検証する。
//! 期待値は PostgreSQL 17 の実測（`json` 型の `->`・`->>`・`#>`・`#>>`・`json_*`・`to_json`）に
//! 合わせて固定する。固定する契約:
//! - `->`・`#>` は原文の部分テキスト（空白・数値表記・重複キーを保持。重複キーは最後の値）を
//!   json で返し、`->>`・`#>>` は text（文字列はエスケープを復号、JSON の `null` は NULL）。
//! - jsonb 専用演算子（`@>` `<@` `?` `?|` `?&`）・比較・ORDER BY は `42883`。
//! - テナント境界（他テナントの行が結果にもエラーにも現れない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "textdocs";

/// 空白・キー順・数値表記・重複キー・エスケープ・JSON null を含む原文（外側にも空白を置く）。
const DOC: &str = r#" {"a" :  [ 1 , 2 ] , "b":"xAy", "d":1.0e2, "dup":1, "dup" : 2, "n":null, "o":{"x" : 1.0}, "p":{"q":1},"p":{"r":2}} "#;

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

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("json-text-operators");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, doc: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, doc, docb, n) \
             VALUES ({id}, '[0.1,0.2]', 'k', '{doc}', '{{\"a\":1}}', 3) \
             USING OPERATION_ID 'seed-{id}'"
        ),
    )
    .expect("insert should succeed");
}

fn cell(core: &EngineCore, ctx: &PolicyContext, expr: &str) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT {expr} FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
        .unwrap_or_else(|e| panic!("select {expr} should succeed: {}", e.wire_code()));
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

fn code(core: &EngineCore, ctx: &PolicyContext, expr: &str) -> String {
    core.execute_sql(
        ctx,
        &format!("SELECT {expr} FROM {TABLE} WHERE id = 1 LIMIT 1"),
    )
    .expect_err("statement should fail")
    .wire_code()
    .to_string()
}

fn code_sql(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    core.execute_sql(ctx, sql)
        .expect_err("statement should fail")
        .wire_code()
        .to_string()
}

fn json(s: &str) -> Cell {
    Cell::Json(s.to_string())
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn ids(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} ORDER BY id LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("where {where_clause} should succeed: {}", e.wire_code()));
    result.rows.iter().map(|r| r.id).collect()
}

fn seeded() -> (EngineCore, std::path::PathBuf, PolicyContext) {
    let (core, path) = new_core();
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, DOC);
    (core, path, alice)
}

#[test]
fn stored_text_is_returned_verbatim_including_duplicate_keys() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    assert_eq!(cell(&core, &alice, "doc"), json(DOC));
}

#[test]
fn arrow_returns_original_partial_text_as_json() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    // 外側の空白は除かれ、内側の空白・数値表記はそのまま。
    assert_eq!(cell(&core, &alice, "doc -> 'a'"), json("[ 1 , 2 ]"));
    assert_eq!(cell(&core, &alice, "doc -> 'o'"), json(r#"{"x" : 1.0}"#));
    assert_eq!(cell(&core, &alice, "doc -> 'd'"), json("1.0e2"));
    // 重複キーは最後の値。
    assert_eq!(cell(&core, &alice, "doc -> 'dup'"), json("2"));
    // JSON null は json の `null`、不在キーは SQL の NULL。
    assert_eq!(cell(&core, &alice, "doc -> 'n'"), json("null"));
    assert_eq!(cell(&core, &alice, "doc -> 'zz'"), Cell::Null);
    // 添字（負値は末尾から）と連鎖。
    assert_eq!(cell(&core, &alice, "doc -> 'a' -> 0"), json("1"));
    assert_eq!(cell(&core, &alice, "doc -> 'a' -> -1"), json("2"));
    assert_eq!(cell(&core, &alice, "doc -> 'a' -> 5"), Cell::Null);
    assert_eq!(cell(&core, &alice, "doc -> 'o' -> 'x'"), json("1.0"));
    // 型の不一致は NULL。
    assert_eq!(cell(&core, &alice, "doc -> 0"), Cell::Null);
    assert_eq!(cell(&core, &alice, "doc -> 'a' -> 'k'"), Cell::Null);
}

#[test]
fn double_arrow_returns_text_with_decoded_strings() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    assert_eq!(cell(&core, &alice, "doc ->> 'b'"), text("xAy"));
    assert_eq!(cell(&core, &alice, "doc ->> 'd'"), text("1.0e2"));
    assert_eq!(cell(&core, &alice, "doc ->> 'o'"), text(r#"{"x" : 1.0}"#));
    assert_eq!(cell(&core, &alice, "doc ->> 'n'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "doc ->> 'zz'"), Cell::Null);
}

#[test]
fn path_operators_follow_postgres_json_semantics() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    assert_eq!(cell(&core, &alice, "doc #> '{o,x}'"), json("1.0"));
    assert_eq!(cell(&core, &alice, "doc #>> '{o,x}'"), text("1.0"));
    assert_eq!(cell(&core, &alice, "doc #> '{a,1}'"), json("2"));
    assert_eq!(cell(&core, &alice, "doc #> '{a,-1}'"), json("2"));
    assert_eq!(cell(&core, &alice, "doc #> '{a,x}'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "doc #> '{b,x}'"), Cell::Null);
    // 途中段に重複キーがある場合は、辿り切れた出現のうち最後のもの（PostgreSQL 17 の実測）。
    assert_eq!(cell(&core, &alice, "doc #> '{p,q}'"), json("1"));
    assert_eq!(cell(&core, &alice, "doc #> '{p,r}'"), json("2"));
}

#[test]
fn json_functions_return_postgres_values() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    assert_eq!(cell(&core, &alice, "json_typeof(doc)"), text("object"));
    assert_eq!(
        cell(&core, &alice, "json_typeof(doc -> 'a')"),
        text("array")
    );
    assert_eq!(
        cell(&core, &alice, "json_typeof(doc -> 'b')"),
        text("string")
    );
    assert_eq!(
        cell(&core, &alice, "json_typeof(doc -> 'd')"),
        text("number")
    );
    assert_eq!(cell(&core, &alice, "json_typeof(doc -> 'n')"), text("null"));
    assert_eq!(cell(&core, &alice, "json_typeof('true')"), text("boolean"));
    assert_eq!(cell(&core, &alice, "json_typeof(doc -> 'zz')"), Cell::Null);

    assert_eq!(
        cell(&core, &alice, "json_array_length(doc -> 'a')"),
        Cell::SignedInteger(2)
    );
    assert_eq!(
        cell(&core, &alice, "json_array_length('[1,[2,3],null]')"),
        Cell::SignedInteger(3)
    );
    assert_eq!(
        cell(&core, &alice, "json_array_length(doc -> 'zz')"),
        Cell::Null
    );
    // json_array_length は int4: 整数除算（3 / 2 = 1）として評価される（Issue #2180）。
    assert_eq!(
        cell(&core, &alice, "json_array_length('[1,[2,3],null]') / 2"),
        Cell::SignedInteger(1)
    );
    // int4 式として添字に使える。
    assert_eq!(
        cell(
            &core,
            &alice,
            "(doc -> 'a') -> (json_array_length(doc -> 'a') - 2)"
        ),
        cell(&core, &alice, "(doc -> 'a') -> 0")
    );

    assert_eq!(
        cell(&core, &alice, "json_extract_path(doc, 'o', 'x')"),
        json("1.0")
    );
    assert_eq!(
        cell(&core, &alice, "json_extract_path(doc, 'a', '1')"),
        json("2")
    );
    assert_eq!(
        cell(&core, &alice, "json_extract_path_text(doc, 'b')"),
        text("xAy")
    );
    // パス要素の NULL（不在キーの結果）は結果 NULL。
    assert_eq!(
        cell(&core, &alice, "json_extract_path(doc, doc ->> 'zz')"),
        Cell::Null
    );
}

#[test]
fn json_build_functions_and_to_json_use_postgres_output_format() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    assert_eq!(
        cell(&core, &alice, "json_build_object('a', 1, 'b', 2)"),
        json(r#"{"a" : 1, "b" : 2}"#)
    );
    assert_eq!(cell(&core, &alice, "json_build_object()"), json("{}"));
    assert_eq!(cell(&core, &alice, "json_build_array()"), json("[]"));
    assert_eq!(
        cell(
            &core,
            &alice,
            "json_build_object('a', doc -> 'zz', 'b', 'x\"y', 'd', 1.50, 'f', doc -> 'o')"
        ),
        json(r#"{"a" : null, "b" : "x\"y", "d" : 1.50, "f" : {"x" : 1.0}}"#)
    );
    assert_eq!(
        cell(&core, &alice, "json_build_array(1, 'a', doc -> 'zz', 1.50)"),
        json(r#"[1, "a", null, 1.50]"#)
    );
    assert_eq!(
        cell(&core, &alice, "json_build_object(1, 2, 2.5, 3)"),
        json(r#"{"1" : 2, "2.5" : 3}"#)
    );
    assert_eq!(
        cell(&core, &alice, "json_build_object('a', 1, 'a', 2)"),
        json(r#"{"a" : 1, "a" : 2}"#)
    );
    assert_eq!(cell(&core, &alice, "to_json('a')"), json("\"a\""));
    assert_eq!(cell(&core, &alice, "to_json(n)"), json("3"));
    assert_eq!(
        cell(&core, &alice, "to_json(DATE '2024-01-02')"),
        json("\"2024-01-02\"")
    );
    assert_eq!(
        cell(&core, &alice, "to_json(TIMESTAMP '2024-01-02 03:04:05')"),
        json("\"2024-01-02T03:04:05\"")
    );
    // json 値は原文のまま埋め込む。
    assert_eq!(
        cell(&core, &alice, "to_json(doc -> 'a')"),
        json("[ 1 , 2 ]")
    );
    assert_eq!(
        cell(&core, &alice, "json_build_array(doc -> 'a', docb)"),
        json(r#"[[ 1 , 2 ], {"a":1}]"#)
    );
}

#[test]
fn json_function_errors_use_postgres_sqlstates() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    assert_eq!(code(&core, &alice, "json_array_length(doc)"), "22023");
    assert_eq!(code(&core, &alice, "json_array_length('1')"), "22023");
    assert_eq!(code(&core, &alice, "json_build_object('a')"), "22023");
    // NULL のキーは拒否する（PostgreSQL は 22004。分類追加までは 22023）。
    assert_eq!(
        code(&core, &alice, "json_build_object(doc ->> 'zz', 1)"),
        "22023"
    );
    assert_eq!(code(&core, &alice, "json_build_object(doc, 1)"), "22023");
    // 引数個数・型の不一致は `42883`。
    assert_eq!(code(&core, &alice, "json_extract_path(doc)"), "42883");
    assert_eq!(code(&core, &alice, "json_typeof(lang)"), "42883");
    assert_eq!(code(&core, &alice, "json_typeof(docb)"), "42883");
    assert_eq!(code(&core, &alice, "json_typeof(doc, doc)"), "42883");
    // 不正な JSON リテラルは `22P02`。
    assert_eq!(code(&core, &alice, "json_typeof('{')"), "22P02");
    // 集合返却関数は未対応。
    assert_eq!(code(&core, &alice, "json_object_keys(doc)"), "0A000");
    // ベクトル値は json へ変換できない。
    assert_eq!(code(&core, &alice, "to_json(embedding)"), "0A000");
}

#[test]
fn jsonb_only_operators_on_json_are_undefined() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    for expr in [
        "doc @> '{}'",
        "doc ? 'a'",
        "doc ?| '{a}'",
        "doc ?& '{a}'",
        "(doc -> 'a') @> '{}'",
        "doc -> 'a' ?| '{x}'",
        "json_build_object('a', 1) @> '{}'",
    ] {
        assert_eq!(code(&core, &alice, expr), "42883", "{expr}");
    }
    assert_eq!(
        code_sql(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE doc @> '{{}}' LIMIT 10")
        ),
        "42883"
    );
    assert_eq!(
        code_sql(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE '{{}}' <@ doc LIMIT 10")
        ),
        "42883"
    );
    assert_eq!(
        code_sql(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE (doc -> 'a') ? 'x' LIMIT 10")
        ),
        "42883"
    );
}

#[test]
fn json_values_have_no_comparison_or_ordering_and_cannot_be_consumed_elsewhere() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    for clause in [
        "doc -> 'a' = '1'",
        "doc -> 'a' = doc -> 'a'",
        "doc = doc -> 'a'",
    ] {
        assert_eq!(
            code_sql(
                &core,
                &alice,
                &format!("SELECT id FROM {TABLE} WHERE {clause} LIMIT 10")
            ),
            "42883",
            "{clause}"
        );
    }
    assert_eq!(
        code_sql(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY doc -> 'a' LIMIT 10")
        ),
        "42883"
    );
    // 算術・関数引数・CASE などは jsonb と同じ fail-closed の `0A000`。
    assert_eq!(code(&core, &alice, "doc + 1"), "0A000");
    assert_eq!(code(&core, &alice, "vec_norm(doc)"), "0A000");
    assert_eq!(code(&core, &alice, "COALESCE(doc, doc)"), "0A000");
    assert_eq!(code(&core, &alice, "length(doc -> 'a')"), "0A000");
}

#[test]
fn existing_declarative_equality_path_is_unchanged() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    insert(&core, &alice, 2, r#"{"a":2}"#);
    // 格納値は重複キーでも値等価（最後の値）で一致する。
    insert(&core, &alice, 3, r#"{"k":1, "k" : 2}"#);
    assert_eq!(ids(&core, &alice, r#"doc = '{"k":2}'"#), vec![3]);
    assert_eq!(ids(&core, &alice, r#"doc = '{"a":2, "a":2}'"#), vec![2]);
    assert_eq!(ids(&core, &alice, "doc IS NOT NULL"), vec![1, 2, 3]);
    // jsonb 列の右辺リテラルの重複キーは従来どおり拒否。
    assert_eq!(
        code_sql(
            &core,
            &alice,
            &format!(r#"SELECT id FROM {TABLE} WHERE docb = '{{"a":1,"a":2}}' LIMIT 10"#)
        ),
        "22P02"
    );
}

#[test]
fn where_clause_can_use_json_text_extraction() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    insert(&core, &alice, 2, r#"{"b":"zzz"}"#);
    assert_eq!(ids(&core, &alice, "doc ->> 'b' = 'xAy'"), vec![1]);
    assert_eq!(
        ids(&core, &alice, "json_typeof(doc -> 'a') = 'array'"),
        vec![1]
    );
    assert_eq!(ids(&core, &alice, "doc #>> '{b}' = 'zzz'"), vec![2]);
    assert_eq!(
        ids(&core, &alice, "json_array_length(doc -> 'a') = 2"),
        vec![1]
    );
}

#[test]
fn tenant_boundary_hides_other_tenants_rows_from_json_expressions() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    let bob = ctx_for("bob");
    assert!(ids(&core, &bob, "doc ->> 'b' = 'xAy'").is_empty());
    let result = core
        .execute_sql(
            &bob,
            &format!("SELECT doc -> 'a' FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
        .expect("select");
    assert!(result.rows.is_empty());
    assert_eq!(cell(&core, &alice, "doc -> 'dup'"), json("2"));
}

#[test]
fn bare_null_literal_arguments_are_accepted_by_json_functions() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    assert_eq!(cell(&core, &alice, "json_typeof(NULL)"), Cell::Null);
    assert_eq!(cell(&core, &alice, "to_json(NULL)"), Cell::Null);
    assert_eq!(
        cell(&core, &alice, "json_extract_path(doc, 'a', NULL)"),
        Cell::Null
    );
    assert_eq!(
        cell(&core, &alice, "json_build_array(NULL)"),
        json("[null]")
    );
    assert_eq!(
        cell(&core, &alice, "json_build_object('a', NULL)"),
        json(r#"{"a" : null}"#)
    );
    // NULL のキーは拒否する（PostgreSQL は 22004。分類追加までは 22023）。
    assert_eq!(code(&core, &alice, "json_build_object(NULL, 1)"), "22023");
}

#[test]
fn unsupported_positions_fail_closed_with_fixed_codes() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    // FROM なしの SELECT（定数式）は jsonb 関数（#1599）と同じ経路で評価できる。
    assert_eq!(
        core.execute_sql(&alice, "SELECT json_build_object('a', 1)")
            .expect("constant json function should succeed")
            .rows
            .first()
            .and_then(|r| r.cells.first())
            .cloned(),
        Some(json("{\"a\" : 1}"))
    );
    // json 式の GROUP BY・DISTINCT・集計引数は構文段で受理されない（42601）。
    for sql in [
        format!("SELECT doc -> 'a' AS v, COUNT(*) FROM {TABLE} GROUP BY doc -> 'a' LIMIT 10"),
        format!("SELECT DISTINCT doc -> 'a' FROM {TABLE} LIMIT 10"),
        format!("SELECT COUNT(doc -> 'a') FROM {TABLE} LIMIT 1"),
    ] {
        assert_eq!(code_sql(&core, &alice, &sql), "42601", "{sql}");
    }
    // json 列そのものの ORDER BY は従来どおり 22000。
    assert_eq!(
        code_sql(
            &core,
            &alice,
            &format!("SELECT doc FROM {TABLE} ORDER BY doc LIMIT 10")
        ),
        "22000"
    );
}

#[test]
fn predicate_update_with_json_extraction_is_idempotent_by_operation_id() {
    let (core, path, alice) = seeded();
    let _guard = CleanupGuard(path);
    let update =
        format!("UPDATE {TABLE} SET n = 9 WHERE doc ->> 'b' = 'xAy' USING OPERATION_ID 'upd-1'");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &update)
        .expect("first update should succeed");
    // 同一文の再送は commit 済みと確定でき 23505（content hash は AST 上で不変）。
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &update)
        .expect_err("resend should be rejected");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(cell(&core, &alice, "n"), Cell::SignedInteger(9));
}
