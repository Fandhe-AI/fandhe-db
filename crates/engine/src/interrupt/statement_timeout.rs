//! セッション変数 `statement_timeout` の値の扱い（Issue #1954。ポインタ: `docs/spec/04-behavior/`
//! SQL-71・SQL-46）。
//!
//! 役割: 範囲検証・`SET` の値文字列のパース・`SHOW` の表示の 3 つを engine 側の単一情報源にする。
//! wire-server の起動時 CLI（`statement_timeout_opt`）は数値へ厳密パースしたあと
//! [`validate_statement_timeout_ms`] を呼び、`sql::allowlist` の `SET` 構文解析は
//! [`parse_statement_timeout_text`] と同じ検証を通す。`SHOW` の整形は `core.rs` が
//! [`format_statement_timeout`] を呼ぶ。期限の発火自体は同モジュールの `deadline.rs`（#1951）が担う。

use std::time::Duration;

/// `statement_timeout` の上限（ミリ秒。PG の整数 GUC の上限と同じ）。他の定数を参照しない独立リテラル。
pub const MAX_STATEMENT_TIMEOUT_MS: u64 = 2_147_483_647;

/// `statement_timeout` の既定値（ミリ秒。`0` は無効＝現行挙動）。
pub const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 0;

/// `SET statement_timeout` が不正値のときの固定文言（入力値を含めない。`22023`）。
pub const INVALID_STATEMENT_TIMEOUT_DETAIL: &str =
    "invalid value for parameter \"statement_timeout\"";

/// [`validate_statement_timeout_ms`] が範囲外を拒否したときのエラー（Display は英語で値と範囲だけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementTimeoutOutOfRange {
    pub value: u64,
}

impl std::fmt::Display for StatementTimeoutOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range 0..={MAX_STATEMENT_TIMEOUT_MS}",
            self.value
        )
    }
}

impl std::error::Error for StatementTimeoutOutOfRange {}

/// ミリ秒値の範囲検証（`0..=MAX_STATEMENT_TIMEOUT_MS`）の単一情報源。範囲外は fail-closed で拒否する。
pub fn validate_statement_timeout_ms(
    value: u64,
) -> std::result::Result<Duration, StatementTimeoutOutOfRange> {
    if value <= MAX_STATEMENT_TIMEOUT_MS {
        Ok(Duration::from_millis(value))
    } else {
        Err(StatementTimeoutOutOfRange { value })
    }
}

/// `SET statement_timeout` の値文字列（`'5s'`・`'100'`・`'5 min'` 等）をミリ秒へ変換する。
/// 前後の空白可・整数部は ASCII 数字のみ・単位は `ms`／`s`／`min`／`h`／`d`（大文字小文字区別。
/// 単位なしはミリ秒）。小数・`us`・16 進・負数は `None`（受理を狭める側）。範囲検証は呼び出し側。
pub fn parse_statement_timeout_text(text: &str) -> Option<u64> {
    let s = text.trim();
    let digits_end = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    let (num, rest) = s.split_at(digits_end);
    if num.is_empty() {
        return None;
    }
    let value: u64 = num.parse().ok()?;
    let factor: u64 = match rest.trim_start() {
        "" | "ms" => 1,
        "s" => 1_000,
        "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    value.checked_mul(factor)
}

/// `SHOW statement_timeout` の表示。`0` は `0`、それ以外は割り切れる最大の単位（`d`→`h`→`min`→`s`→`ms`）。
pub fn format_statement_timeout(ms: u64) -> String {
    if ms == 0 {
        return "0".to_string();
    }
    for (factor, unit) in [
        (86_400_000u64, "d"),
        (3_600_000, "h"),
        (60_000, "min"),
        (1_000, "s"),
    ] {
        if ms.is_multiple_of(factor) {
            return format!("{}{unit}", ms / factor);
        }
    }
    format!("{ms}ms")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_units() {
        for (s, v) in [
            ("100", 100),
            ("5s", 5_000),
            ("5 s", 5_000),
            (" 1min ", 60_000),
            ("2h", 7_200_000),
            ("1d", 86_400_000),
            ("7ms", 7),
            ("0", 0),
        ] {
            assert_eq!(parse_statement_timeout_text(s), Some(v), "{s}");
        }
    }

    #[test]
    fn parse_rejects_malformed() {
        for s in [
            "1.5s", "1us", "1S", "1x", "", "0x10", "s", "-1", "+1", "１２",
        ] {
            assert_eq!(parse_statement_timeout_text(s), None, "{s}");
        }
        assert_eq!(parse_statement_timeout_text("99999999999999999999d"), None);
        assert_eq!(parse_statement_timeout_text("18446744073709551615d"), None);
    }

    #[test]
    fn format_picks_largest_exact_unit() {
        for (ms, s) in [
            (0, "0"),
            (1, "1ms"),
            (1500, "1500ms"),
            (5000, "5s"),
            (60_000, "1min"),
            (90_000, "90s"),
            (3_600_000, "1h"),
            (86_400_000, "1d"),
        ] {
            assert_eq!(format_statement_timeout(ms), s);
        }
    }

    #[test]
    fn validate_range() {
        assert_eq!(validate_statement_timeout_ms(0), Ok(Duration::ZERO));
        assert!(validate_statement_timeout_ms(MAX_STATEMENT_TIMEOUT_MS).is_ok());
        assert!(validate_statement_timeout_ms(MAX_STATEMENT_TIMEOUT_MS + 1).is_err());
    }
}
