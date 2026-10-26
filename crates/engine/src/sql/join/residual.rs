//! JOIN の WHERE 残余（結合後の行単位評価）と並べ替え（Issue #1190、SQL-28・
//! RLS-10、TASK-212）。
//!
//! 責務境界: `sql::join::plan` が「単一 relation の走査へプッシュダウンできない」と
//! 判定した WHERE 部分木（列同士の比較・relation を跨ぐ `OR`）を、`sql::join::exec`
//! が返した結合タプルに対して評価する。単一 relation で完結する部分木は束縛済みの
//! 述語（単一テーブル経路と同じ `BoundOrGroup`）を、結合行のセルから組み立てた
//! `scanned`（[`super::values::cell_scalar`]）に当てて評価する——リテラルの型解析・
//! 評価規則の第 2 実装を作らない。
//!
//! 3 値論理の扱い: 受理する木は `AND`／`OR` のみ（`NOT` を受理しない）で単調なため、
//! NULL 補完された relation を参照する葉を偽（不定）として 2 値で評価しても、
//! 「真の行だけを残す」結果は 3 値論理と一致する。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::ResultRow;
use crate::sql::expr_program::StackValue;

use super::plan::{ColCompare, PlainOrder, ResNode, SingleFilter};
use super::values::{cell_scalar, cmp_val, compare_order, compare_vals, op_holds};
use super::JoinBudget;

/// 結合タプルの `rel` 番目の relation の行（NULL 補完なら `None`）。
pub(super) fn row_at<'a>(
    sides: &'a [Vec<ResultRow>],
    tuple: &[Option<u32>],
    rel: usize,
) -> Result<Option<&'a ResultRow>, SqlSurfaceError> {
    match tuple.get(rel).copied().flatten() {
        None => Ok(None),
        Some(i) => sides
            .get(rel)
            .and_then(|rows| rows.get(i as usize))
            .map(Some)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN row index out of range".to_string(),
            }),
    }
}

/// 結合タプルの `(rel, pos)` のセルを返す。`fallbacks` が空でなければ（FULL JOIN USING の
/// 統合列 `COALESCE(左, 右)`）、先頭ソースが NULL 補完または NULL セルのとき、代替ソースを
/// 順に見て最初の非 NULL セルを採る。全て NULL なら先頭ソースの結果（`None` または NULL セル）を
/// そのまま返す。行を複製せず借用で返すため、呼び出し元は複製前にバイト見積もりを計上できる。
pub(super) fn cell_with_fallbacks<'a>(
    sides: &'a [Vec<ResultRow>],
    tuple: &[Option<u32>],
    rel: usize,
    pos: usize,
    fallbacks: &[(usize, usize)],
) -> Result<Option<&'a crate::sql::exec::Cell>, SqlSurfaceError> {
    let first = cell_at(sides, tuple, rel, pos)?;
    if fallbacks.is_empty() || first.is_some_and(|c| !matches!(c, crate::sql::exec::Cell::Null)) {
        return Ok(first);
    }
    for &(r, p) in fallbacks {
        if let Some(c) = cell_at(sides, tuple, r, p)? {
            if !matches!(c, crate::sql::exec::Cell::Null) {
                return Ok(Some(c));
            }
        }
    }
    Ok(first)
}

fn cell_at<'a>(
    sides: &'a [Vec<ResultRow>],
    tuple: &[Option<u32>],
    rel: usize,
    pos: usize,
) -> Result<Option<&'a crate::sql::exec::Cell>, SqlSurfaceError> {
    match row_at(sides, tuple, rel)? {
        None => Ok(None),
        Some(row) => row
            .cells
            .get(pos)
            .map(Some)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN column position out of range".to_string(),
            }),
    }
}

fn eval_single(
    f: &SingleFilter,
    sides: &[Vec<ResultRow>],
    tuple: &[Option<u32>],
    scratch: &mut Vec<StackValue>,
) -> Result<bool, SqlSurfaceError> {
    let row = match row_at(sides, tuple, f.rel)? {
        Some(r) => r,
        // NULL 補完された relation を参照する部分木は偽（不定）。
        None => return Ok(false),
    };
    let mut scanned: Vec<Option<crate::row_codec::ScalarRef<'_>>> = vec![None; f.schema_len];
    for (schema_idx, pos, ty) in &f.cols {
        let cell = row
            .cells
            .get(*pos)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN residual column position out of range".to_string(),
            })?;
        if let Some(slot) = scanned.get_mut(*schema_idx) {
            *slot = cell_scalar(cell, ty)?;
        }
    }
    f.group.matches(&scanned, row.id, &[], 0, scratch)
}

fn eval_compare(
    c: &ColCompare,
    sides: &[Vec<ResultRow>],
    tuple: &[Option<u32>],
) -> Result<bool, SqlSurfaceError> {
    let (l, r) = match (
        row_at(sides, tuple, c.lhs.0)?,
        row_at(sides, tuple, c.rhs.0)?,
    ) {
        (Some(l), Some(r)) => (l, r),
        _ => return Ok(false),
    };
    let lc = l
        .cells
        .get(c.lhs.1)
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN comparison column position out of range".to_string(),
        })?;
    let rc = r
        .cells
        .get(c.rhs.1)
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN comparison column position out of range".to_string(),
        })?;
    match (cmp_val(lc, &c.class)?, cmp_val(rc, &c.class)?) {
        (Some(a), Some(b)) => Ok(op_holds(c.op, compare_vals(&a, &b))),
        _ => Ok(false),
    }
}

fn eval(
    node: &ResNode,
    sides: &[Vec<ResultRow>],
    tuple: &[Option<u32>],
    scratch: &mut Vec<StackValue>,
) -> Result<bool, SqlSurfaceError> {
    match node {
        ResNode::And(ch) => {
            for c in ch {
                if !eval(c, sides, tuple, scratch)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        ResNode::Or(ch) => {
            for c in ch {
                if eval(c, sides, tuple, scratch)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        ResNode::Single(f) => eval_single(f, sides, tuple, scratch),
        ResNode::Compare(c) => eval_compare(c, sides, tuple),
    }
}

/// 残余 conjunct 列（`AND` 結合）がタプルに対してすべて真か（空なら真）。WHERE 残余
/// （[`filter_tuples`]）と ON 残余（`exec::join_step` の一致条件。Issue #1830）が共有する
/// 唯一の評価入口で、評価規則の第 2 実装を作らない。
pub(super) fn eval_all(
    nodes: &[ResNode],
    sides: &[Vec<ResultRow>],
    tuple: &[Option<u32>],
    scratch: &mut Vec<StackValue>,
) -> Result<bool, SqlSurfaceError> {
    for node in nodes {
        if !eval(node, sides, tuple, scratch)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 残余 conjunct（`AND` 結合）をすべて満たすタプルだけを、順序を保って残す。
pub(super) fn filter_tuples(
    order: Vec<u32>,
    tuples: &super::exec::Tuples,
    residual: &[ResNode],
    sides: &[Vec<ResultRow>],
    budget: &mut JoinBudget,
) -> Result<Vec<u32>, SqlSurfaceError> {
    if residual.is_empty() {
        return Ok(order);
    }
    budget.charge(order.len().saturating_mul(std::mem::size_of::<u32>()))?;
    let mut scratch: Vec<StackValue> = Vec::new();
    let mut kept: Vec<u32> = Vec::with_capacity(order.len());
    for &ti in &order {
        let tuple = tuples
            .get(ti as usize)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN tuple index out of range".to_string(),
            })?;
        if eval_all(residual, sides, tuple, &mut scratch)? {
            kept.push(ti);
        }
    }
    Ok(kept)
}

/// 非集計形のスカラー `ORDER BY`（安定ソート。同値のときは結合直後の決定的順序を
/// 保つ。NULL 位置は PostgreSQL 既定〔ASC 末尾・DESC 先頭〕）。キー値は比較前に
/// 一度だけ取り出す（型クラスの不整合を `Internal` として伝播させるため）。
pub(super) fn sort_by_keys(
    order: Vec<u32>,
    tuples: &super::exec::Tuples,
    keys: &[PlainOrder],
    sides: &[Vec<ResultRow>],
    budget: &mut JoinBudget,
) -> Result<Vec<u32>, SqlSurfaceError> {
    if keys.is_empty() || order.len() < 2 {
        return Ok(order);
    }
    let nkeys = keys.len();
    budget.charge(
        order
            .len()
            .saturating_mul(nkeys)
            .saturating_mul(std::mem::size_of::<Option<super::values::CmpVal<'_>>>())
            .saturating_add(order.len().saturating_mul(std::mem::size_of::<usize>() * 2)),
    )?;
    let mut flat: Vec<Option<super::values::CmpVal<'_>>> =
        Vec::with_capacity(order.len().saturating_mul(nkeys));
    for &ti in &order {
        let tuple = tuples
            .get(ti as usize)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN tuple index out of range".to_string(),
            })?;
        for k in keys {
            let v = match cell_with_fallbacks(sides, tuple, k.rel, k.pos, &k.fallbacks)? {
                None => None,
                Some(cell) => cmp_val(cell, &k.class)?,
            };
            flat.push(v);
        }
    }
    let mut perm: Vec<usize> = (0..order.len()).collect();
    perm.sort_by(|&a, &b| {
        for (j, k) in keys.iter().enumerate() {
            let x = flat.get(a * nkeys + j).and_then(|v| v.as_ref());
            let y = flat.get(b * nkeys + j).and_then(|v| v.as_ref());
            let ord = compare_order(x, y, k.descending, k.nulls);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    // `perm` は `0..order.len()` の並べ替えなので範囲外は起きない。行を黙って
    // 落とさず、万一の不整合は `Internal` にする。
    perm.into_iter()
        .map(|p| {
            order
                .get(p)
                .copied()
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN sort permutation out of range".to_string(),
                })
        })
        .collect()
}
