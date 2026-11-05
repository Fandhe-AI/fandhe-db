//! `EXPLAIN INSERT` の検証済み文と段の木の組み立て（Issue #2051・SQL-75 (2)(3)(5)(6) ポインタ）。
//!
//! 呼び出し元は `core.rs` の `ParsedSql::ExplainInsert` の解析腕（[`ExplainInsertTarget`] を作る）と、
//! `core/insert_explain_exec.rs` の実行本体（[`insert_node`] で `insert` 段の木を組む）。権限ゲート
//! （`sql::privilege_gate::write`）は [`ExplainInsertTarget`] を通常の `INSERT` と同じビットで分類する
//! （`Statement::Explain` に載せると INSERT 権限を課せないため、独立した variant にしている）。
//!
//! 出力語彙は閉じた集合（`insert`・`select`・`result`、`insert_source`・`insert_method`・
//! `on_conflict`・`conflict_target`・`conflict_filter`・`returning`・`operation_id`）。行数・件数・
//! 表名・列名・制約名・シーケンス名・リテラルは出さない（SQL-75 (5)。テナント存在情報の露出を避ける）。
//! 行・ロック・台帳・シーケンス・トリガー・埋め込み I/O には触れない純粋関数だけを置く。

use crate::sql::allowlist::{ExplainTarget, ValidatedInsert, ValidatedInsertSelect};
use crate::sql::explain_tree::{present, ExplainNode, ExplainTreeError};

/// `INSERT ... SELECT` の内側 SELECT の `EXPLAIN` 対象。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum InsertSelectExplainInner {
    /// `EXPLAIN SELECT` と同じ検証で作った内側の対象。
    Target(Box<ExplainTarget>),
    /// `FROM` なしの SELECT（定数行）。
    Constant,
}

/// `EXPLAIN INSERT` の検証済みの対象。実行はせず、束縛までを行って段の木を返す。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ExplainInsertTarget {
    /// 表への `VALUES`／`DEFAULT VALUES`／ファイル形（`ON CONFLICT`・`RETURNING` を含みうる）。
    Values(Box<ValidatedInsert>),
    /// `INSERT ... SELECT`。`inner` は内側 SELECT の `EXPLAIN` 対象。
    Select {
        /// 検証済みの `INSERT ... SELECT` 本体。
        insert: Box<ValidatedInsertSelect>,
        /// 内側 SELECT の `EXPLAIN` 対象。
        inner: InsertSelectExplainInner,
    },
}

impl ExplainInsertTarget {
    /// 挿入先の表名（権限ゲートと束縛が使う。出力には出さない）。
    pub fn table_name(&self) -> &str {
        match self {
            Self::Values(v) => &v.table_name,
            Self::Select { insert, .. } => insert.table_name(),
        }
    }
}

/// `insert` 段の属性（束縛結果から決めた静的な分類だけ）。
pub(crate) struct InsertShape {
    /// `values`／`default_values`／`select`。
    pub(crate) source: &'static str,
    /// `row`／`row_batch`／`file`／`upsert`（VALUES 系だけ）。
    pub(crate) method: Option<&'static str>,
    /// `ON CONFLICT` の静的な形。
    pub(crate) conflict: Option<ConflictShape>,
    pub(crate) returning: bool,
    pub(crate) operation_id: bool,
}

/// `ON CONFLICT` の静的な形。
pub(crate) struct ConflictShape {
    /// `do_nothing`／`do_update`。
    pub(crate) action: &'static str,
    /// `row_id`／`unique`／`omitted`。
    pub(crate) target: &'static str,
    /// `DO UPDATE ... WHERE` の有無（`do_update` のときだけ意味を持つ）。
    pub(crate) filter: bool,
}

/// `VALUES` 系の子の葉（`result`。`source: values`）。
pub(crate) fn values_result_leaf() -> Result<ExplainNode, ExplainTreeError> {
    ExplainNode::try_stage("result".to_string(), vec!["source: values".to_string()])
}

/// `FROM` なし SELECT の子の葉（`result`。`source: constant`）。
pub(crate) fn constant_result_leaf() -> Result<ExplainNode, ExplainTreeError> {
    crate::sql::cte_explain::const_result_leaf()
}

/// `select` 段（属性なし）で内側の計画を包む。単一表の計画は見出しなしの葉なので、そのまま
/// `insert` の子にすると `insert` の属性行と紛れる。内側は描画済みの行を葉として載せる。
pub(crate) fn select_stage(inner: ExplainNode) -> Result<ExplainNode, ExplainTreeError> {
    let mut node = ExplainNode::try_stage("select".to_string(), Vec::new())?;
    node.push_child(inner)?;
    Ok(node)
}

/// `insert` 段の木を組む。属性行は固定順（`insert_source`・`insert_method`・`on_conflict`・
/// `conflict_target`・`conflict_filter`・`returning`・`operation_id`）。
pub(crate) fn insert_node(
    shape: &InsertShape,
    child: ExplainNode,
) -> Result<ExplainNode, ExplainTreeError> {
    let mut lines = vec![format!("insert_source: {}", shape.source)];
    if let Some(m) = shape.method {
        lines.push(format!("insert_method: {m}"));
    }
    match &shape.conflict {
        None => lines.push("on_conflict: none".to_string()),
        Some(c) => {
            lines.push(format!("on_conflict: {}", c.action));
            lines.push(format!("conflict_target: {}", c.target));
            if c.action == "do_update" {
                lines.push(format!("conflict_filter: {}", present(c.filter)));
            }
        }
    }
    lines.push(format!("returning: {}", present(shape.returning)));
    lines.push(format!("operation_id: {}", present(shape.operation_id)));
    let mut node = ExplainNode::try_stage("insert".to_string(), lines)?;
    node.push_child(child)?;
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_row_renders_fixed_vocabulary() {
        let shape = InsertShape {
            source: "values",
            method: Some("row"),
            conflict: None,
            returning: false,
            operation_id: true,
        };
        let node = insert_node(&shape, values_result_leaf().unwrap()).unwrap();
        assert_eq!(
            node.into_lines(),
            [
                "insert",
                "insert_source: values",
                "insert_method: row",
                "on_conflict: none",
                "returning: none",
                "operation_id: present",
                "-> result",
                "   source: values",
            ]
        );
    }

    #[test]
    fn upsert_do_update_reports_filter() {
        let shape = InsertShape {
            source: "values",
            method: Some("upsert"),
            conflict: Some(ConflictShape {
                action: "do_update",
                target: "unique",
                filter: true,
            }),
            returning: true,
            operation_id: false,
        };
        let lines = insert_node(&shape, values_result_leaf().unwrap())
            .unwrap()
            .into_lines();
        assert!(lines.contains(&"on_conflict: do_update".to_string()));
        assert!(lines.contains(&"conflict_target: unique".to_string()));
        assert!(lines.contains(&"conflict_filter: present".to_string()));
        assert!(lines.contains(&"returning: present".to_string()));
    }

    #[test]
    fn select_source_nests_inner_under_select_stage() {
        let shape = InsertShape {
            source: "select",
            method: None,
            conflict: None,
            returning: false,
            operation_id: false,
        };
        let inner = ExplainNode::leaf(vec!["scalar_plan: plain_scan".to_string()]);
        let node = insert_node(&shape, select_stage(inner).unwrap()).unwrap();
        let lines = node.into_lines();
        assert_eq!(lines.first().map(String::as_str), Some("insert"));
        assert!(lines.contains(&"-> select".to_string()));
        assert!(lines.iter().any(|l| l.ends_with("scalar_plan: plain_scan")));
        assert!(!lines.iter().any(|l| l.starts_with("insert_method")));
    }
}
