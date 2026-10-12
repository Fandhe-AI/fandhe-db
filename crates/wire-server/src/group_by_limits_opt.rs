//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-group-by-columns`・
//! `--max-groups` opt-in CLI 引数のパーサ（Issue #1550。ポインタ:
//! `docs/spec/04-behavior/` SQL-46）。
//!
//! `schema_limits_opt`（`--max-table-columns` 等）と同型の「プロセス起動時にのみ
//! 明示指定する注入点」。範囲（各ハード上限以下への引き下げのみ）の判定は engine 側の
//! 単一情報源 [`engine::group_by_limits`] の `validate_*` に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して
//! 渡すだけにする。各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、
//! 共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_group_by_limits` へ 1 回だけ注入する
//! （`docs/design/group-by-limits-cli.md`）。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::group_by_limits::{validate_max_group_by_columns, validate_max_groups, GroupByLimits};

/// `GROUP BY` キー列数の上限を設定する CLI フラグ名。
pub const MAX_GROUP_BY_COLUMNS_FLAG: &str = "--max-group-by-columns";

/// 1 クエリが生成できるグループ数の上限を設定する CLI フラグ名。
pub const MAX_GROUPS_FLAG: &str = "--max-groups";

/// 2 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupByLimitsRawFlags<'a> {
    pub max_group_by_columns: Option<&'a str>,
    pub max_groups: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`schema_limits_opt` と同じ方針。
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

/// 2 フラグの未パース値から [`GroupByLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`GroupByLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は
/// `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: GroupByLimitsRawFlags<'_>) -> Result<GroupByLimits, String> {
    let base = GroupByLimits::default();
    let mut max_group_by_columns = base.max_group_by_columns();
    let mut max_groups = base.max_groups();
    if let Some(r) = raw.max_group_by_columns {
        let v = parse_flag(MAX_GROUP_BY_COLUMNS_FLAG, r)?;
        max_group_by_columns = validate_max_group_by_columns(v)
            .map_err(|e| format!("{MAX_GROUP_BY_COLUMNS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_groups {
        let v = parse_flag(MAX_GROUPS_FLAG, r)?;
        max_groups = validate_max_groups(v).map_err(|e| format!("{MAX_GROUPS_FLAG}: {e}"))?;
    }
    Ok(GroupByLimits::new(max_group_by_columns, max_groups))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(cols: Option<&str>, groups: Option<&str>) -> Result<GroupByLimits, String> {
        resolve(GroupByLimitsRawFlags {
            max_group_by_columns: cols,
            max_groups: groups,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None), Ok(GroupByLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("1"), Some("1")).expect("lower bound");
        assert_eq!((l.max_group_by_columns(), l.max_groups()), (1, 1));
        let l = one(Some("8"), Some("10000")).expect("upper bound");
        assert_eq!(l, GroupByLimits::default());
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
            let e = one(Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_GROUP_BY_COLUMNS_FLAG), "{bad}: {e}");
        }
        for bad in ["0", "10001", "+5", "", "x"] {
            let e = one(None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_GROUPS_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("8")).expect("groups only");
        assert_eq!(l.max_groups(), 8);
        assert_eq!(
            l.max_group_by_columns(),
            GroupByLimits::default().max_group_by_columns()
        );
    }
}
