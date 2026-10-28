//! `sql::relation`（複数テーブル参照スコープの束縛基盤。SQL-28・RLS-10、
//! TASK-212、Issue #924）が engine クレート外から到達可能な公開 API であることを
//! 固定する結合テスト。`sql::relation` 自体の詳細な分岐網羅は同モジュール内の
//! 単体テストが担うため、本ファイルは公開 API 経路の疎通と、単一参照スコープの
//! 非修飾列解決が既存の単一テーブル束縛と同じ添字になる等価性のみを確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::error_format::ClassifiedError;
use engine::sql::relation::{BindingScope, ColumnRef, ColumnSlot, TableRef, MAX_TABLE_REFS};

fn schema(name: &str, columns: Vec<ColumnDef>) -> TableSchema {
    TableSchema::new(name, columns)
}

#[test]
fn multi_table_scope_resolves_qualified_and_unqualified_columns() {
    let docs = schema(
        "docs",
        vec![ColumnDef::new("title", ColumnType::Text, false)],
    );
    let authors = schema(
        "authors",
        vec![ColumnDef::new("name", ColumnType::Text, false)],
    );
    let scope = BindingScope::new(vec![
        (TableRef::with_alias("docs", "d"), &docs),
        (TableRef::with_alias("authors", "a"), &authors),
    ])
    .expect("scope construction succeeds within MAX_TABLE_REFS");

    let title = scope
        .resolve(&ColumnRef::qualified("d", "title"))
        .expect("qualified resolution succeeds");
    assert_eq!(title.relation(), 0);
    assert_eq!(title.slot(), ColumnSlot::Column(0));

    let name = scope
        .resolve(&ColumnRef::unqualified("name"))
        .expect("unambiguous unqualified resolution succeeds");
    assert_eq!(name.relation(), 1);
}

#[test]
fn ambiguous_unqualified_column_is_42702() {
    let a = schema("a", vec![ColumnDef::new("tag", ColumnType::Text, false)]);
    let b = schema("b", vec![ColumnDef::new("tag", ColumnType::Text, false)]);
    let scope =
        BindingScope::new(vec![(TableRef::new("a"), &a), (TableRef::new("b"), &b)]).expect("scope");

    let err = scope
        .resolve(&ColumnRef::unqualified("tag"))
        .expect_err("ambiguous column must be rejected");
    assert_eq!(err.error_class().wire_code(), "42702");
}

#[test]
fn too_many_table_refs_is_rejected_before_allocation() {
    let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
    let relations: Vec<(TableRef, &TableSchema)> = (0..(MAX_TABLE_REFS + 1))
        .map(|i| (TableRef::with_alias("a", format!("t{i}")), &a))
        .collect();
    let err = BindingScope::new(relations).expect_err("must reject over-limit reference count");
    assert_eq!(err.error_class().wire_code(), "54000");
}

/// 単一参照スコープでの非修飾列解決は、既存の単一テーブル束縛
/// （`sql::parser` の列解決。実カラムを疑似列 `id` より優先）と同じ添字を返す
/// （SQL-28 の受け入れ条件 4「単一テーブルクエリの結果を変えない」の一部）。
#[test]
fn single_relation_scope_matches_single_table_binding_column_priority() {
    let docs = schema(
        "docs",
        vec![
            ColumnDef::new("id", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    let scope = BindingScope::new(vec![(TableRef::new("docs"), &docs)]).expect("scope");

    // 実カラム "id"（添字 0）を疑似列 `id` より優先する
    // （`sql::parser::bind_projection` と同じ規則）。
    let resolved_id = scope
        .resolve(&ColumnRef::unqualified("id"))
        .expect("resolves to real column");
    assert_eq!(resolved_id.slot(), ColumnSlot::Column(0));

    let resolved_body = scope
        .resolve(&ColumnRef::unqualified("body"))
        .expect("resolves body column");
    assert_eq!(resolved_body.slot(), ColumnSlot::Column(1));
}
