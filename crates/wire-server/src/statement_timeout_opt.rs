//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--statement-timeout-ms` opt-in
//! CLI 引数のパーサ（Issue #1954。ポインタ: `docs/spec/04-behavior/` SQL-71・SQL-46）。
//!
//! `write_lock_wait_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲（`0`〜
//! `2147483647` ミリ秒。`0` は無効）の判定は engine 側の単一情報源
//! [`engine::interrupt::validate_statement_timeout_ms`] に委ね、本モジュールは untrusted な
//! CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_default_statement_timeout` へ 1 回だけ注入する。各セッションは認証成功後に
//! この値を既定値（`RESET` の戻り先を兼ねる）として取り込み、`SET statement_timeout` で上書きできる。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::interrupt::{validate_statement_timeout_ms, DEFAULT_STATEMENT_TIMEOUT_MS};

/// 全セッションの `statement_timeout` 既定値（ミリ秒）を設定する CLI フラグ名。
pub const FLAG: &str = "--statement-timeout-ms";

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// 未パース値から既定の `statement_timeout` を解決する純関数。未指定は `0`（無効＝現行挙動）、
/// 範囲外・不正値は `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: Option<&str>) -> Result<std::time::Duration, String> {
    let Some(raw) = raw else {
        return Ok(std::time::Duration::from_millis(
            DEFAULT_STATEMENT_TIMEOUT_MS,
        ));
    };
    let v = parse_strict_decimal(raw)
        .ok_or_else(|| format!("{FLAG} expects a non-negative integer, got {raw:?}"))?;
    validate_statement_timeout_ms(v).map_err(|e| format!("{FLAG}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unspecified_is_disabled() {
        assert_eq!(resolve(None), Ok(Duration::ZERO));
    }

    #[test]
    fn accepts_boundaries() {
        assert_eq!(resolve(Some("0")), Ok(Duration::ZERO));
        assert_eq!(resolve(Some("1500")), Ok(Duration::from_millis(1500)));
        assert_eq!(
            resolve(Some("2147483647")),
            Ok(Duration::from_millis(2_147_483_647))
        );
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "2147483648",
            "-1",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "abc",
            "99999999999999999999999",
        ] {
            let e = resolve(Some(bad)).expect_err(bad);
            assert!(e.contains(FLAG), "{bad}: {e}");
        }
    }
}
