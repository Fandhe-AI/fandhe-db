//! `insert`／`update`／`delete` op の任意キー `returning` の値検査
//! （Issue #2090・NOSQL-22。ポインタ: `docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-22・SQL 側の対応は SQL-21）。
//!
//! 責務境界: 型（文字列または文字列配列）の検査は [`super::schema`] の
//! `StringOrArray(String)` 宣言が担い、本モジュールはその後段で
//! 「値の意味」（`"*"` または非空の識別子配列）と、分割実行修飾（`mode`／`chunk`）
//! との併用拒否を検査して `engine::sql::allowlist::Projection` へ写像する。
//! 列の実在確認・権限検査（SQL-40）・RLS 再判定は engine の束縛段が単一の真実源として
//! 担い、wire 側では行わない（存在情報の推知経路を増やさない）。
//!
//! 呼び出し元: [`super::gate::handle`] の手順 5b。得た投影は `insert`／`update`／`delete` の
//! `handle` が engine の RETURNING 付き束縛済み入口へ渡して実行する（Issue #2091）。
//!
//! エラー文言はすべて固定で、untrusted な入力（列名・文字列）を保持もエコーもしない
//! （[`super::schema::SchemaError::UnknownKey`] と同じ理由）。

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::Projection;

use super::ident::check_identifier;
use super::schema::{SchemaError, StringOrArray, Validated};

/// [`parse_returning`]・[`check_not_partitioned`] の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturningRequestError {
    /// スキーマ層の失敗（透過。`42601`）。
    Shape(SchemaError),
    /// `"*"` でも非空の配列でもない値（`42601`）。
    Invalid,
    /// 配列要素が識別子の形状でない（`"*"` を含む。`42601`）。
    InvalidIdentifier,
    /// 分割実行（`mode`／`chunk`）との併用（`42601`）。
    WithPartitioned,
}

impl ClassifiedError for ReturningRequestError {
    fn error_class(&self) -> ErrorClass {
        match self {
            ReturningRequestError::Shape(e) => e.error_class(),
            ReturningRequestError::Invalid
            | ReturningRequestError::InvalidIdentifier
            | ReturningRequestError::WithPartitioned => ErrorClass::UnsupportedSqlSyntax,
        }
    }

    fn client_message(&self) -> String {
        match self {
            ReturningRequestError::Shape(e) => e.client_message(),
            ReturningRequestError::Invalid => {
                "returning must be \"*\" or a non-empty array of column names".to_string()
            }
            ReturningRequestError::InvalidIdentifier => "invalid identifier".to_string(),
            ReturningRequestError::WithPartitioned => {
                "returning cannot be combined with mode partitioned".to_string()
            }
        }
    }
}

impl From<SchemaError> for ReturningRequestError {
    fn from(e: SchemaError) -> Self {
        ReturningRequestError::Shape(e)
    }
}

/// `returning` を [`Projection`] へ写像する。キー無しは `Ok(None)`。
///
/// 文字列形は `"*"` 専用（列名 1 つの文字列形は拒否）。配列形は非空で、各要素が
/// [`check_identifier`] を通る文字列であること（`"*"` を含む配列は識別子ではないため拒否）。
pub fn parse_returning(
    validated: &Validated<'_>,
) -> Result<Option<Projection>, ReturningRequestError> {
    let Some(raw) = validated.optional_string_or_array("returning")? else {
        return Ok(None);
    };
    match raw {
        StringOrArray::Single("*") => Ok(Some(Projection::All)),
        StringOrArray::Single(_) => Err(ReturningRequestError::Invalid),
        StringOrArray::Many(items) => {
            if items.is_empty() {
                return Err(ReturningRequestError::Invalid);
            }
            // 長さは JSON パーサーと本文サイズの上限で既に有界。
            let mut cols = Vec::with_capacity(items.len());
            for item in items {
                let JsonValue::String(name) = item else {
                    return Err(ReturningRequestError::Shape(SchemaError::TypeMismatch {
                        key: "returning",
                    }));
                };
                check_identifier(name).map_err(|_| ReturningRequestError::InvalidIdentifier)?;
                cols.push(name.clone());
            }
            Ok(Some(Projection::Columns(cols)))
        }
    }
}

/// `returning` 指定と分割実行修飾（`mode`／`chunk`）の併用を拒否する
/// （SQL-32 (a) と同じ判断。ポインタ: `docs/design/partitioned-dml.md` §3.1）。
/// `mode` の値は見ない（値の不正も併用違反も `42601` に収束する）。
pub fn check_not_partitioned(
    validated: &Validated<'_>,
    returning: &Option<Projection>,
) -> Result<(), ReturningRequestError> {
    if returning.is_none() {
        return Ok(());
    }
    // `insert` のスキーマは `mode`／`chunk` を宣言しない（`UnknownKey` は未指定扱い）。
    let has_mode = matches!(validated.optional_str("mode"), Ok(Some(_)));
    let has_chunk = matches!(validated.optional_number("chunk"), Ok(Some(_)));
    if has_mode || has_chunk {
        return Err(ReturningRequestError::WithPartitioned);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::query::schema::{DELETE_SCHEMA, INSERT_SCHEMA, UPDATE_SCHEMA};
    use engine::json::parse_json;

    fn parse_del(extra: &str) -> Result<Option<Projection>, ReturningRequestError> {
        let v = parse_json(&format!(
            r#"{{"op":"delete","table":"docs","where":{{"id":1}}{extra}}}"#
        ))
        .expect("json");
        let validated = DELETE_SCHEMA.validate(&v).expect("schema");
        parse_returning(&validated)
    }

    #[test]
    fn absent_star_and_columns() {
        assert_eq!(parse_del(""), Ok(None));
        assert_eq!(parse_del(r#","returning":"*""#), Ok(Some(Projection::All)));
        assert_eq!(
            parse_del(r#","returning":["a","b"]"#),
            Ok(Some(Projection::Columns(vec!["a".into(), "b".into()])))
        );
    }

    #[test]
    fn invalid_values_are_42601() {
        let long = "a".repeat(64);
        let cases = [
            r#""""#.to_string(),
            r#""a""#.to_string(),
            r#"" *""#.to_string(),
            r#""**""#.to_string(),
            "[]".to_string(),
            r#"["*"]"#.to_string(),
            r#"[""]"#.to_string(),
            r#"["a b"]"#.to_string(),
            format!(r#"["{long}"]"#),
            r#"["a\u0000"]"#.to_string(),
        ];
        for c in cases {
            let err = parse_del(&format!(r#","returning":{c}"#)).expect_err(&c);
            assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax, "{c}");
        }
    }

    #[test]
    fn messages_do_not_echo_input() {
        let err = parse_del(r#","returning":["secret_col!"]"#).expect_err("invalid");
        assert!(!err.client_message().contains("secret"));
        let err = parse_del(r#","returning":"secret""#).expect_err("invalid");
        assert!(!err.client_message().contains("secret"));
    }

    #[test]
    fn partitioned_combination_is_rejected() {
        for extra in [
            r#","returning":"*","mode":"partitioned""#,
            r#","returning":"*","chunk":10"#,
        ] {
            let v = parse_json(&format!(
                r#"{{"op":"delete","table":"docs","filter":[{{"column":"a","op":"eq","value":1}}]{extra}}}"#
            ))
            .expect("json");
            let validated = DELETE_SCHEMA.validate(&v).expect("schema");
            let ret = parse_returning(&validated).expect("parse");
            assert_eq!(
                check_not_partitioned(&validated, &ret),
                Err(ReturningRequestError::WithPartitioned)
            );
        }
        // returning なしなら素通し。
        let v = parse_json(
            r#"{"op":"update","table":"docs","set":{"a":1},"filter":[],"mode":"partitioned"}"#,
        )
        .expect("json");
        let validated = UPDATE_SCHEMA.validate(&v).expect("schema");
        assert_eq!(check_not_partitioned(&validated, &None), Ok(()));
    }

    #[test]
    fn insert_without_mode_keys_passes() {
        let v = parse_json(r#"{"op":"insert","table":"docs","rows":[],"returning":"*"}"#)
            .expect("json");
        let validated = INSERT_SCHEMA.validate(&v).expect("schema");
        let ret = parse_returning(&validated).expect("parse");
        assert_eq!(ret, Some(Projection::All));
        assert_eq!(check_not_partitioned(&validated, &ret), Ok(()));
    }
}
