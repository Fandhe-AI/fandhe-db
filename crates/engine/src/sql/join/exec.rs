//! N 方向ハッシュ結合の実行器（Issue #925・#926・#1190、SQL-28・RLS-10、
//! TASK-212）。
//!
//! 責務境界: `sql::join::execute_with_limits` が relation ごとに独立に走査した結果
//! （RLS 適用済みの可視行だけ）を受け取り、left-deep 連鎖の各段をハッシュ結合して
//! 「結合タプル」の列を返す。タプルは relation ごとに 1 つの走査位置
//! （`Option<u32>`。`None` は NULL 補完）を持つ。WHERE の残余評価・並べ替え・
//! 集計・投影は本モジュールの外（`residual`・`aggregate`・`mod.rs`）が担う。
//!
//! 上限（`54000`。いずれも可視行だけで判定し、`LIMIT`／`OFFSET` の値には依存させない）:
//! 段ごとの結合カーディナリティ（一致ペア＋NULL 補完行）が
//! [`super::JoinLimits::max_output_rows`] を超えたら、実体化する前に拒否する。確保は
//! すべて共有バイト予算（[`super::JoinBudget`]）へ確保前に計上する。
//!
//! 決定的順序: 最終タプル列は `(is_none_0, idx_0, is_none_1, idx_1, ...)` の安定
//! ソートで固定する（`scripts/check_sort_determinism.sh` ゲート対応。`sort_unstable*`
//! は使わない）。2 relation のときは従来の `(l.is_none(), l, r.is_none(), r)` と
//! 同一の順序になる（NULL 補完行は「無し」を「有り」より後に並べる）。ハッシュ表の
//! 反復順序は結果順に一切影響しない。

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ResultRow};

use super::plan::{StepKey, StepPlan};
use super::values::encode_key_component;
use super::{JoinBudget, JoinLimits};

/// 結合タプルの列（フラット配置。`n` は relation 数）。
pub(super) struct Tuples {
    pub(super) n: usize,
    data: Vec<Option<u32>>,
}

impl Tuples {
    pub(super) fn len(&self) -> usize {
        self.data.len().checked_div(self.n).unwrap_or(0)
    }

    /// `i` 番目のタプル（relation ごとの走査位置。範囲外は `None`）。
    pub(super) fn get(&self, i: usize) -> Option<&[Option<u32>]> {
        let start = i.checked_mul(self.n)?;
        let end = start.checked_add(self.n)?;
        self.data.get(start..end)
    }
}

const SLOT_BYTES: usize = std::mem::size_of::<Option<u32>>();

fn to_u32(v: usize, what: &str) -> Result<u32, SqlSurfaceError> {
    u32::try_from(v).map_err(|_| SqlSurfaceError::payload_too_large(what.to_string()))
}

/// 行から結合キー（複数列の連結）を抽出する。いずれかの成分が `NULL` の行は
/// 結合キーを持たない（`Ok(None)`）——ビルド・プローブいずれからも除外する
/// （Issue #925 §2.4。NULL は決して一致しない）。`row` が `None`（NULL 補完された
/// relation）のときもキーを持たない。
fn extract_key(
    row: Option<&ResultRow>,
    keys: &[StepKey],
    pos_of: impl Fn(&StepKey) -> usize,
) -> Result<Option<Vec<u8>>, SqlSurfaceError> {
    let row = match row {
        Some(r) => r,
        None => return Ok(None),
    };
    let mut out = Vec::new();
    for k in keys {
        let cell = row
            .cells
            .get(pos_of(k))
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN key position out of range in result row".to_string(),
            })?;
        if matches!(cell, Cell::Null) {
            return Ok(None);
        }
        encode_key_component(cell, &k.class, &mut out)?;
    }
    Ok(Some(out))
}

/// 結合連鎖を実行し、決定的な順序に並べたタプル列（実体＋順序の置換）を返す。
pub(super) fn run_joins(
    sides: &[Vec<ResultRow>],
    steps: &[StepPlan],
    budget: &mut JoinBudget,
    limits: &JoinLimits,
) -> Result<(Tuples, Vec<u32>), SqlSurfaceError> {
    let n = sides.len();
    let first = sides.first().ok_or_else(|| SqlSurfaceError::Internal {
        detail: "JOIN has no relations".to_string(),
    })?;
    budget.charge(first.len().saturating_mul(n).saturating_mul(SLOT_BYTES))?;
    let mut data: Vec<Option<u32>> = Vec::with_capacity(first.len().saturating_mul(n));
    for i in 0..first.len() {
        data.push(Some(to_u32(i, "JOIN row count exceeds limit")?));
        for _ in 1..n {
            data.push(None);
        }
    }
    let mut acc = Tuples { n, data };

    for (k, step) in steps.iter().enumerate() {
        let new_rel = k + 1;
        let new_rows = sides
            .get(new_rel)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN relation index out of range".to_string(),
            })?;
        acc = join_step(sides, step, new_rel, new_rows, &acc, budget, limits)?;
    }

    budget.charge(
        acc.len()
            .saturating_mul(std::mem::size_of::<u32>())
            .saturating_mul(2),
    )?;
    let mut order: Vec<u32> = Vec::with_capacity(acc.len());
    for i in 0..acc.len() {
        order.push(to_u32(i, "JOIN row count exceeds limit")?);
    }
    order.sort_by(|&a, &b| {
        let (ta, tb) = match (acc.get(a as usize), acc.get(b as usize)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Ordering::Equal,
        };
        for (x, y) in ta.iter().zip(tb.iter()) {
            let ord = (x.is_none(), x.unwrap_or(0)).cmp(&(y.is_none(), y.unwrap_or(0)));
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    Ok((acc, order))
}

/// 1 段のハッシュ結合。`acc`（relation 0..=k の結合結果）に relation `new_rel` を
/// 加える。新 relation 側をビルド側にし、蓄積タプルをプローブする。
fn join_step(
    sides: &[Vec<ResultRow>],
    step: &StepPlan,
    new_rel: usize,
    new_rows: &[ResultRow],
    acc: &Tuples,
    budget: &mut JoinBudget,
    limits: &JoinLimits,
) -> Result<Tuples, SqlSurfaceError> {
    let n = acc.n;
    let mut table: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
    for (idx, row) in new_rows.iter().enumerate() {
        let idx_u32 = to_u32(idx, "JOIN build side row count exceeds limit")?;
        if let Some(key) = extract_key(Some(row), &step.keys, |k| k.new_pos)? {
            budget.charge(key.len().saturating_add(std::mem::size_of::<u32>()))?;
            table.entry(key).or_default().push(idx_u32);
        }
    }

    let acc_len = acc.len();
    // 保存側だけ未一致フラグ配列を確保する（確保前に予算へ計上する）。
    let mut matched_acc: Vec<bool> = if step.preserve_acc {
        budget.charge(acc_len)?;
        vec![false; acc_len]
    } else {
        Vec::new()
    };
    let mut matched_new: Vec<bool> = if step.preserve_new {
        budget.charge(new_rows.len())?;
        vec![false; new_rows.len()]
    } else {
        Vec::new()
    };

    // 一致ペア数の上限判定（ループ内の早期打ち切り）は出力を実体化する前に行う。
    // 判定に使うのは可視行だけ（RLS・WHERE 適用済み）なので、他テナントの行数は
    // 結果に影響しない。
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for ti in 0..acc_len {
        let tuple = acc.get(ti).ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN accumulated tuple index out of range".to_string(),
        })?;
        // 複数条件の成分は relation が異なりうるため、成分ごとに行を引く。
        let mut key: Vec<u8> = Vec::new();
        let mut has_key = true;
        for k in &step.keys {
            let row = tuple
                .get(k.acc_rel)
                .copied()
                .flatten()
                .and_then(|i| sides.get(k.acc_rel).and_then(|rows| rows.get(i as usize)));
            match extract_key(row, std::slice::from_ref(k), |k| k.acc_pos)? {
                Some(part) => key.extend_from_slice(&part),
                None => {
                    has_key = false;
                    break;
                }
            }
        }
        if !has_key {
            continue;
        }
        budget.charge(key.len())?;
        if let Some(matched) = table.get(&key) {
            let ti_u32 = to_u32(ti, "JOIN row count exceeds limit")?;
            for &new_idx in matched {
                pairs.push((ti_u32, new_idx));
                if let Some(m) = matched_acc.get_mut(ti) {
                    *m = true;
                }
                if let Some(m) = matched_new.get_mut(new_idx as usize) {
                    *m = true;
                }
                if pairs.len() > limits.max_output_rows {
                    return Err(SqlSurfaceError::payload_too_large(
                        "JOIN result exceeds the row limit",
                    ));
                }
            }
        }
    }

    let unmatched_acc = matched_acc.iter().filter(|&&m| !m).count();
    let unmatched_new = matched_new.iter().filter(|&&m| !m).count();

    // 合計カーディナリティ（NULL 補完行を含む）を実体化する前に判定する。
    let total = pairs
        .len()
        .checked_add(unmatched_acc)
        .and_then(|t| t.checked_add(unmatched_new));
    let total = match total {
        Some(t) if t <= limits.max_output_rows => t,
        _ => {
            return Err(SqlSurfaceError::payload_too_large(
                "JOIN result exceeds the row limit",
            ));
        }
    };

    budget.charge(total.saturating_mul(n).saturating_mul(SLOT_BYTES))?;
    let mut data: Vec<Option<u32>> = Vec::with_capacity(total.saturating_mul(n));
    let mut push_tuple = |base: Option<&[Option<u32>]>, new_idx: Option<u32>| {
        for j in 0..n {
            if j == new_rel {
                data.push(new_idx);
            } else {
                data.push(base.and_then(|b| b.get(j).copied()).flatten());
            }
        }
    };
    for &(ti, ni) in &pairs {
        push_tuple(acc.get(ti as usize), Some(ni));
    }
    if step.preserve_acc {
        for (ti, &m) in matched_acc.iter().enumerate() {
            if !m {
                push_tuple(acc.get(ti), None);
            }
        }
    }
    if step.preserve_new {
        for (ni, &m) in matched_new.iter().enumerate() {
            if !m {
                push_tuple(None, Some(to_u32(ni, "JOIN row count exceeds limit")?));
            }
        }
    }
    Ok(Tuples { n, data })
}
