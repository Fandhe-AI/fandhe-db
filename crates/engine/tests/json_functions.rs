//! jsonb 関数群 `jsonb_typeof`・`jsonb_array_length`・`jsonb_extract_path(_text)`・
//! `jsonb_build_object`・`jsonb_build_array`・`to_jsonb`（対象ビヘイビア: TABLE-25。Issue #1599）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25（関連: TABLE-14・SQL-24）。
//!
//! `tests/json_containment_operators.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql` を production 経路として検証）。固定する契約: 各関数の NULL の扱い、
//! SQLSTATE（`22023`・`22004`・`42883`・`42804`・`22P02`・`54000`・`0A000`）、結果列の型、
//! 投影・WHERE・ORDER BY・集計・FROM なし SELECT・UPDATE での利用、テナント境界、再送時の内容検査。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "fndocs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("d", ColumnType::Date, true),
            ColumnDef::new("ts", ColumnType::Timestamp, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("json-functions");
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

/// `docb` が SQL の NULL の行を含め、`docs` の各要素を id = 1.. で挿入する（`None` は NULL）。
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
                "INSERT INTO {TABLE} (id, embedding, lang, doc, docb, n, d, ts) \
                 VALUES ({id}, '[0.1,0.2]', 'k{id}', '{{}}', {docb}, {id}, \
                 DATE '2024-03-05', TIMESTAMP '2024-03-05 01:02:03.500') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .expect("insert should succeed");
    }
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql} should succeed: {}", e.wire_code()));
    result
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::Integer(v) => *v,
            Cell::SignedInteger(v) => u64::try_from(*v).expect("non-negative"),
            other => panic!("unexpected id cell {other:?}"),
        })
        .collect()
}

fn matching(core: &EngineCore, ctx: &PolicyContext, pred: &str) -> Vec<u64> {
    let mut v = ids(
        core,
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 100"),
    );
    v.sort_unstable();
    v
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

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

#[test]
fn jsonb_typeof_reports_all_six_types_and_null() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"a":1}"#),
            Some(r#"[1]"#),
            Some(r#""s""#),
            Some("1.5"),
            Some("true"),
            Some("null"),
            None,
        ],
    );
    let expected = [
        text("object"),
        text("array"),
        text("string"),
        text("number"),
        text("boolean"),
        text("null"),
        Cell::Null,
    ];
    for (i, want) in expected.iter().enumerate() {
        assert_eq!(
            &cell(&core, &alice, "jsonb_typeof(docb)", (i + 1) as u64),
            want
        );
    }
    // 型なし文字列リテラルは jsonb として解釈する。裸の NULL は NULL。
    assert_eq!(
        cell(&core, &alice, r#"jsonb_typeof('{"x":[]}')"#, 1),
        text("object")
    );
    assert_eq!(cell(&core, &alice, "jsonb_typeof(NULL)", 1), Cell::Null);
    // パス演算子の結果（jsonb）にも使える。
    assert_eq!(
        cell(&core, &alice, "jsonb_typeof(docb -> 'a')", 1),
        text("number")
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_typeof(docb -> 'zz')", 1),
        Cell::Null
    );
}

#[test]
fn jsonb_array_length_counts_top_level_elements() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some("[]"),
            Some("[1,[2,3],{\"a\":[1]}]"),
            Some("{\"a\":1}"),
            Some("5"),
            None,
        ],
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_array_length(docb)", 1),
        Cell::SignedInteger(0)
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_array_length(docb)", 2),
        Cell::SignedInteger(3)
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_array_length(docb)", 5),
        Cell::Null
    );
    // オブジェクト・スカラーは 22023。
    assert_eq!(
        err_code(&core, &alice, "jsonb_array_length(docb)", 3),
        "22023"
    );
    assert_eq!(
        err_code(&core, &alice, "jsonb_array_length(docb)", 4),
        "22023"
    );
    // 結果列は int4。
    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT jsonb_array_length(docb) FROM {TABLE} WHERE id = 2 LIMIT 1"),
        )
        .expect("select");
    assert!(matches!(
        &r.columns[0],
        ColumnMeta::Computed {
            ty: Some(ColumnType::Integer),
            ..
        }
    ));
}

#[test]
fn jsonb_extract_path_matches_path_operators() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"a":{"b":[10,{"c":"x"}]},"n":null,"s":"str"}"#),
            None,
        ],
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 'a', 'b', '1')", 1),
        json(r#"{"c":"x"}"#)
    );
    assert_eq!(
        cell(
            &core,
            &alice,
            "jsonb_extract_path_text(docb, 'a', 'b', '1', 'c')",
            1
        ),
        text("x")
    );
    // 添字は負値も可。文字列は _text でクォートが外れ、jsonb 版は残る。
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 'a', 'b', '-2')", 1),
        json("10")
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 's')", 1),
        json("\"str\"")
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path_text(docb, 's')", 1),
        text("str")
    );
    // JSON の null: jsonb 版は null、_text 版は SQL の NULL。
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 'n')", 1),
        json("null")
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path_text(docb, 'n')", 1),
        Cell::Null
    );
    // 不在・型不一致は NULL。パス要素が NULL でも NULL。行が NULL でも NULL。
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 'zz')", 1),
        Cell::Null
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 'a', NULL)", 1),
        Cell::Null
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 'a')", 2),
        Cell::Null
    );
    // パス要素は text 式（列でもよい）。`#>` と同じ結果になる。
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, 'a', 'b')", 1),
        cell(&core, &alice, "docb #> '{a,b}'", 1)
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_extract_path(docb, lang)", 1),
        Cell::Null
    );
}

#[test]
fn jsonb_build_object_and_array_follow_postgres_rules() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"x":1}"#)]);
    assert_eq!(cell(&core, &alice, "jsonb_build_object()", 1), json("{}"));
    assert_eq!(cell(&core, &alice, "jsonb_build_array()", 1), json("[]"));
    // 値の NULL は JSON の null。キーは昇順に正規化される。
    assert_eq!(
        cell(
            &core,
            &alice,
            "jsonb_build_object('b', 1, 'a', NULL, 'c', 'q\"uote')",
            1
        ),
        json(r#"{"a":null,"b":1,"c":"q\"uote"}"#)
    );
    // 数値・bool のキーは文字列化される。重複キーは最後の値が勝つ。
    assert_eq!(
        cell(&core, &alice, "jsonb_build_object(1, 'a', 2.5, 'b')", 1),
        json(r#"{"1":"a","2.5":"b"}"#)
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_build_object('k', 1, 'k', 2)", 1),
        json(r#"{"k":2}"#)
    );
    // jsonb 値は入れ子になる。列・日付・時刻も使える。
    assert_eq!(
        cell(&core, &alice, "jsonb_build_object('doc', docb, 'n', n)", 1),
        json(r#"{"doc":{"x":1},"n":1}"#)
    );
    assert_eq!(
        cell(
            &core,
            &alice,
            "jsonb_build_array(1, 'a', NULL, docb, d, ts)",
            1
        ),
        json(r#"[1,"a",null,{"x":1},"2024-03-05","2024-03-05T01:02:03.5"]"#)
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_build_array(1.5, 0.1, 1e3)", 1),
        json("[1.5,0.1,1000]")
    );
    // 奇数個は 22023、NULL キーは 22004、jsonb のキーは 22023。
    assert_eq!(
        err_code(&core, &alice, "jsonb_build_object('a')", 1),
        "22023"
    );
    assert_eq!(
        err_code(&core, &alice, "jsonb_build_object(NULL, 1)", 1),
        "22004"
    );
    assert_eq!(
        err_code(&core, &alice, "jsonb_build_object(docb, 1)", 1),
        "22023"
    );
    // json 列・vector は拒否する。
    assert_eq!(
        err_code(&core, &alice, "jsonb_build_array(doc)", 1),
        "0A000"
    );
    assert_eq!(
        err_code(&core, &alice, "jsonb_build_array(embedding)", 1),
        "0A000"
    );
}

#[test]
fn odd_argument_count_is_a_runtime_check_so_empty_tables_succeed() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT jsonb_build_object('a') FROM {TABLE} LIMIT 5"),
        )
        .expect("an empty table never evaluates the call");
    assert!(r.rows.is_empty());
}

#[test]
fn to_jsonb_converts_scalars_and_rejects_unknown_types() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"x":[1]}"#), None]);
    assert_eq!(cell(&core, &alice, "to_jsonb(lang)", 1), json("\"k1\""));
    assert_eq!(cell(&core, &alice, "to_jsonb(n)", 1), json("1"));
    assert_eq!(
        cell(&core, &alice, "to_jsonb(d)", 1),
        json("\"2024-03-05\"")
    );
    assert_eq!(
        cell(&core, &alice, "to_jsonb(ts)", 1),
        json("\"2024-03-05T01:02:03.5\"")
    );
    assert_eq!(
        cell(&core, &alice, "to_jsonb(docb)", 1),
        json(r#"{"x":[1]}"#)
    );
    assert_eq!(cell(&core, &alice, "to_jsonb(docb)", 2), Cell::Null);
    assert_eq!(
        cell(&core, &alice, "to_jsonb(lower('A\"'))", 1),
        json(r#""a\"""#)
    );
    assert_eq!(cell(&core, &alice, "to_jsonb(n * 2 + 0.5)", 1), json("2.5"));
    // 型なしリテラル・裸の NULL は多相型を決められない（42804）。
    assert_eq!(err_code(&core, &alice, "to_jsonb('x')", 1), "42804");
    assert_eq!(err_code(&core, &alice, "to_jsonb(NULL)", 1), "42804");
    assert_eq!(err_code(&core, &alice, "to_jsonb(embedding)", 1), "0A000");
    assert_eq!(err_code(&core, &alice, "to_jsonb(doc)", 1), "0A000");
}

#[test]
fn argument_type_errors_use_fixed_sqlstates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#)]);
    for expr in [
        "jsonb_typeof(n)",
        "jsonb_typeof(lang)",
        "jsonb_typeof(doc)",
        "jsonb_typeof(docb, docb)",
        "jsonb_typeof()",
        "jsonb_array_length(n)",
        "jsonb_extract_path(docb)",
        "jsonb_extract_path(docb, n)",
        "jsonb_extract_path(lang, 'a')",
        "jsonb_extract_path_text(doc, 'a')",
        "to_jsonb()",
        "to_jsonb(n, n)",
    ] {
        assert_eq!(err_code(&core, &alice, expr, 1), "42883", "{expr}");
    }
    // 不正な jsonb リテラルは 22P02（本文は含めない）。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT jsonb_typeof('{{alice-secret') FROM {TABLE} LIMIT 1"),
        )
        .expect_err("invalid literal");
    assert_eq!(err.wire_code(), "22P02");
    assert!(!err.to_string().contains("alice-secret"));
}

#[test]
fn result_size_and_depth_limits_map_to_54000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // 深さ 16 の jsonb を 1 段包むと 17 になり拒否される。
    let deep16 = format!("{}{}", "[".repeat(16), "]".repeat(16));
    let deep15 = format!("{}{}", "[".repeat(15), "]".repeat(15));
    seed(&core, &alice, &[Some(&deep16), Some(&deep15)]);
    assert_eq!(
        err_code(&core, &alice, "jsonb_build_array(docb)", 1),
        "54000"
    );
    assert_eq!(
        err_code(&core, &alice, "jsonb_build_object('k', docb)", 1),
        "54000"
    );
    let wrapped = cell(&core, &alice, "jsonb_build_array(docb)", 2);
    assert_eq!(
        wrapped,
        json(&format!("{}{}", "[".repeat(16), "]".repeat(16)))
    );
}

#[test]
fn functions_work_in_where_order_by_aggregates_and_from_less_select() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[Some(r#"{"a":1}"#), Some(r#"[1,2,3]"#), Some(r#"[1]"#), None],
    );
    assert_eq!(
        matching(&core, &alice, "jsonb_typeof(docb) = 'array'"),
        vec![2, 3]
    );
    assert_eq!(
        matching(&core, &alice, "jsonb_typeof(docb) = 'object'"),
        vec![1]
    );
    assert_eq!(
        matching(&core, &alice, "jsonb_typeof(docb) = 'array' AND n > 2"),
        vec![3]
    );
    // jsonb_array_length は object に対して評価時エラーになるため、配列の行だけに絞って使う。
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE jsonb_typeof(docb) = 'array' \
                 ORDER BY jsonb_array_length(docb) LIMIT 10"
            )
        ),
        vec![3, 2]
    );
    // jsonb の結果で並べ替える。
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY jsonb_build_array(n) DESC LIMIT 10")
        ),
        vec![4, 3, 2, 1]
    );
    // 構築した jsonb を包含・存在演算子やパス演算子と組み合わせる。
    assert_eq!(
        matching(&core, &alice, "jsonb_build_object('k', n) @> '{\"k\":2}'"),
        vec![2]
    );
    assert_eq!(
        matching(&core, &alice, "jsonb_build_object('k', n) ? 'k'"),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        cell(&core, &alice, "jsonb_build_object('k', n) -> 'k'", 3),
        json("3")
    );
    // 集計の引数。
    let sum = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT SUM(jsonb_array_length(docb)) FROM {TABLE} \
                 WHERE jsonb_typeof(docb) = 'array'"
            ),
        )
        .expect("aggregate over jsonb_array_length");
    assert_eq!(sum.rows.len(), 1);
    // FROM なし SELECT。
    let r = core
        .execute_sql(
            &alice,
            "SELECT jsonb_build_object('a', 1, 'b', jsonb_build_array(1, 2)), \
             jsonb_typeof('[1]'), jsonb_array_length('[1,2]')",
        )
        .expect("FROM-less select");
    assert_eq!(r.rows[0].cells[0], json(r#"{"a":1,"b":[1,2]}"#));
    assert_eq!(r.rows[0].cells[1], text("array"));
    assert_eq!(r.rows[0].cells[2], Cell::SignedInteger(2));
    assert!(matches!(
        &r.columns[0],
        ColumnMeta::Computed {
            ty: Some(ColumnType::Jsonb),
            ..
        }
    ));
}

#[test]
fn unsupported_contexts_fail_closed() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#)]);
    // 集合返却関数は基盤ができるまで 0A000（名前は予約済み）。
    for f in [
        "jsonb_object_keys(docb)",
        "jsonb_array_elements(docb)",
        "jsonb_array_elements_text(docb)",
        "jsonb_each(docb)",
        "jsonb_each_text(docb)",
    ] {
        assert_eq!(err_code(&core, &alice, f, 1), "0A000", "{f}");
    }
    // jsonb の結果を算術・文字列関数へ渡すのは 0A000。
    assert_eq!(
        err_code(&core, &alice, "length(jsonb_build_array(1))", 1),
        "0A000"
    );
    // jsonb 列への式代入・INSERT VALUES の jsonb 関数は従来どおり拒否する。
    let run = |sql: &str| core.execute_sql_in_session(&alice, &mut SessionState::default(), sql);
    let e = run(&format!(
        "UPDATE {TABLE} SET docb = jsonb_build_object('a', 1) WHERE id = 1 \
         USING OPERATION_ID 'upd-jsonb-col'"
    ))
    .expect_err("jsonb column assignment from a function");
    assert_eq!(e.wire_code(), "0A000");
    let e = run(&format!(
        "INSERT INTO {TABLE} (id, embedding, docb) \
         VALUES (9, '[0.1,0.2]', jsonb_build_object('a', 1)) USING OPERATION_ID 'ins-fn'"
    ))
    .expect_err("jsonb function in VALUES");
    assert!(matches!(e.wire_code(), "0A000" | "42601" | "42883"));
    // CHECK 本体では未対応。
    let e = core
        .execute_sql(
            &alice,
            "CREATE TABLE chk (embedding VECTOR(2), docb JSONB, \
             CHECK (jsonb_typeof(docb) = 'object'))",
        )
        .map(|_| ())
        .expect_err("CHECK with a jsonb function must be rejected");
    assert!(matches!(e.wire_code(), "0A000" | "42601"));
    // 同名の関数は定義できない。
    let e = core
        .execute_sql(
            &alice,
            "CREATE FUNCTION jsonb_typeof(x) RETURNS DOUBLE AS x",
        )
        .map(|_| ())
        .expect_err("reserved name");
    assert!(matches!(e.wire_code(), "42723" | "42601"));
}

#[test]
fn text_column_can_be_updated_from_a_jsonb_function_and_resend_is_content_checked() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#), Some(r#"[1]"#)]);
    let run = |sql: &str| core.execute_sql_in_session(&alice, &mut SessionState::default(), sql);
    let upd = |col: &str| {
        format!("UPDATE {TABLE} SET lang = jsonb_typeof({col}) WHERE id = 1 USING OPERATION_ID 'upd-typeof'")
    };
    run(&upd("docb")).expect("first update succeeds");
    assert_eq!(cell(&core, &alice, "lang", 1), text("object"));
    // 同一内容の再送は 23505、関数の引数だけを変えた再送は内容不一致の 22023。
    assert_eq!(run(&upd("docb")).expect_err("resend").wire_code(), "23505");
    assert_eq!(
        run(&upd("docb -> 'a'"))
            .expect_err("different arg")
            .wire_code(),
        "22023"
    );
}

#[test]
fn other_tenants_rows_never_affect_results_or_errors() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    // alice の行は array_length がエラーになる（オブジェクト）が、bob の問い合わせには現れない。
    seed(&core, &alice, &[Some(r#"{"alice-secret":1}"#)]);
    assert_eq!(
        matching(&core, &bob, "jsonb_array_length(docb) > 0"),
        Vec::<u64>::new()
    );
    let r = core
        .execute_sql(
            &bob,
            &format!("SELECT jsonb_array_length(docb) FROM {TABLE} LIMIT 5"),
        )
        .expect("bob sees no rows, so no evaluation error");
    assert!(r.rows.is_empty());
    let e = core
        .execute_sql(
            &alice,
            &format!("SELECT jsonb_array_length(docb) FROM {TABLE} LIMIT 5"),
        )
        .expect_err("alice's own row is an object");
    assert_eq!(e.wire_code(), "22023");
    assert!(!e.to_string().contains("alice-secret"));
}

#[test]
fn jsonb_array_length_participates_in_integer_arithmetic() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some("[1,2,3]")]);
    // int4 なので整数除算（1）。倍精度除算（1.5）にならない。
    assert_eq!(
        cell(&core, &alice, "jsonb_array_length(docb) / 2", 1),
        Cell::SignedInteger(1)
    );
}

#[test]
fn build_object_size_check_uses_retained_entries_after_duplicate_keys() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // 約 1 MB の jsonb を上書きしても、最終的に残るのは小さな値のみで成功する。
    let big = format!("[\"{}\"]", "x".repeat(1_000_000));
    seed(&core, &alice, &[Some(&big)]);
    assert_eq!(
        cell(
            &core,
            &alice,
            "jsonb_build_object('k', docb, 'k', docb, 'k', docb, 'k', docb, 'k', 0)",
            1
        ),
        json("{\"k\":0}")
    );
    // 最終的に残る値の総量が上限を超えるときは 54000。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_build_object('a', docb, 'b', docb, 'c', docb, 'd', docb, 'e', docb)",
            1
        ),
        "54000"
    );
}
