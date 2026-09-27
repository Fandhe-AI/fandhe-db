//! `update`／`delete` op（Issue #876・TASK-186・NOSQL-12）が共有する
//! 「対象行の指定形」判定。両 op の `execute`（[`super::update`]・
//! [`super::delete`]）はこのモジュールを経由して同一の判定（順序・エラー
//! 分類）を共有し、`where`／`filter` の排他判定を二重実装しない。
//!
//! ## `where`／`filter` の排他契約
//!
//! [`super::schema::UPDATE_SCHEMA`]／[`super::schema::DELETE_SCHEMA`] は
//! `where`（[`super::schema::WHERE_ID_SCHEMA`]。単一行・`id` 完全一致形）・
//! `filter`（述語形。Issue #869・#870 が構造検証のみ実装済み）をいずれも
//! `Optional` として宣言する（排他の意味検証はスキーマ側では行わない）。
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
//! 5. `where` のみ → [`TargetForm::RowId`]（`where.id` を読み取る）。
//!
//! `where.id` は [`super::schema::WHERE_ID_SCHEMA`] により非 `Null`・
//! `Number` 型であることが [`Validated::optional_object`] の時点で既に
//! 保証されている（ネストしたオブジェクトも [`super::schema::ObjectSchema::
//! validate`] が再帰的に検証する）。本モジュールはその値が
//! [`engine::json::JsonNumber::as_exact_u64`]（非負整数・`u64` 無損失）で
//! あることのみを追加検証するが、`wire_code` は SQL 表層の字句解析・束縛と
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

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;

use super::schema::{SchemaError, Validated};

/// 対象行の指定形（[`bind_target_form`] の出力）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetForm<'a> {
    /// `where: { "id": <n> }`（単一行・`id` 完全一致形）。
    RowId(u64),
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
            // `WHERE_ID_SCHEMA` が `id` の必須・`Number` 型を既に保証済み。
            // `JsonNumber` の variant で SQL 表層とのパリティを取る
            // （モジュール doc 参照）。
            let id = match where_obj.get("id") {
                Some(JsonValue::Number(n)) => match n.as_exact_u64() {
                    Some(id) => id,
                    None if matches!(n, engine::json::JsonNumber::NegInt(_)) => {
                        return Err(DmlTargetError::NegativeWhereId)
                    }
                    None => return Err(DmlTargetError::InvalidWhereId),
                },
                _ => {
                    // スキーマ検証済みのため通常到達しない（多層防御）。
                    return Err(DmlTargetError::InvalidWhereId);
                }
            };
            Ok(TargetForm::RowId(id))
        }
    }
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
        assert_eq!(bind_target_form(&v).unwrap(), TargetForm::RowId(1));
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
        assert!(matches!(
            bind_target_form(&v),
            Err(DmlTargetError::InvalidWhereId)
        ));
    }

    #[test]
    fn where_id_negative_is_negative_where_id_42601() {
        let v = validated(
            r#"{"op":"delete","table":"docs","where":{"id":-1}}"#,
            &super::super::schema::DELETE_SCHEMA,
        );
        assert!(matches!(
            bind_target_form(&v),
            Err(DmlTargetError::NegativeWhereId)
        ));
    }
}
