//! jsonb のパス取得演算子 `->` `->>` `#>` `#>>`（対象ビヘイビア: TABLE-25。Issue #1596）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25（関連: TABLE-14・
//! SQL-26・SQL-42）。
//!
//! `tests/json_column.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql` を production 経路として検証）。固定する契約:
//! `->`・`#>` は jsonb、`->>`・`#>>` は text を返し、存在しないキー・範囲外の添字・容器の型
//! 不一致は SQL の NULL。優先順位（加減算より弱く比較より強い・左結合）。jsonb 値を消費
//! できない位置（比較・順序・集計・代入・CHECK）と `json` 列・非 jsonb 左辺の拒否コード。
//! テナント境界（他テナントの行が結果にもエラーにも現れない）。

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

const TABLE: &str = "pathdocs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("json-path-operators");
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

fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, docb: &str, n: i32) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, doc, docb, n, big) \
             VALUES ({id}, '[0.1,0.2]', '{lang}', '{{}}', {docb}, {n}, 7) \
             USING OPERATION_ID 'seed-{id}'"
        ),
    )
    .expect("insert should succeed");
}

/// 1 行 1 列の SELECT を `WHERE id = 1` で実行してセルを返す。
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

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
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

const DOC: &str = r#"'{"a":{"b":[10,20,{"c":"x\ny"}]},"s":"str","n":null,"k":1}'"#;

#[test]
fn key_and_index_access_returns_jsonb_or_text() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);

    assert_eq!(cell(&core, &alice, "docb -> 's'"), json("\"str\""));
    assert_eq!(cell(&core, &alice, "docb ->> 's'"), text("str"));
    assert_eq!(cell(&core, &alice, "docb -> 'k'"), json("1"));
    assert_eq!(cell(&core, &alice, "docb ->> 'k'"), text("1"));
    // ネストと連鎖（左結合）。
    assert_eq!(cell(&core, &alice, "docb -> 'a' -> 'b' -> 1"), json("20"));
    assert_eq!(cell(&core, &alice, "docb -> 'a' -> 'b' ->> 0"), text("10"));
    assert_eq!(
        cell(&core, &alice, "docb -> 'a' -> 'b' -> -1 ->> 'c'"),
        text("x\ny")
    );
    // 負の添字は末尾から数える。範囲外は NULL。
    assert_eq!(cell(&core, &alice, "docb -> 'a' -> 'b' -> -3"), json("10"));
    assert_eq!(cell(&core, &alice, "docb -> 'a' -> 'b' -> -4"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb -> 'a' -> 'b' -> 3"), Cell::Null);
    // JSON null: `->` は 'null'、`->>` は SQL の NULL。
    assert_eq!(cell(&core, &alice, "docb -> 'n'"), json("null"));
    assert_eq!(cell(&core, &alice, "docb ->> 'n'"), Cell::Null);
}

#[test]
fn missing_key_and_container_type_mismatch_return_null() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);

    assert_eq!(cell(&core, &alice, "docb -> 'zz'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb ->> 'zz'"), Cell::Null);
    // オブジェクトに添字・配列にキー・スカラーに適用はいずれも NULL。
    assert_eq!(cell(&core, &alice, "docb -> 0"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb -> 'a' -> 'b' -> 'x'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb -> 'k' -> 'x'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb -> 'k' -> 0"), Cell::Null);
}

#[test]
fn null_column_yields_null() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'k') \
             USING OPERATION_ID 'seed-null'"
        ),
    )
    .expect("insert");
    assert_eq!(cell(&core, &alice, "docb -> 'a'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb #>> '{a}'"), Cell::Null);
}

#[test]
fn path_operators_traverse_objects_and_arrays() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);

    assert_eq!(cell(&core, &alice, "docb #> '{a,b,1}'"), json("20"));
    assert_eq!(cell(&core, &alice, "docb #>> '{a,b,2,c}'"), text("x\ny"));
    assert_eq!(cell(&core, &alice, "docb #>> '{a,b,-1,c}'"), text("x\ny"));
    // 空パスは値全体。
    assert_eq!(
        cell(&core, &alice, "docb #> '{}'"),
        json(r#"{"a":{"b":[10,20,{"c":"x\ny"}]},"k":1,"n":null,"s":"str"}"#)
    );
    // 配列に対する非整数の要素・存在しないパス・NULL 要素は NULL。
    assert_eq!(cell(&core, &alice, "docb #> '{a,b,x}'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb #> '{a,zz,0}'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb #> '{a,NULL}'"), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb #>> '{k,x}'"), Cell::Null);
    // JSON null: `#>` は 'null'、`#>>` は NULL。
    assert_eq!(cell(&core, &alice, "docb #> '{n}'"), json("null"));
    assert_eq!(cell(&core, &alice, "docb #>> '{n}'"), Cell::Null);
}

#[test]
fn precedence_is_below_addition_above_comparison_and_left_associative() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", r#"'[10,20,30,{"x":"y"}]'"#, 2);

    // `docb -> 1 + 1` は `docb -> (1 + 1)`。
    assert_eq!(cell(&core, &alice, "docb -> 1 + 1"), json("30"));
    assert_eq!(cell(&core, &alice, "docb -> n"), json("30"));
    assert_eq!(cell(&core, &alice, "docb -> n + 1 ->> 'x'"), text("y"));
    // 左結合。
    assert_eq!(cell(&core, &alice, "docb -> 3 ->> 'x'"), text("y"));
    // WHERE での比較は演算子の結果（text）に対して働く。
    let hit = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE docb -> 3 ->> 'x' = 'y' LIMIT 10"),
        )
        .expect("filter should succeed");
    assert_eq!(hit.rows.len(), 1);
    let miss = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE docb -> 3 ->> 'x' = 'z' LIMIT 10"),
        )
        .expect("filter should succeed");
    assert!(miss.rows.is_empty());
}

#[test]
fn operand_type_rules_map_to_fixed_sqlstates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);
    let sel = |e: &str| format!("SELECT {e} FROM {TABLE} WHERE id = 1 LIMIT 1");

    // 左辺が jsonb でない（TEXT 列・数値・文字列リテラル）。
    assert_eq!(code(&core, &alice, &sel("lang -> 'k'")), "42883");
    assert_eq!(code(&core, &alice, &sel("n -> 'k'")), "42883");
    // 文字列リテラル始まりの投影項目は式として受理されない（従来どおり構文エラー）。
    assert_eq!(code(&core, &alice, &sel("'x' -> 'k'")), "42601");
    // `json` 列は原文保持の意味論が別 Issue のため未対応。
    assert_eq!(code(&core, &alice, &sel("doc -> 'k'")), "0A000");
    // 右辺の型: 小数・bigint の範囲・BIGINT 列は演算子が存在しない。
    assert_eq!(code(&core, &alice, &sel("docb -> 1.5")), "42883");
    assert_eq!(code(&core, &alice, &sel("docb -> 3000000000")), "42883");
    assert_eq!(code(&core, &alice, &sel("docb -> big")), "42883");
    // `#>` の右辺は text[] リテラルのみ。形式不正は 22P02。
    assert_eq!(code(&core, &alice, &sel("docb #> 'a'")), "22P02");
    assert_eq!(code(&core, &alice, &sel("docb #> lang")), "0A000");
    // jsonb 列の裸の参照は従来どおり式では使えない（22000）。
    assert_eq!(code(&core, &alice, &sel("vec_norm(docb)")), "22000");
}

#[test]
fn jsonb_values_are_rejected_where_they_cannot_be_consumed_yet() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);

    // 比較・ORDER BY・集計・関数引数・COALESCE は #1597 まで 0A000。
    for sql in [
        format!("SELECT id FROM {TABLE} WHERE docb -> 'a' = docb -> 'b' LIMIT 10"),
        format!("SELECT id FROM {TABLE} WHERE docb -> 'a' = 'x' LIMIT 10"),
        format!("SELECT COUNT(docb -> 'k') FROM {TABLE}"),
        // 非投影モードでは整数除算の切り捨てが入らないため、添字右辺の除算は拒否する。
        format!("SELECT id FROM {TABLE} WHERE docb -> n/2 ->> 'x' = 'y' LIMIT 10"),
        format!("SELECT lower(docb -> 'k') FROM {TABLE} LIMIT 10"),
        format!("SELECT COALESCE(docb -> 'k', docb -> 'a') FROM {TABLE} LIMIT 10"),
    ] {
        assert_eq!(code(&core, &alice, &sql), "0A000", "{sql}");
    }
    // ORDER BY の式キーはパス演算子を構文段で受理しない（fail-closed）。
    assert_eq!(
        code(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY docb -> 'k' LIMIT 10")
        ),
        "42601"
    );
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET docb = docb -> 'a' WHERE id = 1 USING OPERATION_ID 'upd-1'"
            ),
        )
        .expect_err("assignment of a jsonb expression is not supported yet");
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn text_result_can_feed_text_functions() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);
    assert_eq!(cell(&core, &alice, "upper(docb ->> 's')"), text("STR"));
    assert_eq!(cell(&core, &alice, "docb ->> 's' AS v"), text("str"));
}

#[test]
fn other_tenants_rows_never_appear_in_results_or_errors() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    insert(&core, &alice, 1, "k", r#"'{"k":"alice-secret"}'"#, 0);

    let sql =
        format!("SELECT docb ->> 'k' FROM {TABLE} WHERE docb ->> 'k' = 'alice-secret' LIMIT 10");
    let bob_rows = core.execute_sql(&bob, &sql).expect("bob query succeeds");
    assert!(bob_rows.rows.is_empty());
    let alice_rows = core
        .execute_sql(&alice, &sql)
        .expect("alice query succeeds");
    assert_eq!(alice_rows.rows.len(), 1);
    assert_eq!(alice_rows.rows[0].cells[0], text("alice-secret"));
}

#[test]
fn path_operator_in_check_body_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql(
            &alice,
            "CREATE TABLE chk (embedding VECTOR(2), docb JSONB, CHECK (docb ->> 'k' = 'v'))",
        )
        .map(|_| ())
        .expect_err("CHECK with a path operator must be rejected");
    // CHECK 本体の述語文法は式述語の字句を限定しており、パス演算子は `in_check_body` の
    // 0A000 ガードより手前（述語の区切り判定）で構文エラーになる。どちらも fail-closed。
    assert_eq!(err.wire_code(), "42601");
}

/// 検索 SELECT（`ORDER BY embedding <=> ...`）の投影と WHERE でも、jsonb 列が行スカラーとして
/// デコードされ値が返る（デコードマスク漏れなら黙って NULL になるため値を直接検証する）。
#[test]
fn ranked_search_projection_and_filter_decode_the_jsonb_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);
    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT docb -> 'a' -> 'b' -> 0, docb ->> 's' FROM {TABLE} \
                 WHERE docb ->> 'k' = '1' ORDER BY embedding <=> '[0.1,0.2]' LIMIT 5"
            ),
        )
        .unwrap_or_else(|e| panic!("ranked select should succeed: {}", e.wire_code()));
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells, vec![json("10"), text("str")]);
}

/// UPDATE の SET 右辺 (text 結果) と DELETE の WHERE の内側でパス演算子が評価され、
/// operation_id の再送判定がパス演算子の種別を区別する（content hash タグ 12）。
#[test]
fn dml_paths_evaluate_path_operators_and_resend_is_content_checked() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "UPDATE {TABLE} SET lang = docb ->> 's' WHERE id = 1 USING OPERATION_ID 'upd-path-1'"
        ),
    )
    .expect("update with a text-valued path expression succeeds");
    assert_eq!(cell(&core, &alice, "lang"), text("str"));

    // 集計経路 (COUNT) のデコードマスク。
    let count = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE docb ->> 'k' = '1'"),
        )
        .expect("aggregate with a path filter succeeds");
    assert_eq!(count.rows.len(), 1);
    assert_eq!(count.rows[0].cells[0], Cell::Integer(1));

    let delete = |expr: &str, op_id: &str| {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!("DELETE FROM {TABLE} WHERE {expr} = '1' USING OPERATION_ID '{op_id}'"),
        )
    };
    delete("docb ->> 'k'", "del-path-1").expect("delete with a path predicate succeeds");
    let gone = core
        .execute_sql(&alice, &format!("SELECT id FROM {TABLE} LIMIT 10"))
        .expect("select succeeds");
    assert!(gone.rows.is_empty());
    // 同一内容の再送は 23505、`->>` を `#>>` に替えた再送は内容不一致の 22023。
    assert_eq!(
        delete("docb ->> 'k'", "del-path-1")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    assert_eq!(
        delete("docb #>> '{k}'", "del-path-1")
            .unwrap_err()
            .wire_code(),
        "22023"
    );
}
