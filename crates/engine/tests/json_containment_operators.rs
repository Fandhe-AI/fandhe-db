//! jsonb の包含・存在演算子 `@>` `<@` `?` `?|` `?&`（対象ビヘイビア: TABLE-25。Issue #1598）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25（関連: TABLE-14・
//! SQL-24・SQL-42）。
//!
//! `tests/json_path_operators.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql` を production 経路として検証）。固定する契約: 述語は bool を返し
//! WHERE の裸の述語として受理される、NULL の伝播、PostgreSQL の包含・存在規則、
//! 型不一致・`json` 列・不正リテラル・配列式の拒否コード、`NOT <裸の述語>` と CHECK 本体の拒否、
//! テナント境界、DML の再送時の内容検査。

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

const TABLE: &str = "containdocs";

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
    let path = unique_db_path("json-containment-operators");
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
                "INSERT INTO {TABLE} (id, embedding, lang, doc, docb, n) \
                 VALUES ({id}, '[0.1,0.2]', 'k', '{{}}', {docb}, 0) \
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

/// `WHERE <pred>` に一致する id（昇順の前提で比較できるよう `ORDER BY` は付けず整列して返す）。
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

#[test]
fn contains_and_contained_by_follow_postgres_rules_in_where() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"product":"PostgreSQL","version":9.4,"tags":["a","b"]}"#),
            Some(r#"["foo","bar"]"#),
            Some(r#"{"foo":{"bar":"baz"}}"#),
            Some(r#"[1,2,[1,3]]"#),
            None,
        ],
    );
    assert_eq!(
        matching(&core, &alice, r#"docb @> '{"version":9.4}'"#),
        vec![1]
    );
    assert_eq!(
        matching(&core, &alice, r#"docb @> '{"version":9.40}'"#),
        vec![1]
    );
    assert_eq!(
        matching(&core, &alice, r#"docb @> '{"tags":["b"]}'"#),
        vec![1]
    );
    // 最上位の例外（配列 ⊇ スカラー）。
    assert_eq!(matching(&core, &alice, r#"docb @> '"bar"'"#), vec![2]);
    // 入れ子のキーは最上位では見えない。
    assert_eq!(
        matching(&core, &alice, r#"docb @> '{"bar":"baz"}'"#),
        Vec::<u64>::new()
    );
    assert_eq!(matching(&core, &alice, r#"docb @> '{"foo":{}}'"#), vec![3]);
    assert_eq!(
        matching(&core, &alice, "docb @> '[1,3]'"),
        Vec::<u64>::new()
    );
    assert_eq!(matching(&core, &alice, "docb @> '[[1,3]]'"), vec![4]);
    // `<@` は左右を入れ替えた `@>`。リテラルを左に置いても同じ。
    assert_eq!(
        matching(&core, &alice, r#"'{"foo":{"bar":"baz"}}' @> docb"#),
        vec![3]
    );
    assert_eq!(
        matching(&core, &alice, r#"docb <@ '{"foo":{"bar":"baz"},"x":1}'"#),
        vec![3]
    );
    assert_eq!(matching(&core, &alice, r#"'["bar"]' <@ docb"#), vec![2]);
    // NULL 列は非該当（`@>` でも `<@` でも）。
    assert_eq!(matching(&core, &alice, "docb @> '{}'"), vec![1, 3]);
    assert_eq!(matching(&core, &alice, "docb <@ '{}'"), Vec::<u64>::new());
}

#[test]
fn existence_operators_follow_postgres_rules() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"a":1,"b":2}"#),
            Some(r#"["a","c"]"#),
            Some(r#""a""#),
            Some(r#"{"x":{"a":1}}"#),
            Some(r#"[1]"#),
            None,
        ],
    );
    assert_eq!(matching(&core, &alice, "docb ? 'a'"), vec![1, 2, 3]);
    assert_eq!(matching(&core, &alice, "docb ? '1'"), Vec::<u64>::new());
    assert_eq!(matching(&core, &alice, "docb ?| '{b,c}'"), vec![1, 2]);
    assert_eq!(matching(&core, &alice, "docb ?| '{c,NULL}'"), vec![2]);
    assert_eq!(matching(&core, &alice, "docb ?| '{}'"), Vec::<u64>::new());
    assert_eq!(matching(&core, &alice, "docb ?& '{a,b}'"), vec![1]);
    assert_eq!(matching(&core, &alice, "docb ?& '{a,c}'"), vec![2]);
    // NULL 要素は読み飛ばす。空配列は NULL でない行すべてに該当する。
    assert_eq!(matching(&core, &alice, "docb ?& '{a,NULL}'"), vec![1, 2, 3]);
    assert_eq!(matching(&core, &alice, "docb ?& '{}'"), vec![1, 2, 3, 4, 5]);
    // 右辺は text の式でもよい。
    assert_eq!(matching(&core, &alice, "docb ? lang"), Vec::<u64>::new());
}

#[test]
fn predicates_compose_with_and_or_parentheses_and_path_operators() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"tags":["x","y"],"k":1}"#),
            Some(r#"{"tags":["z"],"k":2}"#),
            Some(r#"{"other":true,"k":1}"#),
        ],
    );
    assert_eq!(
        matching(&core, &alice, "docb ? 'tags' AND docb @> '{\"k\":1}'"),
        vec![1]
    );
    assert_eq!(
        matching(&core, &alice, "docb ? 'other' OR docb @> '{\"k\":2}'"),
        vec![2, 3]
    );
    assert_eq!(
        matching(&core, &alice, "(docb ? 'tags') AND n = 0"),
        vec![1, 2]
    );
    assert_eq!(matching(&core, &alice, "(docb -> 'tags') ? 'z'"), vec![2]);
    // `->` は `?` より先に結合する（同じ優先順位の左結合）。
    assert_eq!(matching(&core, &alice, "docb -> 'tags' ? 'x'"), vec![1]);
    assert_eq!(
        matching(&core, &alice, "docb -> 'tags' @> '[\"y\"]'"),
        vec![1]
    );
    // 述語を比較の左辺に置ける（bool 同士の比較は束縛段が拒否する形ではないことの確認ではなく、
    // 比較を伴う従来の式述語が壊れていないこと）。
    assert_eq!(matching(&core, &alice, "docb -> 'k' = '1'"), vec![1, 3]);
}

#[test]
fn projection_returns_bool_and_null_propagates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#), None]);
    assert_eq!(
        cell(&core, &alice, "docb @> '{\"a\":1}'", 1),
        Cell::Bool(true)
    );
    assert_eq!(
        cell(&core, &alice, "docb @> '{\"a\":2}'", 1),
        Cell::Bool(false)
    );
    assert_eq!(cell(&core, &alice, "docb ? 'a'", 1), Cell::Bool(true));
    assert_eq!(cell(&core, &alice, "docb ?| '{z}'", 1), Cell::Bool(false));
    assert_eq!(cell(&core, &alice, "docb ?& '{a}'", 1), Cell::Bool(true));
    assert_eq!(
        cell(&core, &alice, "docb <@ '{\"a\":1,\"b\":2}'", 1),
        Cell::Bool(true)
    );
    // NULL 列は NULL。
    assert_eq!(cell(&core, &alice, "docb @> '{}'", 2), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb ? 'a'", 2), Cell::Null);
    assert_eq!(cell(&core, &alice, "docb ?| '{a}'", 2), Cell::Null);
}

#[test]
fn right_operand_null_yields_null() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // `lang` が NULL の行（キー式が NULL）では `?` の結果も NULL で、WHERE では非該当。
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, docb) VALUES (1, '[0.1,0.2]', '{{\"a\":1}}') \
             USING OPERATION_ID 'seed-null-key'"
        ),
    )
    .expect("insert");
    assert_eq!(cell(&core, &alice, "docb ? lang", 1), Cell::Null);
    assert_eq!(matching(&core, &alice, "docb ? lang"), Vec::<u64>::new());
}

#[test]
fn operand_errors_use_fixed_sqlstates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#)]);
    let sel = |pred: &str| format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 5");
    // `json` 列には包含・存在演算子が無い（パス演算子の 0A000 とは異なる）。
    assert_eq!(code(&core, &alice, &sel("doc @> '{}'")), "42883");
    assert_eq!(code(&core, &alice, &sel("doc ? 'a'")), "42883");
    assert_eq!(code(&core, &alice, &sel("docb @> doc")), "42883");
    // 左辺が jsonb でない。
    assert_eq!(code(&core, &alice, &sel("lang ? 'a'")), "42883");
    assert_eq!(code(&core, &alice, &sel("lang @> '{}'")), "42883");
    // `?` の右辺は text。`?|` の右辺は text[] リテラルのみ。
    assert_eq!(code(&core, &alice, &sel("docb ? 1")), "42883");
    assert_eq!(code(&core, &alice, &sel("docb ?| lang")), "0A000");
    assert_eq!(code(&core, &alice, &sel("docb ?& n")), "0A000");
    // 不正な jsonb リテラルは 22P02（左右どちらでも）。本文は含めない。
    let err = core
        .execute_sql(&alice, &sel("docb @> '{alice-secret'"))
        .expect_err("invalid literal");
    assert_eq!(err.wire_code(), "22P02");
    assert!(!err.to_string().contains("alice-secret"));
    assert_eq!(code(&core, &alice, &sel("'{broken' <@ docb")), "22P02");
    // 不正な text[] リテラル。
    assert_eq!(code(&core, &alice, &sel("docb ?| '{a'")), "22P02");
    // 文字列リテラル同士は解決しない。
    assert_eq!(code(&core, &alice, &sel("'{}' @> '{}'")), "42883");
}

#[test]
fn bare_predicate_restrictions_fail_closed() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#)]);
    let sel = |pred: &str| format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 5");
    // 三値論理を守った否定は未対応のため 42601（fail-closed）。
    assert_eq!(code(&core, &alice, &sel("NOT docb ? 'a'")), "42601");
    assert_eq!(code(&core, &alice, &sel("NOT (docb ? 'a')")), "42601");
    // 比較を伴わない非述語の式は従来どおり 42601。
    assert_eq!(code(&core, &alice, &sel("docb -> 'a'")), "42601");
    // 字句: 単独の `@` は未対応のまま（`@?`・`@@` は Issue #2186 で対応済み）。
    assert_eq!(code(&core, &alice, &sel("docb @ '{}'")), "42601");
    // CHECK 本体では受理しない。
    let err = core
        .execute_sql(
            &alice,
            "CREATE TABLE chk (embedding VECTOR(2), docb JSONB, CHECK (docb ? 'k'))",
        )
        .map(|_| ())
        .expect_err("CHECK with a jsonb predicate must be rejected");
    assert!(matches!(err.wire_code(), "42601" | "0A000"));
}

#[test]
fn other_tenants_rows_never_appear_in_results_or_errors() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    seed(&core, &alice, &[Some(r#"{"alice-secret":1}"#)]);
    assert_eq!(
        matching(&core, &bob, "docb ? 'alice-secret'"),
        Vec::<u64>::new()
    );
    assert_eq!(matching(&core, &bob, "docb @> '{}'"), Vec::<u64>::new());
    assert_eq!(matching(&core, &alice, "docb ? 'alice-secret'"), vec![1]);
}

#[test]
fn ranked_search_filter_decodes_the_jsonb_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#), Some(r#"{"b":1}"#)]);
    assert_eq!(
        ids(
            &core,
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE docb ? 'a' \
                 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 5"
            )
        ),
        vec![1]
    );
    let count = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE docb @> '{{\"b\":1}}'"),
        )
        .expect("aggregate with a containment filter succeeds");
    assert_eq!(count.rows[0].cells[0], Cell::Integer(1));
}

#[test]
fn dml_where_with_predicates_acts_on_matching_rows_and_resend_is_content_checked() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"k":1}"#), Some(r#"{"j":1}"#)]);
    let run = |sql: &str| core.execute_sql_in_session(&alice, &mut SessionState::default(), sql);
    let upd = |pred: &str| {
        format!("UPDATE {TABLE} SET n = 5 WHERE {pred} USING OPERATION_ID 'upd-contain'")
    };
    run(&upd("docb @> '{\"k\":1}'")).expect("first update succeeds");
    assert_eq!(matching(&core, &alice, "n = 5"), vec![1]);
    // 同一内容の再送は 23505、演算子だけを変えた再送は内容不一致の 22023。
    assert_eq!(
        run(&upd("docb @> '{\"k\":1}'"))
            .expect_err("resend")
            .wire_code(),
        "23505"
    );
    assert_eq!(
        run(&upd("docb <@ '{\"k\":1}'"))
            .expect_err("different op")
            .wire_code(),
        "22023"
    );
    assert_eq!(
        run(&upd("docb ? 'k'"))
            .expect_err("different op")
            .wire_code(),
        "22023"
    );
    run(&format!(
        "DELETE FROM {TABLE} WHERE docb ? 'j' USING OPERATION_ID 'del-exists'"
    ))
    .expect("delete with an existence predicate succeeds");
    assert_eq!(matching(&core, &alice, "n = 0"), Vec::<u64>::new());
    assert_eq!(matching(&core, &alice, "docb ? 'k'"), vec![1]);
}
