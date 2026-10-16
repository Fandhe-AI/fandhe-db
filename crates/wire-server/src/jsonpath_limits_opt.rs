//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-jsonpath-steps`・
//! `--max-jsonpath-depth` opt-in CLI 引数のパーサ（Issue #2184。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-25）。
//!
//! `group_by_limits_opt`（`--max-group-by-columns` 等）と同型の「プロセス起動時にのみ
//! 明示指定する注入点」。範囲（各ハード上限以下への引き下げのみ）の判定は engine 側の
//! 単一情報源 [`engine::jsonpath_limits`] の `validate_*` に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して
//! 渡すだけにする。各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、
//! 共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_jsonpath_limits` へ 1 回だけ注入する
//! （`docs/design/jsonpath-limits-cli.md`）。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::jsonpath_limits::{validate_max_eval_depth, validate_max_eval_steps, JsonPathLimits};

/// jsonpath 評価のステップ数上限を設定する CLI フラグ名。
pub const MAX_JSONPATH_STEPS_FLAG: &str = "--max-jsonpath-steps";

/// jsonpath 評価の再帰深さ上限を設定する CLI フラグ名。
pub const MAX_JSONPATH_DEPTH_FLAG: &str = "--max-jsonpath-depth";

/// 2 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonPathLimitsRawFlags<'a> {
    pub max_steps: Option<&'a str>,
    pub max_depth: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`group_by_limits_opt` と同じ方針。
/// `str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_flag(flag: &str, raw: &str) -> Result<u64, String> {
    parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))
}

/// 2 フラグの未パース値から [`JsonPathLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`JsonPathLimits::default`]＝ハード上限）、範囲外・不正値は `Err`
/// （fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: JsonPathLimitsRawFlags<'_>) -> Result<JsonPathLimits, String> {
    let base = JsonPathLimits::default();
    let mut max_steps = base.max_eval_steps();
    let mut max_depth = base.max_eval_depth();
    if let Some(r) = raw.max_steps {
        let v = parse_flag(MAX_JSONPATH_STEPS_FLAG, r)?;
        max_steps =
            validate_max_eval_steps(v).map_err(|e| format!("{MAX_JSONPATH_STEPS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_depth {
        let v = parse_flag(MAX_JSONPATH_DEPTH_FLAG, r)?;
        max_depth =
            validate_max_eval_depth(v).map_err(|e| format!("{MAX_JSONPATH_DEPTH_FLAG}: {e}"))?;
    }
    Ok(JsonPathLimits::new(max_steps, max_depth))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(steps: Option<&str>, depth: Option<&str>) -> Result<JsonPathLimits, String> {
        resolve(JsonPathLimitsRawFlags {
            max_steps: steps,
            max_depth: depth,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None), Ok(JsonPathLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("1"), Some("1")).expect("lower bound");
        assert_eq!((l.max_eval_steps(), l.max_eval_depth()), (1, 1));
        let l = one(Some("4000000"), Some("1024")).expect("upper bound");
        assert_eq!(l, JsonPathLimits::default());
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "4000001",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ] {
            let e = one(Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_JSONPATH_STEPS_FLAG), "{bad}: {e}");
        }
        for bad in ["0", "1025", "+5", "", "x", "99999999999999999999999"] {
            let e = one(None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_JSONPATH_DEPTH_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("8")).expect("depth only");
        assert_eq!(l.max_eval_depth(), 8);
        assert_eq!(
            l.max_eval_steps(),
            JsonPathLimits::default().max_eval_steps()
        );
    }
}
