//! 集合演算（`UNION`／`INTERSECT`／`EXCEPT`）の `EXPLAIN` 用の段の木の組み立て
//! （Issue #2047・SQL-75 (2) ポインタ）。
//!
//! 呼び出し元は `core.rs` の `Statement::Explain` アーム（`run_set_operation_explain`）。
//! `sql::set_op::prepare` で行を読まずに束縛した葉（[`BoundSetLeaf`]）と、葉ごとの走査分類の行を
//! 受け取り、`sql::explain_tree` の [`ExplainNode`] を返す純粋関数だけを置く。行・ロック・台帳・
//! 世代・キャッシュには触れない。
//!
//! 出力語彙は JOIN の `EXPLAIN`（`sql::join::explain`）と同じ閉じた集合
//! （`set_operation`・`set_operator`・`scan`・`relation`・`aggregate`・`group_keys`・`aggregates`・
//! `having`・`sort`・`sort_keys`・`limit`・`offset`）。表名・別名・LIMIT／OFFSET の値・行数は
//! 出さない（SQL-75 (5)。テナント存在情報と利用者リテラルの露出を避ける）。

use crate::sql::allowlist::{SetOperator, SetTree, ValidatedSetOperation};
use crate::sql::explain_tree::{present, ExplainNode, ExplainTreeError};
use crate::sql::set_op::{BoundSetLeaf, LeafShape};

/// 集合演算の段の木を組み立てる。外側から全体 `limit`（あるときだけ）→ `set_operation` の入れ子 →
/// 葉（`scan`、枝内 `limit`／`sort`／`aggregate` 付き）。`leaves` と `leaf_lines` の長さが木の葉の数と
/// 違うときは fail-closed（[`ExplainTreeError::InvalidLine`]）。再帰の深さは構文段の上限
/// （枝 16・括弧 4）で抑えられる。
pub(crate) fn explain_tree(
    validated: &ValidatedSetOperation,
    leaves: &[BoundSetLeaf],
    leaf_lines: Vec<Vec<String>>,
) -> Result<ExplainNode, ExplainTreeError> {
    if leaves.len() != leaf_lines.len() || leaves.len() != count_leaves(&validated.tree) {
        return Err(ExplainTreeError::InvalidLine);
    }
    let mut lines = leaf_lines.into_iter();
    let mut shapes = leaves.iter().map(|l| &l.shape);
    let mut ordinal = 0usize;
    let mut current = build(&validated.tree, &mut shapes, &mut lines, &mut ordinal)?;
    if validated.limit.is_some() || validated.offset != 0 {
        let mut node = ExplainNode::try_stage(
            "limit".to_string(),
            vec![
                format!("limit: {}", present(validated.limit.is_some())),
                format!("offset: {}", present(validated.offset != 0)),
            ],
        )?;
        node.push_child(current)?;
        current = node;
    }
    Ok(current)
}

fn count_leaves(tree: &SetTree) -> usize {
    match tree {
        SetTree::Op { left, right, .. } => count_leaves(left).saturating_add(count_leaves(right)),
        _ => 1,
    }
}

fn operator_token(op: SetOperator) -> &'static str {
    match op {
        SetOperator::Union => "union",
        SetOperator::UnionAll => "union_all",
        SetOperator::Intersect => "intersect",
        SetOperator::Except => "except",
    }
}

fn build<'a>(
    tree: &SetTree,
    shapes: &mut impl Iterator<Item = &'a LeafShape>,
    lines: &mut impl Iterator<Item = Vec<String>>,
    ordinal: &mut usize,
) -> Result<ExplainNode, ExplainTreeError> {
    match tree {
        SetTree::Op { op, left, right } => {
            let mut node = ExplainNode::try_stage(
                "set_operation".to_string(),
                vec![format!("set_operator: {}", operator_token(*op))],
            )?;
            node.push_child(build(left, shapes, lines, ordinal)?)?;
            node.push_child(build(right, shapes, lines, ordinal)?)?;
            Ok(node)
        }
        SetTree::Branch(_) | SetTree::LimitedBranch(_) | SetTree::AggregateBranch(_) => {
            let shape = shapes.next().ok_or(ExplainTreeError::InvalidLine)?;
            let scan_lines = lines.next().ok_or(ExplainTreeError::InvalidLine)?;
            *ordinal = ordinal.saturating_add(1);
            leaf(shape, scan_lines, *ordinal)
        }
    }
}

/// 葉 1 枚分。外側から `limit` → `sort` → `aggregate` → `scan`（実行順の逆）。
fn leaf(
    shape: &LeafShape,
    scan_lines: Vec<String>,
    ordinal: usize,
) -> Result<ExplainNode, ExplainTreeError> {
    let mut all = Vec::with_capacity(scan_lines.len().saturating_add(1));
    all.push(format!("relation: {ordinal}"));
    all.extend(scan_lines);
    let mut current = ExplainNode::try_stage("scan".to_string(), all)?;
    if let Some(agg) = &shape.aggregate {
        let mut node = ExplainNode::try_stage(
            "aggregate".to_string(),
            vec![
                format!("group_keys: {}", agg.group_keys),
                format!("aggregates: {}", agg.aggregates),
                format!("having: {}", present(agg.having)),
            ],
        )?;
        node.push_child(current)?;
        current = node;
    }
    if shape.limited && shape.sort_keys > 0 {
        let mut node = ExplainNode::try_stage(
            "sort".to_string(),
            vec![format!("sort_keys: {}", shape.sort_keys)],
        )?;
        node.push_child(current)?;
        current = node;
    }
    if shape.limited && (shape.has_limit || shape.has_offset) {
        let mut node = ExplainNode::try_stage(
            "limit".to_string(),
            vec![
                format!("limit: {}", present(shape.has_limit)),
                format!("offset: {}", present(shape.has_offset)),
            ],
        )?;
        node.push_child(current)?;
        current = node;
    }
    Ok(current)
}
