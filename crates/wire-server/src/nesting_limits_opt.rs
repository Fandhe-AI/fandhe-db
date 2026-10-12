//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-subquery-depth`・
//! `--max-subquery-executions`・`--max-cte-definitions`・`--max-cte-nesting-depth`
//! opt-in CLI 引数のパーサ（Issue #1552。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `group_by_limits_opt`（`--max-group-by-columns` 等）と同型の「プロセス起動時にのみ
//! 明示指定する注入点」。範囲（各ハード上限以下への引き下げのみ）の判定は engine 側の
//! 単一情報源 [`engine::nesting_limits`] の `validate_*` に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して
//! 渡すだけにする。各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、
//! 共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_nesting_limits` へ 1 回だけ注入する
//! （`docs/design/nesting-limits-cli.md`）。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::nesting_limits::{
    validate_max_cte_definitions, validate_max_cte_nesting_depth, validate_max_subquery_depth,
    validate_max_subquery_executions, NestingLimits,
};

/// サブクエリ入れ子深さの上限を設定する CLI フラグ名。
pub const MAX_SUBQUERY_DEPTH_FLAG: &str = "--max-subquery-depth";

/// 1 文あたりの内側サブクエリ実行回数の上限を設定する CLI フラグ名。
pub const MAX_SUBQUERY_EXECUTIONS_FLAG: &str = "--max-subquery-executions";

/// CTE 定義数の上限を設定する CLI フラグ名。
pub const MAX_CTE_DEFINITIONS_FLAG: &str = "--max-cte-definitions";

/// CTE 連鎖深さの上限を設定する CLI フラグ名。
pub const MAX_CTE_NESTING_DEPTH_FLAG: &str = "--max-cte-nesting-depth";

/// 4 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct NestingLimitsRawFlags<'a> {
    pub max_subquery_depth: Option<&'a str>,
    pub max_subquery_executions: Option<&'a str>,
    pub max_cte_definitions: Option<&'a str>,
    pub max_cte_nesting_depth: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`group_by_limits_opt` と同じ方針。
/// `str::parse::<usize>` が受理する先頭 `+` 等を弾く）。
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

/// 4 フラグの未パース値から [`NestingLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`NestingLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は
/// `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: NestingLimitsRawFlags<'_>) -> Result<NestingLimits, String> {
    let base = NestingLimits::default();
    let mut depth = base.max_subquery_depth();
    let mut executions = base.max_subquery_executions();
    let mut definitions = base.max_cte_definitions();
    let mut nesting = base.max_cte_nesting_depth();
    if let Some(r) = raw.max_subquery_depth {
        let v = parse_flag(MAX_SUBQUERY_DEPTH_FLAG, r)?;
        depth = validate_max_subquery_depth(v)
            .map_err(|e| format!("{MAX_SUBQUERY_DEPTH_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_subquery_executions {
        let v = parse_flag(MAX_SUBQUERY_EXECUTIONS_FLAG, r)?;
        executions = validate_max_subquery_executions(v)
            .map_err(|e| format!("{MAX_SUBQUERY_EXECUTIONS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_cte_definitions {
        let v = parse_flag(MAX_CTE_DEFINITIONS_FLAG, r)?;
        definitions = validate_max_cte_definitions(v)
            .map_err(|e| format!("{MAX_CTE_DEFINITIONS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_cte_nesting_depth {
        let v = parse_flag(MAX_CTE_NESTING_DEPTH_FLAG, r)?;
        nesting = validate_max_cte_nesting_depth(v)
            .map_err(|e| format!("{MAX_CTE_NESTING_DEPTH_FLAG}: {e}"))?;
    }
    Ok(NestingLimits::new(depth, executions, definitions, nesting))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw<'a>(
        depth: Option<&'a str>,
        executions: Option<&'a str>,
        definitions: Option<&'a str>,
        nesting: Option<&'a str>,
    ) -> NestingLimitsRawFlags<'a> {
        NestingLimitsRawFlags {
            max_subquery_depth: depth,
            max_subquery_executions: executions,
            max_cte_definitions: definitions,
            max_cte_nesting_depth: nesting,
        }
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(
            resolve(NestingLimitsRawFlags::default()),
            Ok(NestingLimits::default())
        );
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = resolve(raw(Some("1"), Some("1"), Some("1"), Some("1"))).expect("lower bound");
        assert_eq!(l, NestingLimits::new(1, 1, 1, 1));
        let l = resolve(raw(Some("4"), Some("16"), Some("16"), Some("4"))).expect("upper bound");
        assert_eq!(l, NestingLimits::default());
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        let malformed = [
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ];
        type Build = for<'a> fn(&'a str) -> NestingLimitsRawFlags<'a>;
        let cases: [(&str, &str, Build); 4] = [
            (MAX_SUBQUERY_DEPTH_FLAG, "5", |v| {
                raw(Some(v), None, None, None)
            }),
            (MAX_SUBQUERY_EXECUTIONS_FLAG, "17", |v| {
                raw(None, Some(v), None, None)
            }),
            (MAX_CTE_DEFINITIONS_FLAG, "17", |v| {
                raw(None, None, Some(v), None)
            }),
            (MAX_CTE_NESTING_DEPTH_FLAG, "5", |v| {
                raw(None, None, None, Some(v))
            }),
        ];
        for (flag, over, build) in cases {
            for bad in malformed.iter().copied().chain(["0", over]) {
                let e = resolve(build(bad)).expect_err(bad);
                assert!(e.contains(flag), "{flag} {bad}: {e}");
            }
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = resolve(raw(None, Some("3"), None, None)).expect("executions only");
        assert_eq!(l.max_subquery_executions(), 3);
        let d = NestingLimits::default();
        assert_eq!(l.max_subquery_depth(), d.max_subquery_depth());
        assert_eq!(l.max_cte_definitions(), d.max_cte_definitions());
        assert_eq!(l.max_cte_nesting_depth(), d.max_cte_nesting_depth());
    }
}
