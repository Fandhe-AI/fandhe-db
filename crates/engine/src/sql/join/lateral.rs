//! 外側の行ごとに再評価する LATERAL 派生表の実行器（SQL-62・Issue #3322、親 #3319。設計記録
//! `docs/design/lateral-derived-table.md` の L1・L2・L5・L7〜L15。ポインタ: SQL-60・SQL-61・RLS-10）。
//!
//! 責務境界: `sql::derived_table` が左側の列を参照する LATERAL を「基底（外側参照を含まない部分。
//! 合成 CTE の slot として 1 回だけ評価する）」と「後処理（投影・`ORDER BY`・`LIMIT`・`OFFSET`。
//! `Derived` の層）」に分け、相関述語（外側参照を含む連言）を [`crate::sql::allowlist::JoinLateral`]
//! として運ぶ。`sql::join` の `execute_with_inputs` が基底の行から [`LateralRuntime`] を組み
//! （相関述語の束縛・層の計画。評価の前に必ず束縛するため、束縛エラーは行数に依存しない）、
//! `exec::run_joins_lateral` が段ごとに [`lateral_step`] を呼ぶ。
//!
//! 外側タプル 1 件ごとの処理は次の順で、第 2 の評価器・実行器は持たない。
//! 1. 基底の各行に相関述語をかける（`where_tree` の評価器。外側の値は合成スキーマの予約名の列へ
//!    セルとして渡し、SQL 文字列・トークン列・AST のリテラルへは連結しない。L3）。
//! 2. 一致した行へ後処理の層（`view_buffered::apply_rows_layer`）を適用する。
//! 3. 外側の文の側スキャン（その relation への投影とプッシュダウン述語）を適用する。
//! 4. 結果を出力領域（arena）へ追記する。`sides[new_rel]` はこの arena に置き換わり、下流（残余・
//!    並べ替え・集計・投影）は従来どおり `sides` を読む。順序は「外側タプル順 → 後処理後の行順」（L13）。
//!
//! 上限（`54000`。可視行だけで判定し `LIMIT`／`OFFSET` の値に依存させない。L7〜L12）: 評価を始める前に
//! 通過した外側タプル数を再評価予算と、「外側タプル数 × 基底の行数」の見積りを比較予算と照合し、見積り
//! 全量を一括して消費する（バケット化は行わない。L10）。作業領域は確保前に共有バイト予算へ計上する。
//! RLS（L15）: 基底は呼び出したセッションの `PolicyContext`・同じ `read_txn` で 1 回だけ評価済みの可視行で、
//! 外側の値はポリシーの評価に入らない。エラー文に予約内部名・行・件数を含めない。

use std::collections::HashSet;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::{BufferedRows, CteSlot, JoinLateral, SqlSurfaceError, ValidatedScan};
use crate::sql::correlation::{lateral_ref_placeholder, LATERAL_REF_PREFIX};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::expr_program::StackValue;
use crate::sql::udf_call::UdfRegistry;
use crate::sql::view_buffered::OuterPlan;
use crate::sql::where_tree::{BoundConjunction, BoundOrGroup};

use super::exec::{to_u32, Tuples, SLOT_BYTES};
use super::plan::LateralStepPlan;
use super::values::{cell_scalar, cell_scalar_supports};
use super::{JoinBudget, JoinLimits};

/// 1 つの LATERAL relation を実行するための束縛済みの入力（外側タプルに依存しない部分）。
pub(super) struct LateralRuntime {
    /// 基底の評価済みの行（可視行。文内で 1 回だけ評価したもの）。
    base: QueryResult,
    /// 合成スキーマ（基底の列 ＋ 外側参照ごとの列）へ束縛した相関述語。
    group: BoundOrGroup,
    /// 合成スキーマの列数。
    width: usize,
    /// `(基底の行のセル位置 = 合成スキーマの列位置, 列型)`。述語が参照する基底の列だけ。
    inner_cols: Vec<(usize, ColumnType)>,
    /// `(合成スキーマの列位置, 外側 relation, 側スキャン結果内のセル位置, 列型)`。
    outer_cols: Vec<(usize, usize, usize, ColumnType)>,
    /// 後処理の層の計画（基底の列 → 後処理後の列）。
    layer: OuterPlan,
    /// 後処理の `LIMIT` 省略（`OFFSET` 適用後の件数を結果行数上限で判定する印）。
    layer_limit_implicit: bool,
    /// 後処理後の列（列名リスト適用後）。側スキャンはこの列に対して計画する。
    layered_columns: Vec<ColumnMeta>,
    /// 外側の文の側スキャンの計画（後処理後の列 → 側投影）。
    side: OuterPlan,
}

/// [`LateralRuntime::build`] の入力（引数の個数を抑えるための束ね）。
pub(super) struct BuildInput<'a> {
    pub(super) lat: &'a JoinLateral,
    pub(super) step: &'a LateralStepPlan,
    /// `JoinPlan::schemas`（relation と同じ並び）。
    pub(super) schemas: &'a [&'a TableSchema],
    /// LATERAL の relation が参照する slot（後処理。列名リストを持つ）。
    pub(super) slot: &'a CteSlot,
    /// `slot` の層（`Derived` の `layer`）。
    pub(super) layer: &'a BufferedRows,
    /// 外側の文がこの relation へかける側スキャン（投影・プッシュダウン述語）。
    pub(super) scan: &'a ValidatedScan,
    pub(super) udfs: &'a UdfRegistry,
}

fn unsupported_column_type() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "this column type is not supported in a LATERAL subquery condition",
    )
}

fn too_large(what: &str) -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(what.to_string())
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// セルを評価用のスカラーへ写す。行 `id` の列（`Cell::Integer(u64)`）は `BIGINT` として載せる
/// （`i64` に収まらなければ `22003`。`cell_scalar` は符号付きのセルだけを受け付けるため）。
fn lateral_scalar<'a>(
    cell: &'a Cell,
    ty: &ColumnType,
) -> Result<Option<ScalarRef<'a>>, SqlSurfaceError> {
    match (ty, cell) {
        (ColumnType::BigInt, Cell::Integer(v)) => {
            let v = i64::try_from(*v)
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("row id is out of range"))?;
            Ok(Some(ScalarRef::BigInt(v)))
        }
        _ => cell_scalar(cell, ty),
    }
}

/// 束縛エラーの文言に予約内部名が含まれる場合は、固定文言の `42601` へ置き換える（合成スキーマの列名が
/// 型エラー等の文言へ引用されても、予約名をクライアントへ出さない。fail-closed）。
fn scrub_reserved_names(e: SqlSurfaceError) -> SqlSurfaceError {
    if e.to_string().contains(LATERAL_REF_PREFIX) {
        SqlSurfaceError::unsupported("unsupported condition in a LATERAL subquery")
    } else {
        e
    }
}

impl LateralRuntime {
    /// 基底の行 `base` と束縛情報から実行用の入力を組む。相関述語の束縛は行数・テナントに依存しない
    /// （空の基底・空の外側でも必ず通すため、束縛エラーが可視行の有無で変わらない）。
    pub(super) fn build(base: QueryResult, input: BuildInput<'_>) -> Result<Self, SqlSurfaceError> {
        let BuildInput {
            lat,
            step,
            schemas,
            slot,
            layer,
            scan,
            udfs,
        } = input;
        let (mut composite, _ids) = crate::sql::join::cte_schema(&slot.name, &base.columns)?;
        let base_width = composite.columns.len();
        let mut outer_cols: Vec<(usize, usize, usize, ColumnType)> = Vec::new();
        if step.refs.len() != lat.outer_refs.len() {
            return Err(internal("LATERAL outer reference count mismatch"));
        }
        for ((name, col), (rel, _)) in step.refs.iter().zip(lat.outer_refs.iter()) {
            let schema = *schemas
                .get(col.rel)
                .ok_or_else(|| internal("LATERAL outer relation out of range"))?;
            let mut def = match col.schema_idx {
                Some(i) => schema
                    .columns
                    .get(i)
                    .ok_or_else(|| internal("LATERAL outer column out of range"))?
                    .clone(),
                None => ColumnDef::new(name.clone(), ColumnType::BigInt, true),
            };
            if matches!(def.ty, ColumnType::Vector(_)) || !cell_scalar_supports(&def.ty) {
                return Err(unsupported_column_type());
            }
            def.name = lateral_ref_placeholder(*rel, name);
            def.nullable = true;
            let slot_pos = composite.columns.len();
            outer_cols.push((slot_pos, col.rel, col.pos, def.ty.clone()));
            composite.columns.push(def);
        }

        let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let (metadata, expr, _rls, or_filters) = crate::sql::parser::bind_where_predicates(
            &lat.correlated,
            &composite,
            udfs,
            &mut node_budget,
            &[],
        )
        .map_err(scrub_reserved_names)?;
        let group = BoundOrGroup::new(vec![BoundConjunction::new(metadata, expr, or_filters)]);
        if group.references_embedding() {
            return Err(unsupported_column_type());
        }

        // 述語が参照する基底の列だけをセルから取り出す（参照しない列の型は問わない）。
        let mut idents: HashSet<String> = HashSet::new();
        crate::sql::parser::collect_where_predicate_idents(&lat.correlated, &mut idents);
        let mut sorted: Vec<&String> = idents.iter().collect();
        sorted.sort();
        let mut inner_cols: Vec<(usize, ColumnType)> = Vec::new();
        for ident in sorted {
            if ident.starts_with(LATERAL_REF_PREFIX) {
                continue;
            }
            if let Some(idx) = composite
                .columns
                .iter()
                .take(base_width)
                .position(|c| &c.name == ident)
            {
                let ty = composite
                    .columns
                    .get(idx)
                    .map(|c| c.ty.clone())
                    .ok_or_else(|| internal("LATERAL base column out of range"))?;
                if matches!(ty, ColumnType::Vector(_)) || !cell_scalar_supports(&ty) {
                    return Err(unsupported_column_type());
                }
                inner_cols.push((idx, ty));
            }
        }

        // 後処理の層と、外側の側スキャンを事前に計画する（外側タプルごとには計画し直さない）。
        let layer_plan =
            crate::sql::view_buffered::plan_rows_layer(&base.columns, &slot.name, layer, udfs)?;
        let mut layered_columns = layer_plan.columns().to_vec();
        if !slot.column_aliases.is_empty() {
            layered_columns = crate::sql::view_buffered::rename_cte_columns(
                &layered_columns,
                &slot.column_aliases,
            )?;
        }
        let side = super::plan_cte_side_scan(&layered_columns, scan, udfs)?;
        Ok(Self {
            base,
            group,
            width: composite.columns.len(),
            inner_cols,
            outer_cols,
            layer: layer_plan,
            layer_limit_implicit: layer.limit_implicit,
            layered_columns,
            side,
        })
    }

    /// 基底の行数。
    pub(super) fn base_len(&self) -> usize {
        self.base.rows.len()
    }
}

/// LATERAL の段 1 つを実行する。`acc` は relation `0..new_rel` の結合結果で、戻りは新しいタプル列と
/// `sides[new_rel]` に置く出力領域。`stop_after_pairs` は最終段の早期停止の閾値（Issue #1797）。
#[allow(clippy::too_many_arguments)]
pub(super) fn lateral_step(
    sides: &[Vec<ResultRow>],
    rt: &LateralRuntime,
    new_rel: usize,
    acc: &Tuples,
    budget: &mut JoinBudget,
    limits: &JoinLimits,
    stop_after_pairs: Option<usize>,
) -> Result<(Tuples, Vec<ResultRow>), SqlSurfaceError> {
    let n = acc.n;
    let outer_count = acc.len();
    let base_len = rt.base_len();

    // L10: 評価・確保の前に、再評価数と比較数の見積り（外側タプル数 × 基底の行数）を照合する。
    budget.charge_reevaluations(outer_count)?;
    let estimate = outer_count
        .checked_mul(base_len)
        .ok_or_else(|| too_large("JOIN comparison count exceeds the limit"))?;
    budget.charge_comparisons(estimate, limits.max_comparisons)?;

    // L12: 外側タプルごとに再利用する作業領域（一致した添字・部分集合・後処理の出力の最大値）を 1 回だけ計上する。
    if estimate > 0 {
        budget.charge(super::result_bytes(rt.base.columns.len(), &rt.base.rows))?;
        budget.charge(base_len.saturating_mul(std::mem::size_of::<u32>()))?;
        let layer_bound = rt
            .layer
            .rows_output_bytes(&rt.base)
            .ok_or_else(|| internal("layer output size is not estimable"))?;
        budget.charge(layer_bound)?;
    }

    let mut data: Vec<Option<u32>> = Vec::new();
    let mut arena: Vec<ResultRow> = Vec::new();
    let mut matched: Vec<u32> = Vec::new();
    let mut scratch: Vec<StackValue> = Vec::new();
    let mut scanned: Vec<Option<ScalarRef<'_>>> = vec![None; rt.width];
    let mut compared: usize = 0;

    for ti in 0..outer_count {
        crate::interrupt::check()?;
        let tuple = acc
            .get(ti)
            .ok_or_else(|| internal("JOIN accumulated tuple index out of range"))?;
        // 外側参照の値（NULL 補完された relation・NULL のセルは NULL）。
        for (slot_pos, rel, pos, ty) in &rt.outer_cols {
            let row = tuple
                .get(*rel)
                .copied()
                .flatten()
                .and_then(|i| sides.get(*rel).and_then(|rows| rows.get(i as usize)));
            let value = match row {
                None => None,
                Some(r) => {
                    let cell = r
                        .cells
                        .get(*pos)
                        .ok_or_else(|| internal("LATERAL outer cell position out of range"))?;
                    lateral_scalar(cell, ty)?
                }
            };
            *scanned
                .get_mut(*slot_pos)
                .ok_or_else(|| internal("LATERAL outer slot out of range"))? = value;
        }
        // 基底の各行へ相関述語をかける。
        matched.clear();
        for (bi, row) in rt.base.rows.iter().enumerate() {
            if bi % crate::interrupt::CHECK_STRIDE as usize == 0 {
                crate::interrupt::check()?;
            }
            compared = compared
                .checked_add(1)
                .ok_or_else(|| too_large("JOIN comparison count exceeds the limit"))?;
            if compared > estimate {
                return Err(internal(
                    "LATERAL comparisons exceeded the reserved estimate",
                ));
            }
            for (cell_pos, ty) in &rt.inner_cols {
                let cell = row
                    .cells
                    .get(*cell_pos)
                    .ok_or_else(|| internal("LATERAL base cell position out of range"))?;
                *scanned
                    .get_mut(*cell_pos)
                    .ok_or_else(|| internal("LATERAL base slot out of range"))? =
                    lateral_scalar(cell, ty)?;
            }
            if rt.group.matches(&scanned, row.id, &[], 0, &mut scratch)? {
                matched.push(to_u32(bi, "JOIN row count exceeds limit")?);
            }
        }
        if matched.is_empty() {
            continue;
        }
        // 一致行へ後処理の層・側スキャンを適用する。
        let subset = QueryResult {
            columns: rt.base.columns.clone(),
            rows: matched
                .iter()
                .filter_map(|&i| rt.base.rows.get(i as usize).cloned())
                .collect(),
        };
        let mut layered = crate::sql::view_buffered::apply_rows_layer(subset, &rt.layer)?;
        if rt.layer_limit_implicit && layered.rows.len() > limits.max_result_rows {
            return Err(crate::sql::scan::row_limit_exceeded());
        }
        layered.columns = rt.layered_columns.clone();
        // 側スキャンの出力（出力領域へ残る分）は適用の前に計上する。
        budget.charge_layer_output(&layered, &rt.side)?;
        let sided = crate::sql::view_buffered::apply_rows_layer(layered, &rt.side)?;
        for out_row in sided.rows {
            // L11: 追加の前に出力行数・最終段の停止閾値と照合する。
            if arena.len() >= limits.max_output_rows {
                return Err(too_large("JOIN result exceeds the row limit"));
            }
            if matches!(stop_after_pairs, Some(t) if arena.len() >= t) {
                return Err(crate::sql::scan::row_limit_exceeded());
            }
            budget.charge(n.saturating_mul(SLOT_BYTES))?;
            let idx = to_u32(arena.len(), "JOIN row count exceeds limit")?;
            for (rel, slot) in tuple.iter().enumerate() {
                data.push(if rel == new_rel { Some(idx) } else { *slot });
            }
            arena.push(out_row);
        }
    }
    Ok((Tuples::from_parts(n, data)?, arena))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_cells_map_to_bigint_and_reject_out_of_range() {
        assert!(matches!(
            lateral_scalar(&Cell::Integer(7), &ColumnType::BigInt),
            Ok(Some(ScalarRef::BigInt(7)))
        ));
        let e = lateral_scalar(&Cell::Integer(u64::MAX), &ColumnType::BigInt)
            .expect_err("out of range");
        assert_eq!(e.wire_code(), "22003");
        assert!(matches!(
            lateral_scalar(&Cell::Null, &ColumnType::BigInt),
            Ok(None)
        ));
    }

    #[test]
    fn reserved_names_in_binding_errors_are_replaced_with_a_fixed_message() {
        let leaked = SqlSurfaceError::unsupported(format!("bad {LATERAL_REF_PREFIX}0.lang"));
        let scrubbed = scrub_reserved_names(leaked);
        assert_eq!(scrubbed.wire_code(), "42601");
        assert!(!scrubbed.to_string().contains(LATERAL_REF_PREFIX));
        let other = SqlSurfaceError::undefined_column("nope".to_string());
        assert_eq!(scrub_reserved_names(other).wire_code(), "42703");
    }
}
