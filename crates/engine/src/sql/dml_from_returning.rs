//! `UPDATE … FROM`／`DELETE … USING` の `RETURNING`（Issue #1756・SQL-53 のポインタ）の
//! 束縛・投影・収集を担う。FROM／USING 側の列も参照できる点だけが従来の `RETURNING`
//! （[`crate::sql::returning`]）との違いで、対象表の列は `returning::project_row` へ
//! そのまま委譲する（第 2 の投影実装を作らない。「対象表の列だけを参照する場合は従来と
//! 同じ列・行」を構造的に保証する）。
//!
//! 責務境界: 名前解決は `sql::relation::BindingScope`（`relations[0]` = 対象表、
//! `relations[1..]` = FROM／USING の宣言順）に委譲し、FROM／USING の構文束縛（#1754）・
//! 結合実行と多重一致の一意化（#1755）の結果を受け取って `RETURNING` だけを担う。
//! RLS（RLS-10 のポインタ）は呼び出し側の走査が書き込むセッションの `PolicyContext` で
//! 各表を独立に評価することで担保し、ここでは対象行の再判定
//! （[`joined_returning_collector`]）だけを行う。
//!
//! 現状は #1754／#1755 の結線待ちで、`core.rs` からは未接続（結線までは未使用のため
//! モジュール全体で `dead_code` を許可している）。未接続の入口は固定文言の `0A000` で
//! fail-closed のまま残し、仮の成功経路は作らない。

#![allow(dead_code)]

use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ColumnMeta, ResultRow};
use crate::sql::parser::ProjectedColumn;
use crate::sql::relation::{BindingScope, ColumnRef, ColumnSlot};
use crate::sql::returning::{self, MAX_RETURNING_RESULT_BYTES};
use crate::tenant::{CapturedRow, CapturedRowOrigin, TenantWriteError};

/// `RETURNING` 項目数の構文段上限（無制限確保の防止。JOIN の SELECT 項目上限と同値）。
pub(crate) const MAX_DML_FROM_RETURNING_ITEMS: usize = 4096;

/// 構文段が返す `RETURNING` 項目 1 つ。式・関数呼び出し・`AS` 別名は構文段で `42601`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DmlFromReturningItem {
    /// 裸の `*`（対象表に続けて FROM／USING の各表を宣言順に展開）。
    All,
    /// `<公開名>.*`。
    QualifiedAll(String),
    /// 列参照（修飾あり・なし）。
    Column(ColumnRef),
}

/// 構文段が返す `RETURNING` 句（FROM／USING つき UPDATE／DELETE 専用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DmlFromReturning {
    pub(crate) items: Vec<DmlFromReturningItem>,
}

/// 束縛済みの出力列 1 つ。`relation` は [`BindingScope`] 内の位置（0 = 対象表）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FromReturningColumn {
    pub(crate) relation: usize,
    pub(crate) slot: ColumnSlot,
    pub(crate) meta: ColumnMeta,
    /// 対象表（`relation == 0`）の列だけが持つ従来形の投影（`returning::project_row` へ委譲する）。
    target_projection: Option<ProjectedColumn>,
}

/// 束縛済みの `RETURNING`。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundFromReturning {
    pub(crate) columns: Vec<FromReturningColumn>,
    /// relation ごとの「FROM／USING 側セル列の先頭に疑似列 `id` を持つか」
    /// （従来方式の表だけ `true`。`FromRowRef::cells` の並び規約の判定に使う）。
    id_offsets: Vec<bool>,
}

/// FROM／USING 側の 1 行。`cells` の並びは JOIN の `SideProjection::All` と同じ
/// （疑似列 `id` は従来方式の表だけ位置 0、列は `id_offset + index`）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FromRowRef<'a> {
    pub(crate) cells: &'a [Cell],
}

fn bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("RETURNING (FROM/USING) projection mismatch: {detail}"),
    }
}

fn push_column(
    out: &mut Vec<FromReturningColumn>,
    relation: usize,
    slot: ColumnSlot,
    scope: &BindingScope<'_>,
) -> Result<(), SqlSurfaceError> {
    let (_, schema) = scope
        .relations()
        .get(relation)
        .ok_or_else(|| bug("relation index out of range"))?;
    let (meta, projected) = match slot {
        ColumnSlot::Id => (ColumnMeta::Id, ProjectedColumn::Id),
        ColumnSlot::Column(index) => {
            let col = schema
                .columns
                .get(index)
                .ok_or_else(|| bug("column index out of range"))?;
            (
                ColumnMeta::Scalar {
                    name: col.name.clone(),
                    ty: col.ty.clone(),
                },
                ProjectedColumn::Column {
                    index,
                    name: col.name.clone(),
                },
            )
        }
    };
    if out.len() >= MAX_DML_FROM_RETURNING_ITEMS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many RETURNING columns",
        ));
    }
    out.try_reserve(1).map_err(|e| SqlSurfaceError::Internal {
        detail: format!("failed to reserve RETURNING columns: {e}"),
    })?;
    out.push(FromReturningColumn {
        relation,
        slot,
        meta,
        target_projection: (relation == 0).then_some(projected),
    });
    Ok(())
}

/// 1 relation 分の `*` 展開（従来方式の表だけ疑似列 `id` を先頭に置く）。
fn expand_relation(
    out: &mut Vec<FromReturningColumn>,
    relation: usize,
    scope: &BindingScope<'_>,
) -> Result<(), SqlSurfaceError> {
    let (_, schema) = scope
        .relations()
        .get(relation)
        .ok_or_else(|| bug("relation index out of range"))?;
    if schema.has_pseudo_id() {
        push_column(out, relation, ColumnSlot::Id, scope)?;
    }
    for index in 0..schema.columns.len() {
        push_column(out, relation, ColumnSlot::Column(index), scope)?;
    }
    Ok(())
}

/// 構文段の項目を [`BindingScope`] で解決する。裸の `*` は PG 17 と同じく対象表に続けて
/// FROM／USING の各表を宣言順に並べる（JOIN の `SELECT *` と同じ規則）。未知の修飾子は
/// `42P01`、曖昧な列は `42702`、未知列は束縛エラー（カタログの存在オラクルにしない）。
pub(crate) fn bind_from_returning(
    ret: &DmlFromReturning,
    scope: &BindingScope<'_>,
) -> Result<BoundFromReturning, SqlSurfaceError> {
    if ret.items.len() > MAX_DML_FROM_RETURNING_ITEMS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many RETURNING items",
        ));
    }
    let mut columns = Vec::new();
    for item in &ret.items {
        match item {
            DmlFromReturningItem::All => {
                for relation in 0..scope.len() {
                    expand_relation(&mut columns, relation, scope)?;
                }
            }
            DmlFromReturningItem::QualifiedAll(qualifier) => {
                let relation = scope
                    .relations()
                    .iter()
                    .position(|(r, _)| r.matches_qualifier(qualifier))
                    .ok_or_else(|| SqlSurfaceError::undefined_table(qualifier.clone()))?;
                expand_relation(&mut columns, relation, scope)?;
            }
            DmlFromReturningItem::Column(colref) => {
                let resolved = scope.resolve(colref)?;
                push_column(&mut columns, resolved.relation(), resolved.slot(), scope)?;
            }
        }
    }
    let id_offsets = scope
        .relations()
        .iter()
        .map(|(_, schema)| schema.has_pseudo_id())
        .collect();
    Ok(BoundFromReturning {
        columns,
        id_offsets,
    })
}

/// 列メタデータ（`RowDescription` 相当）。
pub(crate) fn column_meta(bound: &BoundFromReturning) -> Vec<ColumnMeta> {
    bound.columns.iter().map(|c| c.meta.clone()).collect()
}

/// FROM／USING 側セルの予算つき複製（確保前に累計を検証する）。
fn clone_from_cell(cell: &Cell, budget: &mut usize) -> Result<Cell, SqlSurfaceError> {
    let add = crate::sql::join::cell_payload_bytes(cell);
    let next = budget.saturating_add(add);
    if next > MAX_RETURNING_RESULT_BYTES {
        return Err(SqlSurfaceError::payload_too_large(
            "RETURNING result exceeds capacity",
        ));
    }
    *budget = next;
    Ok(cell.clone())
}

/// 対象行 1 行と、その SET／削除判定に使った FROM／USING 側の一致行から結果行を組む。
/// `from_rows` の `relation - 1` 番目が relation `relation` の行（`BindingScope` の
/// 宣言順）。対象側のセルは `returning::project_row` へ委譲する。範囲外は `XX000`。
pub(crate) fn project_joined_row(
    target_id: u64,
    target_values: &[Value],
    from_rows: &[FromRowRef<'_>],
    bound: &BoundFromReturning,
    budget: &mut usize,
) -> Result<ResultRow, SqlSurfaceError> {
    let mut cells = Vec::new();
    cells
        .try_reserve(bound.columns.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve RETURNING cells: {e}"),
        })?;
    for col in &bound.columns {
        if col.relation == 0 {
            let projected = col
                .target_projection
                .as_ref()
                .ok_or_else(|| bug("target projection missing"))?;
            let row = returning::project_row(
                target_id,
                target_values,
                std::slice::from_ref(projected),
                budget,
            )?;
            let cell = row
                .cells
                .into_iter()
                .next()
                .ok_or_else(|| bug("empty target projection"))?;
            cells.push(cell);
            continue;
        }
        let side = from_rows
            .get(col.relation.saturating_sub(1))
            .ok_or_else(|| bug("FROM/USING row missing"))?;
        let pos = match col.slot {
            ColumnSlot::Id => 0,
            ColumnSlot::Column(index) => {
                let id_offset = usize::from(
                    bound
                        .id_offsets
                        .get(col.relation)
                        .copied()
                        .ok_or_else(|| bug("relation offset missing"))?,
                );
                index
                    .checked_add(id_offset)
                    .ok_or_else(|| bug("cell position overflow"))?
            }
        };
        let cell = side
            .cells
            .get(pos)
            .ok_or_else(|| bug("FROM/USING cell index out of range"))?;
        cells.push(clone_from_cell(cell, budget)?);
    }
    Ok(ResultRow {
        id: target_id,
        score: 0.0,
        cells,
    })
}

/// 書き込み中の対象行ごとに呼ばれる収集コールバックを作る
/// （`tenant::*_capturing_unchecked` の `project` 引数へ渡す想定）。
/// `lookup` は対象行 id から、その行の SET／削除判定に使った FROM／USING 側の一致行群を引く。
///
/// - RLS 再判定（多層防御）: 不可視の更新・削除行は結果へ積まない（`returning_collector` と同じ）。
///   `Inserted` は FROM／USING つき DML には来ない契約で、来たら `XX000`（fail-closed）。
/// - 一致行を引けない対象行は `XX000`（結合と書き込みの食い違いを黙って落とさない）。
/// - 予算超過は `ReturningProjectionTooLarge`（`54000`）、その他は `ReturningProjectionFailed`。
pub(crate) fn joined_returning_collector<'a>(
    ctx: &'a PolicyContext,
    bound: &'a BoundFromReturning,
    mask_row_id: bool,
    lookup: &'a dyn Fn(u64) -> Option<Vec<FromRowRef<'a>>>,
    budget: &'a mut usize,
    rows: &'a mut Vec<ResultRow>,
) -> impl FnMut(&CapturedRow) -> Result<(), TenantWriteError> + 'a {
    move |row: &CapturedRow| {
        if row.origin == CapturedRowOrigin::Inserted {
            return Err(TenantWriteError::ReturningProjectionFailed(
                "unexpected inserted row in UPDATE FROM / DELETE USING".to_string(),
            ));
        }
        if !ctx.is_visible(&row.tenant_id, row.visibility) {
            return Ok(());
        }
        let from_rows = lookup(row.id).ok_or_else(|| {
            TenantWriteError::ReturningProjectionFailed(
                "matched FROM/USING row not found for target row".to_string(),
            )
        })?;
        match project_joined_row(row.id, &row.values, &from_rows, bound, budget) {
            Ok(mut result_row) => {
                if mask_row_id {
                    result_row.id = 0;
                }
                rows.try_reserve(1).map_err(|e| {
                    TenantWriteError::ReturningProjectionFailed(format!(
                        "failed to reserve RETURNING row buffer: {e}"
                    ))
                })?;
                rows.push(result_row);
                Ok(())
            }
            Err(SqlSurfaceError::PayloadTooLarge { detail }) => {
                Err(TenantWriteError::ReturningProjectionTooLarge(detail))
            }
            Err(other) => Err(TenantWriteError::ReturningProjectionFailed(
                other.to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    //! SQL-53（Issue #1756）の束縛・投影・収集の単体テスト。
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::relation::TableRef;
    use crate::storage::Visibility;

    fn schema(name: &str, cols: &[&str]) -> TableSchema {
        TableSchema::new(
            name,
            cols.iter()
                .map(|c| ColumnDef::new(*c, ColumnType::Text, true))
                .collect(),
        )
    }

    fn colref(q: Option<&str>, n: &str) -> DmlFromReturningItem {
        DmlFromReturningItem::Column(match q {
            Some(q) => ColumnRef::qualified(q, n),
            None => ColumnRef::unqualified(n),
        })
    }

    fn ret(items: Vec<DmlFromReturningItem>) -> DmlFromReturning {
        DmlFromReturning { items }
    }

    fn two_scope<'a>(t: &'a TableSchema, s: &'a TableSchema) -> BindingScope<'a> {
        BindingScope::new(vec![
            (TableRef::with_alias("t", "a"), t),
            (TableRef::with_alias("s", "b"), s),
        ])
        .expect("scope")
    }

    #[test]
    fn sql53_resolves_qualified_unqualified_and_self_join() {
        let t = schema("t", &["x", "k"]);
        let s = schema("s", &["y", "k"]);
        let scope = two_scope(&t, &s);
        let b = bind_from_returning(
            &ret(vec![
                colref(None, "x"),
                colref(Some("b"), "y"),
                colref(Some("b"), "k"),
            ]),
            &scope,
        )
        .expect("bind");
        let rels: Vec<usize> = b.columns.iter().map(|c| c.relation).collect();
        assert_eq!(rels, vec![0, 1, 1]);
        // 自己結合（同じ表を別名で 2 回）。
        let self_scope = BindingScope::new(vec![
            (TableRef::with_alias("t", "a"), &t),
            (TableRef::with_alias("t", "b"), &t),
        ])
        .expect("scope");
        let b = bind_from_returning(
            &ret(vec![colref(Some("a"), "x"), colref(Some("b"), "x")]),
            &self_scope,
        )
        .expect("bind");
        assert_eq!(b.columns[0].relation, 0);
        assert_eq!(b.columns[1].relation, 1);
    }

    #[test]
    fn sql53_rejects_ambiguous_unknown_qualifier_and_unknown_column() {
        let t = schema("t", &["x", "k"]);
        let s = schema("s", &["y", "k"]);
        let scope = two_scope(&t, &s);
        let code = |item| {
            bind_from_returning(&ret(vec![item]), &scope)
                .expect_err("must fail")
                .wire_code()
        };
        assert_eq!(code(colref(None, "k")), "42702");
        assert_eq!(code(colref(Some("zz"), "x")), "42P01");
        assert_eq!(
            code(DmlFromReturningItem::QualifiedAll("zz".into())),
            "42P01"
        );
        assert_eq!(code(colref(None, "nope")), "42703");
        // 元の表名での修飾は別名があると使えない。
        assert_eq!(code(colref(Some("t"), "x")), "42P01");
    }

    #[test]
    fn sql53_star_expands_target_then_from_in_order_with_pseudo_id() {
        let t = schema("t", &["x"]);
        let s = schema("s", &["y"]);
        let scope = two_scope(&t, &s);
        let all = bind_from_returning(&ret(vec![DmlFromReturningItem::All]), &scope).expect("a");
        assert_eq!(
            column_meta(&all)
                .iter()
                .map(|m| matches!(m, ColumnMeta::Id))
                .collect::<Vec<_>>(),
            vec![true, false, true, false]
        );
        let qs = bind_from_returning(
            &ret(vec![DmlFromReturningItem::QualifiedAll("b".into())]),
            &scope,
        )
        .expect("q");
        assert_eq!(qs.columns.len(), 2);
        assert!(qs.columns.iter().all(|c| c.relation == 1));
    }

    #[test]
    fn sql53_target_only_columns_match_existing_returning() {
        let t = schema("t", &["x", "k"]);
        let s = schema("s", &["y"]);
        let scope = two_scope(&t, &s);
        let b = bind_from_returning(&ret(vec![colref(None, "id"), colref(None, "k")]), &scope);
        // `id` は 2 表が持つ疑似列のため非修飾は曖昧（42702）。修飾して束縛する。
        assert_eq!(b.expect_err("ambiguous").wire_code(), "42702");
        let b = bind_from_returning(
            &ret(vec![
                colref(Some("a"), "id"),
                colref(None, "k"),
                colref(None, "x"),
            ]),
            &scope,
        )
        .expect("bind");
        let projection = vec![
            ProjectedColumn::Id,
            ProjectedColumn::Column {
                index: 1,
                name: "k".into(),
            },
            ProjectedColumn::Column {
                index: 0,
                name: "x".into(),
            },
        ];
        assert_eq!(
            column_meta(&b),
            returning::column_meta(&projection, &t).expect("meta")
        );
        let values = vec![Value::Text("vx".into()), Value::Null];
        let mut b1 = 0usize;
        let mut b2 = 0usize;
        let joined = project_joined_row(9, &values, &[], &b, &mut b1).expect("joined");
        let plain = returning::project_row(9, &values, &projection, &mut b2).expect("plain");
        assert_eq!(joined, plain);
        assert_eq!(b1, b2);
    }

    #[test]
    fn sql53_projects_from_side_cells_and_checks_bounds() {
        let t = schema("t", &["x"]);
        let s = schema("s", &["y"]);
        let scope = two_scope(&t, &s);
        let b = bind_from_returning(
            &ret(vec![colref(Some("b"), "y"), colref(Some("b"), "id")]),
            &scope,
        )
        .expect("bind");
        let cells = [Cell::Integer(5), Cell::Text("hello".into())];
        let from = [FromRowRef { cells: &cells }];
        let mut budget = 0usize;
        let row = project_joined_row(1, &[Value::Null], &from, &b, &mut budget).expect("row");
        assert_eq!(
            row.cells,
            vec![Cell::Text("hello".into()), Cell::Integer(5)]
        );
        assert_eq!(budget, 5);
        // FROM 行なし／セル範囲外は XX000。
        let err = project_joined_row(1, &[Value::Null], &[], &b, &mut 0).expect_err("missing");
        assert_eq!(err.wire_code(), "XX000");
        let short = [FromRowRef { cells: &cells[..1] }];
        let err = project_joined_row(1, &[Value::Null], &short, &b, &mut 0).expect_err("short");
        assert_eq!(err.wire_code(), "XX000");
        // 予算超過は 54000。
        let mut full = MAX_RETURNING_RESULT_BYTES;
        let err = project_joined_row(1, &[Value::Null], &from, &b, &mut full).expect_err("budget");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn sql53_collector_filters_invisible_and_rejects_unmatched() {
        let t = schema("t", &["x"]);
        let s = schema("s", &["y"]);
        let scope = two_scope(&t, &s);
        let b = bind_from_returning(&ret(vec![colref(Some("b"), "y")]), &scope).expect("bind");
        let ctx = PolicyContext::new("alice").expect("ctx");
        let cells = [Cell::Integer(1), Cell::Text("v".into())];
        let lookup = |id: u64| (id == 1).then(|| vec![FromRowRef { cells: &cells }]);
        let mk = |id: u64, tenant: &str, origin| CapturedRow {
            id,
            tenant_id: tenant.to_string(),
            visibility: if tenant == "alice" {
                Visibility::Public
            } else {
                Visibility::Private
            },
            values: vec![Value::Null],
            origin,
        };
        let mut budget = 0usize;
        let mut rows = Vec::new();
        {
            let mut f =
                joined_returning_collector(&ctx, &b, false, &lookup, &mut budget, &mut rows);
            f(&mk(1, "alice", CapturedRowOrigin::Updated)).expect("visible");
            // 他テナントの非公開行は一致行の有無にかかわらず黙って除外される。
            f(&mk(2, "bob", CapturedRowOrigin::Deleted)).expect("invisible");
            assert!(matches!(
                f(&mk(2, "alice", CapturedRowOrigin::Updated)),
                Err(TenantWriteError::ReturningProjectionFailed(_))
            ));
            assert!(matches!(
                f(&mk(1, "alice", CapturedRowOrigin::Inserted)),
                Err(TenantWriteError::ReturningProjectionFailed(_))
            ));
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cells, vec![Cell::Text("v".into())]);
    }
}
