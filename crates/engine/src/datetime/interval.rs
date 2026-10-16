//! `INTERVAL` 列型（TABLE-34・Issue #1579）の値表現・リテラル解析・整形。
//!
//! 責務境界: `datetime` モジュールの子として `INTERVAL` の三要素表現（月・日・
//! マイクロ秒）、文字列リテラルの閉じた文法の解析、`IntervalStyle=postgres` 形式の
//! 整形、番兵の検証を集約する。`catalog`（列型宣言）・`row_codec`（17 バイトの行
//! ペイロード）・`sql::parser`（リテラル束縛と SQLSTATE 写像）・`wire-server`
//! （投影テキスト）が本モジュールへ委譲する。比較・正規化・算術は後続 Issue
//! （#1580・#1581）の担当で、ここでは生の三要素の一致だけを扱う。
//!
//! 受理する文法は PostgreSQL 17 の公開仕様に準拠した閉じた定義とする。
//! (a) 単位句（`1 day 2 hours`・`@ 1 day ago`）、(b) SQL 標準形（`1-2`・`1 2:03:04`）、
//! (c) ISO 8601（`P1Y2M3DT4H5M6S`・`P0001-02-03T04:05:06`）。
//! 入力は untrusted なので、長さ・フィールド数の上限を走査前後で検査し、算術は
//! すべて checked 演算で行う（panic も wrap も起こさない）。

use std::fmt::Write as _;

/// リテラルの最大長（バイト）。PG の作業バッファ 256 に合わせ、走査前に検査する。
pub const MAX_INTERVAL_LITERAL_LEN: usize = 255;

/// 単位句形式のフィールド数上限（PG の `MAXDATEFIELDS` 相当）。
const MAX_INTERVAL_FIELDS: usize = 25;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
const MICROS_PER_DAY_I64: i64 = 24 * MICROS_PER_HOUR;
const DAYS_PER_MONTH: f64 = 30.0;
const MONTHS_PER_YEAR: i64 = 12;

/// `INTERVAL` の値。月・日・マイクロ秒を独立に保持する（`1 mon` と `30 days` は
/// 別の値。正規化比較は #1580）。`PartialEq` は生の三要素の一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interval {
    /// 月数（年は 12 か月に換算済み）。
    pub months: i32,
    /// 日数。
    pub days: i32,
    /// マイクロ秒（時・分・秒）。
    pub micros: i64,
}

/// 無限大の予約値（PG17 の `interval_in` と同じ番兵）。本 Issue では受理しない。
const INFINITY_SENTINEL: Interval = Interval {
    months: i32::MAX,
    days: i32::MAX,
    micros: i64::MAX,
};
const NEG_INFINITY_SENTINEL: Interval = Interval {
    months: i32::MIN,
    days: i32::MIN,
    micros: i64::MIN,
};

/// 番兵（`±infinity` の予約値）以外なら true。codec の encode・decode・scan と
/// `tenant::validate_set_assignments` が多層防御として呼ぶ。
pub fn validate_interval(v: &Interval) -> bool {
    *v != INFINITY_SENTINEL && *v != NEG_INFINITY_SENTINEL
}

/// `INTERVAL` リテラルの解析エラー。`sql::parser` が SQLSTATE へ写像する
/// （`Format` は 22007、`FieldOverflow` は 22015、`OutOfRange` は 22008、
/// `Infinity` は 0A000）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntervalLiteralError {
    /// 閉じた文法に一致しない（未知の単位・語順違反・空入力・非 ASCII・長さ超過等）。
    Format(&'static str),
    /// 個々のフィールドの値が範囲外（数値が i64 に収まらない・単位換算や累積があふれる）。
    FieldOverflow,
    /// 全フィールド合成後に範囲外（年×12＋月が i32 をあふれる・番兵と一致する）。
    OutOfRange,
    /// `infinity`／`-infinity`（未対応）。
    Infinity,
}

impl std::fmt::Display for IntervalLiteralError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntervalLiteralError::Format(d) => write!(f, "{d}"),
            IntervalLiteralError::FieldOverflow => write!(f, "interval field value out of range"),
            IntervalLiteralError::OutOfRange => write!(f, "interval out of range"),
            IntervalLiteralError::Infinity => {
                write!(f, "infinite interval values are not supported")
            }
        }
    }
}

type ParseResult<T> = Result<T, IntervalLiteralError>;

/// 合成前の中間値（PG の `pg_itm_in` 相当）。
#[derive(Default, Clone, Copy)]
struct Acc {
    years: i32,
    months: i32,
    days: i32,
    micros: i64,
}

impl Acc {
    fn add_years(&mut self, v: i64) -> ParseResult<()> {
        let v = i32::try_from(v).map_err(|_| IntervalLiteralError::FieldOverflow)?;
        self.years = self
            .years
            .checked_add(v)
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        Ok(())
    }
    fn add_months(&mut self, v: i64) -> ParseResult<()> {
        let v = i32::try_from(v).map_err(|_| IntervalLiteralError::FieldOverflow)?;
        self.months = self
            .months
            .checked_add(v)
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        Ok(())
    }
    fn add_days(&mut self, v: i64) -> ParseResult<()> {
        let v = i32::try_from(v).map_err(|_| IntervalLiteralError::FieldOverflow)?;
        self.days = self
            .days
            .checked_add(v)
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        Ok(())
    }
    fn add_micros(&mut self, v: i64) -> ParseResult<()> {
        self.micros = self
            .micros
            .checked_add(v)
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        Ok(())
    }

    /// 日の端数（`scale` 日単位）を日とマイクロ秒へ繰り下げる（PG の
    /// `AdjustFractDays` 相当。端数 → µs は偶数丸め）。
    fn adjust_fract_days(&mut self, frac: f64, scale: f64) -> ParseResult<()> {
        if frac == 0.0 {
            return Ok(());
        }
        let scaled = frac * scale;
        let extra_days = scaled.trunc();
        self.add_days(extra_days as i64)?;
        let rest = scaled - extra_days;
        let micros = round_to_i64(rest * MICROS_PER_DAY_I64 as f64)?;
        self.add_micros(micros)
    }

    /// 年の端数（`scale` 年単位）を整数月へ偶数丸めして加える（PG の
    /// `AdjustFractYears` 相当）。月の残余は日へ繰り下げない（`0.1 years` は `1 mon`）。
    fn adjust_fract_years(&mut self, frac: f64, scale: i64) -> ParseResult<()> {
        if frac == 0.0 {
            return Ok(());
        }
        let scaled = frac * (scale * MONTHS_PER_YEAR) as f64;
        let extra_months = round_to_i64(scaled)?;
        self.add_months(extra_months)
    }

    /// 整数部と端数を `scale` マイクロ秒単位で加える（`AdjustMicroseconds` 相当）。
    fn add_scaled_micros(&mut self, val: i64, frac: f64, scale: i64) -> ParseResult<()> {
        let whole = val
            .checked_mul(scale)
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        self.add_micros(whole)?;
        let part = round_to_i64(frac * scale as f64)?;
        self.add_micros(part)
    }
}

/// f64 を偶数丸めして i64 へ変換する。NaN・無限大・範囲外は FieldOverflow。
fn round_to_i64(v: f64) -> ParseResult<i64> {
    let r = v.round_ties_even();
    if !r.is_finite() || r < -9.2e18 || r > 9.2e18 {
        return Err(IntervalLiteralError::FieldOverflow);
    }
    Ok(r as i64)
}

/// 単位。
#[derive(Clone, Copy)]
enum Unit {
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
    Decade,
    Century,
    Millennium,
}

fn lookup_unit(word: &str) -> Option<Unit> {
    Some(match word {
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" | "usecond" | "useconds" => {
            Unit::Microsecond
        }
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" | "msecond" | "mseconds" => {
            Unit::Millisecond
        }
        "second" | "seconds" | "s" | "sec" | "secs" => Unit::Second,
        "minute" | "minutes" | "m" | "min" | "mins" => Unit::Minute,
        "hour" | "hours" | "h" | "hr" | "hrs" => Unit::Hour,
        "day" | "days" | "d" => Unit::Day,
        "week" | "weeks" | "w" => Unit::Week,
        "month" | "months" | "mon" | "mons" => Unit::Month,
        "year" | "years" | "y" | "yr" | "yrs" => Unit::Year,
        "decade" | "decades" | "dec" | "decs" => Unit::Decade,
        "century" | "centuries" | "c" | "cent" | "cents" => Unit::Century,
        "millennium" | "millennia" | "mil" | "mils" => Unit::Millennium,
        _ => return None,
    })
}

/// 数値トークン（符号付き整数部と端数）。
#[derive(Clone, Copy)]
struct Num {
    int: i64,
    /// 符号適用済みの端数（(-1, 1) の範囲）。
    frac: f64,
}

fn apply_unit(acc: &mut Acc, n: Num, unit: Unit) -> ParseResult<()> {
    match unit {
        Unit::Microsecond => acc.add_scaled_micros(n.int, n.frac, 1),
        Unit::Millisecond => acc.add_scaled_micros(n.int, n.frac, 1_000),
        Unit::Second => acc.add_scaled_micros(n.int, n.frac, MICROS_PER_SECOND),
        Unit::Minute => acc.add_scaled_micros(n.int, n.frac, MICROS_PER_MINUTE),
        Unit::Hour => acc.add_scaled_micros(n.int, n.frac, MICROS_PER_HOUR),
        Unit::Day => {
            acc.add_days(n.int)?;
            acc.adjust_fract_days(n.frac, 1.0)
        }
        Unit::Week => {
            let d = n
                .int
                .checked_mul(7)
                .ok_or(IntervalLiteralError::FieldOverflow)?;
            acc.add_days(d)?;
            acc.adjust_fract_days(n.frac, 7.0)
        }
        Unit::Month => {
            acc.add_months(n.int)?;
            acc.adjust_fract_days(n.frac, DAYS_PER_MONTH)
        }
        Unit::Year => add_years_scaled(acc, n, 1),
        Unit::Decade => add_years_scaled(acc, n, 10),
        Unit::Century => add_years_scaled(acc, n, 100),
        Unit::Millennium => add_years_scaled(acc, n, 1000),
    }
}

fn add_years_scaled(acc: &mut Acc, n: Num, scale: i64) -> ParseResult<()> {
    let y = n
        .int
        .checked_mul(scale)
        .ok_or(IntervalLiteralError::FieldOverflow)?;
    acc.add_years(y)?;
    acc.adjust_fract_years(n.frac, scale)
}

/// 合成: 年×12＋月を i32 に収め、番兵と一致したら範囲外。
fn compose(acc: Acc) -> ParseResult<Interval> {
    let months = i64::from(acc.years) * MONTHS_PER_YEAR + i64::from(acc.months);
    let months = i32::try_from(months).map_err(|_| IntervalLiteralError::OutOfRange)?;
    let v = Interval {
        months,
        days: acc.days,
        micros: acc.micros,
    };
    if !validate_interval(&v) {
        return Err(IntervalLiteralError::OutOfRange);
    }
    Ok(v)
}

/// 字句走査器。ASCII 検証済みのバイト列上で位置を進める。
struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn peek_at(&self, off: usize) -> Option<u8> {
        self.b.get(self.i.checked_add(off)?).copied()
    }
    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }
    fn at_end(&self) -> bool {
        self.i >= self.b.len()
    }
    /// 10 進数字列を読み、スライスを返す（空なら None）。
    fn digits(&mut self) -> Option<&'a [u8]> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start {
            None
        } else {
            self.b.get(start..self.i)
        }
    }
    fn sign(&mut self) -> bool {
        match self.peek() {
            Some(b'-') => {
                self.i += 1;
                true
            }
            Some(b'+') => {
                self.i += 1;
                false
            }
            _ => false,
        }
    }
}

fn digits_to_i64(d: &[u8]) -> ParseResult<i64> {
    let mut v: i64 = 0;
    for &c in d {
        v = v
            .checked_mul(10)
            .and_then(|x| x.checked_add(i64::from(c - b'0')))
            .ok_or(IntervalLiteralError::FieldOverflow)?;
    }
    Ok(v)
}

/// 端数桁（`.` の後の数字列）を (0, 1) の f64 へ。
fn frac_digits(d: &[u8]) -> f64 {
    let mut s = String::with_capacity(d.len().saturating_add(2));
    s.push_str("0.");
    for &c in d {
        s.push(c as char);
    }
    s.parse::<f64>().unwrap_or(0.0)
}

/// `[sign] digits [. digits]`（整数部か端数部のどちらかは必須）を読む。
fn read_number(sc: &mut Scanner<'_>) -> ParseResult<Num> {
    const BAD: IntervalLiteralError =
        IntervalLiteralError::Format("invalid input syntax for type interval");
    let neg = sc.sign();
    let int_digits = sc.digits();
    let mut frac_part: Option<&[u8]> = None;
    if sc.peek() == Some(b'.') {
        sc.i += 1;
        frac_part = Some(sc.digits().ok_or(BAD)?);
    }
    if int_digits.is_none() && frac_part.is_none() {
        return Err(BAD);
    }
    let mut int = match int_digits {
        Some(d) => digits_to_i64(d)?,
        None => 0,
    };
    let mut frac = frac_part.map_or(0.0, frac_digits);
    if neg {
        int = -int; // digits_to_i64 は 0..=i64::MAX なので反転は安全。
        frac = -frac;
    }
    Ok(Num { int, frac })
}

/// 負側の i64::MIN（`-9223372036854775808`）を受理するための特別扱いは行わない
/// （整数部が i64::MAX を超える入力は FieldOverflow）。
const BAD_FORMAT: IntervalLiteralError =
    IntervalLiteralError::Format("invalid input syntax for type interval");

/// 現在位置から時刻トークン `[sign]H+:MM[:SS[.f+]]` を読む。
fn read_time(sc: &mut Scanner<'_>, acc: &mut Acc) -> ParseResult<()> {
    let neg = sc.sign();
    let h = sc.digits().ok_or(BAD_FORMAT)?;
    if sc.peek() != Some(b':') {
        return Err(BAD_FORMAT);
    }
    sc.i += 1;
    let h = digits_to_i64(h)?;
    let m = digits_to_i64(sc.digits().ok_or(BAD_FORMAT)?)?;
    let mut hour = h;
    let mut min = m;
    let mut sec: i64 = 0;
    let mut frac = 0.0;
    if sc.peek() == Some(b':') {
        sc.i += 1;
        sec = digits_to_i64(sc.digits().ok_or(BAD_FORMAT)?)?;
        if sc.peek() == Some(b'.') {
            sc.i += 1;
            frac = frac_digits(sc.digits().ok_or(BAD_FORMAT)?);
        }
    } else if sc.peek() == Some(b'.') {
        // `MM:SS.f` 形（時を持たない）。
        sc.i += 1;
        frac = frac_digits(sc.digits().ok_or(BAD_FORMAT)?);
        sec = m;
        min = h;
        hour = 0;
    }
    if min > 59 || sec > 60 {
        return Err(IntervalLiteralError::FieldOverflow);
    }
    let total = i128::from(hour) * i128::from(MICROS_PER_HOUR)
        + i128::from(min) * i128::from(MICROS_PER_MINUTE)
        + i128::from(sec) * i128::from(MICROS_PER_SECOND)
        + i128::from(round_to_i64(frac * MICROS_PER_SECOND as f64)?);
    let total = if neg { -total } else { total };
    let total = i64::try_from(total).map_err(|_| IntervalLiteralError::FieldOverflow)?;
    acc.add_micros(total)
}

/// 次のトークンが時刻形（`[sign]digits:`）か先読みする。位置は進めない。
fn next_is_time(sc: &Scanner<'_>) -> bool {
    let mut off = 0usize;
    while sc.peek_at(off).is_some_and(|c| c.is_ascii_whitespace()) {
        off += 1;
    }
    if matches!(sc.peek_at(off), Some(b'+') | Some(b'-')) {
        off += 1;
    }
    let mut n = 0;
    while sc.peek_at(off).is_some_and(|c| c.is_ascii_digit()) {
        off += 1;
        n += 1;
    }
    n > 0 && sc.peek_at(off) == Some(b':')
}

/// 現在位置の数値トークンが SQL 標準形 `[sign]Y-M` か先読みする。
fn is_year_month(sc: &Scanner<'_>) -> bool {
    let mut off = 0usize;
    if matches!(sc.peek_at(off), Some(b'+') | Some(b'-')) {
        off += 1;
    }
    let mut n = 0;
    while sc.peek_at(off).is_some_and(|c| c.is_ascii_digit()) {
        off += 1;
        n += 1;
    }
    n > 0
        && sc.peek_at(off) == Some(b'-')
        && sc.peek_at(off + 1).is_some_and(|c| c.is_ascii_digit())
}

/// 現在位置が時刻形か（符号＋数字列＋`:`）。
fn here_is_time(sc: &Scanner<'_>) -> bool {
    let mut off = 0usize;
    if matches!(sc.peek_at(off), Some(b'+') | Some(b'-')) {
        off += 1;
    }
    let mut n = 0;
    while sc.peek_at(off).is_some_and(|c| c.is_ascii_digit()) {
        off += 1;
        n += 1;
    }
    n > 0 && sc.peek_at(off) == Some(b':')
}

fn read_year_month(sc: &mut Scanner<'_>, acc: &mut Acc) -> ParseResult<()> {
    let neg = sc.sign();
    let y = digits_to_i64(sc.digits().ok_or(BAD_FORMAT)?)?;
    if sc.peek() != Some(b'-') {
        return Err(BAD_FORMAT);
    }
    sc.i += 1;
    let m = digits_to_i64(sc.digits().ok_or(BAD_FORMAT)?)?;
    if m > 11 {
        return Err(IntervalLiteralError::FieldOverflow);
    }
    let (y, m) = if neg { (-y, -m) } else { (y, m) };
    // `2020-01-01` や `1-2-3` のような日付形は PG17 で 22007 となるため、Y-M の直後の `-` を拒否する。
    if sc.peek() == Some(b'-') {
        return Err(BAD_FORMAT);
    }
    acc.add_years(y)?;
    acc.add_months(m)
}

/// 単位句・SQL 標準形（(a)・(b)）を `Acc` へ解析する。
fn parse_postgres_like(s: &str) -> ParseResult<Acc> {
    let mut sc = Scanner {
        b: s.as_bytes(),
        i: 0,
    };
    let mut acc = Acc::default();
    let mut fields = 0usize;
    let mut ago = false;
    let mut seen_item = false;
    // 値要素（数値・時刻・年月）を 1 つ以上読んだか。`@` 単独・`ago` 単独を拒否するため。
    let mut values = 0usize;
    loop {
        sc.skip_ws();
        if sc.at_end() {
            break;
        }
        fields += 1;
        if fields > MAX_INTERVAL_FIELDS {
            return Err(BAD_FORMAT);
        }
        let c = sc.peek().ok_or(BAD_FORMAT)?;
        if c == b'@' {
            if seen_item {
                return Err(BAD_FORMAT);
            }
            seen_item = true;
            sc.i += 1;
            continue;
        }
        seen_item = true;
        if c.is_ascii_alphabetic() {
            let word = read_word(&mut sc);
            if word == "ago" {
                ago = true;
                sc.skip_ws();
                if !sc.at_end() {
                    return Err(BAD_FORMAT);
                }
                break;
            }
            return Err(BAD_FORMAT);
        }
        if is_year_month(&sc) {
            read_year_month(&mut sc, &mut acc)?;
            values += 1;
            continue;
        }
        if here_is_time(&sc) {
            read_time(&mut sc, &mut acc)?;
            values += 1;
            continue;
        }
        let n = read_number(&mut sc)?;
        values += 1;
        sc.skip_ws();
        match sc.peek() {
            Some(c) if c.is_ascii_alphabetic() => {
                let save = sc.i;
                let word = read_word(&mut sc);
                if word == "ago" {
                    // 単位なしの数値（秒）の直後の `ago`。
                    sc.i = save;
                    apply_unit(&mut acc, n, Unit::Second)?;
                } else {
                    fields += 1;
                    if fields > MAX_INTERVAL_FIELDS {
                        return Err(BAD_FORMAT);
                    }
                    let unit = lookup_unit(&word).ok_or(BAD_FORMAT)?;
                    apply_unit(&mut acc, n, unit)?;
                }
            }
            _ => {
                if next_is_time(&sc) {
                    // `1 2:03:04` の先頭の数値は日。
                    apply_unit(&mut acc, n, Unit::Day)?;
                } else {
                    apply_unit(&mut acc, n, Unit::Second)?;
                }
            }
        }
    }
    if !seen_item || values == 0 {
        return Err(BAD_FORMAT);
    }
    if ago {
        acc.years = acc
            .years
            .checked_neg()
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        acc.months = acc
            .months
            .checked_neg()
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        acc.days = acc
            .days
            .checked_neg()
            .ok_or(IntervalLiteralError::FieldOverflow)?;
        acc.micros = acc
            .micros
            .checked_neg()
            .ok_or(IntervalLiteralError::FieldOverflow)?;
    }
    Ok(acc)
}

/// 英字列を小文字化して読む。
fn read_word(sc: &mut Scanner<'_>) -> String {
    let mut w = String::new();
    while let Some(c) = sc.peek() {
        if c.is_ascii_alphabetic() {
            w.push(c.to_ascii_lowercase() as char);
            sc.i += 1;
        } else {
            break;
        }
    }
    w
}

/// ISO 8601（(c)）を `Acc` へ解析する。`s` は `P` で始まる前提（大文字小文字区別）。
fn parse_iso8601(s: &str) -> ParseResult<Acc> {
    let mut sc = Scanner {
        b: s.as_bytes(),
        i: 1,
    };
    let mut acc = Acc::default();
    if sc.at_end() {
        return Err(BAD_FORMAT);
    }
    // 代替形式 `P[YYYY-MM-DD][THH:MM:SS]`。
    if is_year_month_plain(&sc) {
        let y = digits_to_i64(sc.digits().ok_or(BAD_FORMAT)?)?;
        sc.i += 1; // '-'
        let m = digits_to_i64(sc.digits().ok_or(BAD_FORMAT)?)?;
        if sc.peek() != Some(b'-') {
            return Err(BAD_FORMAT);
        }
        sc.i += 1;
        let d = digits_to_i64(sc.digits().ok_or(BAD_FORMAT)?)?;
        if m > 12 || d > 31 {
            return Err(IntervalLiteralError::FieldOverflow);
        }
        acc.add_years(y)?;
        acc.add_months(m)?;
        acc.add_days(d)?;
        if !sc.at_end() {
            if sc.peek() != Some(b'T') {
                return Err(BAD_FORMAT);
            }
            sc.i += 1;
            if !here_is_time(&sc) {
                return Err(BAD_FORMAT);
            }
            read_time(&mut sc, &mut acc)?;
            if !sc.at_end() {
                return Err(BAD_FORMAT);
            }
        }
        return Ok(acc);
    }
    let mut in_time = false;
    let mut components = 0usize;
    let mut time_components = 0usize;
    while !sc.at_end() {
        if sc.peek() == Some(b'T') {
            if in_time {
                return Err(BAD_FORMAT);
            }
            in_time = true;
            sc.i += 1;
            continue;
        }
        components += 1;
        if in_time {
            time_components += 1;
        }
        if components > MAX_INTERVAL_FIELDS {
            return Err(BAD_FORMAT);
        }
        let n = read_number(&mut sc)?;
        let u = sc.peek().ok_or(BAD_FORMAT)?;
        sc.i += 1;
        let unit = match (in_time, u) {
            (false, b'Y') => Unit::Year,
            (false, b'M') => Unit::Month,
            (false, b'W') => Unit::Week,
            (false, b'D') => Unit::Day,
            (true, b'H') => Unit::Hour,
            (true, b'M') => Unit::Minute,
            (true, b'S') => Unit::Second,
            _ => return Err(BAD_FORMAT),
        };
        apply_unit(&mut acc, n, unit)?;
    }
    // `T` の後に時間要素が無い（`P1DT`）入力は不正。
    if components == 0 || (in_time && time_components == 0) {
        return Err(BAD_FORMAT);
    }
    Ok(acc)
}

/// 現在位置が `digits-digits-` で始まるか（ISO 代替形式の判定）。
fn is_year_month_plain(sc: &Scanner<'_>) -> bool {
    let mut off = 0usize;
    let mut n = 0;
    while sc.peek_at(off).is_some_and(|c| c.is_ascii_digit()) {
        off += 1;
        n += 1;
    }
    n > 0 && sc.peek_at(off) == Some(b'-')
}

/// `INTERVAL` リテラルを解析する。解析 → （将来の修飾子適用）→ 合成の段構成。
///
/// `sql::parser` の束縛関数から呼ばれ、エラーは呼び出し側が SQLSTATE へ写像する。
pub fn parse_interval(s: &str) -> Result<Interval, IntervalLiteralError> {
    if s.len() > MAX_INTERVAL_LITERAL_LEN {
        return Err(IntervalLiteralError::Format(
            "interval literal exceeds the maximum length",
        ));
    }
    if !s.is_ascii() {
        return Err(BAD_FORMAT);
    }
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    if t.is_empty() {
        return Err(BAD_FORMAT);
    }
    let lower = t.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "infinity" | "+infinity" | "-infinity" | "inf" | "+inf" | "-inf"
    ) {
        return Err(IntervalLiteralError::Infinity);
    }
    let acc = if t.starts_with('P') {
        parse_iso8601(t)?
    } else {
        parse_postgres_like(t)?
    };
    compose(acc)
}

/// `IntervalStyle=postgres` の整形（PG の `EncodeInterval` 相当）。
pub fn format_interval(v: &Interval) -> String {
    let mut out = String::new();
    let years = v.months / 12;
    let mons = v.months % 12;
    let mut is_before = false;
    let mut first = true;
    for (val, name) in [(years, "year"), (mons, "mon"), (v.days, "day")] {
        if val == 0 {
            continue;
        }
        if !first {
            out.push(' ');
        }
        let plus = if is_before && val > 0 { "+" } else { "" };
        let plural = if val == 1 { "" } else { "s" };
        let _ = write!(out, "{plus}{val} {name}{plural}");
        is_before = val < 0;
        first = false;
    }
    if first || v.micros != 0 {
        if !first {
            out.push(' ');
        }
        let neg = v.micros < 0;
        let abs = v.micros.unsigned_abs();
        if neg {
            out.push('-');
        } else if is_before {
            out.push('+');
        }
        let micros_per_hour = MICROS_PER_HOUR as u64;
        let micros_per_minute = MICROS_PER_MINUTE as u64;
        let micros_per_second = MICROS_PER_SECOND as u64;
        let h = abs / micros_per_hour;
        let m = (abs % micros_per_hour) / micros_per_minute;
        let s = (abs % micros_per_minute) / micros_per_second;
        let us = abs % micros_per_second;
        let _ = write!(out, "{h:02}:{m:02}:{s:02}");
        if us != 0 {
            let frac = format!("{us:06}");
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iv(months: i32, days: i32, micros: i64) -> Interval {
        Interval {
            months,
            days,
            micros,
        }
    }

    #[test]
    fn roundtrip_table() {
        let cases: &[(&str, &str)] = &[
            ("1 day 2 hours", "1 day 02:00:00"),
            (
                "1 year 2 months 3 days 4 hours 5 minutes 6.789 seconds",
                "1 year 2 mons 3 days 04:05:06.789",
            ),
            ("0", "00:00:00"),
            ("0 seconds", "00:00:00"),
            ("@ 1 day ago", "-1 days"),
            ("-1 days +02:03:00", "-1 days +02:03:00"),
            ("-1 year -2 mons +3 days", "-1 years -2 mons +3 days"),
            ("P1Y2M3DT4H5M6S", "1 year 2 mons 3 days 04:05:06"),
            ("P1W", "7 days"),
            ("PT1.5S", "00:00:01.5"),
            ("P0001-02-03T04:05:06", "1 year 2 mons 3 days 04:05:06"),
            ("1.5 years", "1 year 6 mons"),
            ("1.5 months", "1 mon 15 days"),
            ("0.1 years", "1 mon"),
            ("0.01 years", "00:00:00"),
            ("1.5 days", "1 day 12:00:00"),
            ("100 hours", "100:00:00"),
            ("1-2", "1 year 2 mons"),
            ("1 2:03:04", "1 day 02:03:04"),
            ("-00:00:01", "-00:00:01"),
            ("1 millisecond", "00:00:00.001"),
            ("1 mon -1 days", "1 mon -1 days"),
            ("  1 DAY   2 Hours ", "1 day 02:00:00"),
            ("1:30", "01:30:00"),
            ("-1-2", "-1 years -2 mons"),
            ("2 weeks", "14 days"),
            ("1 decade", "10 years"),
            ("1 microsecond", "00:00:00.000001"),
        ];
        for (input, expected) in cases {
            let v = parse_interval(input).unwrap_or_else(|e| panic!("{input}: {e:?}"));
            assert_eq!(&format_interval(&v), expected, "input {input}");
            assert_eq!(parse_interval(expected), Ok(v), "reparse {expected}");
        }
    }

    #[test]
    fn boundaries_accepted() {
        assert_eq!(parse_interval("2147483647 days"), Ok(iv(0, i32::MAX, 0)));
        assert_eq!(parse_interval("-2147483648 days"), Ok(iv(0, i32::MIN, 0)));
        let max = parse_interval("2562047788:00:54.775807").unwrap();
        assert_eq!(max.micros, i64::MAX);
        let min = parse_interval("-2562047788:00:54.775808").unwrap();
        assert_eq!(min.micros, i64::MIN);
        assert_eq!(
            parse_interval("-178956970 years -8 mons"),
            Ok(iv(i32::MIN, 0, 0))
        );
        assert_eq!(format_interval(&min), "-2562047788:00:54.775808");
    }

    #[test]
    fn errors_classified() {
        assert_eq!(
            parse_interval("2147483648 days"),
            Err(IntervalLiteralError::FieldOverflow)
        );
        assert_eq!(
            parse_interval("9223372036854775807 hours"),
            Err(IntervalLiteralError::FieldOverflow)
        );
        assert_eq!(
            parse_interval("178956971 years"),
            Err(IntervalLiteralError::OutOfRange)
        );
        for bad in [
            "1 fortnight",
            "ago 1 day",
            "1 day @",
            "",
            "   ",
            "1 日",
            "P",
            "PT",
            "1 day ago 1",
            "@",
            "ago",
            "@ ago",
            "P1DT",
            "2020-01-01",
            "1-2-3",
        ] {
            assert!(
                matches!(parse_interval(bad), Err(IntervalLiteralError::Format(_))),
                "{bad}"
            );
        }
        let long = "1 day ".repeat(60);
        assert!(matches!(
            parse_interval(&long),
            Err(IntervalLiteralError::Format(_))
        ));
        let many = "1 s ".repeat(13);
        assert!(matches!(
            parse_interval(&many),
            Err(IntervalLiteralError::Format(_))
        ));
        assert_eq!(
            parse_interval("infinity"),
            Err(IntervalLiteralError::Infinity)
        );
        assert_eq!(
            parse_interval("-Infinity"),
            Err(IntervalLiteralError::Infinity)
        );
        assert_eq!(
            parse_interval("1:99:00"),
            Err(IntervalLiteralError::FieldOverflow)
        );
    }

    #[test]
    fn sentinel_rejected() {
        assert!(!validate_interval(&INFINITY_SENTINEL));
        assert!(!validate_interval(&NEG_INFINITY_SENTINEL));
        assert!(validate_interval(&iv(0, 0, 0)));
        assert_eq!(
            parse_interval("2147483647 mons 2147483647 days 2562047788:00:54.775807"),
            Err(IntervalLiteralError::OutOfRange)
        );
    }

    #[test]
    fn format_parse_roundtrip_grid() {
        let ms = [0, 1, -1, 12, -12, 13, i32::MAX, i32::MIN + 1, i32::MIN];
        let ds = [0, 1, -1, 30, i32::MAX, i32::MIN];
        let us = [0, 1, -1, 3_600_000_000, -86_399_999_999, i64::MAX, i64::MIN];
        for &m in &ms {
            for &d in &ds {
                for &u in &us {
                    let v = iv(m, d, u);
                    if !validate_interval(&v) {
                        continue;
                    }
                    let text = format_interval(&v);
                    assert_eq!(parse_interval(&text), Ok(v), "text {text}");
                }
            }
        }
    }
}
