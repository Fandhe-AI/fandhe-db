//! `update`／`delete` op（Issue #876・TASK-186・NOSQL-12）が共有する
//! 「対象行の指定形」判定。両 op の `execute`（[`super::update`]・
//! [`super::delete`]）はこのモジュールを経由して同一の判定（順序・エラー
//! 分類）を共有し、`where`／`filter` の排他判定を二重実装しない。
//!
//! ## `where`／`filter` の排他契約
//!
//! [`super::schema::UPDATE_SCHEMA`]／[`super::schema::DELETE_SCHEMA`] は
//! `where`（任意キーの単一行アドレス。`FieldType::AnyObject`）・
//! `filter`（述語形。Issue #869・#870 が構造検証のみ実装済み）をいずれも
//! `Optional` として宣言する（排他の意味検証・`where` のキー集合の検証は
//! スキーマ側では行わない。TABLE-23・NOSQL-23 のポインタ。Issue #2201）。
//! 本モジュールが以下の順で判定する:
//!
//! 1. 両方指定 → [`DmlTargetError::ExclusiveTargetRequired`]（`42601`）。
//! 2. 双方欠落 → 同じく `42601`（対象指定なし。SQL 表層の `WHERE` 句省略が
//!    構文エラーであることとのパリティ）。
//! 3. `filter` が空配列（`filter: []`） → [`DmlTargetError::EmptyFilter`]
//!    （`42601`。SQL 表層の `WHERE` 句省略が構文エラーであることとの
//!    パリティ。engine を一切呼ばず副作用ゼロで拒否する）。
//! 4. `filter` が非空 → [`TargetForm::Predicate`]（述語形。TASK-186・
//!    NOSQL-12、Issue #1062。SQL 表層 `UPDATE`／`DELETE ... WHERE <述語>
//!    USING OPERATION_ID`（SQL-19・TASK-192）と同一の実行本体
//!    〔`engine::core::EngineCore::execute_bound_predicate_update_in_session`／
//!    `execute_bound_predicate_delete_in_session`〕へ委譲する。実行結線は
//!    [`super::update`]・[`super::delete`] の担当）。
//! 5. `where` のみ → [`TargetForm::RowAddress`]。スキーマに依存しない検査
//!    （空オブジェクト・キー数上限・列名の識別子形状。いずれも `42601`）をここで
//!    済ませる。表のスキーマに依存する解決は呼び出し元（[`super::update`]・
//!    [`super::delete`]）が次の規則で行う。
//!
//! ## `where` の行アドレス解決規則（Issue #2201）
//!
//! - キー集合がちょうど `{"id"}` で表が疑似列 `id` を持つ（従来方式・移行済み）
//!   → 従来の単一行形（[`row_id_from_address`]。実行は `execute_bound_update_in_session`
//!   系）。採番方式の表では `id` は宣言した通常の列でしかないため述語形へ流す
//! - それ以外で表に PRIMARY KEY があり、キー集合が PK の列集合と完全一致
//!   → PK 列ごとの等価述語（[`bind_primary_key_predicates`]。`filter` の `eq` と同じ
//!   束縛器を再利用し、SQL 表層の `WHERE a = .. AND b = ..` と同一の `WherePredicate`
//!   になる）へ写像して述語形の実行入口へ流す
//! - それ以外（PK なし表・キーの過不足・未知列・`null` 値）→ `42601`（固定文言）。
//!   engine の書き込みへ到達しない（fail-closed）
//!
//! 単一行形の `where.id` は、本モジュール（[`row_id_from_address`]）が
//! [`engine::json::JsonNumber::as_exact_u64`]（非負整数・`u64` 無損失）で
//! あることを検証する。`wire_code` は SQL 表層の字句解析・束縛と
//! パリティを取るため [`JsonNumber`] の variant で分岐する:
//!
//! - `JsonNumber::Float{..}`（小数点・指数部を含む。整数値に丸められる
//!   小数を含む）→ [`DmlTargetError::InvalidWhereId`]（`22000`）。SQL 表層の
//!   字句解析は `1.5` を単一の `Number` トークンとして読み `WHERE id = <n>`
//!   の単一行形へ振り分けるが、`sql::parser::bind_update`／`bind_delete` の
//!   `id_literal.parse::<u64>()` が失敗し `SqlSurfaceError::invalid_input`
//!   （`22000`）になる（`sql/parser.rs::bind_update` 参照）
//! - `JsonNumber::NegInt(_)`（`-` 始まりの整数リテラル。`-0` を含む）→
//!   [`DmlTargetError::NegativeWhereId`]（`42601`）。SQL 表層の字句解析は
//!   `-` を数値リテラルに含めず独立した `Punct('-')` として読むため、
//!   `WHERE id = -1` は「`Ident("id")`・`Punct('=')`・`Token::Number`」の
//!   単一行形パターンに一致せず述語形（`UpdateWhereForm::Predicates`）へ
//!   振り分けられ、単一行 id 指定形専用エントリポイント
//!   （`sql::allowlist::validate_update`／`validate_delete`）がこれを
//!   `UnsupportedSyntax`（`42601`）で拒否する（`sql/allowlist.rs::
//!   parse_update_where` 参照）
//!
//! [`super::insert::bind_row`] の `id` 疑似列判定（すべて `22000` に統一）
//! とは意図的に異なる（`insert` の `id` は SQL 表層の字句解析を経由しない
//! 疑似列であり本モジュールが対応する SQL パリティ対象ではないため）。

use std::collections::BTreeMap;

use engine::catalog::TableSchema;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::{SqlSurfaceError, WherePredicate};

use super::filter;
use super::ident;
use super::schema::{SchemaError, Validated};

/// `where` のキー数の上限（engine の PRIMARY KEY 列数上限と同値。`Vec` 確保より前の検査用）。
pub const MAX_ROW_ADDRESS_KEYS: usize = 32;

/// 対象行の指定形（[`bind_target_form`] の出力）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetForm<'a> {
    /// `where: { <列>: <値>, ... }`（単一行アドレス。Issue #2201）。スキーマ非依存の検査
    /// （空・キー数上限・列名の形状）だけを通過した値で、`id` 形か PRIMARY KEY 形かの解決は
    /// 表のスキーマが要る（モジュール doc「行アドレス解決規則」）。
    RowAddress(&'a BTreeMap<String, JsonValue>),
    /// `filter: [...]`（述語形。空配列は [`DmlTargetError::EmptyFilter`] で
    /// 別途拒否されるため、本 variant が保持する配列は常に非空。
    /// TASK-186・NOSQL-12、Issue #1062）。実行結線（`WherePredicate` への
    /// 写像・束縛）は [`super::filter::bind_filter_where_predicates`] の担当で、
    /// 本モジュールは形の判定のみを行う。
    Predicate(&'a [JsonValue]),
}

/// [`bind_target_form`] の失敗を表す。いずれも [`ClassifiedError`] を実装する。
#[derive(Debug, Clone)]
pub enum DmlTargetError {
    /// [`Validated`] アクセサの型・キー不整合（多層防御。通常は
    /// `UPDATE_SCHEMA`／`DELETE_SCHEMA` の検証済みのため到達しない）。
    Shape(SchemaError),
    /// `where`／`filter` の両方指定、または双方欠落。
    ExclusiveTargetRequired,
    /// `filter: []`（空配列）。SQL 表層の `WHERE` 句省略が構文エラーで
    /// あることとのパリティで `42601`（Issue #1062）。engine を一切呼ばず
    /// 副作用ゼロで拒否する。
    EmptyFilter,
    /// `where.id` が小数（`JsonNumber::Float`）。SQL 表層の `bind_update`／
    /// `bind_delete` の `u64` パース失敗と同じ `22000`（モジュール doc 参照）。
    InvalidWhereId,
    /// `where.id` が負の整数（`JsonNumber::NegInt`。`-0` を含む）。SQL 表層の
    /// 字句解析が述語形へ振り分けたうえで単一行形専用エントリポイントが
    /// 拒否するのと同じ `42601`（モジュール doc 参照）。
    NegativeWhereId,
    /// `where.id` が数値でない（旧スキーマ検証の型不一致と同じ `42601`）。
    WhereIdNotNumber,
    /// `where` が空オブジェクト（`42601`）。
    EmptyRowAddress,
    /// `where` のキー数が [`MAX_ROW_ADDRESS_KEYS`] を超える（`42601`。確保より前に拒否）。
    TooManyAddressKeys,
    /// `where` の列名が識別子として意味を持ちうる形状を満たさない（`42601`）。
    InvalidAddressColumn,
    /// `where` のキー集合が表の PRIMARY KEY の列集合と一致しない、PRIMARY KEY が無い、
    /// または値が `null`（`42601`。固定文言で列名・表名・値を含めない）。
    RowAddressMismatch,
}

impl From<SchemaError> for DmlTargetError {
    fn from(err: SchemaError) -> Self {
        DmlTargetError::Shape(err)
    }
}

impl ClassifiedError for DmlTargetError {
    fn error_class(&self) -> ErrorClass {
        match self {
            DmlTargetError::Shape(err) => err.error_class(),
            DmlTargetError::ExclusiveTargetRequired
            | DmlTargetError::NegativeWhereId
            | DmlTargetError::WhereIdNotNumber
            | DmlTargetError::EmptyRowAddress
            | DmlTargetError::TooManyAddressKeys
            | DmlTargetError::InvalidAddressColumn
            | DmlTargetError::RowAddressMismatch
            | DmlTargetError::EmptyFilter => ErrorClass::UnsupportedSqlSyntax,
            DmlTargetError::InvalidWhereId => ErrorClass::InvalidInput,
        }
    }

    fn client_message(&self) -> String {
        match self {
            DmlTargetError::Shape(err) => err.client_message(),
            DmlTargetError::ExclusiveTargetRequired => {
                "exactly one of where or filter must be specified".to_string()
            }
            DmlTargetError::EmptyFilter => "filter must contain at least one predicate".to_string(),
            DmlTargetError::InvalidWhereId => "where.id must be an integer JSON number".to_string(),
            DmlTargetError::NegativeWhereId => {
                "where.id must be a non-negative integer".to_string()
            }
            DmlTargetError::WhereIdNotNumber => "where.id must be a number".to_string(),
            DmlTargetError::EmptyRowAddress => "where must not be empty".to_string(),
            DmlTargetError::TooManyAddressKeys => "where has too many keys".to_string(),
            DmlTargetError::InvalidAddressColumn => "invalid identifier".to_string(),
            DmlTargetError::RowAddressMismatch => {
                "where must specify exactly the primary key columns".to_string()
            }
        }
    }
}

/// `validated`（`UPDATE_SCHEMA`／`DELETE_SCHEMA` いずれかの検証済み値）から
/// 対象行の指定形を判定する（モジュール doc の判定順序を参照）。
pub fn bind_target_form<'a>(validated: &Validated<'a>) -> Result<TargetForm<'a>, DmlTargetError> {
    let where_obj = validated.optional_object("where")?;
    let filter_items = validated.optional_array("filter")?;

    match (where_obj, filter_items) {
        (Some(_), Some(_)) => Err(DmlTargetError::ExclusiveTargetRequired),
        (None, None) => Err(DmlTargetError::ExclusiveTargetRequired),
        (None, Some([])) => Err(DmlTargetError::EmptyFilter),
        (None, Some(items)) => Ok(TargetForm::Predicate(items)),
        (Some(where_obj), None) => {
            check_row_address_shape(where_obj)?;
            Ok(TargetForm::RowAddress(where_obj))
        }
    }
}

/// `where` のスキーマ非依存の検査（空・キー数上限・列名の識別子形状。すべて `42601`）。
/// untrusted な列名を engine のエラー文言へ渡さないため、束縛より前に弾く。
fn check_row_address_shape(address: &BTreeMap<String, JsonValue>) -> Result<(), DmlTargetError> {
    if address.is_empty() {
        return Err(DmlTargetError::EmptyRowAddress);
    }
    if address.len() > MAX_ROW_ADDRESS_KEYS {
        return Err(DmlTargetError::TooManyAddressKeys);
    }
    for key in address.keys() {
        ident::check_identifier(key).map_err(|_| DmlTargetError::InvalidAddressColumn)?;
    }
    Ok(())
}

/// `where` のキー集合がちょうど `{"id"}` か（従来の単一行 `id` 形の候補か）。
pub fn is_id_address(address: &BTreeMap<String, JsonValue>) -> bool {
    address.len() == 1 && address.contains_key("id")
}

/// 従来の単一行形の `where.id` を `u64` へ読む（SQL 表層とのパリティ。モジュール doc 参照）。
pub fn row_id_from_address(address: &BTreeMap<String, JsonValue>) -> Result<u64, DmlTargetError> {
    match address.get("id") {
        Some(JsonValue::Number(n)) => match n.as_exact_u64() {
            Some(id) => Ok(id),
            None if matches!(n, engine::json::JsonNumber::NegInt(_)) => {
                Err(DmlTargetError::NegativeWhereId)
            }
            None => Err(DmlTargetError::InvalidWhereId),
        },
        _ => Err(DmlTargetError::WhereIdNotNumber),
    }
}

/// `where` を `schema` の PRIMARY KEY の等価述語へ写像する（モジュール doc の規則 2・3）。
///
/// 述語は PK の宣言順に並べ、`filter` の `eq` 葉を構造化した [`JsonValue`] で組み立てて
/// [`filter::bind_filter_where_predicates`] へ渡す（SQL 文字列は組み立てない。値の型別レーンと
/// 型不一致の `wire_code` が `filter` の `eq` と一致し、SQL 表層の述語形と同一の
/// `WherePredicate` になる）。
///
/// 疑似列 `id` を持つ表で `{"id"}` のみが渡された場合（事前判定と束縛時スナップショットの
/// 食い違い＝並行 DDL）は、別経路で書き込まず `42601` で拒否する。
pub fn bind_primary_key_predicates(
    address: &BTreeMap<String, JsonValue>,
    schema: &TableSchema,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let mismatch = || SqlSurfaceError::UnsupportedSyntax {
        detail: DmlTargetError::RowAddressMismatch.client_message(),
    };
    if is_id_address(address) && !schema.uses_allocated_row_identity() {
        return Err(mismatch());
    }
    let Some(pk) = schema.primary_key() else {
        return Err(mismatch());
    };
    if pk.len() != address.len() {
        return Err(mismatch());
    }
    let mut items = Vec::with_capacity(pk.len());
    for column in pk {
        let Some(value) = address.get(column) else {
            return Err(mismatch());
        };
        if matches!(value, JsonValue::Null) {
            return Err(mismatch());
        }
        let mut leaf = BTreeMap::new();
        leaf.insert("column".to_string(), JsonValue::String(column.clone()));
        leaf.insert("op".to_string(), JsonValue::String("eq".to_string()));
        leaf.insert("value".to_string(), value.clone());
        items.push(JsonValue::Object(leaf));
    }
    filter::bind_filter_where_predicates(&items, schema)
        .map_err(filter::FilterError::into_sql_surface_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::json::parse_json;

    fn validated(
        json: &'static str,
        schema: &'static super::super::schema::ObjectSchema,
    ) -> Validated<'static> {
        let value: &'static JsonValue = Box::leak(Box::new(parse_json(json).expect("valid json")));
        schema.validate(value).expect("schema validate")
    }

    #[test]
    fn where_only_binds_row_id() {
        let v = validated(
            r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1}}"#,
            &super::super::schema::UPDATE_SCHEMA,
        );
        let TargetForm::RowAddress(addr) = bind_target_form(&v).unwrap() else {
            panic!("expected row address form");
        };
        assert!(is_id_address(addr));
        assert_eq!(row_id_from_address(addr).unwrap(), 1);
    }

    fn address_error(json: &'static str) -> DmlTargetError {
        let v = validated(json, &super::super::schema::DELETE_SCHEMA);
        bind_target_form(&v).unwrap_err()
    }

    #[test]
    fn empty_where_is_rejected_with_42601() {
        let e = address_error(r#"{"op":"delete","table":"docs","where":{}}"#);
        assert!(matches!(e, DmlTargetError::EmptyRowAddress));
        assert_eq!(e.wire_code(), "42601");
    }

    #[test]
    fn invalid_where_column_name_is_rejected_with_42601() {
        let e = address_error(r#"{"op":"delete","table":"docs","where":{"a b":1}}"#);
        assert!(matches!(e, DmlTargetError::InvalidAddressColumn));
        assert_eq!(e.wire_code(), "42601");
    }

    #[test]
    fn where_with_too_many_keys_is_rejected_before_binding() {
        let mut body = String::from(r#"{"op":"delete","table":"docs","where":{"#);
        for i in 0..=MAX_ROW_ADDRESS_KEYS {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&format!("\"c{i}\":1"));
        }
        body.push_str("}}");
        let v: &'static JsonValue = Box::leak(Box::new(parse_json(&body).expect("valid json")));
        let validated = super::super::schema::DELETE_SCHEMA
            .validate(v)
            .expect("schema validate");
        assert!(matches!(
            bind_target_form(&validated),
            Err(DmlTargetError::TooManyAddressKeys)
        ));
    }

    #[test]
    fn where_id_non_number_is_42601() {
        let v = validated(
            r#"{"op":"delete","table":"docs","where":{"id":"1"}}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        let TargetForm::RowAddress(addr) = bind_target_form(&v).unwrap() else {
            panic!("expected row address form");
        };
        let e = row_id_from_address(addr).unwrap_err();
        assert!(matches!(e, DmlTargetError::WhereIdNotNumber));
        assert_eq!(e.wire_code(), "42601");
    }

    fn pk_schema() -> TableSchema {
        use engine::catalog::{ColumnDef, ColumnType};
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("a", ColumnType::Text, false),
                ColumnDef::new("b", ColumnType::Integer, false),
                ColumnDef::new("v", ColumnType::Text, true),
            ],
        )
        .with_primary_key(vec!["a".to_string(), "b".to_string()])
    }

    fn address(json: &str) -> BTreeMap<String, JsonValue> {
        match parse_json(json).expect("valid json") {
            JsonValue::Object(m) => m,
            _ => panic!("object expected"),
        }
    }

    #[test]
    fn primary_key_address_binds_in_declared_order() {
        let preds = bind_primary_key_predicates(&address(r#"{"b":2,"a":"x"}"#), &pk_schema())
            .expect("bind");
        assert_eq!(preds.len(), 2);
    }

    #[test]
    fn primary_key_address_mismatch_is_fail_closed() {
        for json in [
            r#"{"a":"x"}"#,
            r#"{"a":"x","b":2,"v":"y"}"#,
            r#"{"a":"x","zz":2}"#,
            r#"{"a":"x","b":null}"#,
        ] {
            let e = bind_primary_key_predicates(&address(json), &pk_schema()).unwrap_err();
            assert!(
                matches!(e, SqlSurfaceError::UnsupportedSyntax { .. }),
                "{json}"
            );
        }
    }

    #[test]
    fn table_without_primary_key_rejects_address() {
        use engine::catalog::{ColumnDef, ColumnType};
        let schema = TableSchema::new("t", vec![ColumnDef::new("a", ColumnType::Text, false)]);
        assert!(bind_primary_key_predicates(&address(r#"{"a":"x"}"#), &schema).is_err());
    }

    #[test]
    fn id_address_on_pseudo_id_table_is_not_rerouted_to_predicate_form() {
        use engine::catalog::{ColumnDef, ColumnType};
        let schema = TableSchema::new("t", vec![ColumnDef::new("a", ColumnType::Text, true)]);
        assert!(bind_primary_key_predicates(&address(r#"{"id":1}"#), &schema).is_err());
    }

    #[test]
    fn both_where_and_filter_is_rejected() {
        let v = validated(
            r#"{"op":"delete","table":"docs","where":{"id":1},"filter":[]}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        assert!(matches!(
            bind_target_form(&v),
            Err(DmlTargetError::ExclusiveTargetRequired)
        ));
    }

    #[test]
    fn neither_where_nor_filter_is_rejected() {
        let v = validated(
            r#"{"op":"delete","table":"docs"}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        assert!(matches!(
            bind_target_form(&v),
            Err(DmlTargetError::ExclusiveTargetRequired)
        ));
    }

    #[test]
    fn empty_filter_is_rejected_with_42601() {
        let v = validated(
            r#"{"op":"delete","table":"docs","filter":[]}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        assert!(matches!(
            bind_target_form(&v),
            Err(DmlTargetError::EmptyFilter)
        ));
        assert_eq!(bind_target_form(&v).unwrap_err().wire_code(), "42601");
    }

    #[test]
    fn non_empty_filter_only_binds_predicate_form() {
        let v = validated(
            r#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        let TargetForm::Predicate(items) = bind_target_form(&v).expect("predicate form") else {
            panic!("expected predicate form");
        };
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn wire_codes_match_expected_classes() {
        assert_eq!(DmlTargetError::ExclusiveTargetRequired.wire_code(), "42601");
        assert_eq!(DmlTargetError::InvalidWhereId.wire_code(), "22000");
        assert_eq!(DmlTargetError::NegativeWhereId.wire_code(), "42601");
        assert_eq!(DmlTargetError::EmptyFilter.wire_code(), "42601");
    }

    #[test]
    fn where_id_float_is_invalid_where_id_22000() {
        let v = validated(
            r#"{"op":"delete","table":"docs","where":{"id":1.5}}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        let TargetForm::RowAddress(addr) = bind_target_form(&v).expect("address form") else {
            panic!("expected row address form");
        };
        assert!(matches!(
            row_id_from_address(addr),
            Err(DmlTargetError::InvalidWhereId)
        ));
    }

    #[test]
    fn where_id_negative_is_negative_where_id_42601() {
        let v = validated(
            r#"{"op":"delete","table":"docs","where":{"id":-1}}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        let TargetForm::RowAddress(addr) = bind_target_form(&v).expect("address form") else {
            panic!("expected row address form");
        };
        assert!(matches!(
            row_id_from_address(addr),
            Err(DmlTargetError::NegativeWhereId)
        ));
    }
}
