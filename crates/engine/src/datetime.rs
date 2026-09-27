//! `DATE`／`TIMESTAMP` 列型（TABLE-13・TASK-197。Issue #884）のリテラル解析・
//! 内部表現変換・投影テキスト整形を集約する単一情報源。
//!
//! 責務境界: `catalog`（列型宣言）・`row_codec`（行バイト表現の encode/decode）・
//! `sql::parser`（INSERT/UPDATE/UPSERT リテラル束縛）・`sql::exec`／`result_encoder`
//! （投影テキスト整形）のいずれもここへ委譲し、暦計算・範囲定数・リテラル文法を
//! 複数箇所へ分散させない。値そのものは内部表現（`DATE` は 1970-01-01 起点の
//! 日数 `i32`、`TIMESTAMP` はマイクロ秒精度の同起点 `i64`。いずれもタイムゾーンを
//! 持たない naive 値）で扱い、リテラル文字列と行バイト列の双方から到達する変換の
//! 唯一の実装とする（`docs/design/datetime-column.md` に設計判断を記録。spec 本文は
//! 転記せず TABLE-13・TASK-197 のポインタのみ）。

use std::fmt::Write as _;

/// 先発グレゴリオ暦（proleptic Gregorian）で表現できる `DATE` の最小値
/// （`0001-01-01`）を 1970-01-01 起点の日数で表した値。
pub const DATE_MIN_DAYS: i32 = -719_162;

/// `DATE` の最大値（`9999-12-31`）を 1970-01-01 起点の日数で表した値。
pub const DATE_MAX_DAYS: i32 = 2_932_896;

/// 1 日あたりのマイクロ秒数。`TIMESTAMP` の内部表現（マイクロ秒精度）と
/// `DATE`（日精度）の変換で共有する。`pub(crate)`: `sql::datetime_fn`
/// （TASK-210・SQL-26。Issue #920）が `date_part`／`date_trunc` の
/// マイクロ秒⇔日変換で同じ定数を使う。
pub(crate) const MICROS_PER_DAY: i64 = 86_400_000_000;

/// `TIMESTAMP` の最小値（`0001-01-01 00:00:00.000000`）をマイクロ秒で表した値。
pub const TIMESTAMP_MIN_MICROS: i64 = DATE_MIN_DAYS as i64 * MICROS_PER_DAY;

/// `TIMESTAMP` の最大値（`9999-12-31 23:59:59.999999`）をマイクロ秒で表した値。
pub const TIMESTAMP_MAX_MICROS: i64 = DATE_MAX_DAYS as i64 * MICROS_PER_DAY + (MICROS_PER_DAY - 1);

/// リテラル文字列の受理上限バイト長。`TIMESTAMP` の最長形（`999999-12-31
/// 23:59:59.999999` 相当）に十分な余裕を持たせつつ、パース前の早期拒否で
/// untrusted 入力に対する無駄な走査を避ける（`.claude/rules/coding-rust.md`
/// 「untrusted 入力の扱い」）。
pub const MAX_DATETIME_LITERAL_LEN: usize = 40;

/// `DATE`／`TIMESTAMP` リテラルの解析エラー。呼び出し元（`sql::parser`）が
/// `Format` は `22000`（構文違反）、`Overflow` は `22008`
/// （`DATETIME_FIELD_OVERFLOW`。範囲外・暦上不正）へ写像する（D-1。
/// `docs/design/datetime-column.md` 参照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateTimeLiteralError {
    /// 閉じた文法（本モジュール doc 参照）に一致しない（区切り文字違い・桁数
    /// 不足・TZ 接尾辞・特殊語・前後空白・非 ASCII 数字・長さ超過等）。
    Format(String),
    /// 文法上は解析できたが値が受理範囲外、または暦上不正
    /// （月 13・2 月 30 日・非閏年の 2/29・時 24・分 60・秒 60・年 0000・
    /// 年 10000 以上等）。
    Overflow(String),
}

impl std::fmt::Display for DateTimeLiteralError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DateTimeLiteralError::Format(detail) => write!(f, "{detail}"),
            DateTimeLiteralError::Overflow(detail) => write!(f, "{detail}"),
        }
    }
}

/// 先発グレゴリオ暦での閏年判定（西暦 4 年で割り切れ、100 年で割り切れない
/// か、400 年で割り切れる）。
fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// 月ごとの日数（閏年の 2 月を考慮）。`month` は呼び出し前に 1..=12 の範囲
///検証済みであることを前提とする。
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 31, // 呼び出し前に検証済みのため到達しない（fail-closed の保険値）。
    }
}

/// Howard Hinnant の `days_from_civil` アルゴリズム（先発グレゴリオ暦。全年代で
/// 妥当）。1970-01-01 起点の日数を返す。`year`・`month`・`day` は呼び出し前に
/// 暦上妥当な組であることを検証済みであることを前提とする（本関数自体は暦
/// 妥当性を検証しない純粋な変換）。
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 } as i64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// `days_from_civil` の逆変換。1970-01-01 起点の日数から (year, month, day) を
/// 復元する（先発グレゴリオ暦。全整数域で妥当）。
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

/// `days`（1970-01-01 起点）が `DATE` の受理範囲内かどうか。行コーデックの
/// decode 側（格納済み・untrusted なバイト列）が範囲外値を fail-closed に
/// 拒否するために使う（TABLE-7）。
pub fn validate_date_days(days: i32) -> bool {
    (DATE_MIN_DAYS..=DATE_MAX_DAYS).contains(&days)
}

/// `micros`（1970-01-01 00:00:00 起点のマイクロ秒）が `TIMESTAMP` の受理範囲内
/// かどうか。[`validate_date_days`] と同じ理由で decode 側から使う。
pub fn validate_timestamp_micros(micros: i64) -> bool {
    (TIMESTAMP_MIN_MICROS..=TIMESTAMP_MAX_MICROS).contains(&micros)
}

/// バイト列中の `start..start+len` が ASCII 数字のみで構成されるかを検証し、
/// `u32` として返す。添字直接アクセスをせず `get()` で明示的に処理する
/// （`.claude/rules/coding-rust.md`）。
fn parse_ascii_digits(bytes: &[u8], start: usize, len: usize) -> Option<(u32, usize)> {
    let end = start.checked_add(len)?;
    let slice = bytes.get(start..end)?;
    if slice.iter().any(|b| !b.is_ascii_digit()) {
        return None;
    }
    let mut value: u32 = 0;
    for b in slice {
        value = value.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some((value, end))
}

/// `DATE` 部（`Y{4,6}-MM-DD`）を解析する。年は 4〜6 桁の ASCII 数字、月・日は
/// 各 2 桁固定。戻り値は (year, month, day, 消費済みバイト数)。
fn parse_date_part(bytes: &[u8]) -> Result<(i64, u32, u32, usize), DateTimeLiteralError> {
    // 年の桁数（4〜6 桁）を長い方から順に試す。`-` の直前までを年として
    // 受理できる最長の桁数を採用することで、`10000-01-01`（5 桁年）のような
    // 範囲外年も「文法違反」ではなく「範囲外」として区別できる。
    for year_len in [6usize, 5, 4] {
        let Some((year, after_year)) = parse_ascii_digits(bytes, 0, year_len) else {
            continue;
        };
        if bytes.get(after_year) != Some(&b'-') {
            continue;
        }
        let month_start = after_year + 1;
        let Some((month, after_month)) = parse_ascii_digits(bytes, month_start, 2) else {
            continue;
        };
        if bytes.get(after_month) != Some(&b'-') {
            continue;
        }
        let day_start = after_month + 1;
        let Some((day, after_day)) = parse_ascii_digits(bytes, day_start, 2) else {
            continue;
        };
        return Ok((i64::from(year), month, day, after_day));
    }
    Err(DateTimeLiteralError::Format(
        "expected DATE literal in the form YYYY-MM-DD".to_string(),
    ))
}

/// 暦上の妥当性（年範囲・月範囲・日範囲）を検証し、1970-01-01 起点の日数へ
/// 変換する。
fn validate_and_convert_date(year: i64, month: u32, day: u32) -> Result<i32, DateTimeLiteralError> {
    if !(1..=9999).contains(&year) {
        return Err(DateTimeLiteralError::Overflow(format!(
            "year {year} is out of range 0001..=9999"
        )));
    }
    if !(1..=12).contains(&month) {
        return Err(DateTimeLiteralError::Overflow(format!(
            "month {month} is out of range 01..=12"
        )));
    }
    let max_day = days_in_month(year, month);
    if day < 1 || day > max_day {
        return Err(DateTimeLiteralError::Overflow(format!(
            "day {day} is out of range for {year:04}-{month:02}"
        )));
    }
    let days = days_from_civil(year, month, day);
    let days_i32 = i32::try_from(days).map_err(|_| {
        DateTimeLiteralError::Overflow("date value overflows internal representation".to_string())
    })?;
    if !validate_date_days(days_i32) {
        return Err(DateTimeLiteralError::Overflow(
            "date value is out of the representable range".to_string(),
        ));
    }
    Ok(days_i32)
}

/// `DATE` リテラル（`YYYY-MM-DD`）を解析し、1970-01-01 起点の日数（`i32`）へ
/// 変換する。閉じた文法・範囲外の判定は本モジュール doc（D-1・D-5）参照。
pub fn parse_date(s: &str) -> Result<i32, DateTimeLiteralError> {
    if s.len() > MAX_DATETIME_LITERAL_LEN {
        return Err(DateTimeLiteralError::Format(
            "DATE literal exceeds the maximum length".to_string(),
        ));
    }
    if !s.is_ascii() {
        return Err(DateTimeLiteralError::Format(
            "DATE literal must be ASCII".to_string(),
        ));
    }
    let bytes = s.as_bytes();
    let (year, month, day, consumed) = parse_date_part(bytes)?;
    if consumed != bytes.len() {
        return Err(DateTimeLiteralError::Format(
            "DATE literal has trailing characters".to_string(),
        ));
    }
    validate_and_convert_date(year, month, day)
}

/// `TIMESTAMP` リテラル（`<DATE 部><区切り>HH:MM:SS[.f{1,6}]`）を解析し、
/// 1970-01-01 00:00:00 起点のマイクロ秒（`i64`）へ変換する。区切りは半角空白
/// または `T` の 1 文字のみを受理する（D-4・D-5）。
pub fn parse_timestamp(s: &str) -> Result<i64, DateTimeLiteralError> {
    if s.len() > MAX_DATETIME_LITERAL_LEN {
        return Err(DateTimeLiteralError::Format(
            "TIMESTAMP literal exceeds the maximum length".to_string(),
        ));
    }
    if !s.is_ascii() {
        return Err(DateTimeLiteralError::Format(
            "TIMESTAMP literal must be ASCII".to_string(),
        ));
    }
    let bytes = s.as_bytes();
    let (year, month, day, after_date) = parse_date_part(bytes)?;
    let sep = bytes.get(after_date).ok_or_else(|| {
        DateTimeLiteralError::Format(
            "TIMESTAMP literal must include a time-of-day part".to_string(),
        )
    })?;
    if *sep != b' ' && *sep != b'T' {
        return Err(DateTimeLiteralError::Format(
            "TIMESTAMP literal date/time separator must be a single space or 'T'".to_string(),
        ));
    }
    let time_start = after_date + 1;
    let (hour, after_hour) = parse_ascii_digits(bytes, time_start, 2).ok_or_else(|| {
        DateTimeLiteralError::Format("expected HH in the time-of-day part".to_string())
    })?;
    if bytes.get(after_hour) != Some(&b':') {
        return Err(DateTimeLiteralError::Format(
            "expected ':' after hour".to_string(),
        ));
    }
    let min_start = after_hour + 1;
    let (minute, after_min) = parse_ascii_digits(bytes, min_start, 2).ok_or_else(|| {
        DateTimeLiteralError::Format("expected MM in the time-of-day part".to_string())
    })?;
    if bytes.get(after_min) != Some(&b':') {
        return Err(DateTimeLiteralError::Format(
            "expected ':' after minute".to_string(),
        ));
    }
    let sec_start = after_min + 1;
    let (second, after_sec) = parse_ascii_digits(bytes, sec_start, 2).ok_or_else(|| {
        DateTimeLiteralError::Format("expected SS in the time-of-day part".to_string())
    })?;

    let mut frac_micros: i64 = 0;
    let mut cursor = after_sec;
    if bytes.get(cursor) == Some(&b'.') {
        let frac_start = cursor + 1;
        // 小数秒は 1〜6 桁のみ受理する。7 桁以上は丸めず文法違反として拒否する
        // （D-5）。何桁続くかを先に数えてから範囲を確定する。
        let mut frac_len = 0usize;
        while bytes
            .get(frac_start + frac_len)
            .is_some_and(u8::is_ascii_digit)
        {
            frac_len += 1;
            if frac_len > 6 {
                return Err(DateTimeLiteralError::Format(
                    "fractional seconds must be 1 to 6 digits".to_string(),
                ));
            }
        }
        if frac_len == 0 {
            return Err(DateTimeLiteralError::Format(
                "expected at least one digit after '.'".to_string(),
            ));
        }
        let (frac_value, frac_end) =
            parse_ascii_digits(bytes, frac_start, frac_len).ok_or_else(|| {
                DateTimeLiteralError::Format("malformed fractional seconds".to_string())
            })?;
        // 桁数に応じて 10^(6-frac_len) を掛け、常にマイクロ秒単位へ揃える
        // （例: `.5` → 500000 マイクロ秒）。
        let scale: i64 = 10i64.pow(6 - u32::try_from(frac_len).unwrap_or(6));
        frac_micros = i64::from(frac_value) * scale;
        cursor = frac_end;
    }

    if cursor != bytes.len() {
        return Err(DateTimeLiteralError::Format(
            "TIMESTAMP literal has trailing characters".to_string(),
        ));
    }

    if hour > 23 {
        return Err(DateTimeLiteralError::Overflow(format!(
            "hour {hour} is out of range 00..=23"
        )));
    }
    if minute > 59 {
        return Err(DateTimeLiteralError::Overflow(format!(
            "minute {minute} is out of range 00..=59"
        )));
    }
    if second > 59 {
        return Err(DateTimeLiteralError::Overflow(format!(
            "second {second} is out of range 00..=59"
        )));
    }

    let date_days = validate_and_convert_date(year, month, day)?;
    let day_micros = i64::from(hour) * 3_600_000_000
        + i64::from(minute) * 60_000_000
        + i64::from(second) * 1_000_000
        + frac_micros;
    let total_micros = i64::from(date_days)
        .checked_mul(MICROS_PER_DAY)
        .and_then(|d| d.checked_add(day_micros))
        .ok_or_else(|| {
            DateTimeLiteralError::Overflow(
                "timestamp value overflows internal representation".to_string(),
            )
        })?;
    if !validate_timestamp_micros(total_micros) {
        return Err(DateTimeLiteralError::Overflow(
            "timestamp value is out of the representable range".to_string(),
        ));
    }
    Ok(total_micros)
}

/// `DATE` の内部表現（1970-01-01 起点の日数）を PostgreSQL 互換の既定出力
/// 形式（`YYYY-MM-DD`。年は常に 4 桁ゼロ埋め）へ整形する。`days` は
/// [`validate_date_days`] 済みであることを前提とする（範囲外値を渡した場合の
/// 出力内容は未規定だが、暦計算自体は panic しない）。
pub fn format_date(days: i32) -> String {
    let (year, month, day) = civil_from_days(i64::from(days));
    let mut out = String::with_capacity(10);
    let _ = write!(out, "{year:04}-{month:02}-{day:02}");
    out
}

/// `TIMESTAMP` の内部表現（マイクロ秒）を PostgreSQL 互換の既定出力形式
/// （`YYYY-MM-DD HH:MM:SS[.ffffff]`。小数秒が 0 なら省略、末尾の 0 は削る）
/// へ整形する。
pub fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let days_i32 = i32::try_from(days).unwrap_or(if days < 0 { i32::MIN } else { i32::MAX });
    let date_part = format_date(days_i32);
    let hour = day_micros / 3_600_000_000;
    let rem = day_micros % 3_600_000_000;
    let minute = rem / 60_000_000;
    let rem = rem % 60_000_000;
    let second = rem / 1_000_000;
    let frac = rem % 1_000_000;
    let mut out = String::with_capacity(26);
    let _ = write!(out, "{date_part} {hour:02}:{minute:02}:{second:02}");
    if frac != 0 {
        let mut frac_str = format!("{frac:06}");
        while frac_str.ends_with('0') {
            frac_str.pop();
        }
        let _ = write!(out, ".{frac_str}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_neighbors() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn range_bounds_match_constants() {
        assert_eq!(days_from_civil(1, 1, 1), i64::from(DATE_MIN_DAYS));
        assert_eq!(days_from_civil(9999, 12, 31), i64::from(DATE_MAX_DAYS));
        assert_eq!(civil_from_days(i64::from(DATE_MIN_DAYS)), (1, 1, 1));
        assert_eq!(civil_from_days(i64::from(DATE_MAX_DAYS)), (9999, 12, 31));
    }

    #[test]
    fn parse_date_roundtrip_boundaries() {
        assert_eq!(parse_date("0001-01-01"), Ok(DATE_MIN_DAYS));
        assert_eq!(parse_date("9999-12-31"), Ok(DATE_MAX_DAYS));
        assert_eq!(parse_date("1970-01-01"), Ok(0));
        assert_eq!(parse_date("1969-12-31"), Ok(-1));
        assert_eq!(format_date(DATE_MIN_DAYS), "0001-01-01");
        assert_eq!(format_date(DATE_MAX_DAYS), "9999-12-31");
    }

    #[test]
    fn parse_date_leap_years() {
        assert!(parse_date("2000-02-29").is_ok());
        assert_eq!(
            parse_date("1900-02-29"),
            Err(DateTimeLiteralError::Overflow(
                "day 29 is out of range for 1900-02".to_string()
            ))
        );
        assert_eq!(
            parse_date("2100-02-29"),
            Err(DateTimeLiteralError::Overflow(
                "day 29 is out of range for 2100-02".to_string()
            ))
        );
        assert!(parse_date("2024-02-29").is_ok());
        assert!(matches!(
            parse_date("2023-02-29"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
    }

    #[test]
    fn parse_date_rejects_calendar_overflow() {
        assert!(matches!(
            parse_date("2024-13-01"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
        assert!(matches!(
            parse_date("2024-00-01"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
        assert!(matches!(
            parse_date("2024-01-32"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
        assert!(matches!(
            parse_date("2024-01-00"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
        assert!(matches!(
            parse_date("0000-12-31"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
        assert!(matches!(
            parse_date("10000-01-01"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
    }

    #[test]
    fn parse_date_rejects_format_violations() {
        assert!(matches!(
            parse_date("2024/01/01"),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_date("2024-1-01"),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_date(" 2024-01-01"),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_date("2024-01-01 "),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_date("2024-01-01Z"),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_date("１２３４-01-01"),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_date("infinity"),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_date(""),
            Err(DateTimeLiteralError::Format(_))
        ));
    }

    #[test]
    fn parse_timestamp_roundtrip() {
        assert_eq!(parse_timestamp("1970-01-01 00:00:00"), Ok(0));
        assert_eq!(
            parse_timestamp("0001-01-01 00:00:00"),
            Ok(TIMESTAMP_MIN_MICROS)
        );
        assert_eq!(
            parse_timestamp("9999-12-31 23:59:59.999999"),
            Ok(TIMESTAMP_MAX_MICROS)
        );
        assert_eq!(
            format_timestamp(TIMESTAMP_MIN_MICROS),
            "0001-01-01 00:00:00"
        );
        assert_eq!(
            format_timestamp(TIMESTAMP_MAX_MICROS),
            "9999-12-31 23:59:59.999999"
        );
    }

    #[test]
    fn parse_timestamp_accepts_t_separator_and_fraction_widths() {
        assert_eq!(
            parse_timestamp("2024-01-02T03:04:05"),
            parse_timestamp("2024-01-02 03:04:05")
        );
        assert_eq!(
            parse_timestamp("2024-01-02 03:04:05.5"),
            Ok({
                let base = parse_timestamp("2024-01-02 03:04:05").unwrap();
                base + 500_000
            })
        );
        assert_eq!(
            format_timestamp(parse_timestamp("2024-01-02 03:04:05.100000").unwrap()),
            "2024-01-02 03:04:05.1"
        );
        assert_eq!(
            format_timestamp(parse_timestamp("2024-01-02 03:04:05.000001").unwrap()),
            "2024-01-02 03:04:05.000001"
        );
    }

    #[test]
    fn parse_timestamp_rejects_seven_fraction_digits() {
        assert!(matches!(
            parse_timestamp("2024-01-02 03:04:05.1234567"),
            Err(DateTimeLiteralError::Format(_))
        ));
    }

    #[test]
    fn parse_timestamp_rejects_timezone_suffix() {
        assert!(matches!(
            parse_timestamp("2024-01-02 03:04:05Z"),
            Err(DateTimeLiteralError::Format(_))
        ));
        assert!(matches!(
            parse_timestamp("2024-01-02 03:04:05+09:00"),
            Err(DateTimeLiteralError::Format(_))
        ));
    }

    #[test]
    fn parse_timestamp_rejects_date_only() {
        assert!(matches!(
            parse_timestamp("2024-01-02"),
            Err(DateTimeLiteralError::Format(_))
        ));
    }

    #[test]
    fn parse_timestamp_rejects_field_overflow() {
        assert!(matches!(
            parse_timestamp("2024-01-02 24:00:00"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
        assert!(matches!(
            parse_timestamp("2024-01-02 00:60:00"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
        assert!(matches!(
            parse_timestamp("2024-01-02 00:00:60"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
    }

    #[test]
    fn date_and_timestamp_range_sampling_roundtrip() {
        // 全域を細かくサンプリングして parse→format→parse の往復を検証する。
        let mut days = DATE_MIN_DAYS;
        let mut samples = 0;
        while days <= DATE_MAX_DAYS && samples < 2000 {
            let text = format_date(days);
            let reparsed = parse_date(&text).expect("reparse must succeed");
            assert_eq!(reparsed, days, "roundtrip mismatch for {text}");
            days = days.saturating_add(3_650_003); // 素数に近い歩幅で広く分散させる
            samples += 1;
        }
    }

    #[test]
    fn validate_range_helpers() {
        assert!(validate_date_days(0));
        assert!(validate_date_days(DATE_MIN_DAYS));
        assert!(validate_date_days(DATE_MAX_DAYS));
        assert!(!validate_date_days(DATE_MIN_DAYS - 1));
        assert!(!validate_date_days(DATE_MAX_DAYS + 1));
        assert!(validate_timestamp_micros(0));
        assert!(validate_timestamp_micros(TIMESTAMP_MIN_MICROS));
        assert!(validate_timestamp_micros(TIMESTAMP_MAX_MICROS));
        assert!(!validate_timestamp_micros(TIMESTAMP_MIN_MICROS - 1));
        assert!(!validate_timestamp_micros(TIMESTAMP_MAX_MICROS + 1));
    }
}
