//! 数値スカラー関数群（ABS/ROUND/FLOOR/CEIL/CEILING/MOD/POWER/SQRT）の純粋関数実装
//! （Issue #920、対象ビヘイビア: SQL-26。ポインタ: `docs/spec/05-tasks.md`
//! TASK-210・`docs/spec/04-behavior/sql-surface.md` SQL-26）。
//!
//! 責務境界: 本モジュールは行コンテキスト（`id`・`embedding`）を持たない値レベルの
//! 計算のみを提供する。`sql::udf_call::BuiltinFn`（`Abs`/`Round1`/`Round2`/`Floor`/
//! `Ceil`/`Mod`/`Power`/`Sqrt`）の解決・型検査・行ループでの呼び出しは
//! `sql::udf_call::bind_call`・`apply_builtin` が担い、本モジュールへ委譲する
//! （関数本体を `udf_call.rs` へ複製しない）。
//!
//! fail-closed の方針（`.claude/rules/security.md`「不安全な設計」）: 0 除算・
//! 未定義（`0^負数`・負数の非整数乗）・非有限値の生成は黙って丸めず `Err` で
//! 拒否する。オーバーフロー・アンダーフローは `SqlSurfaceError::numeric_out_of_range`
//! （`22003`）、それ以外の不正入力（非整数の丸め桁数・0 除算・sqrt の負数等）は
//! `SqlSurfaceError::invalid_input`（`22000`）に写像する。

use crate::sql::allowlist::SqlSurfaceError;

/// 有限入力なら理論上は常に有限であるはずの結果を検査する共通ヘルパー
/// （`abs`/`round`/`floor`/`ceil`/`mod` 用。何らかの理由で非有限値が生じた場合の
/// 保険として `22003` へ fail-closed に写像する）。
fn finite_result(v: f64, fn_name: &str) -> Result<f64, SqlSurfaceError> {
    if !v.is_finite() {
        return Err(SqlSurfaceError::numeric_out_of_range(format!(
            "{fn_name}: result is not finite"
        )));
    }
    Ok(v)
}

/// `abs(x: Scalar) -> Scalar`。
pub(crate) fn abs(x: f64) -> Result<f64, SqlSurfaceError> {
    finite_result(x.abs(), "abs")
}

/// `round(x: Scalar) -> Scalar`（0.5 は 0 から遠い側へ丸める。`f64::round` と同じ
/// 意味論）。
pub(crate) fn round1(x: f64) -> Result<f64, SqlSurfaceError> {
    finite_result(x.round(), "round")
}

/// `round(x: Scalar, n: Scalar) -> Scalar`（小数点以下 `n` 桁への丸め。`n` は整数値
/// でなければならず、`i32` の表現域を超える場合は `22003`）。
///
/// `n` が大きく `x * 10^n` がオーバーフローする場合は丸めの効果が生じない
/// （`x` をそのまま返す）。`n` が大きく負で `10^n` がアンダーフローして 0 になる
/// 場合は丸め先が存在しないとみなし 0 を返す。いずれも黙った精度欠落ではなく、
/// 数学的に丸め操作が定義できない領域への意図的な縮退である。
/// 一方、`10^n` 自体は非零（サブノーマル等）で `scaled` も有限だが、桁を
/// 戻す除算（`rounded / 10^n`）が真にオーバーフローするケースは上記の
/// 意図的な縮退とは区別し、`22003`（`numeric_out_of_range`）として拒否する。
pub(crate) fn round2(x: f64, n: f64) -> Result<f64, SqlSurfaceError> {
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(SqlSurfaceError::invalid_input(
            "round: second argument must be an integer",
        ));
    }
    if n < (i32::MIN as f64) || n > (i32::MAX as f64) {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "round: second argument is out of range",
        ));
    }
    // `n as i32`: 直前の範囲検査（`i32::MIN..=i32::MAX`）済みのため安全な変換。
    let n_i32 = n as i32;
    let pow10 = 10f64.powi(n_i32);
    let scaled = x * pow10;
    if !scaled.is_finite() {
        return Ok(x);
    }
    let rounded = scaled.round();
    let result = rounded / pow10;
    if !result.is_finite() {
        // `pow10 == 0.0`（`10^n` がアンダーフローで真に 0 になった場合）は
        // `rounded / pow10` が `0.0/0.0`（NaN）等になるだけで、丸め先の桁が
        // 存在しないという意図どおりの縮退なので 0 を返す。
        // `pow10 != 0.0`（非零のサブノーマル等）で非有限になった場合は、
        // `scaled` 自体は有限でも桁を戻す除算で真にオーバーフローしている
        // ため、黙って 0 を返さず `22003` として拒否する（fail-closed）。
        if pow10 == 0.0 {
            return Ok(0.0);
        }
        return Err(SqlSurfaceError::numeric_out_of_range(
            "round: result is out of range",
        ));
    }
    Ok(result)
}

/// `floor(x: Scalar) -> Scalar`。
pub(crate) fn floor(x: f64) -> Result<f64, SqlSurfaceError> {
    finite_result(x.floor(), "floor")
}

/// `ceil(x: Scalar) -> Scalar`（`ceiling` の別名としても解決される。
/// `sql::udf_call::builtin_from_name` 参照）。
pub(crate) fn ceil(x: f64) -> Result<f64, SqlSurfaceError> {
    finite_result(x.ceil(), "ceil")
}

/// `mod(x: Scalar, y: Scalar) -> Scalar`（剰余。符号は被除数 `x` に従う。`f64` の
/// `%` 演算子と同じ意味論）。除数 0 は `22000`（既存の `/` 演算子の前例に揃える。
/// PostgreSQL の `MOD(x, 0)` 相当の `22012`〔division_by_zero〕への分離は後続
/// 課題とする）。
pub(crate) fn modulo(x: f64, y: f64) -> Result<f64, SqlSurfaceError> {
    if y == 0.0 {
        return Err(SqlSurfaceError::invalid_input("mod: division by zero"));
    }
    finite_result(x % y, "mod")
}

/// `power(x: Scalar, y: Scalar) -> Scalar`。`0` の負数乗・負の底の非整数乗
/// （結果が定義されない）は `22000`。結果が `±∞`（オーバーフロー）・底が非零
/// なのに結果が `0`（アンダーフロー）は `22003`。
pub(crate) fn power(x: f64, y: f64) -> Result<f64, SqlSurfaceError> {
    if x == 0.0 && y < 0.0 {
        return Err(SqlSurfaceError::invalid_input(
            "power: zero raised to a negative power is undefined",
        ));
    }
    let r = x.powf(y);
    if r.is_nan() {
        return Err(SqlSurfaceError::invalid_input("power: result is undefined"));
    }
    if r.is_infinite() {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "power: result overflows",
        ));
    }
    if r == 0.0 && x != 0.0 {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "power: result underflows",
        ));
    }
    Ok(r)
}

/// `sqrt(x: Scalar) -> Scalar`。負の入力は結果が実数域に存在しないため `22000`。
pub(crate) fn sqrt(x: f64) -> Result<f64, SqlSurfaceError> {
    if x < 0.0 {
        return Err(SqlSurfaceError::invalid_input(
            "sqrt: input must be non-negative",
        ));
    }
    finite_result(x.sqrt(), "sqrt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_negates_negative_values() {
        assert_eq!(abs(-3.5).unwrap(), 3.5);
        assert_eq!(abs(3.5).unwrap(), 3.5);
        assert_eq!(abs(0.0).unwrap(), 0.0);
    }

    #[test]
    fn round1_rounds_half_away_from_zero() {
        assert_eq!(round1(2.5).unwrap(), 3.0);
        assert_eq!(round1(-2.5).unwrap(), -3.0);
        assert_eq!(round1(2.4).unwrap(), 2.0);
    }

    #[test]
    fn round2_rounds_to_requested_decimal_places() {
        assert!((round2(1.234_58, 2.0).unwrap() - 1.23).abs() < 1e-9);
        assert!((round2(1234.5, -2.0).unwrap() - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn round2_rejects_non_integer_second_argument() {
        let err = round2(1.0, 0.5).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn round2_rejects_n_beyond_i32_range_with_numeric_out_of_range() {
        let err = round2(1.0, 3_000_000_000.0).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn round2_returns_input_unchanged_when_scale_overflows() {
        // n は i32 範囲内だが 10^n がオーバーフローする（非有限）ため、丸めの
        // 効果が生じないとみなし x をそのまま返す。
        let x = 12345.6789;
        let got = round2(x, 400.0).unwrap();
        assert_eq!(got, x);
    }

    #[test]
    fn round2_returns_zero_when_scale_underflows() {
        // n が大きく負で 10^n がアンダーフローして 0 になるため、丸め先が
        // 存在しないとみなし 0 を返す。
        let got = round2(12345.6789, -400.0).unwrap();
        assert_eq!(got, 0.0);
    }

    #[test]
    fn round2_rejects_overflow_on_scale_back_with_numeric_out_of_range() {
        // 10^n（n=-308）はサブノーマルだが非零で `scaled` も有限だが、
        // 桁を戻す除算 `rounded / 10^n` が f64::MAX を超えて真にオーバー
        // フローする。アンダーフロー由来の 0 と誤判定せず 22003 を返す。
        let err = round2(1.7e308, -308.0).unwrap_err();
        assert_eq!(err.wire_code(), "22003");

        let err = round2(f64::MAX, -307.0).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn floor_and_ceil_match_std() {
        assert_eq!(floor(2.7).unwrap(), 2.0);
        assert_eq!(floor(-2.1).unwrap(), -3.0);
        assert_eq!(ceil(2.1).unwrap(), 3.0);
        assert_eq!(ceil(-2.7).unwrap(), -2.0);
    }

    #[test]
    fn modulo_follows_dividend_sign_like_f64_remainder() {
        assert_eq!(modulo(5.0, 3.0).unwrap(), 2.0);
        assert_eq!(modulo(-5.0, 3.0).unwrap(), -2.0);
    }

    #[test]
    fn modulo_by_zero_is_rejected_fail_closed() {
        let err = modulo(1.0, 0.0).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn power_computes_normal_cases() {
        assert_eq!(power(2.0, 10.0).unwrap(), 1024.0);
        assert!((power(9.0, 0.5).unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn power_overflow_is_numeric_out_of_range() {
        let err = power(10.0, 400.0).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn power_zero_to_negative_power_is_invalid_input() {
        let err = power(0.0, -1.0).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn power_negative_base_non_integer_exponent_is_invalid_input() {
        let err = power(-8.0, 0.5).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn sqrt_of_negative_is_invalid_input() {
        let err = sqrt(-1.0).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn sqrt_of_nonnegative_matches_std() {
        assert_eq!(sqrt(9.0).unwrap(), 3.0);
        assert_eq!(sqrt(0.0).unwrap(), 0.0);
    }
}
