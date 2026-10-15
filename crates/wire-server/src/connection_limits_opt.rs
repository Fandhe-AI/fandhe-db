//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る接続資源上限の opt-in CLI 引数
//! （`--read-timeout-secs`・`--max-connections`・`--max-prepared-statements`）のパーサ
//! （Issue #2454。ポインタ: `docs/spec/04-behavior/` WIRE-25。対象 WIRE-5・WIRE-6・WIRE-11）。
//!
//! `write_lock_wait_opt`・`transaction_limits_opt` と同型の「プロセス起動時にのみ明示指定する
//! 注入点」。範囲（下限・ハード上限）は [`crate::limits`] の `validate_*` に一本化し、
//! 本モジュールは untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を
//! 弾く）して委譲するだけにする。値は SQL 表層・NoSQL（HTTP）表層で共有される
//! （`ConnectionLimiter` と読み取りタイムアウトは `main.rs` が表層分岐の前に確定する）。
//!
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。
//! 書き込みロック待機（`--write-lock-wait-ms`・SQL-46）とは独立で、互いに参照しない。
//! フラグ名・範囲は CLI 設計メモ（`docs/design/connection-limits-cli.md`。#2453）の確定値。

use std::time::Duration;

use crate::limits::{
    validate_max_connections, validate_max_prepared_statements, validate_read_timeout_secs,
    SessionLimits, MAX_CONNECTIONS, READ_TIMEOUT,
};

/// 読み取りタイムアウト（秒）を設定する CLI フラグ名。
pub const READ_TIMEOUT_FLAG: &str = "--read-timeout-secs";
/// 同時接続数上限を設定する CLI フラグ名。
pub const MAX_CONNECTIONS_FLAG: &str = "--max-connections";
/// 接続あたり prepared statement 件数上限を設定する CLI フラグ名。
pub const MAX_PREPARED_STATEMENTS_FLAG: &str = "--max-prepared-statements";

/// 未パースの CLI 値（`main.rs` が収集した生文字列）。
#[derive(Debug, Default, Clone, Copy)]
pub struct ConnectionLimitsRawFlags<'a> {
    pub read_timeout_secs: Option<&'a str>,
    pub max_connections: Option<&'a str>,
    pub max_prepared_statements: Option<&'a str>,
}

/// 解決済みの接続資源上限。`main.rs` が accept ループへ注入する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    pub read_timeout: Duration,
    pub max_connections: usize,
    pub session: SessionLimits,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            read_timeout: READ_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
            session: SessionLimits::default(),
        }
    }
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_flag<T>(
    flag: &str,
    raw: &str,
    validate: impl FnOnce(u64) -> Result<T, String>,
) -> Result<T, String> {
    let v = parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))?;
    validate(v).map_err(|e| format!("{flag}: {e}"))
}

/// 未パース値から上限を解決する純関数。未指定の項目は既定（現行挙動）、
/// 範囲外・不正値は `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: ConnectionLimitsRawFlags<'_>) -> Result<ConnectionLimits, String> {
    let mut out = ConnectionLimits::default();
    if let Some(r) = raw.read_timeout_secs {
        out.read_timeout = parse_flag(READ_TIMEOUT_FLAG, r, validate_read_timeout_secs)?;
    }
    if let Some(r) = raw.max_connections {
        out.max_connections = parse_flag(MAX_CONNECTIONS_FLAG, r, validate_max_connections)?;
    }
    if let Some(r) = raw.max_prepared_statements {
        out.session.max_prepared_statements = parse_flag(
            MAX_PREPARED_STATEMENTS_FLAG,
            r,
            validate_max_prepared_statements,
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::MAX_PREPARED_STATEMENTS_PER_SESSION;

    fn raw1(which: usize, v: &str) -> ConnectionLimitsRawFlags<'_> {
        let mut r = ConnectionLimitsRawFlags::default();
        match which {
            0 => r.read_timeout_secs = Some(v),
            1 => r.max_connections = Some(v),
            _ => r.max_prepared_statements = Some(v),
        }
        r
    }

    #[test]
    fn unspecified_is_default() {
        let l = resolve(ConnectionLimitsRawFlags::default()).expect("default");
        assert_eq!(l.read_timeout, READ_TIMEOUT);
        assert_eq!(l.max_connections, MAX_CONNECTIONS);
        assert_eq!(
            l.session.max_prepared_statements,
            MAX_PREPARED_STATEMENTS_PER_SESSION
        );
        assert_eq!(l, ConnectionLimits::default());
    }

    #[test]
    fn accepts_boundaries() {
        assert_eq!(
            resolve(raw1(0, "1")).expect("min").read_timeout,
            Duration::from_secs(1)
        );
        assert_eq!(
            resolve(raw1(0, "600")).expect("max").read_timeout,
            Duration::from_secs(600)
        );
        assert_eq!(resolve(raw1(1, "1")).expect("min").max_connections, 1);
        assert_eq!(resolve(raw1(1, "256")).expect("max").max_connections, 256);
        assert_eq!(
            resolve(raw1(2, "1"))
                .expect("min")
                .session
                .max_prepared_statements,
            1
        );
        assert_eq!(
            resolve(raw1(2, "1024"))
                .expect("max")
                .session
                .max_prepared_statements,
            1024
        );
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        let flags = [
            READ_TIMEOUT_FLAG,
            MAX_CONNECTIONS_FLAG,
            MAX_PREPARED_STATEMENTS_FLAG,
        ];
        let common = [
            "0",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "abc",
            "99999999999999999999999",
            "18446744073709551615",
        ];
        for (idx, flag) in flags.iter().enumerate() {
            for bad in common {
                let e = resolve(raw1(idx, bad)).expect_err(bad);
                assert!(e.contains(flag), "{flag} {bad}: {e}");
            }
        }
        for (idx, bad) in [(0, "601"), (1, "257"), (2, "1025")] {
            let e = resolve(raw1(idx, bad)).expect_err(bad);
            assert!(e.contains(flags[idx]), "{bad}: {e}");
        }
    }

    #[test]
    fn flags_are_resolved_independently() {
        let l = resolve(ConnectionLimitsRawFlags {
            read_timeout_secs: Some("5"),
            max_connections: None,
            max_prepared_statements: Some("8"),
        })
        .expect("mixed");
        assert_eq!(l.read_timeout, Duration::from_secs(5));
        assert_eq!(l.max_connections, MAX_CONNECTIONS);
        assert_eq!(l.session.max_prepared_statements, 8);
    }
}
