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
use crate::catalog::{ArrayElemType, ArrayType, ColumnType, MAX_ARRAY_ELEMENTS};

/// 構文木を、カタログを参照せずに可能な範囲で `ColumnType` へ写した結果
/// （[`to_static_column_type`] の戻り値。Issue #1348）。
///
/// ENUM 型名の存在確認は実行段（`sql::ddl`。DDL 権限ゲートの後）でのみ行う契約の
/// ため、ENUM 候補は未解決のまま運ぶ。`CREATE TABLE` の構文検証
/// （`sql::allowlist`）は本型を使い、実行段（`sql::ddl::resolve_column_type`）が
/// 同じ変換表を共有する（変換表を 2 つ持たない）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StaticColumnType {
    /// カタログ参照なしで確定した型。
    Resolved(ColumnType),
    /// ENUM 型名の候補（実行段が `Storage::get_enum_type` で解決する）。
    EnumCandidate(String),
    /// 要素が ENUM 型名候補の配列（`<enum>[N]`。Issue #1357）。第 2 要素は要素数上限。
    /// 実行段（DDL 権限ゲートの後）が `Storage::get_enum_type` で語彙を解決して
    /// [`ArrayType::new_enum`] へ差し替える。
    ArrayOfEnumCandidate(String, u32),
}

/// 構文木 [`SqlColumnTypeName`] を [`StaticColumnType`] へ写す（カタログ非参照の純関数）。
///
/// - `VECTOR(N)` は `Resolved(Vector(N))`（採否は呼び出し元が決める。`ADD COLUMN` は
///   実行段で `0A000`、`CREATE TABLE` は従来どおり受理）
/// - 配列は要素型を写したうえで [`ArrayType::new`] へ渡す。`[]` は
///   `MAX_ARRAY_ELEMENTS`、範囲外の `[N]` は `42601`。要素が `VECTOR`・配列は `42601`。
///   NUMERIC・BYTEA・JSON・JSONB・ENUM 要素も受理する（Issue #1357）
/// - `ColumnType` から `ArrayElemType` への写像はワイルドカードを使わず網羅的に
///   書く（要素型が増えたときコンパイラが追従漏れを検出する）
pub(crate) fn to_static_column_type(
    ty: &SqlColumnTypeName,
) -> Result<StaticColumnType, SqlSurfaceError> {
    let resolved = match ty {
        SqlColumnTypeName::Text => ColumnType::Text,
        SqlColumnTypeName::Integer => ColumnType::Integer,
        SqlColumnTypeName::BigInt => ColumnType::BigInt,
        SqlColumnTypeName::Real => ColumnType::Real,
        SqlColumnTypeName::Double => ColumnType::Double,
        SqlColumnTypeName::Boolean => ColumnType::Boolean,
        SqlColumnTypeName::Date => ColumnType::Date,
        SqlColumnTypeName::Timestamp => ColumnType::Timestamp,
        SqlColumnTypeName::Bytea => ColumnType::Bytea,
        SqlColumnTypeName::Json => ColumnType::Json,
        SqlColumnTypeName::Jsonb => ColumnType::Jsonb,
        SqlColumnTypeName::Uuid => ColumnType::Uuid,
        SqlColumnTypeName::Numeric { precision, scale } => ColumnType::Numeric {
            precision: *precision,
            scale: *scale,
        },
        SqlColumnTypeName::Vector(dim) => ColumnType::Vector(*dim),
        SqlColumnTypeName::Enum(name) => return Ok(StaticColumnType::EnumCandidate(name.clone())),
        SqlColumnTypeName::Array { elem, max_len } => {
            let max_len = max_len.unwrap_or(MAX_ARRAY_ELEMENTS);
            let elem_ty = match to_static_column_type(elem)? {
                StaticColumnType::Resolved(t) => t,
                StaticColumnType::EnumCandidate(name) => {
                    // 上限の範囲だけは型名の存在確認より先に検証する（実行段の
                    // 解決を待たずに構文として確定できる誤りのため）。
                    ArrayType::new(ArrayElemType::Text, max_len).map_err(array_size_error)?;
                    return Ok(StaticColumnType::ArrayOfEnumCandidate(name, max_len));
                }
                StaticColumnType::ArrayOfEnumCandidate(..) => {
                    return Err(nested_array_error());
                }
            };
            let elem_ty = match elem_ty {
                ColumnType::Text => ArrayElemType::Text,
                ColumnType::Boolean => ArrayElemType::Bool,
                ColumnType::Integer => ArrayElemType::Integer,
                ColumnType::BigInt => ArrayElemType::BigInt,
                ColumnType::Real => ArrayElemType::Real,
                ColumnType::Double => ArrayElemType::Double,
                ColumnType::Date => ArrayElemType::Date,
                ColumnType::Timestamp => ArrayElemType::Timestamp,
                ColumnType::Uuid => ArrayElemType::Uuid,
                ColumnType::Numeric { precision, scale } => {
                    ArrayElemType::Numeric { precision, scale }
                }
                ColumnType::Bytea => ArrayElemType::Bytea,
                ColumnType::Json => ArrayElemType::Json,
                ColumnType::Jsonb => ArrayElemType::Jsonb,
                ColumnType::Vector(_) => {
                    return Err(SqlSurfaceError::unsupported(
                        "VECTOR cannot be an array element type",
                    ))
                }
                ColumnType::Array(_) => return Err(nested_array_error()),
                // ENUM 要素は上の `EnumCandidate` 腕で処理済み（型名の解決は実行段）。
                // `to_static_column_type` が `Resolved(Enum)` を返すことは無いが、
                // 到達した場合は拒否側（fail-closed）に倒す。
                ColumnType::Enum(_) => {
                    return Err(SqlSurfaceError::FeatureNotSupported {
                        detail: "this array element type is not supported yet".to_string(),
                    })
                }
            };
            let array = ArrayType::new(elem_ty, max_len).map_err(array_size_error)?;
            ColumnType::Array(array)
        }
    };
    Ok(StaticColumnType::Resolved(resolved))
}

fn nested_array_error() -> SqlSurfaceError {
    SqlSurfaceError::unsupported("nested or multi-dimensional array types are not supported")
}

/// 配列の要素数上限が範囲外（`catalog::ArrayType::new` の拒否）を `42601` へ写す。
fn array_size_error(e: crate::catalog::CatalogError) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(format!("invalid array type: {e}"))
}

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
/// 配列型（`<型>[]`・`<型>[N]`。TABLE-14・Issue #1348）は [`Self::Array`] で表す。
/// 多次元（`<型>[][]`）は構文段階で `42601`。`[N]` の `N` は書き込み時の要素数上限
/// （`catalog::ArrayType::max_len`）として実行段が `1..=MAX_ARRAY_ELEMENTS` を検証する
/// （PostgreSQL と異なり、サイズ指定は無視されず超過書き込みが `54000` になる）。
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
    /// `<型>[]`／`<型>[N]`（Issue #1348）。`max_len` が `None` は `[]`（実行段が
    /// `MAX_ARRAY_ELEMENTS` を採用）。要素型が配列・`VECTOR` の場合は実行段が拒否する
    /// （構文段では入れ子を作らない）。
    Array {
        elem: Box<SqlColumnTypeName>,
        max_len: Option<u32>,
    },
}

/// `tokens[*pos..]` の先頭から型名 1 個を読み取り、消費した分だけ `*pos` を
/// 進める。`sql::allowlist::Parser::parse_alter_table_add_column` が自身の
/// `tokens`／`pos` を共有して呼ぶ。
pub(crate) fn parse_column_type_name(
    tokens: &[Token],
    pos: &mut usize,
) -> Result<SqlColumnTypeName, SqlSurfaceError> {
    let base = parse_base_type_name(tokens, pos)?;
    // 配列サフィックス `[]`／`[N]`（高々 1 個。2 個目の `[` は呼び出し元の余剰
    // トークン判定が `42601` で拒否するため、ここでは明示的に拒否する）。
    if !matches!(tokens.get(*pos), Some(Token::Punct('['))) {
        return Ok(base);
    }
    *pos += 1;
    let max_len = if matches!(tokens.get(*pos), Some(Token::Number(_))) {
        Some(expect_strict_u32(tokens, pos)?)
    } else {
        None
    };
    expect_punct(tokens, pos, ']')?;
    if matches!(tokens.get(*pos), Some(Token::Punct('['))) {
        return Err(SqlSurfaceError::unsupported(
            "multi-dimensional array types are not supported",
        ));
    }
    Ok(SqlColumnTypeName::Array {
        elem: Box::new(base),
        max_len,
    })
}

/// 配列サフィックスを除いた基本型名 1 個の解析（[`parse_column_type_name`] の下請け）。
fn parse_base_type_name(
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
    fn accepts_array_suffix_with_and_without_size() {
        let (ty, consumed) = parse_type("INTEGER[]").expect("valid");
        assert_eq!(
            ty,
            SqlColumnTypeName::Array {
                elem: Box::new(SqlColumnTypeName::Integer),
                max_len: None
            }
        );
        assert_eq!(consumed, 3);
        let (ty, consumed) = parse_type("text[3]").expect("valid");
        assert_eq!(
            ty,
            SqlColumnTypeName::Array {
                elem: Box::new(SqlColumnTypeName::Text),
                max_len: Some(3)
            }
        );
        assert_eq!(consumed, 4);
        // 範囲（0・上限超）の検証は実行段の責務。構文上は受理する。
        assert!(parse_type("TEXT[0]").is_ok());
        let (ty, _) = parse_type("mood[]").expect("valid");
        assert!(matches!(ty, SqlColumnTypeName::Array { .. }));
    }

    #[test]
    fn rejects_malformed_array_suffix() {
        for raw in [
            "INTEGER[][]",
            "INTEGER[3][]",
            "INTEGER[-1]",
            "INTEGER[01]",
            "INTEGER[1.5]",
            "INTEGER[4294967296]",
            "INTEGER[",
            "INTEGER[3",
            "INTEGER[a]",
            "INTEGER]",
        ] {
            // `INTEGER]` は型名として読めても `]` が残る。残余の拒否は呼び出し元の責務。
            let mut pos = 0;
            let tokens = crate::sql::lexer::tokenize(raw).expect("tokenize");
            let r = parse_column_type_name(&tokens, &mut pos);
            let rejected = r.is_err() || pos < tokens.len();
            assert!(rejected, "expected {raw:?} to be rejected");
        }
    }

    #[test]
    fn rejects_non_ident_leading_token() {
        for raw in ["123", "'text'", "(", ")"] {
            assert!(parse_type(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }
}
