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
    // `json` 列のパス演算子は原文保存の意味で受理する（Issue #2180。
    // 結合テストは `tests/json_text_operators.rs`）。
    // 右辺の型: 小数・bigint の範囲・BIGINT 列は演算子が存在しない。
    assert_eq!(code(&core, &alice, &sel("docb -> 1.5")), "42883");
    assert_eq!(code(&core, &alice, &sel("docb -> 3000000000")), "42883");
    assert_eq!(code(&core, &alice, &sel("docb -> big")), "42883");
    // `#>` の右辺は text[] リテラルのみ。形式不正は 22P02。
    assert_eq!(code(&core, &alice, &sel("docb #> 'a'")), "22P02");
    assert_eq!(code(&core, &alice, &sel("docb #> lang")), "0A000");
    // jsonb 列の裸の参照は式の中で受理されるが、関数引数としては消費できない（Issue #1597。
    // これまでの 22000 から 0A000 へ変わる）。
    assert_eq!(code(&core, &alice, &sel("vec_norm(docb)")), "0A000");
}

#[test]
fn jsonb_values_are_rejected_where_they_cannot_be_consumed_yet() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);

    // 集計・関数引数・COALESCE・算術は引き続き 0A000（比較と ORDER BY は Issue #1597 で対応）。
    for sql in [
        format!("SELECT id FROM {TABLE} WHERE (docb -> 'k') + 1 = 2 LIMIT 10"),
        format!("SELECT COUNT(docb -> 'k') FROM {TABLE}"),
        format!("SELECT lower(docb -> 'k') FROM {TABLE} LIMIT 10"),
        format!("SELECT COALESCE(docb -> 'k', docb -> 'a') FROM {TABLE} LIMIT 10"),
    ] {
        assert_eq!(code(&core, &alice, &sql), "0A000", "{sql}");
    }
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

/// 添字の整数演算は WHERE／UPDATE／DELETE でも投影と同じ型付き演算（int4 中間あふれは 22003）。
#[test]
fn subscript_integer_arithmetic_matches_between_projection_and_filters() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", r#"'["a","x"]'"#, 1);
    let expr = "docb ->> (n + 2147483647 - 2147483647)";

    assert_eq!(
        code(
            &core,
            &alice,
            &format!("SELECT {expr} FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        "22003"
    );
    assert_eq!(
        code(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE {expr} = 'x' LIMIT 1")
        ),
        "22003"
    );
    for (sql, op) in [
        (
            format!(
                "UPDATE {TABLE} SET lang = 'z' WHERE {expr} = 'x' USING OPERATION_ID 'sub-upd'"
            ),
            "upd",
        ),
        (
            format!("DELETE FROM {TABLE} WHERE {expr} = 'x' USING OPERATION_ID 'sub-del'"),
            "del",
        ),
    ] {
        let err = core
            .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
            .expect_err(op);
        assert_eq!(err.wire_code(), "22003", "{op}");
    }
    // 範囲内の整数除算は WHERE でも切り捨てで投影と一致する（n=1 -> 1/2=0 -> "a"）。
    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE docb ->> (n / 2) = 'a' LIMIT 1"),
        )
        .expect("integer division in a filter subscript");
    assert_eq!(r.rows.len(), 1);
}

// ---- Issue #1597: jsonb の比較・述語・ORDER BY ----

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

fn seed(core: &EngineCore, ctx: &PolicyContext, docs: &[&str]) {
    for (i, d) in docs.iter().enumerate() {
        insert(core, ctx, (i + 1) as u64, "k", &format!("'{d}'"), 0);
    }
}

#[test]
fn where_equality_against_a_jsonb_literal_uses_value_equality() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            r#"{"k":1}"#,
            r#"{"k":1.0}"#,
            r#"{"k":10e-1}"#,
            r#"{"k":"1"}"#,
            r#"{"k":null}"#,
            r#"{"x":1}"#,
            r#"{"k":[1]}"#,
        ],
    );
    let q = |w: &str| {
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE {w} LIMIT 100"),
        )
    };
    // 受け入れ基準: 数値の値としての等価（1 = 1.0 = 10e-1）、文字列 "1" は別。
    let mut got = q("docb -> 'k' = '1'");
    got.sort();
    assert_eq!(got, vec![1, 2, 3]);
    assert_eq!(q("'1' = docb -> 'k'").len(), 3);
    // キー不在（SQL の NULL）は除外され、JSON の null は 'null' に一致する。
    assert_eq!(q("docb -> 'k' = 'null'"), vec![5]);
    assert_eq!(q(r#"docb -> 'k' = '"1"'"#), vec![4]);
    assert_eq!(q("docb -> 'k' = '[1.0]'"), vec![7]);
    // jsonb 同士の比較と NOT。
    assert_eq!(q("docb -> 'k' = docb -> 'k'").len(), 6);
    let mut not_one = q("NOT (docb -> 'k' = '1')");
    not_one.sort();
    assert_eq!(not_one, vec![4, 5, 7]);
}

#[test]
fn where_ordering_follows_the_jsonb_btree_order() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            r#"{"k":null}"#,
            r#"{"k":"a"}"#,
            r#"{"k":2}"#,
            r#"{"k":true}"#,
            r#"{"k":[1]}"#,
            r#"{"k":{"a":1}}"#,
        ],
    );
    let q = |w: &str| {
        let mut v = ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE {w} LIMIT 100"),
        );
        v.sort();
        v
    };
    // 型順位: Object > Array > Boolean > Number > String > Null。
    assert_eq!(q("docb -> 'k' > '2'"), vec![4, 5, 6]);
    assert_eq!(q("docb -> 'k' >= '2'"), vec![3, 4, 5, 6]);
    assert_eq!(q("docb -> 'k' < '2'"), vec![1, 2]);
    assert_eq!(q("docb -> 'k' <= '2'"), vec![1, 2, 3]);
    assert_eq!(q("docb -> 'k' < '{}'"), vec![1, 2, 3, 4, 5]);
    // 数値の厳密比較（f64 に収まらない差）。
    assert_eq!(q("docb -> 'k' < '2.0000000000000000000001'"), vec![1, 2, 3]);
}

#[test]
fn comparison_operand_errors_use_fixed_sqlstates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[r#"{"k":1}"#]);
    let w = |p: &str| format!("SELECT id FROM {TABLE} WHERE {p} LIMIT 10");
    assert_eq!(code(&core, &alice, &w("docb -> 'k' = lang")), "42883");
    assert_eq!(code(&core, &alice, &w("docb -> 'k' = 1")), "42883");
    assert_eq!(code(&core, &alice, &w("(docb -> 'k') + 1 = 2")), "0A000");
    // 不正な JSON リテラル（長さ超過の 54000 は json::jsonb_sort_key の単体テストで固定）。
    assert_eq!(code(&core, &alice, &w("docb -> 'k' = 'x'")), "22P02");
    assert_eq!(code(&core, &alice, &w("docb -> 'k' = '{'")), "22P02");
}

#[test]
fn order_by_jsonb_keys_expression_column_and_alias() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // id 1..=7: 型順位が混在する値と、キー不在の行（SQL の NULL）。
    seed(
        &core,
        &alice,
        &[
            r#"{"k":{"a":1}}"#,
            r#"{"k":"b"}"#,
            r#"{"k":null}"#,
            r#"{"x":0}"#,
            r#"{"k":[1,2]}"#,
            r#"{"k":10}"#,
            r#"{"k":true}"#,
        ],
    );
    let asc = vec![3, 2, 6, 7, 5, 1, 4];
    let mut desc: Vec<u64> = asc.clone();
    desc.reverse();
    // 昇順は NULL が末尾（4）、降順は先頭。
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY docb -> 'k' LIMIT 100")
        ),
        asc
    );
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY docb -> 'k' DESC LIMIT 100")
        ),
        vec![4, 1, 5, 7, 6, 2, 3]
    );
    // LIMIT 付き（ヒープ経路）と全件の先頭が一致する。
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY docb -> 'k' LIMIT 3")
        ),
        vec![3, 2, 6]
    );
    // jsonb の式項目を指す別名。
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id, docb -> 'k' AS v FROM {TABLE} ORDER BY v LIMIT 100")
        ),
        asc
    );
    // text 結果のパス演算子（バイト順）。
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY docb ->> 'k', id LIMIT 100")
        )
        .len(),
        7
    );
    // jsonb 列の裸のキー（オブジェクトはペア数 → 内容の順）。
    let col_order = ids(
        &core,
        &alice,
        &format!("SELECT id FROM {TABLE} ORDER BY docb, id LIMIT 100"),
    );
    assert_eq!(col_order.len(), 7);
    // 2 キー目への同点解消。
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY docb -> 'k' DESC, id DESC LIMIT 100")
        ),
        vec![4, 1, 5, 7, 6, 2, 3]
    );
}

#[test]
fn jsonb_expression_projection_returns_normalized_text() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert(&core, &alice, 1, "k", DOC, 0);
    assert_eq!(cell(&core, &alice, "docb -> 'k' AS x"), json("1"));
    assert_eq!(
        cell(&core, &alice, "docb AS x"),
        json(r#"{"a":{"b":[10,20,{"c":"x\ny"}]},"k":1,"n":null,"s":"str"}"#)
    );
}

#[test]
fn jsonb_order_and_predicates_respect_tenant_boundary() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    insert(&core, &alice, 1, "k", r#"'{"k":"alice-secret"}'"#, 0);
    let w = format!("SELECT id FROM {TABLE} WHERE docb -> 'k' = '\"alice-secret\"' LIMIT 10");
    assert!(ids(&core, &bob, &w).is_empty());
    assert_eq!(ids(&core, &alice, &w), vec![1]);
    let o = format!("SELECT id FROM {TABLE} ORDER BY docb -> 'k' LIMIT 10");
    assert!(ids(&core, &bob, &o).is_empty());
    // エラー文言にも他テナントの値は現れない。
    let err = core
        .execute_sql(
            &bob,
            &format!("SELECT id FROM {TABLE} WHERE docb -> 'k' = 'alice-secret' LIMIT 10"),
        )
        .expect_err("invalid jsonb literal");
    assert_eq!(err.wire_code(), "22P02");
    assert!(!err.to_string().contains("alice-secret"));
}

#[test]
fn dml_where_with_jsonb_comparison_is_content_checked_on_resend() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[r#"{"k":1}"#, r#"{"k":2}"#]);
    let run = |sql: &str| core.execute_sql_in_session(&alice, &mut SessionState::default(), sql);
    let upd = |lit: &str| {
        format!("UPDATE {TABLE} SET n = 5 WHERE docb -> 'k' = '{lit}' USING OPERATION_ID 'upd-j'")
    };
    run(&upd("1")).expect("first update succeeds");
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE n = 5 LIMIT 10")
        ),
        vec![1]
    );
    // 同じ内容の再送は 23505、jsonb リテラルだけが異なる再送は 22023。
    assert_eq!(run(&upd("1")).expect_err("resend").wire_code(), "23505");
    assert_eq!(
        run(&upd("2")).expect_err("different content").wire_code(),
        "22023"
    );
}
