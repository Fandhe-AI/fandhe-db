//! 相関 `EXISTS` の半結合プローブ（SQL-61 ポインタ、Issue #1842、親 #1840。RLS-10 ポインタ）。
//!
//! 責務境界: `sql::correlated_exists` が内側クエリを 1 回だけ実体化（外側と同じ
//! `PolicyContext`・同じ `read_txn` の走査経路。RLS 暗黙適用）して作る [`CorrelatedProbe`]
//! を保持し、外側の行ごとに「内側に条件を満たす行が `OFFSET` 件を超えて存在するか」を判定する。
//! `WherePredicate::CorrelatedExists`（内部専用の葉）→ `BoundExpr::CorrelatedExists` →
//! `ExprStep::CorrelatedExists` の経路で、外側の走査（`sql::scan`・`sql::aggregate` 等）の
//! 式述語評価から [`CorrelatedProbe::eval`] が呼ばれる。第 2 の述語評価器は作らない。
//!
//! 2 つの評価方式（`sql::correlated_exists` が分解結果から選ぶ）:
//! - **ハッシュ半結合**: 等価の相関述語（`内側列 = 外側列`）だけで書ける形。内側行を結合キーの
//!   正準バイト列ごとの件数へまとめておき、外側の行は自身のキーで 1 回引くだけで判定する
//!   （`IN (SELECT ...)` の集合照合のような値数上限を受け継がない）。キーの型クラスと
//!   正準化は `sql::join::values` を再利用し、結合の 2 系統を作らない。
//! - **行ごとの再評価**: 非等価・外側と内側が混ざった `OR` などの残余述語がある形。等価キーが
//!   あれば該当バケットの内側行だけ、無ければ全内側行に対して残余を既存の述語評価器
//!   （`where_tree`）で評価する。再評価の回数と比較回数は文全体で共有する予算で頭打ちにし、
//!   超過は `54000` で文全体を拒否する（部分結果を返さない。fail-closed）。
//!
//! NULL 規則: 外側キーに NULL があれば偽（`EXISTS` は UNKNOWN にならない）。内側キーに NULL を
//! 含む行はバケットへ入れない（`=` は NULL で真にならない）。
//!
//! 情報漏えい対策: 内側行はすべて呼び出したセッションの可視行だけ。[`CorrelatedProbeRef`] の
//! `Debug` は行データを一切出さない。エラー文言は静的文字列のみ。バケットの反復順は結果に
//! 影響しない（バケット内は走査順の `Vec`、ハッシュ表は引くだけで走査しない）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::catalog::ColumnType;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::expr_program::StackValue;
use crate::sql::where_tree::BoundOrGroup;

use super::values::{cell_scalar, encode_key_component, key_cell_from_scalar, JoinKeyClass};

/// 相関 `EXISTS` の再評価回数の上限（実装既定値。Issue #1842・SQL-61 ポインタ、
/// 設計記録 `docs/design/sql-subquery.md`）。再評価 1 回 = 残余述語を評価する外側の行 1 行で、
/// 外側の入力行数の上限（`MAX_JOIN_INPUT_ROWS`）と同じ桁に揃える。文全体で 1 つの予算を
/// 共有し、超過は `54000`。CLI 化は対象外（`MAX_JOIN_COMPARISONS` の前例に倣う）。
pub(crate) const MAX_CORRELATED_REEVALUATIONS: usize = 100_000;

/// 文全体（入れ子の相関 `EXISTS` を含む）で共有する再評価・比較回数の予算。
/// `sql::correlated_exists::CorrelatedCtx` が 1 文の実行ごとに 1 つ作り、各プローブが
/// `Arc` で共有する。カウンタはスレッド間共有を想定しない（1 文は 1 スレッドで評価される）が、
/// `Arc` 越しに `&self` で更新するため原子型を使う。
#[derive(Debug)]
pub(crate) struct CorrelatedBudget {
    reevaluations: AtomicUsize,
    comparisons: AtomicUsize,
    max_reevaluations: usize,
    max_comparisons: usize,
}

impl CorrelatedBudget {
    pub(crate) fn new(max_reevaluations: usize, max_comparisons: usize) -> Self {
        Self {
            reevaluations: AtomicUsize::new(0),
            comparisons: AtomicUsize::new(0),
            max_reevaluations,
            max_comparisons,
        }
    }

    fn charge(
        counter: &AtomicUsize,
        n: usize,
        cap: usize,
        what: &'static str,
    ) -> Result<(), SqlSurfaceError> {
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(n).filter(|t| *t <= cap)
            })
            .map(|_| ())
            .map_err(|_| SqlSurfaceError::payload_too_large(what))
    }

    /// 残余述語を評価する外側の行 1 行ぶんを再評価 1 回として計上する。
    fn charge_reevaluation(&self) -> Result<(), SqlSurfaceError> {
        Self::charge(
            &self.reevaluations,
            1,
            self.max_reevaluations,
            "correlated subquery re-evaluation count exceeds the limit",
        )
    }

    /// 外側の行と内側の行の 1 組を比較 1 回として計上する。
    fn charge_comparison(&self) -> Result<(), SqlSurfaceError> {
        Self::charge(
            &self.comparisons,
            1,
            self.max_comparisons,
            "correlated subquery comparison count exceeds the limit",
        )
    }
}

/// 外側の行から値を取り出す元（外側スキーマ上の論理列インデックス、または行 `id`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OuterSource {
    Column(usize),
    Id,
}

/// 結合キー 1 成分の外側側の取り出し元とキーの型クラス。
#[derive(Debug, Clone)]
pub(crate) struct OuterKey {
    pub(crate) source: OuterSource,
    pub(crate) class: JoinKeyClass,
}

/// 内側の 1 行（残余述語の評価に必要な列だけを保持する）。内側の可視行の内容を持つため `Debug` は
/// 導出しない（エラー・ログ経由の漏えい防止）。
struct InnerRow {
    id: u64,
    cells: Vec<Cell>,
}

/// 内側行の保持形。残余が無い（半結合）ときは件数だけ、あるときは行そのものを持つ（`Debug` 非導出）。
enum ProbeRows {
    Counts(HashMap<Vec<u8>, usize>),
    Rows {
        rows: Vec<InnerRow>,
        by_key: HashMap<Vec<u8>, Vec<u32>>,
    },
}

/// 残余述語の評価器。内側スキーマに外側参照ごとの列を足した合成スキーマで束縛した結果を
/// 保持し、1 組ごとに合成行（内側スカラー ＋ 外側スカラー）を組んで `where_tree` を呼ぶ。
#[derive(Debug)]
pub(crate) struct Residual {
    pub(crate) group: BoundOrGroup,
    /// `(内側行のセル位置, 合成スキーマ上の列インデックス, 列型)`。
    pub(crate) inner_cols: Vec<(usize, usize, ColumnType)>,
    /// `(合成スキーマ上の列インデックス, 外側の取り出し元, 外側の列型)`。
    /// 疑似列 `id` の列型は `BigInt`（`u64` → `i64` は checked 変換）。
    pub(crate) outer_cols: Vec<(usize, OuterSource, ColumnType)>,
    /// 合成スキーマの列数。
    pub(crate) width: usize,
}

/// 外側の行ごとに判定する相関 `EXISTS` のプローブ。[`CorrelatedProbeRef`] 越しに共有される。
/// 内側の可視行を保持するため `Debug` は導出しない。
pub struct CorrelatedProbe {
    keys: Vec<OuterKey>,
    rows: ProbeRows,
    residual: Option<Residual>,
    /// 一致が必要な最小件数（内側 `OFFSET k` の意味の保存。`k + 1`）。
    min_matches: usize,
    budget: Arc<CorrelatedBudget>,
    /// 外側スキーマで評価に必要な論理列インデックス（キーと残余が参照する列。重複なし・昇順）。
    outer_columns: Vec<usize>,
}

/// 内側行の入力（`sql::correlated_exists` が走査結果から作る）。
pub(crate) struct InnerInput {
    pub(crate) id: u64,
    /// 結合キー成分のセル（`keys` と同じ順）。
    pub(crate) key_cells: Vec<Cell>,
    /// 残余の評価に必要な内側列のセル（`Residual::inner_cols` と同じ順）。
    pub(crate) cells: Vec<Cell>,
}

/// プローブの構築パラメータ。
pub(crate) struct ProbeSpec {
    pub(crate) keys: Vec<OuterKey>,
    pub(crate) residual: Option<Residual>,
    pub(crate) min_matches: usize,
    pub(crate) budget: Arc<CorrelatedBudget>,
    /// 内側行の保持に使える推定バイト量の上限（確保前に計上して超過なら `54000`）。
    pub(crate) byte_cap: usize,
}

fn key_bytes_overhead(key: &[u8]) -> usize {
    key.len()
        .saturating_add(std::mem::size_of::<Vec<u8>>())
        .saturating_add(std::mem::size_of::<usize>())
}

fn too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("correlated subquery input exceeds the byte budget")
}

impl CorrelatedProbe {
    /// 内側行（可視行だけ）からプローブを組み立てる。キーに NULL を含む行は入れない。
    /// 確保は予算へ計上してから行い、超過は `54000`（`try_reserve` で失敗しても同じ）。
    pub(crate) fn build(
        spec: ProbeSpec,
        inputs: impl IntoIterator<Item = Result<InnerInput, SqlSurfaceError>>,
    ) -> Result<Self, SqlSurfaceError> {
        let ProbeSpec {
            keys,
            residual,
            min_matches,
            budget,
            byte_cap,
        } = spec;
        let mut used = 0usize;
        let mut charge = |n: usize| -> Result<(), SqlSurfaceError> {
            used = used.saturating_add(n);
            if used > byte_cap {
                return Err(too_large());
            }
            Ok(())
        };
        let mut counts: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut rows: Vec<InnerRow> = Vec::new();
        let mut by_key: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
        let keep_rows = residual.is_some();
        'rows: for input in inputs {
            let input = input?;
            let mut key = Vec::new();
            if input.key_cells.len() != keys.len() {
                return Err(SqlSurfaceError::Internal {
                    detail: "correlated key arity mismatch".to_string(),
                });
            }
            for (cell, k) in input.key_cells.iter().zip(&keys) {
                if matches!(cell, Cell::Null) {
                    continue 'rows;
                }
                encode_key_component(cell, &k.class, &mut key)?;
            }
            if keep_rows {
                let idx = u32::try_from(rows.len()).map_err(|_| too_large())?;
                let cell_bytes = input
                    .cells
                    .iter()
                    .map(super::cell_payload_bytes)
                    .fold(0usize, usize::saturating_add);
                charge(
                    std::mem::size_of::<InnerRow>()
                        .saturating_add(
                            input
                                .cells
                                .len()
                                .saturating_mul(std::mem::size_of::<Cell>()),
                        )
                        .saturating_add(cell_bytes)
                        .saturating_add(std::mem::size_of::<u32>()),
                )?;
                rows.try_reserve(1).map_err(|_| too_large())?;
                rows.push(InnerRow {
                    id: input.id,
                    cells: input.cells,
                });
                match by_key.get_mut(&key) {
                    Some(v) => {
                        v.try_reserve(1).map_err(|_| too_large())?;
                        v.push(idx);
                    }
                    None => {
                        charge(key_bytes_overhead(&key))?;
                        by_key.try_reserve(1).map_err(|_| too_large())?;
                        by_key.insert(key, vec![idx]);
                    }
                }
            } else {
                match counts.get_mut(&key) {
                    Some(n) => *n = n.saturating_add(1),
                    None => {
                        charge(key_bytes_overhead(&key))?;
                        counts.try_reserve(1).map_err(|_| too_large())?;
                        counts.insert(key, 1);
                    }
                }
            }
        }
        let mut outer_columns: Vec<usize> = keys
            .iter()
            .filter_map(|k| match k.source {
                OuterSource::Column(i) => Some(i),
                OuterSource::Id => None,
            })
            .collect();
        if let Some(r) = &residual {
            outer_columns.extend(r.outer_cols.iter().filter_map(|(_, src, _)| match src {
                OuterSource::Column(i) => Some(*i),
                OuterSource::Id => None,
            }));
        }
        outer_columns.sort();
        outer_columns.dedup();
        Ok(Self {
            keys,
            rows: if keep_rows {
                ProbeRows::Rows { rows, by_key }
            } else {
                ProbeRows::Counts(counts)
            },
            residual,
            min_matches,
            budget,
            outer_columns,
        })
    }

    /// 外側スキーマで評価に必要な論理列インデックス（デコード対象へ含める用）。
    pub(crate) fn outer_columns(&self) -> &[usize] {
        &self.outer_columns
    }

    /// 外側の 1 行に対して判定する。`row_scalars` は外側スキーマの論理列インデックスで引く
    /// 行スカラー値のビュー、`id` は外側の行 `id`。結果は常に真偽（UNKNOWN にならない）。
    pub(crate) fn eval(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        let mut key = Vec::new();
        for k in &self.keys {
            let scalar = match k.source {
                OuterSource::Id => {
                    encode_key_component(&Cell::Integer(id), &k.class, &mut key)?;
                    continue;
                }
                OuterSource::Column(i) => outer_scalar(row_scalars, i)?,
            };
            let Some(scalar) = scalar else {
                // 外側キーが NULL: `=` は真にならないため偽（EXISTS は UNKNOWN にならない）。
                return Ok(false);
            };
            let cell = key_cell_from_scalar(&scalar, &k.class)?;
            encode_key_component(&cell, &k.class, &mut key)?;
        }
        match &self.rows {
            ProbeRows::Counts(map) => Ok(map.get(&key).is_some_and(|n| *n >= self.min_matches)),
            ProbeRows::Rows { rows, by_key } => {
                let Some(indices) = by_key.get(&key) else {
                    return Ok(false);
                };
                // 残余を持たない `Rows` は構築されない不変条件。破れたら fail-closed。
                let residual = self
                    .residual
                    .as_ref()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "correlated probe has rows without a residual".to_string(),
                    })?;
                self.budget.charge_reevaluation()?;
                self.count_matches(rows, indices, residual, row_scalars, id)
            }
        }
    }

    fn count_matches(
        &self,
        rows: &[InnerRow],
        indices: &[u32],
        residual: &Residual,
        row_scalars: &[Option<ScalarRef<'_>>],
        outer_id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        let mut scanned: Vec<Option<ScalarRef<'_>>> = vec![None; residual.width];
        for (slot, source, _ty) in &residual.outer_cols {
            let value = match source {
                OuterSource::Column(i) => outer_scalar(row_scalars, *i)?,
                OuterSource::Id => Some(ScalarRef::BigInt(i64::try_from(outer_id).map_err(
                    |_| SqlSurfaceError::numeric_out_of_range("outer row id is out of range"),
                )?)),
            };
            if let Some(dst) = scanned.get_mut(*slot) {
                *dst = value;
            } else {
                return Err(SqlSurfaceError::Internal {
                    detail: "correlated residual slot out of range".to_string(),
                });
            }
        }
        let mut scratch: Vec<StackValue> = Vec::new();
        let mut matched = 0usize;
        for &idx in indices {
            self.budget.charge_comparison()?;
            let row = rows
                .get(idx as usize)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated probe row index out of range".to_string(),
                })?;
            for (cell_pos, slot, ty) in &residual.inner_cols {
                let cell = row
                    .cells
                    .get(*cell_pos)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "correlated probe cell position out of range".to_string(),
                    })?;
                let value = cell_scalar(cell, ty)?;
                match scanned.get_mut(*slot) {
                    Some(dst) => *dst = value,
                    None => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "correlated residual slot out of range".to_string(),
                        })
                    }
                }
            }
            if residual
                .group
                .matches(&scanned, row.id, &[], 0, &mut scratch)?
            {
                matched = matched.saturating_add(1);
                if matched >= self.min_matches {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// 外側の行スカラーから `index` 列を取り出す。範囲外は配線不備として fail-closed
/// （実 NULL と取り違えない）。
fn outer_scalar<'a>(
    row_scalars: &[Option<ScalarRef<'a>>],
    index: usize,
) -> Result<Option<ScalarRef<'a>>, SqlSurfaceError> {
    row_scalars
        .get(index)
        .copied()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "correlated outer column is outside the decoded row scalar view".to_string(),
        })
}

/// [`CorrelatedProbe`] の共有ハンドル。`WherePredicate`／`BoundExpr` の内部専用の葉が保持する。
///
/// `PartialEq`／`Eq` は同一プローブか（`Arc::ptr_eq`）で判定し、`Debug` は行データを出さない
/// （エラー・ログ経由で内側の可視行の内容が漏れないようにする。RLS-10 ポインタ）。
#[derive(Clone)]
pub struct CorrelatedProbeRef(Arc<CorrelatedProbe>);

impl CorrelatedProbeRef {
    pub(crate) fn new(probe: CorrelatedProbe) -> Self {
        Self(Arc::new(probe))
    }

    pub(crate) fn probe(&self) -> &CorrelatedProbe {
        &self.0
    }
}

impl PartialEq for CorrelatedProbeRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CorrelatedProbeRef {}

impl std::fmt::Debug for CorrelatedProbeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CorrelatedProbeRef(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Arc<CorrelatedBudget> {
        Arc::new(CorrelatedBudget::new(
            MAX_CORRELATED_REEVALUATIONS,
            super::super::MAX_JOIN_COMPARISONS,
        ))
    }

    fn int_key() -> Vec<OuterKey> {
        vec![OuterKey {
            source: OuterSource::Column(0),
            class: JoinKeyClass::Integer,
        }]
    }

    fn input(key: Cell) -> InnerInput {
        InnerInput {
            id: 0,
            key_cells: vec![key],
            cells: Vec::new(),
        }
    }

    fn probe(min_matches: usize, keys: Vec<Cell>) -> CorrelatedProbe {
        CorrelatedProbe::build(
            ProbeSpec {
                keys: int_key(),
                residual: None,
                min_matches,
                budget: budget(),
                byte_cap: usize::MAX,
            },
            keys.into_iter().map(|c| Ok(input(c))),
        )
        .expect("build")
    }

    #[test]
    fn semi_join_matches_across_integer_classes_and_skips_null_keys() {
        let p = probe(
            1,
            vec![Cell::SignedInteger(7), Cell::Integer(9), Cell::Null],
        );
        assert!(p.eval(&[Some(ScalarRef::Integer(7))], 1).unwrap());
        assert!(p.eval(&[Some(ScalarRef::BigInt(9))], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(8))], 1).unwrap());
        // 外側キーが NULL の行は偽（UNKNOWN にならない）。
        assert!(!p.eval(&[None], 1).unwrap());
    }

    #[test]
    fn semi_join_has_no_distinct_value_cap() {
        let keys: Vec<Cell> = (0..2_000_i64).map(Cell::SignedInteger).collect();
        let p = probe(1, keys);
        assert!(p.eval(&[Some(ScalarRef::BigInt(1_999))], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(2_000))], 1).unwrap());
    }

    #[test]
    fn min_matches_models_inner_offset() {
        let p = probe(
            2,
            vec![
                Cell::SignedInteger(1),
                Cell::SignedInteger(1),
                Cell::SignedInteger(2),
            ],
        );
        assert!(p.eval(&[Some(ScalarRef::BigInt(1))], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(2))], 1).unwrap());
    }

    #[test]
    fn out_of_range_outer_column_fails_closed() {
        let p = probe(1, vec![Cell::SignedInteger(1)]);
        let e = p.eval(&[], 1).expect_err("range");
        assert_eq!(e.wire_code(), "XX000");
    }

    #[test]
    fn byte_budget_is_charged_before_allocation() {
        let e = CorrelatedProbe::build(
            ProbeSpec {
                keys: int_key(),
                residual: None,
                min_matches: 1,
                budget: budget(),
                byte_cap: 8,
            },
            (0..10_i64).map(|i| Ok(input(Cell::SignedInteger(i)))),
        )
        .err()
        .expect("over budget");
        assert_eq!(e.wire_code(), "54000");
    }

    #[test]
    fn budget_counters_reject_past_the_cap() {
        let b = CorrelatedBudget::new(2, 3);
        assert!(b.charge_reevaluation().is_ok());
        assert!(b.charge_reevaluation().is_ok());
        assert_eq!(b.charge_reevaluation().unwrap_err().wire_code(), "54000");
        for _ in 0..3 {
            assert!(b.charge_comparison().is_ok());
        }
        assert_eq!(b.charge_comparison().unwrap_err().wire_code(), "54000");
    }

    #[test]
    fn probe_ref_debug_hides_data_and_eq_is_identity() {
        let a = CorrelatedProbeRef::new(probe(1, vec![Cell::SignedInteger(42)]));
        let b = a.clone();
        let c = CorrelatedProbeRef::new(probe(1, vec![Cell::SignedInteger(42)]));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(!format!("{a:?}").contains("42"));
    }

    #[test]
    fn expression_leaf_agrees_between_recursive_and_compiled_evaluators() {
        use crate::sql::expr_program::ExprProgram;
        use crate::sql::udf_call::{
            eval_with_scalars, mark_referenced_scalar_columns, visit_referenced_scalar_columns,
            BoundExpr, ExprValue,
        };
        let probe = CorrelatedProbeRef::new(probe(1, vec![Cell::SignedInteger(7)]));
        let expr = BoundExpr::CorrelatedExists(probe);
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        let rows: Vec<Vec<Option<ScalarRef<'_>>>> = vec![
            vec![Some(ScalarRef::BigInt(7))],
            vec![Some(ScalarRef::BigInt(8))],
            vec![None],
        ];
        for row in &rows {
            let recursive = eval_with_scalars(&expr, 1, &[], row).expect("recursive");
            let compiled = program.eval(1, &[], row, &mut scratch).expect("compiled");
            assert_eq!(recursive, compiled);
            assert!(matches!(recursive, ExprValue::Bool(_)));
        }
        // 外側キー列がデコード対象に含まれる（欠けるとマスク外参照になる）。
        let mut mask = [false; 3];
        assert!(mark_referenced_scalar_columns(&expr, &mut mask));
        assert_eq!(mask, [true, false, false]);
        let mut visited = Vec::new();
        visit_referenced_scalar_columns(&expr, &mut |i| visited.push(i));
        assert_eq!(visited, vec![0]);
    }
}
