//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-correlated-reevaluations`
//! opt-in CLI 引数のパーサ（Issue #1847。ポインタ: `docs/spec/04-behavior/` SQL-61）。
//!
//! `join_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲（ハード上限以下への
//! 引き下げのみ）の判定は engine 側の単一情報源 [`engine::correlated_limits`] の
//! [`validate_max_correlated_reevaluations`] に委ね、本モジュールは untrusted な CLI 文字列を
//! 厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。各モジュールが
//! 独立した小さい下請けを持つ既存の流儀を踏襲し、共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_correlated_limits` へ 1 回だけ注入する。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::correlated_limits::{validate_max_correlated_reevaluations, CorrelatedLimits};

/// 相関サブクエリの再評価回数上限を設定する CLI フラグ名。
pub const MAX_CORRELATED_REEVALUATIONS_FLAG: &str = "--max-correlated-reevaluations";

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<usize>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// フラグの未パース値から [`CorrelatedLimits`] を解決する純関数。未指定は既定値
/// （[`CorrelatedLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は `Err`（fail-closed）。
/// エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: Option<&str>) -> Result<CorrelatedLimits, String> {
    let Some(raw) = raw else {
        return Ok(CorrelatedLimits::default());
    };
    let value = parse_strict_decimal(raw).ok_or_else(|| {
        format!("{MAX_CORRELATED_REEVALUATIONS_FLAG} expects a non-negative integer, got {raw:?}")
    })?;
    let checked = validate_max_correlated_reevaluations(value)
        .map_err(|e| format!("{MAX_CORRELATED_REEVALUATIONS_FLAG}: {e}"))?;
    Ok(CorrelatedLimits::new(checked))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_is_default() {
        assert_eq!(resolve(None), Ok(CorrelatedLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        assert_eq!(resolve(Some("1")).expect("lower").max_reevaluations(), 1);
        assert_eq!(resolve(Some("100000")), Ok(CorrelatedLimits::default()));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "100001",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ] {
            let e = resolve(Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_CORRELATED_REEVALUATIONS_FLAG), "{bad}: {e}");
        }
    }
}
