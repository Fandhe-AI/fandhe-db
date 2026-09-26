//! Describe（拡張クエリプロトコルの 'D' 種別 S。Issue #933・TASK-71・WIRE-11）が
//! 返す結果列メタデータの純粋な導出関数を集める。
//!
//! 呼び出し元は `core.rs::EngineCore::describe_parsed_in_session` のみ
//! （行データ・台帳・LLM I/O にはいずれも触れない）。ここに置く関数は
//! 「束縛済みの投影列（`ProjectedColumn`／`ProjectionColumn`）→
//! `sql::exec::ColumnMeta`」という、実行結果に依存しない写像だけを担う。
//! `SELECT`（[`crate::sql::exec::execute_statement_with_cache`]）・広域取得
//! （[`crate::sql::scan::execute_scan`]）・`RETURNING`
//! （[`crate::sql::returning::column_meta`]）が実際の実行時に組み立てる列と
//! 完全に一致する契約（`crates/engine/tests/describe_parity.rs` で機械検証）。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::exec::ColumnMeta;
use crate::sql::parser::{ProjectedColumn, ProjectionColumn};

/// `SELECT`・広域取得（scan）が共有する `ProjectedColumn` 列から `ColumnMeta` を
/// 導出する。`sql::exec::execute_statement_with_cache`・`sql::scan::execute_scan`
/// の投影列メタデータ組み立てと同一の写像（`Computed` 項目を拒否しない点で
/// `sql::returning::column_meta` とは異なる——`RETURNING` は `Computed` 項目を
/// 構造検証段で排除済みだが、通常の `SELECT` は宣言的 UDF・組み込み関数呼び出し
/// （TASK-79・SQL-9）による `Computed` 項目を持ちうる）。
///
/// `index` が `schema.columns` の範囲外になることは、`schema` が束縛時と同一の
/// スナップショットから取得されている限り起こらない（`bind_projection` が
/// 束縛時点の `schema` に対して検証済みの添字のみを積む）。範囲外だった場合は
/// `sql::exec` の既存経路と同じ `ColumnType::Text` へのフォールバックで
/// fail-closed に倒す（panic させない）。
pub(crate) fn projected_columns(
    projection: &[ProjectedColumn],
    schema: &TableSchema,
) -> Vec<ColumnMeta> {
    projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Id => ColumnMeta::Id,
            ProjectedColumn::Column { index, name } => ColumnMeta::Scalar {
                name: name.clone(),
                ty: schema
                    .columns
                    .get(*index)
                    .map(|c| c.ty.clone())
                    .unwrap_or(ColumnType::Text),
            },
            ProjectedColumn::Computed { name, .. } => ColumnMeta::Computed { name: name.clone() },
        })
        .collect()
}

/// 集計（`GROUP BY` の有無を問わない）が持つ `ProjectionColumn` 列から
/// `ColumnMeta` を導出する。`sql::aggregate::finish_aggregate_result`・
/// `sql::group_by::execute_grouped_aggregate` の PROJECT 段と同一の写像
/// （`GroupKey`・`Aggregate` のいずれも `ColumnMeta::Computed` になる既存の
/// 非対称——`GROUP BY` 列は `TEXT` 型だが `ColumnMeta::Scalar` にはならない
/// ——を変えずに踏襲する）。
pub(crate) fn aggregate_columns(projection: &[ProjectionColumn]) -> Vec<ColumnMeta> {
    projection
        .iter()
        .map(|col| {
            let name = match col {
                ProjectionColumn::GroupKey { name, .. } => name.clone(),
                ProjectionColumn::Aggregate { name, .. } => name.clone(),
            };
            ColumnMeta::Computed { name }
        })
        .collect()
}
