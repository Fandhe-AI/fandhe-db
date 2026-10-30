//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-trigger-depth`
//! opt-in CLI 引数のパーサ（Issue #1727。ポインタ: `docs/spec/04-behavior/` TABLE-45 (5)・SQL-46）。
//!
//! `nesting_limits_opt`（`--max-subquery-depth` 等）と同型の「プロセス起動時にのみ
//! 明示指定する注入点」。範囲（ハード上限以下への引き下げのみ）の判定は engine 側の
//! 単一情報源 [`engine::trigger_limits`] の `validate_max_trigger_depth` に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して
//! 渡すだけにする。各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、
//! 共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_trigger_limits` へ 1 回だけ注入する（`docs/design/trigger-depth-limit-cli.md`）。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::trigger_limits::{validate_max_trigger_depth, TriggerLimits};

/// トリガ呼び出しの入れ子深さ上限を設定する CLI フラグ名。
pub const MAX_TRIGGER_DEPTH_FLAG: &str = "--max-trigger-depth";

/// CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct TriggerLimitsRawFlags<'a> {
    pub max_trigger_depth: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// フラグの未パース値から [`TriggerLimits`] を解決する純関数。未指定は既定
/// （[`TriggerLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は `Err`
/// （fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: TriggerLimitsRawFlags<'_>) -> Result<TriggerLimits, String> {
    let Some(r) = raw.max_trigger_depth else {
        return Ok(TriggerLimits::default());
    };
    let v = parse_strict_decimal(r).ok_or_else(|| {
        format!("{MAX_TRIGGER_DEPTH_FLAG} expects a non-negative integer, got {r:?}")
    })?;
    let v = validate_max_trigger_depth(v).map_err(|e| format!("{MAX_TRIGGER_DEPTH_FLAG}: {e}"))?;
    Ok(TriggerLimits::new(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(v: Option<&str>) -> TriggerLimitsRawFlags<'_> {
        TriggerLimitsRawFlags {
            max_trigger_depth: v,
        }
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(resolve(raw(None)), Ok(TriggerLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        assert_eq!(resolve(raw(Some("1"))), Ok(TriggerLimits::new(1)));
        assert_eq!(resolve(raw(Some("16"))), Ok(TriggerLimits::default()));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "17",
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
            assert!(e.contains(MAX_TRIGGER_DEPTH_FLAG), "{bad}: {e}");
        }
    }
}
