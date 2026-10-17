//! `CAST` の値変換（Issue #1591・#1592・SQL-47 のポインタ）。
//!
//! 責務境界: 束縛段（`sql::cast::bind_cast`）が規則表から [`CastFn`]（Copy な変換記述子）を
//! 組み立てて `BuiltinFn::Cast` に載せ、評価段（`sql::udf_call::apply_builtin`）が
//! [`apply_cast`] を呼ぶ。型なし文字列リテラルの束縛（`bind_string_literal_as`）も同じ
//! 入力関数を束縛時に 1 度呼んで形式不正・範囲外を早期に報告する（第 2 の文法を作らない）。
//!
//! 変換は 2 系統に分かれる。数値 6 型・BOOLEAN・文字列 3 型どうし（Issue #1591）は
//! [`convert_basic`]、日付時刻・JSON・UUID・BYTEA・配列が絡むもの（Issue #1592）は
//! [`apply_cast_ext`] が担う（jsonb → 数値・BOOLEAN は抽出した値を [`convert_basic`] へ流して
//! 丸め・範囲検査を共有する）。
//!
//! 規則の拠り所は `docs/design/cast-conversion-rules.md`（PostgreSQL 17 の公開挙動）。
//! - 整数への変換: 浮動小数は偶数丸め、numeric は 0 から遠い側への四捨五入の後に範囲検査（`22003`）。
//! - 文字列からの変換: 形式不正は `22P02`、範囲外は `22003`。本エンジンが値として持たない
//!   `NaN`／`Infinity` 入力は `0A000`（fail-closed。`22P02` にもしない）。
//! - 日付時刻: 形式不正は `22007`、範囲外は `22008`／`22015`。timestamptz が絡む変換と文字列化は
//!   評価時のセッション `TimeZone` に従う。
//! - 式評価は整数も f64 で運ぶため、|v| > 2^53 の int8 は丸めずに `22003` で拒否する
//!   （PostgreSQL との既知の差分。設計書に記録）。
//!
//! エラー文言は固定語彙＋呼び出し元が与えた値（先頭 [`ECHO_LIMIT`] 文字まで）に限り、
//! 他テナントの値・存在情報を含めない（CAST は RLS を通過した可視行の値にだけ作用する）。

use std::borrow::Cow;

use crate::catalog::{ArrayElemType, ArrayType, MAX_ARRAY_ELEMENTS, MAX_CHAR_TYPE_LENGTH};
use crate::numeric::{self, Decimal, NumericError};
use crate::row_codec::ArrayValue;
use crate::scalar_float::{format_double, format_real};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::udf_call::ExprValue;

/// エラー文言へ埋め込む入力値の最大文字数（表示専用。長大な入力でエラー文が膨らまない）。
const ECHO_LIMIT: usize = 64;

/// 式評価が整数を f64 で正確に運べる上限（2^53）。
const EXACT_INT_MAX: i64 = 1 << 53;

/// 変換元の型（規則表の見出しから文字列 3 型を [`CastSrc::Text`] にまとめた集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastSrc {
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Bool,
    Text,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    /// `json`（原文保存。式の型は `ExprType::JsonText`）。
    Json,
    /// `jsonb`（正規化テキスト。式の型は `ExprType::Json`）。
    Jsonb,
    Uuid,
    Bytea,
    /// 配列。要素型を `Copy` な `ArrayElemType` で運び、要素の変換記述子は
    /// （変換元の要素型, 変換先の要素型）から導く（`CastFn` を再帰させない）。
    Array(ArrayElemType),
}

/// 変換先の型。型修飾子は値の変換（丸め・切り詰め・空白埋め）を伴うため記述子へ含める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastDst {
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    /// `None` は修飾子なし（任意精度の numeric。丸めない）。
    Numeric(Option<(u8, u8)>),
    Bool,
    Text,
    /// `None` は長さ修飾なし。
    Varchar(Option<u32>),
    Bpchar(u32),
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Json,
    Jsonb,
    Uuid,
    Bytea,
    /// 配列（要素型は [`CastSrc::Array`] と同じ運び方。`varchar(n)[]` 等の対応要素型の無い
    /// 変換先は束縛段が `0A000` にするため、ここへは来ない）。
    Array(ArrayElemType),
}

impl CastSrc {
    /// 数値 6 型・BOOLEAN・文字列（Issue #1591 の範囲）か。
    fn is_basic(self) -> bool {
        matches!(
            self,
            CastSrc::Int2
                | CastSrc::Int4
                | CastSrc::Int8
                | CastSrc::Float4
                | CastSrc::Float8
                | CastSrc::Numeric
                | CastSrc::Bool
                | CastSrc::Text
        )
    }
}

impl CastDst {
    /// 数値 6 型・BOOLEAN・文字列（Issue #1591 の範囲）か。
    fn is_basic(self) -> bool {
        matches!(
            self,
            CastDst::Int2
                | CastDst::Int4
                | CastDst::Int8
                | CastDst::Float4
                | CastDst::Float8
                | CastDst::Numeric(_)
                | CastDst::Bool
                | CastDst::Text
                | CastDst::Varchar(_)
                | CastDst::Bpchar(_)
        )
    }
}

/// 配列の要素型から、要素の変換元の記述子を導く（`enum` 要素は対象外で `None`）。
pub(crate) fn elem_cast_src(e: ArrayElemType) -> Option<CastSrc> {
    Some(match e {
        ArrayElemType::Text => CastSrc::Text,
        ArrayElemType::Bool => CastSrc::Bool,
        ArrayElemType::Integer => CastSrc::Int4,
        ArrayElemType::BigInt => CastSrc::Int8,
        ArrayElemType::Real => CastSrc::Float4,
        ArrayElemType::Double => CastSrc::Float8,
        ArrayElemType::Date => CastSrc::Date,
        ArrayElemType::Timestamp => CastSrc::Timestamp,
        ArrayElemType::Uuid => CastSrc::Uuid,
        ArrayElemType::Numeric { .. } => CastSrc::Numeric,
        ArrayElemType::Bytea => CastSrc::Bytea,
        ArrayElemType::Json => CastSrc::Json,
        ArrayElemType::Jsonb => CastSrc::Jsonb,
        ArrayElemType::Enum => return None,
    })
}

/// 配列の要素型から、要素の変換先の記述子を導く（`numeric(p,s)` は修飾子つき）。
pub(crate) fn elem_cast_dst(e: ArrayElemType) -> Option<CastDst> {
    Some(match e {
        ArrayElemType::Text => CastDst::Text,
        ArrayElemType::Bool => CastDst::Bool,
        ArrayElemType::Integer => CastDst::Int4,
        ArrayElemType::BigInt => CastDst::Int8,
        ArrayElemType::Real => CastDst::Float4,
        ArrayElemType::Double => CastDst::Float8,
        ArrayElemType::Date => CastDst::Date,
        ArrayElemType::Timestamp => CastDst::Timestamp,
        ArrayElemType::Uuid => CastDst::Uuid,
        ArrayElemType::Numeric { precision, scale } => CastDst::Numeric(Some((precision, scale))),
        ArrayElemType::Bytea => CastDst::Bytea,
        ArrayElemType::Json => CastDst::Json,
        ArrayElemType::Jsonb => CastDst::Jsonb,
        ArrayElemType::Enum => return None,
    })
}

/// `BuiltinFn::Cast` が持つ変換記述子。`BuiltinFn` が `Copy` のため `CastType`（`Box` を含む）
/// ではなく、この小さな Copy 型で運ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CastFn {
    pub from: CastSrc,
    pub to: CastDst,
}

impl CastFn {
    /// 値を変えない変換（束縛段は変換ノードを作らず被演算子を素通しする）。
    pub fn is_noop(self) -> bool {
        use CastDst as D;
        use CastSrc as S;
        matches!(
            (self.from, self.to),
            (S::Int2, D::Int2 | D::Int4 | D::Int8 | D::Float8)
                | (S::Int4, D::Int4 | D::Int8 | D::Float8)
                | (S::Int8, D::Int8 | D::Float8)
                | (S::Float4, D::Float4 | D::Float8)
                | (S::Float8, D::Float8)
                | (S::Numeric, D::Numeric(None))
                | (S::Bool, D::Bool)
                | (S::Text, D::Text | D::Varchar(None))
                | (S::Date, D::Date)
                | (S::Time, D::Time)
                | (S::Timestamp, D::Timestamp)
                | (S::TimestampTz, D::TimestampTz)
                | (S::Interval, D::Interval)
                | (S::Json, D::Json)
                | (S::Jsonb, D::Jsonb)
                | (S::Uuid, D::Uuid)
                | (S::Bytea, D::Bytea)
        ) || matches!((self.from, self.to), (S::Array(a), D::Array(b)) if a == b)
    }
}

fn echo(s: &str) -> String {
    s.chars().take(ECHO_LIMIT).collect()
}

fn out_of_range(detail: impl Into<String>) -> SqlSurfaceError {
    SqlSurfaceError::numeric_out_of_range(detail)
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

fn syntax_error(type_name: &str, s: &str) -> SqlSurfaceError {
    SqlSurfaceError::invalid_text_representation(format!(
        "invalid input syntax for type {type_name}: \"{}\"",
        echo(s)
    ))
}

fn non_finite_unsupported(type_name: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: format!("NaN and Infinity are not supported as input for type {type_name}"),
    }
}

/// 整数の幅（`Int2`／`Int4`／`Int8`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntWidth {
    W16,
    W32,
    W64,
}

impl IntWidth {
    fn name(self) -> &'static str {
        match self {
            IntWidth::W16 => "smallint",
            IntWidth::W32 => "integer",
            IntWidth::W64 => "bigint",
        }
    }

    fn min(self) -> i64 {
        match self {
            IntWidth::W16 => i64::from(i16::MIN),
            IntWidth::W32 => i64::from(i32::MIN),
            IntWidth::W64 => i64::MIN,
        }
    }

    fn max(self) -> i64 {
        match self {
            IntWidth::W16 => i64::from(i16::MAX),
            IntWidth::W32 => i64::from(i32::MAX),
            IntWidth::W64 => i64::MAX,
        }
    }

    fn from_dst(dst: CastDst) -> Option<Self> {
        match dst {
            CastDst::Int2 => Some(IntWidth::W16),
            CastDst::Int4 => Some(IntWidth::W32),
            CastDst::Int8 => Some(IntWidth::W64),
            _ => None,
        }
    }
}

/// 整数値を幅の範囲で検査して返す。int8 は式評価が f64 で運ぶため 2^53 を超える値も拒否する。
fn finish_int(v: i128, w: IntWidth) -> Result<i64, SqlSurfaceError> {
    let range_err = || out_of_range(format!("{} out of range", w.name()));
    let v = i64::try_from(v).map_err(|_| range_err())?;
    if v < w.min() || v > w.max() {
        return Err(range_err());
    }
    if w == IntWidth::W64 && v.unsigned_abs() > EXACT_INT_MAX as u64 {
        return Err(out_of_range(
            "bigint value exceeds the range this engine can evaluate exactly",
        ));
    }
    Ok(v)
}

/// 文字列 → 整数の失敗理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntTextError {
    Syntax,
    Range,
}

/// PostgreSQL 17 の `pg_strtoint*` と同じ文法で文字列を整数にする。前後の ASCII 空白を除き、
/// 符号 `+`／`-`、接頭辞 `0x`／`0o`／`0b`、数字の間の単独の `_` を受け付ける。`1.5`・空文字・
/// 空白のみは構文違反。範囲外は走査の途中で検出する（PostgreSQL と同じく、以降の文字が
/// 不正でも範囲外を優先する）。
fn parse_pg_int(s: &str, w: IntWidth) -> Result<i64, IntTextError> {
    let t = s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'));
    let b = t.as_bytes();
    let mut i = 0usize;
    let mut neg = false;
    match b.first() {
        Some(b'-') => {
            neg = true;
            i += 1;
        }
        Some(b'+') => i += 1,
        _ => {}
    }
    let mut radix: u64 = 10;
    if b.get(i) == Some(&b'0') {
        match b.get(i + 1) {
            Some(b'x' | b'X') => radix = 16,
            Some(b'o' | b'O') => radix = 8,
            Some(b'b' | b'B') => radix = 2,
            _ => {}
        }
        if radix != 10 {
            i += 2;
        }
    }
    // 大きさの上限（負は 2^(w-1)、正は 2^(w-1)-1）。
    let max_mag: u64 = if neg {
        w.min().unsigned_abs()
    } else {
        w.max().unsigned_abs()
    };
    let digit_of = |c: u8| -> Option<u64> {
        let d = match c {
            b'0'..=b'9' => u64::from(c - b'0'),
            b'a'..=b'f' => u64::from(c - b'a') + 10,
            b'A'..=b'F' => u64::from(c - b'A') + 10,
            _ => return None,
        };
        (d < radix).then_some(d)
    };
    let mut acc: u64 = 0;
    let mut digits = 0usize;
    while let Some(&c) = b.get(i) {
        if let Some(d) = digit_of(c) {
            acc = acc
                .checked_mul(radix)
                .and_then(|v| v.checked_add(d))
                .filter(|v| *v <= max_mag)
                .ok_or(IntTextError::Range)?;
            digits += 1;
            i += 1;
        } else if c == b'_' {
            // 単独の `_` は数字の間にだけ置ける（先頭・連続・末尾は構文違反）。
            let prev_is_digit =
                digits > 0 && i > 0 && b.get(i - 1).and_then(|p| digit_of(*p)).is_some();
            let next_is_digit = b.get(i + 1).and_then(|n| digit_of(*n)).is_some();
            if !prev_is_digit || !next_is_digit {
                return Err(IntTextError::Syntax);
            }
            i += 1;
        } else {
            break;
        }
    }
    if digits == 0 || i != b.len() {
        return Err(IntTextError::Syntax);
    }
    let v = i128::from(acc);
    let v = if neg { -v } else { v };
    i64::try_from(v).map_err(|_| IntTextError::Range)
}

fn text_to_int(s: &str, w: IntWidth) -> Result<i64, SqlSurfaceError> {
    match parse_pg_int(s, w) {
        Ok(v) => finish_int(i128::from(v), w),
        Err(IntTextError::Syntax) => Err(syntax_error(w.name(), s)),
        Err(IntTextError::Range) => Err(out_of_range(format!(
            "value \"{}\" is out of range for type {}",
            echo(s),
            w.name()
        ))),
    }
}

/// 文字列 → 浮動小数の失敗理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatTextError {
    Syntax,
    Range,
    /// `NaN`／`Infinity` 系（本エンジンは非有限値を持たない）。
    NonFinite,
}

fn is_non_finite_word(t: &str) -> bool {
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    ["nan", "inf", "infinity"]
        .iter()
        .any(|w| body.eq_ignore_ascii_case(w))
}

/// 浮動小数の入力文法（前後の空白を除き、符号・`.5`・`1.`・指数を受け付ける）を検査し、
/// 仮数が 0 かどうかを返す。16 進表記は構文違反として拒否する（既知の差分）。
fn scan_float_grammar(t: &str) -> Result<bool, FloatTextError> {
    let b = t.as_bytes();
    let mut i = 0usize;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let mut digits = 0usize;
    let mut nonzero = false;
    while let Some(c) = b.get(i).filter(|c| c.is_ascii_digit()) {
        digits += 1;
        nonzero |= *c != b'0';
        i += 1;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        while let Some(c) = b.get(i).filter(|c| c.is_ascii_digit()) {
            digits += 1;
            nonzero |= *c != b'0';
            i += 1;
        }
    }
    if digits == 0 {
        return Err(FloatTextError::Syntax);
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return Err(FloatTextError::Syntax);
        }
    }
    if i != b.len() {
        return Err(FloatTextError::Syntax);
    }
    Ok(nonzero)
}

fn trim_ascii_ws(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'))
}

/// 文字列 → `f64`（double precision）。
pub(crate) fn parse_pg_double(s: &str) -> Result<f64, FloatTextError> {
    let t = trim_ascii_ws(s);
    if is_non_finite_word(t) {
        return Err(FloatTextError::NonFinite);
    }
    let nonzero = scan_float_grammar(t)?;
    let v: f64 = t.parse().map_err(|_| FloatTextError::Syntax)?;
    if !v.is_finite() || (v == 0.0 && nonzero) {
        return Err(FloatTextError::Range);
    }
    Ok(crate::scalar_float::canonicalize_double(v))
}

/// 文字列 → `f32`（real）。`f64` を経由せず `f32` の `FromStr` で直接解析する（二重丸め防止）。
pub(crate) fn parse_pg_real(s: &str) -> Result<f32, FloatTextError> {
    let t = trim_ascii_ws(s);
    if is_non_finite_word(t) {
        return Err(FloatTextError::NonFinite);
    }
    let nonzero = scan_float_grammar(t)?;
    let v: f32 = t.parse().map_err(|_| FloatTextError::Syntax)?;
    if !v.is_finite() || (v == 0.0 && nonzero) {
        return Err(FloatTextError::Range);
    }
    Ok(crate::scalar_float::canonicalize_real(v))
}

fn text_to_float(s: &str, single: bool) -> Result<f64, SqlSurfaceError> {
    let type_name = if single { "real" } else { "double precision" };
    let r = if single {
        parse_pg_real(s).map(f64::from)
    } else {
        parse_pg_double(s)
    };
    r.map_err(|e| match e {
        FloatTextError::Syntax => syntax_error(type_name, s),
        FloatTextError::Range => out_of_range(format!(
            "\"{}\" is out of range for type {type_name}",
            echo(s)
        )),
        FloatTextError::NonFinite => non_finite_unsupported(type_name),
    })
}

/// numeric の型修飾子 `(p, s)` を丸めと桁検査つきで適用する（`parse_for_column` を再利用し、
/// 丸め規則〔0 から遠い側〕と精度判定を二重実装しない）。
pub(crate) fn apply_numeric_typmod(
    d: Decimal,
    typmod: Option<(u8, u8)>,
) -> Result<Decimal, SqlSurfaceError> {
    let Some((p, s)) = typmod else {
        return Ok(d);
    };
    numeric::parse_for_column(&d.to_string(), p, s).map_err(|e| match e {
        NumericError::OutOfRange => out_of_range("numeric field overflow"),
        NumericError::Malformed(_) => internal("numeric value could not be re-scaled"),
    })
}

/// 文字列 → numeric。`NaN`／`Infinity` は `0A000`。`_` 区切り・非 10 進の接頭辞・小数 38 桁超は
/// 受け付けない（既知の差分。前二者は `22P02`、後者は `22003`）。
pub(crate) fn text_to_numeric(
    s: &str,
    typmod: Option<(u8, u8)>,
) -> Result<Decimal, SqlSurfaceError> {
    let t = trim_ascii_ws(s);
    if is_non_finite_word(t) {
        return Err(non_finite_unsupported("numeric"));
    }
    let r = match typmod {
        Some((p, sc)) => numeric::parse_for_column(t, p, sc),
        None => numeric::parse_literal_exact(t),
    };
    r.map_err(|e| match e {
        NumericError::Malformed(_) => syntax_error("numeric", s),
        NumericError::OutOfRange if typmod.is_some() => out_of_range("numeric field overflow"),
        NumericError::OutOfRange => out_of_range("value overflows numeric format"),
    })
}

/// float → numeric。PostgreSQL 17 は有効桁 15（float8）／6（float4）の `%g` 表現を経由する
/// （最短往復表記ではない）。`{:.N e}` で同じ有効桁へ丸め、仮数の末尾ゼロを除いて
/// 正確な 10 進として解析する。38 桁・scale 38 を超える値は `22003`（既知の差分）。
fn float_to_decimal(v: f64, single: bool) -> Result<Decimal, SqlSurfaceError> {
    if v == 0.0 {
        return Decimal::from_parts(0, 0).map_err(|_| internal("zero decimal"));
    }
    let formatted = if single {
        format!("{:.5e}", v as f32)
    } else {
        format!("{v:.14e}")
    };
    let (mantissa, exp) = formatted
        .split_once('e')
        .ok_or_else(|| internal("float formatting produced no exponent"))?;
    let mantissa = if mantissa.contains('.') {
        mantissa.trim_end_matches('0').trim_end_matches('.')
    } else {
        mantissa
    };
    numeric::parse_literal_exact(&format!("{mantissa}e{exp}"))
        .map_err(|_| out_of_range("value overflows numeric format"))
}

/// numeric → float。文字列表現からその型の `FromStr` で直接解析する（二重丸め防止）。
fn decimal_to_float(d: &Decimal, single: bool) -> Result<f64, SqlSurfaceError> {
    let text = d.to_string();
    if single {
        text.parse::<f32>()
            .map(f64::from)
            .map_err(|_| internal("decimal could not be converted to real"))
    } else {
        text.parse::<f64>()
            .map_err(|_| internal("decimal could not be converted to double precision"))
    }
}

/// 型修飾子の適用結果の文字列（切り詰め・空白埋め）。`varchar(n)`／`char(n)` への明示的な変換は
/// 文字数 `n` で黙って切り詰める（代入と違い `22001` にならない）。
fn apply_char_typmod(s: Cow<'_, str>, to: CastDst) -> Result<Cow<'_, str>, SqlSurfaceError> {
    let (n, pad) = match to {
        CastDst::Varchar(Some(n)) => (n, false),
        CastDst::Bpchar(n) => (n, true),
        _ => return Ok(s),
    };
    if n > MAX_CHAR_TYPE_LENGTH {
        return Err(internal(
            "character length modifier exceeds the supported maximum",
        ));
    }
    let n = n as usize;
    let mut out = s;
    // 文字境界を `char_indices` で求める（添字アクセスは使わない）。
    let char_count = match out.char_indices().nth(n) {
        Some((byte_idx, _)) => {
            out.to_mut().truncate(byte_idx);
            n
        }
        None => out.chars().count(),
    };
    if pad && char_count < n {
        let fill = n - char_count;
        let owned = out.to_mut();
        owned
            .try_reserve(fill)
            .map_err(|_| SqlSurfaceError::payload_too_large("char(n) padding exceeds memory"))?;
        owned.extend(std::iter::repeat_n(' ', fill));
    }
    Ok(out)
}

/// 変換元の値（評価値から取り出した正規形）。
enum SrcValue<'a> {
    Int(i64),
    Float(f64),
    Dec(Decimal),
    Bool(bool),
    Text(Cow<'a, str>),
}

fn extract_src<'a>(from: CastSrc, v: ExprValue<'a>) -> Result<SrcValue<'a>, SqlSurfaceError> {
    let mismatch = || internal("cast argument type mismatch at evaluation time");
    match (from, v) {
        (CastSrc::Int2 | CastSrc::Int4 | CastSrc::Int8, ExprValue::Scalar(x)) => {
            if x.is_finite() && x.fract() == 0.0 && x.abs() <= EXACT_INT_MAX as f64 {
                Ok(SrcValue::Int(x as i64))
            } else {
                Err(internal("integer cast argument is not an exact integer"))
            }
        }
        (CastSrc::Float4 | CastSrc::Float8, ExprValue::Scalar(x)) => Ok(SrcValue::Float(x)),
        (CastSrc::Numeric, ExprValue::Numeric(d)) => Ok(SrcValue::Dec(d)),
        (CastSrc::Bool, ExprValue::Bool(b)) => Ok(SrcValue::Bool(b)),
        (CastSrc::Text, ExprValue::Text(s)) => Ok(SrcValue::Text(s)),
        _ => Err(mismatch()),
    }
}

/// 変換を 1 値に適用する。NULL は呼び出し元（`apply_builtin` の strict ガード）が先に返すが、
/// 念のためここでも NULL を素通しする。
pub(crate) fn apply_cast<'a>(
    f: CastFn,
    v: ExprValue<'a>,
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    if matches!(v, ExprValue::Null) {
        return Ok(ExprValue::Null);
    }
    if !(f.from.is_basic() && f.to.is_basic()) {
        return apply_cast_ext(f, v);
    }
    let src = extract_src(f.from, v)?;
    convert_basic(src, f.from == CastSrc::Float4, f.to)
}

/// 数値 6 型・BOOLEAN・文字列の変換先へ、正規化済みの変換元の値を変換する（Issue #1591）。
/// jsonb → 数値・BOOLEAN（Issue #1592）も抽出した値をここへ流し、丸め・範囲検査を共有する。
fn convert_basic<'a>(
    src: SrcValue<'a>,
    single_src: bool,
    to: CastDst,
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    match to {
        CastDst::Int2 | CastDst::Int4 | CastDst::Int8 => {
            let w = IntWidth::from_dst(to).ok_or_else(|| internal("integer width"))?;
            let i = match src {
                SrcValue::Int(i) => finish_int(i128::from(i), w)?,
                SrcValue::Float(x) => {
                    let r = x.round_ties_even();
                    // i64 の範囲外（NaN を含む）は幅によらず範囲外。
                    if !r.is_finite() || r.abs() >= 9_223_372_036_854_775_808.0 {
                        return Err(out_of_range(format!("{} out of range", w.name())));
                    }
                    finish_int(i128::from(r as i64), w)?
                }
                SrcValue::Dec(d) => {
                    let r = numeric::round_to_integer(&d)
                        .ok_or_else(|| out_of_range(format!("{} out of range", w.name())))?;
                    finish_int(r, w)?
                }
                SrcValue::Bool(b) => finish_int(i128::from(b), w)?,
                SrcValue::Text(s) => text_to_int(&s, w)?,
            };
            Ok(ExprValue::Scalar(i as f64))
        }
        CastDst::Float4 => {
            let x = match src {
                SrcValue::Int(i) => f64::from(i as f32),
                SrcValue::Float(x) => {
                    let narrowed = x as f32;
                    if !narrowed.is_finite() {
                        return Err(out_of_range("value out of range: overflow"));
                    }
                    if narrowed == 0.0 && x != 0.0 {
                        return Err(out_of_range("value out of range: underflow"));
                    }
                    f64::from(narrowed)
                }
                SrcValue::Dec(d) => decimal_to_float(&d, true)?,
                SrcValue::Text(s) => text_to_float(&s, true)?,
                SrcValue::Bool(_) => return Err(internal("boolean cannot be cast to real")),
            };
            Ok(ExprValue::Scalar(x))
        }
        CastDst::Float8 => {
            let x = match src {
                SrcValue::Int(i) => i as f64,
                SrcValue::Float(x) => x,
                SrcValue::Dec(d) => decimal_to_float(&d, false)?,
                SrcValue::Text(s) => text_to_float(&s, false)?,
                SrcValue::Bool(_) => {
                    return Err(internal("boolean cannot be cast to double precision"))
                }
            };
            Ok(ExprValue::Scalar(x))
        }
        CastDst::Numeric(typmod) => {
            let d = match src {
                SrcValue::Int(i) => Decimal::from_parts(i128::from(i), 0)
                    .map_err(|_| internal("integer does not fit a decimal"))?,
                SrcValue::Float(x) => float_to_decimal(x, single_src)?,
                SrcValue::Dec(d) => d,
                // 文字列は修飾子つきで直接解析する（丸めを 1 回にする）。
                SrcValue::Text(s) => return Ok(ExprValue::Numeric(text_to_numeric(&s, typmod)?)),
                SrcValue::Bool(_) => return Err(internal("boolean cannot be cast to numeric")),
            };
            Ok(ExprValue::Numeric(apply_numeric_typmod(d, typmod)?))
        }
        CastDst::Bool => match src {
            SrcValue::Int(i) => Ok(ExprValue::Bool(i != 0)),
            SrcValue::Bool(b) => Ok(ExprValue::Bool(b)),
            SrcValue::Text(s) => crate::sql::params::parse_pg_bool(&s)
                .map(ExprValue::Bool)
                .ok_or_else(|| syntax_error("boolean", &s)),
            SrcValue::Float(_) | SrcValue::Dec(_) => {
                Err(internal("this value type cannot be cast to boolean"))
            }
        },
        CastDst::Text | CastDst::Varchar(_) | CastDst::Bpchar(_) => {
            let s: Cow<'a, str> = match src {
                SrcValue::Int(i) => Cow::Owned(i.to_string()),
                SrcValue::Float(x) => Cow::Owned(if single_src {
                    format_real(x as f32)
                } else {
                    format_double(x)
                }),
                SrcValue::Dec(d) => Cow::Owned(d.to_string()),
                SrcValue::Bool(b) => Cow::Borrowed(if b { "true" } else { "false" }),
                SrcValue::Text(s) => s,
            };
            Ok(ExprValue::Text(apply_char_typmod(s, to)?))
        }
        // 日付時刻・JSON・UUID・BYTEA・配列は `apply_cast_ext` が担う（呼び出し元が振り分ける）。
        _ => Err(internal("non-basic cast destination reached convert_basic")),
    }
}

/// `DateTimeLiteralError` の SQLSTATE 写像（`22007` 書式・`22008` 範囲外）。型なしリテラルの
/// 束縛（`sql::cast::bind_string_literal_as`）と文字列式からの評価時変換が共有する。
pub(crate) fn datetime_literal_error(e: crate::datetime::DateTimeLiteralError) -> SqlSurfaceError {
    use crate::datetime::DateTimeLiteralError;
    match e {
        DateTimeLiteralError::Format(detail) => SqlSurfaceError::invalid_datetime_format(detail),
        DateTimeLiteralError::Overflow(detail) => SqlSurfaceError::datetime_field_overflow(detail),
    }
}

/// `IntervalLiteralError` の SQLSTATE 写像（`22007`／`22015`／`22008`、無限大は `0A000`）。
pub(crate) fn interval_literal_error(e: crate::datetime::IntervalLiteralError) -> SqlSurfaceError {
    use crate::datetime::IntervalLiteralError;
    match e {
        IntervalLiteralError::Format(detail) => SqlSurfaceError::invalid_datetime_format(detail),
        IntervalLiteralError::FieldOverflow => {
            SqlSurfaceError::interval_field_overflow(e.to_string())
        }
        IntervalLiteralError::OutOfRange => SqlSurfaceError::datetime_field_overflow(e.to_string()),
        IntervalLiteralError::Infinity => SqlSurfaceError::FeatureNotSupported {
            detail: "infinite interval values are not supported yet".to_string(),
        },
    }
}

/// `TimestampTzLiteralError` の SQLSTATE 写像（未知の TZ 名は `22023`）。
fn timestamptz_literal_error(e: crate::datetime::TimestampTzLiteralError) -> SqlSurfaceError {
    use crate::datetime::TimestampTzLiteralError;
    match e {
        TimestampTzLiteralError::Literal(inner) => datetime_literal_error(inner),
        TimestampTzLiteralError::UnknownTimeZone => SqlSurfaceError::InvalidParameterValue {
            detail: crate::datetime::UNRECOGNIZED_TIME_ZONE_DETAIL,
        },
    }
}

/// `json`／`jsonb` の入力検査エラーの写像（形式不正は `22P02`、長さ超過は `54000`）。
fn json_column_error(e: crate::json::JsonColumnError, type_name: &str) -> SqlSurfaceError {
    match e {
        crate::json::JsonColumnError::TooLong => {
            SqlSurfaceError::payload_too_large("JSON value exceeds the maximum field length")
        }
        crate::json::JsonColumnError::Invalid => SqlSurfaceError::invalid_text_representation(
            format!("invalid input syntax for type {type_name}"),
        ),
    }
}

pub(crate) fn bytea_cast_error(e: crate::bytea::ByteaCastError) -> SqlSurfaceError {
    use crate::bytea::ByteaCastError;
    match e {
        ByteaCastError::TooLong => {
            SqlSurfaceError::payload_too_large("bytea value exceeds the maximum field length")
        }
        ByteaCastError::OddLength => SqlSurfaceError::InvalidParameterValue {
            detail: "invalid hexadecimal data: odd number of digits",
        },
        ByteaCastError::InvalidDigit => SqlSurfaceError::InvalidParameterValue {
            detail: "invalid hexadecimal digit",
        },
        ByteaCastError::InvalidSyntax => {
            SqlSurfaceError::invalid_text_representation("invalid input syntax for type bytea")
        }
    }
}

/// 日付時刻・JSON・UUID・BYTEA・配列が変換元または変換先に絡む変換（Issue #1592）。
///
/// 文字列型への変換は各型の出力関数、文字列型からの変換は入力関数を通す（規則表の `io/a`・
/// `io/e`）。それ以外は型どうしの個別の変換（日付時刻間・json ⇄ jsonb・jsonb → 数値/BOOLEAN・
/// 配列 → 配列）。型の組み合わせは束縛段が規則表で絞っており、ここへ来ない組は `Internal`。
fn apply_cast_ext<'a>(f: CastFn, v: ExprValue<'a>) -> Result<ExprValue<'a>, SqlSurfaceError> {
    if matches!(
        f.to,
        CastDst::Text | CastDst::Varchar(_) | CastDst::Bpchar(_)
    ) {
        let s = render_to_text(f.from, v)?;
        return Ok(ExprValue::Text(apply_char_typmod(s, f.to)?));
    }
    if f.from == CastSrc::Text {
        let ExprValue::Text(s) = v else {
            return Err(internal("cast argument type mismatch at evaluation time"));
        };
        return parse_from_text(&s, f.to);
    }
    convert_between(f, v)
}

fn mismatch() -> SqlSurfaceError {
    internal("cast argument type mismatch at evaluation time")
}

/// 各型 → 文字列型（`io/a`）の文字列表現。BYTEA と配列は結果が TEXT の上限を超えるときは
/// 確保前（配列は描画の途中）に `54000` で拒否する。
fn render_to_text<'a>(from: CastSrc, v: ExprValue<'a>) -> Result<Cow<'a, str>, SqlSurfaceError> {
    let max_text = crate::row_codec::MAX_TEXT_FIELD_LEN as usize;
    match (from, v) {
        (CastSrc::Date, ExprValue::Date(d)) => Ok(Cow::Owned(crate::datetime::format_date(d))),
        (CastSrc::Time, ExprValue::Time(t)) => Ok(Cow::Owned(crate::datetime::format_time(t))),
        (CastSrc::Timestamp, ExprValue::Timestamp(t)) => {
            Ok(Cow::Owned(crate::datetime::format_timestamp(t)))
        }
        (CastSrc::TimestampTz, ExprValue::TimestampTz(t)) => {
            Ok(Cow::Owned(crate::datetime::with_current_session_time_zone(
                |tz| crate::datetime::format_timestamptz(t, tz),
            )))
        }
        (CastSrc::Interval, ExprValue::Interval(iv)) => {
            Ok(Cow::Owned(crate::datetime::format_interval(&iv)))
        }
        (CastSrc::Json, ExprValue::JsonText(t)) | (CastSrc::Jsonb, ExprValue::Json(t)) => Ok(t),
        (CastSrc::Uuid, ExprValue::Uuid(u)) => Ok(Cow::Owned(u.to_string())),
        (CastSrc::Bytea, ExprValue::Bytes(b)) => {
            // `\x` ＋ 2 桁/バイト。確保前に長さを計算する。
            let hex_len = b.len().checked_mul(2).and_then(|n| n.checked_add(2));
            if hex_len.is_none_or(|n| n > max_text) {
                return Err(SqlSurfaceError::payload_too_large(
                    "bytea hex text exceeds the maximum TEXT field length",
                ));
            }
            Ok(Cow::Owned(crate::bytea::format_hex_text(&b)))
        }
        (CastSrc::Array(_), ExprValue::Array(a)) => a
            .to_pg_text_bounded(max_text)
            .map(Cow::Owned)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(
                    "array text exceeds the maximum TEXT field length",
                )
            }),
        _ => Err(mismatch()),
    }
}

/// 文字列型 → 各型（`io/e`）の入力関数。
fn parse_from_text(s: &str, to: CastDst) -> Result<ExprValue<'static>, SqlSurfaceError> {
    match to {
        CastDst::Date => crate::datetime::parse_date(s)
            .map(ExprValue::Date)
            .map_err(datetime_literal_error),
        CastDst::Time => crate::datetime::parse_time(s)
            .map(ExprValue::Time)
            .map_err(datetime_literal_error),
        CastDst::Timestamp => crate::datetime::parse_timestamp(s)
            .map(ExprValue::Timestamp)
            .map_err(datetime_literal_error),
        CastDst::TimestampTz => {
            // ゾーンの無い入力は評価時のセッション TimeZone の壁時計として解釈する。
            crate::datetime::with_current_session_time_zone(|tz| {
                crate::datetime::parse_timestamptz(s, tz)
            })
            .map(ExprValue::TimestampTz)
            .map_err(timestamptz_literal_error)
        }
        CastDst::Interval => crate::datetime::parse_interval(s)
            .map(ExprValue::Interval)
            .map_err(interval_literal_error),
        CastDst::Json => {
            crate::json::validate_json_column_text(s).map_err(|e| json_column_error(e, "json"))?;
            Ok(ExprValue::JsonText(Cow::Owned(s.to_string())))
        }
        CastDst::Jsonb => crate::json::canonicalize_jsonb_text(s)
            .map(|t| ExprValue::Json(Cow::Owned(t)))
            .map_err(|e| json_column_error(e, "jsonb")),
        CastDst::Uuid => crate::uuid::parse_uuid_text(s)
            .map(ExprValue::Uuid)
            .map_err(|_| syntax_error("uuid", s)),
        CastDst::Bytea => crate::bytea::parse_cast_text(s)
            .map(|b| ExprValue::Bytes(Cow::Owned(b)))
            .map_err(bytea_cast_error),
        CastDst::Array(elem) => {
            let ty = ArrayType::new(elem, MAX_ARRAY_ELEMENTS)
                .map_err(|_| internal("array element type is not supported as a cast target"))?;
            crate::sql::parser::parse_array_literal_for_cast(s, &ty).map(ExprValue::Array)
        }
        _ => Err(internal("unexpected cast destination for a text source")),
    }
}

/// `json`（原文）→ `jsonb`。`json` 列は重複キーを保持しうる（取り出し時は最後の値が有効）ため、
/// PostgreSQL の `json::jsonb` と同じく最後の値を採って正規化する。
fn json_text_to_jsonb(text: &str) -> Result<String, SqlSurfaceError> {
    if text.len() > crate::json::MAX_JSON_FIELD_LEN {
        return Err(json_column_error(
            crate::json::JsonColumnError::TooLong,
            "jsonb",
        ));
    }
    let value = crate::json::parse_json_with(text, crate::json::DuplicateKeys::LastWins)
        .map_err(|_| json_column_error(crate::json::JsonColumnError::Invalid, "jsonb"))?;
    let mut out = String::new();
    crate::json::write_canonical(&value, &mut out);
    if out.len() > crate::json::MAX_JSON_FIELD_LEN {
        return Err(json_column_error(
            crate::json::JsonColumnError::TooLong,
            "jsonb",
        ));
    }
    Ok(out)
}

/// jsonb の値の種別（`22023` の文言用）。
#[derive(Clone, Copy)]
enum JsonbKind {
    String,
    Numeric,
    Boolean,
    Null,
    Array,
    Object,
}

/// `cannot cast jsonb <種別> to type <型名>`（`22023`）。値を含まない固定文言で、
/// `InvalidParameterValue` が静的文字列のみを持つため種別 × 型名を `concat!` で列挙する。
fn jsonb_cast_error(kind: JsonbKind, to: CastDst) -> SqlSurfaceError {
    macro_rules! detail {
        ($ty:literal) => {
            match kind {
                JsonbKind::String => concat!("cannot cast jsonb string to type ", $ty),
                JsonbKind::Numeric => concat!("cannot cast jsonb numeric to type ", $ty),
                JsonbKind::Boolean => concat!("cannot cast jsonb boolean to type ", $ty),
                JsonbKind::Null => concat!("cannot cast jsonb null to type ", $ty),
                JsonbKind::Array => concat!("cannot cast jsonb array to type ", $ty),
                JsonbKind::Object => concat!("cannot cast jsonb object to type ", $ty),
            }
        };
    }
    let detail = match to {
        CastDst::Int2 => detail!("smallint"),
        CastDst::Int4 => detail!("integer"),
        CastDst::Int8 => detail!("bigint"),
        CastDst::Float4 => detail!("real"),
        CastDst::Float8 => detail!("double precision"),
        CastDst::Numeric(_) => detail!("numeric"),
        _ => detail!("boolean"),
    };
    SqlSurfaceError::InvalidParameterValue { detail }
}

/// jsonb → 数値 6 型・BOOLEAN（`e`）。数値は numeric を経由して既存の変換（四捨五入・範囲検査・
/// `numeric(p,s)` の丸め）を共有し、型の合わない JSON は `22023`。
fn jsonb_to_scalar<'a>(text: &str, to: CastDst) -> Result<ExprValue<'a>, SqlSurfaceError> {
    use crate::json::{JsonNumber, JsonValue};
    let value = crate::json::parse_json(text)
        .map_err(|_| internal("stored jsonb text could not be parsed"))?;
    match value {
        JsonValue::Bool(b) if matches!(to, CastDst::Bool) => Ok(ExprValue::Bool(b)),
        JsonValue::Bool(_) => Err(jsonb_cast_error(JsonbKind::Boolean, to)),
        JsonValue::Number(_) if matches!(to, CastDst::Bool) => {
            Err(jsonb_cast_error(JsonbKind::Numeric, to))
        }
        JsonValue::Number(n) => {
            let overflow = || out_of_range("value overflows numeric format");
            // 変換先が浮動小数のときは、小数・指数表記の JSON 数値を numeric（38 桁制約）へ
            // 通さず、元テキストから対象の型へ直接解析する（`1e100`／`1e-100` を許容する。
            // 型の範囲外〔非有限・float4 のアンダーフロー含む〕のみ `22003`）。
            if let (JsonNumber::Float { text, .. }, CastDst::Float4 | CastDst::Float8) = (&n, to) {
                let x = text_to_float(text, matches!(to, CastDst::Float4))?;
                return Ok(ExprValue::Scalar(x));
            }
            let d = match n {
                JsonNumber::PosInt(u) => {
                    Decimal::from_parts(i128::from(u), 0).map_err(|_| overflow())?
                }
                JsonNumber::NegInt(i) => {
                    Decimal::from_parts(i128::from(i), 0).map_err(|_| overflow())?
                }
                JsonNumber::Float { text, .. } => {
                    numeric::parse_literal_exact(&text).map_err(|_| overflow())?
                }
            };
            // 抽出した numeric を既存の数値変換（四捨五入・範囲検査・修飾子）へ流す。
            match convert_basic(SrcValue::Dec(d), false, to)? {
                ExprValue::Scalar(x) => Ok(ExprValue::Scalar(x)),
                ExprValue::Numeric(d) => Ok(ExprValue::Numeric(d)),
                _ => Err(internal(
                    "jsonb numeric conversion produced an unexpected value",
                )),
            }
        }
        JsonValue::String(_) => Err(jsonb_cast_error(JsonbKind::String, to)),
        JsonValue::Null => Err(jsonb_cast_error(JsonbKind::Null, to)),
        JsonValue::Array(_) => Err(jsonb_cast_error(JsonbKind::Array, to)),
        JsonValue::Object(_) => Err(jsonb_cast_error(JsonbKind::Object, to)),
    }
}

/// 型どうしの個別の変換（文字列型が絡まないもの）。
fn convert_between<'a>(f: CastFn, v: ExprValue<'a>) -> Result<ExprValue<'a>, SqlSurfaceError> {
    use crate::datetime::MICROS_PER_DAY;
    use crate::sql::datetime_fn::{date_to_timestamp, tz_out_of_range};
    match (f.from, f.to, v) {
        (CastSrc::Date, CastDst::Timestamp, ExprValue::Date(d)) => {
            Ok(ExprValue::Timestamp(date_to_timestamp(d)))
        }
        (CastSrc::Date, CastDst::TimestampTz, ExprValue::Date(d)) => {
            crate::datetime::with_current_session_time_zone(|tz| tz.date_to_timestamptz(d))
                .map(ExprValue::TimestampTz)
                .ok_or_else(tz_out_of_range)
        }
        (CastSrc::Timestamp, CastDst::TimestampTz, ExprValue::Timestamp(t)) => {
            crate::datetime::with_current_session_time_zone(|tz| tz.timestamp_to_timestamptz(t))
                .map(ExprValue::TimestampTz)
                .ok_or_else(tz_out_of_range)
        }
        (CastSrc::TimestampTz, CastDst::Timestamp, ExprValue::TimestampTz(t)) => {
            crate::datetime::with_current_session_time_zone(|tz| tz.timestamptz_to_timestamp(t))
                .map(ExprValue::Timestamp)
                .ok_or_else(tz_out_of_range)
        }
        (CastSrc::TimestampTz, CastDst::Date, ExprValue::TimestampTz(t)) => {
            crate::datetime::with_current_session_time_zone(|tz| tz.timestamptz_to_date(t))
                .map(ExprValue::Date)
                .ok_or_else(tz_out_of_range)
        }
        (CastSrc::Timestamp, CastDst::Date, ExprValue::Timestamp(t)) => {
            let days =
                i32::try_from(t.div_euclid(MICROS_PER_DAY)).map_err(|_| tz_out_of_range())?;
            if !crate::datetime::validate_date_days(days) {
                return Err(tz_out_of_range());
            }
            Ok(ExprValue::Date(days))
        }
        (CastSrc::Timestamp, CastDst::Time, ExprValue::Timestamp(t)) => {
            Ok(ExprValue::Time(t.rem_euclid(MICROS_PER_DAY)))
        }
        (CastSrc::TimestampTz, CastDst::Time, ExprValue::TimestampTz(t)) => {
            // セッション TZ の壁時計にしてから時刻部分を取る。
            let local = crate::datetime::with_current_session_time_zone(|tz| {
                tz.timestamptz_to_timestamp(t)
            })
            .ok_or_else(tz_out_of_range)?;
            Ok(ExprValue::Time(local.rem_euclid(MICROS_PER_DAY)))
        }
        (CastSrc::Time, CastDst::Interval, ExprValue::Time(t)) => {
            Ok(ExprValue::Interval(crate::datetime::Interval {
                months: 0,
                days: 0,
                micros: t,
            }))
        }
        (CastSrc::Interval, CastDst::Time, ExprValue::Interval(iv)) => {
            // 月・日は捨て、時刻部分を 1 日の剰余にする（`interval_time`）。
            Ok(ExprValue::Time(iv.micros.rem_euclid(MICROS_PER_DAY)))
        }
        (CastSrc::Json, CastDst::Jsonb, ExprValue::JsonText(t)) => {
            json_text_to_jsonb(&t).map(|s| ExprValue::Json(Cow::Owned(s)))
        }
        (CastSrc::Jsonb, CastDst::Json, ExprValue::Json(t)) => Ok(ExprValue::JsonText(t)),
        (
            CastSrc::Jsonb,
            CastDst::Int2
            | CastDst::Int4
            | CastDst::Int8
            | CastDst::Float4
            | CastDst::Float8
            | CastDst::Numeric(_)
            | CastDst::Bool,
            ExprValue::Json(t),
        ) => jsonb_to_scalar(&t, f.to),
        (CastSrc::Array(from_elem), CastDst::Array(to_elem), ExprValue::Array(a)) => {
            cast_array(from_elem, to_elem, &a).map(ExprValue::Array)
        }
        _ => Err(mismatch()),
    }
}

/// 配列の要素を評価値へ写す（NULL 要素は `ExprValue::Null`）。bigint は式評価の整数表現の
/// 上限（2^53）を超える要素を `22003` で拒否する（スカラーの int8 と同じ既知の差分）。
pub(crate) fn array_to_values(a: &ArrayValue) -> Result<Vec<ExprValue<'static>>, SqlSurfaceError> {
    fn map<T>(
        items: &[Option<T>],
        f: impl Fn(&T) -> Result<ExprValue<'static>, SqlSurfaceError>,
    ) -> Result<Vec<ExprValue<'static>>, SqlSurfaceError> {
        items
            .iter()
            .map(|e| match e {
                Some(v) => f(v),
                None => Ok(ExprValue::Null),
            })
            .collect()
    }
    let text = |s: &String| Ok(ExprValue::Text(Cow::Owned(s.clone())));
    // 多次元値の呼び出し元（`cast_array`）は次元を保持して包み直す。次元を捨てて平坦化する
    // 経路を作らないため、ここへ `Multi` が直接来たら fail-closed（Issue #1605）。
    match a {
        ArrayValue::Text(v) => map(v, text),
        ArrayValue::Bool(v) => map(v, |b| Ok(ExprValue::Bool(*b))),
        ArrayValue::Integer(v) => map(v, |i| Ok(ExprValue::Scalar(f64::from(*i)))),
        ArrayValue::BigInt(v) => map(v, |i| {
            if i.unsigned_abs() > EXACT_INT_MAX as u64 {
                return Err(out_of_range(
                    "bigint value exceeds the range this engine can evaluate exactly",
                ));
            }
            Ok(ExprValue::Scalar(*i as f64))
        }),
        ArrayValue::Real(v) => map(v, |x| Ok(ExprValue::Scalar(f64::from(*x)))),
        ArrayValue::Double(v) => map(v, |x| Ok(ExprValue::Scalar(*x))),
        ArrayValue::Date(v) => map(v, |d| Ok(ExprValue::Date(*d))),
        ArrayValue::Timestamp(v) => map(v, |t| Ok(ExprValue::Timestamp(*t))),
        ArrayValue::Uuid(v) => map(v, |u| Ok(ExprValue::Uuid(*u))),
        ArrayValue::Numeric(v) => map(v, |d| Ok(ExprValue::Numeric(*d))),
        ArrayValue::Bytea(v) => map(v, |b| Ok(ExprValue::Bytes(Cow::Owned(b.clone())))),
        ArrayValue::Json(v) => map(v, |s| Ok(ExprValue::JsonText(Cow::Owned(s.clone())))),
        ArrayValue::Jsonb(v) => map(v, |s| Ok(ExprValue::Json(Cow::Owned(s.clone())))),
        ArrayValue::Enum(_) => Err(internal("enum array reached an array cast")),
        ArrayValue::Multi(_) => Err(SqlSurfaceError::unsupported(
            "this operation on multi-dimensional arrays is not supported",
        )),
    }
}

/// 要素ごとの評価値を、変換先の要素型の配列へ詰め直す。
pub(crate) fn values_to_array(
    values: Vec<ExprValue<'static>>,
    to: ArrayElemType,
) -> Result<ArrayValue, SqlSurfaceError> {
    fn collect<T>(
        values: Vec<ExprValue<'static>>,
        f: impl Fn(ExprValue<'static>) -> Result<T, SqlSurfaceError>,
    ) -> Result<Vec<Option<T>>, SqlSurfaceError> {
        values
            .into_iter()
            .map(|v| match v {
                ExprValue::Null => Ok(None),
                other => f(other).map(Some),
            })
            .collect()
    }
    let bad = || internal("array element conversion produced an unexpected value");
    let int_of = |v: ExprValue<'static>| match v {
        ExprValue::Scalar(x) if x.is_finite() && x.fract() == 0.0 => Ok(x as i64),
        _ => Err(internal(
            "array element conversion produced an unexpected value",
        )),
    };
    Ok(match to {
        ArrayElemType::Text => ArrayValue::Text(collect(values, |v| match v {
            ExprValue::Text(s) => Ok(s.into_owned()),
            _ => Err(bad()),
        })?),
        ArrayElemType::Bool => ArrayValue::Bool(collect(values, |v| match v {
            ExprValue::Bool(b) => Ok(b),
            _ => Err(bad()),
        })?),
        ArrayElemType::Integer => ArrayValue::Integer(collect(values, |v| {
            i32::try_from(int_of(v)?).map_err(|_| out_of_range("integer out of range"))
        })?),
        ArrayElemType::BigInt => ArrayValue::BigInt(collect(values, int_of)?),
        ArrayElemType::Real => ArrayValue::Real(collect(values, |v| match v {
            ExprValue::Scalar(x) => Ok(x as f32),
            _ => Err(bad()),
        })?),
        ArrayElemType::Double => ArrayValue::Double(collect(values, |v| match v {
            ExprValue::Scalar(x) => Ok(x),
            _ => Err(bad()),
        })?),
        ArrayElemType::Date => ArrayValue::Date(collect(values, |v| match v {
            ExprValue::Date(d) => Ok(d),
            _ => Err(bad()),
        })?),
        ArrayElemType::Timestamp => ArrayValue::Timestamp(collect(values, |v| match v {
            ExprValue::Timestamp(t) => Ok(t),
            _ => Err(bad()),
        })?),
        ArrayElemType::Uuid => ArrayValue::Uuid(collect(values, |v| match v {
            ExprValue::Uuid(u) => Ok(u),
            _ => Err(bad()),
        })?),
        ArrayElemType::Numeric { .. } => ArrayValue::Numeric(collect(values, |v| match v {
            ExprValue::Numeric(d) => Ok(d),
            _ => Err(bad()),
        })?),
        ArrayElemType::Bytea => ArrayValue::Bytea(collect(values, |v| match v {
            ExprValue::Bytes(b) => Ok(b.into_owned()),
            _ => Err(bad()),
        })?),
        ArrayElemType::Json => ArrayValue::Json(collect(values, |v| match v {
            ExprValue::JsonText(s) => Ok(s.into_owned()),
            _ => Err(bad()),
        })?),
        ArrayElemType::Jsonb => ArrayValue::Jsonb(collect(values, |v| match v {
            ExprValue::Json(s) => Ok(s.into_owned()),
            _ => Err(bad()),
        })?),
        ArrayElemType::Enum => return Err(internal("enum array is not a cast target")),
    })
}

/// 配列 → 配列（`elem`）。要素ごとに（変換元の要素型, 変換先の要素型）の変換を適用し、
/// NULL 要素は NULL のまま、要素のエラーはその SQLSTATE のまま返す。要素数は入力と同じ。
fn cast_array(
    from: ArrayElemType,
    to: ArrayElemType,
    a: &ArrayValue,
) -> Result<ArrayValue, SqlSurfaceError> {
    let (Some(src), Some(dst)) = (elem_cast_src(from), elem_cast_dst(to)) else {
        return Err(internal("array element type is not supported for casts"));
    };
    let element = CastFn { from: src, to: dst };
    // 多次元は平坦な要素列を変換してから次元を与え直す（次元を保つ。Issue #1605）。
    let converted = array_to_values(a.flat())?
        .into_iter()
        .map(|v| apply_cast(element, v))
        .collect::<Result<Vec<_>, _>>()?;
    let flat = values_to_array(converted, to)?;
    ArrayValue::with_dims(flat, a.dims())
        .map_err(|_| internal("array dimensions were lost in an array cast"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast(
        from: CastSrc,
        to: CastDst,
        v: ExprValue<'static>,
    ) -> Result<ExprValue<'static>, SqlSurfaceError> {
        apply_cast(CastFn { from, to }, v)
    }

    fn text(s: &str) -> ExprValue<'static> {
        ExprValue::Text(Cow::Owned(s.to_string()))
    }

    fn dec(s: &str) -> Decimal {
        numeric::parse_literal_exact(s).expect("decimal")
    }

    fn scalar(r: Result<ExprValue<'static>, SqlSurfaceError>) -> f64 {
        match r.expect("ok") {
            ExprValue::Scalar(v) => v,
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    fn as_text(r: Result<ExprValue<'static>, SqlSurfaceError>) -> String {
        match r.expect("ok") {
            ExprValue::Text(t) => t.into_owned(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn numeric_text(r: Result<ExprValue<'static>, SqlSurfaceError>) -> String {
        match r.expect("ok") {
            ExprValue::Numeric(d) => d.to_string(),
            other => panic!("expected numeric, got {other:?}"),
        }
    }

    #[test]
    fn float_to_int_rounds_half_to_even() {
        for (input, want) in [(2.5, 2.0), (3.5, 4.0), (-2.5, -2.0), (0.5, 0.0), (1.5, 2.0)] {
            assert_eq!(
                scalar(cast(
                    CastSrc::Float8,
                    CastDst::Int4,
                    ExprValue::Scalar(input)
                )),
                want,
                "float8->int4 {input}"
            );
        }
    }

    #[test]
    fn numeric_to_int_rounds_half_away_from_zero() {
        for (input, want) in [
            ("2.5", 3.0),
            ("-2.5", -3.0),
            ("2.4", 2.0),
            ("-2.49", -2.0),
            ("0.5", 1.0),
        ] {
            assert_eq!(
                scalar(cast(
                    CastSrc::Numeric,
                    CastDst::Int4,
                    ExprValue::Numeric(dec(input))
                )),
                want,
                "numeric->int4 {input}"
            );
        }
    }

    #[test]
    fn int_narrowing_checks_range() {
        assert_eq!(
            scalar(cast(
                CastSrc::Int4,
                CastDst::Int2,
                ExprValue::Scalar(32767.0)
            )),
            32767.0
        );
        assert!(cast(CastSrc::Int4, CastDst::Int2, ExprValue::Scalar(32768.0)).is_err());
        assert!(cast(
            CastSrc::Int8,
            CastDst::Int4,
            ExprValue::Scalar(2_147_483_648.0)
        )
        .is_err());
        assert_eq!(
            scalar(cast(
                CastSrc::Int8,
                CastDst::Int4,
                ExprValue::Scalar(-2_147_483_648.0)
            )),
            -2_147_483_648.0
        );
        let err = cast(CastSrc::Int4, CastDst::Int2, ExprValue::Scalar(40000.0)).unwrap_err();
        assert!(matches!(err, SqlSurfaceError::NumericOutOfRange { .. }));
    }

    #[test]
    fn text_to_int_follows_pg_grammar() {
        let ok = |s: &str, want: f64| {
            assert_eq!(
                scalar(cast(CastSrc::Text, CastDst::Int4, text(s))),
                want,
                "{s:?}"
            );
        };
        ok("12", 12.0);
        ok(" 12 ", 12.0);
        ok("+7", 7.0);
        ok("-7", -7.0);
        ok("0x1A", 26.0);
        ok("-0x10", -16.0);
        ok("0o17", 15.0);
        ok("0b101", 5.0);
        ok("1_000", 1000.0);
        for bad in [
            "", "  ", "abc", "1.5", "1__0", "_1", "1_", "0x", "12abc", "--1",
        ] {
            let e = cast(CastSrc::Text, CastDst::Int4, text(bad)).unwrap_err();
            assert!(
                matches!(e, SqlSurfaceError::InvalidTextRepresentation { .. }),
                "{bad:?}: {e:?}"
            );
        }
        let e = cast(CastSrc::Text, CastDst::Int4, text("2147483648")).unwrap_err();
        assert!(matches!(e, SqlSurfaceError::NumericOutOfRange { .. }));
        let e = cast(
            CastSrc::Text,
            CastDst::Int2,
            text("99999999999999999999999"),
        )
        .unwrap_err();
        assert!(matches!(e, SqlSurfaceError::NumericOutOfRange { .. }));
    }

    #[test]
    fn text_to_int_error_message_shape() {
        let e = cast(CastSrc::Text, CastDst::Int4, text("abc")).unwrap_err();
        assert!(
            format!("{e}").ends_with("invalid input syntax for type integer: \"abc\""),
            "{e}"
        );
    }

    #[test]
    fn int8_beyond_exact_range_is_rejected() {
        assert!(cast(CastSrc::Text, CastDst::Int8, text("9007199254740993")).is_err());
        assert_eq!(
            scalar(cast(CastSrc::Text, CastDst::Int8, text("9007199254740992"))),
            9_007_199_254_740_992.0
        );
        assert!(cast(
            CastSrc::Numeric,
            CastDst::Int8,
            ExprValue::Numeric(dec("9007199254740993"))
        )
        .is_err());
    }

    #[test]
    fn float_narrowing_and_text_input() {
        assert!(cast(CastSrc::Float8, CastDst::Float4, ExprValue::Scalar(1e300)).is_err());
        assert!(cast(CastSrc::Float8, CastDst::Float4, ExprValue::Scalar(1e-300)).is_err());
        assert_eq!(
            scalar(cast(
                CastSrc::Float8,
                CastDst::Float4,
                ExprValue::Scalar(0.1)
            )),
            f64::from(0.1f32)
        );
        assert_eq!(
            scalar(cast(CastSrc::Text, CastDst::Float8, text("+1.5"))),
            1.5
        );
        assert_eq!(
            scalar(cast(CastSrc::Text, CastDst::Float8, text(".5"))),
            0.5
        );
        assert_eq!(
            scalar(cast(CastSrc::Text, CastDst::Float8, text("1."))),
            1.0
        );
        assert_eq!(
            scalar(cast(CastSrc::Text, CastDst::Float8, text(" 1e3 "))),
            1000.0
        );
        for bad in ["", "e5", "1e", "0x1p3", "1_0", "abc"] {
            assert!(
                matches!(
                    cast(CastSrc::Text, CastDst::Float8, text(bad)).unwrap_err(),
                    SqlSurfaceError::InvalidTextRepresentation { .. }
                ),
                "{bad:?}"
            );
        }
        assert!(matches!(
            cast(CastSrc::Text, CastDst::Float8, text("1e999")).unwrap_err(),
            SqlSurfaceError::NumericOutOfRange { .. }
        ));
        assert!(matches!(
            cast(CastSrc::Text, CastDst::Float8, text("1e-999")).unwrap_err(),
            SqlSurfaceError::NumericOutOfRange { .. }
        ));
        for word in ["NaN", "Infinity", "-inf", "+INF"] {
            assert!(
                matches!(
                    cast(CastSrc::Text, CastDst::Float8, text(word)).unwrap_err(),
                    SqlSurfaceError::FeatureNotSupported { .. }
                ),
                "{word}"
            );
            assert!(
                matches!(
                    cast(CastSrc::Text, CastDst::Numeric(None), text(word)).unwrap_err(),
                    SqlSurfaceError::FeatureNotSupported { .. }
                ),
                "{word}"
            );
        }
    }

    #[test]
    fn numeric_typmod_rounds_and_overflows() {
        let t = Some((3, 1));
        assert_eq!(
            numeric_text(cast(
                CastSrc::Numeric,
                CastDst::Numeric(t),
                ExprValue::Numeric(dec("1.25"))
            )),
            "1.3"
        );
        assert_eq!(
            numeric_text(cast(
                CastSrc::Numeric,
                CastDst::Numeric(t),
                ExprValue::Numeric(dec("-1.25"))
            )),
            "-1.3"
        );
        assert!(matches!(
            cast(
                CastSrc::Numeric,
                CastDst::Numeric(t),
                ExprValue::Numeric(dec("99.95"))
            )
            .unwrap_err(),
            SqlSurfaceError::NumericOutOfRange { .. }
        ));
        assert_eq!(
            numeric_text(cast(
                CastSrc::Int4,
                CastDst::Numeric(Some((5, 2))),
                ExprValue::Scalar(7.0)
            )),
            "7.00"
        );
        assert!(cast(
            CastSrc::Int4,
            CastDst::Numeric(Some((3, 2))),
            ExprValue::Scalar(10.0)
        )
        .is_err());
        assert_eq!(
            numeric_text(cast(
                CastSrc::Text,
                CastDst::Numeric(Some((5, 2))),
                text("3.14159")
            )),
            "3.14"
        );
        assert_eq!(
            numeric_text(cast(CastSrc::Text, CastDst::Numeric(None), text(" 3.140 "))),
            "3.140"
        );
    }

    #[test]
    fn float_to_numeric_uses_significant_digit_representation() {
        assert_eq!(
            numeric_text(cast(
                CastSrc::Float8,
                CastDst::Numeric(None),
                ExprValue::Scalar(1.5)
            )),
            "1.5"
        );
        assert_eq!(
            numeric_text(cast(
                CastSrc::Float8,
                CastDst::Numeric(None),
                ExprValue::Scalar(100.0)
            )),
            "100"
        );
        assert_eq!(
            numeric_text(cast(
                CastSrc::Float8,
                CastDst::Numeric(None),
                ExprValue::Scalar(0.1)
            )),
            "0.1"
        );
        assert_eq!(
            numeric_text(cast(
                CastSrc::Float8,
                CastDst::Numeric(None),
                ExprValue::Scalar(1.0 / 3.0)
            )),
            "0.333333333333333"
        );
        assert_eq!(
            numeric_text(cast(
                CastSrc::Float4,
                CastDst::Numeric(None),
                ExprValue::Scalar(f64::from(0.1f32))
            )),
            "0.1"
        );
        assert_eq!(
            numeric_text(cast(
                CastSrc::Float8,
                CastDst::Numeric(None),
                ExprValue::Scalar(-0.0)
            )),
            "0"
        );
        assert!(cast(
            CastSrc::Float8,
            CastDst::Numeric(None),
            ExprValue::Scalar(1e300)
        )
        .is_err());
    }

    #[test]
    fn bool_conversions() {
        assert_eq!(
            cast(CastSrc::Int4, CastDst::Bool, ExprValue::Scalar(0.0)).expect("ok"),
            ExprValue::Bool(false)
        );
        assert_eq!(
            cast(CastSrc::Int4, CastDst::Bool, ExprValue::Scalar(-3.0)).expect("ok"),
            ExprValue::Bool(true)
        );
        assert_eq!(
            scalar(cast(CastSrc::Bool, CastDst::Int4, ExprValue::Bool(true))),
            1.0
        );
        assert_eq!(
            scalar(cast(CastSrc::Bool, CastDst::Int4, ExprValue::Bool(false))),
            0.0
        );
        assert_eq!(
            as_text(cast(CastSrc::Bool, CastDst::Text, ExprValue::Bool(true))),
            "true"
        );
        assert_eq!(
            cast(CastSrc::Text, CastDst::Bool, text(" TRUE ")).expect("ok"),
            ExprValue::Bool(true)
        );
        assert_eq!(
            cast(CastSrc::Text, CastDst::Bool, text("off")).expect("ok"),
            ExprValue::Bool(false)
        );
        let e = cast(CastSrc::Text, CastDst::Bool, text("maybe")).unwrap_err();
        assert!(
            format!("{e}").ends_with("invalid input syntax for type boolean: \"maybe\""),
            "{e}"
        );
    }

    #[test]
    fn to_text_uses_pg_output_forms() {
        assert_eq!(
            as_text(cast(CastSrc::Int4, CastDst::Text, ExprValue::Scalar(-12.0))),
            "-12"
        );
        assert_eq!(
            as_text(cast(CastSrc::Float8, CastDst::Text, ExprValue::Scalar(1.5))),
            "1.5"
        );
        assert_eq!(
            as_text(cast(
                CastSrc::Float4,
                CastDst::Text,
                ExprValue::Scalar(f64::from(0.1f32))
            )),
            "0.1"
        );
        assert_eq!(
            as_text(cast(
                CastSrc::Numeric,
                CastDst::Text,
                ExprValue::Numeric(dec("2.50"))
            )),
            "2.50"
        );
    }

    #[test]
    fn char_typmods_truncate_and_pad() {
        assert_eq!(
            as_text(cast(
                CastSrc::Text,
                CastDst::Varchar(Some(3)),
                text("abcdef")
            )),
            "abc"
        );
        assert_eq!(
            as_text(cast(CastSrc::Text, CastDst::Varchar(Some(10)), text("abc"))),
            "abc"
        );
        assert_eq!(
            as_text(cast(CastSrc::Text, CastDst::Bpchar(5), text("ab"))),
            "ab   "
        );
        assert_eq!(
            as_text(cast(CastSrc::Text, CastDst::Bpchar(2), text("abcdef"))),
            "ab"
        );
        // 文字数で数える（マルチバイトの途中で切らない）。
        assert_eq!(
            as_text(cast(
                CastSrc::Text,
                CastDst::Varchar(Some(2)),
                text("日本語")
            )),
            "日本"
        );
        assert_eq!(
            as_text(cast(
                CastSrc::Int4,
                CastDst::Bpchar(4),
                ExprValue::Scalar(7.0)
            )),
            "7   "
        );
    }

    #[test]
    fn noop_classification() {
        let f = |from, to| CastFn { from, to };
        assert!(f(CastSrc::Int2, CastDst::Int8).is_noop());
        assert!(f(CastSrc::Int8, CastDst::Float8).is_noop());
        assert!(f(CastSrc::Float4, CastDst::Float8).is_noop());
        assert!(!f(CastSrc::Int8, CastDst::Int4).is_noop());
        assert!(!f(CastSrc::Int4, CastDst::Float4).is_noop());
        assert!(!f(CastSrc::Numeric, CastDst::Numeric(Some((5, 2)))).is_noop());
        assert!(f(CastSrc::Text, CastDst::Varchar(None)).is_noop());
        assert!(!f(CastSrc::Text, CastDst::Bpchar(3)).is_noop());
    }

    #[test]
    fn noop_classification_for_ext_types() {
        let f = |from, to| CastFn { from, to };
        assert!(f(CastSrc::Date, CastDst::Date).is_noop());
        assert!(f(CastSrc::Uuid, CastDst::Uuid).is_noop());
        assert!(f(
            CastSrc::Array(ArrayElemType::Integer),
            CastDst::Array(ArrayElemType::Integer)
        )
        .is_noop());
        // 値を変える変換は noop にしない（json -> jsonb は正規化、型が変わる組は別の値）。
        assert!(!f(CastSrc::Json, CastDst::Jsonb).is_noop());
        assert!(!f(CastSrc::Date, CastDst::Timestamp).is_noop());
        assert!(!f(CastSrc::Uuid, CastDst::Text).is_noop());
        assert!(!f(
            CastSrc::Array(ArrayElemType::Integer),
            CastDst::Array(ArrayElemType::BigInt)
        )
        .is_noop());
    }

    #[test]
    fn bytea_to_text_over_the_text_limit_is_rejected_before_allocation() {
        // 4 MiB の TEXT 上限に対し、2 MiB + 1 バイトの bytea は 16 進で上限を超える。
        let max = crate::row_codec::MAX_TEXT_FIELD_LEN as usize;
        let big = ExprValue::Bytes(Cow::Owned(vec![0u8; max / 2 + 1]));
        let e = cast(CastSrc::Bytea, CastDst::Text, big).expect_err("too long");
        assert_eq!(e.wire_code(), "54000");
        let exact = ExprValue::Bytes(Cow::Owned(vec![0u8; (max - 2) / 2]));
        assert_eq!(
            as_text(cast(CastSrc::Bytea, CastDst::Text, exact)).len(),
            max - (max - 2) % 2
        );
    }

    #[test]
    fn array_to_text_is_bounded() {
        // 1 MiB の BYTEA 要素 3 個の配列は文字列化すると TEXT の上限を超える。
        let items = vec![Some(vec![0u8; 1 << 20]); 3];
        let e = cast(
            CastSrc::Array(ArrayElemType::Bytea),
            CastDst::Text,
            ExprValue::Array(ArrayValue::Bytea(items)),
        )
        .expect_err("too long");
        assert_eq!(e.wire_code(), "54000");
    }

    #[test]
    fn array_element_conversion_keeps_nulls_and_element_errors() {
        let src = ExprValue::Array(ArrayValue::Integer(vec![Some(1), None, Some(300)]));
        let out = cast(
            CastSrc::Array(ArrayElemType::Integer),
            CastDst::Array(ArrayElemType::BigInt),
            src.clone(),
        )
        .expect("widen");
        assert_eq!(
            out,
            ExprValue::Array(ArrayValue::BigInt(vec![Some(1), None, Some(300)]))
        );
        // 数値配列の縮小変換の失敗はその要素の SQLSTATE（ここでは numeric(2,0) の桁あふれ）
        let nums = ExprValue::Array(ArrayValue::Integer(vec![Some(1), Some(300)]));
        let e = cast(
            CastSrc::Array(ArrayElemType::Integer),
            CastDst::Array(ArrayElemType::Numeric {
                precision: 2,
                scale: 0,
            }),
            nums,
        )
        .expect_err("overflow");
        assert_eq!(e.wire_code(), "22003");
        // 空配列も要素型を変えて通る
        let empty = cast(
            CastSrc::Array(ArrayElemType::Integer),
            CastDst::Array(ArrayElemType::Text),
            ExprValue::Array(ArrayValue::Integer(vec![])),
        )
        .expect("empty");
        assert_eq!(empty, ExprValue::Array(ArrayValue::Text(vec![])));
    }

    #[test]
    fn calendar_boundaries_for_timestamp_to_date_and_time() {
        // エポック前でも日は負の側へ丸め、時刻は 0 以上の剰余になる。
        let t = crate::datetime::parse_timestamp("1969-12-31 23:59:59").expect("ts");
        assert_eq!(
            cast(CastSrc::Timestamp, CastDst::Date, ExprValue::Timestamp(t)).expect("date"),
            ExprValue::Date(-1)
        );
        assert_eq!(
            cast(CastSrc::Timestamp, CastDst::Time, ExprValue::Timestamp(t)).expect("time"),
            ExprValue::Time(86_399_000_000)
        );
        // 24:00:00 -> interval -> time は 00:00:00
        let iv = cast(
            CastSrc::Time,
            CastDst::Interval,
            ExprValue::Time(86_400_000_000),
        )
        .expect("interval");
        assert_eq!(
            cast(CastSrc::Interval, CastDst::Time, iv).expect("time"),
            ExprValue::Time(0)
        );
    }

    #[test]
    fn round_to_integer_helper() {
        assert_eq!(numeric::round_to_integer(&dec("0.5")), Some(1));
        assert_eq!(numeric::round_to_integer(&dec("-0.5")), Some(-1));
        assert_eq!(numeric::round_to_integer(&dec("0.49")), Some(0));
        assert_eq!(numeric::round_to_integer(&dec("12")), Some(12));
    }
}
