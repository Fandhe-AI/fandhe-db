//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-join-relations`・
//! `--max-join-input-rows`・`--max-join-output-rows` opt-in CLI 引数のパーサ
//! （Issue #1551。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `group_by_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲
//! （各ハード上限以下への引き下げのみ）の判定は engine 側の単一情報源
//! [`engine::join_limits`] の `validate_*` に委ね、本モジュールは untrusted な CLI 文字列を
//! 厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。各モジュールが
//! 独立した小さい下請けを持つ既存の流儀を踏襲し、共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_join_limits` へ 1 回だけ注入する（`docs/design/join-limits-cli.md`）。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::join_limits::{
    validate_max_join_input_rows, validate_max_join_output_rows, validate_max_join_relations,
    JoinSizeLimits,
};

/// JOIN のリレーション数上限を設定する CLI フラグ名。
pub const MAX_JOIN_RELATIONS_FLAG: &str = "--max-join-relations";

/// JOIN 1 辺の入力行数上限を設定する CLI フラグ名。
pub const MAX_JOIN_INPUT_ROWS_FLAG: &str = "--max-join-input-rows";

/// JOIN の出力行数上限を設定する CLI フラグ名。
pub const MAX_JOIN_OUTPUT_ROWS_FLAG: &str = "--max-join-output-rows";

/// 3 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct JoinLimitsRawFlags<'a> {
    pub max_relations: Option<&'a str>,
    pub max_input_rows: Option<&'a str>,
    pub max_output_rows: Option<&'a str>,
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

/// 3 フラグの未パース値から [`JoinSizeLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`JoinSizeLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は
/// `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: JoinLimitsRawFlags<'_>) -> Result<JoinSizeLimits, String> {
    let base = JoinSizeLimits::default();
    let mut max_relations = base.max_relations();
    let mut max_input_rows = base.max_input_rows();
    let mut max_output_rows = base.max_output_rows();
    if let Some(r) = raw.max_relations {
        let v = parse_flag(MAX_JOIN_RELATIONS_FLAG, r)?;
        max_relations = validate_max_join_relations(v)
            .map_err(|e| format!("{MAX_JOIN_RELATIONS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_input_rows {
        let v = parse_flag(MAX_JOIN_INPUT_ROWS_FLAG, r)?;
        max_input_rows = validate_max_join_input_rows(v)
            .map_err(|e| format!("{MAX_JOIN_INPUT_ROWS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_output_rows {
        let v = parse_flag(MAX_JOIN_OUTPUT_ROWS_FLAG, r)?;
        max_output_rows = validate_max_join_output_rows(v)
            .map_err(|e| format!("{MAX_JOIN_OUTPUT_ROWS_FLAG}: {e}"))?;
    }
    Ok(JoinSizeLimits::new(
        max_relations,
        max_input_rows,
        max_output_rows,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(
        rel: Option<&str>,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<JoinSizeLimits, String> {
        resolve(JoinLimitsRawFlags {
            max_relations: rel,
            max_input_rows: input,
            max_output_rows: output,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None, None), Ok(JoinSizeLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("2"), Some("1"), Some("1")).expect("lower bound");
        assert_eq!(
            (l.max_relations(), l.max_input_rows(), l.max_output_rows()),
            (2, 1, 1)
        );
        let l = one(Some("8"), Some("100000"), Some("100000")).expect("upper bound");
        assert_eq!(l, JoinSizeLimits::default());
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
        ];
        for bad in common.iter().chain(["0", "1", "9"].iter()) {
            let e = one(Some(bad), None, None).expect_err(bad);
            assert!(e.contains(MAX_JOIN_RELATIONS_FLAG), "{bad}: {e}");
        }
        for bad in common.iter().chain(["0", "100001"].iter()) {
            let e = one(None, Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_JOIN_INPUT_ROWS_FLAG), "{bad}: {e}");
            let e = one(None, None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_JOIN_OUTPUT_ROWS_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("5"), None).expect("input only");
        assert_eq!(l.max_input_rows(), 5);
        assert_eq!(l.max_relations(), JoinSizeLimits::default().max_relations());
        assert_eq!(
            l.max_output_rows(),
            JoinSizeLimits::default().max_output_rows()
        );
    }
}
