//! `LATERAL unnest(<左側の配列列>) [WITH ORDINALITY]` の実行（SQL-62・SQL-67・Issue #3360、親 #3319。
//! 設計記録 `docs/design/lateral-derived-table.md` の L18〜L23。ポインタ: TASK-344・RLS-10）。
//!
//! 責務境界: `sql::derived_table` が `unnest(...)` を派生表と同じプレースホルダの 1 relation として抽出し
//! （[`crate::sql::allowlist::LateralUnnest`]）、`core.rs` が Describe・Execute の両方で本モジュールの
//! [`unnest_columns_for`] を通して出力列（要素型は左側 relation のスキーマから決める）を導く。`sql::join` の
//! `execute_with_inputs` が [`UnnestRuntime`] を組み、`lateral::lateral_step` が段ごとに [`unnest_front`] を
//! 呼ぶ。後段（ON の評価・LEFT の NULL 補完）は本文つき LATERAL と共有し、第 2 の評価器・実行器は持たない。
//!
//! 外側タプル 1 件ごとの処理: 左側 relation の配列セルを要素のセル列へ展開し（`WITH ORDINALITY` なら 1 始まりの
//! `bigint` を末尾へ足す）、外側の文の側スキャンを適用して出力領域（arena）へ追記する。NULL 配列・空配列は
//! 0 行、NULL 要素は値 NULL の行、多次元配列は行優先で平坦化する（PostgreSQL 17.12 と照合済み）。
//!
//! 上限（`54000`。L22）: 評価の前に、通過した外側タプルの配列の要素数の総和を `checked_add` で求め、出力行数の
//! 上限・比較数の予算と照合する。判定は可視行だけで行い、`LIMIT`／`OFFSET` の値に依存させない。
//! RLS（L23）: 関数は記憶域を読まない。入力は呼び出したセッションの `PolicyContext`・同じ `read_txn` で
//! 可視になった外側の行のセルだけで、新しい可視性判定の経路は作らない。エラー文に行・要素値・件数を含めない。

use std::collections::HashMap;

use crate::catalog::{ArrayElemType, ArrayType, ColumnType, TableSchema};
use crate::row_codec::ArrayValue;
use crate::sql::allowlist::{
    JoinLateral, LateralUnnest, SqlSurfaceError, ValidatedJoin, ValidatedScan,
};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::udf_call::UdfRegistry;
use crate::sql::view_buffered::OuterPlan;

use super::exec::{push_charged_pair, to_u32, Tuples};
use super::lateral::{internal, too_large, LateralFront};
use super::plan::LateralStepPlan;
use super::{JoinBudget, JoinLimits};

/// `unnest` の実行用の入力（外側タプルに依存しない部分）。
pub(super) struct UnnestRuntime {
    /// 引数の列がある左側 relation の添字。
    arg_rel: usize,
    /// 引数の列の、左側 relation の側スキャン結果内のセル位置。
    arg_pos: usize,
    ordinality: bool,
    /// 出力列（要素列 [＋ 序数列]）。側スキャンはこの列に対して計画する。
    columns: Vec<ColumnMeta>,
    /// 外側の文の側スキャンの計画（出力列 → 側投影）。
    side: OuterPlan,
}

/// 配列の要素型を結合のスキーマへ載せる列型へ写す。ENUM 要素は語彙を持つ配列型から引く。
fn element_column_type(at: &ArrayType) -> Result<ColumnType, SqlSurfaceError> {
    Ok(match at.elem() {
        ArrayElemType::Text => ColumnType::Text,
        ArrayElemType::Bool => ColumnType::Boolean,
        ArrayElemType::Integer => ColumnType::Integer,
        ArrayElemType::SmallInt => ColumnType::SmallInt,
        ArrayElemType::BigInt => ColumnType::BigInt,
        ArrayElemType::Real => ColumnType::Real,
        ArrayElemType::Double => ColumnType::Double,
        ArrayElemType::Date => ColumnType::Date,
        ArrayElemType::Timestamp => ColumnType::Timestamp,
        ArrayElemType::Time => ColumnType::Time,
        ArrayElemType::TimestampTz => ColumnType::TimestampTz,
        ArrayElemType::Interval => ColumnType::Interval,
        ArrayElemType::Uuid => ColumnType::Uuid,
        ArrayElemType::Numeric { precision, scale } => ColumnType::Numeric { precision, scale },
        ArrayElemType::Bytea => ColumnType::Bytea,
        ArrayElemType::Json => ColumnType::Json,
        ArrayElemType::Jsonb => ColumnType::Jsonb,
        ArrayElemType::Varchar(n) => ColumnType::Varchar(n),
        ArrayElemType::Bpchar(n) => ColumnType::Bpchar(n),
        ArrayElemType::Bit(n) => ColumnType::Bit(n),
        ArrayElemType::Enum => match at.enum_def() {
            Some(def) => ColumnType::Enum(def.clone()),
            None => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "unnest of this array element type is not supported",
                ))
            }
        },
    })
}

/// 引数の列型 `arg_ty`（左側 relation のスキーマの列型）から `unnest` の出力列を導く（Describe と Execute が
/// 共有する唯一の導出。L20）。配列でなければ PostgreSQL 17.12 と同じ `42883`。
pub(crate) fn unnest_columns(
    arg_ty: &ColumnType,
    un: &LateralUnnest,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    let ColumnType::Array(at) = arg_ty else {
        return Err(SqlSurfaceError::undefined_function(
            "function unnest does not exist for the argument type",
        ));
    };
    let elem = element_column_type(at)?;
    let mut tys = vec![elem];
    if un.ordinality {
        tys.push(ColumnType::BigInt);
    }
    if un.column_names.len() != tys.len() {
        return Err(internal("unnest column name count mismatch"));
    }
    Ok(un
        .column_names
        .iter()
        .zip(tys)
        .map(|(name, ty)| ColumnMeta::Scalar {
            name: name.clone(),
            ty,
        })
        .collect())
}

/// `lat`（`unnest` の relation）の出力列を、左側 relation のスキーマ（`schemas`。CTE・派生表は先に合成済み）
/// から導く。`core.rs` の Describe・Execute が relation の並び順に呼ぶ。
pub(crate) fn unnest_columns_for(
    validated: &ValidatedJoin,
    schemas: &HashMap<String, TableSchema>,
    lat: &JoinLateral,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    let Some(un) = lat.unnest.as_ref() else {
        return Err(internal("LATERAL relation is not an unnest"));
    };
    let [(rel, column)] = lat.outer_refs.as_slice() else {
        return Err(internal("unnest must have exactly one argument column"));
    };
    let table = validated
        .relations
        .get(*rel)
        .ok_or_else(|| internal("unnest argument relation out of range"))?
        .table();
    let ty = schemas
        .get(table)
        .and_then(|s| s.columns.iter().find(|c| &c.name == column))
        .map(|c| c.ty.clone());
    match ty {
        Some(ty) => unnest_columns(&ty, un),
        // 疑似列 `id` などスキーマに無い列は配列ではない。
        None => Err(SqlSurfaceError::undefined_function(
            "function unnest does not exist for the argument type",
        )),
    }
}

impl UnnestRuntime {
    /// 束縛済みの段の計画（`step`）と左側のスキーマから実行用の入力を組む。要素型の導出は Describe と
    /// 同じ [`unnest_columns`]。側スキャンは出力列に対して 1 回だけ計画する（外側タプルごとには計画しない）。
    pub(super) fn build(
        un: &LateralUnnest,
        step: &LateralStepPlan,
        schemas: &[&TableSchema],
        scan: &ValidatedScan,
        udfs: &UdfRegistry,
    ) -> Result<Self, SqlSurfaceError> {
        let [(_, col)] = step.refs.as_slice() else {
            return Err(internal("unnest must have exactly one argument column"));
        };
        let schema = *schemas
            .get(col.rel)
            .ok_or_else(|| internal("unnest argument relation out of range"))?;
        let ty = match col.schema_idx {
            Some(i) => schema
                .columns
                .get(i)
                .ok_or_else(|| internal("unnest argument column out of range"))?
                .ty
                .clone(),
            None => ColumnType::BigInt,
        };
        let columns = unnest_columns(&ty, un)?;
        let side = super::plan_cte_side_scan(&columns, scan, udfs)?;
        Ok(Self {
            arg_rel: col.rel,
            arg_pos: col.pos,
            ordinality: un.ordinality,
            columns,
            side,
        })
    }
}

/// 配列値の要素を、行優先で平坦化したセル列へ展開する（NULL 要素は `Cell::Null`。新しい型変換は作らず、
/// 行 codec の値表現と結果セルの対応〔`matview::value_to_cell` と同じ〕をそのまま使う）。
fn element_cells(a: &ArrayValue) -> Result<Vec<Cell>, SqlSurfaceError> {
    fn map<T>(items: &[Option<T>], f: impl Fn(&T) -> Cell) -> Vec<Cell> {
        items
            .iter()
            .map(|e| e.as_ref().map_or(Cell::Null, &f))
            .collect()
    }
    Ok(match a.flat() {
        ArrayValue::Text(v) | ArrayValue::Enum(v) => map(v, |s| Cell::Text(s.clone())),
        ArrayValue::Bool(v) => map(v, |b| Cell::Bool(*b)),
        ArrayValue::Integer(v) => map(v, |i| Cell::SignedInteger(i64::from(*i))),
        ArrayValue::SmallInt(v) => map(v, |i| Cell::SignedInteger(i64::from(*i))),
        ArrayValue::BigInt(v) => map(v, |i| Cell::SignedInteger(*i)),
        ArrayValue::Real(v) => map(v, |x| Cell::Float(f64::from(*x))),
        ArrayValue::Double(v) => map(v, |x| Cell::Float(*x)),
        ArrayValue::Date(v) => map(v, |d| Cell::Date(*d)),
        ArrayValue::Timestamp(v) => map(v, |t| Cell::Timestamp(*t)),
        ArrayValue::Time(v) => map(v, |t| Cell::Time(*t)),
        ArrayValue::TimestampTz(v) => map(v, |t| Cell::TimestampTz(*t)),
        ArrayValue::Interval(v) => map(v, |i| Cell::Interval(*i)),
        ArrayValue::Uuid(v) => map(v, |u| Cell::Uuid(*u)),
        ArrayValue::Numeric(v) => map(v, |d| Cell::Numeric(*d)),
        ArrayValue::Bytea(v) => map(v, |b| Cell::Bytes(b.clone())),
        ArrayValue::Json(v) | ArrayValue::Jsonb(v) => map(v, |j| Cell::Json(j.clone())),
        ArrayValue::Bit(v) => map(v, |b| Cell::Bit(b.clone())),
        // `flat()` は `Multi` を返さない（不変条件）。
        ArrayValue::Multi(_) => return Err(internal("flattened array is still multi-dimensional")),
    })
}

/// 外側タプル `ti` の引数セル（配列）を借用で返す。NULL 補完された relation・NULL のセルは `None`（0 行）。
fn outer_array<'a>(
    sides: &'a [Vec<ResultRow>],
    acc: &Tuples,
    rt: &UnnestRuntime,
    ti: usize,
) -> Result<Option<&'a ArrayValue>, SqlSurfaceError> {
    let tuple = acc
        .get(ti)
        .ok_or_else(|| internal("JOIN accumulated tuple index out of range"))?;
    let row = tuple
        .get(rt.arg_rel)
        .copied()
        .flatten()
        .and_then(|i| sides.get(rt.arg_rel).and_then(|rows| rows.get(i as usize)));
    let Some(row) = row else { return Ok(None) };
    match row.cells.get(rt.arg_pos) {
        Some(Cell::Array(a)) => Ok(Some(a)),
        Some(Cell::Null) => Ok(None),
        _ => Err(internal("unnest argument cell is not an array")),
    }
}

/// `unnest` の前段（外側タプルごとに要素行を作って側スキャンを適用し、候補ペアと出力領域〔arena〕を作る）。
/// `passes` は外側だけを参照する ON 残余（`on_acc`）を通過した外側タプルか。評価の前に要素数の総和を
/// 出力行数の上限・比較数の予算と照合する（L22。超過・オーバーフローは評価も確保もせず `54000`）。
pub(super) fn unnest_front(
    sides: &[Vec<ResultRow>],
    rt: &UnnestRuntime,
    acc: &Tuples,
    passes: &dyn Fn(usize) -> bool,
    budget: &mut JoinBudget,
    limits: &JoinLimits,
) -> Result<LateralFront, SqlSurfaceError> {
    let outer_count = acc.len();
    // 事前判定: 通過した外側タプルの要素数の総和（確保なしで配列の長さだけを読む）。
    let mut total: usize = 0;
    for ti in 0..outer_count {
        if ti % crate::interrupt::CHECK_STRIDE as usize == 0 {
            crate::interrupt::check()?;
        }
        if !passes(ti) {
            continue;
        }
        if let Some(a) = outer_array(sides, acc, rt, ti)? {
            total = total
                .checked_add(a.flat().len())
                .ok_or_else(|| too_large("JOIN result exceeds the row limit"))?;
        }
    }
    if total > limits.max_output_rows {
        return Err(too_large("JOIN result exceeds the row limit"));
    }
    // 候補行の総和を比較数の見積りとして一括消費する（ON の評価回数の上界。実際の評価数に依存させない）。
    budget.charge_comparisons(total, limits.max_comparisons)?;

    let mut pairs: Vec<(u32, u32)> = Vec::new();
    let mut arena: Vec<ResultRow> = Vec::new();
    for ti in 0..outer_count {
        crate::interrupt::check()?;
        if !passes(ti) {
            continue;
        }
        let Some(array) = outer_array(sides, acc, rt, ti)? else {
            continue;
        };
        let elems = element_cells(array)?;
        if elems.is_empty() {
            continue;
        }
        let mut rows: Vec<ResultRow> = Vec::with_capacity(elems.len());
        for (k, cell) in elems.into_iter().enumerate() {
            let mut cells = vec![cell];
            if rt.ordinality {
                let n = i64::try_from(k.saturating_add(1))
                    .map_err(|_| too_large("JOIN row count exceeds limit"))?;
                cells.push(Cell::SignedInteger(n));
            }
            rows.push(ResultRow {
                id: 0,
                score: 0.0,
                cells,
            });
        }
        let produced = QueryResult {
            columns: rt.columns.clone(),
            rows,
        };
        // 側スキャンの出力（出力領域へ残る分）は適用の前に計上する。
        budget.charge_layer_output(&produced, &rt.side)?;
        let sided = crate::sql::view_buffered::apply_rows_layer(produced, &rt.side)?;
        let ti_u32 = to_u32(ti, "JOIN row count exceeds limit")?;
        for out_row in sided.rows {
            if arena.len() >= limits.max_output_rows {
                return Err(too_large("JOIN result exceeds the row limit"));
            }
            let idx = to_u32(arena.len(), "JOIN row count exceeds limit")?;
            push_charged_pair(&mut pairs, budget, (ti_u32, idx))?;
            arena.push(out_row);
        }
    }
    Ok((pairs, arena))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_codec::ArrayValue;

    fn un(ordinality: bool, names: &[&str]) -> LateralUnnest {
        LateralUnnest {
            ordinality,
            column_names: names.iter().map(|n| n.to_string()).collect(),
        }
    }

    fn array_ty(elem: ArrayElemType) -> ColumnType {
        ColumnType::Array(ArrayType::new(elem, 8).expect("array type"))
    }

    /// 要素型の写像と序数列（bigint）。配列でない列は `42883`、列名の個数の不整合は内部エラー。
    #[test]
    fn output_columns_follow_the_element_type() {
        let cols = unnest_columns(&array_ty(ArrayElemType::Integer), &un(true, &["v", "n"]))
            .expect("columns");
        assert!(matches!(
            cols.as_slice(),
            [
                ColumnMeta::Scalar {
                    ty: ColumnType::Integer,
                    ..
                },
                ColumnMeta::Scalar {
                    ty: ColumnType::BigInt,
                    ..
                }
            ]
        ));
        let cols =
            unnest_columns(&array_ty(ArrayElemType::Text), &un(false, &["v"])).expect("columns");
        assert!(matches!(
            cols.as_slice(),
            [ColumnMeta::Scalar {
                ty: ColumnType::Text,
                ..
            }]
        ));
        let e = unnest_columns(&ColumnType::Text, &un(false, &["v"])).expect_err("non-array");
        assert_eq!(e.wire_code(), "42883");
        let e = unnest_columns(&array_ty(ArrayElemType::Text), &un(true, &["v"]))
            .expect_err("name count");
        assert_eq!(e.wire_code(), "XX000");
    }

    /// NULL 要素は `Cell::Null`、多次元は行優先に平坦化し、bigint は 2^53 を超えても欠けない。
    #[test]
    fn elements_expand_in_row_major_order_with_nulls() {
        let a = ArrayValue::Integer(vec![Some(7), None, Some(9)]);
        assert_eq!(
            element_cells(&a).expect("cells"),
            vec![Cell::SignedInteger(7), Cell::Null, Cell::SignedInteger(9)]
        );
        let m = ArrayValue::with_dims(ArrayValue::Integer((1..=4).map(Some).collect()), vec![2, 2])
            .expect("dims");
        assert_eq!(
            element_cells(&m).expect("cells"),
            (1..=4).map(Cell::SignedInteger).collect::<Vec<_>>()
        );
        let big = i64::MAX;
        assert_eq!(
            element_cells(&ArrayValue::BigInt(vec![Some(big)])).expect("cells"),
            vec![Cell::SignedInteger(big)]
        );
        assert!(element_cells(&ArrayValue::Text(Vec::new()))
            .expect("empty")
            .is_empty());
    }
}
