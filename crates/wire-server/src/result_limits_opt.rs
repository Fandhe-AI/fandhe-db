//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-result-rows` opt-in CLI
//! 引数のパーサ（Issue #1555。ポインタ: `docs/spec/04-behavior/` SQL-34・SQL-46）。
//!
//! `join_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲
//! （`1..=100000`。上げ下げ両方向）の判定は engine 側の単一情報源
//! [`engine::result_limits::validate_max_result_rows`] に委ね、本モジュールは untrusted な
//! CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_result_limits` へ 1 回だけ注入する（`docs/design/result-limits-cli.md`）。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。
//! 同じフラグは後続の結果行数上限（LIMIT 省略時の `54000`）も共有するため、フラグ名定数は
//! 本モジュールを唯一の定義とする。

use engine::result_limits::{validate_max_result_rows, ResultLimits};

/// 明示 `LIMIT` の範囲上限（結果行数上限）を設定する CLI フラグ名。
pub const MAX_RESULT_ROWS_FLAG: &str = "--max-result-rows";

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<usize>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// `--max-result-rows` の未パース値から [`ResultLimits`] を解決する純関数。未指定は既定値
/// （[`ResultLimits::default`]＝従来の 10,000＝現行挙動）、範囲外・不正値は `Err`
/// （fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: Option<&str>) -> Result<ResultLimits, String> {
    let Some(raw) = raw else {
        return Ok(ResultLimits::default());
    };
    let value = parse_strict_decimal(raw).ok_or_else(|| {
        format!("{MAX_RESULT_ROWS_FLAG} expects a non-negative integer, got {raw:?}")
    })?;
    let value =
        validate_max_result_rows(value).map_err(|e| format!("{MAX_RESULT_ROWS_FLAG}: {e}"))?;
    Ok(ResultLimits::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_is_default() {
        assert_eq!(resolve(None), Ok(ResultLimits::default()));
        assert_eq!(resolve(None).unwrap().max_result_rows(), 10_000);
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        assert_eq!(resolve(Some("1")).unwrap().max_result_rows(), 1);
        assert_eq!(resolve(Some("10000")).unwrap(), ResultLimits::default());
        assert_eq!(resolve(Some("100000")).unwrap().max_result_rows(), 100_000);
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "100001",
            "",
            "+10",
            " 10",
            "10 ",
            "１０",
            "abc",
            "-1",
            "99999999999999999999999",
        ] {
            let e = resolve(Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_RESULT_ROWS_FLAG), "{bad}: {e}");
        }
    }
}
