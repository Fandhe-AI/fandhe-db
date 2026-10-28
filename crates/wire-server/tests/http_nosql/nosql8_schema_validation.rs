//! 層 A 結合テスト（TASK-175・NOSQL-8。Issue #760）: 公開 API のみを使い
//! `engine::json::parse_json` → `wire_server::http::query::schema::extract_op`
//! → `schema_for` → `ObjectSchema::validate` の通しを固定する。
//!
//! 単体テスト（`schema.rs` 内 `mod tests`）は crate 内部の `SEARCH_SCHEMA` 等
//! 定数へ直接アクセスするが、本ファイルは wire-server クレート外から見える
//! 公開関数・型のみを経由する（実際の呼び出し元＝後続 Issue #759 の
//! HTTP ハンドラが使う形を再現する）。

use engine::json::parse_json;
use wire_server::http::query::schema::{extract_op, schema_for, SchemaError};

/// 受理 6 件: `search`／`scan`／`aggregate`／`insert`／`update`／`delete`
/// それぞれの最小構成が構文解析・意味検証を通しで通過する（`update`／
/// `delete` は Issue #875・NOSQL-12 で語彙へ加わったが、束縛・実行結線は
/// 未実装のため、ここでは「語彙・スキーマ検証を通過する」ことのみを固定
/// する）。
#[test]
fn parse_extract_schema_validate_accepts_each_op_minimal_form() {
    let cases = [
        (r#"{"op":"search","table":"docs","limit":10}"#, "search"),
        (r#"{"op":"scan","table":"docs","limit":500}"#, "scan"),
        (
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
            "aggregate",
        ),
        (r#"{"op":"insert","table":"docs","rows":[]}"#, "insert"),
        (
            r#"{"op":"update","table":"docs","set":{"lang":"en"}}"#,
            "update",
        ),
        (r#"{"op":"delete","table":"docs"}"#, "delete"),
    ];
    for (text, expected_op) in cases {
        let value = parse_json(text).expect("valid JSON fixture");
        let op = extract_op(&value).expect("op must be present");
        assert_eq!(op, expected_op);
        let schema = schema_for(op).unwrap_or_else(|| panic!("schema_for must resolve {op}"));
        let result = schema.validate(&value);
        assert!(result.is_ok(), "expected accept for {text}, got {result:?}");
    }
}

/// 拒否 3 類型: 必須欠落・未知キー・型不一致がいずれも通しの経路で
/// `42601`（`UnsupportedSqlSyntax`）へ収束する。
#[test]
fn parse_extract_schema_validate_rejects_three_violation_types_as_unsupported_sql_syntax() {
    let cases = [
        (
            r#"{"op":"search","table":"docs"}"#,
            "missing required (limit)",
        ),
        (
            r#"{"op":"search","table":"docs","limit":1,"unexpected":true}"#,
            "unknown key",
        ),
        (
            r#"{"op":"search","table":"docs","limit":"ten"}"#,
            "type mismatch (limit)",
        ),
    ];
    for (text, label) in cases {
        let value = parse_json(text).expect("valid JSON fixture");
        let op = extract_op(&value).expect("op must be present");
        let schema = schema_for(op).expect("schema_for must resolve search");
        let err = schema
            .validate(&value)
            .expect_err(&format!("expected rejection for {label}"));
        assert_eq!(err.wire_code(), "42601", "case={label}");
    }
}

/// 語彙外 op（`0A000` への写像・応答は #759 の担当）は `schema_for` が
/// `None` を返すのみで、本ヘルパー自体はエラー型を持たない。
#[test]
fn schema_for_returns_none_for_out_of_vocabulary_op() {
    // `delete` は Issue #875（NOSQL-12）、`drop_table` は Issue #910
    // （NOSQL-13）で語彙へ加わったため、語彙外 op の fixture としては
    // `drop_index`（NOSQL-13 の対象外・引き続き語彙外）を使う。
    let value = parse_json(r#"{"op":"drop_index","table":"docs"}"#).expect("valid JSON fixture");
    let op = extract_op(&value).expect("op must be present");
    assert_eq!(op, "drop_index");
    assert!(schema_for(op).is_none());
}

/// 構文エラー（`engine::json::JsonError`）と意味エラー（`SchemaError`）が
/// 同じ `42601` へ収束することを固定する（クライアントからは区別不可能な
/// 単一の wire_code として観測される契約）。
#[test]
fn syntax_error_and_semantic_error_converge_on_same_wire_code() {
    let syntax_err = parse_json(r#"{"op": }"#).unwrap_err();
    assert_eq!(syntax_err.wire_code(), "42601");

    let value = parse_json(r#"{"op":"search","table":"docs","limit":1,"x":1}"#)
        .expect("valid JSON fixture");
    let op = extract_op(&value).expect("op must be present");
    let schema = schema_for(op).expect("schema_for must resolve search");
    let semantic_err = schema.validate(&value).unwrap_err();
    assert_eq!(semantic_err, SchemaError::UnknownKey);
    assert_eq!(semantic_err.wire_code(), "42601");
}

/// ルートが JSON オブジェクトでない入力は `extract_op` の時点で拒否される
/// （`schema_for` へ到達する前に fail-closed）。
#[test]
fn non_object_root_is_rejected_before_schema_lookup() {
    let value = parse_json("[1,2,3]").expect("valid JSON fixture");
    let err = extract_op(&value).unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}
