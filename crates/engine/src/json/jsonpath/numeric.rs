//! jsonpath 評価器の数値演算（`eval.rs` から呼ばれる。Issue #2184・TABLE-25 ポインタ）。
//!
//! 責務: 算術（`+ - * / %`・単項 `-`）と数値メソッド（`.abs()`・`.floor()`・`.ceiling()`・
//! `.integer()`・`.bigint()`・`.decimal(p,s)`）を `crate::numeric::Decimal`（`i128` +
//! `scale`）の厳密演算で行う。比較は `json::cmp_json_numbers`（桁文字列、`f64` 非経由）
//! が担い、本モジュールは関与しない。
//!
//! PostgreSQL の `numeric` は任意精度だが、本実装は `i128` に収まる範囲（`scale` は 38
//! 以下）に限る。オペランドや結果がこれに収まらない場合は `Overflow`（呼び出し側が
//! `22003` へ写像）とする。これは意図的な差分で、`docs/design/json-expr-type-mapping.md`
//! に記録する。除算の結果 `scale` は PostgreSQL の `select_div_scale`（有効桁 16 以上・
//! 入力の `scale` 以上）に合わせ、丸めは 0 から遠い側へ（half away from zero）。
//! すべての整数演算は `checked_*` で行い、`unwrap`／添字アクセスを使わない。

use std::cmp::Ordering;

use crate::json::JsonNumber;
use crate::numeric::{parse_literal_exact, Decimal};

/// 演算の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumFail {
    /// `i128`／`scale <= 38` に収まらない（`22003`）。
    Overflow,
    /// 0 による除算・剰余（`22012`）。
    DivisionByZero,
}

/// `10^n`（`n <= 38`）。範囲外は `None`。
fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

fn make(unscaled: i128, scale: i64) -> Result<Decimal, NumFail> {
    let scale = u8::try_from(scale).map_err(|_| NumFail::Overflow)?;
    Decimal::from_parts(unscaled, scale).map_err(|_| NumFail::Overflow)
}

/// [`JsonNumber`] を [`Decimal`] へ厳密に変換する。`1e400` のように収まらない値は `None`。
pub(super) fn from_json_number(n: &JsonNumber) -> Option<Decimal> {
    match n {
        JsonNumber::PosInt(v) => Decimal::from_parts(i128::from(*v), 0).ok(),
        JsonNumber::NegInt(v) => Decimal::from_parts(i128::from(*v), 0).ok(),
        JsonNumber::Float { text, .. } => parse_literal_exact(text).ok(),
    }
}

/// [`Decimal`] を [`JsonNumber`] へ変換する（`scale == 0` かつ 64 bit に収まれば整数
/// variant、それ以外は `Display` の正規テキストを持つ `Float`）。
pub(super) fn to_json_number(d: Decimal) -> JsonNumber {
    if d.scale() == 0 {
        if let Ok(v) = u64::try_from(d.unscaled()) {
            return JsonNumber::PosInt(v);
        }
        if let Ok(v) = i64::try_from(d.unscaled()) {
            return JsonNumber::NegInt(v);
        }
    }
    let text = d.to_string();
    // `Display` は常に有効な 10 進テキストを返すため解析は失敗しない。
    let value = text.parse::<f64>().unwrap_or(0.0);
    JsonNumber::Float {
        value,
        text: text.into_boxed_str(),
    }
}

/// 2 つの `Decimal` を共通 `scale` へ揃えた `(a, b, scale)`。
fn align(a: &Decimal, b: &Decimal) -> Result<(i128, i128, u8), NumFail> {
    let scale = a.scale().max(b.scale());
    let fa = pow10(u32::from(scale - a.scale())).ok_or(NumFail::Overflow)?;
    let fb = pow10(u32::from(scale - b.scale())).ok_or(NumFail::Overflow)?;
    let x = a.unscaled().checked_mul(fa).ok_or(NumFail::Overflow)?;
    let y = b.unscaled().checked_mul(fb).ok_or(NumFail::Overflow)?;
    Ok((x, y, scale))
}

pub(super) fn add(a: &Decimal, b: &Decimal) -> Result<Decimal, NumFail> {
    let (x, y, s) = align(a, b)?;
    make(x.checked_add(y).ok_or(NumFail::Overflow)?, i64::from(s))
}

pub(super) fn sub(a: &Decimal, b: &Decimal) -> Result<Decimal, NumFail> {
    let (x, y, s) = align(a, b)?;
    make(x.checked_sub(y).ok_or(NumFail::Overflow)?, i64::from(s))
}

pub(super) fn mul(a: &Decimal, b: &Decimal) -> Result<Decimal, NumFail> {
    let u = a
        .unscaled()
        .checked_mul(b.unscaled())
        .ok_or(NumFail::Overflow)?;
    make(u, i64::from(a.scale()) + i64::from(b.scale()))
}

pub(super) fn neg(a: &Decimal) -> Result<Decimal, NumFail> {
    make(
        a.unscaled().checked_neg().ok_or(NumFail::Overflow)?,
        i64::from(a.scale()),
    )
}

/// `%`。結果の符号は被除数、`scale` は大きい方（PostgreSQL の `numeric_mod`）。
pub(super) fn rem(a: &Decimal, b: &Decimal) -> Result<Decimal, NumFail> {
    let (x, y, s) = align(a, b)?;
    if y == 0 {
        return Err(NumFail::DivisionByZero);
    }
    make(x.checked_rem(y).ok_or(NumFail::Overflow)?, i64::from(s))
}

/// 10 進の先頭桁を基数 10000 の桁として見たときの `(weight, 先頭桁)`（0 は `(0, 0)`）。
/// PostgreSQL の `select_div_scale` が結果 `scale` の推定に使う量。
fn weight_and_first(d: &Decimal) -> (i64, i128) {
    let u = d.unscaled().unsigned_abs();
    if u == 0 {
        return (0, 0);
    }
    let digits = i64::from(u.ilog10()) + 1;
    let e = digits - 1 - i64::from(d.scale());
    let w = e.div_euclid(4);
    // 先頭の基数 10000 桁に含まれる 10 進桁数（1..=4）。
    let lead_len = e - 4 * w + 1;
    let first = if digits >= lead_len {
        let shift = u32::try_from(digits - lead_len).unwrap_or(0);
        let div = 10u128.checked_pow(shift).unwrap_or(1);
        i128::try_from(u / div).unwrap_or(0)
    } else {
        let shift = u32::try_from(lead_len - digits).unwrap_or(0);
        let mul = 10u128.checked_pow(shift).unwrap_or(1);
        i128::try_from(u.saturating_mul(mul)).unwrap_or(0)
    };
    (w, first)
}

/// `/`。結果 `scale` は PostgreSQL の `select_div_scale`、丸めは half away from zero。
pub(super) fn div(a: &Decimal, b: &Decimal) -> Result<Decimal, NumFail> {
    if b.unscaled() == 0 {
        return Err(NumFail::DivisionByZero);
    }
    let (w1, d1) = weight_and_first(a);
    let (w2, d2) = weight_and_first(b);
    let mut qweight = w1 - w2;
    if d1 <= d2 {
        qweight -= 1;
    }
    let rscale = (16 - qweight * 4)
        .max(i64::from(a.scale()))
        .max(i64::from(b.scale()))
        .max(0);
    // 結果の `unscaled` = round(a.unscaled * 10^(rscale + b.scale - a.scale) / b.unscaled)。
    let k = rscale + i64::from(b.scale()) - i64::from(a.scale());
    let mut num = a.unscaled().unsigned_abs();
    let mut den = b.unscaled().unsigned_abs();
    if k >= 0 {
        let f = 10u128
            .checked_pow(u32::try_from(k).map_err(|_| NumFail::Overflow)?)
            .ok_or(NumFail::Overflow)?;
        num = num.checked_mul(f).ok_or(NumFail::Overflow)?;
    } else {
        let f = 10u128
            .checked_pow(u32::try_from(-k).map_err(|_| NumFail::Overflow)?)
            .ok_or(NumFail::Overflow)?;
        den = den.checked_mul(f).ok_or(NumFail::Overflow)?;
    }
    let mut q = num / den;
    let r = num % den;
    // half away from zero: 2r >= den なら絶対値を 1 増やす（2r は den <= u128::MAX/2 でなくても
    // 比較を `r >= den - r` で行ってオーバーフローを避ける）。
    if r >= den - r {
        q = q.checked_add(1).ok_or(NumFail::Overflow)?;
    }
    let q = i128::try_from(q).map_err(|_| NumFail::Overflow)?;
    let negative = (a.unscaled() < 0) != (b.unscaled() < 0);
    let q = if negative {
        q.checked_neg().ok_or(NumFail::Overflow)?
    } else {
        q
    };
    make(q, rscale)
}

/// 床（負の無限大方向）。結果の `scale` は 0。
pub(super) fn floor(a: &Decimal) -> Result<Decimal, NumFail> {
    let f = pow10(u32::from(a.scale())).ok_or(NumFail::Overflow)?;
    make(a.unscaled().div_euclid(f), 0)
}

/// 天井（正の無限大方向）。結果の `scale` は 0。
pub(super) fn ceil(a: &Decimal) -> Result<Decimal, NumFail> {
    let f = pow10(u32::from(a.scale())).ok_or(NumFail::Overflow)?;
    let q = a.unscaled().div_euclid(f);
    let r = a.unscaled().rem_euclid(f);
    let q = if r != 0 {
        q.checked_add(1).ok_or(NumFail::Overflow)?
    } else {
        q
    };
    make(q, 0)
}

/// 0 方向への切り捨て（配列添字の `numeric_trunc(x, 0)` 相当）。
pub(super) fn trunc_to_i128(a: &Decimal) -> Option<i128> {
    let f = pow10(u32::from(a.scale()))?;
    a.unscaled().checked_div(f)
}

/// 整数への丸め（half away from zero。`numeric_int8`／`numeric_int4` と同じ）。
pub(super) fn round_to_i128(a: &Decimal) -> Option<i128> {
    let f = pow10(u32::from(a.scale()))?;
    let q = a.unscaled().checked_div(f)?;
    let r = a.unscaled().checked_rem(f)?;
    // |r| * 2 >= f なら 0 から遠い側へ。`f` は 10 の冪（>= 1）。
    let twice = r.checked_abs()?.checked_mul(2)?;
    if twice >= f {
        if a.unscaled() < 0 {
            q.checked_sub(1)
        } else {
            q.checked_add(1)
        }
    } else {
        Some(q)
    }
}

/// `.decimal(p, s)` の適用（`numeric(p, s)` への丸め込み）。`scale` が負なら 10 の冪へ
/// 丸める。丸め後の桁数が `p` を超える、または本実装の表現範囲に収まらないときは `Err`。
pub(super) fn apply_precision_scale(a: &Decimal, p: i64, s: i64) -> Result<Decimal, NumFail> {
    let cur = i64::from(a.scale());
    // 丸め後の「s 位置の整数」(= round(value * 10^s))。
    let scaled: i128 = match s.cmp(&cur) {
        Ordering::Equal => a.unscaled(),
        Ordering::Greater => {
            let f = pow10(u32::try_from(s - cur).map_err(|_| NumFail::Overflow)?)
                .ok_or(NumFail::Overflow)?;
            a.unscaled().checked_mul(f).ok_or(NumFail::Overflow)?
        }
        Ordering::Less => {
            let drop = u32::try_from(cur - s).map_err(|_| NumFail::Overflow)?;
            // 切り捨てる桁数が 38 を超えるなら絶対値は 1 未満なので 0 に丸まる。
            match pow10(drop) {
                Some(f) => {
                    let q = a.unscaled() / f;
                    let r = a.unscaled() % f;
                    let twice = r.checked_abs().ok_or(NumFail::Overflow)?.checked_mul(2);
                    let up = matches!(twice, Some(t) if t >= f);
                    if up {
                        if a.unscaled() < 0 {
                            q.checked_sub(1).ok_or(NumFail::Overflow)?
                        } else {
                            q.checked_add(1).ok_or(NumFail::Overflow)?
                        }
                    } else {
                        q
                    }
                }
                None => 0,
            }
        }
    };
    // 精度検査: 丸めた整数 `scaled`（`s` 位置の値）が `|scaled| < 10^p`（PostgreSQL の
    // `numeric(p, s)` と同じ。`s` が負でも `p` は `scaled` の桁数の上限）。
    if let Some(limit) = u32::try_from(p).ok().and_then(pow10) {
        if scaled.unsigned_abs() >= limit.unsigned_abs() {
            return Err(NumFail::Overflow);
        }
    }
    // 10^p が i128 に収まらないほど大きい p なら、i128 の値は必ず収まる。
    if s >= 0 {
        make(scaled, s)
    } else if scaled == 0 {
        make(0, 0)
    } else {
        // 負の scale: 10^(-s) の倍数へ丸めた整数を scale 0 で保持する。
        let f =
            pow10(u32::try_from(-s).map_err(|_| NumFail::Overflow)?).ok_or(NumFail::Overflow)?;
        make(scaled.checked_mul(f).ok_or(NumFail::Overflow)?, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> Decimal {
        parse_literal_exact(text).expect("valid literal")
    }

    fn show(r: Result<Decimal, NumFail>) -> String {
        match r {
            Ok(v) => v.to_string(),
            Err(e) => format!("{e:?}"),
        }
    }

    #[test]
    fn division_scale_follows_select_div_scale() {
        // 期待値は PostgreSQL 17 の `select a::numeric / b::numeric` を採取したもの。
        assert_eq!(show(div(&d("1"), &d("1"))), "1.00000000000000000000");
        assert_eq!(show(div(&d("3"), &d("3"))), "1.00000000000000000000");
        assert_eq!(show(div(&d("10"), &d("3"))), "3.3333333333333333");
        assert_eq!(show(div(&d("1"), &d("3"))), "0.33333333333333333333");
        assert_eq!(show(div(&d("2"), &d("3"))), "0.66666666666666666667");
        assert_eq!(show(div(&d("100000"), &d("3"))), "33333.333333333333");
        assert_eq!(show(div(&d("0"), &d("7"))), "0.00000000000000000000");
        assert_eq!(
            show(div(&d("1"), &d("10000"))),
            "0.000100000000000000000000"
        );
        assert_eq!(show(div(&d("0.5"), &d("3"))), "0.16666666666666666667");
        assert_eq!(show(div(&d("12345678"), &d("7"))), "1763668.285714285714");
        assert_eq!(show(div(&d("1.5"), &d("0.25"))), "6.0000000000000000");
        assert_eq!(show(div(&d("-1"), &d("3"))), "-0.33333333333333333333");
    }

    #[test]
    fn division_by_zero_and_overflow_are_distinguished() {
        assert_eq!(div(&d("1"), &d("0")), Err(NumFail::DivisionByZero));
        assert_eq!(rem(&d("1"), &d("0.0")), Err(NumFail::DivisionByZero));
        // 結果の scale（PostgreSQL の規則では 56）が 38 を超える。
        assert_eq!(
            div(&d("0.0000000000000000000000000000000000001"), &d("3")),
            Err(NumFail::Overflow)
        );
        // 整数部だけで 38 桁ある被除数は scale 0 の結果になり、収まる。
        assert_eq!(
            show(div(&d("99999999999999999999999999999999999999"), &d("3"))),
            "33333333333333333333333333333333333333"
        );
    }

    #[test]
    fn modulo_takes_dividend_sign_and_larger_scale() {
        assert_eq!(show(rem(&d("7"), &d("3"))), "1");
        assert_eq!(show(rem(&d("-7"), &d("3"))), "-1");
        assert_eq!(show(rem(&d("7.5"), &d("2"))), "1.5");
        assert_eq!(show(rem(&d("5.5"), &d("-2"))), "1.5");
    }

    #[test]
    fn arithmetic_boundaries() {
        let max38 = d("99999999999999999999999999999999999999");
        assert_eq!(
            show(add(&max38, &d("1"))),
            "100000000000000000000000000000000000000"
        );
        assert_eq!(show(mul(&d("1.5"), &d("2"))), "3.0");
        // i128 を超える乗算は Overflow。
        assert_eq!(mul(&max38, &max38), Err(NumFail::Overflow));
        assert_eq!(show(sub(&d("0.3"), &d("0.1"))), "0.2");
    }

    #[test]
    fn floor_ceil_round() {
        assert_eq!(show(floor(&d("-1.5"))), "-2");
        assert_eq!(show(ceil(&d("-1.5"))), "-1");
        assert_eq!(show(ceil(&d("1.2"))), "2");
        assert_eq!(round_to_i128(&d("1.5")), Some(2));
        assert_eq!(round_to_i128(&d("-1.5")), Some(-2));
        assert_eq!(round_to_i128(&d("1.4")), Some(1));
        assert_eq!(trunc_to_i128(&d("-1.9")), Some(-1));
    }

    #[test]
    fn precision_and_scale() {
        assert_eq!(show(apply_precision_scale(&d("123.456"), 10, 2)), "123.46");
        assert_eq!(
            apply_precision_scale(&d("123.456"), 4, 2),
            Err(NumFail::Overflow)
        );
        assert_eq!(show(apply_precision_scale(&d("123.456"), 5, 2)), "123.46");
        assert_eq!(show(apply_precision_scale(&d("123.456"), 6, 0)), "123");
        assert_eq!(show(apply_precision_scale(&d("5"), 5, 2)), "5.00");
        assert_eq!(show(apply_precision_scale(&d("123.456"), 3, -1)), "120");
    }
}
