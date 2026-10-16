//! `DATE`／`TIMESTAMP` 向け日時スカラー関数群（`date_part`／`date_trunc`）と
//! `DATE` 算術（加減算・差分）の値レベル計算（対象ビヘイビア: SQL-26。ポインタ:
//! `docs/spec/05-tasks.md` TASK-210・`docs/spec/04-behavior/sql-surface.md`
//! SQL-26。Issue #920）。
//!
//! 責務境界: 本モジュールは `crate::datetime`（`DATE`／`TIMESTAMP` の内部表現・
//! 暦計算の単一情報源）の上に構築する純粋関数のみを提供し、`sql::udf_call`
//! （束縛・評価・型検査）から呼ばれる。field／unit の文字列解決
//! （[`DatePartField::from_name`]・[`DateTruncUnit::from_name`]）は
//! `sql::udf_call::bind_call` が束縛時に一度だけ行い、行ループでは払い出し済みの
//! enum のみを扱う（行ごとの文字列比較を避ける設計。§2-4 参照）。
//!
//! `TIMESTAMP`／`TIMESTAMPTZ` と `INTERVAL` の加減算・差分は `crate::sql::interval_arith`（TABLE-34。
//! Issue #1581）が担い、本モジュールは `DATE ± n` の日数算術だけを持つ。
//!
//! `TIME` 入力（Issue #1570）は [`time_part`] が担う。`date_part` は暦計算で日付境界へ
//! 折り返す（`24:00:00` が hour=0 になる）ため流用せず、0 時起点のマイクロ秒をそのまま分解する。
//!
//! `INTERVAL` 入力（Issue #1582・TABLE-34）は [`interval_part`]／[`interval_trunc`] が担う。
//! 分解は PostgreSQL の `interval2itm` と同じく時間を日へ繰り上げず（`'36 hours'` は day 0・
//! hour 36）、符号付き整数除算は 0 方向への切り捨て（暦計算の `div_euclid` は流用しない）。
//!
//! 意味論は PostgreSQL 互換（`date_part`／`EXTRACT`／`date_trunc` の公開済み
//! 挙動）に合わせるが、以下は既知の差分として `docs/design/
//! datetime-scalar-functions.md`（ADR）に記録する: `TIMESTAMPTZ` 非対応・
//! `DATE` 入力の暗黙昇格・`EXTRACT` の既定列名差分等。spec 本文は転記せず
//! TASK-210・SQL-26 のポインタのみを付す。

use crate::datetime::{
    civil_from_days, days_from_civil, validate_interval, Interval, MICROS_PER_DAY,
};
use crate::sql::allowlist::SqlSurfaceError;

/// `date_part`／`EXTRACT` が受理する field（大小無視・単数形/複数形の一部を
/// 許容。§2-3）。`Copy + Eq` にして [`crate::sql::udf_call::BuiltinFn::DatePart`]
/// のペイロードとして持ち回れるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePartField {
    Microseconds,
    Milliseconds,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
    Dow,
    IsoDow,
    Doy,
    IsoYear,
    Epoch,
    /// `timezone`・`timezone_hour`・`timezone_minute`（`TIMESTAMPTZ` 専用。UTC からの offset。
    /// Issue #1576）。`TIMESTAMP`／`DATE`／`TIME` に対しては束縛時に `0A000` で拒否する。
    Timezone,
    TimezoneHour,
    TimezoneMinute,
}

impl DatePartField {
    /// 大小無視・単数形/複数形いずれも受理する（PostgreSQL 互換。未知の field は
    /// `None`。呼び出し元〔`bind_call`〕が `22000` へ写像する）。
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "microseconds" | "microsecond" => Self::Microseconds,
            "milliseconds" | "millisecond" => Self::Milliseconds,
            "second" | "seconds" => Self::Second,
            "minute" | "minutes" => Self::Minute,
            "hour" | "hours" => Self::Hour,
            "day" | "days" => Self::Day,
            "week" | "weeks" => Self::Week,
            "month" | "months" => Self::Month,
            "quarter" | "quarters" => Self::Quarter,
            "year" | "years" => Self::Year,
            "decade" | "decades" => Self::Decade,
            "century" | "centuries" => Self::Century,
            "millennium" | "millenniums" | "millennia" => Self::Millennium,
            "dow" => Self::Dow,
            "isodow" => Self::IsoDow,
            "doy" => Self::Doy,
            "isoyear" => Self::IsoYear,
            "epoch" => Self::Epoch,
            "timezone" => Self::Timezone,
            "timezone_hour" => Self::TimezoneHour,
            "timezone_minute" => Self::TimezoneMinute,
            _ => return None,
        })
    }
}

/// `date_trunc` が受理する unit（`dow`／`isodow`／`doy`／`isoyear`／`epoch` は
/// 切り捨て先として意味を持たないため対象外。§2-3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTruncUnit {
    Microseconds,
    Milliseconds,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
}

impl DateTruncUnit {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "microseconds" | "microsecond" => Self::Microseconds,
            "milliseconds" | "millisecond" => Self::Milliseconds,
            "second" | "seconds" => Self::Second,
            "minute" | "minutes" => Self::Minute,
            "hour" | "hours" => Self::Hour,
            "day" | "days" => Self::Day,
            "week" | "weeks" => Self::Week,
            "month" | "months" => Self::Month,
            "quarter" | "quarters" => Self::Quarter,
            "year" | "years" => Self::Year,
            "decade" | "decades" => Self::Decade,
            "century" | "centuries" => Self::Century,
            "millennium" | "millenniums" | "millennia" => Self::Millennium,
            _ => return None,
        })
    }
}

/// 1970-01-01 起点の日数 `days` の曜日を日曜 = 0 として返す（`rem_euclid` で
/// 負の日数〔紀元前寄りの日付〕でも 0..=6 に収める）。1970-01-01 は木曜日
/// （曜日 4）であることを基準にする。
fn dow_sunday_zero(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

/// ISO 8601 の暦年 `iso_year` における「第 1 週の月曜日」の 1970-01-01 起点日数。
/// 1 月 4 日を含む週が常に第 1 週になる ISO 8601 の定義をそのまま使う。
fn iso_week1_monday(iso_year: i64) -> i64 {
    let jan4 = days_from_civil(iso_year, 1, 4);
    let jan4_isodow = {
        let d = dow_sunday_zero(jan4);
        if d == 0 {
            7
        } else {
            d
        }
    };
    jan4 - (jan4_isodow - 1)
}

/// `days`（1970-01-01 起点）の ISO 週番号と ISO 暦年を返す（`(isoyear, week)`）。
/// 年境界をまたぐ週（12/29〜1/3 付近）は所属する ISO 暦年が西暦年と食い違う
/// ことがある（ISO 8601 の定義どおり）。
fn iso_week_and_year(days: i64) -> (i64, i64) {
    let (calendar_year, _, _) = civil_from_days(days);
    let mut iso_year = calendar_year;
    let mut week_start = iso_week1_monday(iso_year);
    if days < week_start {
        iso_year -= 1;
        week_start = iso_week1_monday(iso_year);
    } else {
        let next_start = iso_week1_monday(iso_year + 1);
        if days >= next_start {
            iso_year += 1;
            week_start = next_start;
        }
    }
    let week = (days - week_start) / 7 + 1;
    (iso_year, week)
}

/// `date_part(field, src)`／`EXTRACT(field FROM src)` の値レベル計算。`micros`
/// は `TIMESTAMP` の内部表現（`DATE` は呼び出し元〔`bind_call`〕が
/// `BuiltinFn::DateToTimestamp` で深夜 0 時の `TIMESTAMP` へ昇格済み）。
/// 純粋関数で常に有限値を返すため `Result` を返さない（§2-2）。
pub(crate) fn date_part(field: DatePartField, micros: i64) -> f64 {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = day_micros / 3_600_000_000;
    let minute = (day_micros % 3_600_000_000) / 60_000_000;
    // その分内の秒（小数秒含む）をマイクロ秒単位で表した値。`second`／
    // `milliseconds`／`microseconds` の共有基点。
    let micros_in_minute = day_micros % 60_000_000;
    match field {
        DatePartField::Microseconds => micros_in_minute as f64,
        DatePartField::Milliseconds => micros_in_minute as f64 / 1_000.0,
        DatePartField::Second => micros_in_minute as f64 / 1_000_000.0,
        DatePartField::Minute => minute as f64,
        DatePartField::Hour => hour as f64,
        DatePartField::Day => f64::from(day),
        DatePartField::Month => f64::from(month),
        DatePartField::Quarter => ((i64::from(month) - 1) / 3 + 1) as f64,
        DatePartField::Year => year as f64,
        DatePartField::Decade => year.div_euclid(10) as f64,
        DatePartField::Century => (year + 99).div_euclid(100) as f64,
        DatePartField::Millennium => (year + 999).div_euclid(1000) as f64,
        DatePartField::Dow => dow_sunday_zero(days) as f64,
        DatePartField::IsoDow => {
            let d = dow_sunday_zero(days);
            if d == 0 {
                7.0
            } else {
                d as f64
            }
        }
        DatePartField::Doy => {
            (days_from_civil(year, month, day) - days_from_civil(year, 1, 1) + 1) as f64
        }
        DatePartField::IsoYear => iso_week_and_year(days).0 as f64,
        DatePartField::Week => iso_week_and_year(days).1 as f64,
        DatePartField::Epoch => micros as f64 / 1_000_000.0,
        // 束縛時に `TIMESTAMPTZ` 以外へは到達させない（`0A000`）。到達した場合の値は NaN とし、
        // 呼び出し元の `finite_scalar` が内部エラーへ倒す（fail-closed）。
        DatePartField::Timezone | DatePartField::TimezoneHour | DatePartField::TimezoneMinute => {
            f64::NAN
        }
    }
}

/// `TIMESTAMPTZ` の `date_part`／`EXTRACT`（Issue #1576）。`epoch` は UTC の絶対値、
/// `timezone*` は `tz` の offset、それ以外は `tz` の壁時計での暦計算（[`date_part`]）。
/// 壁時計が `TIMESTAMP` の範囲を外れる入力は `22008`。
pub(crate) fn timestamptz_part(
    field: DatePartField,
    utc_micros: i64,
    tz: &crate::datetime::SessionTimeZone,
) -> Result<f64, SqlSurfaceError> {
    let offset = tz.offset_seconds_at(utc_micros);
    Ok(match field {
        DatePartField::Epoch => utc_micros as f64 / 1_000_000.0,
        DatePartField::Timezone => f64::from(offset),
        DatePartField::TimezoneHour => f64::from(offset / 3600),
        DatePartField::TimezoneMinute => f64::from((offset % 3600) / 60),
        _ => {
            let local = tz
                .timestamptz_to_timestamp(utc_micros)
                .ok_or_else(tz_out_of_range)?;
            date_part(field, local)
        }
    })
}

/// `date_trunc(unit, timestamptz[, zone])`（Issue #1576）。壁時計で切り捨て、day 以上の単位は
/// 切り捨て後の局所時刻を DST の gap／fold 規則で UTC へ再解決し、hour 以下の単位は元の
/// offset を保つ（PG 17 の実測）。
pub(crate) fn timestamptz_trunc(
    unit: DateTruncUnit,
    utc_micros: i64,
    tz: &crate::datetime::SessionTimeZone,
) -> Result<i64, SqlSurfaceError> {
    let local = tz
        .timestamptz_to_timestamp(utc_micros)
        .ok_or_else(tz_out_of_range)?;
    let truncated = date_trunc(unit, local)?;
    let utc = match unit {
        DateTruncUnit::Microseconds
        | DateTruncUnit::Milliseconds
        | DateTruncUnit::Second
        | DateTruncUnit::Minute
        | DateTruncUnit::Hour => {
            let offset = i64::from(tz.offset_seconds_at(utc_micros)) * 1_000_000;
            truncated.checked_sub(offset)
        }
        _ => tz.timestamp_to_timestamptz(truncated),
    };
    utc.filter(|u| crate::datetime::validate_timestamptz_micros(*u))
        .ok_or_else(tz_out_of_range)
}

/// `TIMESTAMPTZ` の変換結果が表現範囲外のときの共通エラー（`22008`）。
pub(crate) fn tz_out_of_range() -> SqlSurfaceError {
    SqlSurfaceError::datetime_field_overflow("timestamptz value is out of the representable range")
}

/// `TIME` が `time_part` で扱える field か（束縛時の拒否判定に使う。Issue #1570）。
/// 時刻成分（`hour`/`minute`/`second`/`milliseconds`/`microseconds`）と `epoch` だけが
/// 意味を持ち、日付由来の field は `time_part` が `None` を返す。
pub(crate) fn time_supports_field(field: DatePartField) -> bool {
    time_part(field, 0).is_some()
}

/// `date_part(field, time)`／`EXTRACT(field FROM time)` の値レベル計算（Issue #1570）。
/// `micros` は `TIME` の内部表現（0 時起点のマイクロ秒。`24:00:00` = `86_400_000_000` を
/// 含む）。`24:00:00` は hour=24・minute=0・second=0・epoch=86400（PostgreSQL と同じ）。
/// 日付由来の field（`year`/`month`/`day`/`dow` 等）は `None`（束縛時に `0A000` で拒否済みの
/// ため評価時には到達しない）。
pub(crate) fn time_part(field: DatePartField, micros: i64) -> Option<f64> {
    let hour = micros / 3_600_000_000;
    let minute = (micros % 3_600_000_000) / 60_000_000;
    let micros_in_minute = micros % 60_000_000;
    match field {
        DatePartField::Hour => Some(hour as f64),
        DatePartField::Minute => Some(minute as f64),
        DatePartField::Second => Some(micros_in_minute as f64 / 1_000_000.0),
        DatePartField::Milliseconds => Some(micros_in_minute as f64 / 1_000.0),
        DatePartField::Microseconds => Some(micros_in_minute as f64),
        DatePartField::Epoch => Some(micros as f64 / 1_000_000.0),
        DatePartField::Day
        | DatePartField::Month
        | DatePartField::Quarter
        | DatePartField::Year
        | DatePartField::Decade
        | DatePartField::Century
        | DatePartField::Millennium
        | DatePartField::Dow
        | DatePartField::IsoDow
        | DatePartField::Doy
        | DatePartField::IsoYear
        | DatePartField::Week
        | DatePartField::Timezone
        | DatePartField::TimezoneHour
        | DatePartField::TimezoneMinute => None,
    }
}

/// `INTERVAL` を PostgreSQL の `interval2itm` と同じ規則で分解した値（Issue #1582）。
/// 年・月は `months` を 12 で割った商・余り、日はそのまま、時間以下は `micros` から
/// 時・分・秒・マイクロ秒へ切り出す（日へは繰り上げない）。符号は各要素が元の符号を保つ。
#[derive(Debug, Clone, Copy)]
struct IntervalParts {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    usec: i64,
}

impl IntervalParts {
    fn from_interval(iv: &Interval) -> Self {
        let months = i64::from(iv.months);
        let mut t = iv.micros;
        let hour = t / 3_600_000_000;
        t %= 3_600_000_000;
        let min = t / 60_000_000;
        t %= 60_000_000;
        let sec = t / 1_000_000;
        let usec = t % 1_000_000;
        Self {
            year: months / 12,
            mon: months % 12,
            mday: i64::from(iv.days),
            hour,
            min,
            sec,
            usec,
        }
    }
}

/// `INTERVAL` が `date_part` で扱える field か（束縛時の拒否判定。Issue #1582）。
/// `week`・`dow`・`isodow`・`doy`・`isoyear`・`timezone*` は PostgreSQL と同じく非対応（`0A000`）。
pub(crate) fn interval_supports_field(field: DatePartField) -> bool {
    !matches!(
        field,
        DatePartField::Week
            | DatePartField::Dow
            | DatePartField::IsoDow
            | DatePartField::Doy
            | DatePartField::IsoYear
            | DatePartField::Timezone
            | DatePartField::TimezoneHour
            | DatePartField::TimezoneMinute
    )
}

/// `date_part(field, interval)`／`EXTRACT(field FROM interval)` の値レベル計算（Issue #1582。
/// PostgreSQL 17 の `interval_part_common` の float8 経路）。未対応 field は `None`
/// （束縛時に `0A000` で拒否済みのため評価時には到達しない）。
pub(crate) fn interval_part(field: DatePartField, iv: &Interval) -> Option<f64> {
    let p = IntervalParts::from_interval(iv);
    Some(match field {
        DatePartField::Microseconds => (p.sec * 1_000_000 + p.usec) as f64,
        DatePartField::Milliseconds => p.sec as f64 * 1000.0 + p.usec as f64 / 1000.0,
        DatePartField::Second => p.sec as f64 + p.usec as f64 / 1_000_000.0,
        DatePartField::Minute => p.min as f64,
        DatePartField::Hour => p.hour as f64,
        DatePartField::Day => p.mday as f64,
        DatePartField::Month => p.mon as f64,
        DatePartField::Quarter => (p.mon / 3 + 1) as f64,
        DatePartField::Year => p.year as f64,
        DatePartField::Decade => (p.year / 10) as f64,
        DatePartField::Century => (p.year / 100) as f64,
        DatePartField::Millennium => (p.year / 1000) as f64,
        DatePartField::Epoch => {
            let months = i64::from(iv.months);
            let mut r = iv.micros as f64 / 1_000_000.0;
            r += (365.25 * 86_400.0) * (months / 12) as f64;
            r += (30.0 * 86_400.0) * (months % 12) as f64;
            r += 86_400.0 * f64::from(iv.days);
            r
        }
        DatePartField::Week
        | DatePartField::Dow
        | DatePartField::IsoDow
        | DatePartField::Doy
        | DatePartField::IsoYear
        | DatePartField::Timezone
        | DatePartField::TimezoneHour
        | DatePartField::TimezoneMinute => return None,
    })
}

fn interval_trunc_out_of_range() -> SqlSurfaceError {
    SqlSurfaceError::datetime_field_overflow("interval out of range")
}

/// `date_trunc(unit, interval)` の値レベル計算（Issue #1582。PostgreSQL 17 の `interval_trunc`）。
/// 上位の単位から下位へ順に切り落とす。`week` は PostgreSQL と同じく非対応（束縛時に `0A000`
/// 済みのため評価時には `0A000` を返す fail-closed）。あふれ・番兵衝突は `22008`。
pub(crate) fn interval_trunc(
    unit: DateTruncUnit,
    iv: &Interval,
) -> Result<Interval, SqlSurfaceError> {
    let mut p = IntervalParts::from_interval(iv);
    if unit == DateTruncUnit::Week {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "date_trunc unit \"week\" is not supported for type interval".to_string(),
        });
    }
    // 上位 unit の切り捨ては下位 unit の切り捨てを含む（PostgreSQL の FALL THRU と同じ）。
    let rank = |u: DateTruncUnit| -> u8 {
        match u {
            DateTruncUnit::Millennium => 12,
            DateTruncUnit::Century => 11,
            DateTruncUnit::Decade => 10,
            DateTruncUnit::Year => 9,
            DateTruncUnit::Quarter => 8,
            DateTruncUnit::Month => 7,
            DateTruncUnit::Week => 6,
            DateTruncUnit::Day => 5,
            DateTruncUnit::Hour => 4,
            DateTruncUnit::Minute => 3,
            DateTruncUnit::Second => 2,
            DateTruncUnit::Milliseconds => 1,
            DateTruncUnit::Microseconds => 0,
        }
    };
    let r = rank(unit);
    if r >= 12 {
        p.year = (p.year / 1000) * 1000;
    }
    if r >= 11 {
        p.year = (p.year / 100) * 100;
    }
    if r >= 10 {
        p.year = (p.year / 10) * 10;
    }
    if r >= 9 {
        p.mon = 0;
    }
    if r == 8 {
        p.mon = 3 * (p.mon / 3);
    }
    if r >= 7 {
        p.mday = 0;
    }
    if r >= 5 {
        p.hour = 0;
    }
    if r >= 4 {
        p.min = 0;
    }
    if r >= 3 {
        p.sec = 0;
    }
    if r >= 2 {
        p.usec = 0;
    }
    if r == 1 {
        p.usec = (p.usec / 1000) * 1000;
    }
    let months = p
        .year
        .checked_mul(12)
        .and_then(|v| v.checked_add(p.mon))
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(interval_trunc_out_of_range)?;
    let days = i32::try_from(p.mday).map_err(|_| interval_trunc_out_of_range())?;
    let micros = p
        .hour
        .checked_mul(3_600_000_000)
        .and_then(|v| v.checked_add(p.min * 60_000_000))
        .and_then(|v| v.checked_add(p.sec * 1_000_000))
        .and_then(|v| v.checked_add(p.usec))
        .ok_or_else(interval_trunc_out_of_range)?;
    let out = Interval {
        months,
        days,
        micros,
    };
    if validate_interval(&out) {
        Ok(out)
    } else {
        Err(interval_trunc_out_of_range())
    }
}

/// `days.checked_mul(MICROS_PER_DAY)` のオーバーフローを `22008`
/// （[`SqlSurfaceError::datetime_field_overflow`]）へ写像する共通ヘルパー
/// （`date_trunc` の各 unit 分岐が共有する）。
fn days_to_micros_checked(days: i64) -> Result<i64, SqlSurfaceError> {
    days.checked_mul(MICROS_PER_DAY).ok_or_else(|| {
        SqlSurfaceError::datetime_field_overflow(
            "date_trunc result overflows the representable range",
        )
    })
}

/// `date_trunc(unit, src)` の値レベル計算。結果は常に `TIMESTAMP`
/// （`DATE` 入力は `bind_call` が `DateToTimestamp` で昇格済み。§2-3）。
/// 範囲外（`decade` 切り捨てで年 0 になる等）は
/// [`SqlSurfaceError::datetime_field_overflow`]（`22008`）で拒否する（§2-2）。
pub(crate) fn date_trunc(unit: DateTruncUnit, micros: i64) -> Result<i64, SqlSurfaceError> {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let (year, month, _day) = civil_from_days(days);
    let result_micros = match unit {
        DateTruncUnit::Microseconds => micros,
        DateTruncUnit::Milliseconds => micros - micros.rem_euclid(1_000),
        DateTruncUnit::Second => micros - micros.rem_euclid(1_000_000),
        DateTruncUnit::Minute => micros - micros.rem_euclid(60_000_000),
        DateTruncUnit::Hour => micros - micros.rem_euclid(3_600_000_000),
        DateTruncUnit::Day => days_to_micros_checked(days)?,
        DateTruncUnit::Week => {
            let dow = dow_sunday_zero(days);
            let isodow = if dow == 0 { 7 } else { dow };
            days_to_micros_checked(days - (isodow - 1))?
        }
        DateTruncUnit::Month => days_to_micros_checked(days_from_civil(year, month, 1))?,
        DateTruncUnit::Quarter => {
            let quarter_start_month = (((i64::from(month) - 1) / 3) * 3 + 1) as u32;
            days_to_micros_checked(days_from_civil(year, quarter_start_month, 1))?
        }
        DateTruncUnit::Year => days_to_micros_checked(days_from_civil(year, 1, 1))?,
        // 1〜9 年は `year.div_euclid(10) * 10 == 0` となり暦上存在しない
        // 「西暦 0 年」を指す。`days_from_civil` 自体は範囲外の年でも値を
        // 返す純粋変換のため、最終的な `validate_timestamp_micros`
        // （呼び出し元）が `22008` として拒否する（§2-2 の唯一の date_trunc
        // 範囲外ケース）。
        DateTruncUnit::Decade => {
            days_to_micros_checked(days_from_civil(year.div_euclid(10) * 10, 1, 1))?
        }
        DateTruncUnit::Century => days_to_micros_checked(days_from_civil(
            ((year - 1).div_euclid(100)) * 100 + 1,
            1,
            1,
        ))?,
        DateTruncUnit::Millennium => days_to_micros_checked(days_from_civil(
            ((year - 1).div_euclid(1000)) * 1000 + 1,
            1,
            1,
        ))?,
    };
    if !crate::datetime::validate_timestamp_micros(result_micros) {
        return Err(SqlSurfaceError::datetime_field_overflow(
            "date_trunc result is out of the representable range",
        ));
    }
    Ok(result_micros)
}

/// `n` が整数値の `f64` であり `i32` の受理範囲内であることを検証し、
/// 値を保存したまま `i32` へ変換する（`date_add_days`／`date_sub_days` の
/// 共有前段。両者とも符号反転前にこの検証を行う契約 ― `date_sub_days` の
/// ドキュメンテーションコメント参照）。
///
/// 非整数（NaN・±Inf を含む）は「整数ではない」型不一致として `42804`
/// （[`SqlSurfaceError::datatype_mismatch`]。SQL-26・Issue #1274）で拒否する。
/// 非整数の判定を `i32` 範囲検査（`22003`）より先に行う順序は変えない。
fn validate_and_narrow_date_arithmetic_operand(n: f64) -> Result<i32, SqlSurfaceError> {
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(SqlSurfaceError::datatype_mismatch(
            "DATE arithmetic operand must be a whole number of days",
        ));
    }
    if n < f64::from(i32::MIN) || n > f64::from(i32::MAX) {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "DATE arithmetic operand exceeds the 32-bit day-count range",
        ));
    }
    // 上の範囲検査により `n` は `i32` の範囲内の整数値であることが確定して
    // いるため `as i32` は値を保存する（丸め・切り捨ては発生しない）。
    Ok(n as i32)
}

/// `DATE ± n` の共有実装。`n` を符号反転前に検証し（`sign` が減算方向を表す）、
/// 実際の加減算は `i64` の広い範囲で行ってから `i32`／`DATE` 受理範囲を検証
/// する（[`date_add_days`]・[`date_sub_days`] の前段）。
///
/// codex 指摘対応（PR #1120）: 当初は「`n` を先に符号反転してから `i32` の
/// `checked_add`／`checked_sub` で計算し、桁あふれを `22003`
/// （[`SqlSurfaceError::numeric_out_of_range`]、オペランド範囲外の意味）に
/// 写像する」実装だったが、これには 2 つの契約違反があった:
/// (a) `n == i32::MIN` の符号反転自体が `i32` の範囲をオーバーフローし、
/// 妥当な `n` が「オペランド範囲外」として誤検出される。
/// (b) `n` は妥当（`i32` 範囲内）で `days` も妥当（`DATE` 受理範囲内）でも、
/// 両者の和・差が `i32` の全域（`DATE` の受理範囲よりはるかに広い）を
/// 超えうる。この場合の正しい分類は「オペランドが無効」ではなく「計算結果が
/// `DATE` の受理範囲外」（`22008`）であり、`docs/design/
/// datetime-scalar-functions.md` の型規約が定める 2 分類
/// （`n` 自体が無効なら `22003`、結果が無効なら `22008`）と食い違う。
/// `i64` で計算し、`i32` へ収まるか否かに関わらず「`DATE` として有効か」の
/// 単一の判定（[`crate::datetime::validate_date_days`]）だけで結果の可否を
/// 決めることで、この 2 分類を正しく再現する。
fn date_add_or_sub_days(days: i32, n: f64, sign: i64) -> Result<i32, SqlSurfaceError> {
    let n_i32 = validate_and_narrow_date_arithmetic_operand(n)?;
    // `i32::MIN` を含むすべての `i32` 値・`sign`（`±1`）は `i64` へ無損失に
    // 拡張できるため、ここでの加減算は桁あふれしない。
    let result_i64 = i64::from(days) + sign * i64::from(n_i32);
    let result = i32::try_from(result_i64)
        .ok()
        .filter(|&r| crate::datetime::validate_date_days(r));
    result.ok_or_else(|| {
        SqlSurfaceError::datetime_field_overflow(
            "DATE arithmetic result is out of the representable range",
        )
    })
}

/// `DATE + n`／`n + DATE`（`n` は日数のスカラー）。`n` が整数でない場合は
/// 型不一致の `42804`（Issue #1274）、`i32` 範囲外は `22003`（[`SqlSurfaceError::numeric_out_of_range`]）、
/// 結果が `DATE` の受理範囲外は `22008` で拒否する（§2-2）。
pub(crate) fn date_add_days(days: i32, n: f64) -> Result<i32, SqlSurfaceError> {
    date_add_or_sub_days(days, n, 1)
}

/// `DATE - n`（`n` は日数のスカラー）。エラー分類は [`date_add_days`] と同じ
/// （`42804`／`22003`／`22008`）。
///
/// Cursor Bugbot 指摘対応（PR #1120）: `n` を先に符号反転してから
/// `date_add_days` へ委譲する実装だと、`n == i32::MIN`
/// （`-2147483648`。これ自体は妥当な `i32` 値）の符号反転が `i32` の範囲を
/// オーバーフローし（`-i32::MIN == 2147483648 > i32::MAX`）、妥当な `n` が
/// 「`i32` 範囲外」という誤ったエラー（`22003`）で拒否されてしまっていた。
/// `date_add_or_sub_days`（`sign = -1`）で `n` 自身を符号反転前に検証し、
/// 実際の減算は `i64` の広い範囲で行うことでこの境界値を正しく扱う。
pub(crate) fn date_sub_days(days: i32, n: f64) -> Result<i32, SqlSurfaceError> {
    date_add_or_sub_days(days, n, -1)
}

/// `DATE - DATE`（日数差。両辺は既に `DATE` の受理範囲内に検証済みの内部表現
/// のため、差は必ず `f64` で正確に表現できる ― 最大絶対値は
/// `DATE_MAX_DAYS - DATE_MIN_DAYS` 程度で `2^53` を大きく下回る）。
pub(crate) fn date_diff_days(a: i32, b: i32) -> f64 {
    f64::from(a) - f64::from(b)
}

/// `DATE` を深夜 0 時の `TIMESTAMP` へ昇格する（`BuiltinFn::DateToTimestamp`。
/// `bind_call`／`bind_binary` が `date_part`／`date_trunc` の `src` や
/// `DATE`／`TIMESTAMP` 比較のために挿入する）。
pub(crate) fn date_to_timestamp(days: i32) -> i64 {
    i64::from(days) * MICROS_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::parse_timestamp;

    fn ts(s: &str) -> i64 {
        parse_timestamp(s).expect("fixture timestamp must parse")
    }

    /// 2024-02-29（閏年）12:34:56.789012 の全 field を独立オラクル
    /// （PostgreSQL の公開済み挙動）で固定する。
    #[test]
    fn interval_part_and_trunc() {
        use crate::datetime::{format_interval, parse_interval};
        let iv = |s: &str| parse_interval(s).unwrap();
        let p = |f, s: &str| interval_part(f, &iv(s)).unwrap();
        let v = iv("1 year 2 mons 3 days 04:05:06.789");
        assert_eq!(interval_part(DatePartField::Milliseconds, &v), Some(6789.0));
        assert_eq!(
            interval_part(DatePartField::Epoch, &v),
            Some(37_015_506.789_000_005)
        );
        assert_eq!(p(DatePartField::Quarter, "-14 mons"), 1.0);
        assert_eq!(p(DatePartField::Year, "-14 mons"), -1.0);
        assert_eq!(p(DatePartField::Hour, "36 hours"), 36.0);
        assert_eq!(p(DatePartField::Day, "36 hours"), 0.0);
        assert!(!interval_supports_field(DatePartField::Week));
        assert!(!interval_supports_field(DatePartField::Timezone));
        assert!(interval_part(DatePartField::Dow, &v).is_none());
        let t = |u, s: &str| format_interval(&interval_trunc(u, &iv(s)).unwrap());
        assert_eq!(t(DateTruncUnit::Decade, "-15 years"), "-10 years");
        assert_eq!(t(DateTruncUnit::Quarter, "1 year 5 mons"), "1 year 3 mons");
        assert_eq!(
            t(DateTruncUnit::Milliseconds, "00:00:01.234567"),
            "00:00:01.234"
        );
        assert_eq!(
            interval_trunc(DateTruncUnit::Week, &v)
                .unwrap_err()
                .wire_code(),
            "0A000"
        );
    }

    #[test]
    fn time_part_fields_and_24h_boundary() {
        let t = crate::datetime::parse_time("12:34:56.789012").unwrap();
        assert_eq!(time_part(DatePartField::Hour, t), Some(12.0));
        assert_eq!(time_part(DatePartField::Minute, t), Some(34.0));
        assert!((time_part(DatePartField::Second, t).unwrap() - 56.789012).abs() < 1e-9);
        assert!((time_part(DatePartField::Milliseconds, t).unwrap() - 56_789.012).abs() < 1e-6);
        assert!((time_part(DatePartField::Microseconds, t).unwrap() - 56_789_012.0).abs() < 1e-3);
        let end = crate::datetime::parse_time("24:00:00").unwrap();
        assert_eq!(time_part(DatePartField::Hour, end), Some(24.0));
        assert_eq!(time_part(DatePartField::Minute, end), Some(0.0));
        assert_eq!(time_part(DatePartField::Second, end), Some(0.0));
        assert_eq!(time_part(DatePartField::Epoch, end), Some(86_400.0));
        assert_eq!(time_part(DatePartField::Hour, 0), Some(0.0));
        let last = crate::datetime::parse_time("23:59:59.999999").unwrap();
        assert_eq!(time_part(DatePartField::Hour, last), Some(23.0));
        assert_eq!(time_part(DatePartField::Minute, last), Some(59.0));
        assert!(time_part(DatePartField::Year, t).is_none());
        assert!(time_part(DatePartField::Dow, t).is_none());
        assert!(time_supports_field(DatePartField::Epoch));
        assert!(!time_supports_field(DatePartField::Month));
    }

    #[test]
    fn date_part_leap_day_all_fields() {
        let t = ts("2024-02-29 12:34:56.789012");
        assert_eq!(date_part(DatePartField::Year, t), 2024.0);
        assert_eq!(date_part(DatePartField::Month, t), 2.0);
        assert_eq!(date_part(DatePartField::Day, t), 29.0);
        assert_eq!(date_part(DatePartField::Hour, t), 12.0);
        assert_eq!(date_part(DatePartField::Minute, t), 34.0);
        assert!((date_part(DatePartField::Second, t) - 56.789012).abs() < 1e-9);
        assert!((date_part(DatePartField::Milliseconds, t) - 56_789.012).abs() < 1e-6);
        assert!((date_part(DatePartField::Microseconds, t) - 56_789_012.0).abs() < 1e-3);
        assert_eq!(date_part(DatePartField::Quarter, t), 1.0);
        assert_eq!(date_part(DatePartField::Decade, t), 202.0);
        assert_eq!(date_part(DatePartField::Century, t), 21.0);
        assert_eq!(date_part(DatePartField::Millennium, t), 3.0);
        // 2024-02-29 は木曜日（dow=4, isodow=4）。
        assert_eq!(date_part(DatePartField::Dow, t), 4.0);
        assert_eq!(date_part(DatePartField::IsoDow, t), 4.0);
        assert_eq!(date_part(DatePartField::Doy, t), 60.0);
    }

    /// 2021-01-01 は ISO 週 53・isoyear 2020 になる（年境界をまたぐ週の代表例）。
    #[test]
    fn iso_week_year_boundary() {
        let t = ts("2021-01-01 00:00:00");
        assert_eq!(date_part(DatePartField::IsoYear, t), 2020.0);
        assert_eq!(date_part(DatePartField::Week, t), 53.0);
    }

    /// 境界値: `0001-01-01 00:00:00` と `9999-12-31 23:59:59.999999`。
    #[test]
    fn boundary_timestamps() {
        let min = ts("0001-01-01 00:00:00");
        assert_eq!(date_part(DatePartField::Year, min), 1.0);
        let max = ts("9999-12-31 23:59:59.999999");
        assert_eq!(date_part(DatePartField::Year, max), 9999.0);
        assert_eq!(date_part(DatePartField::Month, max), 12.0);
        assert_eq!(date_part(DatePartField::Day, max), 31.0);
    }

    /// 2000 年 / 2001 年の century と millennium（PG は「2000 年は 20 世紀・
    /// 2001 年は 21 世紀」という桁境界の扱いをする）。
    #[test]
    fn century_and_millennium_boundary() {
        let y2000 = ts("2000-06-15 00:00:00");
        let y2001 = ts("2001-06-15 00:00:00");
        assert_eq!(date_part(DatePartField::Century, y2000), 20.0);
        assert_eq!(date_part(DatePartField::Century, y2001), 21.0);
        assert_eq!(date_part(DatePartField::Millennium, y2000), 2.0);
        assert_eq!(date_part(DatePartField::Millennium, y2001), 3.0);
    }

    #[test]
    fn date_trunc_week_quarter_decade_century_millennium_milliseconds() {
        // 2024-02-29（木曜）の ISO 週の月曜は 2024-02-26。
        let t = ts("2024-02-29 12:34:56.789012");
        let week_trunc = date_trunc(DateTruncUnit::Week, t).unwrap();
        assert_eq!(week_trunc, ts("2024-02-26 00:00:00"));
        let quarter_trunc = date_trunc(DateTruncUnit::Quarter, t).unwrap();
        assert_eq!(quarter_trunc, ts("2024-01-01 00:00:00"));
        let decade_trunc = date_trunc(DateTruncUnit::Decade, t).unwrap();
        assert_eq!(decade_trunc, ts("2020-01-01 00:00:00"));
        let century_trunc = date_trunc(DateTruncUnit::Century, t).unwrap();
        assert_eq!(century_trunc, ts("2001-01-01 00:00:00"));
        let millennium_trunc = date_trunc(DateTruncUnit::Millennium, t).unwrap();
        assert_eq!(millennium_trunc, ts("2001-01-01 00:00:00"));
        let ms_trunc = date_trunc(DateTruncUnit::Milliseconds, t).unwrap();
        assert_eq!(ms_trunc, ts("2024-02-29 12:34:56.789000"));
    }

    /// 1〜9 年の `decade` 切り捨ては西暦 0 年（存在しない）になり `22008`。
    #[test]
    fn date_trunc_decade_before_year_ten_overflows() {
        let t = ts("0005-06-15 00:00:00");
        let err = date_trunc(DateTruncUnit::Decade, t).unwrap_err();
        assert!(matches!(err, SqlSurfaceError::DatetimeFieldOverflow { .. }));
    }

    #[test]
    fn date_add_days_rejects_non_integer_and_out_of_range() {
        let base = crate::datetime::parse_date("2024-01-01").unwrap();
        assert!(matches!(
            date_add_days(base, 1.5).unwrap_err(),
            SqlSurfaceError::DatatypeMismatch { .. }
        ));
        // NaN・±Inf・範囲外かつ非整数は、非整数検査が先に働き型不一致になる。
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 3_000_000_000.5] {
            assert!(matches!(
                date_add_days(base, bad).unwrap_err(),
                SqlSurfaceError::DatatypeMismatch { .. }
            ));
        }
        assert!(matches!(
            date_add_days(base, 3_000_000_000.0).unwrap_err(),
            SqlSurfaceError::NumericOutOfRange { .. }
        ));
        let near_max = crate::datetime::parse_date("9999-12-31").unwrap();
        assert!(matches!(
            date_add_days(near_max, 1.0).unwrap_err(),
            SqlSurfaceError::DatetimeFieldOverflow { .. }
        ));
        assert_eq!(
            date_add_days(base, 30.0).unwrap(),
            crate::datetime::parse_date("2024-01-31").unwrap()
        );
    }

    /// codex 指摘対応（PR #1120）: `n == i32::MIN` の符号反転オーバーフローで
    /// 妥当な `n` を誤って「範囲外」（`NumericOutOfRange`）扱いしないことを
    /// 固定する（`date_sub_days` のドキュメンテーションコメント参照）。
    #[test]
    fn date_sub_days_handles_i32_min_operand_without_negation_overflow() {
        // `DATE_MIN_DAYS`（`0001-01-01`）を基準日にすると、`n = i32::MIN` の
        // 減算結果（`days - i32::MIN` = `days + 2147483648`）はちょうど `i32`
        // の範囲には収まるが `DATE` の受理範囲を大きく超えるため
        // `DatetimeFieldOverflow`（`22008`）になるべきで、`NumericOutOfRange`
        // （`22003`、オペランド自体が無効という意味）になってはならない
        // （旧実装は符号反転自体が `i32` をオーバーフローし `base` に関わらず
        // 常に `NumericOutOfRange` になっていた）。
        let base = crate::datetime::DATE_MIN_DAYS;
        let err = date_sub_days(base, f64::from(i32::MIN)).unwrap_err();
        assert!(matches!(err, SqlSurfaceError::DatetimeFieldOverflow { .. }));

        let normal_base = crate::datetime::parse_date("2024-01-01").unwrap();
        assert!(matches!(
            date_sub_days(normal_base, 1.5).unwrap_err(),
            SqlSurfaceError::DatatypeMismatch { .. }
        ));
        assert!(matches!(
            date_sub_days(normal_base, 3_000_000_000.0).unwrap_err(),
            SqlSurfaceError::NumericOutOfRange { .. }
        ));
        assert_eq!(
            date_sub_days(normal_base, 10.0).unwrap(),
            crate::datetime::parse_date("2023-12-22").unwrap()
        );
    }

    /// codex 指摘対応（PR #1120）: `n` 自体は `i32` 範囲内でも、`days + n` が
    /// `i32` の全域（`DATE` の受理範囲よりはるかに広い）を超える場合、拒否
    /// 理由は「オペランドが範囲外」（`22003`）ではなく「計算結果が `DATE` の
    /// 受理範囲外」（`22008`）であるべき（`docs/design/
    /// datetime-scalar-functions.md` の型規約が定める 2 分類と一致させる）。
    /// 旧実装は `i32::checked_add` の桁あふれを `22003` に写像していたため、
    /// この分類が食い違っていた。
    #[test]
    fn date_add_days_result_overflowing_i32_is_datetime_field_overflow_not_numeric_out_of_range() {
        let near_max = crate::datetime::DATE_MAX_DAYS;
        let err = date_add_days(near_max, f64::from(i32::MAX)).unwrap_err();
        assert!(matches!(err, SqlSurfaceError::DatetimeFieldOverflow { .. }));
    }
}
