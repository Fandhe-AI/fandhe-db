//! `MERGE` の束縛入口（SQL-79・Issue #3352。親 #3349、設計記録は `docs/design/sql-merge.md`）。
//!
//! 責務境界: 構文検証済みの [`ValidatedMerge`] に対し、対象・元のスキーマを**同一の読み取りスナップショット**
//! から集めて `sql::merge_bind::bind_merge` へ渡すだけを担う。行・台帳・世代・writer・暗黙 Tx には一切触れず、
//! 束縛エラーは書き込みより前に返る（Execute・明示 Tx・Describe の各腕が同じ入口を通るため、
//! SQLSTATE がそろう）。実行（押し下げ・書き込み・台帳）は #3353・#3354、`RETURNING` の束縛は #3356 の担当で、
//! 束縛に成功した文は従来どおり固定文言の `0A000` で止まる。
//!
//! 元が部分クエリの形は、内側の検証済み `Statement` を Describe して列記述だけを取り（**実行しない**）、
//! 疑似列 `id` を持たない採番方式の合成スキーマへ写す。内側の表の RLS は実行時（#3353 以降）に書き込む
//! セッションの `PolicyContext` で読み取り経路が適用するため、本モジュールはテナント情報を持たず、
//! 他テナントの存在・件数を観測させる経路を作らない。
//!
//! 呼び出し元: `core.rs` の `ParsedSql::Merge` の 3 つの腕（autocommit・明示 Tx・Describe）。

use super::EngineCore;
use crate::catalog::{RowIdentity, TableSchema};
use crate::sql::allowlist::{MergeSource, SqlSurfaceError, ValidatedMerge};
use crate::sql::merge_bind::{bind_merge, BoundMerge, MergeBindInput};
use crate::sql::mode::SessionState;
use crate::sql::udf_call::UdfRegistry;
use crate::sql::view::{resolve_from, Resolved};

impl EngineCore {
    /// `MERGE` を束縛する（Execute・Describe 共通）。`dummy_flags` は Describe の `$n` ダミーフラグ
    /// （Execute は空）。権限ゲート（`42501`）は呼び出し元が済ませている前提で、本関数は読み取りスナップショット
    /// だけを使う。
    pub(super) fn bind_merge_stmt(
        &self,
        session: &SessionState,
        stmt: &ValidatedMerge,
        udfs: &UdfRegistry,
        dummy_flags: &[bool],
        snap: Option<&redb::ReadTransaction>,
    ) -> Result<BoundMerge, SqlSurfaceError> {
        // 呼び出し元が読み取り源（明示 Tx の `BEGIN` 時点のスナップショット）を渡したときは、対象・元・
        // 部分クエリのすべてでそれを使う（最新の確定カタログで束縛すると、BEGIN 後の別セッションの
        // 列変更で Describe と Execute の見る世界がずれる）。無ければ最新の確定カタログ。
        let owned;
        let read_txn = match snap {
            Some(s) => s,
            None => {
                owned = self.begin_read_txn()?;
                &owned
            }
        };
        let snap = crate::catalog::ViewWriteSnapshot::over(read_txn);
        let schema_of = |name: &str| -> Result<TableSchema, SqlSurfaceError> {
            snap.table_schema(name)
                .map_err(crate::catalog::table_lookup_error)
        };

        // 対象: 実表またはビュー（自動更新できなければ単独のビュー書き込みと同じ `55000`）。
        let target_name = stmt.target.table();
        let (target_schema, target_view) = match resolve_from(&snap, target_name)? {
            Resolved::Table => (schema_of(target_name)?, None),
            Resolved::View { .. } | Resolved::Buffered { .. } => {
                let view = crate::sql::view_dml::resolve_merge_target(&snap, target_name)?;
                (schema_of(&view.base_table)?, Some(view))
            }
        };

        // 元: 実表・単純形ビューは基底表のスキーマ（ビューは公開列だけを見せる）、部分クエリは Describe から合成。
        let (source_schema, source_visible) = match &stmt.source {
            MergeSource::Table(rel) => match resolve_from(&snap, rel.table())? {
                Resolved::Table => (schema_of(rel.table())?, None),
                Resolved::View {
                    base_table,
                    view_columns,
                    column_map: None,
                    ..
                } => (schema_of(&base_table)?, view_columns),
                Resolved::View { .. } | Resolved::Buffered { .. } => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "MERGE supports only simple views without column aliases as the USING source",
                    ));
                }
            },
            MergeSource::Subquery {
                query,
                alias,
                column_aliases,
                ..
            } => (
                self.merge_subquery_schema(
                    session,
                    read_txn,
                    query,
                    alias,
                    column_aliases.as_deref(),
                )?,
                None,
            ),
        };

        bind_merge(
            stmt,
            &MergeBindInput {
                target_schema: &target_schema,
                target_view: target_view.as_ref(),
                source_schema: &source_schema,
                source_visible,
                dummy_flags,
            },
            udfs,
        )
    }

    /// 部分クエリの元の合成スキーマ（疑似列 `id` なしの採番方式）。内側は Describe するだけで実行しない。
    /// 静的な型を持てない列・不正な列名は固定文言の `0A000`、重複する出力名は `42701`、列別名が出力列より
    /// 多ければ `DerivedTableColumnListTooLong`。
    fn merge_subquery_schema(
        &self,
        session: &SessionState,
        read_txn: &redb::ReadTransaction,
        query: &crate::sql::allowlist::Statement,
        alias: &str,
        column_aliases: Option<&[String]>,
    ) -> Result<TableSchema, SqlSurfaceError> {
        let metas = self
            .describe_parsed_in_session_snap(
                session,
                &super::ParsedSql::Statement(query.clone()),
                &crate::sql::parser::LiteralSkip::NONE,
                &[],
                &crate::result_limits::ResultLimits::default(),
                Some(read_txn),
            )?
            .ok_or_else(|| {
                SqlSurfaceError::feature_not_supported(
                    "MERGE source subquery has no output columns",
                )
            })?;
        let mut columns = crate::sql::ctas::output_columns_from_metas(metas);
        if let Some(aliases) = column_aliases {
            if aliases.len() > columns.len() {
                return Err(SqlSurfaceError::DerivedTableColumnListTooLong {
                    alias: alias.to_string(),
                    available: columns.len(),
                    specified: aliases.len(),
                });
            }
            for (col, a) in columns.iter_mut().zip(aliases.iter()) {
                col.name = a.clone();
            }
        }
        let defs = crate::sql::matview::column_defs_from_view_columns(&columns).map_err(|e| {
            match e.wire_code() {
                // 重複する出力名はそのまま（`42701`）。それ以外は固定文言で拒否する（列名を示さない）。
                "42701" => e,
                _ => SqlSurfaceError::feature_not_supported(
                    "MERGE source subquery columns need valid names and static types",
                ),
            }
        })?;
        Ok(TableSchema::new(alias, defs).with_row_identity(RowIdentity::Allocated))
    }
}
