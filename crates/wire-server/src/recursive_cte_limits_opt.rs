//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-recursive-cte-iterations`・
//! `--max-recursive-cte-rows` opt-in CLI 引数のパーサ（Issue #1871。
//! ポインタ: `docs/spec/04-behavior/` SQL-35 (3)・SQL-46）。
//!
//! `join_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲
//! （ハード上限以下への引き下げのみ）の判定は engine 側の単一情報源
//! [`engine::recursive_cte_limits`] の `validate_*` に委ね、本モジュールは untrusted な CLI 文字列を
//! 厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_recursive_cte_limits` へ 1 回だけ注入する。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::recursive_cte_limits::{
    validate_max_recursive_cte_iterations, validate_max_recursive_cte_rows, RecursiveCteLimits,
};

/// 再帰 CTE の反復回数上限を設定する CLI フラグ名。
pub const MAX_RECURSIVE_CTE_ITERATIONS_FLAG: &str = "--max-recursive-cte-iterations";

/// 再帰 CTE の累積行数上限を設定する CLI フラグ名。
pub const MAX_RECURSIVE_CTE_ROWS_FLAG: &str = "--max-recursive-cte-rows";

/// 2 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct RecursiveCteLimitsRawFlags<'a> {
    pub max_iterations: Option<&'a str>,
    pub max_rows: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<usize>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_flag(flag: &str, raw: &str) -> Result<usize, String> {
    parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))
}

/// 2 フラグの未パース値から [`RecursiveCteLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`RecursiveCteLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は
/// `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: RecursiveCteLimitsRawFlags<'_>) -> Result<RecursiveCteLimits, String> {
    let base = RecursiveCteLimits::default();
    let mut max_iterations = base.max_iterations();
    let mut max_rows = base.max_rows();
    if let Some(r) = raw.max_iterations {
        let v = parse_flag(MAX_RECURSIVE_CTE_ITERATIONS_FLAG, r)?;
        max_iterations = validate_max_recursive_cte_iterations(v)
            .map_err(|e| format!("{MAX_RECURSIVE_CTE_ITERATIONS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_rows {
        let v = parse_flag(MAX_RECURSIVE_CTE_ROWS_FLAG, r)?;
        max_rows = validate_max_recursive_cte_rows(v)
            .map_err(|e| format!("{MAX_RECURSIVE_CTE_ROWS_FLAG}: {e}"))?;
    }
    Ok(RecursiveCteLimits::new(max_iterations, max_rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(iter: Option<&str>, rows: Option<&str>) -> Result<RecursiveCteLimits, String> {
        resolve(RecursiveCteLimitsRawFlags {
            max_iterations: iter,
            max_rows: rows,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None), Ok(RecursiveCteLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("1"), Some("1")).expect("lower bound");
        assert_eq!((l.max_iterations(), l.max_rows()), (1, 1));
        let l = one(Some("1000"), Some("100000")).expect("upper bound");
        assert_eq!(l, RecursiveCteLimits::default());
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        let common = [
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
            "0",
        ];
        for bad in common.iter().chain(["1001"].iter()) {
            let e = one(Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_RECURSIVE_CTE_ITERATIONS_FLAG), "{bad}: {e}");
        }
        for bad in common.iter().chain(["100001"].iter()) {
            let e = one(None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_RECURSIVE_CTE_ROWS_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(Some("5"), None).expect("iterations only");
        assert_eq!(l.max_iterations(), 5);
        assert_eq!(l.max_rows(), RecursiveCteLimits::default().max_rows());
        let l = one(None, Some("7")).expect("rows only");
        assert_eq!(l.max_rows(), 7);
        assert_eq!(
            l.max_iterations(),
            RecursiveCteLimits::default().max_iterations()
        );
    }
}
