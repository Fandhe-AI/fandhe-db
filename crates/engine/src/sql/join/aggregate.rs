//! JOIN 結果に対する集計形（`COUNT`／`SUM`／`AVG`／`MIN`／`MAX`・`GROUP BY`・
//! `HAVING`・`ORDER BY`・`LIMIT`／`OFFSET`。Issue #1190、SQL-28・RLS-10、TASK-212）。
//!
//! 責務境界: `sql::join::exec` が返した（RLS 適用済みの可視行だけから作った）結合
//! タプルをグループ化して集計する。第 2 の集計エンジンは作らず、集計値の型・
//! `22003`（桁あふれ）・`finish` は既存の [`Accumulator`]（`sql::aggregate`）を、
//! `HAVING` の判定・集計値の並べ替えは `sql::group_by` の既存比較器を再利用する
//! （`sql::window` が窓集計で採る方式と同じ）。結合行は embedding を持たないため
//! VECTOR 列を引数とする集計は束縛段で拒否済み。
//!
//! NULL の扱い: NULL 補完された relation を引数とする集計項目は NULL 入力として
//! 観測しない（`COUNT(*)` だけは常に数える）。`GROUP BY` の NULL キーは 1 つの
//! グループにまとめる（PostgreSQL と同じ）。`GROUP BY` なしの集計は、結合結果が
//! 0 行でも 1 行を返す（`COUNT` は 0、他は NULL）。`GROUP BY` ありで 0 行なら
//! 0 行を返す。`ORDER BY` が無いときのグループ順は、キー昇順（NULL 末尾）の
//! 決定的な順序にする。

use std::collections::HashMap;

use crate::sql::aggregate::{Accumulator, RowVector};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, QueryResult, ResultRow};
use crate::sql::expr_program::StackValue;
use crate::sql::parser::AggregateInput;

use super::exec::Tuples;
use super::plan::{AggOrderTarget, AggOut, AggPlan};
use super::residual::row_at;
use super::values::{cell_scalar, cmp_val, compare_order, encode_key_component};
use super::{cell_payload_bytes, JoinBudget};

struct Group {
    key_cells: Vec<Cell>,
    accs: Vec<Accumulator>,
}

/// `index + 1` の長さで `index` にのみ値を持つ `scanned` を組み立てる
/// （[`Accumulator::observe`] が `scanned.get(index)` でしか参照しないため）。
fn build_scanned<'a>(
    index: usize,
    value: Option<crate::row_codec::ScalarRef<'a>>,
) -> Vec<Option<crate::row_codec::ScalarRef<'a>>> {
    let mut out = vec![None; index + 1];
    if let Some(slot) = out.get_mut(index) {
        *slot = value;
    }
    out
}

/// 値の中身を見ず非 NULL の有無だけを数える入力型（`Accumulator::observe` が
/// 任意の `Some` で代用できる。`sql::window` の該当分岐と同じ）。
fn is_presence_only(input: &AggregateInput) -> bool {
    matches!(
        input,
        AggregateInput::BooleanColumn(_)
            | AggregateInput::ArrayColumn(_)
            | AggregateInput::ByteaColumn(_)
            | AggregateInput::JsonColumn(_)
            | AggregateInput::EnumColumn(_)
            | AggregateInput::UuidColumn(_)
    )
}

fn new_group(agg: &AggPlan, key_cells: Vec<Cell>) -> Result<Group, SqlSurfaceError> {
    let mut accs = Vec::with_capacity(agg.items.len());
    for it in &agg.items {
        accs.push(Accumulator::new(it.item.func, &it.item.input)?);
    }
    Ok(Group { key_cells, accs })
}

/// 集計形を実行して結果行を返す。`limit`／`offset` は検証済みの値。
pub(super) fn run_aggregate(
    agg: &AggPlan,
    sides: &[Vec<ResultRow>],
    tuples: &Tuples,
    order: &[u32],
    (limit, offset): (Option<usize>, usize),
    max_groups: usize,
    budget: &mut JoinBudget,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut scratch: Vec<StackValue> = Vec::new();
    let no_vector = RowVector {
        dim: 0,
        values: None,
    };
    let per_group_bytes = agg
        .items
        .len()
        .saturating_mul(std::mem::size_of::<Accumulator>())
        .saturating_add(
            agg.keys
                .len()
                .saturating_mul(std::mem::size_of::<Cell>() + 1),
        )
        .saturating_add(std::mem::size_of::<Group>());

    // `GROUP BY` なしの集計は、結合結果が 0 行でも 1 行を返す。
    if agg.keys.is_empty() {
        budget.charge(per_group_bytes)?;
        groups.push(new_group(agg, Vec::new())?);
        index.insert(Vec::new(), 0);
    }

    for &ti in order {
        let tuple = tuples
            .get(ti as usize)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN tuple index out of range".to_string(),
            })?;
        // グループキー（NULL は先頭 1 バイトのタグで区別し、NULL 同士を同一視する）。
        let mut key: Vec<u8> = Vec::new();
        let mut key_cells: Vec<Cell> = Vec::with_capacity(agg.keys.len());
        for k in &agg.keys {
            match row_at(sides, tuple, k.rel)? {
                None => {
                    key.push(0);
                    key_cells.push(Cell::Null);
                }
                Some(row) => {
                    let cell = row
                        .cells
                        .get(k.pos)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "JOIN group key position out of range".to_string(),
                        })?;
                    if matches!(cell, Cell::Null) {
                        key.push(0);
                        key_cells.push(Cell::Null);
                    } else {
                        key.push(1);
                        encode_key_component(cell, &k.key_class, &mut key)?;
                        key_cells.push(cell.clone());
                    }
                }
            }
        }
        let gi = match index.get(&key) {
            Some(&g) => g,
            None => {
                // 新しいグループの確保前に上限（件数・バイト予算）を判定する。
                if groups.len() >= max_groups {
                    return Err(SqlSurfaceError::payload_too_large(
                        "JOIN GROUP BY exceeds the group limit",
                    ));
                }
                let payload: usize = key_cells.iter().map(cell_payload_bytes).sum();
                budget.charge(
                    per_group_bytes
                        .saturating_add(key.len().saturating_mul(2))
                        .saturating_add(payload),
                )?;
                let g = groups.len();
                groups.push(new_group(agg, key_cells)?);
                index.insert(key, g);
                g
            }
        };
        let group = groups
            .get_mut(gi)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN group index out of range".to_string(),
            })?;
        for (i, it) in agg.items.iter().enumerate() {
            let acc = group
                .accs
                .get_mut(i)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN accumulator index out of range".to_string(),
                })?;
            let rel = match it.rel {
                // `COUNT(*)` は結合行を常に数える。
                None => {
                    acc.observe(&it.item.input, 0, &no_vector, &[], &mut scratch)?;
                    continue;
                }
                Some(r) => r,
            };
            // NULL 補完された relation は NULL 入力（観測しない）。
            let row = match row_at(sides, tuple, rel)? {
                Some(r) => r,
                None => continue,
            };
            match (&it.item.input, &it.column) {
                (AggregateInput::AllVisible | AggregateInput::IdU64, _) => {
                    acc.observe(&it.item.input, row.id, &no_vector, &[], &mut scratch)?;
                }
                (input, Some((schema_idx, pos, ty))) => {
                    let cell = row
                        .cells
                        .get(*pos)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "JOIN aggregate column position out of range".to_string(),
                        })?;
                    let value = if is_presence_only(input) {
                        if matches!(cell, Cell::Null) {
                            None
                        } else {
                            Some(crate::row_codec::ScalarRef::Bool(true))
                        }
                    } else {
                        cell_scalar(cell, ty)?
                    };
                    let scanned = build_scanned(*schema_idx, value);
                    acc.observe(input, row.id, &no_vector, &scanned, &mut scratch)?;
                }
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "JOIN aggregate input has no bound column".to_string(),
                    })
                }
            }
        }
    }

    // 確定: 各グループの集計値を確定し、HAVING で絞る。
    struct Finished {
        key_cells: Vec<Cell>,
        item_cells: Vec<Cell>,
    }
    let mut finished: Vec<Finished> = Vec::with_capacity(groups.len());
    for g in groups {
        let mut item_cells = Vec::with_capacity(g.accs.len());
        for acc in g.accs {
            item_cells.push(acc.finish()?);
        }
        let mut keep = true;
        for (idx, op, literal) in &agg.having {
            match item_cells.get(*idx) {
                Some(c) => {
                    if !crate::sql::group_by::having_matches(c, *op, *literal) {
                        keep = false;
                        break;
                    }
                }
                None => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "JOIN HAVING item index out of range".to_string(),
                    })
                }
            }
        }
        if keep {
            finished.push(Finished {
                key_cells: g.key_cells,
                item_cells,
            });
        }
    }

    // 決定的な既定順序（キー昇順・NULL 末尾）→ 明示 ORDER BY（安定ソート）。
    let key_cmp = |a: &Finished, b: &Finished, k: usize, descending: bool| {
        let (Some(ca), Some(cb), Some(kp)) =
            (a.key_cells.get(k), b.key_cells.get(k), agg.keys.get(k))
        else {
            return std::cmp::Ordering::Equal;
        };
        // 型クラスとセルの不整合は束縛済みの不変条件に反する（比較不能は Equal に倒す）。
        let va = cmp_val(ca, &kp.cmp_class).ok().flatten();
        let vb = cmp_val(cb, &kp.cmp_class).ok().flatten();
        compare_order(va.as_ref(), vb.as_ref(), descending)
    };
    finished.sort_by(|a, b| {
        for k in 0..agg.keys.len() {
            let ord = key_cmp(a, b, k, false);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    if !agg.order.is_empty() {
        finished.sort_by(|a, b| {
            for o in &agg.order {
                let ord = match o.target {
                    AggOrderTarget::Key(k) => key_cmp(a, b, k, o.descending),
                    AggOrderTarget::Item(i) => match (a.item_cells.get(i), b.item_cells.get(i)) {
                        (Some(x), Some(y)) => {
                            crate::sql::group_by::cmp_cells_pg_nulls(x, y, o.descending)
                        }
                        _ => std::cmp::Ordering::Equal,
                    },
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    let start = offset.min(finished.len());
    let end = match limit {
        Some(l) => start.saturating_add(l).min(finished.len()),
        None => finished.len(),
    };
    let per_row_struct = agg
        .out
        .len()
        .saturating_mul(std::mem::size_of::<Cell>())
        .saturating_add(std::mem::size_of::<ResultRow>());
    let mut rows = Vec::with_capacity(end.saturating_sub(start));
    for f in finished.get(start..end).unwrap_or(&[]) {
        let mut cells = Vec::with_capacity(agg.out.len());
        for o in &agg.out {
            let cell = match o {
                AggOut::Key(k) => f.key_cells.get(*k),
                AggOut::Item(i) => f.item_cells.get(*i),
            }
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN aggregate output index out of range".to_string(),
            })?;
            cells.push(cell);
        }
        let payload: usize = cells.iter().map(|c| cell_payload_bytes(c)).sum();
        budget.charge(per_row_struct.saturating_add(payload))?;
        rows.push(ResultRow {
            id: 0,
            score: 0.0,
            cells: cells.into_iter().cloned().collect(),
        });
    }
    Ok(QueryResult {
        columns: agg.metas.clone(),
        rows,
    })
}
