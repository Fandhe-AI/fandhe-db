//! 分散系集計 `variance`／`var_pop`／`var_samp` の累積状態と確定処理
//! （SQL-57・Issue #1803。ポインタ表記のみ。spec 本文は転記しない）。
//!
//! 役割: `sql::aggregate::Accumulator` の分散 variant が保持する状態を提供する。
//! 入力の型で 2 系統に分かれる。
//!
//! - [`VarianceExact`]: `smallint`／`integer`／`bigint`／疑似列 `id`／`numeric(p,s)` 列。
//!   件数・和・二乗和を正確に保持し、確定時に PostgreSQL 17 の `numeric_stddev_internal`
//!   と同じ手順（分子 `N·Σx² − (Σx)²`、分母 `N·N` または `N·(N−1)`、除算 scale は
//!   `select_div_scale`）で numeric を返す。中間値は `bigint` の二乗和でも `i128` を超えるため
//!   [`crate::numeric::U256`] で持つ。
//! - [`VarianceFloat`]: `real`／`double precision` 列と数値スカラー式。PostgreSQL の
//!   `float8_accum` と同じ Youngs–Cramer 更新で数値的に安定に累積し、double を返す。
//!
//! 後続の `stddev` 系（Issue #1804）は [`VarianceExact::finish_decimal`]／
//! [`VarianceFloat::finish_f64`] が返す分散の確定値に平方根をかぶせるだけで済むよう、
//! 状態の更新と確定を分けて公開している。
//!
//! 契約: 行数 0 は `None`（SQL の NULL）。行数 1 は標本分散が `None`、母分散が 0。
//! 範囲超過（累積のあふれ・numeric の 38 桁超過・結果 scale が 38 超）は `22003`
//! （fail-closed。PostgreSQL の任意精度との差分は `docs/design/variance.md` に記録する）。
//! 内部不変条件の違反は `XX000`。全演算は checked で、`unsafe` は使わない。

use crate::numeric::{
    div_round_wide, select_div_scale_wide, Decimal, NumericError, MAX_PRECISION, U256,
};
use crate::sql::aggregate::accumulator_bug;
use crate::sql::allowlist::{AggregateFunc, SqlSurfaceError};

/// 母分散か標本分散か。`AggregateFunc::Variance` は標本分散（`var_samp` と同義）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarianceKind {
    Pop,
    Samp,
}

impl VarianceKind {
    /// 分散系の関数から種別を決める。分散系以外は `None`。
    pub(crate) fn from_func(func: AggregateFunc) -> Option<Self> {
        match func {
            AggregateFunc::VarPop => Some(VarianceKind::Pop),
            AggregateFunc::Variance | AggregateFunc::VarSamp => Some(VarianceKind::Samp),
            _ => None,
        }
    }
}

fn overflow(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::numeric_out_of_range(detail)
}

/// 正確演算の累積状態（整数系・`id`・`numeric(p,s)`）。`Accumulator` では `Box` 越しに持つ
/// （GROUP BY のグループごとに生成されるため enum 自体を大きくしない）。
#[derive(Debug, Clone)]
pub(crate) struct VarianceExact {
    count: u64,
    sum_x: i128,
    sum_x2: U256,
    /// 入力の scale（整数系は 0）。和の dscale が `scale`、二乗和・分子の dscale が `2 * scale`。
    scale: u8,
}

impl VarianceExact {
    pub(crate) fn new(scale: u8) -> Self {
        VarianceExact {
            count: 0,
            sum_x: 0,
            sum_x2: U256::ZERO,
            scale,
        }
    }

    /// 非 NULL 値 1 件（`scale` 桁の unscaled 値）を取り込む。
    pub(crate) fn observe(&mut self, unscaled: i128) -> Result<(), SqlSurfaceError> {
        let count = self
            .count
            .checked_add(1)
            .ok_or_else(|| overflow("variance row count exceeds u64 range"))?;
        let sum_x = self
            .sum_x
            .checked_add(unscaled)
            .ok_or_else(|| overflow("variance intermediate accumulation overflowed"))?;
        let square = U256::mul_wide(unscaled.unsigned_abs(), unscaled.unsigned_abs());
        let sum_x2 = self
            .sum_x2
            .checked_add(square)
            .ok_or_else(|| overflow("variance intermediate accumulation overflowed"))?;
        self.count = count;
        self.sum_x = sum_x;
        self.sum_x2 = sum_x2;
        Ok(())
    }

    /// 分散の確定値。行数 0（および標本分散で行数 1）は `None`。
    /// 分子が 0 以下（全値が等しい）なら scale 0 の `0`（PostgreSQL の `const_zero` と同じ）。
    pub(crate) fn finish_decimal(
        &self,
        kind: VarianceKind,
    ) -> Result<Option<Decimal>, SqlSurfaceError> {
        let n = self.count;
        if n == 0 || (kind == VarianceKind::Samp && n == 1) {
            return Ok(None);
        }
        let n_wide = u128::from(n);
        let n_sum_x2 = self
            .sum_x2
            .checked_mul_u128(n_wide)
            .ok_or_else(|| overflow("variance numerator exceeds the supported range"))?;
        let sum_x_sq = U256::mul_wide(self.sum_x.unsigned_abs(), self.sum_x.unsigned_abs());
        // 数学的には常に非負。0 以下は 0 を返す（PostgreSQL の丸め誤差ガードと同じ分岐）。
        let numerator = match n_sum_x2.checked_sub(sum_x_sq) {
            Some(v) if !v.is_zero() => v,
            _ => {
                return zero_decimal().map(Some);
            }
        };
        // `n <= u64::MAX` なので `n * n`・`n * (n - 1)` は u128 に収まる。
        let denominator = match kind {
            VarianceKind::Pop => n_wide * n_wide,
            VarianceKind::Samp => n_wide * (n_wide - 1),
        };
        let numerator_scale = u32::from(self.scale) * 2;
        let result_scale = select_div_scale_wide(numerator, numerator_scale, denominator, 0)
            .map_err(map_numeric_error)?;
        let unscaled = div_round_wide(numerator, numerator_scale, denominator, result_scale)
            .map_err(map_numeric_error)?;
        let result_scale = u8::try_from(result_scale)
            .map_err(|_| overflow("variance result scale exceeds the supported range"))?;
        let decimal = Decimal::from_parts(unscaled, result_scale)
            .map_err(|_| overflow("variance result scale exceeds the supported range"))?;
        if !decimal.fits_precision(MAX_PRECISION) {
            return Err(overflow("variance exceeds the maximum supported precision"));
        }
        Ok(Some(decimal))
    }
}

fn zero_decimal() -> Result<Decimal, SqlSurfaceError> {
    Decimal::from_parts(0, 0).map_err(|_| accumulator_bug("variance zero has an invalid scale"))
}

/// 範囲超過は `22003`、それ以外（不変条件違反）は `XX000` へ写像する。
fn map_numeric_error(e: NumericError) -> SqlSurfaceError {
    match e {
        NumericError::OutOfRange => overflow("variance exceeds the supported numeric range"),
        NumericError::Malformed(_) => accumulator_bug("variance division operands are invalid"),
    }
}

/// 浮動小数の累積状態（`real`／`double precision` 列・数値スカラー式）。
/// PostgreSQL の `float8_accum` と同じ更新式（Youngs–Cramer）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VarianceFloat {
    count: u64,
    sum_x: f64,
    sum_xx: f64,
}

impl VarianceFloat {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 非 NULL 値 1 件を取り込む。更新後に非有限になったら `22003`
    /// （保存値は有限値のみという契約を保つ。`FloatSum` 等と同じ）。
    pub(crate) fn observe(&mut self, v: f64) -> Result<(), SqlSurfaceError> {
        let old_n = self.count as f64;
        let count = self
            .count
            .checked_add(1)
            .ok_or_else(|| overflow("variance row count exceeds u64 range"))?;
        let n = count as f64;
        let sum_x = self.sum_x + v;
        let mut sum_xx = self.sum_xx;
        if old_n > 0.0 {
            let tmp = v * n - sum_x;
            sum_xx += tmp * tmp / (n * old_n);
        }
        if !sum_x.is_finite() || !sum_xx.is_finite() {
            return Err(overflow("variance overflowed to a non-finite value"));
        }
        self.count = count;
        self.sum_x = sum_x;
        self.sum_xx = sum_xx;
        Ok(())
    }

    /// 分散の確定値。行数 0（および標本分散で行数 1）は `None`。
    pub(crate) fn finish_f64(&self, kind: VarianceKind) -> Result<Option<f64>, SqlSurfaceError> {
        let n = self.count;
        if n == 0 || (kind == VarianceKind::Samp && n == 1) {
            return Ok(None);
        }
        // 丸め誤差で負になることはないが、防御として 0 へクランプする。
        let sxx = self.sum_xx.max(0.0);
        let divisor = match kind {
            VarianceKind::Pop => n as f64,
            VarianceKind::Samp => (n - 1) as f64,
        };
        let result = sxx / divisor;
        if !result.is_finite() {
            return Err(overflow("variance overflowed to a non-finite value"));
        }
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(scale: u8, values: &[i128]) -> VarianceExact {
        let mut s = VarianceExact::new(scale);
        for v in values {
            s.observe(*v).expect("observe");
        }
        s
    }

    fn text(d: Option<Decimal>) -> Option<String> {
        d.map(|d| d.to_string())
    }

    #[test]
    fn exact_endpoints() {
        let empty = exact(0, &[]);
        assert_eq!(empty.finish_decimal(VarianceKind::Pop).expect("ok"), None);
        assert_eq!(empty.finish_decimal(VarianceKind::Samp).expect("ok"), None);
        let one = exact(2, &[1234]);
        assert_eq!(one.finish_decimal(VarianceKind::Samp).expect("ok"), None);
        // 1 行の母分散は scale 0 の 0（`0.00` ではない）。
        let pop = one.finish_decimal(VarianceKind::Pop).expect("ok");
        assert_eq!(text(pop), Some("0".to_string()));
    }

    #[test]
    fn exact_integer_values_match_pg17() {
        let s = exact(0, &[1, 2, 3]);
        assert_eq!(
            text(s.finish_decimal(VarianceKind::Pop).expect("ok")),
            Some("0.66666666666666666667".to_string())
        );
        assert_eq!(
            text(s.finish_decimal(VarianceKind::Samp).expect("ok")),
            Some("1.00000000000000000000".to_string())
        );
        let s = exact(0, &[1, 2, 3, 4]);
        assert_eq!(
            text(s.finish_decimal(VarianceKind::Samp).expect("ok")),
            Some("1.6666666666666667".to_string())
        );
        assert_eq!(
            text(s.finish_decimal(VarianceKind::Pop).expect("ok")),
            Some("1.2500000000000000".to_string())
        );
    }

    #[test]
    fn exact_equal_values_give_scale_zero_zero() {
        let s = exact(2, &[500, 500, 500]);
        assert_eq!(
            text(s.finish_decimal(VarianceKind::Samp).expect("ok")),
            Some("0".to_string())
        );
    }

    #[test]
    fn exact_numeric_scale_is_reflected() {
        // numeric(10,2): 1.00, 2.00, 4.00 → 平均 7/3、母分散 14/9 = 1.5555...
        let s = exact(2, &[100, 200, 400]);
        let pop = s
            .finish_decimal(VarianceKind::Pop)
            .expect("ok")
            .expect("some");
        assert!(pop.scale() >= 4);
        assert!(pop.to_string().starts_with("1.5555555555"));
    }

    #[test]
    fn exact_bigint_epoch_micros_do_not_overflow() {
        // 約 1.7e15 の値 10 万行でも N * sum_x2 が i128 を超える中間値を扱える。
        let base: i128 = 1_700_000_000_000_000;
        let mut s = VarianceExact::new(0);
        for i in 0..100_000i128 {
            s.observe(base + (i % 7)).expect("observe");
        }
        let v = s
            .finish_decimal(VarianceKind::Pop)
            .expect("ok")
            .expect("some");
        // 値は 0..7 の周期列の分散（おおよそ 4）に近い。
        let text = v.to_string();
        assert!(
            text.starts_with("3.99") || text.starts_with("4.0"),
            "{text}"
        );
    }

    #[test]
    fn exact_accumulation_overflow_is_out_of_range() {
        let mut s = VarianceExact::new(0);
        s.observe(i128::MAX).expect("first");
        assert!(s.observe(i128::MAX).is_err());
    }

    #[test]
    fn float_matches_known_values() {
        let mut s = VarianceFloat::new();
        assert_eq!(s.finish_f64(VarianceKind::Pop).expect("ok"), None);
        s.observe(1.0).expect("observe");
        assert_eq!(s.finish_f64(VarianceKind::Samp).expect("ok"), None);
        assert_eq!(s.finish_f64(VarianceKind::Pop).expect("ok"), Some(0.0));
        for v in [2.0, 3.0, 4.0] {
            s.observe(v).expect("observe");
        }
        assert_eq!(s.finish_f64(VarianceKind::Pop).expect("ok"), Some(1.25));
        let samp = s.finish_f64(VarianceKind::Samp).expect("ok").expect("some");
        assert!((samp - 5.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn float_overflow_is_out_of_range() {
        let mut s = VarianceFloat::new();
        s.observe(f64::MAX).expect("first");
        assert!(s.observe(f64::MAX).is_err());
    }

    #[test]
    fn kind_from_func() {
        assert_eq!(
            VarianceKind::from_func(AggregateFunc::Variance),
            Some(VarianceKind::Samp)
        );
        assert_eq!(
            VarianceKind::from_func(AggregateFunc::VarPop),
            Some(VarianceKind::Pop)
        );
        assert_eq!(VarianceKind::from_func(AggregateFunc::Sum), None);
    }
}
