//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-functions` opt-in CLI 引数の
//! パーサ（SQL-66 (5)・Issue #1922。SQL-46 の `temp_table_limits_opt` と同型）。
//!
//! 範囲（ハード上限以下への引き下げのみ）の判定は engine 側の単一情報源
//! [`engine::function_limits`] の `validate_max_functions` に委ね、本モジュールは untrusted な
//! CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_function_limits` へ 1 回だけ注入する。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::function_limits::{validate_max_functions, FunctionLimits};

/// UDF 数の上限を設定する CLI フラグ名。
pub const MAX_FUNCTIONS_FLAG: &str = "--max-functions";

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// フラグの未パース値から [`FunctionLimits`] を解決する純関数。未指定は既定値
/// （ハード上限＝現行挙動）、範囲外・不正値は `Err`（fail-closed）。エラー文にはフラグ名・
/// 入力値・範囲だけを含める。
pub fn resolve(raw: Option<&str>) -> Result<FunctionLimits, String> {
    let Some(r) = raw else {
        return Ok(FunctionLimits::default());
    };
    let v = parse_strict_decimal(r)
        .ok_or_else(|| format!("{MAX_FUNCTIONS_FLAG} expects a non-negative integer, got {r:?}"))?;
    let n = validate_max_functions(v).map_err(|e| format!("{MAX_FUNCTIONS_FLAG}: {e}"))?;
    Ok(FunctionLimits::new(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_is_default() {
        assert_eq!(resolve(None), Ok(FunctionLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        assert_eq!(resolve(Some("1")).expect("lower").max_functions(), 1);
        assert_eq!(resolve(Some("64")), Ok(FunctionLimits::default()));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "65",
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
            assert!(e.contains(MAX_FUNCTIONS_FLAG), "{bad}: {e}");
        }
    }
}
