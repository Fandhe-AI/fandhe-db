//! SQL 型名の許可リスト構造解析（TASK-202・SQL-23。Issue #900。
//! `ALTER TABLE ADD COLUMN`・`ALTER COLUMN TYPE`（Issue #1167）が呼び出し元）。
//!
//! `sql::allowlist::Parser` の内部状態（`tokens`／`pos`）には触れず、トークン
//! スライスと読み取り位置（`&mut usize`）だけを引数に取る独立実装とする。
//! `allowlist::Parser` の各種 `expect_*` ヘルパーはすべて `allowlist.rs` の
//! private メソッドであり、別モジュールからは呼べないため、型名解析に必要な
//! 最小限のトークン走査だけをここへ複製する（`allowlist::split_parenthesized`
//! と同じ「呼び出し元パーサー種別に依存しない自己完結の走査」方針）。
//!
//! `catalog::ColumnType` へは変換しない。ENUM 型名（未知の識別子）の存在確認・
//! 語彙解決は実行段（`sql::ddl::execute_alter_table_add_column`）が
//! `Storage::get_enum_type` で行う契約とし、本モジュールは構文木
//! （[`SqlColumnTypeName`]）を返すところまでに責務を限定する。
//!
//! `NUMERIC`/`DECIMAL` の精度・位取りの範囲検証（`1 <= precision <= 38`・
//! `scale <= precision`）はここでは行わない——`catalog::alter_table_add_column`
//! が内部で呼ぶ `validate_column`（`catalog.rs` の非公開検証関数）に一本化
//! されており、二重実装を避けるためここでは構文上の形（`u8` として妥当な
//! 非負整数か）だけを検証する。範囲外の値は実行段で `CatalogError::Invalid`
//! （`sql::ddl` が `42601` へ写像）として拒否される。

use super::allowlist::SqlSurfaceError;
use super::lexer::Token;

/// 型名の許可リスト構文木。`ALTER TABLE ADD COLUMN`（将来 `CREATE TABLE` とも
/// 共有する前提。`docs/design/sql-alter-table-add-column.md` 参照）が受理する
/// 型名の閉じた集合＋ ENUM 型名候補。
///
/// `VECTOR` は構文としては受理するが、実行段（`sql::ddl::
/// execute_alter_table_add_column`）が常に `0A000`（`SqlSurfaceError::
/// FeatureNotSupported`）で拒否する（既存行が埋め込みバイトを持たない
/// テーブルへの `VECTOR` 列追加は、arena 構築・KNN・HNSW 各経路の安全性が
/// 未検証のため。詳細は `docs/design/sql-alter-table-add-column.md` 参照）。
///
/// 配列型（`<型>[]`）は対象外——`lexer` が `[`／`]` を字句化できないため、
/// 字句解析段階で `42601` になる（`lexer.rs` を広げない設計判断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlColumnTypeName {
    Text,
    Integer,
    BigInt,
    Real,
    /// `DOUBLE PRECISION`（2 語）。
    Double,
    Boolean,
    Date,
    Timestamp,
    Bytea,
    Json,
    Jsonb,
    Uuid,
    /// `VECTOR(N)`。次元は構文上の非負整数のみ検証し、`MAX_VECTOR_DIM` 等の
    /// 意味検証は行わない（本 variant 自体が実行段で常に `0A000` 拒否される
    /// ため到達しない）。
    Vector(u32),
    /// `NUMERIC(precision, scale)`／`DECIMAL(precision, scale)`。範囲検証は
    /// 実行段（`catalog::alter_table_add_column`）に委譲する（モジュール
    /// ドキュメント参照）。
    Numeric {
        precision: u8,
        scale: u8,
    },
    /// 上記いずれの予約型名にも一致しなかった識別子。ENUM 型名の候補として
    /// 実行段が `Storage::get_enum_type` で存在確認・語彙解決する。
    Enum(String),
}

/// `tokens[*pos..]` の先頭から型名 1 個を読み取り、消費した分だけ `*pos` を
/// 進める。`sql::allowlist::Parser::parse_alter_table_add_column` が自身の
/// `tokens`／`pos` を共有して呼ぶ。
pub(crate) fn parse_column_type_name(
    tokens: &[Token],
    pos: &mut usize,
) -> Result<SqlColumnTypeName, SqlSurfaceError> {
    let name = expect_ident(tokens, pos)?;
    match name.to_ascii_uppercase().as_str() {
        "TEXT" => Ok(SqlColumnTypeName::Text),
        "INTEGER" => Ok(SqlColumnTypeName::Integer),
        "BIGINT" => Ok(SqlColumnTypeName::BigInt),
        "REAL" => Ok(SqlColumnTypeName::Real),
        "DOUBLE" => {
            expect_contextual_keyword(tokens, pos, "PRECISION")?;
            Ok(SqlColumnTypeName::Double)
        }
        "BOOLEAN" => Ok(SqlColumnTypeName::Boolean),
        "DATE" => Ok(SqlColumnTypeName::Date),
        "TIMESTAMP" => Ok(SqlColumnTypeName::Timestamp),
        "BYTEA" => Ok(SqlColumnTypeName::Bytea),
        "JSON" => Ok(SqlColumnTypeName::Json),
        "JSONB" => Ok(SqlColumnTypeName::Jsonb),
        "UUID" => Ok(SqlColumnTypeName::Uuid),
        "VECTOR" => {
            expect_punct(tokens, pos, '(')?;
            let dim = expect_strict_u32(tokens, pos)?;
            expect_punct(tokens, pos, ')')?;
            Ok(SqlColumnTypeName::Vector(dim))
        }
        "NUMERIC" | "DECIMAL" => {
            expect_punct(tokens, pos, '(')?;
            let precision = expect_strict_u8(tokens, pos)?;
            expect_punct(tokens, pos, ',')?;
            let scale = expect_strict_u8(tokens, pos)?;
            expect_punct(tokens, pos, ')')?;
            Ok(SqlColumnTypeName::Numeric { precision, scale })
        }
        // 未知の識別子は ENUM 型名候補として構文上は受理する（原文の大文字小文字
        // をそのまま保持する。ENUM 型名は `catalog::validate_identifier` の
        // 識別子規則に従うため大文字小文字を区別する）。
        _ => Ok(SqlColumnTypeName::Enum(name)),
    }
}

fn expect_ident(tokens: &[Token], pos: &mut usize) -> Result<String, SqlSurfaceError> {
    match tokens.get(*pos) {
        Some(Token::Ident(s)) => {
            *pos += 1;
            Ok(s.clone())
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected a type name, got {other:?}"
        ))),
    }
}

fn expect_contextual_keyword(
    tokens: &[Token],
    pos: &mut usize,
    word: &str,
) -> Result<(), SqlSurfaceError> {
    match tokens.get(*pos) {
        Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word) => {
            *pos += 1;
            Ok(())
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected {word}, got {other:?}"
        ))),
    }
}

fn expect_punct(tokens: &[Token], pos: &mut usize, c: char) -> Result<(), SqlSurfaceError> {
    match tokens.get(*pos) {
        Some(Token::Punct(p)) if *p == c => {
            *pos += 1;
            Ok(())
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected '{c}', got {other:?}"
        ))),
    }
}

/// 非負整数の厳密パース（先頭ゼロ・符号・小数はいずれも不受理）。untrusted な
/// SQL テキストからのパースのため `unwrap`/`expect`/添字アクセスは使わない
/// （`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
fn parse_strict_decimal<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

fn expect_strict_u32(tokens: &[Token], pos: &mut usize) -> Result<u32, SqlSurfaceError> {
    match tokens.get(*pos) {
        Some(Token::Number(s)) => {
            let v = parse_strict_decimal::<u32>(s).ok_or_else(|| {
                SqlSurfaceError::unsupported(format!("malformed numeric literal: {s:?}"))
            })?;
            *pos += 1;
            Ok(v)
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected a numeric literal, got {other:?}"
        ))),
    }
}

fn expect_strict_u8(tokens: &[Token], pos: &mut usize) -> Result<u8, SqlSurfaceError> {
    match tokens.get(*pos) {
        Some(Token::Number(s)) => {
            let v = parse_strict_decimal::<u8>(s).ok_or_else(|| {
                SqlSurfaceError::unsupported(format!("malformed numeric literal: {s:?}"))
            })?;
            *pos += 1;
            Ok(v)
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected a numeric literal, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn parse_type(sql_fragment: &str) -> Result<(SqlColumnTypeName, usize), SqlSurfaceError> {
        let tokens = tokenize(sql_fragment).expect("tokenize");
        let mut pos = 0;
        let ty = parse_column_type_name(&tokens, &mut pos)?;
        Ok((ty, pos))
    }

    #[test]
    fn accepts_all_scalar_type_names_case_insensitively() {
        for (raw, expected) in [
            ("TEXT", SqlColumnTypeName::Text),
            ("text", SqlColumnTypeName::Text),
            ("INTEGER", SqlColumnTypeName::Integer),
            ("BIGINT", SqlColumnTypeName::BigInt),
            ("REAL", SqlColumnTypeName::Real),
            ("BOOLEAN", SqlColumnTypeName::Boolean),
            ("DATE", SqlColumnTypeName::Date),
            ("TIMESTAMP", SqlColumnTypeName::Timestamp),
            ("BYTEA", SqlColumnTypeName::Bytea),
            ("JSON", SqlColumnTypeName::Json),
            ("JSONB", SqlColumnTypeName::Jsonb),
            ("UUID", SqlColumnTypeName::Uuid),
        ] {
            let (ty, _) = parse_type(raw).unwrap_or_else(|e| panic!("{raw:?} rejected: {e}"));
            assert_eq!(ty, expected, "for {raw:?}");
        }
    }

    #[test]
    fn accepts_double_precision_two_words() {
        let (ty, consumed) = parse_type("DOUBLE PRECISION").expect("valid");
        assert_eq!(ty, SqlColumnTypeName::Double);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn rejects_double_without_precision() {
        assert!(parse_type("DOUBLE").is_err());
        assert!(parse_type("DOUBLE FLOAT").is_err());
    }

    #[test]
    fn accepts_vector_with_dimension() {
        let (ty, consumed) = parse_type("VECTOR(384)").expect("valid");
        assert_eq!(ty, SqlColumnTypeName::Vector(384));
        assert_eq!(consumed, 4);
    }

    #[test]
    fn rejects_vector_without_arguments() {
        for raw in ["VECTOR", "VECTOR()", "VECTOR(384", "VECTOR 384)"] {
            assert!(parse_type(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }

    #[test]
    fn accepts_numeric_and_decimal_with_precision_scale() {
        for raw in ["NUMERIC(5,2)", "numeric(5, 2)", "DECIMAL(38,0)"] {
            let (ty, _) = parse_type(raw).unwrap_or_else(|e| panic!("{raw:?} rejected: {e}"));
            assert!(matches!(ty, SqlColumnTypeName::Numeric { .. }));
        }
    }

    #[test]
    fn numeric_precision_scale_round_trip_values() {
        let (ty, _) = parse_type("NUMERIC(12,4)").expect("valid");
        assert_eq!(
            ty,
            SqlColumnTypeName::Numeric {
                precision: 12,
                scale: 4
            }
        );
    }

    #[test]
    fn rejects_malformed_numeric_parameter_shapes() {
        for raw in [
            "NUMERIC",
            "NUMERIC()",
            "NUMERIC(5)",
            "NUMERIC(5 2)",
            "NUMERIC(5,2,1)",
            "NUMERIC(05,2)",
            "NUMERIC(-1,2)",
            "NUMERIC(1.5,2)",
            "NUMERIC(256,2)",
        ] {
            assert!(parse_type(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }

    #[test]
    fn unknown_identifier_is_treated_as_enum_candidate() {
        let (ty, consumed) = parse_type("mood").expect("valid as enum candidate");
        assert_eq!(ty, SqlColumnTypeName::Enum("mood".to_string()));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn enum_candidate_preserves_original_case() {
        let (ty, _) = parse_type("MoodEnum").expect("valid");
        assert_eq!(ty, SqlColumnTypeName::Enum("MoodEnum".to_string()));
    }

    #[test]
    fn rejects_non_ident_leading_token() {
        for raw in ["123", "'text'", "(", ")"] {
            assert!(parse_type(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }
}
