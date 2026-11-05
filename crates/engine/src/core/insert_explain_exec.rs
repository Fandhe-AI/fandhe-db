//! `EXPLAIN INSERT` の実行本体（Issue #2051・SQL-75 (2)(3)(5)(6) ポインタ）。
//!
//! 呼び出し元は `core.rs` の `ParsedSql::ExplainInsert` の実行腕（autocommit・明示トランザクション）。
//! 通常の `INSERT`／`INSERT ... SELECT` と同じ束縛（スキーマ取得・行数上限・列・型・`ON CONFLICT` の対象・
//! `RETURNING`・バッチ上限）までを同じ関数で行い、束縛エラーの `wire_code` と判定順を一致させる。
//! その先の `execute_*`（行の書き込み・writer・台帳・採番・トリガー・埋め込み I/O）は呼ばない。
//! 出力は `sql::insert_explain` の閉じた語彙だけで、行数・表名・列名・制約名は含まない。

use super::EngineCore;
use crate::catalog::CatalogError;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{OnConflictAction, OnConflictClause, SqlSurfaceError, Statement};
use crate::sql::exec::Cell;
use crate::sql::explain_tree::ExplainNode;
use crate::sql::insert_explain::{
    constant_result_leaf, insert_node, select_stage, values_result_leaf, ConflictShape,
    ExplainInsertTarget, InsertSelectExplainInner, InsertShape,
};
use crate::sql::mode::SessionState;
use crate::sql::parser::{BoundConflictTarget, BoundInsertForm};
use crate::sql::SqlOutcome;

thread_local! {
    /// `INSERT ... SELECT` の EXPLAIN が内側の計画生成の間だけ張る結果行数上限の上書き。
    static EXPLAIN_LIMITS_OVERRIDE: std::cell::Cell<Option<crate::result_limits::ResultLimits>> =
        const { std::cell::Cell::new(None) };
}

/// [`EXPLAIN_LIMITS_OVERRIDE`] を張る RAII ガード。drop で直前の値へ戻す（入れ子・早期 return 安全）。
struct ExplainLimitsScope(Option<crate::result_limits::ResultLimits>);

impl ExplainLimitsScope {
    fn enter(limits: crate::result_limits::ResultLimits) -> Self {
        Self(EXPLAIN_LIMITS_OVERRIDE.with(|c| c.replace(Some(limits))))
    }
}

impl Drop for ExplainLimitsScope {
    fn drop(&mut self) {
        EXPLAIN_LIMITS_OVERRIDE.with(|c| c.set(self.0));
    }
}

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 解決済みの衝突対象を、名前を含まない種別トークンにする。
fn conflict_target_token(target: &BoundConflictTarget) -> &'static str {
    match target {
        BoundConflictTarget::RowId => "row_id",
        BoundConflictTarget::Unique(_) => "unique",
        BoundConflictTarget::AnyUnique => "omitted",
    }
}

fn conflict_shape(clause: &OnConflictClause, target: &BoundConflictTarget) -> ConflictShape {
    let (action, filter) = match &clause.action {
        OnConflictAction::DoNothing => ("do_nothing", false),
        OnConflictAction::DoUpdate { condition, .. } => ("do_update", !condition.is_empty()),
    };
    ConflictShape {
        action,
        target: conflict_target_token(target),
        filter,
    }
}

impl EngineCore {
    /// EXPLAIN 系の束縛が使う結果行数上限。`INSERT ... SELECT` の EXPLAIN が暗黙 `LIMIT`（cap+1）を
    /// 付けた内側を作る間は、通常実行の `bind_insert_select` と同じ上限（付与値）を返し、受理条件を
    /// 一致させる。それ以外は設定値（`result_limits`）。
    pub(super) fn explain_limits(&self) -> crate::result_limits::ResultLimits {
        EXPLAIN_LIMITS_OVERRIDE
            .with(|c| c.get())
            .unwrap_or(self.result_limits)
    }

    /// `ParsedSql::ExplainInsert` の実行。書き込まずに挿入方式の段の木（`QUERY PLAN` 1 列）を返す。
    pub(super) fn run_insert_explain(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        target: &ExplainInsertTarget,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let node = match target {
            ExplainInsertTarget::Values(stmt) => self.insert_values_explain_node(stmt)?,
            ExplainInsertTarget::Select { insert, inner } => {
                self.insert_select_explain_node(ctx, session, insert, inner)?
            }
        };
        Ok(SqlOutcome::Explain(node.into_query_result()))
    }

    /// `VALUES`／`DEFAULT VALUES`／ファイル形の束縛と段の木。`execute_insert_form`／
    /// `execute_insert_returning_form` の書き込み前の手順（行数上限 → スキーマ → `RETURNING` →
    /// 索引由来の衝突制約の拒否 → 束縛 → バッチ上限）と同じ順で呼ぶ。
    fn insert_values_explain_node(
        &self,
        stmt: &crate::sql::allowlist::ValidatedInsert,
    ) -> Result<ExplainNode, SqlSurfaceError> {
        self.check_insert_row_count_limit(stmt.rows.len())?;
        let schema = self
            .storage
            .get_table_schema(&stmt.table_name)
            .map_err(|e| match e {
                CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
                // 格納済みカタログ断片・I/O 情報を wire へ出さない固定文言（`execute_insert_form` と同じ）。
                _ => SqlSurfaceError::Internal {
                    detail: "failed to load table schema".to_string(),
                },
            })?;
        if stmt.returning.is_some() {
            crate::sql::parser::bind_returning(stmt.returning.as_ref(), &schema)?
                .ok_or_else(internal)?;
        }
        self.reject_index_derived_conflict_constraint(&stmt.table_name, stmt.on_conflict.as_ref())?;
        let _sparsevec_limits_scope = self.sparsevec_limits_scope();
        let bound = crate::sql::parser::bind_insert_form(stmt, &schema)?;
        let mut conflict = None;
        let method = match &bound {
            BoundInsertForm::Row(_) => "row",
            BoundInsertForm::RowBatch(bounds) => {
                self.validate_insert_row_batch_limits(bounds)?;
                "row_batch"
            }
            BoundInsertForm::File(_) => {
                if stmt.returning.is_some() {
                    // `execute_insert_returning_form` と同じ拒否（ファイル形は `RETURNING` 非対応）。
                    return Err(SqlSurfaceError::unsupported(
                        "RETURNING is not supported for file-form INSERT (path/body columns)",
                    ));
                }
                "file"
            }
            BoundInsertForm::Upsert(upsert) => {
                self.validate_upsert_batch_limits(&upsert.rows)?;
                if let Some(clause) = stmt.on_conflict.as_ref() {
                    conflict = Some(conflict_shape(clause, &upsert.target));
                }
                "upsert"
            }
        };
        let default_values =
            stmt.columns.is_empty() && !stmt.columns_omitted && stmt.rows.iter().all(Vec::is_empty);
        let shape = InsertShape {
            source: if default_values {
                "default_values"
            } else {
                "values"
            },
            method: Some(method),
            conflict,
            returning: stmt.returning.is_some(),
            operation_id: stmt.operation_id.is_some(),
        };
        let child = values_result_leaf().map_err(|e| e.into_surface_error())?;
        insert_node(&shape, child).map_err(|e| e.into_surface_error())
    }

    /// `INSERT ... SELECT` の束縛と段の木。列数・型の不一致は `bind_insert_select`（Describe・実行と
    /// 共有）が返す。内側の計画は `EXPLAIN SELECT` と同じ実行本体で作り、`select` 段の子に載せる。
    fn insert_select_explain_node(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        insert: &crate::sql::allowlist::ValidatedInsertSelect,
        inner: &InsertSelectExplainInner,
    ) -> Result<ExplainNode, SqlSurfaceError> {
        self.bind_insert_select(
            session,
            insert,
            &crate::sql::parser::LiteralSkip::NONE,
            &self.result_limits,
        )?;
        let conflict = match insert.on_conflict.as_ref() {
            None => None,
            Some(clause) => {
                let (_read_txn, schema) = self.read_txn_with_schema(&insert.table_name)?;
                let (target, _action) = crate::sql::parser::bind_conflict_clause(clause, &schema)?;
                Some(conflict_shape(clause, &target))
            }
        };
        let inner_node = match inner {
            InsertSelectExplainInner::Constant => {
                constant_result_leaf().map_err(|e| e.into_surface_error())?
            }
            InsertSelectExplainInner::Target(t) => {
                // 暗黙 `LIMIT` を付けた内側は、`bind_insert_select` と同じ付与値を上限にして計画を作る。
                let _limits_scope = insert
                    .implicit_limit
                    .and_then(|n| usize::try_from(n).ok())
                    .map(|n| ExplainLimitsScope::enter(crate::result_limits::ResultLimits::new(n)));
                let outcome = self.execute_validated_in_session(
                    ctx,
                    session,
                    Statement::Explain((**t).clone()),
                )?;
                let SqlOutcome::Explain(result) = outcome else {
                    return Err(internal());
                };
                // 内側は描画済みの行を構造化された木へ戻して載せる（JSON 変換が見出しと属性を区別できるように）。
                let lines = result
                    .rows
                    .into_iter()
                    .map(|row| match row.cells.into_iter().next() {
                        Some(Cell::Text(text)) => Ok(text),
                        _ => Err(internal()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ExplainNode::from_rendered_lines(&lines).map_err(|e| e.into_surface_error())?
            }
        };
        let shape = InsertShape {
            source: "select",
            method: None,
            conflict,
            returning: insert.returning.is_some(),
            operation_id: insert.operation_id.is_some(),
        };
        let child = select_stage(inner_node).map_err(|e| e.into_surface_error())?;
        insert_node(&shape, child).map_err(|e| e.into_surface_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_limits::ResultLimits;

    /// スコープ中だけ上限が差し替わり、drop（入れ子含む）で直前の状態へ戻る。
    #[test]
    fn explain_limits_scope_overrides_and_restores() {
        assert_eq!(EXPLAIN_LIMITS_OVERRIDE.with(|c| c.get()), None);
        {
            let _outer = ExplainLimitsScope::enter(ResultLimits::new(11));
            assert_eq!(
                EXPLAIN_LIMITS_OVERRIDE.with(|c| c.get()),
                Some(ResultLimits::new(11))
            );
            {
                let _inner = ExplainLimitsScope::enter(ResultLimits::new(21));
                assert_eq!(
                    EXPLAIN_LIMITS_OVERRIDE.with(|c| c.get()),
                    Some(ResultLimits::new(21))
                );
            }
            assert_eq!(
                EXPLAIN_LIMITS_OVERRIDE.with(|c| c.get()),
                Some(ResultLimits::new(11))
            );
        }
        assert_eq!(EXPLAIN_LIMITS_OVERRIDE.with(|c| c.get()), None);
    }
}
