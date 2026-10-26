//! 等価キーを持たない ON 残余のある段のネステッドループ結合（NLJ）の実行器
//! （Issue #1837、SQL-60 ポインタ。設計記録 `docs/design/nested-loop-join.md` の D2〜D9・D12）。
//!
//! 責務境界: `exec::join_step` が「キー無し・ON 残余あり」の段だけをここへ振り分ける
//! （D1）。受け取るのは `execute_with_limits` が RLS 適用後に走査した可視行（`sides`）だけで、
//! 比較回数・事前判定の積はいずれも自テナントの可視データだけで決まる。述語の評価は
//! `residual::eval_all`（`ResNode`）を共有し、第 2 の評価器は作らない。
//!
//! 現状は INNER のみ。外部結合（`preserve_acc`／`preserve_new`）の未一致追跡は #1838、
//! SQL からの到達（束縛段の `42601` 解除）は #1839 で行うため、外部結合の段は黙って INNER として
//! 実行せず fail-closed の内部エラーで拒否する。
//!
//! 上限（いずれも `54000`。部分結果は返さない）: 比較回数は文全体で 1 つの予算
//! （[`super::JoinBudget::charge_comparisons`]。D5）で、`on_mixed` の評価前に
//! 「通過 acc 数 × 通過 new 数」を残り予算と照合する（D6）。一致ペア数は既存の出力上限・
//! 最終段の停止閾値で判定する（D7）。確保は確保前に共有バイト予算へ計上する（D8）。
//! 内側ループで `crate::interrupt::check()` を呼び `57014` に協調する（D9）。
//! 最終順序は `exec::run_joins` の安定ソートが決めるため、ループ順は結果に影響しない（D12）。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::ResultRow;
use crate::sql::expr_program::StackValue;

use super::exec::{push_charged_pair, to_u32, Tuples, SLOT_BYTES};
use super::plan::StepPlan;
use super::residual::eval_all;
use super::{JoinBudget, JoinLimits};

fn relation_out_of_range() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "JOIN relation index out of range".to_string(),
    }
}

/// NLJ 1 段（INNER）。`acc`（relation 0..=k の結合結果）に relation `new_rel` を加える。
/// 引数は `exec::join_step` と同じ（呼び出し元は `join_step` のみ）。
// `join_step` と同じ 8 引数（最終段の停止閾値 Issue #1797 を含む）。
#[allow(clippy::too_many_arguments)]
pub(super) fn nlj_step(
    sides: &[Vec<ResultRow>],
    step: &StepPlan,
    new_rel: usize,
    new_rows: &[ResultRow],
    acc: &Tuples,
    budget: &mut JoinBudget,
    limits: &JoinLimits,
    stop_after_pairs: Option<usize>,
) -> Result<Tuples, SqlSurfaceError> {
    // 外部結合の未一致追跡は #1838。ここで INNER として黙って返すと行が欠落する（fail-open）ため拒否する。
    if step.preserve_acc || step.preserve_new {
        return Err(SqlSurfaceError::Internal {
            detail: "nested-loop join does not support outer joins yet".to_string(),
        });
    }
    if !step.keys.is_empty() {
        return Err(SqlSurfaceError::Internal {
            detail: "nested-loop join called with equi-join keys".to_string(),
        });
    }
    let n = acc.n;
    if new_rel >= n {
        return Err(relation_out_of_range());
    }
    let mut scratch: Vec<StackValue> = Vec::new();
    budget.charge(n.saturating_mul(SLOT_BYTES))?;
    let mut probe: Vec<Option<u32>> = vec![None; n];

    // 外側: `on_acc` を各蓄積タプルに 1 回ずつ評価して通過した位置だけを残す。
    let acc_len = acc.len();
    budget.charge(acc_len.saturating_mul(std::mem::size_of::<u32>()))?;
    let mut outer: Vec<u32> = Vec::with_capacity(acc_len);
    for ti in 0..acc_len {
        crate::interrupt::check()?;
        let tuple = acc.get(ti).ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN accumulated tuple index out of range".to_string(),
        })?;
        if step.on_acc.is_empty() || eval_all(&step.on_acc, sides, tuple, &mut scratch)? {
            outer.push(to_u32(ti, "JOIN row count exceeds limit")?);
        }
    }

    // 内側: `on_new` を新 relation の各行に 1 回だけ評価して通過した位置の一覧を作る（D2）。
    budget.charge(new_rows.len().saturating_mul(std::mem::size_of::<u32>()))?;
    let mut inner: Vec<u32> = Vec::with_capacity(new_rows.len());
    for idx in 0..new_rows.len() {
        crate::interrupt::check()?;
        let idx_u32 = to_u32(idx, "JOIN build side row count exceeds limit")?;
        if !step.on_new.is_empty() {
            probe.fill(None);
            let slot = probe.get_mut(new_rel).ok_or_else(relation_out_of_range)?;
            *slot = Some(idx_u32);
            if !eval_all(&step.on_new, sides, &probe, &mut scratch)? {
                continue;
            }
        }
        inner.push(idx_u32);
    }

    // 事前判定（確保・評価の前。可視行の件数だけで決まり、LIMIT／OFFSET に依存しない）。
    let product = outer.len().checked_mul(inner.len());
    if step.on_mixed.is_empty() {
        // D3: 比較 0 回。出力件数（通過 acc × 通過 new）を出力上限・停止閾値で判定する。
        match product {
            Some(p) if p <= limits.max_output_rows => {
                if matches!(stop_after_pairs, Some(t) if p > t) {
                    return Err(crate::sql::scan::row_limit_exceeded());
                }
            }
            _ => {
                return Err(SqlSurfaceError::payload_too_large(
                    "JOIN result exceeds the row limit",
                ));
            }
        }
    } else {
        // D6: 比較回数の予算を超えるなら 1 件も評価せずに拒否する。
        match product {
            Some(p) if p <= budget.remaining_comparisons(limits.max_comparisons) => {}
            _ => {
                return Err(SqlSurfaceError::payload_too_large(
                    "JOIN comparison count exceeds the limit",
                ));
            }
        }
    }

    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for &ti in &outer {
        let tuple = acc
            .get(ti as usize)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN accumulated tuple index out of range".to_string(),
            })?;
        for &ni in &inner {
            crate::interrupt::check()?;
            if !step.on_mixed.is_empty() {
                // 安全網（事前判定を通っていれば超えない）。
                budget.charge_comparisons(1, limits.max_comparisons)?;
                for (j, slot) in probe.iter_mut().enumerate() {
                    *slot = tuple.get(j).copied().flatten();
                }
                let slot = probe.get_mut(new_rel).ok_or_else(relation_out_of_range)?;
                *slot = Some(ni);
                if !eval_all(&step.on_mixed, sides, &probe, &mut scratch)? {
                    continue;
                }
            }
            push_charged_pair(&mut pairs, budget, (ti, ni))?;
            if pairs.len() > limits.max_output_rows {
                return Err(SqlSurfaceError::payload_too_large(
                    "JOIN result exceeds the row limit",
                ));
            }
            if matches!(stop_after_pairs, Some(t) if pairs.len() > t) {
                return Err(crate::sql::scan::row_limit_exceeded());
            }
        }
    }

    // 実体化（INNER なので一致ペアだけ）。
    let total = pairs.len();
    budget.charge(total.saturating_mul(n).saturating_mul(SLOT_BYTES))?;
    let mut data: Vec<Option<u32>> = Vec::with_capacity(total.saturating_mul(n));
    for &(ti, ni) in &pairs {
        let base = acc
            .get(ti as usize)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN accumulated tuple index out of range".to_string(),
            })?;
        for j in 0..n {
            if j == new_rel {
                data.push(Some(ni));
            } else {
                data.push(base.get(j).copied().flatten());
            }
        }
    }
    Tuples::from_flat(n, data)
}

#[cfg(test)]
mod nlj_tests {
    use super::super::exec::run_joins;
    use super::super::plan::{ColCompare, ResNode};
    use super::super::values::CmpClass;
    use super::*;
    use crate::sql::exec::Cell;
    use crate::sql::udf_call::BinOp;

    fn rel(vals: &[&[Option<u64>]]) -> Vec<ResultRow> {
        vals.iter()
            .enumerate()
            .map(|(i, cs)| ResultRow {
                id: i as u64,
                score: 0.0,
                cells: cs
                    .iter()
                    .map(|c| c.map_or(Cell::Null, Cell::Integer))
                    .collect(),
            })
            .collect()
    }

    fn col(v: &[u64]) -> Vec<ResultRow> {
        let cells: Vec<[Option<u64>; 1]> = v.iter().map(|x| [Some(*x)]).collect();
        let refs: Vec<&[Option<u64>]> = cells.iter().map(|c| c.as_slice()).collect();
        rel(&refs)
    }

    fn cmp(lhs: (usize, usize), op: BinOp, rhs: (usize, usize)) -> ResNode {
        ResNode::Compare(ColCompare {
            lhs,
            rhs,
            op,
            class: CmpClass::Integer,
        })
    }

    fn step(on_acc: Vec<ResNode>, on_new: Vec<ResNode>, on_mixed: Vec<ResNode>) -> StepPlan {
        StepPlan {
            preserve_acc: false,
            preserve_new: false,
            keys: Vec::new(),
            on_acc,
            on_new,
            on_mixed,
        }
    }

    fn limits(max_output_rows: usize, max_comparisons: usize) -> JoinLimits {
        JoinLimits {
            max_output_rows,
            max_comparisons,
            ..JoinLimits::default()
        }
    }

    fn run_one(
        sides: &[Vec<ResultRow>],
        st: &StepPlan,
        lim: &JoinLimits,
        stop: Option<usize>,
    ) -> (Result<Tuples, SqlSurfaceError>, JoinBudget) {
        // 実行時と同じく、タプル幅は relation 総数（2）で、新 relation の位置は NULL 補完で始まる。
        let data: Vec<Option<u32>> = (0..sides[0].len() as u32)
            .flat_map(|i| [Some(i), None])
            .collect();
        let acc = Tuples::from_flat(2, data).unwrap();
        let mut budget = JoinBudget::new(usize::MAX / 2);
        let r = nlj_step(sides, st, 1, &sides[1], &acc, &mut budget, lim, stop);
        (r, budget)
    }

    fn pairs_of(t: &Tuples) -> Vec<(u32, u32)> {
        (0..t.len())
            .map(|i| {
                let x = t.get(i).unwrap();
                (x[0].unwrap(), x[1].unwrap())
            })
            .collect()
    }

    /// SQL-60: `a.x < b.y` が期待どおりのペア集合になり、比較回数は積に等しい。
    #[test]
    fn less_than_produces_expected_pairs() {
        let sides = vec![col(&[1, 2, 3]), col(&[2, 3])];
        let st = step(vec![], vec![], vec![cmp((0, 0), BinOp::Lt, (1, 0))]);
        let (r, b) = run_one(&sides, &st, &limits(100, 100), None);
        assert_eq!(pairs_of(&r.unwrap()), vec![(0, 0), (0, 1), (1, 1)]);
        assert_eq!(b.comparisons_used, 6);
    }

    /// SQL-60: `<=`・`>`・`>=` の各演算子。
    #[test]
    fn other_operators_work() {
        let sides = vec![col(&[1, 2, 3]), col(&[2, 3])];
        for (op, want) in [
            (BinOp::Le, vec![(0, 0), (0, 1), (1, 0), (1, 1), (2, 1)]),
            (BinOp::Gt, vec![(2, 0)]),
            (BinOp::Ge, vec![(1, 0), (2, 0), (2, 1)]),
        ] {
            let st = step(vec![], vec![], vec![cmp((0, 0), op, (1, 0))]);
            let (r, _) = run_one(&sides, &st, &limits(100, 100), None);
            assert_eq!(pairs_of(&r.unwrap()), want, "{op:?}");
        }
    }

    /// SQL-60: BETWEEN 相当（`a.x >= b.lo AND a.x <= b.hi`）。
    #[test]
    fn between_like_two_compares() {
        let a = col(&[1, 5, 9]);
        let b = rel(&[&[Some(0), Some(2)], &[Some(4), Some(9)]]);
        let sides = vec![a, b];
        let st = step(
            vec![],
            vec![],
            vec![
                cmp((0, 0), BinOp::Ge, (1, 0)),
                cmp((0, 0), BinOp::Le, (1, 1)),
            ],
        );
        let (r, _) = run_one(&sides, &st, &limits(100, 100), None);
        assert_eq!(pairs_of(&r.unwrap()), vec![(0, 0), (1, 1), (2, 1)]);
    }

    /// SQL-60: NULL セルは一致しない（3 値論理で偽）。
    #[test]
    fn null_cells_never_match() {
        let a = rel(&[&[None], &[Some(1)]]);
        let b = rel(&[&[Some(5)], &[None]]);
        let sides = vec![a, b];
        let st = step(vec![], vec![], vec![cmp((0, 0), BinOp::Lt, (1, 0))]);
        let (r, _) = run_one(&sides, &st, &limits(100, 100), None);
        assert_eq!(pairs_of(&r.unwrap()), vec![(1, 0)]);
    }

    /// SQL-60: 片側述語の通過分だけが比較対象になり、比較回数は通過件数の積に等しい。
    #[test]
    fn single_side_predicates_narrow_comparisons() {
        let a = rel(&[
            &[Some(1), Some(2)],
            &[Some(5), Some(2)],
            &[Some(1), Some(9)],
        ]);
        let b = rel(&[
            &[Some(3), Some(4)],
            &[Some(8), Some(4)],
            &[Some(7), Some(1)],
        ]);
        let sides = vec![a, b];
        let st = step(
            vec![cmp((0, 0), BinOp::Le, (0, 1))],
            vec![cmp((1, 0), BinOp::Le, (1, 1))],
            vec![cmp((0, 0), BinOp::Lt, (1, 0))],
        );
        let (r, b) = run_one(&sides, &st, &limits(100, 100), None);
        // outer = {0, 2}（1 は 5<=2 が偽）、inner = {0}（3<=4 のみ真）
        assert_eq!(b.comparisons_used, 2);
        assert_eq!(pairs_of(&r.unwrap()), vec![(0, 0), (2, 0)]);
    }

    /// SQL-60 (D3): `on_mixed` が空の段は比較 0 回で、積が出力上限超過なら確保前に `54000`。
    #[test]
    fn no_mixed_predicate_checks_product_before_allocation() {
        let sides = vec![col(&[1, 2, 3]), col(&[1, 2])];
        let st = step(vec![], vec![cmp((1, 0), BinOp::Le, (1, 0))], vec![]);
        let (r, b) = run_one(&sides, &st, &limits(5, 100), None);
        assert_eq!(r.err().unwrap().wire_code(), "54000");
        assert_eq!(b.comparisons_used, 0);
        let (r, b) = run_one(&sides, &st, &limits(6, 100), None);
        assert_eq!(r.unwrap().len(), 6);
        assert_eq!(b.comparisons_used, 0);
        let (r, _) = run_one(&sides, &st, &limits(100, 100), Some(5));
        assert_eq!(r.err().unwrap().wire_code(), "54000");
    }

    /// SQL-60 (D4・D6): 比較回数の上限超過は評価前に `54000`。境界ちょうどは成功する。
    #[test]
    fn comparison_cap_rejected_before_evaluation() {
        let sides = vec![col(&[1, 2, 3]), col(&[2, 3])];
        let st = step(vec![], vec![], vec![cmp((0, 0), BinOp::Lt, (1, 0))]);
        let (r, b) = run_one(&sides, &st, &limits(100, 5), None);
        assert_eq!(r.err().unwrap().wire_code(), "54000");
        assert_eq!(b.comparisons_used, 0, "must be rejected before evaluating");
        let (r, b) = run_one(&sides, &st, &limits(100, 6), None);
        assert!(r.is_ok());
        assert_eq!(b.comparisons_used, 6);
    }

    /// SQL-60 (D5・D12): 比較予算は文全体で共有され、最終順序はタプルの辞書順で決まる。
    #[test]
    fn comparison_budget_is_shared_across_steps() {
        let sides = vec![col(&[0, 1, 2]), col(&[1, 2, 3]), col(&[1, 2, 3])];
        let steps = vec![
            step(vec![], vec![], vec![cmp((0, 0), BinOp::Lt, (1, 0))]),
            step(vec![], vec![], vec![cmp((1, 0), BinOp::Lt, (2, 0))]),
        ];
        // 1 段目は 9 回、2 段目は 6 ペア × 3 = 18 回、合計 27 回（各段は単独なら 20 以内）。
        let mut budget = JoinBudget::new(usize::MAX / 2);
        let err = run_joins(&sides, &steps, &mut budget, &limits(100, 20), None)
            .err()
            .expect("27 > 20");
        assert_eq!(err.wire_code(), "54000");
        let mut budget = JoinBudget::new(usize::MAX / 2);
        let (t, order) = run_joins(&sides, &steps, &mut budget, &limits(100, 27), None).unwrap();
        assert_eq!(budget.comparisons_used, 27);
        let got: Vec<Vec<Option<u32>>> = order
            .iter()
            .map(|&i| t.get(i as usize).unwrap().to_vec())
            .collect();
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted);
        assert!(!got.is_empty());
    }

    /// SQL-60: 一致ペアが出力上限を超えると `54000`、停止閾値を超えると行数上限エラー。
    #[test]
    fn output_cap_and_final_stop() {
        let sides = vec![col(&[1, 2, 3]), col(&[2, 3])];
        let st = step(vec![], vec![], vec![cmp((0, 0), BinOp::Lt, (1, 0))]);
        let (r, _) = run_one(&sides, &st, &limits(2, 100), None);
        assert_eq!(r.err().unwrap().wire_code(), "54000");
        let (r, _) = run_one(&sides, &st, &limits(100, 100), Some(2));
        assert_eq!(
            r.err().unwrap().wire_code(),
            crate::sql::scan::row_limit_exceeded().wire_code()
        );
    }

    /// SQL-60 (D9): 中断要求は `57014` になる。
    #[test]
    fn interrupt_yields_57014() {
        let sides = vec![col(&[1, 2, 3]), col(&[2, 3])];
        let st = step(vec![], vec![], vec![cmp((0, 0), BinOp::Lt, (1, 0))]);
        let _g = crate::interrupt::InterruptScope::install(Box::new(|| {
            Some(crate::interrupt::InterruptReason::ClientGone)
        }));
        let (r, _) = run_one(&sides, &st, &limits(100, 100), None);
        assert_eq!(r.err().unwrap().wire_code(), "57014");
    }

    /// 外部結合の段は黙って INNER として実行せず fail-closed で拒否する（#1838 で置換）。
    #[test]
    fn outer_join_step_is_rejected() {
        let sides = vec![col(&[1]), col(&[2])];
        for (pa, pn) in [(true, false), (false, true)] {
            let mut st = step(vec![], vec![], vec![cmp((0, 0), BinOp::Lt, (1, 0))]);
            st.preserve_acc = pa;
            st.preserve_new = pn;
            let (r, _) = run_one(&sides, &st, &limits(100, 100), None);
            assert!(r.is_err());
        }
    }

    /// D4 の実測用（時間は出力するだけで検証しない）。件数と比較回数の正しさだけを検証する。
    #[test]
    fn nlj_comparison_throughput_probe() {
        let side: u64 = if cfg!(debug_assertions) { 316 } else { 3163 };
        let v: Vec<u64> = (0..side).collect();
        let sides = vec![col(&v), col(&v)];
        // 常に偽になる述語（`x < y` かつ `x > y`）。AND の短絡で 2 つ目が評価される割合は約半数。
        let st = step(
            vec![],
            vec![],
            vec![
                cmp((0, 0), BinOp::Lt, (1, 0)),
                cmp((0, 0), BinOp::Gt, (1, 0)),
            ],
        );
        let cap = (side * side) as usize;
        let start = std::time::Instant::now();
        let (r, b) = run_one(&sides, &st, &limits(100, cap), None);
        let elapsed = start.elapsed();
        assert_eq!(r.unwrap().len(), 0);
        assert_eq!(b.comparisons_used, cap);
        eprintln!("nlj probe: {cap} comparisons in {elapsed:?}");
    }
}
