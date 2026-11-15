//! 評価済みセル（[`ResultRow`]）の安定ソートの唯一の実装。
//!
//! 呼び出し元は 2 つ。評価後射影形ビューの外側 `ORDER BY`（`sql::view_buffered`。
//! SQL-35・Issue #2213 の `SEARCH` 順序列を含む）と、集合演算の全体 `ORDER BY`
//! （`sql::set_op`。Issue #3267・SQL-29 (c) ポインタ）。比較は `sql::join::values` の
//! `cmp_class`／`cmp_val`／`compare_order` に一本化し、NULL 位置は `order_value` の規約に従う。
//!
//! 不変条件:
//! - 並べ替えは `sort_by`（安定）だけを使う（`sort_unstable*` 禁止。sort-determinism 規約）。
//!   同値の行は入力順を保つ。
//! - 比較器の中ではエラーを返せないため、キーの比較値は先に行ごとへ展開し、比較中の最初の
//!   エラーは保持して整列後に返す。
//! - 協調的中断（`interrupt::check`）はキー抽出の行ごとと整列直後に置き、比較器の中では呼ばない
//!   （全順序が崩れると panic しうるため）。
//! - 行の可視性（RLS）には関与しない。母集合は呼び出し元が RLS 適用後に確定した行だけである。

use std::cmp::Ordering;

use super::allowlist::SqlSurfaceError;
use super::exec::{Cell, ResultRow};
use super::join::values::{cmp_val, compare_order, CmpClass, CmpVal};
use super::order_value::NullPlacement;

/// `ORDER BY` キーの比較クラス。スカラー列は型クラス 1 つ、再帰 CTE の `SEARCH` 順序列
/// （`record`／`record[]`。SQL-35・Issue #2213）はフィールドごとの型クラス列で比較する。
#[derive(Debug)]
pub(crate) enum OrderClass {
    Scalar(CmpClass),
    Record { fields: Vec<CmpClass>, array: bool },
}

/// `ORDER BY`（列キーのみ）の 1 キー（本文の結果列の位置・比較クラス・降順指定）。
#[derive(Debug)]
pub(crate) struct OrderKeyPlan {
    pub(crate) index: usize,
    pub(crate) class: OrderClass,
    pub(crate) descending: bool,
    pub(crate) nulls: NullPlacement,
}

/// 外側 `ORDER BY` 用の比較値。`COUNT` 等の `Cell::Integer`（`u64`）が浮動小数・
/// `NUMERIC` クラスの列に載る場合は、クラスの値へ寄せる。
pub(crate) fn order_val<'a>(
    cell: &'a Cell,
    class: &CmpClass,
) -> Result<Option<CmpVal<'a>>, SqlSurfaceError> {
    match (class, cell) {
        (CmpClass::Numeric, Cell::Integer(u)) => Ok(Some(CmpVal::Num(
            crate::numeric::Decimal::from_parts(i128::from(*u), 0)
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("integer value out of range"))?,
        ))),
        (CmpClass::Float, Cell::Integer(u)) => Ok(Some(CmpVal::Float(*u as f64))),
        _ => cmp_val(cell, class),
    }
}

/// 行ごとに先に取り出した 1 キー分の比較値。レコード列はセルの借用のまま持ち、比較時に
/// フィールドごとに取り出す（経路長に比例する複製を作らない）。
enum SortKey<'a> {
    Scalar(Option<CmpVal<'a>>),
    Record(&'a Cell),
}

/// [`sort_rows`] が確保する領域の概算バイト数（呼び出し側が確保前に共有バイト予算へ計上する用）。
/// 行別キー配列の外側（行ごとの `Vec` ヘッダー）・実 `SortKey` 本体・添字列とソート作業領域・
/// 並べ替え用スロットを含む。算術は `checked_*` で行い、オーバーフローは `None`。
pub(crate) fn sort_alloc_bytes(rows: usize, keys: usize) -> Option<usize> {
    let per_row = keys
        .checked_mul(std::mem::size_of::<SortKey<'static>>())?
        .checked_add(std::mem::size_of::<Vec<SortKey<'static>>>())?
        .checked_add(std::mem::size_of::<usize>().checked_mul(2)?)?
        .checked_add(std::mem::size_of::<Option<ResultRow>>())?;
    per_row.checked_mul(rows)
}

/// 行を外側 `ORDER BY`（列キーのみ）のキーで安定ソートする。キーの比較値は先に行ごとへ展開し
/// （比較器からエラーを返せないため）、行の添字列だけを `sort_by`（安定）で並べ替える。
/// 比較値の個数は「行数 × キー数（`MAX_SCALAR_ORDER_KEYS` 以下）」で、行数は本文の
/// 結果行数（既存の予算で上限済み）に等しい。
pub(crate) fn sort_rows(
    rows: Vec<ResultRow>,
    order: &[OrderKeyPlan],
) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    // 比較器からはエラーを返せないため、最初のエラーを保持して `Equal` を返し、整列後に返す。
    let failure: std::cell::RefCell<Option<SqlSurfaceError>> = std::cell::RefCell::new(None);
    let idx = {
        let mut keys: Vec<Vec<SortKey<'_>>> = Vec::with_capacity(rows.len());
        for row in &rows {
            // Issue #1952: 協調的中断の確認点（ソートキー抽出の行ごと）。
            crate::interrupt::check()?;
            let mut k = Vec::with_capacity(order.len());
            for o in order {
                let cell = row.cells.get(o.index).ok_or_else(internal_error)?;
                k.push(match &o.class {
                    OrderClass::Scalar(class) => SortKey::Scalar(order_val(cell, class)?),
                    OrderClass::Record { .. } => SortKey::Record(cell),
                });
            }
            keys.push(k);
        }
        let mut idx: Vec<usize> = (0..rows.len()).collect();
        idx.sort_by(|&a, &b| {
            let (Some(ka), Some(kb)) = (keys.get(a), keys.get(b)) else {
                return Ordering::Equal;
            };
            for (o, (va, vb)) in order.iter().zip(ka.iter().zip(kb.iter())) {
                let ord = match (&o.class, va, vb) {
                    (OrderClass::Scalar(_), SortKey::Scalar(x), SortKey::Scalar(y)) => {
                        compare_order(x.as_ref(), y.as_ref(), o.descending, o.nulls)
                    }
                    (
                        OrderClass::Record { fields, array },
                        SortKey::Record(x),
                        SortKey::Record(y),
                    ) => match compare_record_cells(x, y, fields, *array) {
                        Ok(ord) if o.descending => ord.reverse(),
                        Ok(ord) => ord,
                        Err(e) => {
                            failure.borrow_mut().get_or_insert(e);
                            Ordering::Equal
                        }
                    },
                    _ => {
                        failure.borrow_mut().get_or_insert_with(internal_error);
                        Ordering::Equal
                    }
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        idx
    };
    if let Some(e) = failure.into_inner() {
        return Err(e);
    }
    // Issue #1952: ソート本体の比較関数内では中断しない（全順序が崩れると panic しうる）。直後に 1 回確認する。
    crate::interrupt::check()?;
    reorder(rows, &idx)
}

/// `SEARCH` 順序列（`record`／`record[]`）のセル同士を PostgreSQL の btree 比較（`record_cmp`／
/// `array_cmp`）に合わせて昇順で比較する（SQL-35・Issue #2213）。レコードは先頭フィールドから順に
/// 比較し NULL は非 NULL より後（NULL 同士は等しい）、配列は要素ごとに比較して共通部分が等しければ
/// 短い方が先。`DESC` は呼び出し元が全体を反転する。セルの形・フィールド数が計画と食い違えば
/// `Internal`（fail-closed）。
pub(crate) fn compare_record_cells(
    a: &Cell,
    b: &Cell,
    classes: &[CmpClass],
    array: bool,
) -> Result<Ordering, SqlSurfaceError> {
    fn fields_cmp(
        a: &[Cell],
        b: &[Cell],
        classes: &[CmpClass],
    ) -> Result<Ordering, SqlSurfaceError> {
        if a.len() != classes.len() || b.len() != classes.len() {
            return Err(internal_error());
        }
        for ((x, y), class) in a.iter().zip(b.iter()).zip(classes.iter()) {
            let vx = order_val(x, class)?;
            let vy = order_val(y, class)?;
            let ord = compare_order(vx.as_ref(), vy.as_ref(), false, NullPlacement::Last);
            if ord != Ordering::Equal {
                return Ok(ord);
            }
        }
        Ok(Ordering::Equal)
    }
    match (a, b, array) {
        (Cell::Record(x), Cell::Record(y), false) => fields_cmp(x, y, classes),
        (Cell::RecordArray(x), Cell::RecordArray(y), true) => {
            for (rx, ry) in x.iter().zip(y.iter()) {
                let ord = fields_cmp(rx, ry, classes)?;
                if ord != Ordering::Equal {
                    return Ok(ord);
                }
            }
            Ok(x.len().cmp(&y.len()))
        }
        _ => Err(internal_error()),
    }
}

/// 行を添字列 `idx` の順に並べ替える（添字は `0..rows.len()` の順列）。
pub(crate) fn reorder(
    rows: Vec<ResultRow>,
    idx: &[usize],
) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    let mut slots: Vec<Option<ResultRow>> = rows.into_iter().map(Some).collect();
    let mut sorted = Vec::with_capacity(slots.len());
    for &i in idx {
        // Issue #1952: 協調的中断の確認点（並べ替え適用の行ごと）。
        crate::interrupt::check()?;
        sorted.push(
            slots
                .get_mut(i)
                .and_then(Option::take)
                .ok_or_else(internal_error)?,
        );
    }
    Ok(sorted)
}

fn internal_error() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}
