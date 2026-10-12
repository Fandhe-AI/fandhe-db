//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--write-lock-wait-ms` opt-in
//! CLI 引数のパーサ（Issue #1554。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `join_limits_opt`・`dml_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。
//! 範囲（下限・ハード上限）の判定は engine 側の単一情報源
//! [`engine::storage::validate_write_lock_wait_ms`] に委ね、本モジュールは untrusted な
//! CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_write_lock_wait` へ 1 回だけ注入する
//! （`docs/design/write-lock-wait-cli.md`）。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::storage::{validate_write_lock_wait_ms, DEFAULT_WRITE_LOCK_WAIT};

/// 書き込みロック待機上限（ミリ秒）を設定する CLI フラグ名。
pub const FLAG: &str = "--write-lock-wait-ms";

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// 未パース値から待機上限を解決する純関数。未指定は既定
/// （[`DEFAULT_WRITE_LOCK_WAIT`]＝現行挙動）、範囲外・不正値は `Err`（fail-closed）。
/// エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: Option<&str>) -> Result<std::time::Duration, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_WRITE_LOCK_WAIT);
    };
    let v = parse_strict_decimal(raw)
        .ok_or_else(|| format!("{FLAG} expects a non-negative integer, got {raw:?}"))?;
    validate_write_lock_wait_ms(v).map_err(|e| format!("{FLAG}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unspecified_is_default() {
        assert_eq!(resolve(None), Ok(DEFAULT_WRITE_LOCK_WAIT));
    }

    #[test]
    fn accepts_boundaries() {
        assert_eq!(resolve(Some("100")), Ok(Duration::from_millis(100)));
        assert_eq!(resolve(Some("60000")), Ok(Duration::from_secs(60)));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "99",
            "60001",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "abc",
            "99999999999999999999999",
        ] {
            let e = resolve(Some(bad)).expect_err(bad);
            assert!(e.contains(FLAG), "{bad}: {e}");
        }
    }
}
