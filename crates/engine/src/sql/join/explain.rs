//! JOIN の `EXPLAIN` 用の段の木の組み立て（Issue #2046・SQL-75 (2) ポインタ）。
//!
//! 呼び出し元は `core.rs` の `Statement::Explain` アーム（`run_join_explain`）。`prepare_join` で
//! 行を読まずに束縛した [`PreparedJoin`] と、側ごとの走査分類の行（単一表の広域取得
//! `EXPLAIN` と同じ `scalar_plan`／`access_path`）を受け取り、`sql::explain_tree` の
//! [`ExplainNode`] を返す純粋関数だけを置く。行・ロック・台帳・世代・キャッシュには触れない。
//!
//! 出力語彙は閉じている（`join_method`・`join_type`・`join_keys`・`join_filter`・`relation`・
//! `residual_predicates`・`group_keys`・`aggregates`・`having`・`sort_keys`・`limit`・`offset`）。
//! 表名・別名・LIMIT／OFFSET の値・行数・見積もりは出さない（SQL-75 (5)。テナント存在情報と
//! 利用者リテラルの露出を避ける）。結合方式は実行器と同じ `StepPlan::method` で決める。

use super::plan::{JoinMethod, Shape};
use super::PreparedJoin;
use crate::sql::explain_tree::{present, ExplainNode, ExplainTreeError};

/// 結合の段の木を組み立てる。外側から `limit` → `sort` → `aggregate` → `filter` → `join` 連鎖 →
/// `scan`（実行順の逆）。`side_lines.len()` が relation 数と違うときは fail-closed
/// （[`ExplainTreeError::InvalidLine`]）。left-deep の連鎖は再帰せずループで畳む。
pub(crate) fn explain_tree(
    prepared: &PreparedJoin<'_>,
    side_lines: Vec<Vec<String>>,
) -> Result<ExplainNode, ExplainTreeError> {
    let plan = &prepared.plan;
    if side_lines.len() != prepared.side_count() || plan.steps.len() + 1 != side_lines.len() {
        return Err(ExplainTreeError::InvalidLine);
    }
    let mut scans = Vec::with_capacity(side_lines.len());
    for (i, lines) in side_lines.into_iter().enumerate() {
        let mut all = Vec::with_capacity(lines.len().saturating_add(1));
        all.push(format!("relation: {}", i.saturating_add(1)));
        all.extend(lines);
        scans.push(ExplainNode::try_stage("scan".to_string(), all)?);
    }
    let mut it = scans.into_iter();
    let mut current = it.next().ok_or(ExplainTreeError::InvalidLine)?;
    for (step, right) in plan.steps.iter().zip(it) {
        let join_type = match (step.preserve_acc, step.preserve_new) {
            (false, false) => "inner",
            (true, false) => "left",
            (false, true) => "right",
            (true, true) => "full",
        };
        let (method_token, keys) = match step.method() {
            JoinMethod::Hash => ("hash", step.keys.len()),
            JoinMethod::NestedLoop => ("nested_loop", 0),
            JoinMethod::Cross => ("cross", 0),
        };
        let mut node = ExplainNode::try_stage(
            "join".to_string(),
            vec![
                format!("join_method: {method_token}"),
                format!("join_type: {join_type}"),
                format!("join_keys: {keys}"),
                format!("join_filter: {}", present(step.has_on_residual())),
            ],
        )?;
        node.push_child(current)?;
        node.push_child(right)?;
        current = node;
    }
    if !plan.residual.is_empty() {
        let mut node = ExplainNode::try_stage(
            "filter".to_string(),
            vec![format!("residual_predicates: {}", plan.residual.len())],
        )?;
        node.push_child(current)?;
        current = node;
    }
    let sort_keys = match &plan.shape {
        Shape::Aggregate(agg) => {
            let mut node = ExplainNode::try_stage(
                "aggregate".to_string(),
                vec![
                    format!("group_keys: {}", agg.keys.len()),
                    format!("aggregates: {}", agg.items.len()),
                    format!("having: {}", present(!agg.having.is_empty())),
                ],
            )?;
            node.push_child(current)?;
            current = node;
            agg.order.len()
        }
        Shape::Plain { order, .. } => order.len(),
    };
    if sort_keys > 0 {
        let mut node =
            ExplainNode::try_stage("sort".to_string(), vec![format!("sort_keys: {sort_keys}")])?;
        node.push_child(current)?;
        current = node;
    }
    if prepared.limit.is_some() || prepared.offset != 0 {
        let mut node = ExplainNode::try_stage(
            "limit".to_string(),
            vec![
                format!("limit: {}", present(prepared.limit.is_some())),
                format!("offset: {}", present(prepared.offset != 0)),
            ],
        )?;
        node.push_child(current)?;
        current = node;
    }
    Ok(current)
}
