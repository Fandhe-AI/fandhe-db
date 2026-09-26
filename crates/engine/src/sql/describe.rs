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
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::ColumnMeta;
use crate::sql::parser::{BoundScan, ProjectedColumn, ProjectionColumn};

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

/// 広域取得（scan）の [`BoundScan`] から結果列メタデータを導出する（SQL-30・
/// TASK-214、Issue #930）。`bound.windows()` が空なら [`projected_columns`] と
/// 同じ結果になる。非空の場合は、ウィンドウ以外の列（[`projected_columns`]
/// 相当）へ [`crate::sql::parser::BoundWindowItem::position`] に基づいてウィンドウ
/// 列（`ColumnMeta::Computed`）を差し込む——`sql::window::execute_window_scan`
/// の列組み立てと同一の写像（`crates/engine/tests/describe_parity.rs` で機械
/// 検証する契約）。
pub(crate) fn scan_columns(
    bound: &BoundScan,
    schema: &TableSchema,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    let plain = projected_columns(bound.projection(), schema);
    if bound.windows().is_empty() {
        return Ok(plain);
    }

    let total_len = plain.len() + bound.windows().len();
    let mut window_positions: Vec<usize> = bound.windows().iter().map(|w| w.position).collect();
    window_positions.sort_unstable();
    let mut plain_positions: Vec<usize> = Vec::with_capacity(plain.len());
    {
        let mut wpos_iter = window_positions.iter().peekable();
        for pos in 0..total_len {
            if wpos_iter.peek() == Some(&&pos) {
                wpos_iter.next();
            } else {
                plain_positions.push(pos);
            }
        }
    }
    if plain_positions.len() != plain.len() {
        return Err(SqlSurfaceError::Internal {
            detail: "window scan describe position mismatch".to_string(),
        });
    }

    let mut out: Vec<Option<ColumnMeta>> = vec![None; total_len];
    for (pos, meta) in plain_positions.into_iter().zip(plain) {
        if let Some(slot) = out.get_mut(pos) {
            *slot = Some(meta);
        }
    }
    for item in bound.windows() {
        if let Some(slot) = out.get_mut(item.position) {
            *slot = Some(ColumnMeta::Computed {
                name: item.name.clone(),
            });
        }
    }
    out.into_iter()
        .enumerate()
        .map(|(pos, c)| {
            c.ok_or_else(|| SqlSurfaceError::Internal {
                detail: format!("window scan describe column missing at position {pos}"),
            })
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
