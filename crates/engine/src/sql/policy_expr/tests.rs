//! `sql::policy_expr` の単体テスト（RLS-13、Issue #2274）。束縛の分類・往復一致・長さ上限・
//! 真偽値でない式の `42804` を固定する。

use super::*;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("s", ColumnType::Text, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new("owner", ColumnType::Text, true),
        ],
    )
}

fn bind(sql: &str) -> Result<BoundPolicyExpr, SqlSurfaceError> {
    let preds = crate::sql::allowlist::parse_index_predicate_text(sql)?;
    bind_policy_expr(&preds, &schema())
}

fn code(sql: &str) -> &'static str {
    bind(sql).expect_err("must be rejected").wire_code()
}

#[test]
fn bound_expressions_are_normalized_and_list_referenced_columns() {
    let b = bind("owner = 'alice' AND n > 3").expect("bind");
    assert_eq!(b.referenced_columns, vec!["n", "owner"]);
    // 正規化テキストは再束縛しても同じ（往復一致）。
    let again = bind_stored_policy_expr(&b.sql, &schema()).expect("rebind");
    assert_eq!(again, b);
    let flag = bind("flag").expect("bind");
    assert_eq!(flag.referenced_columns, vec!["flag"]);
    assert!(bind("n > 1 OR s = 'x'").is_ok());
    assert!(bind("flag = true").is_ok());
    assert!(bind("n IS NOT NULL").is_ok());
}

#[test]
fn unknown_column_is_42703() {
    assert_eq!(code("nope = 'x'"), "42703");
    assert_eq!(code("nope > 1"), "42703");
}

#[test]
fn non_boolean_expressions_are_42804() {
    // 裸の非真偽値列だけが本ファイルの担当。リテラル・算術は構文段（allowlist::policy）が `42804` にする。
    for sql in ["n", "s", "NOT n"] {
        assert_eq!(code(sql), "42804", "{sql}");
    }
}

#[test]
fn reserved_columns_and_rls_calls_are_42601() {
    assert_eq!(code("tenant_id = 'a'"), "42601");
    assert_eq!(code("visibility = 'a'"), "42601");
    assert_eq!(code("visible()"), "42601");
}

#[test]
fn subqueries_are_rejected() {
    let c = code("n IN (SELECT n FROM t)");
    assert!(c == "0A000" || c == "42601", "{c}");
    let c = code("EXISTS (SELECT n FROM t)");
    assert!(c == "0A000" || c == "42601", "{c}");
}

#[test]
fn too_long_expression_is_54000() {
    let sql = vec!["n > 1"; 1200].join(" AND ");
    assert_eq!(code(&sql), "54000");
}

#[test]
fn merge_keeps_schema_order_without_duplicates() {
    let m = merge_referenced_columns(
        &schema(),
        &["owner".to_string(), "n".to_string()],
        &["n".to_string()],
    );
    assert_eq!(m, vec!["n", "owner"]);
}

/// 真偽定数は `TRUE`／`FALSE` として描画され、再束縛しても同じ木へ戻る（Issue #2274 の指摘対応）。
#[test]
fn boolean_constants_round_trip() {
    for sql in [
        "TRUE",
        "FALSE",
        "owner = 'a' AND FALSE",
        "owner = 'a' OR TRUE",
        "NOT (owner = 'a' OR TRUE)",
        "n > 1 AND (owner = 'a' OR FALSE)",
    ] {
        let b = bind(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let preds = crate::sql::allowlist::parse_index_predicate_text(sql).expect("parse");
        let again = crate::sql::allowlist::parse_index_predicate_text(&b.sql).expect("reparse");
        assert_eq!(preds, again, "{sql}");
        assert_eq!(
            bind_stored_policy_expr(&b.sql, &schema()).expect("rebind"),
            b
        );
    }
    assert_eq!(bind("TRUE").expect("bind").sql, "TRUE");
    assert_eq!(bind("FALSE").expect("bind").sql, "FALSE");
}
