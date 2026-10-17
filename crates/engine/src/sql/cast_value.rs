//! `CAST` の値変換（数値・文字列・BOOLEAN の 10 型どうし。Issue #1591・SQL-47 のポインタ）。
//!
//! 責務境界: 束縛段（`sql::cast::bind_cast`）が規則表から [`CastFn`]（Copy な変換記述子）を
//! 組み立てて `BuiltinFn::Cast` に載せ、評価段（`sql::udf_call::apply_builtin`）が
//! [`apply_cast`] を呼ぶ。型なし文字列リテラルの束縛（`bind_string_literal_as`）も同じ
//! 入力関数を束縛時に 1 度呼んで形式不正・範囲外を早期に報告する（第 2 の文法を作らない）。
//!
//! 規則の拠り所は `docs/design/cast-conversion-rules.md`（PostgreSQL 17 の公開挙動）。
//! - 整数への変換: 浮動小数は偶数丸め、numeric は 0 から遠い側への四捨五入の後に範囲検査（`22003`）。
//! - 文字列からの変換: 形式不正は `22P02`、範囲外は `22003`。本エンジンが値として持たない
//!   `NaN`／`Infinity` 入力は `0A000`（fail-closed。`22P02` にもしない）。
//! - 式評価は整数も f64 で運ぶため、|v| > 2^53 の int8 は丸めずに `22003` で拒否する
//!   （PostgreSQL との既知の差分。設計書に記録）。
//!
//! エラー文言は固定語彙＋呼び出し元が与えた値（先頭 [`ECHO_LIMIT`] 文字まで）に限り、
//! 他テナントの値・存在情報を含めない（CAST は RLS を通過した可視行の値にだけ作用する）。

use std::borrow::Cow;

use crate::catalog::MAX_CHAR_TYPE_LENGTH;
use crate::numeric::{self, Decimal, NumericError};
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
        )
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
    let src = extract_src(f.from, v)?;
    let single_src = f.from == CastSrc::Float4;
    match f.to {
        CastDst::Int2 | CastDst::Int4 | CastDst::Int8 => {
            let w = IntWidth::from_dst(f.to).ok_or_else(|| internal("integer width"))?;
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
            Ok(ExprValue::Text(apply_char_typmod(s, f.to)?))
        }
    }
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
    fn round_to_integer_helper() {
        assert_eq!(numeric::round_to_integer(&dec("0.5")), Some(1));
        assert_eq!(numeric::round_to_integer(&dec("-0.5")), Some(-1));
        assert_eq!(numeric::round_to_integer(&dec("0.49")), Some(0));
        assert_eq!(numeric::round_to_integer(&dec("12")), Some(12));
    }
}
