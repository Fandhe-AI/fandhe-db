//! `REAL`／`DOUBLE PRECISION` 列型（TABLE-13・TASK-196）の値表現に閉じた解析・整形・
//! 順序規約を集約するモジュール。
//!
//! 責務境界: SQL 表層（`sql::allowlist`・`sql::parser`）がリテラルトークンを
//! [`parse_real`]／[`parse_double`] で値へ束縛し、投影・wire 応答（`sql::exec`・
//! wire-server）が [`format_real`]／[`format_double`] でテキスト表現へ戻す。
//! `row_codec` の encode/decode は本モジュールの値表現（正規化済みの有限値）を
//! 前提とし、非有限値・符号付きゼロの扱いは呼び出し元ではなくここで一元的に
//! 決める（型を追加するたびに複数箇所で同じ判断を重複させない）。
//!
//! - 非有限値（NaN・±∞）は受け付けない（fail-closed）。
//! - `-0.0` は `+0.0` へ正規化する（符号付きゼロは保持しない）。
//! - 比較・ソートは [`cmp_real`]／[`cmp_double`] が唯一の情報源（`total_cmp` を
//!   基準にしつつ `-0.0 == +0.0` を保つ）。`NULL` の並び位置は呼び出し側の責務。
//! - テキスト表現は Rust の `Display`（指数表記を出さない最短往復表記）を正準とし、
//!   `f32`／`f64` は互いを経由せず直接その型の `FromStr` で解析する（二重丸め防止）。
//!
//! 指数表記リテラル（`1.5e3`）は Issue #1187 で受理する（PostgreSQL と整合）。
//! 対象外（申し送り）: PostgreSQL 既定出力の再現・文字列リテラルからの暗黙変換は
//! WIRE-13／後続 Issue の担当（Issue #882 計画 §7）。

use std::cmp::Ordering;

/// リテラル解析エラー。SQL 表層が `wire_code` へ写像する（構文違反は
/// `InvalidInput`、範囲外・アンダーフローは `NumericOutOfRange`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseFloatError {
    /// 閉じた文法（`^-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?$`）に適合しない入力
    /// （SQL 表層では `22P02`）。
    Malformed,
    /// 非有限化（オーバーフロー）・非ゼロ入力のアンダーフロー。
    OutOfRange,
}

/// 受理する数値リテラルの文法（符号任意・整数部必須・小数部任意・指数部任意。
/// `-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?`）。`from_str` がそのまま受理してしまう
/// `inf`／`nan`／`infinity` や `1.`・`.5` 等を閉じるための自前検査
/// （F7・Issue #882 計画。指数表記の受理は Issue #1187 で追加）。
fn is_well_formed_literal(input: &str) -> bool {
    let body = input.strip_prefix('-').unwrap_or(input);
    let (mantissa, exponent) = split_exponent(body);
    if mantissa.is_empty() {
        return false;
    }
    let mut parts = mantissa.splitn(2, '.');
    let int_part = parts.next().unwrap_or("");
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some(frac_part) = parts.next() {
        if frac_part.is_empty() || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    match exponent {
        None => true,
        Some(exp) => {
            let digits = exp.strip_prefix(['+', '-']).unwrap_or(exp);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

/// `e`／`E` で仮数部と指数部へ分割する（指数部が無ければ `None`）。
fn split_exponent(body: &str) -> (&str, Option<&str>) {
    match body.find(['e', 'E']) {
        Some(idx) => (
            body.get(..idx).unwrap_or(""),
            Some(body.get(idx + 1..).unwrap_or("")),
        ),
        None => (body, None),
    }
}

/// 非ゼロの入力文字列が指す値が数学的に 0 かどうか（アンダーフロー検出用の
/// 文字列側判定。パース後の値が 0.0 だけでは「元々 0 と書かれていた」のか
/// 「アンダーフローした」のかを区別できないため、文字列上で仮数部の全桁が 0 かを
/// 見る。指数部は値の大きさだけを変えるため判定対象外: `0e5` は 0）。
fn literal_is_textually_zero(input: &str) -> bool {
    let body = input.strip_prefix('-').unwrap_or(input);
    let (mantissa, _) = split_exponent(body);
    mantissa.bytes().all(|b| b == b'.' || b == b'0')
}

/// `REAL`（f32）リテラルを解析する。文法検証 → `f32::from_str` → 非有限・
/// 非ゼロアンダーフローの拒否 → `-0.0` の正規化、の順で行う。
pub fn parse_real(input: &str) -> Result<f32, ParseFloatError> {
    if !is_well_formed_literal(input) {
        return Err(ParseFloatError::Malformed);
    }
    let value: f32 = input.parse().map_err(|_| ParseFloatError::Malformed)?;
    if !value.is_finite() {
        return Err(ParseFloatError::OutOfRange);
    }
    if value == 0.0 && !literal_is_textually_zero(input) {
        return Err(ParseFloatError::OutOfRange);
    }
    Ok(canonicalize_real(value))
}

/// `DOUBLE PRECISION`（f64）リテラルを解析する。[`parse_real`] と同じ手順。
pub fn parse_double(input: &str) -> Result<f64, ParseFloatError> {
    if !is_well_formed_literal(input) {
        return Err(ParseFloatError::Malformed);
    }
    let value: f64 = input.parse().map_err(|_| ParseFloatError::Malformed)?;
    if !value.is_finite() {
        return Err(ParseFloatError::OutOfRange);
    }
    if value == 0.0 && !literal_is_textually_zero(input) {
        return Err(ParseFloatError::OutOfRange);
    }
    Ok(canonicalize_double(value))
}

/// `-0.0` を `+0.0` へ正規化する（F4）。他の値はそのまま返す。
pub fn canonicalize_real(value: f32) -> f32 {
    if value == 0.0 {
        0.0f32
    } else {
        value
    }
}

/// [`canonicalize_real`] の f64 版。
pub fn canonicalize_double(value: f64) -> f64 {
    if value == 0.0 {
        0.0f64
    } else {
        value
    }
}

/// `REAL` 値の正準テキスト表現（指数表記を出さない `Display`。最短往復表記）。
pub fn format_real(value: f32) -> String {
    format!("{value}")
}

/// `DOUBLE PRECISION` 値の正準テキスト表現。
pub fn format_double(value: f64) -> String {
    format!("{value}")
}

/// `REAL` 値の全順序比較（F5）。`a == b`（`-0.0 == +0.0` を含む IEEE 754 の
/// 等価性）を優先し、そうでなければ `total_cmp` で決定的な全順序をつける。
/// 呼び出し側は `NULL` の並び位置を別途決める（本関数は非 NULL 値のみを扱う）。
pub fn cmp_real(a: f32, b: f32) -> Ordering {
    if a == b {
        Ordering::Equal
    } else {
        a.total_cmp(&b)
    }
}

/// [`cmp_real`] の f64 版。
pub fn cmp_double(a: f64, b: f64) -> Ordering {
    if a == b {
        Ordering::Equal
    } else {
        a.total_cmp(&b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(mut x: u64) -> impl FnMut() -> u64 {
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        }
    }

    #[test]
    fn parse_real_roundtrips_boundary_values() {
        let cases: &[f32] = &[
            f32::MIN,
            f32::MAX,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            0.1,
            1.0 / 3.0,
            0.0,
        ];
        for &v in cases {
            let text = format_real(v);
            let parsed = parse_real(&text).expect("roundtrip parse");
            assert_eq!(
                parsed.to_bits(),
                canonicalize_real(v).to_bits(),
                "roundtrip mismatch for {v}"
            );
        }
    }

    #[test]
    fn parse_double_roundtrips_boundary_values() {
        let cases: &[f64] = &[f64::MIN, f64::MAX, 5e-324, 0.1, 1.0 / 3.0, 0.0];
        for &v in cases {
            let text = format_double(v);
            let parsed = parse_double(&text).expect("roundtrip parse");
            assert_eq!(
                parsed.to_bits(),
                canonicalize_double(v).to_bits(),
                "roundtrip mismatch for {v}"
            );
        }
    }

    #[test]
    fn parse_real_roundtrips_bit_pattern_sweep() {
        let mut rng = xorshift(0x1234_5678_9abc_def0);
        let mut checked = 0;
        while checked < 20_000 {
            let bits = (rng() & 0xFFFF_FFFF) as u32;
            let v = f32::from_bits(bits);
            if !v.is_finite() {
                continue;
            }
            let v = canonicalize_real(v);
            let text = format_real(v);
            let parsed = parse_real(&text).expect("sweep parse");
            assert_eq!(parsed.to_bits(), v.to_bits());
            checked += 1;
        }
    }

    #[test]
    fn parse_rejects_non_finite_and_malformed_literals() {
        for bad in [
            "NaN", "nan", "inf", "-inf", "infinity", "Infinity", "+1", "1.", ".5", "1e", "e5",
            "1e+", "1e-", "1.e3", ".5e1", "1e5.5", "0x1p3", "", "-", "1..5", "1.2.3",
        ] {
            assert_eq!(
                parse_real(bad),
                Err(ParseFloatError::Malformed),
                "expected Malformed for {bad:?}"
            );
            assert_eq!(
                parse_double(bad),
                Err(ParseFloatError::Malformed),
                "expected Malformed for {bad:?}"
            );
        }
    }

    #[test]
    fn parse_accepts_exponent_notation() {
        assert_eq!(parse_real("1.5e3"), Ok(1500.0));
        assert_eq!(parse_real("1E+2"), Ok(100.0));
        assert_eq!(parse_double("-1.5e-3"), Ok(-0.0015));
        // 仮数部が 0 なら指数部に関わらず 0（アンダーフロー扱いしない）。
        assert_eq!(parse_real("0e5"), Ok(0.0));
        assert_eq!(parse_double("0.0E-400"), Ok(0.0));
    }

    #[test]
    fn parse_rejects_exponent_overflow_and_underflow() {
        assert_eq!(parse_real("1e-50"), Err(ParseFloatError::OutOfRange));
        assert_eq!(parse_real("1e999"), Err(ParseFloatError::OutOfRange));
        assert_eq!(parse_double("1e999"), Err(ParseFloatError::OutOfRange));
        assert_eq!(parse_double("1e-999"), Err(ParseFloatError::OutOfRange));
    }

    #[test]
    fn parse_rejects_overflow_and_underflow() {
        let huge = format!("4{}", "0".repeat(39));
        assert_eq!(parse_real(&huge), Err(ParseFloatError::OutOfRange));

        let huge_double = format!("1{}", "0".repeat(320));
        assert_eq!(parse_double(&huge_double), Err(ParseFloatError::OutOfRange));

        // f32 の最小非正規化数（約 1.4e-45）未満に潰れる非ゼロ値はアンダーフロー
        // として拒否する（45 桁のゼロでは 1e-45 相当となり非正規化数として
        // 表現可能なため、余裕を持たせた桁数にする）。
        let tiny = "0.".to_string() + &"0".repeat(60) + "1";
        assert_eq!(parse_real(&tiny), Err(ParseFloatError::OutOfRange));
    }

    #[test]
    fn parse_normalizes_negative_zero() {
        assert_eq!(parse_real("-0").unwrap().to_bits(), 0.0f32.to_bits());
        assert_eq!(parse_real("-0.0").unwrap().to_bits(), 0.0f32.to_bits());
        assert_eq!(parse_double("-0.0").unwrap().to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn cmp_real_is_total_order_and_treats_signed_zero_as_equal() {
        // `-0.0 == 0.0` を優先する [`cmp_real`] は、両者を区別する `total_cmp`
        // 単独のソート結果とビット列まで一致するとは限らない（安定ソートが
        // 同値要素の元の並び順を保つため）。ここでは数値としての昇順（0 は
        // 1 箇所にまとまる）を検証し、`-0.0`/`0.0` 自体の判別は個別に確認する。
        let mut values = vec![f32::MAX, -1.0, 0.0, -0.0, 1.0, f32::MIN, f32::MIN_POSITIVE];
        values.sort_by(|a, b| cmp_real(*a, *b));
        let expected_numeric = [f32::MIN, -1.0, 0.0, 0.0, f32::MIN_POSITIVE, 1.0, f32::MAX];
        assert_eq!(values, expected_numeric);
        assert_eq!(cmp_real(0.0, -0.0), Ordering::Equal);
        assert_eq!(cmp_real(f32::MIN, f32::MAX), Ordering::Less);
        assert_eq!(cmp_real(f32::MAX, f32::MIN), Ordering::Greater);
    }

    #[test]
    fn cmp_double_is_deterministic_even_with_nan_input() {
        // 呼び出し側が防御的に NaN を渡しても、決定的な位置に置かれる
        // （total_cmp の全順序契約）。
        assert_ne!(cmp_double(f64::NAN, 0.0), Ordering::Equal);
        assert_eq!(cmp_double(f64::NAN, f64::NAN), Ordering::Equal);
    }
}
