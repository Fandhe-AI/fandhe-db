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
//! （`22003`）、0 除算は `SqlSurfaceError::division_by_zero`（`22012`、Issue #1163）、
//! それ以外の不正入力（非整数の丸め桁数・sqrt の負数等）は
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

/// 大きい順（MSB が先頭）の 10 進数字列（各要素は `0..=9`）を「+1」だけ増分する
/// （`round2` の桁上げ処理の共有ヘルパー）。`f64` の乗除算を経由せず、`round2` が
/// 組み立てた 10 進数字列を直接インクリメントすることで、2 回目の浮動小数点誤差を
/// 混入させない。最上位桁まで繰り上がった場合（例: `"999"` → `"1000"`）は
/// 先頭に `1` を追加する（`digits` が空スライス、つまり丸め位置より上に
/// 保持する桁が無い場合も同じ経路で `[1]` になる）。
fn increment_decimal_digits(digits: &mut Vec<u8>) {
    for d in digits.iter_mut().rev() {
        if *d == 9 {
            *d = 0;
        } else {
            *d += 1;
            return;
        }
    }
    digits.insert(0, 1);
}

/// `round(x: Scalar, n: Scalar) -> Scalar`（小数点以下 `n` 桁への丸め。`n` は整数値
/// でなければならず、`i32` の表現域を超える場合は `22003`）。0.5 は 0 から遠い側へ
/// 丸める（[`round1`] と同じ half-away-from-zero。`docs/design/
/// numeric-scalar-functions.md` 参照）。
///
/// `x * 10^n` を `f64` で計算して丸めると、10 進小数として厳密に `*.5` である
/// 中間値（例: `1.005`）が二進浮動小数点の丸め誤差で `*.5` からわずかにずれ、
/// 丸め方向を誤ることがある（PR #1107 codex-review P1 指摘）。これを避けるため、
/// `x` の**最短往復表現**（Rust の `Display`／`{}` フォーマットは、その `f64` の値へ
/// 一意に戻る最短の 10 進数字列を返す。`ryu`/Grisu 系アルゴリズム相当）を 10 進数字列
/// として取得し、桁の切り捨て・繰り上げを 10 進数字列のまま行う。浮動小数点の
/// 乗除算を一切経由しないため、`10^n` のオーバーフロー・アンダーフローという
/// 中間表現由来の誤差そのものが発生しない。
///
/// `n` が保持対象の桁数（`x` の小数部・整数部の桁数）を超える場合は丸めの余地が
/// ないため `x` をそのまま返す。丸め単位（`10^-n`）が `x` の絶対値の桁数を超えて
/// 大きい場合は、最上位桁で丸めるかどうかだけを判定し、丸め不要なら `0` を返す
/// （黙った精度欠落ではなく、その桁での丸め操作が数学的に意味を持たない領域への
/// 意図的な縮退）。丸め後の 10 進数字列を `f64` へ変換する際に有効範囲を超える
/// （`Infinity` になる）場合のみ `22003`（`numeric_out_of_range`）を返す。
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
    // 非有限値（`NaN`/`Infinity`）は 10 進展開できないため、丸めの効果が
    // 生じないとみなしそのまま返す（旧実装で `x * 10^n` が非有限になり
    // `Ok(x)` を返していたのと同じ観測結果を維持する）。
    if !x.is_finite() {
        return Ok(x);
    }
    if x == 0.0 {
        return Ok(x);
    }
    let negative = x.is_sign_negative();
    // `{}` フォーマットは f64 の最短往復表現を科学的記数法なしで返すため、
    // 常に `<整数部>` または `<整数部>.<小数部>` の形になる（`x.abs()` は非有限・
    // ゼロを上で除外済みのため常に正の有限値）。
    let formatted = format!("{}", x.abs());
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i, f),
        None => (formatted.as_str(), ""),
    };

    // `combined`: 丸め後に得たい 10 進数字列と、そのうち整数部として解釈する
    // 桁数（`point`。`combined[..point]` が整数部、`combined[point..]` が
    // 小数部）。負の `n`（整数部側での丸め）と非負の `n`（小数部側での丸め）を
    // それぞれ組み立ててから、共通の「10 進数字列 → f64」変換へ合流させる。
    let (combined, point): (Vec<u8>, usize) = if n_i32 >= 0 {
        let n_usize = n_i32 as usize;
        if n_usize >= frac_part.len() {
            // 保持したい小数桁数が実際の小数部の桁数以上 = 丸める余地がない。
            return Ok(x);
        }
        let decision_digit = frac_part.as_bytes()[n_usize] - b'0';
        let mut digits: Vec<u8> = int_part
            .bytes()
            .chain(frac_part.bytes().take(n_usize))
            .map(|b| b - b'0')
            .collect();
        if decision_digit >= 5 {
            increment_decimal_digits(&mut digits);
        }
        // 桁上げで `digits` が 1 桁増えていれば整数部もその分伸びる
        // （`digits.len() - n_usize` は増分後の長さから逆算するため常に
        // 正しい整数部長になる）。
        let point = digits.len() - n_usize;
        (digits, point)
    } else {
        // `n_i32.unsigned_abs()`: `i32::MIN` の単純な符号反転はオーバーフロー
        // する（`-i32::MIN` は `i32` で表現できない）ため `unsigned_abs` で
        // 安全に絶対値を取る。
        let pos = n_i32.unsigned_abs() as usize;
        if pos > int_part.len() {
            // 丸め単位が整数部の桁数より大きい = 最上位桁より上を四捨五入する
            // 余地すらない。この場合は必ず 0 になる（意図的な縮退）。
            return Ok(if negative { -0.0 } else { 0.0 });
        }
        let keep_len = int_part.len() - pos;
        let decision_digit = int_part.as_bytes()[keep_len] - b'0';
        let mut digits: Vec<u8> = int_part.bytes().take(keep_len).map(|b| b - b'0').collect();
        if decision_digit >= 5 {
            increment_decimal_digits(&mut digits);
        }
        digits.extend(std::iter::repeat_n(0u8, pos));
        let point = digits.len();
        (digits, point)
    };

    let mut magnitude = String::with_capacity(combined.len() + 2);
    for &d in &combined[..point] {
        magnitude.push((b'0' + d) as char);
    }
    if point < combined.len() {
        magnitude.push('.');
        for &d in &combined[point..] {
            magnitude.push((b'0' + d) as char);
        }
    }
    let signed = if negative {
        format!("-{magnitude}")
    } else {
        magnitude
    };
    // 桁上げ後の数字列を組み立て直しているだけなので `parse` 自体が失敗する
    // ことはない（10 進数字列として常に整形済み）。唯一非有限になり得るのは
    // 丸め後の絶対値が `f64::MAX` を超え `Infinity` へ丸め込まれる場合で、
    // これは真のオーバーフローとして `22003` へ写像する。
    let Ok(result) = signed.parse::<f64>() else {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "round: result is out of range",
        ));
    };
    finite_result(result, "round")
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
/// `%` 演算子と同じ意味論）。除数 0 は `22012`（division_by_zero。
/// 既存の `/` 演算子と同じ分類。Issue #1163）。
pub(crate) fn modulo(x: f64, y: f64) -> Result<f64, SqlSurfaceError> {
    if y == 0.0 {
        return Err(SqlSurfaceError::division_by_zero("mod"));
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
    fn round2_returns_input_unchanged_when_no_digits_to_drop() {
        // n が実際の小数桁数以上のため、丸める余地がなく x をそのまま返す
        // （PR #1107 codex-review P1 是正後: `10^n` のオーバーフローではなく、
        // `x` の 10 進展開（`frac_part`）の桁数と `n` の比較で判定する）。
        let x = 12345.6789;
        let got = round2(x, 400.0).unwrap();
        assert_eq!(got, x);
    }

    #[test]
    fn round2_returns_zero_when_rounding_unit_exceeds_magnitude() {
        // 丸め単位（`10^400`）が `x` の整数部の桁数を大きく超えるため、
        // 最上位桁でも丸め上げが起こり得ず 0 を返す（意図的な縮退）。
        let got = round2(12345.6789, -400.0).unwrap();
        assert_eq!(got, 0.0);
    }

    #[test]
    fn round2_rejects_overflow_on_round_up_with_numeric_out_of_range() {
        // 丸め上げの結果が `f64::MAX` を超えて `Infinity` になる真のオーバー
        // フローは `22003` を返す（10 進数字列の組み立てには乗除算を使わない
        // ため、旧実装のようなアンダーフロー由来の中間表現とは無関係に、
        // 最終結果の非有限性のみで判定できる）。
        let err = round2(1.7e308, -308.0).unwrap_err();
        assert_eq!(err.wire_code(), "22003");

        let err = round2(f64::MAX, -307.0).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn round2_rounds_decimal_midpoint_away_from_zero_not_binary_midpoint() {
        // PR #1107 codex-review P1 回帰テスト: `x * 10^n` を `f64` で計算すると
        // 二進浮動小数点の丸め誤差により `1.005 * 100` が `100.5` よりわずかに
        // 小さくなり、期待される `1.01` ではなく `1.00` を返していた
        // （10 進表現ではちょうど中間値である `*.5` を正しく検出できなかった）。
        // 10 進数字列ベースの実装では `x` の最短往復表現の桁を直接見るため、
        // この種の二進化に由来する誤判定が起こらないことを固定する。
        assert_eq!(round2(1.005, 2.0).unwrap(), 1.01);
        assert_eq!(round2(2.675, 2.0).unwrap(), 2.68);
        assert_eq!(round2(-1.005, 2.0).unwrap(), -1.01);
    }

    #[test]
    fn round2_with_n_zero_rounds_to_nearest_integer_away_from_zero() {
        assert_eq!(round2(2.5, 0.0).unwrap(), 3.0);
        assert_eq!(round2(-2.5, 0.0).unwrap(), -3.0);
        assert_eq!(round2(2.4, 0.0).unwrap(), 2.0);
    }

    #[test]
    fn round2_with_negative_n_rounds_within_integer_digits() {
        assert_eq!(round2(150.0, -2.0).unwrap(), 200.0);
        assert_eq!(round2(149.0, -2.0).unwrap(), 100.0);
        assert_eq!(round2(-150.0, -2.0).unwrap(), -200.0);
    }

    #[test]
    fn round2_carries_into_a_new_leading_digit_on_round_up() {
        // 丸め上げが最上位の保持桁を超えて繰り上がる境界（`99.5` → `100`・
        // `9.95` → `10.0`）。
        assert_eq!(round2(99.5, 0.0).unwrap(), 100.0);
        assert_eq!(round2(9.95, 1.0).unwrap(), 10.0);
    }

    #[test]
    fn round2_returns_input_unchanged_for_non_finite_x() {
        // 非有限な `x`（`NaN`/`Infinity`）は 10 進展開できないため丸めの効果が
        // 生じないとみなしそのまま返す（`apply_builtin`／`finite_result` を
        // 経由する他の組み込み関数と異なり、`round2` は `x` 自体の非有限性を
        // 事前に弾かない設計を維持する）。
        assert!(round2(f64::NAN, 2.0).unwrap().is_nan());
        assert_eq!(round2(f64::INFINITY, 2.0).unwrap(), f64::INFINITY);
        assert_eq!(round2(f64::NEG_INFINITY, -2.0).unwrap(), f64::NEG_INFINITY);
    }

    #[test]
    fn round2_handles_extreme_magnitudes_without_scientific_notation() {
        // Bugbot 指摘の確認テスト（false positive の記録）: Rust の `f64` の
        // `Display`（`{}` フォーマット）は他言語（Python/JavaScript の
        // `str`/`repr`）と異なり、絶対値の大小によらず科学的記数法
        // （`1e-10` 等）へ切り替わらない（`std::fmt::Display for f64` の
        // 実装は常に固定小数点表記。`{:e}` を明示指定した場合のみ指数表記）。
        // `round2` が `format!("{}", x.abs())` の結果に `'e'`/`'E'` を含む
        // ケースを一切考慮していない設計は、この Rust の保証に依拠している。
        // 極小・極大の絶対値でも整数部・小数部の分割と丸めが破綻しないことを
        // 固定する。
        assert_eq!(round2(0.00001234, 8.0).unwrap(), 0.00001234);
        assert_eq!(round2(0.000012349, 8.0).unwrap(), 0.00001235);
        assert!((round2(1.23e-10, 11.0).unwrap() - 1.2e-10).abs() < 1e-25);
        assert_eq!(round2(1.0e20, 0.0).unwrap(), 1.0e20);
        assert_eq!(round2(1.0e20, -25.0).unwrap(), 0.0);
        assert!(!format!("{}", 1.23e-10_f64).contains(['e', 'E']));
        assert!(!format!("{}", 1.0e20_f64).contains(['e', 'E']));
    }

    // NUMERIC 型の値は式（`SELECT`/`WHERE` 中の関数呼び出し引数）としては
    // 束縛段で拒否され本関数へ到達しない（`sql/udf_call.rs` の
    // `ColumnType::Numeric` 分岐、TABLE-13〔検討中〕・TASK-197、Issue #885・
    // #891）ため、NUMERIC 型専用の丸め経路は本関数に存在しない。

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
        assert_eq!(err.wire_code(), "22012");
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
