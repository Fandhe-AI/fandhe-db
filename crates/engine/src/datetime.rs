//! `DATE`／`TIMESTAMP`／`TIME`／`TIMESTAMPTZ` 列型（TABLE-13・TASK-197。Issue #884。TIME は
//! TABLE-32・Issue #1569、TIMESTAMPTZ は TABLE-33・Issue #1574）のリテラル解析・
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

/// `TIME` の最小値（`00:00:00`）を 0 時起点のマイクロ秒で表した値（Issue #1569）。
pub const TIME_MIN_MICROS: i64 = 0;

/// `TIME` の最大値（`24:00:00`。両端を含む）を 0 時起点のマイクロ秒で表した値。
/// `24:00:00` を受理する（TABLE-32）ため上端は `MICROS_PER_DAY` そのもの。
pub const TIME_MAX_MICROS: i64 = MICROS_PER_DAY;

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

/// `micros`（0 時起点のマイクロ秒）が `TIME` の受理範囲（`00:00:00`〜`24:00:00`。
/// 両端を含む）内かどうか。decode 側（`row_codec`）でも壊れたバイト列を
/// 有効値として表に出さないよう再検証に使う（Issue #1569）。
pub fn validate_time_micros(micros: i64) -> bool {
    (TIME_MIN_MICROS..=TIME_MAX_MICROS).contains(&micros)
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

/// `TIME` リテラル（`HH:MM[:SS[.f+]]`）を解析し、0 時起点のマイクロ秒（`i64`）へ
/// 変換する（TABLE-32・Issue #1569）。HH・MM・SS は 2 桁の ASCII 数字固定。
/// 端数は 7 桁目で丸め（0.5 は 0 から遠い側）、8 桁目以降は数字確認のみで捨てる。
/// 丸めた後で範囲（`00:00:00`〜`24:00:00`）を判定する。`sql::parser` 等が
/// `Format` を `22007`、`Overflow` を `22008` へ写像する。
pub fn parse_time(s: &str) -> Result<i64, DateTimeLiteralError> {
    if s.len() > MAX_DATETIME_LITERAL_LEN {
        return Err(DateTimeLiteralError::Format(
            "TIME literal exceeds the maximum length".to_string(),
        ));
    }
    if !s.is_ascii() {
        return Err(DateTimeLiteralError::Format(
            "TIME literal must be ASCII".to_string(),
        ));
    }
    let bytes = s.as_bytes();
    let (hour, after_hour) = parse_ascii_digits(bytes, 0, 2)
        .ok_or_else(|| DateTimeLiteralError::Format("expected HH in TIME literal".to_string()))?;
    if bytes.get(after_hour) != Some(&b':') {
        return Err(DateTimeLiteralError::Format(
            "expected ':' after hour".to_string(),
        ));
    }
    let (minute, after_min) = parse_ascii_digits(bytes, after_hour + 1, 2)
        .ok_or_else(|| DateTimeLiteralError::Format("expected MM in TIME literal".to_string()))?;
    let mut second: u32 = 0;
    let mut frac_micros: i64 = 0;
    let mut cursor = after_min;
    if bytes.get(cursor) == Some(&b':') {
        let (sec, after_sec) = parse_ascii_digits(bytes, cursor + 1, 2).ok_or_else(|| {
            DateTimeLiteralError::Format("expected SS in TIME literal".to_string())
        })?;
        second = sec;
        cursor = after_sec;
        if bytes.get(cursor) == Some(&b'.') {
            let frac_start = cursor + 1;
            let mut frac_len = 0usize;
            while bytes
                .get(frac_start + frac_len)
                .is_some_and(u8::is_ascii_digit)
            {
                frac_len += 1;
            }
            if frac_len == 0 {
                return Err(DateTimeLiteralError::Format(
                    "expected at least one digit after '.'".to_string(),
                ));
            }
            // 先頭 6 桁をマイクロ秒、7 桁目を丸め桁として使う。8 桁目以降は
            // 値に影響させない（累積オーバーフロー防止）。
            let mut micros: i64 = 0;
            for i in 0..6usize {
                let digit = bytes
                    .get(frac_start + i)
                    .filter(|b| b.is_ascii_digit())
                    .map_or(0, |b| i64::from(b - b'0'));
                micros = micros * 10 + digit;
            }
            let round_digit = if frac_len >= 7 {
                bytes.get(frac_start + 6).map_or(0, |b| b - b'0')
            } else {
                0
            };
            if round_digit >= 5 {
                micros += 1;
            }
            frac_micros = micros;
            cursor = frac_start + frac_len;
        }
    }
    if cursor != bytes.len() {
        return Err(DateTimeLiteralError::Format(
            "TIME literal has trailing characters".to_string(),
        ));
    }
    if hour > 24 {
        return Err(DateTimeLiteralError::Overflow(format!(
            "hour {hour} is out of range 00..=24"
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
    let total = i64::from(hour) * 3_600_000_000
        + i64::from(minute) * 60_000_000
        + i64::from(second) * 1_000_000
        + frac_micros;
    if !validate_time_micros(total) {
        return Err(DateTimeLiteralError::Overflow(
            "time value is out of the range 00:00:00..=24:00:00".to_string(),
        ));
    }
    Ok(total)
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
    let mut out = String::with_capacity(26);
    out.push_str(&date_part);
    out.push(' ');
    write_time_of_day(&mut out, day_micros);
    out
}

/// 0 時起点のマイクロ秒を `HH:MM:SS[.ffffff]`（端数 0 なら省略・末尾 0 削除）で
/// `out` へ追記する。`format_timestamp`／`format_time` の共通実装。
fn write_time_of_day(out: &mut String, day_micros: i64) {
    let hour = day_micros / 3_600_000_000;
    let rem = day_micros % 3_600_000_000;
    let minute = rem / 60_000_000;
    let rem = rem % 60_000_000;
    let second = rem / 1_000_000;
    let frac = rem % 1_000_000;
    let _ = write!(out, "{hour:02}:{minute:02}:{second:02}");
    if frac != 0 {
        let mut frac_str = format!("{frac:06}");
        while frac_str.ends_with('0') {
            frac_str.pop();
        }
        let _ = write!(out, ".{frac_str}");
    }
}

/// `TIME` の内部表現（0 時起点のマイクロ秒）を `HH:MM:SS[.ffffff]` へ整形する。
/// `MICROS_PER_DAY` は `24:00:00` と出す（Issue #1569）。
pub fn format_time(micros: i64) -> String {
    let mut out = String::with_capacity(15);
    write_time_of_day(&mut out, micros.clamp(TIME_MIN_MICROS, TIME_MAX_MICROS));
    out
}

// ---------------------------------------------------------------------------
// TIMESTAMPTZ（TABLE-33・Issue #1574）
// ---------------------------------------------------------------------------

/// `TIMESTAMPTZ` の最小値（UTC の `0001-01-01 00:00:00Z`）をマイクロ秒で表した値。
/// 内部表現は常に UTC の 1970-01-01 起点マイクロ秒で、範囲は `TIMESTAMP` と同一。
pub const TIMESTAMPTZ_MIN_MICROS: i64 = TIMESTAMP_MIN_MICROS;

/// `TIMESTAMPTZ` の最大値（UTC の `9999-12-31 23:59:59.999999Z`）をマイクロ秒で表した値。
pub const TIMESTAMPTZ_MAX_MICROS: i64 = TIMESTAMP_MAX_MICROS;

/// タイムゾーン名の受理上限バイト長。bundled tzdb の最長名より十分長く、
/// untrusted 入力の早期拒否に使う。
pub(crate) const MAX_TZ_NAME_LEN: usize = 64;

/// `TIMESTAMPTZ` リテラルの受理上限バイト長（`TIMESTAMP` の最長形 ＋ 空白 1 ＋ TZ 名）。
pub const MAX_TIMESTAMPTZ_LITERAL_LEN: usize = MAX_DATETIME_LITERAL_LEN + 1 + MAX_TZ_NAME_LEN;

/// `TIMESTAMPTZ` の UTC マイクロ秒が受理範囲内かどうか。行コーデックの decode 側
/// （壊れたバイト列の再検証）と INSERT 束縛の双方から使う（fail-closed）。
pub fn validate_timestamptz_micros(micros: i64) -> bool {
    (TIMESTAMPTZ_MIN_MICROS..=TIMESTAMPTZ_MAX_MICROS).contains(&micros)
}

/// セッションの TimeZone を表す不透明型。jiff の型を engine の公開 API へ漏らさず、
/// wire-server が jiff へ依存しないための境界
/// （`docs/design/timestamptz-tz-data-adoption.md`）。中身は bundled tzdb
/// （`TimeZoneDatabase::bundled()`）から解決したものに限り、実行時に tzdata ファイル・
/// `TZ`・`TZDIR` を読む経路は持たない。セッション値の保持と `SET TIME ZONE` は #1575 が
/// 担当し、本 Issue の呼び出し元はすべて [`SessionTimeZone::utc`] を渡す。
#[derive(Debug, Clone)]
pub struct SessionTimeZone(jiff::tz::TimeZone);

impl SessionTimeZone {
    /// UTC。TimeZone 未設定時の既定（#1575 でセッション値へ差し替える）。
    pub fn utc() -> Self {
        SessionTimeZone(jiff::tz::TimeZone::UTC)
    }

    /// IANA 名を bundled tzdb から解決する。長さ・許可文字を検査し、`Etc/Unknown` 等の
    /// 未知ゾーンは明示的に拒否する（fail-closed）。呼び出し元（リテラル解析・#1575 の
    /// `SET TIME ZONE`）が未知名を `22023` へ写像する。
    pub(crate) fn resolve_named(name: &str) -> Option<Self> {
        if name.is_empty() || name.len() > MAX_TZ_NAME_LEN {
            return None;
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'+' | b'-'))
        {
            return None;
        }
        let tz = jiff::tz::TimeZoneDatabase::bundled().get(name).ok()?;
        if tz.is_unknown() {
            return None;
        }
        Some(SessionTimeZone(tz))
    }

    /// UTC 時刻（マイクロ秒）における UTC からの offset（秒）。jiff の `Timestamp` 範囲外は
    /// 端へ丸める（範囲端は 12 月末で DST 規則が変わらないため offset は同じ）。
    fn offset_seconds_at(&self, utc_micros: i64) -> i32 {
        let ts = jiff::Timestamp::from_microsecond(utc_micros).unwrap_or(if utc_micros < 0 {
            jiff::Timestamp::MIN
        } else {
            jiff::Timestamp::MAX
        });
        self.0.to_offset(ts).seconds()
    }

    /// TZ の無い入力（naive）をこの TimeZone の壁時計として解釈した UTC マイクロ秒を返す。
    /// DST の gap は直前の offset、fold は後の offset で解決する（PG の既定解釈。
    /// 暫定: #1576 が PG 17 の実測と照合して最終確定する）。
    fn local_to_utc_micros(&self, local_micros: i64) -> Option<i64> {
        use jiff::tz::AmbiguousOffset;
        let days = local_micros.div_euclid(MICROS_PER_DAY);
        let day_micros = local_micros.rem_euclid(MICROS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        let secs = day_micros / 1_000_000;
        let sub_nanos = (day_micros % 1_000_000) * 1_000;
        let dt = jiff::civil::DateTime::new(
            i16::try_from(year).ok()?,
            i8::try_from(month).ok()?,
            i8::try_from(day).ok()?,
            i8::try_from(secs / 3600).ok()?,
            i8::try_from((secs % 3600) / 60).ok()?,
            i8::try_from(secs % 60).ok()?,
            i32::try_from(sub_nanos).ok()?,
        )
        .ok()?;
        let offset = match self.0.to_ambiguous_timestamp(dt).offset() {
            AmbiguousOffset::Unambiguous { offset } => offset,
            AmbiguousOffset::Gap { before, .. } => before,
            AmbiguousOffset::Fold { after, .. } => after,
        };
        local_micros.checked_sub(i64::from(offset.seconds()).checked_mul(1_000_000)?)
    }
}

/// `TIMESTAMPTZ` リテラルの解析エラー。`Literal` は `22007`（書式）／`22008`（範囲外）、
/// `UnknownTimeZone` は `22023`（`time zone not recognized`）へ呼び出し元が写像する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimestampTzLiteralError {
    /// 書式違反または範囲外（[`DateTimeLiteralError`] と同じ分類）。
    Literal(DateTimeLiteralError),
    /// 解決できない（bundled tzdb に無い・許可文字外・長さ超過）TZ 名。
    UnknownTimeZone,
}

impl From<DateTimeLiteralError> for TimestampTzLiteralError {
    fn from(e: DateTimeLiteralError) -> Self {
        TimestampTzLiteralError::Literal(e)
    }
}

/// `HH`・`HHMM`・`HH:MM`・`HH:MM:SS`（先頭の符号は呼び出し側で消費済み）の数字部を
/// offset 秒へ変換する。時は 0〜15、分・秒は 0〜59（超過は `Overflow`）。
fn parse_numeric_offset(sign: i64, digits: &[u8]) -> Result<i64, DateTimeLiteralError> {
    let fmt_err = || {
        DateTimeLiteralError::Format(
            "UTC offset must be in the form +HH, +HHMM, +HH:MM or +HH:MM:SS".to_string(),
        )
    };
    let pair =
        |at: usize| -> Option<i64> { parse_ascii_digits(digits, at, 2).map(|(v, _)| i64::from(v)) };
    let colon = |at: usize| digits.get(at) == Some(&b':');
    let (h, m, s) = match digits.len() {
        2 => (pair(0), Some(0), Some(0)),
        4 => (pair(0), pair(2), Some(0)),
        5 if colon(2) => (pair(0), pair(3), Some(0)),
        8 if colon(2) && colon(5) => (pair(0), pair(3), pair(6)),
        _ => return Err(fmt_err()),
    };
    let (Some(h), Some(m), Some(s)) = (h, m, s) else {
        return Err(fmt_err());
    };
    // 範囲外の SQLSTATE は Issue が定める写像に従い 22008（PG は 22009。docs に乖離として記録）。
    if h > 15 || m > 59 || s > 59 {
        return Err(DateTimeLiteralError::Overflow(
            "UTC offset is out of range".to_string(),
        ));
    }
    Ok(sign * (h * 3600 + m * 60 + s))
}

/// 値が `TIMESTAMPTZ` の表現範囲を超えたときの共通エラー。
fn timestamptz_out_of_range() -> DateTimeLiteralError {
    DateTimeLiteralError::Overflow(
        "timestamptz value is out of the representable range".to_string(),
    )
}

/// `TIMESTAMPTZ` リテラル（`<TIMESTAMP 文法>[空白 0〜1 個]<ゾーン>`）を解析し、
/// UTC の 1970-01-01 起点マイクロ秒へ正規化する（TABLE-33・Issue #1574）。
///
/// ゾーンは `Z`／`z`、数値 offset（`parse_numeric_offset` 参照）、IANA 名のいずれか。
/// ゾーンが無い入力は `default_tz`（セッション TimeZone。#1575）の壁時計として解釈する。
/// 入力は untrusted のため長さ → ASCII → 閉じた文法の順に検査する。
pub fn parse_timestamptz(
    s: &str,
    default_tz: &SessionTimeZone,
) -> Result<i64, TimestampTzLiteralError> {
    if s.len() > MAX_TIMESTAMPTZ_LITERAL_LEN {
        return Err(DateTimeLiteralError::Format(
            "TIMESTAMPTZ literal exceeds the maximum length".to_string(),
        )
        .into());
    }
    if !s.is_ascii() {
        return Err(
            DateTimeLiteralError::Format("TIMESTAMPTZ literal must be ASCII".to_string()).into(),
        );
    }
    let bytes = s.as_bytes();
    let (_, _, _, after_date) = parse_date_part(bytes)?;
    // 日時部の終端 = 区切りの後で数字・':'・'.' が途切れる位置。
    let mut end = after_date.saturating_add(1).min(bytes.len());
    while bytes
        .get(end)
        .is_some_and(|b| b.is_ascii_digit() || *b == b':' || *b == b'.')
    {
        end += 1;
    }
    let (Some(naive_text), Some(rest)) = (s.get(..end), s.get(end..)) else {
        return Err(
            DateTimeLiteralError::Format("malformed TIMESTAMPTZ literal".to_string()).into(),
        );
    };
    let naive = parse_timestamp(naive_text)?;
    let zone = rest.strip_prefix(' ').unwrap_or(rest);

    let utc = if rest.is_empty() {
        default_tz
            .local_to_utc_micros(naive)
            .ok_or_else(timestamptz_out_of_range)?
    } else if zone.is_empty() {
        // 末尾に空白だけが残る入力は書式違反。
        return Err(DateTimeLiteralError::Format(
            "TIMESTAMPTZ literal has trailing characters".to_string(),
        )
        .into());
    } else if matches!(zone, "Z" | "z") {
        naive
    } else if let Some((sign, digits)) = zone
        .strip_prefix('+')
        .map(|d| (1i64, d))
        .or_else(|| zone.strip_prefix('-').map(|d| (-1i64, d)))
    {
        let secs = parse_numeric_offset(sign, digits.as_bytes())?;
        secs.checked_mul(1_000_000)
            .and_then(|o| naive.checked_sub(o))
            .ok_or_else(timestamptz_out_of_range)?
    } else {
        let tz =
            SessionTimeZone::resolve_named(zone).ok_or(TimestampTzLiteralError::UnknownTimeZone)?;
        tz.local_to_utc_micros(naive)
            .ok_or_else(timestamptz_out_of_range)?
    };
    if !validate_timestamptz_micros(utc) {
        return Err(timestamptz_out_of_range().into());
    }
    Ok(utc)
}

/// `TIMESTAMPTZ` の UTC マイクロ秒を `tz` の壁時計へ変換し、PG の ISO 出力
/// （`YYYY-MM-DD HH:MM:SS[.ffffff]+HH[:MM[:SS]]`）で整形する。局所年が 0 以下になる場合は
/// 年を `1 - year` とし、offset の後ろに ` BC` を付ける。
pub fn format_timestamptz(utc_micros: i64, tz: &SessionTimeZone) -> String {
    let offset_secs = tz.offset_seconds_at(utc_micros);
    let local = utc_micros.saturating_add(i64::from(offset_secs) * 1_000_000);
    let days = local.div_euclid(MICROS_PER_DAY);
    let day_micros = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let (shown_year, bc) = if year <= 0 {
        (1 - year, true)
    } else {
        (year, false)
    };
    let mut out = String::with_capacity(36);
    let _ = write!(out, "{shown_year:04}-{month:02}-{day:02} ");
    write_time_of_day(&mut out, day_micros);
    let abs = offset_secs.unsigned_abs();
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let (hh, mm, ss) = (abs / 3600, (abs % 3600) / 60, abs % 60);
    let _ = write!(out, "{sign}{hh:02}");
    if mm != 0 || ss != 0 {
        let _ = write!(out, ":{mm:02}");
    }
    if ss != 0 {
        let _ = write!(out, ":{ss:02}");
    }
    if bc {
        out.push_str(" BC");
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

    #[test]
    fn time_roundtrip_and_boundaries() {
        assert_eq!(parse_time("00:00:00"), Ok(0));
        assert_eq!(parse_time("23:59:59.999999"), Ok(MICROS_PER_DAY - 1));
        assert_eq!(parse_time("24:00:00"), Ok(MICROS_PER_DAY));
        assert_eq!(parse_time("12:30"), Ok(45_000_000_000));
        for v in [0, 1, 500_000, MICROS_PER_DAY - 1, MICROS_PER_DAY] {
            assert_eq!(parse_time(&format_time(v)), Ok(v));
        }
        assert_eq!(format_time(MICROS_PER_DAY), "24:00:00");
        assert_eq!(format_time(MICROS_PER_DAY - 1), "23:59:59.999999");
        assert_eq!(format_time(500_000), "00:00:00.5");
    }

    #[test]
    fn time_rounding_then_range() {
        assert_eq!(parse_time("12:00:00.1234565"), Ok(43_200_123_457));
        assert_eq!(parse_time("12:00:00.1234564999"), Ok(43_200_123_456));
        assert_eq!(parse_time("23:59:59.9999995"), Ok(MICROS_PER_DAY));
        assert_eq!(parse_time("24:00:00.0000004"), Ok(MICROS_PER_DAY));
        assert!(matches!(
            parse_time("24:00:00.000001"),
            Err(DateTimeLiteralError::Overflow(_))
        ));
    }

    #[test]
    fn time_error_classification() {
        for s in ["24:00:00.000001", "25:00", "12:60", "12:00:60", "24:01"] {
            assert!(
                matches!(parse_time(s), Err(DateTimeLiteralError::Overflow(_))),
                "{s}"
            );
        }
        for s in [
            "",
            "abc",
            "12:00:00+09",
            " 12:00",
            "1:2:3",
            "12:00 PM",
            "12",
            "12:00:",
            "12:00:00.",
            "12:00.5",
            "12:00:00x",
            "allballs",
            "１２:00",
        ] {
            assert!(
                matches!(parse_time(s), Err(DateTimeLiteralError::Format(_))),
                "{s}"
            );
        }
        let long = format!("12:00:00.{}", "1".repeat(60));
        assert!(matches!(
            parse_time(&long),
            Err(DateTimeLiteralError::Format(_))
        ));
    }

    // ---- TIMESTAMPTZ（TABLE-33・Issue #1574） ----

    fn tstz(s: &str) -> Result<i64, TimestampTzLiteralError> {
        parse_timestamptz(s, &SessionTimeZone::utc())
    }

    fn tstz_ok(s: &str) -> i64 {
        tstz(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    #[test]
    fn timestamptz_offset_forms_normalize_to_same_utc() {
        let base = parse_timestamp("2026-01-01 03:00:00").unwrap();
        for lit in [
            "2026-01-01 03:00:00",
            "2026-01-01 03:00:00Z",
            "2026-01-01 03:00:00 z",
            "2026-01-01T03:00:00+00",
            "2026-01-01 03:00:00+00:00",
            "2026-01-01 12:00:00+09",
            "2026-01-01 12:00:00 +09",
            "2026-01-01 12:00:00+0900",
            "2026-01-01 12:00:00+09:00",
            "2026-01-01 12:00:00+09:00:00",
            "2026-01-01 08:30:00+0530",
            "2025-12-31 22:00:00-05",
            "2026-01-01 03:00:00 UTC",
            "2026-01-01 12:00:00 Asia/Tokyo",
        ] {
            assert_eq!(tstz_ok(lit), base, "{lit}");
        }
    }

    #[test]
    fn timestamptz_rejects_malformed_with_format_error() {
        for lit in [
            "",
            "2026-01-01",
            "2026-01-01 03:00",
            "2026-01-01 03:00:00.1234567Z",
            "2026-01-01 03:00:00+9",
            "2026-01-01 03:00:00+090",
            "2026-01-01 03:00:00+09:0",
            "2026-01-01 03:00:00+09:00:0",
            "2026-01-01 03:00:00+0a",
            "2026-01-01 03:00:00 ",
            "2026-01-01 03:00:00+09:00x",
            "2026-01-01 03:00:00\u{ff0b}09",
            "2026-01-01 03:00:00\u{3042}",
        ] {
            assert!(
                matches!(
                    tstz(lit),
                    Err(TimestampTzLiteralError::Literal(
                        DateTimeLiteralError::Format(_)
                    ))
                ),
                "{lit}: {:?}",
                tstz(lit)
            );
        }
        let long = format!(
            "2026-01-01 03:00:00 {}",
            "A".repeat(MAX_TIMESTAMPTZ_LITERAL_LEN)
        );
        assert!(matches!(
            tstz(&long),
            Err(TimestampTzLiteralError::Literal(
                DateTimeLiteralError::Format(_)
            ))
        ));
    }

    #[test]
    fn timestamptz_out_of_range_maps_to_overflow() {
        for lit in [
            "2026-01-01 03:00:00+16",
            "2026-01-01 03:00:00+0960",
            "2026-01-01 03:00:00+09:00:60",
            "0001-01-01 00:00:00+01",
            "9999-12-31 23:59:59.999999-01",
            "2026-02-30 03:00:00Z",
        ] {
            assert!(
                matches!(
                    tstz(lit),
                    Err(TimestampTzLiteralError::Literal(
                        DateTimeLiteralError::Overflow(_)
                    ))
                ),
                "{lit}: {:?}",
                tstz(lit)
            );
        }
        assert_eq!(tstz_ok("0001-01-01 00:00:00+00"), TIMESTAMPTZ_MIN_MICROS);
        assert_eq!(
            tstz_ok("9999-12-31 23:59:59.999999Z"),
            TIMESTAMPTZ_MAX_MICROS
        );
        assert!(matches!(
            tstz("0001-01-01 00:00:00.000000+00:00:01"),
            Err(TimestampTzLiteralError::Literal(
                DateTimeLiteralError::Overflow(_)
            ))
        ));
    }

    #[test]
    fn timestamptz_unknown_zone_names_are_rejected() {
        for lit in [
            "2026-01-01 03:00:00 Mars/Base",
            "2026-01-01 03:00:00 Etc/Unknown",
            "2026-01-01 03:00:00 ../../etc/passwd",
            "2026-01-01 03:00:00 Asia/Tokyo;",
        ] {
            assert_eq!(
                tstz(lit),
                Err(TimestampTzLiteralError::UnknownTimeZone),
                "{lit}"
            );
        }
        assert!(SessionTimeZone::resolve_named("").is_none());
        assert!(SessionTimeZone::resolve_named(&"A".repeat(MAX_TZ_NAME_LEN + 1)).is_none());
    }

    /// DST の gap／fold の解決は暫定（#1576 が PG 17 実測と照合して確定する）。
    #[test]
    fn timestamptz_named_zone_dst_resolution_is_provisional() {
        // gap（2026-03-08 02:30 は存在しない）は直前の offset（-05）で解釈 => 07:30Z。
        assert_eq!(
            tstz_ok("2026-03-08 02:30:00 America/New_York"),
            tstz_ok("2026-03-08 07:30:00Z")
        );
        // fold（2026-11-01 01:30 が 2 回）は後の offset（-05）で解釈 => 06:30Z。
        assert_eq!(
            tstz_ok("2026-11-01 01:30:00 America/New_York"),
            tstz_ok("2026-11-01 06:30:00Z")
        );
        assert_eq!(
            tstz_ok("2026-07-01 12:00:00 America/New_York"),
            tstz_ok("2026-07-01 16:00:00Z")
        );
        // セッション TimeZone（naive 入力の解釈）。
        let ny = SessionTimeZone::resolve_named("America/New_York").unwrap();
        assert_eq!(
            parse_timestamptz("2026-07-01 12:00:00", &ny).unwrap(),
            tstz_ok("2026-07-01 16:00:00Z")
        );
        // 明示 offset はセッション TimeZone に影響されない。
        assert_eq!(
            parse_timestamptz("2026-07-01 12:00:00+00", &ny).unwrap(),
            tstz_ok("2026-07-01 12:00:00Z")
        );
    }

    #[test]
    fn format_timestamptz_uses_iso_offset_suffix() {
        let utc = SessionTimeZone::utc();
        assert_eq!(
            format_timestamptz(tstz_ok("2026-01-01 03:00:00Z"), &utc),
            "2026-01-01 03:00:00+00"
        );
        assert_eq!(
            format_timestamptz(tstz_ok("2026-01-01 03:00:00.5Z"), &utc),
            "2026-01-01 03:00:00.5+00"
        );
        let tokyo = SessionTimeZone::resolve_named("Asia/Tokyo").unwrap();
        assert_eq!(
            format_timestamptz(tstz_ok("2026-01-01 03:00:00Z"), &tokyo),
            "2026-01-01 12:00:00+09"
        );
        let kolkata = SessionTimeZone::resolve_named("Asia/Kolkata").unwrap();
        assert_eq!(
            format_timestamptz(tstz_ok("2026-01-01 03:00:00Z"), &kolkata),
            "2026-01-01 08:30:00+05:30"
        );
        // LMT のように秒を含む offset。
        assert_eq!(
            format_timestamptz(tstz_ok("1880-01-01 00:00:00Z"), &tokyo),
            "1880-01-01 09:18:59+09:18:59"
        );
    }

    #[test]
    fn format_timestamptz_handles_range_edges() {
        let utc = SessionTimeZone::utc();
        assert_eq!(
            format_timestamptz(TIMESTAMPTZ_MIN_MICROS, &utc),
            "0001-01-01 00:00:00+00"
        );
        assert_eq!(
            format_timestamptz(TIMESTAMPTZ_MAX_MICROS, &utc),
            "9999-12-31 23:59:59.999999+00"
        );
        let ny = SessionTimeZone::resolve_named("America/New_York").unwrap();
        // 局所年が 0 以下になる場合は BC 表記。NY の LMT は -04:56:02。
        assert_eq!(
            format_timestamptz(TIMESTAMPTZ_MIN_MICROS, &ny),
            "0001-12-31 19:03:58-04:56:02 BC"
        );
        let tokyo = SessionTimeZone::resolve_named("Asia/Tokyo").unwrap();
        // 局所年が 10000 になる場合は 5 桁のまま出す。
        assert_eq!(
            format_timestamptz(TIMESTAMPTZ_MAX_MICROS, &tokyo),
            "10000-01-01 08:59:59.999999+09"
        );
        // 往復（UTC）。
        for v in [TIMESTAMPTZ_MIN_MICROS, 0, TIMESTAMPTZ_MAX_MICROS] {
            assert_eq!(tstz_ok(&format_timestamptz(v, &utc)), v);
        }
    }
}
