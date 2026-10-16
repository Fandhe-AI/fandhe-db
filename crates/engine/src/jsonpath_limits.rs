//! jsonpath 評価の「ステップ数上限」「評価の再帰深さ上限」の起動時設定（Issue #2184。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25）。
//!
//! `wire-server` の `jsonpath_limits_opt` が CLI フラグ（`--max-jsonpath-steps`・
//! `--max-jsonpath-depth`）を厳密パースし、本モジュールの `validate_*` で範囲検証したうえで
//! [`JsonPathLimits`] を組み立て、`EngineCore::with_jsonpath_limits` へ 1 回だけ注入する
//! （`group_by_limits`・`schema_limits` と同じ流儀）。評価器
//! （[`crate::json::jsonpath::jsonpath_query`] ほか）は呼び出し側（SQL 表層の `jsonb_path_*`
//! 等。Issue #2185 以降）が `EngineCore::jsonpath_limits` から取り出して
//! `JsonPathEvalOptions` へ渡す。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/jsonpath-limits-cli.md`）:
//!
//! - **ハード上限が既定値**。設定できるのは「下げる方向」だけで、フラグ未指定ならハード
//!   上限（＝評価器の安全弁そのもの）で動く。引き上げは対象外（fail-closed）。
//! - 「ステップ」は評価した AST ノード 1 回・アクセサが訪問／生成した項目 1 件・比較の
//!   組 1 組・値の複製ノード 1 個ごとに 1 と数える。「深さ」は評価器の再帰（式の入れ子・
//!   アクセサ連鎖・フィルタ内の再評価）の入れ子数で、解析器が拒否する静的な式の高さ
//!   （`MAX_JSONPATH_NODE_HEIGHT`）とは別の実行時の値である。

use crate::json::jsonpath::MAX_JSONPATH_NODE_HEIGHT;

/// 評価ステップ数のハード上限（既定値）。`MAX_JSONB_CONTAINMENT_STEPS` と同じ桁で、
/// 病的な組合せ爆発（`$.**.**.**`・`$[*] ? (@ == $[*])` 等）を有界時間で止める。
pub const MAX_JSONPATH_EVAL_STEPS: u64 = 4_000_000;

/// 評価の再帰深さのハード上限（既定値）。評価器の再帰は 1 段あたりのスタック消費が
/// 小さくなるよう設計しているが、解析器の式の高さ上限（`MAX_JSONPATH_NODE_HEIGHT`）より
/// 大きくはしない。
pub const MAX_JSONPATH_EVAL_DEPTH: usize = MAX_JSONPATH_NODE_HEIGHT;

/// 2 上限に設定できる最小値。
pub const MIN_JSONPATH_LIMIT: u64 = 1;

/// jsonpath 評価 2 上限の設定値。フィールドは非公開で、[`JsonPathLimits::new`] は
/// ハード上限で飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonPathLimits {
    max_eval_steps: u64,
    max_eval_depth: usize,
}

impl Default for JsonPathLimits {
    fn default() -> Self {
        Self {
            max_eval_steps: MAX_JSONPATH_EVAL_STEPS,
            max_eval_depth: MAX_JSONPATH_EVAL_DEPTH,
        }
    }
}

impl JsonPathLimits {
    /// 検証済みの 2 値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_eval_steps: u64, max_eval_depth: usize) -> Self {
        Self {
            max_eval_steps: max_eval_steps.clamp(MIN_JSONPATH_LIMIT, MAX_JSONPATH_EVAL_STEPS),
            max_eval_depth: max_eval_depth.clamp(1, MAX_JSONPATH_EVAL_DEPTH),
        }
    }

    /// 1 回の評価が消費できるステップ数の上限。
    pub fn max_eval_steps(&self) -> u64 {
        self.max_eval_steps
    }

    /// 評価の再帰深さの上限。
    pub fn max_eval_depth(&self) -> usize {
        self.max_eval_depth
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct JsonPathLimitOutOfRange {
    value: u64,
    max: u64,
}

impl std::fmt::Display for JsonPathLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for JsonPathLimitOutOfRange {}

fn validate_in(value: u64, max: u64) -> Result<u64, JsonPathLimitOutOfRange> {
    if (MIN_JSONPATH_LIMIT..=max).contains(&value) {
        Ok(value)
    } else {
        Err(JsonPathLimitOutOfRange { value, max })
    }
}

/// ステップ数上限の設定値を `1..=MAX_JSONPATH_EVAL_STEPS` で検証する単一情報源。
pub fn validate_max_eval_steps(value: u64) -> Result<u64, JsonPathLimitOutOfRange> {
    validate_in(value, MAX_JSONPATH_EVAL_STEPS)
}

/// 深さ上限の設定値を `1..=MAX_JSONPATH_EVAL_DEPTH` で検証する単一情報源。
pub fn validate_max_eval_depth(value: u64) -> Result<usize, JsonPathLimitOutOfRange> {
    let max = u64::try_from(MAX_JSONPATH_EVAL_DEPTH).unwrap_or(u64::MAX);
    let v = validate_in(value, max)?;
    // 検証済み（max 以下）のため変換は失敗しない。失敗時も範囲外として扱う（fail-closed）。
    usize::try_from(v).map_err(|_| JsonPathLimitOutOfRange { value, max })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = JsonPathLimits::default();
        assert_eq!(d.max_eval_steps(), MAX_JSONPATH_EVAL_STEPS);
        assert_eq!(d.max_eval_depth(), MAX_JSONPATH_EVAL_DEPTH);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_eval_steps(1), Ok(1));
        assert_eq!(
            validate_max_eval_steps(MAX_JSONPATH_EVAL_STEPS),
            Ok(MAX_JSONPATH_EVAL_STEPS)
        );
        assert!(validate_max_eval_steps(0).is_err());
        assert!(validate_max_eval_steps(MAX_JSONPATH_EVAL_STEPS + 1).is_err());
        assert_eq!(validate_max_eval_depth(1), Ok(1));
        assert_eq!(
            validate_max_eval_depth(MAX_JSONPATH_EVAL_DEPTH as u64),
            Ok(MAX_JSONPATH_EVAL_DEPTH)
        );
        assert!(validate_max_eval_depth(0).is_err());
        assert!(validate_max_eval_depth(MAX_JSONPATH_EVAL_DEPTH as u64 + 1).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_eval_steps(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=4000000");
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        assert_eq!(
            JsonPathLimits::new(u64::MAX, usize::MAX),
            JsonPathLimits::default()
        );
        let l = JsonPathLimits::new(0, 0);
        assert_eq!((l.max_eval_steps(), l.max_eval_depth()), (1, 1));
    }
}
