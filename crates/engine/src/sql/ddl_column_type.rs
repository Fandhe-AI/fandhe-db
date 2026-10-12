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
    to_static_column_type_with_array_default(ty, MAX_ARRAY_ELEMENTS)
}

/// [`to_static_column_type`] の `[]`（要素数省略）の既定要素数を引数化した版
/// （Issue #1548）。実行段（`sql::ddl`）が起動時設定 `--max-array-elements`
/// （`schema_limits::SchemaLimits::max_array_elements`）を渡す。明示 `[N]` は
/// 本関数では従来どおり `1..=MAX_ARRAY_ELEMENTS` だけを検証し、設定値との比較は
/// 呼び出し元（`sql::ddl`）が行う。既定値がハード上限なら
/// [`to_static_column_type`] と同一結果。
pub(crate) fn to_static_column_type_with_array_default(
    ty: &SqlColumnTypeName,
    default_max_len: u32,
) -> Result<StaticColumnType, SqlSurfaceError> {
    let resolved = match ty {
        SqlColumnTypeName::Text => ColumnType::Text,
        SqlColumnTypeName::Varchar => ColumnType::Varchar,
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
        SqlColumnTypeName::Alias { target, .. } => {
            return to_static_column_type_with_array_default(target, default_max_len)
        }
        SqlColumnTypeName::Enum(name) => return Ok(StaticColumnType::EnumCandidate(name.clone())),
        SqlColumnTypeName::Array { elem, max_len } => {
            let max_len = max_len.unwrap_or(default_max_len);
            let elem_ty = match to_static_column_type_with_array_default(elem, default_max_len)? {
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
                // `varchar[]` は別 OID（1015）のため Text へ写さず拒否する（Issue #2305。
                // 写すと型の区別が失われ、後で受理へ広げたときに不可逆になる）。
                ColumnType::Varchar => {
                    return Err(SqlSurfaceError::FeatureNotSupported {
                        detail: "this array element type is not supported yet".to_string(),
                    })
                }
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
    /// 長さなしの `VARCHAR`／`CHARACTER VARYING`（TABLE-31・Issue #2305）。
    /// `VARCHAR(n)` は構文段で `0A000` のため本 variant には到達しない。
    Varchar,
    Integer,
    BigInt,
    Real,
    /// `DOUBLE PRECISION`（2 語）または別名 `FLOAT8`。
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
    /// 型名エイリアス `INT4`・`INT8`・`FLOAT4`・`FLOAT8`（Issue #1528）。`target` は写し先の
    /// 組み込み型で、`name` は原文の綴り。作成経路は同名 ENUM の新規作成を拒否するが、
    /// 本変更前から同名 ENUM が存在し得るため、実行段が `name` で既存 ENUM との衝突を
    /// 検出して拒否する（組み込み型へ黙って解決しない。fail-closed）。
    Alias {
        name: String,
        target: Box<SqlColumnTypeName>,
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

/// 型名エイリアスの構文木を組み立てる（[`parse_base_type_name`] の下請け）。
fn alias_type(name: String, target: SqlColumnTypeName) -> SqlColumnTypeName {
    SqlColumnTypeName::Alias {
        name,
        target: Box::new(target),
    }
}

/// 型名（配列要素を含む）に現れる型名エイリアスの原文の綴りを返す。実行段が既存 ENUM
/// との衝突検出に使う（Issue #1528。`sql::ddl` の `CREATE TABLE`・`ADD COLUMN`・
/// `ALTER COLUMN TYPE` から呼ばれる）。
pub(crate) fn alias_spelling(ty: &SqlColumnTypeName) -> Option<&str> {
    match ty {
        SqlColumnTypeName::Alias { name, .. } => Some(name.as_str()),
        SqlColumnTypeName::Array { elem, .. } => alias_spelling(elem),
        _ => None,
    }
}

/// 型名解決表の 1 行の扱い（[`TYPE_NAME_TABLE`]。Issue #2303）。
///
/// 正規名・別名を区別せず同じ表に並べ、行ごとに「受理する形」か「ID が未確定のため
/// `0A000` で拒否する」かを持たせる。後続 Issue（#2304〜#2306）は行の追加・扱いの
/// 切り替えだけで済む。`match` はワイルドカードなしで網羅し、種別を足したときに
/// コンパイラが追従漏れを検出する。
#[derive(Debug, Clone, Copy)]
enum TypeNameKind {
    /// 綴りを保持せず写す（`INT` を含む。構文木は従来どおり plain）。
    Direct(fn() -> SqlColumnTypeName),
    /// 綴りを保持する別名（実行段が同名 ENUM との衝突検出に使う。Issue #1528）。
    Alias(fn() -> SqlColumnTypeName),
    /// `DOUBLE PRECISION`（2 語必須。`DOUBLE` 単独は構文段 `42704`。Issue #2306）。
    DoublePrecision,
    /// `VECTOR(N)`。
    Vector,
    /// `NUMERIC(p,s)`／`DECIMAL(p,s)`。
    Numeric,
    /// `VARCHAR`。長さなしなら `ColumnType::Varchar` として受理し、直後が `(` の
    /// 長さ指定付きは `0A000`（型修飾子と長さ検査は後続フェーズ。Issue #2305）。
    /// 綴りを保持する別名として返す（同名 ENUM との衝突検出に使う。Issue #1528）。
    Varchar,
    /// 定める ID が未確定のため受理しない型。`canonical` は正規名で、綴りに依らず
    /// 同一の `0A000` メッセージを組み立てるために使う（Issue #2303）。
    Unsettled { canonical: &'static str },
    /// `CHARACTER [VARYING]` の先頭語。直後が `VARYING` なら `VARCHAR` と同じ扱い
    /// （長さなしは受理、`(` 付きは `0A000`）、`VARYING` なしの `CHARACTER` は未確定のため
    /// `0A000` で拒否する。
    Character,
}

/// 型名解決表。先頭語（大小無視）で引く。表に無い識別子は ENUM 型名候補
/// （実行段で未登録なら `42704`。Issue #2306）。ポインタ: TABLE-29・TABLE-30・TABLE-31。
/// 全行の先頭語は `allowlist::CREATE_TABLE_COLUMN_TYPE_KEYWORDS` にも載せる契約
/// （単体テストがドリフトを検出する）。
const TYPE_NAME_TABLE: &[(&str, TypeNameKind)] = &[
    ("TEXT", TypeNameKind::Direct(|| SqlColumnTypeName::Text)),
    (
        "INTEGER",
        TypeNameKind::Direct(|| SqlColumnTypeName::Integer),
    ),
    ("INT", TypeNameKind::Direct(|| SqlColumnTypeName::Integer)),
    ("INT4", TypeNameKind::Alias(|| SqlColumnTypeName::Integer)),
    ("BIGINT", TypeNameKind::Direct(|| SqlColumnTypeName::BigInt)),
    ("INT8", TypeNameKind::Alias(|| SqlColumnTypeName::BigInt)),
    ("REAL", TypeNameKind::Direct(|| SqlColumnTypeName::Real)),
    ("FLOAT4", TypeNameKind::Alias(|| SqlColumnTypeName::Real)),
    ("FLOAT8", TypeNameKind::Alias(|| SqlColumnTypeName::Double)),
    ("DOUBLE", TypeNameKind::DoublePrecision),
    (
        "BOOLEAN",
        TypeNameKind::Direct(|| SqlColumnTypeName::Boolean),
    ),
    ("DATE", TypeNameKind::Direct(|| SqlColumnTypeName::Date)),
    (
        "TIMESTAMP",
        TypeNameKind::Direct(|| SqlColumnTypeName::Timestamp),
    ),
    ("BYTEA", TypeNameKind::Direct(|| SqlColumnTypeName::Bytea)),
    ("JSON", TypeNameKind::Direct(|| SqlColumnTypeName::Json)),
    ("JSONB", TypeNameKind::Direct(|| SqlColumnTypeName::Jsonb)),
    ("UUID", TypeNameKind::Direct(|| SqlColumnTypeName::Uuid)),
    ("VECTOR", TypeNameKind::Vector),
    ("NUMERIC", TypeNameKind::Numeric),
    ("DECIMAL", TypeNameKind::Numeric),
    (
        "INT2",
        TypeNameKind::Unsettled {
            canonical: "smallint",
        },
    ),
    (
        "SMALLINT",
        TypeNameKind::Unsettled {
            canonical: "smallint",
        },
    ),
    ("VARCHAR", TypeNameKind::Varchar),
    (
        "CHAR",
        TypeNameKind::Unsettled {
            canonical: "character",
        },
    ),
    ("CHARACTER", TypeNameKind::Character),
];

/// 未確定型の `0A000`。メッセージは正規名のみで組み立て、綴り（別名）に依らず同一にする。
fn unsettled_type_error(canonical: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: format!("type {canonical} is not supported yet"),
    }
}

/// `VARCHAR`／`CHARACTER VARYING` の直後に `(`（長さ指定）が続く場合の `0A000`
/// （Issue #2305。メッセージは両綴りで同一）。
fn reject_length_modifier(tokens: &[Token], pos: usize) -> Result<(), SqlSurfaceError> {
    if matches!(tokens.get(pos), Some(Token::Punct('('))) {
        return Err(unsettled_type_error("character varying"));
    }
    Ok(())
}

/// 配列サフィックスを除いた基本型名 1 個の解析（[`parse_column_type_name`] の下請け）。
///
/// 型名 → 列型の対応は [`TYPE_NAME_TABLE`] に一本化されており、`CREATE TABLE`・
/// `ADD COLUMN`・`ALTER COLUMN TYPE`・配列要素が共有する（ポインタ: TABLE-29・
/// TABLE-30・TABLE-31、Issue #1528・#2303）。未確定型（`INT2`／`SMALLINT`・`VARCHAR`／
/// `CHARACTER VARYING`・`CHAR`／`CHARACTER`）はカタログを参照しない静的判定で、構文段で
/// 両綴りとも同一の `0A000` を返す（書き込みトランザクション開始前のため副作用なし）。
/// `DOUBLE` 単独は構文段 `42704`。表に無い識別子は ENUM 型名候補として
/// 実行段で解決し、未登録は `42704`（Issue #2306）。
fn parse_base_type_name(
    tokens: &[Token],
    pos: &mut usize,
) -> Result<SqlColumnTypeName, SqlSurfaceError> {
    let name = expect_ident(tokens, pos)?;
    let Some((_, kind)) = TYPE_NAME_TABLE
        .iter()
        .find(|(word, _)| name.eq_ignore_ascii_case(word))
    else {
        // 表に無い識別子は ENUM 型名候補として構文上は受理する（原文の大文字小文字
        // をそのまま保持する。ENUM 型名は `catalog::validate_identifier` の
        // 識別子規則に従うため大文字小文字を区別する）。
        return Ok(SqlColumnTypeName::Enum(name));
    };
    match kind {
        TypeNameKind::Direct(make) => Ok(make()),
        TypeNameKind::Alias(make) => Ok(alias_type(name, make())),
        TypeNameKind::DoublePrecision => {
            // `DOUBLE` 単独（`PRECISION` が続かない）は未定義の型名として `42704`
            // （Issue #2306）。次トークンは消費せず、カタログ非依存の静的判定で返す。
            match tokens.get(*pos) {
                Some(Token::Ident(s)) if s.eq_ignore_ascii_case("PRECISION") => {
                    *pos += 1;
                    Ok(SqlColumnTypeName::Double)
                }
                _ => Err(SqlSurfaceError::UndefinedType { name }),
            }
        }
        TypeNameKind::Vector => {
            expect_punct(tokens, pos, '(')?;
            let dim = expect_strict_u32(tokens, pos)?;
            expect_punct(tokens, pos, ')')?;
            Ok(SqlColumnTypeName::Vector(dim))
        }
        TypeNameKind::Numeric => {
            expect_punct(tokens, pos, '(')?;
            let precision = expect_strict_u8(tokens, pos)?;
            expect_punct(tokens, pos, ',')?;
            let scale = expect_strict_u8(tokens, pos)?;
            expect_punct(tokens, pos, ')')?;
            Ok(SqlColumnTypeName::Numeric { precision, scale })
        }
        TypeNameKind::Unsettled { canonical } => Err(unsettled_type_error(canonical)),
        TypeNameKind::Varchar => {
            reject_length_modifier(tokens, *pos)?;
            Ok(alias_type(name, SqlColumnTypeName::Varchar))
        }
        TypeNameKind::Character => {
            let varying = matches!(
                tokens.get(*pos),
                Some(Token::Ident(s)) if s.eq_ignore_ascii_case("VARYING")
            );
            if !varying {
                return Err(unsettled_type_error("character"));
            }
            *pos += 1;
            reject_length_modifier(tokens, *pos)?;
            Ok(SqlColumnTypeName::Varchar)
        }
    }
}

fn expect_ident(tokens: &[Token], pos: &mut usize) -> Result<String, SqlSurfaceError> {
    match tokens.get(*pos) {
        Some(Token::Ident(s) | Token::QuotedNull(s)) => {
            *pos += 1;
            Ok(s.clone())
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected a type name, got {other:?}"
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
            ("INT", SqlColumnTypeName::Integer),
            ("int", SqlColumnTypeName::Integer),
            (
                "Int4",
                alias_type("Int4".to_string(), SqlColumnTypeName::Integer),
            ),
            ("BIGINT", SqlColumnTypeName::BigInt),
            (
                "INT8",
                alias_type("INT8".to_string(), SqlColumnTypeName::BigInt),
            ),
            (
                "int8",
                alias_type("int8".to_string(), SqlColumnTypeName::BigInt),
            ),
            ("REAL", SqlColumnTypeName::Real),
            (
                "FLOAT4",
                alias_type("FLOAT4".to_string(), SqlColumnTypeName::Real),
            ),
            (
                "float4",
                alias_type("float4".to_string(), SqlColumnTypeName::Real),
            ),
            (
                "FLOAT8",
                alias_type("FLOAT8".to_string(), SqlColumnTypeName::Double),
            ),
            (
                "Float8",
                alias_type("Float8".to_string(), SqlColumnTypeName::Double),
            ),
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
    fn aliases_never_fall_through_to_enum() {
        // 実行段の衝突検出に使う綴りを保持する（Issue #1528）。
        let (ty, _) = parse_type("Int4").expect("valid");
        assert_eq!(alias_spelling(&ty), Some("Int4"));
        for raw in ["INT", "INT4", "INT8", "FLOAT4", "FLOAT8"] {
            let (ty, consumed) = parse_type(raw).expect("valid");
            assert!(!matches!(ty, SqlColumnTypeName::Enum(_)), "for {raw:?}");
            assert_eq!(consumed, 1, "for {raw:?}");
        }
    }

    #[test]
    fn alias_array_elements_resolve() {
        let (ty, _) = parse_type("INT4[]").expect("valid");
        assert!(matches!(
            ty,
            SqlColumnTypeName::Array { ref elem, .. } if alias_spelling(elem) == Some("INT4")
        ));
        let (ty, _) = parse_type("INT8[3]").expect("valid");
        assert!(matches!(
            ty,
            SqlColumnTypeName::Array { ref elem, max_len: Some(3) } if alias_spelling(elem) == Some("INT8")
        ));
    }

    #[test]
    fn unsettled_type_names_are_rejected_with_0a000_in_both_spellings() {
        for raw in [
            "INT2",
            "int2",
            "SMALLINT",
            "smallint",
            "VARCHAR(10)",
            "varchar (10)",
            "CHARACTER VARYING(10)",
            "character varying(10)",
            "VARCHAR(10)[]",
            "CHAR",
            "CHAR(1)",
            "CHARACTER",
            "CHARACTER(3)",
            "INT2[]",
            "SMALLINT[3]",
        ] {
            let e = parse_type(raw).expect_err(raw);
            assert_eq!(e.wire_code(), "0A000", "for {raw:?}");
        }
        for (a, b) in [
            ("INT2", "SMALLINT"),
            ("VARCHAR(10)", "CHARACTER VARYING(10)"),
            ("CHAR(1)", "CHARACTER(1)"),
        ] {
            let (ea, eb) = (parse_type(a).unwrap_err(), parse_type(b).unwrap_err());
            assert_eq!(ea.wire_code(), eb.wire_code());
            assert_eq!(ea.to_string(), eb.to_string(), "{a} vs {b}");
        }
    }

    /// 長さなしの `VARCHAR`／`CHARACTER VARYING` は `Varchar` として受理する
    /// （Issue #2305）。`VARCHAR` 綴りは ENUM 衝突検出のため Alias で返す。
    #[test]
    fn unbounded_varchar_spellings_are_accepted() {
        let (ty, pos) = parse_type("VARCHAR").expect("valid");
        assert_eq!(pos, 1);
        assert_eq!(alias_spelling(&ty), Some("VARCHAR"));
        assert_eq!(
            to_static_column_type(&ty).expect("static"),
            StaticColumnType::Resolved(ColumnType::Varchar)
        );
        for raw in ["CHARACTER VARYING", "character varying"] {
            let (ty, pos) = parse_type(raw).expect(raw);
            assert_eq!(pos, 2, "for {raw:?}");
            assert_eq!(ty, SqlColumnTypeName::Varchar, "for {raw:?}");
        }
    }

    /// `varchar[]` は別 OID のため配列要素としては受理せず `0A000`（Issue #2305）。
    #[test]
    fn varchar_array_is_rejected_with_0a000() {
        for raw in ["VARCHAR[]", "CHARACTER VARYING[3]"] {
            let (ty, _) = parse_type(raw).expect(raw);
            let e = to_static_column_type(&ty).expect_err(raw);
            assert_eq!(e.wire_code(), "0A000", "for {raw:?}");
        }
    }

    #[test]
    fn truly_unknown_names_stay_enum_candidates() {
        for raw in ["INT16", "mood", "MoodEnum"] {
            let (ty, _) = parse_type(raw).expect("valid");
            assert_eq!(ty, SqlColumnTypeName::Enum(raw.to_string()), "for {raw:?}");
        }
    }

    /// 解決表の全行の先頭語が予約語リストに載っている（二重管理のドリフト検出）。
    #[test]
    fn type_name_table_words_are_column_type_keywords_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for (word, _) in TYPE_NAME_TABLE {
            assert!(
                crate::sql::allowlist::is_column_type_keyword(word),
                "{word} missing from CREATE_TABLE_COLUMN_TYPE_KEYWORDS"
            );
            assert!(seen.insert(*word), "duplicate row {word}");
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
        for sql in ["DOUBLE", "double", "DOUBLE FLOAT"] {
            let err = parse_type(sql).expect_err("must reject");
            assert_eq!(err.wire_code(), "42704", "{sql}");
        }
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
