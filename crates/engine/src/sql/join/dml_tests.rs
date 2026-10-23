//! `sql::join::dml` の単体テスト（SQL-53 (b)。Issue #1755）。データに依存しない形の検証と、
//! 結合キーの型クラス照合・source 列の補完を固定する。走査・一意化・上限・テナント境界を含む
//! 実行は `core::dml_join_exec_tests` が書き込みトランザクション付きで検証する。

use super::*;
use crate::catalog::{ColumnDef, ColumnType};

fn schema(name: &str, k_ty: ColumnType) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("k", k_ty, true),
            ColumnDef::new("v", ColumnType::BigInt, true),
        ],
    )
}

fn plan(keys: Vec<EquiKey>) -> JoinedDmlPlan {
    JoinedDmlPlan {
        target: JoinedDmlTarget {
            table: "t".to_string(),
            schema: schema("t", ColumnType::Text),
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            or_filters: Vec::new(),
        },
        source: JoinedDmlSource {
            table: "s".to_string(),
            schema: schema("s", ColumnType::Text),
            where_predicates: Vec::new(),
            columns: vec![2],
        },
        keys,
        update: None,
        returning_references_source: false,
    }
}

fn key(t: usize, s: usize) -> EquiKey {
    EquiKey {
        target_column: Some(t),
        source_column: Some(s),
    }
}

#[test]
fn validate_accepts_a_well_formed_plan() {
    plan(vec![key(1, 1)])
        .validate(&JoinLimits::default())
        .expect("valid");
}

#[test]
fn validate_rejects_missing_or_out_of_range_keys_fail_closed() {
    let limits = JoinLimits::default();
    let err = plan(Vec::new()).validate(&limits).expect_err("no keys");
    assert_eq!(err.wire_code(), "XX000");
    let err = plan(vec![key(9, 1)]).validate(&limits).expect_err("range");
    assert_eq!(err.wire_code(), "XX000");
    let err = plan(vec![key(1, 9)]).validate(&limits).expect_err("range");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn validate_applies_the_relation_limit_before_any_scan() {
    let limits = JoinLimits {
        max_relations: 1,
        ..JoinLimits::default()
    };
    let err = plan(vec![key(1, 1)])
        .validate(&limits)
        .expect_err("relations");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn key_classes_reject_incompatible_and_unsupported_types() {
    // text と bigint は結合できない（42804）。
    let err = plan(vec![key(1, 2)]).key_classes().expect_err("mismatch");
    assert_eq!(err.wire_code(), "42804");
    // 浮動小数は結合キーにできない（42601）。
    let mut p = plan(vec![key(1, 1)]);
    p.target.schema = schema("t", ColumnType::Double);
    p.source.schema = schema("s", ColumnType::Double);
    let err = p.key_classes().expect_err("unsupported");
    assert_eq!(err.wire_code(), "42601");
    // 疑似列 id と整数列は同じクラスで結合できる。
    let p = plan(vec![EquiKey {
        target_column: None,
        source_column: Some(2),
    }]);
    assert_eq!(p.key_classes().expect("integer class").len(), 1);
}

#[test]
fn source_columns_append_missing_key_columns_after_the_requested_ones() {
    let p = plan(vec![key(1, 1)]);
    // 要求列 [2] の後ろへ、未含の結合キー列 1 が補完される（結合ビューの添字は先頭から不変）。
    assert_eq!(p.source_columns(), vec![2, 1]);
    let p = plan(vec![key(1, 2)]);
    assert_eq!(p.source_columns(), vec![2]);
}
