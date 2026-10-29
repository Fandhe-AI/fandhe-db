//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-window-keys`
//! opt-in CLI 引数のパーサ（Issue #1900。ポインタ: `docs/spec/04-behavior/` SQL-46・SQL-64）。
//!
//! `nesting_limits_opt`（`--max-subquery-depth` 等）と同型の「プロセス起動時にのみ
//! 明示指定する注入点」。範囲（ハード上限以下への引き下げのみ）の判定は engine 側の
//! 単一情報源 [`engine::window_limits`] の `validate_max_window_keys` に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して
//! 渡すだけにする。各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、
//! 共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_window_limits` へ 1 回だけ注入する（`docs/design/window-limits-cli.md`）。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::window_limits::{validate_max_window_keys, WindowLimits};

/// 窓の `PARTITION BY`／`ORDER BY` キー数上限を設定する CLI フラグ名。
pub const MAX_WINDOW_KEYS_FLAG: &str = "--max-window-keys";

/// CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowLimitsRawFlags<'a> {
    pub max_window_keys: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// フラグの未パース値から [`WindowLimits`] を解決する純関数。未指定は既定
/// （[`WindowLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は `Err`
/// （fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: WindowLimitsRawFlags<'_>) -> Result<WindowLimits, String> {
    let Some(r) = raw.max_window_keys else {
        return Ok(WindowLimits::default());
    };
    let v = parse_strict_decimal(r).ok_or_else(|| {
        format!("{MAX_WINDOW_KEYS_FLAG} expects a non-negative integer, got {r:?}")
    })?;
    let v = validate_max_window_keys(v).map_err(|e| format!("{MAX_WINDOW_KEYS_FLAG}: {e}"))?;
    Ok(WindowLimits::new(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(v: Option<&str>) -> WindowLimitsRawFlags<'_> {
        WindowLimitsRawFlags { max_window_keys: v }
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(resolve(raw(None)), Ok(WindowLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        assert_eq!(resolve(raw(Some("1"))), Ok(WindowLimits::new(1)));
        assert_eq!(resolve(raw(Some("8"))), Ok(WindowLimits::default()));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "9",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ] {
            let e = resolve(raw(Some(bad))).expect_err(bad);
            assert!(e.contains(MAX_WINDOW_KEYS_FLAG), "{bad}: {e}");
        }
    }
}
