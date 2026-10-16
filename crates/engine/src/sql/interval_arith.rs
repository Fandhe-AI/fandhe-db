//! 日付時刻と `INTERVAL` の加減算・乗除・差分の値レベル計算（対象ビヘイビア: TABLE-34。
//! ポインタ: `docs/spec/04-behavior/` の TABLE-34。Issue #1581）。
//!
//! 責務境界: 本モジュールは `sql::udf_call` の評価器（再帰評価と `expr_program` の
//! スタック評価の双方）から呼ばれる純粋関数だけを提供する。暦計算は
//! `crate::datetime`（`days_from_civil`・`civil_from_days`・セッション TimeZone）に
//! 委譲し、ここでは再実装しない。意味論は PostgreSQL 17 の公開済みの挙動
//! （`timestamp_pl_interval`・`timestamptz_pl_interval`・`timestamp_mi`・
//! `interval_mul`・`interval_div`）に合わせる。
//!
//! 安全性: 全演算は `checked_*`／`i128` で行い、範囲外は `22008`
//! （`datetime_field_overflow`）、0 除算は `22012` で fail-closed にする。月・日の加算は
//! 閉形式で計算し、巨大な値でもループしない。エラー文言は固定の英語で値を含めない。

use crate::datetime::{
    civil_from_days, days_from_civil, validate_interval, validate_timestamp_micros,
    validate_timestamptz_micros, Interval, SessionTimeZone, MICROS_PER_DAY,
};
use crate::sql::allowlist::SqlSurfaceError;

/// PostgreSQL の timestamp が表現できる日付範囲（4714 BC-11-24〜294276-12-31）の
/// 下限を 1970-01-01 起点の日数で返す。中間値（月加算後・日加算後）をこの範囲で検査し、
/// 範囲外へ出て戻る入力は PostgreSQL と同じくエラーにする。最終結果の 0001〜9999 年の
/// 検査（`validate_timestamp_micros`）とは分離している。
fn pg_min_days() -> i128 {
    i128::from(days_from_civil(-4713, 11, 24))
}

/// [`pg_min_days`] の上限側（294276-12-31）。
fn pg_max_days() -> i128 {
    i128::from(days_from_civil(294_276, 12, 31))
}

fn interval_out_of_range() -> SqlSurfaceError {
    SqlSurfaceError::datetime_field_overflow("interval out of range")
}

/// 無限大の係数は PG では infinity の interval になるが、本 DB は infinity を扱わないため
/// リテラルの経路と同じ `0A000` で拒否する。
fn infinite_interval_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: "infinite interval values are not supported yet".to_string(),
    }
}

fn timestamp_out_of_range() -> SqlSurfaceError {
    SqlSurfaceError::datetime_field_overflow("timestamp out of range")
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
    }
}

/// 壁時計マイクロ秒（i128）を (通日, 日内マイクロ秒) へ分ける。
fn split_local(local: i128) -> (i128, i128) {
    let day = i128::from(MICROS_PER_DAY);
    (local.div_euclid(day), local.rem_euclid(day))
}

fn join_local(days: i128, tod: i128) -> Result<i128, SqlSurfaceError> {
    if !(pg_min_days()..=pg_max_days()).contains(&days) {
        return Err(timestamp_out_of_range());
    }
    days.checked_mul(i128::from(MICROS_PER_DAY))
        .and_then(|v| v.checked_add(tod))
        .ok_or_else(timestamp_out_of_range)
}

/// 壁時計に月数を加える（日は新しい月の末日へ切り詰める）。
fn add_months_local(local: i128, months: i32) -> Result<i128, SqlSurfaceError> {
    let (days, tod) = split_local(local);
    let days64 = i64::try_from(days).map_err(|_| timestamp_out_of_range())?;
    let (y, m, d) = civil_from_days(days64);
    let total = y
        .checked_mul(12)
        .and_then(|v| v.checked_add(i64::from(m) - 1))
        .and_then(|v| v.checked_add(i64::from(months)))
        .ok_or_else(timestamp_out_of_range)?;
    let ny = total.div_euclid(12);
    let nm = (total.rem_euclid(12) + 1) as u32;
    // 閉区間 [-4713, 294276] 年は予備検査（-4713 は 11-24 未満を含むため正確な下限は
    // join_local の通日検査が判定する）。外は即エラー（days_from_civil の保険）。
    if !(-4713..=294_276).contains(&ny) {
        return Err(timestamp_out_of_range());
    }
    let nd = d.min(days_in_month(ny, nm));
    join_local(i128::from(days_from_civil(ny, nm, nd)), tod)
}

/// 壁時計に日数を加える。
fn add_days_local(local: i128, days: i32) -> Result<i128, SqlSurfaceError> {
    let (d, tod) = split_local(local);
    let nd = d
        .checked_add(i128::from(days))
        .ok_or_else(timestamp_out_of_range)?;
    join_local(nd, tod)
}

/// 月 → 日の順に壁時計へ加算する（PG の `timestamp_pl_interval` と同順）。
fn shift_local(local: i128, iv: &Interval) -> Result<i128, SqlSurfaceError> {
    let mut v = local;
    if iv.months != 0 {
        v = add_months_local(v, iv.months)?;
    }
    if iv.days != 0 {
        v = add_days_local(v, iv.days)?;
    }
    Ok(v)
}

/// `INTERVAL` の符号反転（`i32::MIN`／`i64::MIN` は否定できず `22008`）。
pub(crate) fn negate_interval(iv: &Interval) -> Result<Interval, SqlSurfaceError> {
    let out = Interval {
        months: iv.months.checked_neg().ok_or_else(interval_out_of_range)?,
        days: iv.days.checked_neg().ok_or_else(interval_out_of_range)?,
        micros: iv.micros.checked_neg().ok_or_else(interval_out_of_range)?,
    };
    check_finite(out)
}

/// 結果が無限大の予約値（番兵）と衝突したら `22008`。
fn check_finite(iv: Interval) -> Result<Interval, SqlSurfaceError> {
    if validate_interval(&iv) {
        Ok(iv)
    } else {
        Err(interval_out_of_range())
    }
}

/// `TIMESTAMP + INTERVAL`（PG の `timestamp_pl_interval`）。減算は呼び出し側が
/// [`negate_interval`] した値を渡す。
pub(crate) fn timestamp_add_interval(ts: i64, iv: &Interval) -> Result<i64, SqlSurfaceError> {
    let shifted = shift_local(i128::from(ts), iv)?;
    let out = shifted
        .checked_add(i128::from(iv.micros))
        .ok_or_else(timestamp_out_of_range)?;
    let out = i64::try_from(out).map_err(|_| timestamp_out_of_range())?;
    if validate_timestamp_micros(out) {
        Ok(out)
    } else {
        Err(timestamp_out_of_range())
    }
}

/// `TIMESTAMPTZ + INTERVAL`（PG の `timestamptz_pl_interval`）。月・日の加算は
/// セッション TZ の壁時計で行い、UTC へ戻す際の DST gap／fold は
/// `SessionTimeZone::local_to_utc_micros` の規則に従う。`months`・`days` が共に 0 の
/// ときは TZ に依存せず UTC の時点へマイクロ秒を足すだけ。
pub(crate) fn timestamptz_add_interval(
    utc: i64,
    iv: &Interval,
    tz: &SessionTimeZone,
) -> Result<i64, SqlSurfaceError> {
    // PG の `timestamptz_pl_interval` と同じく、月と日はそれぞれ別の「壁時計 -> UTC」往復で
    // 加算する（月の結果が DST の gap に入ると UTC への変換で正規化され、日の加算は
    // 正規化後の時点の offset から始まる）。
    let mut cur = utc;
    for step in [
        Interval {
            months: iv.months,
            days: 0,
            micros: 0,
        },
        Interval {
            months: 0,
            days: iv.days,
            micros: 0,
        },
    ] {
        if step.months == 0 && step.days == 0 {
            continue;
        }
        let off = i128::from(tz.offset_seconds_at(cur)) * 1_000_000;
        let local = i128::from(cur) + off;
        let shifted = shift_local(local, &step)?;
        let local64 = i64::try_from(shifted).map_err(|_| timestamp_out_of_range())?;
        cur = tz
            .local_to_utc_micros(local64)
            .ok_or_else(timestamp_out_of_range)?;
    }
    let out = cur
        .checked_add(iv.micros)
        .ok_or_else(timestamp_out_of_range)?;
    if validate_timestamptz_micros(out) {
        Ok(out)
    } else {
        Err(timestamp_out_of_range())
    }
}

/// `TIMESTAMP − TIMESTAMP`／`TIMESTAMPTZ − TIMESTAMPTZ`（PG の `timestamp_mi` ＋
/// `interval_justify_hours`）。結果は日数＋日内マイクロ秒（符号を揃える）。
pub(crate) fn timestamp_diff(a: i64, b: i64) -> Result<Interval, SqlSurfaceError> {
    let diff = a.checked_sub(b).ok_or_else(interval_out_of_range)?;
    let mut days = diff / MICROS_PER_DAY;
    let mut micros = diff % MICROS_PER_DAY;
    if days > 0 && micros < 0 {
        micros += MICROS_PER_DAY;
        days -= 1;
    } else if days < 0 && micros > 0 {
        micros -= MICROS_PER_DAY;
        days += 1;
    }
    let days = i32::try_from(days).map_err(|_| interval_out_of_range())?;
    check_finite(Interval {
        months: 0,
        days,
        micros,
    })
}

/// `INTERVAL + INTERVAL`。
pub(crate) fn interval_add(a: &Interval, b: &Interval) -> Result<Interval, SqlSurfaceError> {
    check_finite(Interval {
        months: a
            .months
            .checked_add(b.months)
            .ok_or_else(interval_out_of_range)?,
        days: a
            .days
            .checked_add(b.days)
            .ok_or_else(interval_out_of_range)?,
        micros: a
            .micros
            .checked_add(b.micros)
            .ok_or_else(interval_out_of_range)?,
    })
}

/// `INTERVAL − INTERVAL`。
pub(crate) fn interval_sub(a: &Interval, b: &Interval) -> Result<Interval, SqlSurfaceError> {
    check_finite(Interval {
        months: a
            .months
            .checked_sub(b.months)
            .ok_or_else(interval_out_of_range)?,
        days: a
            .days
            .checked_sub(b.days)
            .ok_or_else(interval_out_of_range)?,
        micros: a
            .micros
            .checked_sub(b.micros)
            .ok_or_else(interval_out_of_range)?,
    })
}

/// PG の `TSROUND`（マイクロ秒単位の偶数丸め）。
fn tsround(v: f64) -> f64 {
    (v * 1_000_000.0).round_ties_even() / 1_000_000.0
}

fn fits_i32(v: f64) -> bool {
    (-2_147_483_648.0..2_147_483_648.0).contains(&v)
}

fn fits_i64(v: f64) -> bool {
    (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&v)
}

/// 乗除の共通カスケード（PG の `interval_mul`／`interval_div`）。`scale` は各要素へ
/// 適用する写像（乗算は `x * factor`、除算は `x / factor`）。
fn interval_scale(iv: &Interval, scale: impl Fn(f64) -> f64) -> Result<Interval, SqlSurfaceError> {
    let month_f = scale(f64::from(iv.months));
    if !fits_i32(month_f) {
        return Err(interval_out_of_range());
    }
    let months = month_f as i32;
    let day_f = scale(f64::from(iv.days));
    if !fits_i32(day_f) {
        return Err(interval_out_of_range());
    }
    let mut days = day_f as i32;
    // 整数部は上で処理済み。小数部を月 → 日 → 秒へカスケードする（上へは繰り上げない）。
    let month_rem_days = tsround((month_f - f64::from(months)) * 30.0);
    let mut sec_rem = tsround(
        (day_f - f64::from(days) + month_rem_days - f64::from(month_rem_days as i32)) * 86_400.0,
    );
    if sec_rem.abs() >= 86_400.0 {
        let whole = (sec_rem / 86_400.0) as i32;
        days = days.checked_add(whole).ok_or_else(interval_out_of_range)?;
        sec_rem -= f64::from(whole) * 86_400.0;
    }
    days = days
        .checked_add(month_rem_days as i32)
        .ok_or_else(interval_out_of_range)?;
    let micros_f = (scale(iv.micros as f64) + sec_rem * 1_000_000.0).round_ties_even();
    if !fits_i64(micros_f) {
        return Err(interval_out_of_range());
    }
    check_finite(Interval {
        months,
        days,
        micros: micros_f as i64,
    })
}

/// `INTERVAL * 数値`（PG17 の `interval_mul`）。NaN・±∞ は `22008`。
pub(crate) fn interval_mul_f64(iv: &Interval, factor: f64) -> Result<Interval, SqlSurfaceError> {
    if factor.is_nan() {
        return Err(interval_out_of_range());
    }
    if factor.is_infinite() {
        return Err(infinite_interval_unsupported());
    }
    interval_scale(iv, |x| x * factor)
}

/// `INTERVAL / 数値`（PG17 の `interval_div`）。0 は `22012`、NaN・±∞ は `22008`。
pub(crate) fn interval_div_checked(
    iv: &Interval,
    factor: f64,
) -> Result<Interval, SqlSurfaceError> {
    if factor == 0.0 {
        return Err(SqlSurfaceError::division_by_zero("division by zero"));
    }
    if factor.is_nan() {
        return Err(interval_out_of_range());
    }
    if factor.is_infinite() {
        return Err(infinite_interval_unsupported());
    }
    interval_scale(iv, |x| x / factor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::{format_interval, format_timestamp, parse_interval, parse_timestamp};

    fn iv(s: &str) -> Interval {
        parse_interval(s).unwrap()
    }
    fn ts(s: &str) -> i64 {
        let full = if s.contains(':') {
            s.to_string()
        } else {
            format!("{s} 00:00:00")
        };
        parse_timestamp(&full).unwrap()
    }
    fn add(t: &str, i: &str) -> String {
        format_timestamp(timestamp_add_interval(ts(t), &iv(i)).unwrap())
    }
    fn sub(t: &str, i: &str) -> String {
        let n = negate_interval(&iv(i)).unwrap();
        format_timestamp(timestamp_add_interval(ts(t), &n).unwrap())
    }
    fn code(e: SqlSurfaceError) -> &'static str {
        e.wire_code()
    }

    #[test]
    fn month_end_and_leap_year() {
        assert_eq!(add("2024-01-31", "1 mon"), "2024-02-29 00:00:00");
        assert_eq!(add("2023-01-31", "1 mon"), "2023-02-28 00:00:00");
        assert_eq!(add("2024-02-29", "1 year"), "2025-02-28 00:00:00");
        assert_eq!(sub("2024-03-31", "1 mon"), "2024-02-29 00:00:00");
        assert_eq!(add("2024-01-31", "1 mon 1 day"), "2024-03-01 00:00:00");
        let once = add("2024-01-31", "1 mon");
        assert_eq!(add(&once, "1 mon"), "2024-03-29 00:00:00");
        assert_eq!(add("1900-02-28", "1 day"), "1900-03-01 00:00:00");
        assert_eq!(add("2000-02-28", "1 day"), "2000-02-29 00:00:00");
    }

    #[test]
    fn diff_justifies_hours() {
        let d = timestamp_diff(ts("2024-03-01"), ts("2024-02-01")).unwrap();
        assert_eq!(format_interval(&d), "29 days");
        let d = timestamp_diff(ts("2024-01-02 06:00:00"), ts("2024-01-01")).unwrap();
        assert_eq!(format_interval(&d), "1 day 06:00:00");
        let d = timestamp_diff(ts("2024-01-01"), ts("2024-01-02 06:00:00")).unwrap();
        assert_eq!(format_interval(&d), "-1 days -06:00:00");
        let d = timestamp_diff(ts("9999-12-31"), ts("0001-01-01")).unwrap();
        assert_eq!(format_interval(&d), "3652058 days");
    }

    #[test]
    fn mul_div_cascade() {
        let f = |s: &str, k: f64| format_interval(&interval_mul_f64(&iv(s), k).unwrap());
        assert_eq!(f("1 day", 1.5), "1 day 12:00:00");
        assert_eq!(f("1 mon", 0.5), "15 days");
        assert_eq!(f("1 mon", 1.5), "1 mon 15 days");
        let g = |s: &str, k: f64| format_interval(&interval_div_checked(&iv(s), k).unwrap());
        assert_eq!(g("1 mon", 3.0), "10 days");
        assert_eq!(g("1 day", 3.0), "08:00:00");
        assert_eq!(
            code(interval_div_checked(&iv("1 day"), 0.0).unwrap_err()),
            "22012"
        );
        assert_eq!(
            code(interval_mul_f64(&iv("1 day"), f64::NAN).unwrap_err()),
            "22008"
        );
        assert_eq!(
            code(interval_mul_f64(&iv("1 day"), f64::INFINITY).unwrap_err()),
            "0A000"
        );
    }

    #[test]
    fn intermediate_beyond_9999_is_allowed_when_final_in_range() {
        // 月加算直後は 12000 年（0001〜9999 年の外）だが PG の timestamp 範囲内。
        let out =
            timestamp_add_interval(ts("2000-01-01"), &iv("120000 mons -3652425 days")).unwrap();
        assert!(validate_timestamp_micros(out));
        assert_eq!(pg_min_days(), -2_440_588);
    }

    #[test]
    fn overflow_is_22008() {
        let e = interval_add(&iv("2147483647 mons"), &iv("1 mon")).unwrap_err();
        assert_eq!(code(e), "22008");
        let e = interval_mul_f64(&iv("2147483647 days"), 2.0).unwrap_err();
        assert_eq!(code(e), "22008");
        let e = timestamp_add_interval(ts("9999-12-31"), &iv("1 day")).unwrap_err();
        assert_eq!(code(e), "22008");
        let min = Interval {
            months: i32::MIN,
            days: 0,
            micros: 0,
        };
        assert_eq!(code(negate_interval(&min).unwrap_err()), "22008");
        // 中間値が PG の範囲を超える入力（戻っても）エラー。
        let huge = Interval {
            months: i32::MAX,
            days: 0,
            micros: 0,
        };
        assert_eq!(
            code(timestamp_add_interval(ts("2000-01-01"), &huge).unwrap_err()),
            "22008"
        );
    }

    #[test]
    fn timestamptz_dst() {
        let tz = SessionTimeZone::parse_setting("America/New_York").unwrap();
        let t = crate::datetime::parse_timestamptz("2024-03-09 12:00:00-05", &tz).unwrap();
        let next = timestamptz_add_interval(t, &iv("1 day"), &tz).unwrap();
        assert_eq!(
            crate::datetime::format_timestamptz(next, &tz),
            "2024-03-10 12:00:00-04"
        );
        let next = timestamptz_add_interval(t, &iv("24 hours"), &tz).unwrap();
        assert_eq!(
            crate::datetime::format_timestamptz(next, &tz),
            "2024-03-10 13:00:00-04"
        );
        // 月の結果が gap に入ると正規化（03:30 EDT）されてから日が加算される。
        let t = crate::datetime::parse_timestamptz("2024-02-10 02:30:00-05", &tz).unwrap();
        let next = timestamptz_add_interval(t, &iv("1 mon 1 day"), &tz).unwrap();
        assert_eq!(
            crate::datetime::format_timestamptz(next, &tz),
            "2024-03-11 03:30:00-04"
        );
    }

    /// 月加算後の中間値が jiff の年範囲（9999 年）を超えても、最終値が有効範囲内なら成功する。
    #[test]
    fn timestamptz_intermediate_beyond_jiff_range() {
        let tz = SessionTimeZone::parse_setting("America/New_York").unwrap();
        let t = crate::datetime::parse_timestamptz("2000-01-01 12:00:00-05", &tz).unwrap();
        let next = timestamptz_add_interval(t, &iv("120000 mons -3652425 days"), &tz).unwrap();
        let s = crate::datetime::format_timestamptz(next, &tz);
        assert!(s.starts_with("2000-") || s.starts_with("1999-"), "{s}");
        // 最終値が範囲外なら従来どおり 22008。
        assert_eq!(
            code(timestamptz_add_interval(t, &iv("120000 mons"), &tz).unwrap_err()),
            "22008"
        );
    }

    /// 範囲外の未来年の中間値が 2007 年以前の歴史的 DST 規則を拾わない（将来規則を使う）。
    /// 存在しない現地時刻 02:30（gap）を含む往復で元の時点へ戻る（Issue #1581 レビュー指摘）。
    #[test]
    fn timestamptz_far_future_intermediate_uses_future_rules() {
        let tz = SessionTimeZone::parse_setting("America/New_York").unwrap();
        let t = crate::datetime::parse_timestamptz("2000-04-01 02:30:00-05", &tz).unwrap();
        let fwd = timestamptz_add_interval(t, &iv("120012 mons -3652790 days"), &tz).unwrap();
        // 中間値 12001-04-01 02:30 は将来規則（3 月から EDT）では存在する壁時計のまま保たれ、
        // 2001 年の歴史的規則（4 月第 1 日曜）の gap として 03:30 へずれない。
        let s = crate::datetime::format_timestamptz(fwd, &tz);
        assert!(s.contains(" 02:30:00-0"), "{s}");
        // 日内の壁時計がずれていなければ、元の時点から日・月の整数倍だけ離れた位置にある。
        assert_eq!((fwd - t).rem_euclid(3_600_000_000), 0);
    }
}
