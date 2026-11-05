//! `EXPLAIN ANALYZE <SELECT>` の運搬型・実行文への写像・計画への実測値の併記
//! （SQL-76 (2)(3) のポインタ。Issue #2054。設計記録は `docs/design/explain-search-engine-exposure.md`）。
//!
//! 役割分担:
//!
//! - [`ExplainAnalyzeSelect`] は `core::ParsedSql::ExplainAnalyze` が保持する検証済みの値で、静的な計画の
//!   対象（[`ExplainTarget`]）と `TIMING`／`SUMMARY` の解決結果を運ぶ。構築は `core::parse_tokens_in`
//!   （オプション解決の後）だけが行う。
//! - [`executable_statement`] は計画の対象から「同じ本文を `EXPLAIN` なしで検証したときの文」を再構成する。
//!   計画と実行の文がずれないことは単体テストと結合テストで固定する。
//! - [`annotate`] は描画済みの静的な計画（`QUERY PLAN` 1 列）へ、実行結果の行数と経過時間を併記する。
//!   呼び出し元は `core/explain_analyze_exec.rs`（計画の生成と文の実行を担う）。
//!
//! テナント境界（SQL-76 (3)）: 併記する `actual_rows` は通常の SELECT の実行器が返した結果の行数
//! （RLS を暗黙適用した後の可視行・LIMIT／OFFSET 適用後）に限る。走査した行数・フィルタで除いた行数・
//! 推定値・他テナントを含む件数は出さない。経過時間は壁時計の値で、`TIMING OFF` で抑止できる。
//!
//! 出力の語彙（安定契約）: 根の段の属性行の末尾に `actual_rows`／`actual_loops`／（`TIMING` が真なら）
//! `actual_total_time_ms`、計画の最後に（`SUMMARY` が真なら）`planning_time_ms`／`execution_time_ms`。

use crate::sql::allowlist::{CteExplainMain, ExplainTarget, SqlSurfaceError, Statement};
use crate::sql::exec::{Cell, QueryResult, ResultRow};
use crate::sql::explain_tree::MAX_EXPLAIN_TREE_LINES;

/// 併記する行数の最大（根の段へ最大 3 行 + 末尾へ最大 2 行）。上限判定の余裕に使う。
const ANNOTATION_LINES: usize = 5;

/// 実行して実測値を併記する `EXPLAIN ANALYZE` の検証済み対象。
#[derive(Debug, Clone, PartialEq)]
pub struct ExplainAnalyzeSelect {
    pub(crate) target: ExplainTarget,
    pub(crate) timing: bool,
    pub(crate) summary: bool,
}

/// 実行で得た実測値（行数と経過時間。単位はミリ秒の小数）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AnalyzeStats {
    pub(crate) rows: usize,
    pub(crate) planning_ms: f64,
    pub(crate) execution_ms: f64,
}

/// 計画の対象を、同じ本文を `EXPLAIN` なしで検証したときの [`Statement`] へ写す。
/// `core::EngineCore::run_explain_analyze` が実行段へ渡す文を作る。
pub(crate) fn executable_statement(target: &ExplainTarget) -> Statement {
    match target {
        ExplainTarget::Search(v) => Statement::Select(v.clone()),
        ExplainTarget::Aggregate(v) => Statement::Aggregate(v.clone()),
        ExplainTarget::Scan(v) => Statement::Scan(v.clone()),
        ExplainTarget::Join(v) => Statement::Join(v.clone()),
        ExplainTarget::SetOperation(v) => Statement::SetOperation(v.clone()),
        ExplainTarget::Cte(v) => match &v.main {
            CteExplainMain::Scan(s) => Statement::Scan(s.clone()),
            CteExplainMain::Aggregate(a) => Statement::Aggregate(a.clone()),
            CteExplainMain::Buffered(b) => Statement::BufferedView((**b).clone()),
        },
    }
}

fn ms(value: f64) -> String {
    // 非有限・負の値は 0 に倒す（クライアントが数として parse できる形を保つ）。
    let v = if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    };
    format!("{v:.3}")
}

fn text_row(text: String) -> ResultRow {
    ResultRow {
        id: 0,
        score: 0.0,
        cells: vec![Cell::Text(text)],
    }
}

/// 根の段の行が終わる位置（最初の子の段の直前）。
///
/// `sql::explain_tree` の字下げ規則（安定契約）に依存する: 深さ 0 の行には前置がなく、深さ 1 以上の行は
/// `"-> "` か空白で始まる。したがって先頭から「どちらでも始まらない行」が続く範囲が根の段である。
pub(crate) fn root_end(rows: &[ResultRow]) -> usize {
    rows.iter()
        .position(|r| match r.cells.first() {
            Some(Cell::Text(t)) => t.starts_with("-> ") || t.starts_with(' '),
            _ => true,
        })
        .unwrap_or(rows.len())
}

fn too_many_lines() -> SqlSurfaceError {
    SqlSurfaceError::StatementTooComplex {
        detail: "EXPLAIN plan has too many lines",
    }
}

/// 静的な計画へ実測値を併記する。行数が上限を超える場合は `54001`（固定文）で fail-closed にする。
pub(crate) fn annotate(
    plan: QueryResult,
    stats: &AnalyzeStats,
    timing: bool,
    summary: bool,
) -> Result<QueryResult, SqlSurfaceError> {
    let QueryResult { columns, mut rows } = plan;
    let limit = MAX_EXPLAIN_TREE_LINES
        .checked_add(ANNOTATION_LINES)
        .ok_or_else(too_many_lines)?;
    if rows.len() > limit {
        return Err(too_many_lines());
    }
    let mut extra = vec![
        text_row(format!("actual_rows: {}", stats.rows)),
        text_row("actual_loops: 1".to_string()),
    ];
    if timing {
        extra.push(text_row(format!(
            "actual_total_time_ms: {}",
            ms(stats.execution_ms)
        )));
    }
    let at = root_end(&rows);
    let tail = rows.split_off(at);
    rows.extend(extra);
    rows.extend(tail);
    if summary {
        rows.push(text_row(format!(
            "planning_time_ms: {}",
            ms(stats.planning_ms)
        )));
        rows.push(text_row(format!(
            "execution_time_ms: {}",
            ms(stats.execution_ms)
        )));
    }
    if rows.len() > limit {
        return Err(too_many_lines());
    }
    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::explain::lines_to_query_result;

    fn lines(q: &QueryResult) -> Vec<String> {
        q.rows
            .iter()
            .map(|r| match r.cells.first() {
                Some(Cell::Text(t)) => t.clone(),
                _ => String::new(),
            })
            .collect()
    }

    fn plan(ls: &[&str]) -> QueryResult {
        lines_to_query_result(ls.iter().map(|s| s.to_string()).collect())
    }

    const STATS: AnalyzeStats = AnalyzeStats {
        rows: 7,
        planning_ms: 0.5,
        execution_ms: 1.25,
    };

    /// SQL-76 (2): 見出しなしの根は属性行の末尾へ挿入する。
    #[test]
    fn inserts_after_headless_root() {
        let out = annotate(plan(&["a: 1", "b: 2"]), &STATS, false, false).unwrap();
        assert_eq!(
            lines(&out),
            ["a: 1", "b: 2", "actual_rows: 7", "actual_loops: 1"]
        );
    }

    /// SQL-76 (2): 子の段（見出し付き・見出しなし）の直前に挿入し、子の行は動かさない。
    #[test]
    fn inserts_before_first_child() {
        let p = plan(&["root", "k: v", "-> child", "   x: 1", "   -> grand"]);
        let out = annotate(p, &STATS, true, true).unwrap();
        assert_eq!(
            lines(&out),
            [
                "root",
                "k: v",
                "actual_rows: 7",
                "actual_loops: 1",
                "actual_total_time_ms: 1.250",
                "-> child",
                "   x: 1",
                "   -> grand",
                "planning_time_ms: 0.500",
                "execution_time_ms: 1.250",
            ]
        );
        let p = plan(&["root", "   headless: 1"]);
        let out = annotate(p, &STATS, false, false).unwrap();
        assert_eq!(
            lines(&out),
            [
                "root",
                "actual_rows: 7",
                "actual_loops: 1",
                "   headless: 1"
            ]
        );
    }

    /// SQL-76 (2): TIMING／SUMMARY の 4 通り。
    #[test]
    fn timing_and_summary_combinations() {
        for (timing, summary, n) in [
            (false, false, 3),
            (true, false, 4),
            (false, true, 5),
            (true, true, 6),
        ] {
            let out = annotate(plan(&["a: 1"]), &STATS, timing, summary).unwrap();
            assert_eq!(out.rows.len(), n, "{timing} {summary}");
        }
    }

    /// 非有限・負の時間は 0.000 に倒す。
    #[test]
    fn non_finite_time_is_zero() {
        let s = AnalyzeStats {
            rows: 0,
            planning_ms: f64::NAN,
            execution_ms: -1.0,
        };
        let out = annotate(plan(&["a: 1"]), &s, true, true).unwrap();
        assert!(lines(&out).contains(&"planning_time_ms: 0.000".to_string()));
    }

    /// 行数の上限超過は 54001 で fail-closed。
    #[test]
    fn too_many_lines_is_rejected() {
        let big = QueryResult {
            columns: Vec::new(),
            rows: (0..MAX_EXPLAIN_TREE_LINES + ANNOTATION_LINES)
                .map(|_| text_row("a: 1".to_string()))
                .collect(),
        };
        assert_eq!(
            annotate(big, &STATS, true, true).unwrap_err().wire_code(),
            "54001"
        );
    }
}
