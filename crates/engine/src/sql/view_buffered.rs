//! 評価後射影形ビュー（本文に `LIMIT`・`ORDER BY`・集計・JOIN を含む
//! `CREATE VIEW`。TABLE-18・Issue #1192）の外側処理。
//!
//! 責務境界: `core.rs` の `Statement::BufferedView` アームが、ビュー本文
//! （`Scan`／`Aggregate`／`Join`）を**参照したセッション自身**の `PolicyContext`
//! で既存の実行経路により評価して [`QueryResult`] を得た後、本モジュールが
//! 外側クエリの列射影と `LIMIT`／`OFFSET` の切り出しだけを行う（第 2 の実行器を
//! 作らない。cursor の DECLARE/FETCH と同じく「内側の文を既存経路で実行し、
//! 結果から列を選び行を切り出す」方式）。Describe（拡張クエリ）は本文の列
//! メタデータに対して [`resolve_projection`] だけを適用し、本文は実行しない。
//!
//! RLS-10 (b) の不変条件: 本モジュールは行の可視性判定に一切関与しない
//! （`PolicyContext` を受け取らない）。可視行の確定は本文の実行経路が参照
//! セッションの `ctx` で行うため、作成者の可視性は構造的に引き継がれない。
//!
//! 順序: 本文の順序（`ORDER BY`／`GROUP BY`／JOIN が固定した決定的な順序）を
//! そのまま保ち、後処理でソートしない（sort-determinism 規約）。

use super::allowlist::{Projection, SqlSurfaceError};
use super::exec::{ColumnMeta, QueryResult, ResultRow};

/// 列メタデータの名前（疑似列 `id` は `"id"`）。
fn column_name(meta: &ColumnMeta) -> &str {
    match meta {
        ColumnMeta::Id => "id",
        ColumnMeta::Scalar { name, .. } | ColumnMeta::Computed { name, .. } => name,
    }
}

/// 外側の射影を本文の結果列に対して解決し、選択する列インデックスと結果列
/// メタデータを返す。`*` は本文の全列（重複名があってもそのまま通す）。列名指定は
/// 本文の結果列名で解決し、存在しない名前は `22000`、本文に同名列が複数あり
/// 一意に決まらない名前は `42702`（fail-closed。どちらか一方を黙って選ばない）。
pub(crate) fn resolve_projection(
    body_columns: &[ColumnMeta],
    projection: &Projection,
) -> Result<(Vec<usize>, Vec<ColumnMeta>), SqlSurfaceError> {
    match projection {
        Projection::All => Ok(((0..body_columns.len()).collect(), body_columns.to_vec())),
        Projection::Columns(names) => {
            let mut indices = Vec::with_capacity(names.len());
            let mut metas = Vec::with_capacity(names.len());
            for name in names {
                let mut found: Option<usize> = None;
                for (i, meta) in body_columns.iter().enumerate() {
                    if column_name(meta) == name {
                        if found.is_some() {
                            return Err(SqlSurfaceError::ambiguous_column(name.clone()));
                        }
                        found = Some(i);
                    }
                }
                let idx = found.ok_or_else(|| SqlSurfaceError::InvalidInput {
                    detail: format!("unknown column: {name}"),
                })?;
                let meta = body_columns.get(idx).cloned().ok_or_else(internal_error)?;
                indices.push(idx);
                metas.push(meta);
            }
            Ok((indices, metas))
        }
        // 構造検証段（`validate_select_statement`）が式項目を拒否済みのため
        // 到達しない（fail-closed）。
        Projection::Items(_) => Err(SqlSurfaceError::unsupported(
            "expression projection items are not supported on this view",
        )),
    }
}

fn internal_error() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 本文の実行結果に外側の列射影と `OFFSET`／`LIMIT` を適用する。行はその場で
/// 先頭から `offset` 件を読み飛ばし `limit` 件で打ち切る（本文の行数を超える
/// `offset` は 0 行）。列インデックスが行のセル数を超える場合は内部エラー
/// （fail-closed）。
pub(crate) fn project_and_slice(
    result: QueryResult,
    projection: &Projection,
    limit: u32,
    offset: u32,
) -> Result<QueryResult, SqlSurfaceError> {
    let (indices, columns) = resolve_projection(&result.columns, projection)?;
    let offset = usize::try_from(offset).map_err(|_| internal_error())?;
    let limit = usize::try_from(limit).map_err(|_| internal_error())?;
    let mut rows = Vec::new();
    for row in result.rows.into_iter().skip(offset).take(limit) {
        let mut cells = Vec::with_capacity(indices.len());
        for &i in &indices {
            cells.push(row.cells.get(i).cloned().ok_or_else(internal_error)?);
        }
        rows.push(ResultRow {
            id: row.id,
            score: row.score,
            cells,
        });
    }
    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnType;
    use crate::sql::exec::Cell;

    fn result() -> QueryResult {
        QueryResult {
            columns: vec![
                ColumnMeta::Scalar {
                    name: "a".to_string(),
                    ty: ColumnType::Text,
                },
                ColumnMeta::Computed {
                    name: "count".to_string(),
                    ty: Some(ColumnType::Integer),
                },
            ],
            rows: (0..5)
                .map(|i| ResultRow {
                    id: i,
                    score: 0.0,
                    cells: vec![Cell::Text(format!("r{i}")), Cell::Integer(i)],
                })
                .collect(),
        }
    }

    #[test]
    fn slices_offset_and_limit_in_place() {
        let out = project_and_slice(result(), &Projection::All, 2, 1).unwrap();
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.rows[0].id, 1);
        assert_eq!(out.rows[1].id, 2);
    }

    #[test]
    fn offset_beyond_rows_yields_empty() {
        let out = project_and_slice(result(), &Projection::All, 10, 99).unwrap();
        assert!(out.rows.is_empty());
        assert_eq!(out.columns.len(), 2);
    }

    #[test]
    fn projects_named_columns_in_requested_order() {
        let p = Projection::Columns(vec!["count".to_string(), "a".to_string()]);
        let out = project_and_slice(result(), &p, 1, 0).unwrap();
        assert_eq!(out.rows[0].cells[0], Cell::Integer(0));
        assert_eq!(out.rows[0].cells[1], Cell::Text("r0".to_string()));
    }

    #[test]
    fn unknown_column_is_22000() {
        let p = Projection::Columns(vec!["nope".to_string()]);
        let err = project_and_slice(result(), &p, 1, 0).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn duplicate_body_column_name_is_42702() {
        let mut r = result();
        r.columns.push(ColumnMeta::Scalar {
            name: "a".to_string(),
            ty: ColumnType::Text,
        });
        for row in &mut r.rows {
            row.cells.push(Cell::Null);
        }
        let p = Projection::Columns(vec!["a".to_string()]);
        let err = project_and_slice(r, &p, 1, 0).unwrap_err();
        assert_eq!(err.wire_code(), "42702");
    }
}
