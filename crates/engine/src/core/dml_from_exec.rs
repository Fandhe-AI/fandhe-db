//! `UPDATE ... FROM`／`DELETE ... USING` の実行腕（SQL-53 (b)、Issue #1754。親 #1753）。
//!
//! 責務境界: 本 Issue の範囲は構文と束縛までで、結合の実行は #1755、全参照への RLS 適用と
//! `RETURNING` は #1756、台帳の content_hash は #1757（構文段で算出済み）。したがって本モジュールの実行入口は
//! 現在のスナップショットのスキーマで束縛（`sql::dml_from`）を通し、通った文を **`0A000` で止める**
//! （行・台帳・世代・シーケンス・書き込みトランザクションのいずれにも触れない副作用ゼロのスタブ）。
//! 束縛エラー（`42702`・`42P01`・`42804` 等）は Execute でも Describe でも同じコードで返る。
//!
//! テナント境界（P0）: 行を一切読まず、書かない。RLS は #1755／#1756 が JOIN 実行器を経由して
//! 適用する（本モジュールは RLS を迂回する経路を作らない）。エラー文言には入力された名前と固定文言
//! だけを含め、他テナントの情報を含めない。
//!
//! 呼び出し元: `core.rs` の autocommit 腕・明示トランザクション腕・Describe 腕の
//! `ParsedSql::UpdateFrom`／`ParsedSql::DeleteUsing`。

use super::EngineCore;
use crate::catalog::TableSchema;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedDeleteUsing, ValidatedUpdateFrom};
use crate::sql::SqlOutcome;

/// 実行が未対応であることを示す固定文言（`0A000`）。
const UPDATE_FROM_UNSUPPORTED: &str = "UPDATE ... FROM execution is not supported yet";
const DELETE_USING_UNSUPPORTED: &str = "DELETE ... USING execution is not supported yet";

impl EngineCore {
    /// 対象表と FROM／USING 側の表のスキーマを同一スナップショットから取得する。
    fn dml_from_schemas(
        &self,
        target: &str,
        others: &[crate::sql::relation::TableRef],
    ) -> Result<(TableSchema, Vec<TableSchema>), SqlSurfaceError> {
        let mut names: Vec<String> = Vec::with_capacity(
            others
                .len()
                .saturating_add(1)
                .min(crate::sql::relation::MAX_TABLE_REFS),
        );
        names.push(target.to_string());
        names.extend(others.iter().map(|r| r.table().to_string()));
        let (_read_txn, schemas) = self.read_txn_with_schemas(&names)?;
        let internal = || SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        };
        // 自己結合（同じ表を別名で複数回参照）は取得を 1 回にまとめているため、複製して並べる。
        let target_schema = schemas.get(target).cloned().ok_or_else(internal)?;
        let mut other_schemas = Vec::with_capacity(others.len());
        for r in others {
            other_schemas.push(schemas.get(r.table()).cloned().ok_or_else(internal)?);
        }
        Ok((target_schema, other_schemas))
    }

    /// `UPDATE ... FROM` を束縛する（Describe 腕と実行腕が共有）。
    pub(super) fn bind_update_from_stmt(
        &self,
        stmt: &ValidatedUpdateFrom,
    ) -> Result<(), SqlSurfaceError> {
        let (target, others) = self.dml_from_schemas(stmt.target_table(), &stmt.from)?;
        let refs: Vec<&TableSchema> = others.iter().collect();
        crate::sql::dml_from::bind_update_from(stmt, &target, &refs).map(|_| ())
    }

    /// `DELETE ... USING` を束縛する（Describe 腕と実行腕が共有）。
    pub(super) fn bind_delete_using_stmt(
        &self,
        stmt: &ValidatedDeleteUsing,
    ) -> Result<(), SqlSurfaceError> {
        let (target, others) = self.dml_from_schemas(stmt.target_table(), &stmt.using)?;
        let refs: Vec<&TableSchema> = others.iter().collect();
        crate::sql::dml_from::bind_delete_using(stmt, &target, &refs).map(|_| ())
    }

    /// `UPDATE ... FROM` の実行腕。束縛を通した後に `0A000` を返す（副作用ゼロ）。
    pub(super) fn execute_update_from_stub(
        &self,
        stmt: &ValidatedUpdateFrom,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        self.bind_update_from_stmt(stmt)?;
        Err(SqlSurfaceError::feature_not_supported(
            UPDATE_FROM_UNSUPPORTED,
        ))
    }

    /// `DELETE ... USING` の実行腕。束縛を通した後に `0A000` を返す（副作用ゼロ）。
    pub(super) fn execute_delete_using_stub(
        &self,
        stmt: &ValidatedDeleteUsing,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        self.bind_delete_using_stmt(stmt)?;
        Err(SqlSurfaceError::feature_not_supported(
            DELETE_USING_UNSUPPORTED,
        ))
    }
}
