//! 述語形 `UPDATE`／`DELETE` の `EXPLAIN`（Issue #2050・SQL-75 (1)(2)(3)(5)(6) のポインタ。親: #2049）。
//!
//! 責務境界: 本モジュールは「文の形」から段の木を組む純粋関数と、構文段が `ParsedSql` に載せる
//! 受理済みの文 [`ExplainDml`] だけを担う。束縛・権限・実行段の結線は `core/explain_dml.rs`
//! （`EngineCore::parse_explain_dml`・`run_explain_dml`）が行い、`EXPLAIN` は行・台帳
//! （operation_id）・writer・シーケンス・トリガのいずれにも触れない（SQL-75 (3)）。
//!
//! 走査の分類を `plain_scan`／`full_scan` に固定する理由: DML 実行器の候補列挙
//! （`core/tenant.rs` の `enumerate_dml_candidates_with`）は PK 点検索・GIN・スカラー索引を使わず、
//! 自テナントの物理キー範囲を常に全走査して行ごとに述語を評価する。SELECT 用の分類関数を流用すると
//! 実行器の実経路と矛盾する出力になる（D5「矛盾出力の防止」）ため、語彙と行の組み立て関数
//! （[`crate::sql::explain::relational_explain_lines`]）だけを SELECT と共有する。
//! **実行器が索引を使うよう変わるときは、この分類も一緒に変えること。**
//!
//! 出力は閉じた語彙のトークンと、文の文面だけから決まる有無・個数（`assignments` の列数）に限り、
//! 値・表名・operation_id の値・行数・可視件数は載せない（SQL-75 (5)・RLS-10）。

use crate::sql::allowlist::{ValidatedPredicateDelete, ValidatedPredicateUpdate};
use crate::sql::explain_tree::{present, ExplainNode, ExplainTreeError};

/// `EXPLAIN` の対象として受理済みの述語形 DML。`ParsedSql::ExplainDml` が保持する
/// （クレート内の構文段が生成し、実行段が束縛して木を組む）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ExplainDml {
    /// `EXPLAIN DELETE FROM <t> [WHERE ...] [RETURNING ...] [USING OPERATION_ID ...]`。
    Delete(ValidatedPredicateDelete),
    /// `EXPLAIN UPDATE <t> SET ... [WHERE ...] [RETURNING ...] [USING OPERATION_ID ...]`。
    Update(ValidatedPredicateUpdate),
}

/// 木の組み立てに必要な、文の形の要約（値を持たない）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExplainDmlShape {
    /// `Some(n)` なら UPDATE で `SET` の列数 n、`None` なら DELETE。
    pub(crate) assignments: Option<usize>,
    /// 束縛後に WHERE のフィルタが残るか（`WHERE TRUE` 畳み込み・省略は `false`）。
    pub(crate) has_filter: bool,
    /// `RETURNING` の有無。
    pub(crate) has_returning: bool,
    /// `USING OPERATION_ID` の有無（値は出さない）。
    pub(crate) has_operation_id: bool,
}

/// 段の木を組む。根は `delete`／`update`、子は全走査の `scan` 1 節（単一表のため `relation` 行は無し）。
/// 上限・形式違反は [`ExplainTreeError`]（呼び出し元が `into_surface_error` で写す）。
pub(crate) fn explain_tree(shape: &ExplainDmlShape) -> Result<ExplainNode, ExplainTreeError> {
    let stage = if shape.assignments.is_some() {
        "update"
    } else {
        "delete"
    };
    let mut lines = Vec::new();
    if let Some(n) = shape.assignments {
        lines.push(format!("assignments: {n}"));
    }
    lines.push(format!("filter: {}", present(shape.has_filter)));
    lines.push(format!("returning: {}", present(shape.has_returning)));
    lines.push(format!("operation_id: {}", present(shape.has_operation_id)));
    let mut root = ExplainNode::try_stage(stage.to_string(), lines)?;
    let scan = ExplainNode::try_stage(
        "scan".to_string(),
        crate::sql::explain::relational_explain_lines(
            crate::sql::scalar_plan::ScalarPlan::PlainScan,
            None,
            crate::sql::explain::AccessPath::FullScan,
            &crate::sql::explain::ExplainIndexNames::default(),
            &[],
        ),
    )?;
    root.push_child(scan)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_tree_lines() {
        let node = explain_tree(&ExplainDmlShape {
            assignments: None,
            has_filter: true,
            has_returning: false,
            has_operation_id: true,
        })
        .unwrap();
        assert_eq!(
            node.into_lines(),
            vec![
                "delete",
                "filter: present",
                "returning: none",
                "operation_id: present",
                "-> scan",
                "   scalar_plan: plain_scan",
                "   access_path: full_scan",
            ]
        );
    }

    #[test]
    fn update_tree_lines() {
        let node = explain_tree(&ExplainDmlShape {
            assignments: Some(2),
            has_filter: false,
            has_returning: true,
            has_operation_id: false,
        })
        .unwrap();
        assert_eq!(
            node.into_lines(),
            vec![
                "update",
                "assignments: 2",
                "filter: none",
                "returning: present",
                "operation_id: none",
                "-> scan",
                "   scalar_plan: plain_scan",
                "   access_path: full_scan",
            ]
        );
    }
}
