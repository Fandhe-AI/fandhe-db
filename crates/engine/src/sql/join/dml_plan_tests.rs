//! `sql::join::dml_plan`（束縛済み `UPDATE ... FROM`／`DELETE ... USING` から `JoinedDmlPlan` への変換器）の
//! 単体テスト（SQL-53 (b)。Issue #3201）。DB を使わず、SQL 文字列を構文段・束縛段に通した結果を変換して、
//! 等値キーの抽出・述語の振り分け・SET の変換・fail-closed の拒否を固定する。実行を含む検証は
//! `core::dml_join_exec_tests` が担う。

use super::*;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::{
    validate_delete_using_tokens_with_limits, validate_update_from_tokens_with_limits,
    StructuralOnlyLookup,
};

fn docs() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("w", ColumnType::Double, true),
        ],
    )
}

fn langs() -> TableSchema {
    TableSchema::new(
        "langs",
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("label", ColumnType::Text, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new("w", ColumnType::Double, true),
            ColumnDef::new("v", ColumnType::Vector(2), true),
            ColumnDef::new("cnt", ColumnType::Integer, true),
        ],
    )
}

fn schema_of(name: &str) -> TableSchema {
    if name == "docs" {
        docs()
    } else {
        langs()
    }
}

fn upd(sql: &str) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    let toks = crate::sql::lexer::tokenize(sql).expect("tokenize");
    let stmt = validate_update_from_tokens_with_limits(
        &toks,
        LedgerMode::default(),
        false,
        crate::nesting_limits::NestingLimits::default(),
        &StructuralOnlyLookup,
    )?;
    let others: Vec<TableSchema> = stmt.from.iter().map(|r| schema_of(r.table())).collect();
    let refs: Vec<&TableSchema> = others.iter().collect();
    let target = schema_of(stmt.target.table());
    let bound = crate::sql::dml_from::bind_update_from(&stmt, &target, &refs)?;
    plan_update_from(&bound, &target, &refs, &UdfRegistry::default())
}

fn del(sql: &str) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    let toks = crate::sql::lexer::tokenize(sql).expect("tokenize");
    let stmt = validate_delete_using_tokens_with_limits(
        &toks,
        LedgerMode::default(),
        false,
        crate::nesting_limits::NestingLimits::default(),
        &StructuralOnlyLookup,
    )?;
    let others: Vec<TableSchema> = stmt.using.iter().map(|r| schema_of(r.table())).collect();
    let refs: Vec<&TableSchema> = others.iter().collect();
    let target = schema_of(stmt.target.table());
    let bound = crate::sql::dml_from::bind_delete_using(&stmt, &target, &refs)?;
    plan_delete_using(&bound, &target, &refs, &UdfRegistry::default())
}

fn key(t: Option<usize>, s: Option<usize>) -> EquiKey {
    EquiKey {
        target_column: t,
        source_column: s,
    }
}

#[test]
fn equality_key_is_extracted_and_oriented_target_first() {
    let a = upd("UPDATE docs SET n = langs.cnt FROM langs WHERE docs.lang = langs.lang").unwrap();
    let b = upd("UPDATE docs SET n = langs.cnt FROM langs WHERE langs.lang = docs.lang").unwrap();
    assert_eq!(a.keys, vec![key(Some(1), Some(0))]);
    assert_eq!(b.keys, a.keys);
    assert_eq!(a.target.table, "docs");
    assert_eq!(a.source.table, "langs");
    assert_eq!(a.source.columns, vec![5]);
    let u = a.update.as_ref().expect("update");
    assert_eq!(u.items.len(), 1);
    assert_eq!(u.items[0].column_index, 2);
}

#[test]
fn pseudo_id_and_composite_keys() {
    let p = del("DELETE FROM docs USING langs WHERE docs.id = langs.id").unwrap();
    assert_eq!(p.keys, vec![key(None, None)]);
    assert!(p.update.is_none());
    let c = del("DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND docs.n = langs.cnt")
        .unwrap();
    assert_eq!(c.keys, vec![key(Some(1), Some(0)), key(Some(2), Some(5))]);
}

#[test]
fn conjuncts_are_split_between_target_source_and_keys() {
    let p = del(
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND docs.n = '1' \
         AND langs.label = 'x' AND (langs.label = 'a' OR langs.cnt = '2')",
    )
    .unwrap();
    assert_eq!(p.keys.len(), 1);
    assert_eq!(target_filter_count(&p), 1);
    assert_eq!(p.source.where_predicates.len(), 2);
    assert!(matches!(
        p.source.where_predicates[0],
        WherePredicate::Equality { .. }
    ));
    assert!(matches!(
        p.source.where_predicates[1],
        WherePredicate::Or(_)
    ));
}

#[test]
fn target_only_or_goes_to_target_filters() {
    let p = del(
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND (docs.n = '1' OR docs.n = '2')",
    )
    .unwrap();
    assert!(p.source.where_predicates.is_empty());
    assert_eq!(p.target.or_filters.len(), 1);
}

#[test]
fn aliases_resolve_to_real_table_names() {
    let p = upd("UPDATE docs AS d SET n = l.cnt FROM langs l WHERE d.lang = l.lang").unwrap();
    assert_eq!(p.target.table, "docs");
    assert_eq!(p.source.table, "langs");
}

#[test]
fn self_join_is_split_by_relation_index_not_by_name() {
    let p = del(
        "DELETE FROM docs USING docs AS d2 WHERE docs.lang = d2.lang AND d2.n = '1' AND docs.n = '2'",
    )
    .unwrap();
    assert_eq!(p.source.table, "docs");
    assert_eq!(p.source.where_predicates.len(), 1);
    assert_eq!(target_filter_count(&p), 1);
}

#[test]
fn set_literal_expression_and_same_named_columns() {
    let p = upd("UPDATE docs SET n = 5 FROM langs WHERE docs.lang = langs.lang").unwrap();
    let u = p.update.as_ref().unwrap();
    assert_eq!(u.literal_assignments.len(), 1);
    assert!(u.items.is_empty());
    assert!(p.source.columns.is_empty());

    let p = upd("UPDATE docs SET n = n + 1 FROM langs WHERE docs.lang = langs.lang").unwrap();
    assert_eq!(p.update.as_ref().unwrap().items.len(), 1);

    // 両表に `lang` がある。source 側の `lang`（0）を取り出し、対象側の `lang`（1）へ代入する。
    let p = upd(
        "UPDATE docs SET lang = langs.lang, n = langs.cnt FROM langs WHERE docs.lang = langs.lang",
    )
    .unwrap();
    assert_eq!(p.source.columns, vec![0, 5]);
    let u = p.update.as_ref().unwrap();
    assert_eq!(u.items.len(), 2);
    assert_eq!(u.items[0].column_index, 1);
    assert_eq!(u.items[1].column_index, 2);

    // 右辺が対象表の列でも変換できる。
    let p =
        upd("UPDATE docs SET lang = docs.lang FROM langs WHERE docs.lang = langs.lang").unwrap();
    assert!(p.source.columns.is_empty());
    assert_eq!(p.update.as_ref().unwrap().items.len(), 1);
}

fn assert_rejected(sql: &str, code: &str) {
    let r = if sql.starts_with("UPDATE") {
        upd(sql)
    } else {
        del(sql)
    };
    match r {
        Ok(_) => panic!("must be rejected: {sql}"),
        Err(e) => assert_eq!(e.wire_code(), code, "{sql}: {e:?}"),
    }
}

// intentional: docs/design/update-from-delete-using.md
#[test]
fn unsupported_shapes_are_rejected_with_feature_not_supported() {
    for sql in [
        // WHERE 省略・等値キー無し
        "DELETE FROM docs USING langs",
        "DELETE FROM docs USING langs WHERE langs.flag",
        "DELETE FROM docs USING langs WHERE docs.n = '1' AND langs.cnt = '2'",
        // relation をまたぐ非等値・OR
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND docs.n < langs.cnt",
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND (docs.n = '1' OR langs.cnt = '2')",
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang OR docs.n = '1'",
        // 同じ relation 内の列比較
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND docs.lang = docs.lang",
        // FROM／USING が 2 表以上
        "DELETE FROM docs USING langs, langs AS l2 WHERE docs.lang = langs.lang",
        // キー化できない型
        "DELETE FROM docs USING langs WHERE docs.w = langs.w",
        // source の疑似列 id・写像できない型を SET の右辺に使う
        "UPDATE docs SET n = langs.id FROM langs WHERE docs.lang = langs.lang",
        "UPDATE docs SET embedding = langs.v FROM langs WHERE docs.lang = langs.lang",
    ] {
        assert_rejected(sql, "0A000");
    }
}

#[test]
fn key_type_class_mismatch_is_datatype_mismatch() {
    assert_rejected(
        "DELETE FROM docs USING langs WHERE docs.lang = langs.cnt",
        "42804",
    );
}

/// 対象側へ振り分けられた述語の総数（述語の型によって束縛先が異なるため合算で固定する）。
fn target_filter_count(p: &JoinedDmlPlan) -> usize {
    p.target.metadata_filters.len() + p.target.expr_filters.len() + p.target.or_filters.len()
}

/// 式経路（`bind_set_expr`）が受理しない列型への source 列の単純コピーは、型を制限せず
/// 単純コピー項（`copies`）として変換される（SET が式を経由しないため 0A000 にならない）。
#[test]
fn plain_set_copy_of_non_expression_types_is_planned_as_copy() {
    let types = [
        ColumnType::Json,
        ColumnType::Jsonb,
        ColumnType::Uuid,
        ColumnType::Bytea,
        ColumnType::Numeric {
            precision: 10,
            scale: 2,
        },
    ];
    for ty in types {
        let target = TableSchema::new(
            "tt",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("c", ty.clone(), true),
            ],
        );
        let source = TableSchema::new(
            "ss",
            vec![
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("c", ty.clone(), true),
            ],
        );
        let toks = crate::sql::lexer::tokenize("UPDATE tt SET c = ss.c FROM ss WHERE tt.k = ss.k")
            .expect("tokenize");
        let stmt = validate_update_from_tokens_with_limits(
            &toks,
            LedgerMode::default(),
            false,
            crate::nesting_limits::NestingLimits::default(),
            &StructuralOnlyLookup,
        )
        .unwrap();
        let refs = [&source];
        let bound = crate::sql::dml_from::bind_update_from(&stmt, &target, &refs).unwrap();
        let p = plan_update_from(&bound, &target, &refs, &UdfRegistry::default())
            .unwrap_or_else(|e| panic!("{ty:?}: {e:?}"));
        let u = p.update.as_ref().unwrap();
        assert!(u.items.is_empty(), "{ty:?}");
        assert_eq!(u.copies.len(), 1, "{ty:?}");
        assert_eq!(u.copies[0].column_index, 2);
        assert_eq!(u.copies[0].target_ty, ty);
        assert_eq!(p.source.columns, vec![1]);
    }
}
