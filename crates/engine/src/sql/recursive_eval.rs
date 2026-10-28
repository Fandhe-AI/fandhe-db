//! `WITH RECURSIVE` の反復評価器（作業表方式。SQL-35・Issue #1870）。
//!
//! 責務境界: `sql::allowlist` が検証した [`ValidatedRecursiveCte`]（アンカー項と再帰項の
//! 検証済み `Statement`）を受け取り、`core.rs` の `Statement::RecursiveCte` 腕から呼ばれる。
//! 本モジュールは次の 3 つだけを担う。
//!
//! 1. 計画（[`describe`]・[`execute`] が共有）: アンカー項の結果列（`column_aliases` 適用後）から
//!    作業表の合成スキーマを作り、再帰項をそのスキーマに対して束縛し、列数・型の整合を
//!    集合演算（`UNION ALL`）と同じ規則（`sql::set_op`）で検査する。Describe は行を読まない。
//! 2. 作業表上の評価（`ScanTerm::eval_over_rows`）: 再帰項の `WHERE` と射影を、メモリ上の作業表の
//!    行に対して評価する。述語・式の評価は評価済みセルを扱う既存部品
//!    （`sql::view_buffered::with_scanned`・`where_matches_for_row`・`ExprProgram`）を再利用し、
//!    第 2 の式評価器を作らない。再帰項が JOIN のときは、基底表側を `sql::join::PreparedJoin` で
//!    1 度だけ走査し、作業表側だけを毎反復で差し替えて結合以降を実行する。
//! 3. 反復制御: アンカー項の結果を最初の作業表とし、「前回の反復で増えた行だけ」を作業表として
//!    再帰項を再実行し、作業表が空になったら終了する。出力順は生成順（アンカー行 → 反復 1 →
//!    反復 2 …）で固定する（`sort_unstable*` は使わない）。
//!
//! 重複除去版 `UNION`（#1872）: `cte.union_all` が偽のとき、アンカー行・各反復の再帰項の出力から
//! 「既に結果に残った行」「同じ反復で先に出た行」と同値の行を捨てる（循環グラフで作業表が空になり
//! 止まる。PostgreSQL 17 の RecursiveUnion と同じ意味）。同値判定は集合演算 `UNION` と同じ行キー
//! （`sql::set_op::row_key`）を再利用し、第 2 の比較器は作らない（[`DedupSet`]）。捨てた行は
//! 行数上限に数えない。キーは確保前に共有バイト予算へ計上する。
//!
//! 4. `SEARCH DEPTH／BREADTH FIRST` 句（#2213）: 反復中に、各行の親（再帰行を生んだ作業表の行）の順序列から
//!    自分の順序列（深さ優先は経路の `record[]`、幅優先は `(深さ, BY 列...)` の `record`）を作り、結果列の
//!    末尾へ足す（[`SearchPlan`]）。順序列は作業表のスキーマには含めない。経路は深さに比例して増えるため、
//!    複製の前に共有バイト予算へ計上する。
//!
//! 5. `CYCLE ... SET ... USING` 句（#2214）: PostgreSQL 17 の書き換え（`rewriteSearchAndCycle`）と同じ意味で、
//!    行ごとに経路（循環判定列の行値の `record[]`。深さ優先の `SEARCH` と同じ生成器 [`SearchPlan`] を共有）を持ち、
//!    新しい行の行値が親の経路に含まれる（`=` の意味。フィールドに NULL があれば一致しない）なら印を `TO` 値に
//!    する。印が `TO` 値の行は結果へ残すが次の作業表へは渡さない（そこで再帰が止まる）。印列・経路列は
//!    順序列（あれば）の後ろへ足し、作業表のスキーマには含めない（[`CyclePlan`]）。経路は複製の前に共有
//!    バイト予算へ計上する。
//!
//! RLS-10 (b) の不変条件: 基底表の読み取りは、参照セッション自身の `PolicyContext` と同一の
//! `read_txn` で既存の走査経路（RLS 暗黙適用）を通る。作業表は前回反復の可視行から作った値
//! だけで構成されるため、停止時点・反復回数・`54000` の有無は可視行だけで決まり、他テナントの
//! 行の存在を観測できない。CTE 名は決してストレージから引かない（同名の実表は CTE に隠れる）。
//!
//! 上限: 反復回数・累積行数・累積バイト数を確保前に判定し、超過は結果を打ち切らず文全体を
//! `54000` で拒否する（部分結果を返さない）。反復ごとに `interrupt::check()` へ協調する
//! （`57014`）。反復回数・累積行数の上限は起動時設定
//! （`EngineCore::with_recursive_cte_limits`。Issue #1871・SQL-35 (3)・SQL-46）で、
//! [`RecursiveLimits::from_settings`] が呼び出し段の `--max-result-rows` と合成する。

use std::collections::{HashMap, HashSet};

use crate::catalog::{ColumnType, RowIdentity, TableSchema};
use crate::policy::PolicyContext;

use super::allowlist::{
    CycleClause, CycleConst, SearchClause, SetOperator, SqlSurfaceError, Statement,
    ValidatedRecursiveCte, ValidatedScan,
};
use super::exec::{computed_column_meta, Cell, ColumnMeta, QueryResult, ResultRow};
use super::expr_program::{ExprProgram, StackValue};
use super::join::{cell_payload_bytes, prepare_join, JoinLimits, PreparedJoin};
use super::parser::{BoundScan, ProjectedColumn};
use super::udf_call::UdfRegistry;
use super::view_buffered::{
    column_name, normalize_cell, rename_body_columns, synth_schema, where_matches_for_row,
    with_scanned,
};

/// 再帰評価の実行上限。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecursiveLimits {
    /// 再帰項の実行回数の上限。
    pub(crate) max_iterations: usize,
    /// 結果行数（アンカー行を含む累積）の上限。
    pub(crate) max_rows: usize,
    /// 結果・作業表の累計バイト予算。
    pub(crate) max_bytes: usize,
}

impl RecursiveLimits {
    /// 起動時設定（[`crate::recursive_cte_limits::RecursiveCteLimits`]）と呼び出し段の
    /// `--max-result-rows`（SQL-34）から上限を組み立てる。累積行数は両者の小さい方で、
    /// 具体化 CTE と同じく CTE 全体の行数が呼び出し段の上限を超えたら `54000` にする。
    pub(crate) fn from_settings(
        cfg: crate::recursive_cte_limits::RecursiveCteLimits,
        max_result_rows: usize,
    ) -> Self {
        Self {
            max_iterations: cfg.max_iterations(),
            max_rows: cfg.max_rows().min(max_result_rows),
            max_bytes: crate::arena::MAX_ARENA_TOTAL_BYTES,
        }
    }
}

fn unsupported_term(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

fn internal(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// 行数・バイト数の共有予算（確保前に判定する単一経路）。結果（`retained`）・基底表側の行
/// （`fixed`。反復中ずっと保持）・反復内の一時行（`scratch`。JOIN の作業表側の行）の合計バイトが
/// `max_bytes` を超える確保は、確保前に `54000` で拒否する。結果と作業表は同じ行列を共有する
/// （作業表は結果の末尾区間。深い複製を作らない）ため二重に計上しない。
struct RowBudget {
    rows: usize,
    retained: usize,
    fixed: usize,
    scratch: usize,
    /// 重複除去キー集合の保持量（`fixed` の内数ではなく別計上。JOIN へ渡す残予算から差し引く）。
    dedup_keys: usize,
    limits: RecursiveLimits,
}

/// 予算へ計上する行の種別。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Charge {
    /// 結果（同時に次の作業表）へ残る行。行数上限にも数える。
    Retained,
    /// 反復中ずっと保持する基底表側の行（バイトのみ）。
    Fixed,
    /// 反復内だけ保持する一時行（バイトのみ。[`RowBudget::clear_scratch`] で解放）。
    Scratch,
}

impl RowBudget {
    fn new(limits: RecursiveLimits) -> Self {
        Self {
            rows: 0,
            retained: 0,
            fixed: 0,
            scratch: 0,
            dedup_keys: 0,
            limits,
        }
    }

    fn total(&self) -> usize {
        self.retained
            .saturating_add(self.fixed)
            .saturating_add(self.scratch)
            .saturating_add(self.dedup_keys)
    }

    /// 全確保経路の合計に対する残りバイト（走査の確保上限）。
    fn remaining(&self) -> usize {
        self.limits.max_bytes.saturating_sub(self.total())
    }

    /// JOIN へ渡す上限（JOIN は側の行を自分の予算へ計上し直すため、保持済みの結果と、評価中ずっと
    /// 残る重複除去キーだけを差し引く）。
    fn remaining_for_join(&self) -> usize {
        self.limits
            .max_bytes
            .saturating_sub(self.retained)
            .saturating_sub(self.dedup_keys)
    }

    /// 重複除去キーの保持量を確保前に計上する（超過は `54000`）。
    fn reserve_dedup_key(&mut self, bytes: usize) -> Result<(), SqlSurfaceError> {
        self.total()
            .checked_add(bytes)
            .filter(|n| *n <= self.limits.max_bytes)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the shared result byte budget",
                )
            })?;
        self.dedup_keys = self.dedup_keys.saturating_add(bytes);
        Ok(())
    }

    /// 一時行の計上を解放する（反復の終わり）。
    fn clear_scratch(&mut self) {
        self.scratch = 0;
    }

    /// 行 1 件の構造体ぶん（`ResultRow` 本体＋セル配列）のバイト数。
    fn row_overhead(ncols: usize) -> usize {
        ncols
            .saturating_mul(std::mem::size_of::<Cell>())
            .saturating_add(std::mem::size_of::<ResultRow>())
    }

    /// 全確保経路の合計に対する残りバイトの見積もり（式セルの生成上限に渡す）。
    fn headroom(&self) -> usize {
        self.remaining()
    }

    /// `bytes` を確保する **前** に計上する（唯一の計上点）。超過は `54000`（計上は変えない）。
    fn reserve(&mut self, bytes: usize, charge: Charge) -> Result<(), SqlSurfaceError> {
        let total = self
            .total()
            .checked_add(bytes)
            .filter(|n| *n <= self.limits.max_bytes)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the shared result byte budget",
                )
            })?;
        let _ = total;
        match charge {
            Charge::Retained => self.retained = self.retained.saturating_add(bytes),
            Charge::Fixed => self.fixed = self.fixed.saturating_add(bytes),
            Charge::Scratch => self.scratch = self.scratch.saturating_add(bytes),
        }
        Ok(())
    }

    /// 行 1 件ぶん（構造体＋ペイロード）のバイト数。
    fn row_bytes(cells: &[Cell]) -> usize {
        let payload = cells
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(cell_payload_bytes(c)));
        Self::row_overhead(cells.len()).saturating_add(payload)
    }

    /// 一時行（`Scratch` で計上済み）を結果へ残る行（`Retained`）へ移す。行数上限を判定してから
    /// 計上を付け替える（合計バイトは変わらず、二重計上を作らない）。超過は `54000`。
    fn promote(&mut self, cells: &[Cell]) -> Result<(), SqlSurfaceError> {
        let rows = self
            .rows
            .checked_add(1)
            .filter(|n| *n <= self.limits.max_rows)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
        let bytes = Self::row_bytes(cells);
        self.scratch = self.scratch.saturating_sub(bytes);
        self.retained = self.retained.saturating_add(bytes);
        self.rows = rows;
        Ok(())
    }

    /// 重複として捨てる一時行の計上を解放する（結果にも次の作業表にも入らない）。
    fn discard_scratch(&mut self, cells: &[Cell]) {
        self.scratch = self.scratch.saturating_sub(Self::row_bytes(cells));
    }

    /// 行を 1 件作る前に、行数上限（結果へ残る行のみ）と構造体ぶんのバイトを計上する。
    fn reserve_row(&mut self, ncols: usize, charge: Charge) -> Result<(), SqlSurfaceError> {
        if charge == Charge::Retained {
            let rows = self
                .rows
                .checked_add(1)
                .filter(|n| *n <= self.limits.max_rows)
                .ok_or_else(|| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?;
            self.reserve(Self::row_overhead(ncols), charge)?;
            self.rows = rows;
            Ok(())
        } else {
            self.reserve(Self::row_overhead(ncols), charge)
        }
    }

    /// すでに実体化済みの行（アンカーの結果・基底表の走査結果・JOIN の出力。いずれも上流が残予算を
    /// 上限として生成済みで、所有権ごと移して複製しない）を保持する計上。超過は `54000`。
    fn admit(&mut self, cells: &[Cell], charge: Charge) -> Result<(), SqlSurfaceError> {
        let payload = cells
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(cell_payload_bytes(c)));
        let bytes = Self::row_overhead(cells.len()).saturating_add(payload);
        // 行数・バイトの両方を検査してから、まとめて計上する（片方だけ計上された状態を残さない）。
        let rows = if charge == Charge::Retained {
            self.rows
                .checked_add(1)
                .filter(|n| *n <= self.limits.max_rows)
                .ok_or_else(|| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?
        } else {
            self.rows
        };
        self.reserve(bytes, charge)?;
        self.rows = rows;
        Ok(())
    }
}

/// 重複除去版 `UNION` の既出行集合。キーは集合演算 `UNION` と同じ `set_op::row_key`（NULL 同士は
/// 等しい・`-0.0`＝`+0.0`・numeric は scale 無視）。キーは評価中ずっと残るため、確保の前に
/// 共有バイト予算へ（`RowBudget::reserve_dedup_key`＝行数に数えない）計上する。重複と分かったキーの計上は残す
/// （上限が厳しくなる側）。
struct DedupSet {
    seen: HashSet<Vec<u8>>,
}

/// 1 キーあたりの集合の管理領域の見積もり（`sql::distinct` の定数と同じ値）。
const DEDUP_ENTRY_OVERHEAD_BYTES: usize = 32;

impl DedupSet {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }

    /// 行が未出なら記録して `true`、既出なら `false`（呼び出し側が捨てる）。
    fn insert(&mut self, row: &ResultRow, budget: &mut RowBudget) -> Result<bool, SqlSurfaceError> {
        let len = super::set_op::row_key_len(row)?;
        budget.reserve_dedup_key(len.saturating_add(DEDUP_ENTRY_OVERHEAD_BYTES))?;
        let key = super::set_op::row_key(row)?;
        self.seen.try_reserve(1).map_err(|_| {
            SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
        })?;
        Ok(self.seen.insert(key))
    }
}

/// 作業表上の走査（束縛済み述語・射影と、式項目のコンパイル済みプログラム）。
struct ScanTerm {
    bound: BoundScan,
    programs: Vec<Option<ExprProgram>>,
    /// 作業表の列ごとの `id` 型フラグ（[`work_column`]）。`id` 型の列は公告を `AliasedId`、値を
    /// `Cell::Integer(u64)` のまま運ぶ。
    id_cols: Vec<bool>,
}

impl ScanTerm {
    fn new(bound: BoundScan, id_cols: Vec<bool>) -> Result<Self, SqlSurfaceError> {
        // 疑似列 `id` は作業表に存在しない（行 id を持たない）。`SELECT *` や `<cte>.id`（実列 `id` が
        // 無い場合）は黙って NULL／0 にせず拒否する（fail-closed）。
        if bound
            .projection
            .iter()
            .any(|c| matches!(c, ProjectedColumn::Id | ProjectedColumn::AliasedId { .. }))
        {
            return Err(unsupported_term(
                "the recursive reference cannot expose the pseudo column id (list the columns explicitly)",
            ));
        }
        let programs = bound
            .projection
            .iter()
            .map(|c| match c {
                ProjectedColumn::Computed { expr, .. } => Some(ExprProgram::compile(expr)),
                _ => None,
            })
            .collect();
        Ok(Self {
            bound,
            programs,
            id_cols,
        })
    }

    /// 射影の結果列メタデータ（再帰項の列整合の検査に使う）。
    fn column_metas(&self, schema: &TableSchema) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
        self.bound
            .projection
            .iter()
            .map(|c| match c {
                ProjectedColumn::Column { index, name } => {
                    let def = schema
                        .columns
                        .get(*index)
                        .ok_or_else(|| internal("projected column index out of range"))?;
                    if self.id_cols.get(*index).copied().unwrap_or(false) {
                        return Ok(crate::sql::exec::aliased_id_column_meta(name));
                    }
                    Ok(ColumnMeta::Scalar {
                        name: name.clone(),
                        ty: def.ty.clone(),
                    })
                }
                ProjectedColumn::Computed {
                    name, result_ty, ..
                } => Ok(computed_column_meta(name, result_ty)),
                ProjectedColumn::Id | ProjectedColumn::AliasedId { .. } => {
                    Err(internal("pseudo column in the recursive reference"))
                }
            })
            .collect()
    }

    /// 作業表・結果に入る 1 行を作る唯一の処理。確保の順序を固定する: (1) 行の構造体ぶんと各セルの
    /// ペイロードを、借用セルから見積もって共有予算へ **確保前に** 計上する（超過は `54000`）、
    /// (2) 計上が通った後でのみ `clone`／`into_owned` する、(3) 式セルは共有予算の残りを上限として
    /// `computed_value_to_cell` へ渡し、生成したバイトを同じ予算へ計上する。Scan 版・JOIN の作業表側の
    /// どちらもここを通る。
    fn build_row(
        &self,
        types: &[Option<ColumnType>],
        row: &ResultRow,
        scanned: &[Option<crate::row_codec::ScalarRef<'_>>],
        budget: &mut RowBudget,
        charge: Charge,
        proj_scratch: &mut Vec<StackValue>,
    ) -> Result<Vec<Cell>, SqlSurfaceError> {
        budget.reserve_row(self.bound.projection.len(), charge)?;
        let mut cells: Vec<Cell> = Vec::new();
        cells
            .try_reserve_exact(self.bound.projection.len())
            .map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
        for (i, col) in self.bound.projection.iter().enumerate() {
            match col {
                ProjectedColumn::Column { index, .. } => {
                    let cell = row
                        .cells
                        .get(*index)
                        .ok_or_else(|| internal("projected column index out of range"))?;
                    // (1) 借用セルから見積もって計上（複製の前）。
                    budget.reserve(cell_payload_bytes(cell), charge)?;
                    // (2) 計上後にだけ複製する。`id` 型の列は元のセル（`u64`）のまま運ぶ。
                    let is_id = self.id_cols.get(*index).copied().unwrap_or(false);
                    cells.push(match types.get(*index) {
                        Some(Some(_)) if is_id => cell.clone(),
                        Some(Some(ty)) => normalize_cell(cell, ty)?.into_owned(),
                        _ => cell.clone(),
                    });
                }
                ProjectedColumn::Computed { result_ty, .. } => {
                    let program = self
                        .programs
                        .get(i)
                        .and_then(|p| p.as_ref())
                        .ok_or_else(|| internal("computed projection program missing"))?;
                    let value = program.eval(0, &[], scanned, proj_scratch)?;
                    // (3) 共有予算の残りを上限にセルを生成し（確保前に超過を検査）、実バイトを計上する。
                    let mut cell_bytes = 0usize;
                    let cell = crate::sql::scan::computed_value_to_cell(
                        value,
                        result_ty.as_ref(),
                        &mut cell_bytes,
                        budget.headroom(),
                    )?;
                    budget.reserve(cell_payload_bytes(&cell), charge)?;
                    cells.push(cell);
                }
                ProjectedColumn::Id | ProjectedColumn::AliasedId { .. } => {
                    return Err(internal("pseudo column in the recursive reference"));
                }
            }
        }
        Ok(cells)
    }

    /// 作業表の行 `rows` に `WHERE` と射影を適用した行を返す。1 行ごとに、結果へ加える前に
    /// `budget` で行数・バイト数を判定する（超過は `54000`。部分結果は返さない）。`parents` が
    /// `Some` のときは、出力行ごとに入力行（`rows` 内の位置）を追記する（`SEARCH` 句が親行を引く
    /// ために使う。SQL-35・Issue #2213）。
    #[allow(clippy::too_many_arguments)]
    fn eval_over_rows(
        &self,
        types: &[Option<ColumnType>],
        rows: &[ResultRow],
        budget: &mut RowBudget,
        charge: Charge,
        mut parents: Option<&mut Vec<usize>>,
        mut dedup: Option<&mut DedupSet>,
        skip: Option<&[bool]>,
    ) -> Result<Vec<ResultRow>, SqlSurfaceError> {
        // `skip` は `rows` と同じ並びの「次の作業表へ渡さない行」（`CYCLE` の循環行。#2214）。長さの
        // 食い違いは黙って全行を通す・捨てるのではなく `Internal`（fail-closed）にする。
        if skip.is_some_and(|s| s.len() != rows.len()) {
            return Err(internal("working table skip mask length mismatch"));
        }
        let referenced = vec![true; types.len()];
        let mut where_scratch: Vec<StackValue> = Vec::new();
        let mut proj_scratch: Vec<StackValue> = Vec::new();
        let mut out: Vec<ResultRow> = Vec::new();
        for (n, row) in rows.iter().enumerate() {
            if n % 1024 == 0 {
                crate::interrupt::check()?;
            }
            if skip.is_some_and(|s| s.get(n).copied().unwrap_or(true)) {
                continue;
            }
            let cells = with_scanned(types, &referenced, row, |scanned| {
                if !where_matches_for_row(
                    &self.bound.metadata_filters,
                    &self.bound.or_filters,
                    &self.bound.expr_filter_programs,
                    scanned,
                    0,
                    &mut where_scratch,
                )? {
                    return Ok(None);
                }
                Ok(Some(self.build_row(
                    types,
                    row,
                    scanned,
                    budget,
                    charge,
                    &mut proj_scratch,
                )?))
            })?;
            if let Some(cells) = cells {
                let row = ResultRow {
                    id: 0,
                    score: 0.0,
                    cells,
                };
                // 重複除去版: 候補行（`Scratch` で計上済み）が既出なら計上を解放して捨て、未出なら
                // 結果へ残る行へ移す（重複は行数上限に数えない）。
                if let Some(d) = dedup.as_deref_mut() {
                    if !d.insert(&row, budget)? {
                        budget.discard_scratch(&row.cells);
                        continue;
                    }
                    budget.promote(&row.cells)?;
                }
                out.try_reserve(1).map_err(|_| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?;
                if let Some(parents) = parents.as_deref_mut() {
                    parents.try_reserve(1).map_err(|_| {
                        SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                    })?;
                    parents.push(n);
                }
                out.push(row);
            }
        }
        Ok(out)
    }
}

/// 再帰項を作業表に対して束縛した結果。
enum Step<'a> {
    /// 単一関係の再帰項（`SELECT .. FROM <cte> [WHERE ..]`）。
    Scan(ScanTerm),
    /// JOIN の再帰項。`cte_side` は作業表が占める側の添字、`side_term` はその側の束縛済み走査。
    Join {
        prepared: Box<PreparedJoin<'a>>,
        cte_side: usize,
        side_term: ScanTerm,
    },
}

/// 計画の結果（Describe と Execute が共有する）。
struct Prepared<'a> {
    /// CTE の公開列（`column_aliases` 適用後・型合成後。`SEARCH` 句があれば末尾に順序列、`CYCLE` 句が
    /// あればさらに印列・経路列）。
    columns: Vec<ColumnMeta>,
    /// `SEARCH` 句の計画（順序列の生成に使う）。
    search: Option<SearchPlan>,
    /// `CYCLE` 句の計画（印・経路の生成と循環判定に使う）。
    cycle: Option<CyclePlan>,
    /// 作業表の列型（`synth_schema` の `types`）。
    work_types: Vec<Option<ColumnType>>,
    step: Step<'a>,
}

/// `SEARCH` 句の計画（SQL-35・Issue #2213）。BY 列は CTE の公開列の添字で持ち、順序列は
/// PostgreSQL の書き換えと同じ意味で生成する: 深さ優先は「根から当該行までの BY 列の行値の配列」
/// （アンカー行は `ARRAY[ROW(by...)]`、再帰行は親の経路の末尾へ自分の `ROW(by...)` を足す）、
/// 幅優先は `ROW(深さ, by...)`（アンカー行は深さ 0、再帰行は親の深さ + 1）。
struct SearchPlan {
    breadth_first: bool,
    by_idx: Vec<usize>,
}

/// `SEARCH` 句の BY 列・SET 名を CTE の公開列に対して解決し、計画と順序列の列メタを返す。
/// エラーの SQLSTATE は PostgreSQL（`parse_cte.c`）に合わせる: BY 列が列名リストに無い・SET 名が列と
/// 衝突は `42601`、BY 列の重複は `42701`。並べ替えできない型（`VECTOR`・`ARRAY`・`JSON`・型なし等）の
/// BY 列は、PostgreSQL が並べ替え時まで遅延するところを計画時に固定文言の `0A000` で拒否する
/// （fail-closed。Describe と Execute が同じ検査を通る）。
fn plan_search(
    clause: &SearchClause,
    columns: &[ColumnMeta],
) -> Result<(SearchPlan, ColumnMeta), SqlSurfaceError> {
    let mut by_idx: Vec<usize> = Vec::new();
    by_idx
        .try_reserve(clause.by.len())
        .map_err(|_| SqlSurfaceError::payload_too_large("too many columns in the SEARCH clause"))?;
    for name in &clause.by {
        let mut found: Option<usize> = None;
        for (i, meta) in columns.iter().enumerate() {
            if column_name(meta).eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return Err(SqlSurfaceError::ambiguous_column(name.clone()));
                }
                found = Some(i);
            }
        }
        let i = found.ok_or_else(|| {
            SqlSurfaceError::unsupported(format!(
                "search column \"{name}\" not in WITH query column list"
            ))
        })?;
        if by_idx.contains(&i) {
            return Err(SqlSurfaceError::duplicate_column(name.clone()));
        }
        by_idx.push(i);
    }
    if columns
        .iter()
        .any(|m| column_name(m).eq_ignore_ascii_case(&clause.set))
    {
        return Err(SqlSurfaceError::unsupported(format!(
            "search sequence column name \"{}\" already used in WITH query column list",
            clause.set
        )));
    }
    let mut fields: Vec<Option<ColumnType>> = Vec::new();
    fields
        .try_reserve(by_idx.len().saturating_add(1))
        .map_err(|_| SqlSurfaceError::payload_too_large("too many columns in the SEARCH clause"))?;
    if clause.breadth_first {
        fields.push(Some(ColumnType::BigInt));
    }
    for &i in &by_idx {
        let unsupported =
            || unsupported_term("this column type cannot be used as a SEARCH BY column");
        let meta = columns
            .get(i)
            .ok_or_else(|| internal("search column out of range"))?;
        let field = match meta {
            // 行 id 型の列（セルは `Cell::Integer(u64)`）。
            ColumnMeta::Id | ColumnMeta::AliasedId { .. } => None,
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => {
                if super::join::values::cmp_class(Some(ty)).is_none() {
                    return Err(unsupported());
                }
                Some(ty.clone())
            }
            ColumnMeta::Computed { ty: None, .. } | ColumnMeta::Record { .. } => {
                return Err(unsupported());
            }
        };
        fields.push(field);
    }
    Ok((
        SearchPlan {
            breadth_first: clause.breadth_first,
            by_idx,
        },
        ColumnMeta::Record {
            name: clause.set.clone(),
            fields,
            array: !clause.breadth_first,
        },
    ))
}

/// `CYCLE` 句の計画（SQL-35・Issue #2214）。循環判定列は CTE の公開列の添字で持ち、経路は深さ優先の
/// `SEARCH` と同じ生成器（[`SearchPlan`]。アンカー行は `ARRAY[ROW(cols)]`、再帰行は親の経路の末尾へ
/// 自分の `ROW(cols)` を足す）を共有する。印は `TO` 値と `DEFAULT` 値の 2 値で、循環検出は評価ごとに
/// [`CyclePlan::is_cycle`] が親の経路に対して行う。
struct CyclePlan {
    /// 経路の生成器（`breadth_first` は常に偽）。
    path: SearchPlan,
    /// 循環判定列の比較クラス（`path.by_idx` と同じ並び）。
    classes: Vec<super::join::values::CmpClass>,
    /// 循環行の印（`TO` 値。省略時は `true`）。
    mark_to: Cell,
    /// 循環でない行の印（`DEFAULT` 値。省略時は `false`）。
    mark_default: Cell,
    /// `TO` と `DEFAULT` が同じ値か。真のとき、循環でない行の印も `TO` 値と等しく、どの行も次の作業表へ
    /// 渡らない（PostgreSQL の書き換えが足す `WHERE 印 <> TO` と同じ結果）。
    default_is_to: bool,
}

/// `TO`／`DEFAULT` 定数の値の種別（共通型の解決用）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CycleConstKind {
    Bool,
    Int,
    Text,
}

fn cycle_const_kind(c: &CycleConst) -> Result<CycleConstKind, SqlSurfaceError> {
    match c {
        CycleConst::Bool(_) => Ok(CycleConstKind::Bool),
        CycleConst::Integer(_) => Ok(CycleConstKind::Int),
        CycleConst::Text(_) => Ok(CycleConstKind::Text),
        // 小数・NULL・型名付きリテラルは、黙って別の型へ変換せず固定文言で拒否する（fail-closed）。
        CycleConst::Decimal(_) | CycleConst::Null | CycleConst::Typed => Err(unsupported_term(
            "this form of constant in the CYCLE clause is not supported",
        )),
    }
}

/// 定数を整数として読む（整数リテラルは範囲外なら `0A000`、文字列は整数として解釈できなければ `22P02`）。
fn cycle_const_i64(c: &CycleConst) -> Result<i64, SqlSurfaceError> {
    match c {
        CycleConst::Integer(s) => s.parse::<i64>().map_err(|_| {
            unsupported_term("an integer constant in the CYCLE clause is out of range")
        }),
        CycleConst::Text(s) => s.trim().parse::<i64>().map_err(|_| {
            SqlSurfaceError::invalid_text_representation("invalid input syntax for type integer")
        }),
        _ => Err(internal("unexpected CYCLE constant form")),
    }
}

/// 定数を真偽値として読む（文字列は PostgreSQL の boolean 入力規則で解釈し、できなければ `22P02`）。
fn cycle_const_bool(c: &CycleConst) -> Result<bool, SqlSurfaceError> {
    match c {
        CycleConst::Bool(b) => Ok(*b),
        CycleConst::Text(s) => super::params::parse_pg_bool(s).ok_or_else(|| {
            SqlSurfaceError::invalid_text_representation("invalid input syntax for type boolean")
        }),
        _ => Err(internal("unexpected CYCLE constant form")),
    }
}

/// 印の `TO`／`DEFAULT` を共通型へ解決する（PostgreSQL の `select_common_type` と同じ決め方）。
/// 省略時は boolean の `true`／`false`。両方が文字列なら text、整数どうし（または整数と文字列）なら
/// integer（`i32` に収まらなければ bigint）、真偽値と文字列なら boolean。真偽値と整数は `42804`。
/// 返す印のセルは `Cell::Bool`／`Cell::SignedInteger`／`Cell::Text`。
fn resolve_cycle_marks(
    values: &Option<(CycleConst, CycleConst)>,
) -> Result<(Cell, Cell, ColumnType), SqlSurfaceError> {
    use CycleConstKind::{Bool, Int, Text};
    let Some((to, default)) = values else {
        return Ok((Cell::Bool(true), Cell::Bool(false), ColumnType::Boolean));
    };
    let (kt, kd) = (cycle_const_kind(to)?, cycle_const_kind(default)?);
    match (kt, kd) {
        (Bool, Bool) | (Bool, Text) | (Text, Bool) => Ok((
            Cell::Bool(cycle_const_bool(to)?),
            Cell::Bool(cycle_const_bool(default)?),
            ColumnType::Boolean,
        )),
        (Text, Text) => match (to, default) {
            (CycleConst::Text(a), CycleConst::Text(b)) => Ok((
                Cell::Text(a.clone()),
                Cell::Text(b.clone()),
                ColumnType::Text,
            )),
            _ => Err(internal("unexpected CYCLE constant form")),
        },
        (Int, Int) | (Int, Text) | (Text, Int) => {
            let (a, b) = (cycle_const_i64(to)?, cycle_const_i64(default)?);
            let fits = |v: i64| i32::try_from(v).is_ok();
            let ty = if fits(a) && fits(b) {
                ColumnType::Integer
            } else {
                ColumnType::BigInt
            };
            Ok((Cell::SignedInteger(a), Cell::SignedInteger(b), ty))
        }
        (Bool, Int) | (Int, Bool) => Err(SqlSurfaceError::datatype_mismatch(
            "CYCLE types boolean and integer cannot be matched",
        )),
    }
}

/// `CYCLE` 句の循環判定列・印列名・経路列名を CTE の公開列（`SEARCH` の順序列を除く。`base`）に対して
/// 解決し、計画と印列・経路列の列メタを返す。エラーの SQLSTATE は PostgreSQL（`parse_cte.c`）に合わせる:
/// 循環列が列名リストに無い・印／経路の列名が列や `SEARCH` の SET 名と衝突・印と経路が同名は `42601`、
/// 循環列の重複は `42701`。比較できない型（`VECTOR`・`ARRAY`・`JSON`・型なし等）の循環列は、
/// PostgreSQL が実行時まで遅延するところを計画時に固定文言の `0A000` で拒否する（fail-closed。
/// Describe と Execute が同じ検査を通る）。
fn plan_cycle(
    clause: &CycleClause,
    base: &[ColumnMeta],
    search_set: Option<&str>,
) -> Result<(CyclePlan, ColumnMeta, ColumnMeta), SqlSurfaceError> {
    let too_many = || SqlSurfaceError::payload_too_large("too many columns in the CYCLE clause");
    let mut by_idx: Vec<usize> = Vec::new();
    by_idx
        .try_reserve(clause.columns.len())
        .map_err(|_| too_many())?;
    for name in &clause.columns {
        let mut found: Option<usize> = None;
        for (i, meta) in base.iter().enumerate() {
            if column_name(meta).eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return Err(SqlSurfaceError::ambiguous_column(name.clone()));
                }
                found = Some(i);
            }
        }
        let i = found.ok_or_else(|| {
            SqlSurfaceError::unsupported(format!(
                "cycle column \"{name}\" not in WITH query column list"
            ))
        })?;
        if by_idx.contains(&i) {
            return Err(SqlSurfaceError::duplicate_column(name.clone()));
        }
        by_idx.push(i);
    }
    let clashes = |n: &str| {
        base.iter().any(|m| column_name(m).eq_ignore_ascii_case(n))
            || search_set.is_some_and(|s| s.eq_ignore_ascii_case(n))
    };
    if clashes(&clause.mark) {
        return Err(SqlSurfaceError::unsupported(format!(
            "cycle mark column name \"{}\" already used in WITH query column list",
            clause.mark
        )));
    }
    if clashes(&clause.path) {
        return Err(SqlSurfaceError::unsupported(format!(
            "cycle path column name \"{}\" already used in WITH query column list",
            clause.path
        )));
    }
    if clause.mark.eq_ignore_ascii_case(&clause.path) {
        return Err(SqlSurfaceError::unsupported(
            "cycle mark column name and cycle path column name are the same",
        ));
    }
    let mut fields: Vec<Option<ColumnType>> = Vec::new();
    fields.try_reserve(by_idx.len()).map_err(|_| too_many())?;
    let mut classes: Vec<super::join::values::CmpClass> = Vec::new();
    classes.try_reserve(by_idx.len()).map_err(|_| too_many())?;
    for &i in &by_idx {
        let unsupported = || unsupported_term("this column type cannot be used as a CYCLE column");
        let meta = base
            .get(i)
            .ok_or_else(|| internal("cycle column out of range"))?;
        match meta {
            // 行 id 型の列（セルは `Cell::Integer(u64)`。整数クラスで比べる）。
            ColumnMeta::Id | ColumnMeta::AliasedId { .. } => {
                fields.push(None);
                classes.push(super::join::values::CmpClass::Integer);
            }
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => {
                let class = super::join::values::cmp_class(Some(ty)).ok_or_else(unsupported)?;
                fields.push(Some(ty.clone()));
                classes.push(class);
            }
            ColumnMeta::Computed { ty: None, .. } | ColumnMeta::Record { .. } => {
                return Err(unsupported());
            }
        }
    }
    let (mark_to, mark_default, mark_ty) = resolve_cycle_marks(&clause.mark_values)?;
    let default_is_to = mark_to == mark_default;
    Ok((
        CyclePlan {
            path: SearchPlan {
                breadth_first: false,
                by_idx,
            },
            classes,
            mark_to,
            mark_default,
            default_is_to,
        },
        ColumnMeta::Computed {
            name: clause.mark.clone(),
            ty: Some(mark_ty),
        },
        ColumnMeta::Record {
            name: clause.path.clone(),
            fields,
            array: true,
        },
    ))
}

impl CyclePlan {
    /// 印のセル（循環なら `TO` 値、そうでなければ `DEFAULT` 値）。
    fn mark_cell(&self, hit: bool) -> Cell {
        if hit {
            self.mark_to.clone()
        } else {
            self.mark_default.clone()
        }
    }

    /// 印のセル 1 個ぶんのバイト数の見積もり（確保前の予算計上用）。
    fn mark_bytes(&self, hit: bool) -> usize {
        let cell = if hit {
            &self.mark_to
        } else {
            &self.mark_default
        };
        std::mem::size_of::<Cell>().saturating_add(cell_payload_bytes(cell))
    }

    /// 新しい行 `row` の循環判定列の行値が、親の経路 `parent_path`（親自身の行値を含む `record[]`）の
    /// いずれかの要素と等しいか。等値は SQL の `=` の意味で、フィールドに NULL が 1 つでもあれば一致
    /// しない（`ROW(1, NULL) = ROW(1, NULL)` は NULL で循環扱いにならない）。コストは O(深さ × 列数)。
    /// セルの形が計画と食い違えば `Internal`（fail-closed）。
    fn is_cycle(&self, parent_path: &Cell, row: &ResultRow) -> Result<bool, SqlSurfaceError> {
        use super::join::values::{cmp_val, compare_order};
        use super::order_value::NullPlacement;
        let Cell::RecordArray(elements) = parent_path else {
            return Err(internal("recursive cycle path has an unexpected shape"));
        };
        let new = self.path.by_cells(row)?;
        if new.len() != self.classes.len() || new.iter().any(|c| matches!(c, Cell::Null)) {
            return if new.len() != self.classes.len() {
                Err(internal("cycle column count mismatch"))
            } else {
                Ok(false)
            };
        }
        for element in elements {
            if element.len() != self.classes.len() {
                return Err(internal("cycle path element has an unexpected width"));
            }
            let mut equal = true;
            for ((old, now), class) in element.iter().zip(new.iter()).zip(self.classes.iter()) {
                let (Some(a), Some(b)) = (cmp_val(old, class)?, cmp_val(now, class)?) else {
                    equal = false;
                    break;
                };
                if compare_order(Some(&a), Some(&b), false, NullPlacement::Last)
                    != std::cmp::Ordering::Equal
                {
                    equal = false;
                    break;
                }
            }
            if equal {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
/// レコードのフィールド列が占めるバイト数の見積もり（確保前の予算計上用。借用セルから算出する）。
fn fields_bytes(cells: &[&Cell]) -> usize {
    cells.iter().fold(0usize, |acc, c| {
        acc.saturating_add(std::mem::size_of::<Cell>())
            .saturating_add(cell_payload_bytes(c))
    })
}

impl SearchPlan {
    /// 行の BY 列のセル（借用）を取り出す。添字が行の範囲外なら `Internal`（fail-closed）。
    fn by_cells<'r>(&self, row: &'r ResultRow) -> Result<Vec<&'r Cell>, SqlSurfaceError> {
        self.by_idx
            .iter()
            .map(|&i| {
                row.cells
                    .get(i)
                    .ok_or_else(|| internal("search column index out of range"))
            })
            .collect()
    }

    /// BY 列のセルを複製してフィールド列を作る（呼び出し元が確保前に予算へ計上済み）。
    fn clone_fields(by: &[&Cell], lead: Option<Cell>) -> Result<Vec<Cell>, SqlSurfaceError> {
        let mut out: Vec<Cell> = Vec::new();
        out.try_reserve_exact(by.len().saturating_add(usize::from(lead.is_some())))
            .map_err(|_| {
                SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the shared result byte budget",
                )
            })?;
        out.extend(lead);
        out.extend(by.iter().map(|c| (*c).clone()));
        Ok(out)
    }

    /// アンカー行の順序列。確保前に共有予算へ計上する（超過は `54000`）。
    fn initial(&self, row: &ResultRow, budget: &mut RowBudget) -> Result<Cell, SqlSurfaceError> {
        let by = self.by_cells(row)?;
        if self.breadth_first {
            let bytes = std::mem::size_of::<Cell>()
                .saturating_add(std::mem::size_of::<Cell>())
                .saturating_add(fields_bytes(&by));
            budget.reserve(bytes, Charge::Retained)?;
            Ok(Cell::Record(Self::clone_fields(
                &by,
                Some(Cell::SignedInteger(0)),
            )?))
        } else {
            let bytes = std::mem::size_of::<Cell>()
                .saturating_add(std::mem::size_of::<Vec<Cell>>())
                .saturating_add(fields_bytes(&by));
            budget.reserve(bytes, Charge::Retained)?;
            let mut path: Vec<Vec<Cell>> = Vec::new();
            path.try_reserve_exact(1).map_err(|_| {
                SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the shared result byte budget",
                )
            })?;
            path.push(Self::clone_fields(&by, None)?);
            Ok(Cell::RecordArray(path))
        }
    }

    /// 再帰行（親の順序列 `parent`・新しい行 `row`）の順序列。深さ優先は経路が深さに比例して増える
    /// ため、複製する **前** に親の経路ぶんを含めて共有予算へ計上する（超過は `54000`）。
    fn child(
        &self,
        parent: &Cell,
        row: &ResultRow,
        budget: &mut RowBudget,
    ) -> Result<Cell, SqlSurfaceError> {
        let by = self.by_cells(row)?;
        match (self.breadth_first, parent) {
            (true, Cell::Record(parent_fields)) => {
                let depth = match parent_fields.first() {
                    Some(Cell::SignedInteger(d)) => d
                        .checked_add(1)
                        .ok_or_else(|| internal("recursive search depth overflow"))?,
                    _ => return Err(internal("recursive search sequence has no depth")),
                };
                let bytes = std::mem::size_of::<Cell>()
                    .saturating_add(std::mem::size_of::<Cell>())
                    .saturating_add(fields_bytes(&by));
                budget.reserve(bytes, Charge::Retained)?;
                Ok(Cell::Record(Self::clone_fields(
                    &by,
                    Some(Cell::SignedInteger(depth)),
                )?))
            }
            (false, Cell::RecordArray(parent_path)) => {
                let bytes = std::mem::size_of::<Cell>()
                    .saturating_add(cell_payload_bytes(parent))
                    .saturating_add(std::mem::size_of::<Vec<Cell>>())
                    .saturating_add(fields_bytes(&by));
                budget.reserve(bytes, Charge::Retained)?;
                let mut path: Vec<Vec<Cell>> = Vec::new();
                path.try_reserve_exact(parent_path.len().saturating_add(1))
                    .map_err(|_| {
                        SqlSurfaceError::payload_too_large(
                            "recursive query exceeds the shared result byte budget",
                        )
                    })?;
                path.extend(parent_path.iter().cloned());
                path.push(Self::clone_fields(&by, None)?);
                Ok(Cell::RecordArray(path))
            }
            _ => Err(internal(
                "recursive search sequence has an unexpected shape",
            )),
        }
    }
}

/// 再帰項の `ValidatedScan` が作業表上で評価できる形かを検査する。
fn check_scan_shape(scan: &ValidatedScan) -> Result<(), SqlSurfaceError> {
    if !scan.limit_implicit || scan.offset != 0 {
        return Err(unsupported_term(
            "LIMIT/OFFSET in a recursive term is not supported",
        ));
    }
    if !scan.order_by.is_empty() || !scan.order_keys.is_empty() || scan.with_ties {
        return Err(unsupported_term(
            "ORDER BY in a recursive term is not supported",
        ));
    }
    if !scan.window_items.is_empty() || !scan.scalar_subquery_items.is_empty() {
        return Err(unsupported_term(
            "window functions and subqueries in a recursive term are not supported",
        ));
    }
    Ok(())
}

/// 「アンカー列 → 作業表列」の唯一の写像。疑似列 `id`（行 id）を結果列にしたアンカー項の列は、
/// 作業表では `id` 型（[`ColumnMeta::AliasedId`]。numeric(20,0) で `u64` 全域を損失なく運ぶ。評価後射影形
/// ビューの外側の `id` 列と同じ写像）として扱う。BIGINT の実列にすると `u64` を `i64` へ変換して
/// `i64::MAX` 超の行 id を壊す。作業表自身は行 id を持たない。作業表スキーマ・列型（`work_types`）・
/// 型整合検査（[`compose_columns`]）・再帰項の束縛のすべてがこの写像の結果を使い、公開結果列の公告
/// （`Id`／`AliasedId` は numeric）と値表現（`Cell::Integer(u64)`）は元のメタのまま保つ。
fn work_column(m: &ColumnMeta) -> ColumnMeta {
    match m {
        ColumnMeta::Id => crate::sql::exec::aliased_id_column_meta("id"),
        other => other.clone(),
    }
}

/// 作業表の列が `id` 型（セルが `Cell::Integer(u64)`）か。
fn is_id_column(m: &ColumnMeta) -> bool {
    matches!(m, ColumnMeta::Id | ColumnMeta::AliasedId { .. })
}

/// 結果列の静的な整数型（`SMALLINT`／`INTEGER`／`BIGINT`）。
fn static_integer_type(m: &ColumnMeta) -> Option<&ColumnType> {
    match m {
        ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => {
            ty.integer_rank().map(|_| ty)
        }
        _ => None,
    }
}

/// 重複除去版 `UNION` の列に、PostgreSQL が等値・ハッシュを持たない型（`JSON` と `JSON` の配列）が
/// 無いことを計画時に検査する（固定 `0A000`。PostgreSQL では recursive UNION を実装できない型）。
/// 行キーはこれらを文字列として比べてしまうため、黙って同値判定せず拒否する（fail-closed）。
fn check_union_equality_types(columns: &[ColumnMeta]) -> Result<(), SqlSurfaceError> {
    for m in columns {
        let ty = match m {
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => ty,
            _ => continue,
        };
        let no_equality = match ty {
            ColumnType::Json => true,
            ColumnType::Array(a) => matches!(a.elem(), crate::catalog::ArrayElemType::Json),
            _ => false,
        };
        if no_equality {
            return Err(unsupported_term(
                "could not implement recursive UNION for this column type",
            ));
        }
    }
    Ok(())
}

/// 作業表へ載せる列の型が、実行時にセルを述語・式へ渡す変換（`join::values::cell_scalar`）で
/// 扱える型だけであることを検査する。未対応型（`TIME`・`TIMESTAMPTZ`・`INTERVAL`・`JSONB` 等）は
/// 実行時の `Internal` ではなく、計画時（describe／execute 共通）に固定 `0A000` で拒否する。
/// `VECTOR`・`ARRAY`・型なし列は述語・式へ渡さない列（合成スキーマの到達不能名）なのでここでは通す。
fn check_work_types(columns: &[ColumnMeta]) -> Result<(), SqlSurfaceError> {
    for m in columns {
        let ty = match m {
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => ty,
            _ => continue,
        };
        if matches!(ty, ColumnType::Vector(_) | ColumnType::Array(_)) {
            continue;
        }
        if !crate::sql::join::values::cell_scalar_supports(ty) {
            return Err(unsupported_term(
                "this column type is not supported in a recursive query",
            ));
        }
    }
    Ok(())
}

/// 列整合の検査と結果列の合成（`UNION ALL` と同じ規則）。型は PostgreSQL と同じくアンカー項の
/// 列を正とし、再帰項は互換でなければ `42804`。
fn compose_columns(
    cte: &ValidatedRecursiveCte,
    public_anchor: &[ColumnMeta],
    anchor_columns: &[ColumnMeta],
    rec_columns: &[ColumnMeta],
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    // 重複除去版（`UNION`）は集合演算 `UNION` と同じ検査（`VECTOR` 列は `22000` 等）を通す。
    let op = if cte.union_all {
        SetOperator::UnionAll
    } else {
        SetOperator::Union
    };
    super::set_op::check_branch_type_compatibility(
        op,
        anchor_columns,
        rec_columns,
        super::set_op::columns_have_vector(anchor_columns),
        super::set_op::columns_have_vector(rec_columns),
    )?;
    // `id` 型の列は値表現（`Cell::Integer(u64)`）が他の numeric と違うため、アンカーと再帰項で
    // `id` 型かどうかが一致しない列（`id` 列への算術・JOIN 越しの射影など）は `42804` で拒否する。
    for (i, (a, r)) in anchor_columns.iter().zip(rec_columns.iter()).enumerate() {
        if is_id_column(a) != is_id_column(r) {
            return Err(SqlSurfaceError::datatype_mismatch(format!(
                "recursive query column {i} must be the row id in both terms or in neither"
            )));
        }
    }
    // 作業表と再帰項の束縛はアンカー型を正とするため、整数型の拡大（アンカーより広い型を再帰項が
    // 返す形）は受け入れずに `42804` で拒否する（PostgreSQL も再帰項の型がアンカー型と一致しない
    // と `42804`。作業表へ拡大後の型を運ぶと、次の反復で範囲外の値が狭い型の列へ渡る）。
    for (i, (a, r)) in anchor_columns.iter().zip(rec_columns.iter()).enumerate() {
        if let (Some(ta), Some(tr)) = (static_integer_type(a), static_integer_type(r)) {
            if ta != tr {
                return Err(SqlSurfaceError::datatype_mismatch(format!(
                    "recursive query column {i} has a different integer type in the recursive term"
                )));
            }
        }
    }
    // 公開結果列の公告はアンカーの元のメタ（`Id`／`AliasedId` は numeric）を保つ。
    let merged = super::set_op::merge_columns(public_anchor, rec_columns);
    if cte.column_aliases.is_empty() {
        Ok(merged)
    } else {
        rename_body_columns(&merged, &cte.column_aliases)
    }
}

/// 計画本体。`schemas` は基底表のスキーマ（CTE 名を含まない）で、CTE 名は合成スキーマで
/// 差し込む（CTE 名はストレージから引かない）。`f` へ計画を渡して結果を返す（`PreparedJoin` は
/// 差し込み後のスキーマ表を借用するため、計画の寿命をこの関数内に閉じる）。
fn with_prepared<T>(
    cte: &ValidatedRecursiveCte,
    anchor_columns: &[ColumnMeta],
    schemas: &HashMap<String, TableSchema>,
    udfs: &UdfRegistry,
    join_limits: &JoinLimits,
    f: impl FnOnce(&Prepared<'_>) -> Result<T, SqlSurfaceError>,
) -> Result<T, SqlSurfaceError> {
    // 作業表の列は、アンカー項の結果列に列名リストを適用したもの（PostgreSQL と同じく、CTE の列型は
    // アンカー項で決まる）。列名リストが長すぎる場合は既存の列別名と同じ `42601`。
    let work_anchor: Vec<ColumnMeta> = anchor_columns.iter().map(work_column).collect();
    check_work_types(&work_anchor)?;
    let named: Vec<ColumnMeta>;
    let exposed: &[ColumnMeta] = if cte.column_aliases.is_empty() {
        &work_anchor
    } else {
        named = rename_body_columns(&work_anchor, &cte.column_aliases)?;
        &named
    };
    let synth_input: Vec<ColumnMeta> = exposed.to_vec();
    let id_cols: Vec<bool> = exposed.iter().map(is_id_column).collect();
    let (work_schema, work_types) = synth_schema(&cte.name, &synth_input);
    // 作業表は行 id を持たない: 疑似列 `id` を持たない行識別方式（`Allocated`）にして、式・述語・
    // JOIN 条件・ORDER BY を含む全ての `id` 参照を束縛段の未知列として拒否する（実列 `id` は
    // 合成列として解決される）。直接投影の検査（`ScanTerm::new`）は二重の防御として残す。
    let work_schema = work_schema.with_row_identity(RowIdentity::Allocated);
    // JOIN 側のスキーマ（`cte_schema` と同じ写像）: `id` 型の列は結合キーのクラスが整数になる BIGINT で
    // 載せ、`CteIdColumns` でその列の添字を渡す（セルは `Cell::Integer(u64)` のまま。束縛器は公告を
    // `id` 型にし、リテラルを numeric で束縛する。`i64::MAX` 超の行 id も損失なく結合できる）。
    let join_input: Vec<ColumnMeta> = synth_input
        .iter()
        .map(|m| match m {
            ColumnMeta::AliasedId { name } => ColumnMeta::Scalar {
                name: name.clone(),
                ty: ColumnType::BigInt,
            },
            other => other.clone(),
        })
        .collect();
    let (join_schema, _) = synth_schema(&cte.name, &join_input);
    let join_schema = join_schema.with_row_identity(RowIdentity::Allocated);
    // 非バイト列順の照合（ICU）の列を持つ表を参照する再帰は、アンカー列の照合を作業表へ運べない
    // ため、再帰項の比較・結合が既定のバイト列順で黙って評価される fail-open を避けて拒否する
    // （`check_buffered_view_collations` と同じ固定 `0A000`。TABLE-28）。
    if schemas.values().any(|schema| {
        schema
            .columns
            .iter()
            .any(|c| c.collation.is_some_and(|coll| !coll.is_byte_order()))
    }) {
        return Err(SqlSurfaceError::feature_not_supported(
            "this collation is not supported in this expression",
        ));
    }

    let mut all: HashMap<String, TableSchema> = schemas.clone();
    all.insert(cte.name.clone(), join_schema.clone());

    let (rec_columns, step): (Vec<ColumnMeta>, Step<'_>) = match &*cte.recursive {
        Statement::Scan(scan) => {
            if !scan.table_name.eq_ignore_ascii_case(&cte.name) {
                return Err(unsupported_term(
                    "the recursive term must read the recursive reference",
                ));
            }
            check_scan_shape(scan)?;
            let bound = super::parser::bind_scan(scan, &work_schema, udfs)?;
            let term = ScanTerm::new(bound, id_cols.clone())?;
            (term.column_metas(&work_schema)?, Step::Scan(term))
        }
        Statement::Join(join) => {
            let mut cte_side: Option<usize> = None;
            let mut cte_ids = crate::sql::join::CteIdColumns::new();
            for (i, rel) in join.relations.iter().enumerate() {
                if rel.table().eq_ignore_ascii_case(&cte.name) {
                    if cte_side.is_some() {
                        return Err(unsupported_term(
                            "the recursive reference must appear exactly once in the recursive term",
                        ));
                    }
                    cte_side = Some(i);
                    // 関係名の書き方（大文字小文字）に依らず合成スキーマへ解決させる（同名の実表を隠す）。
                    all.insert(rel.table().to_string(), join_schema.clone());
                    // `id` 型の列は CTE の行 `id` として結合へ渡す（セルは `Cell::Integer(u64)` のまま、
                    // 疑似列 `id` と同じ符号なし整数のキー。`i64::MAX` 超の行 id も損失なく結合できる）。
                    let id_idx: Vec<usize> = id_cols
                        .iter()
                        .enumerate()
                        .filter(|(_, is_id)| **is_id)
                        .map(|(i, _)| i)
                        .collect();
                    if !id_idx.is_empty() {
                        cte_ids.insert(rel.table().to_string(), id_idx);
                    }
                }
            }
            let cte_side = cte_side.ok_or_else(|| {
                unsupported_term("the recursive term must read the recursive reference")
            })?;
            let prepared = prepare_join(&all, &cte_ids, join, udfs, join_limits)?;
            if prepared.is_aggregate() || prepared.has_ordering_or_limit() {
                return Err(unsupported_term(
                    "aggregation, ORDER BY and LIMIT in a recursive term are not supported",
                ));
            }
            let (_, side_bound) = prepared
                .side_scan(cte_side)
                .ok_or_else(|| internal("recursive reference side out of range"))?;
            let side_term = ScanTerm::new(side_bound.clone(), id_cols.clone())?;
            (
                prepared.column_metas(),
                Step::Join {
                    prepared: Box::new(prepared),
                    cte_side,
                    side_term,
                },
            )
        }
        _ => {
            return Err(unsupported_term(
                "the recursive term must be a plain SELECT or a JOIN",
            ));
        }
    };
    // 再帰項の疑似列 `id` も同じ写像で BIGINT の実列として扱う（アンカー側と対称にする）。
    let rec_work: Vec<ColumnMeta> = rec_columns.iter().map(work_column).collect();
    let mut columns = compose_columns(cte, anchor_columns, &work_anchor, &rec_work)?;
    check_work_types(&rec_work)?;
    if !cte.union_all {
        check_union_equality_types(&work_anchor)?;
        check_union_equality_types(&rec_work)?;
    }
    // 順序列は公開列（列名リスト適用後）の末尾へ足す。作業表のスキーマ・再帰項の束縛・列整合には
    // 含めない（再帰項からは見えない）。
    let base_len = columns.len();
    let search = match &cte.search {
        Some(clause) => {
            let (plan, meta) = plan_search(clause, &columns)?;
            columns.push(meta);
            Some(plan)
        }
        None => None,
    };
    // 印列・経路列は順序列の後ろ。循環列は元の列（順序列を除く）から引く。
    let cycle = match &cte.cycle {
        Some(clause) => {
            let base = columns
                .get(..base_len)
                .ok_or_else(|| internal("cycle base columns out of range"))?;
            let search_set = cte.search.as_ref().map(|c| c.set.as_str());
            let (plan, mark, path) = plan_cycle(clause, base, search_set)?;
            columns.push(mark);
            columns.push(path);
            Some(plan)
        }
        None => None,
    };
    f(&Prepared {
        columns,
        search,
        cycle,
        work_types,
        step,
    })
}

/// 結果列メタデータを導出する（Describe 用。行は読まない）。束縛・列整合のエラーは [`execute`] と
/// 一致する。
pub(crate) fn describe(
    cte: &ValidatedRecursiveCte,
    anchor_columns: &[ColumnMeta],
    schemas: &HashMap<String, TableSchema>,
    udfs: &UdfRegistry,
    join_limits: &JoinLimits,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    with_prepared(cte, anchor_columns, schemas, udfs, join_limits, |p| {
        Ok(p.columns.clone())
    })
}

/// 反復評価を実行する。`anchor` はアンカー項の実行結果（最初の作業表）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    cte: &ValidatedRecursiveCte,
    anchor: QueryResult,
    schemas: &HashMap<String, TableSchema>,
    udfs: &UdfRegistry,
    join_limits: &JoinLimits,
    limits: RecursiveLimits,
) -> Result<QueryResult, SqlSurfaceError> {
    let QueryResult {
        columns: anchor_columns,
        rows: anchor_rows,
    } = anchor;
    with_prepared(cte, &anchor_columns, schemas, udfs, join_limits, |p| {
        let mut budget = RowBudget::new(limits);
        let mut dedup: Option<DedupSet> = if cte.union_all {
            None
        } else {
            Some(DedupSet::new())
        };
        // 重複除去版はアンカーの中の重複も除く（捨てる行は行数に数えない。入力順で最初の行を残す）。
        // 保持中の候補ぶんを先に一時行として共有予算へ計上してからキーを確保し、キー確保が候補と
        // 合算で上限を超えないようにする（JOIN 出力と同じ手順）。未出の行だけを結果へ移す。
        let mut anchor_rows = anchor_rows;
        if let Some(d) = dedup.as_mut() {
            for row in &anchor_rows {
                budget.reserve(RowBudget::row_bytes(&row.cells), Charge::Scratch)?;
            }
            let mut kept: Vec<ResultRow> = Vec::new();
            kept.try_reserve(anchor_rows.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
            for row in anchor_rows {
                if d.insert(&row, &mut budget)? {
                    budget.promote(&row.cells)?;
                    kept.push(row);
                } else {
                    budget.discard_scratch(&row.cells);
                }
            }
            budget.clear_scratch();
            anchor_rows = kept;
        } else {
            for row in &anchor_rows {
                budget.admit(&row.cells, Charge::Retained)?;
            }
        }
        // 結果は 1 本の行列で持ち、作業表（前回の反復で増えた行）はその末尾区間 `start..end` として
        // 参照する（`result.clone()` や各反復の深い複製を作らない）。
        let mut result: Vec<ResultRow> = anchor_rows;
        let mut start = 0usize;
        let mut end = result.len();
        // `SEARCH` 句の順序列（`result` と同じ添字で並ぶ。最後に各行のセル末尾へ移す）。
        let search = p.search.as_ref();
        let mut seqs: Vec<Cell> = Vec::new();
        if let Some(sp) = search {
            seqs.try_reserve(result.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
            for row in &result {
                seqs.push(sp.initial(row, &mut budget)?);
            }
        }

        // `CYCLE` 句の経路と循環フラグ（`result` と同じ添字で並ぶ。アンカー行は循環ではない）。
        let cycle = p.cycle.as_ref();
        let track = search.is_some() || cycle.is_some();
        let mut paths: Vec<Cell> = Vec::new();
        let mut hits: Vec<bool> = Vec::new();
        if let Some(cp) = cycle {
            paths.try_reserve(result.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
            hits.try_reserve(result.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
            for row in &result {
                budget.reserve(cp.mark_bytes(false), Charge::Retained)?;
                paths.push(cp.path.initial(row, &mut budget)?);
                hits.push(false);
            }
        }

        // JOIN の基底表側は反復によらず同じ行を返す（同一 `read_txn`）ため、ループの前に 1 度だけ読む。
        // 読んだ行は共有予算へ計上する（JOIN 自身の上限〔`join_limits`〕を超えない範囲で保持される）。
        // アンカーが 0 行なら作業表は最初から空で再帰項は 1 度も評価されないため、基底表側は読まない
        // （可視行数が入力行数上限を超える表でも、本来空で終わる文が `54000` にならない）。
        let mut sides: Vec<Vec<ResultRow>> = Vec::new();
        if let (
            true,
            Step::Join {
                prepared, cte_side, ..
            },
        ) = (start < end, &p.step)
        {
            for side in 0..prepared.side_count() {
                if side == *cte_side {
                    sides.push(Vec::new());
                } else {
                    // 走査の確保上限は、保持済みの全確保を差し引いた残予算（確保前に拒否される）。
                    let scan_limits = join_limits.with_budget_cap(budget.remaining());
                    let rows = prepared.scan_side_once(read_txn, ctx, side, &scan_limits)?;
                    for row in &rows {
                        budget.admit(&row.cells, Charge::Fixed)?;
                    }
                    sides.push(rows);
                }
            }
        }

        let mut iterations = 0usize;
        while start < end {
            crate::interrupt::check()?;
            iterations = iterations.saturating_add(1);
            if iterations > limits.max_iterations {
                return Err(SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the iteration limit",
                ));
            }
            let working = result
                .get(start..end)
                .ok_or_else(|| internal("working table range out of bounds"))?;
            let mut parents: Vec<usize> = Vec::new();
            // 次の作業表へ渡さない行（印が `TO` 値の行）。`TO` と `DEFAULT` が同じ値の指定では、
            // 循環でなくても印が `TO` 値と等しいため、PostgreSQL の書き換え（`WHERE 印 <> TO`）と同じく
            // どの行も再帰しない。
            let skip: Option<Vec<bool>> = match cycle {
                Some(cp) => Some(
                    hits.get(start..end)
                        .ok_or_else(|| internal("working table range out of bounds"))?
                        .iter()
                        .map(|h| *h || cp.default_is_to)
                        .collect(),
                ),
                None => None,
            };
            let new_rows = match &p.step {
                Step::Scan(term) => {
                    // 重複除去版は候補行を `Scratch` で作り、未出の行だけを結果へ移す。
                    let charge = if dedup.is_some() {
                        Charge::Scratch
                    } else {
                        Charge::Retained
                    };
                    let rows = term.eval_over_rows(
                        &p.work_types,
                        working,
                        &mut budget,
                        charge,
                        track.then_some(&mut parents),
                        dedup.as_mut(),
                        skip.as_deref(),
                    )?;
                    budget.clear_scratch();
                    rows
                }
                Step::Join {
                    prepared,
                    cte_side,
                    side_term,
                } => {
                    // 作業表側の行は、側スキャンと同じ束縛（述語・射影）を作業表へ適用して作る
                    // （反復内だけ保持する一時行として同じ共有予算へ計上する）。
                    let mut side_parents: Vec<usize> = Vec::new();
                    let side_rows = side_term.eval_over_rows(
                        &p.work_types,
                        working,
                        &mut budget,
                        Charge::Scratch,
                        track.then_some(&mut side_parents),
                        None,
                        skip.as_deref(),
                    )?;
                    let slot = sides
                        .get_mut(*cte_side)
                        .ok_or_else(|| internal("recursive reference side out of range"))?;
                    *slot = side_rows;
                    // JOIN の中間結果・出力の確保上限は、保持済みの結果を差し引いた残予算。
                    let step_limits = join_limits.with_budget_cap(budget.remaining_for_join());
                    // 重複除去版は候補行へ最終結果の行数上限を適用しない（捨てる重複は累積行数に
                    // 数えない契約。結果行数は重複除去後の `admit` で判定する）。
                    let step_limits = if dedup.is_some() {
                        step_limits.with_max_result_rows(usize::MAX)
                    } else {
                        step_limits
                    };
                    let (joined, side_idx) = prepared.run_with_sides_provenance(
                        &sides,
                        &step_limits,
                        track.then_some(*cte_side),
                    )?;
                    if let Some(slot) = sides.get_mut(*cte_side) {
                        slot.clear();
                    }
                    // JOIN 出力行 → 作業表側の行 → 作業表の行、の 2 段で親を引く。
                    if track {
                        parents = side_idx
                            .iter()
                            .map(|&k| side_parents.get(k).copied())
                            .collect::<Option<Vec<usize>>>()
                            .ok_or_else(|| internal("recursive parent mapping out of range"))?;
                    }
                    budget.clear_scratch();
                    let joined_rows = joined.rows;
                    if let Some(d) = dedup.as_mut() {
                        // `SEARCH` との併用は計画段で拒否済みのため `parents` は空。
                        // JOIN 出力は保持したままキーを追加するため、出力ぶんを先に一時行として
                        // 共有予算へ計上し、キー確保が出力と合算で上限を超えないようにする。
                        for row in &joined_rows {
                            budget.reserve(RowBudget::row_bytes(&row.cells), Charge::Scratch)?;
                        }
                        let mut kept: Vec<ResultRow> = Vec::new();
                        kept.try_reserve(joined_rows.len()).map_err(|_| {
                            SqlSurfaceError::payload_too_large(
                                "recursive query exceeds the row limit",
                            )
                        })?;
                        for row in joined_rows {
                            if d.insert(&row, &mut budget)? {
                                budget.promote(&row.cells)?;
                                kept.push(row);
                            } else {
                                budget.discard_scratch(&row.cells);
                            }
                        }
                        budget.clear_scratch();
                        kept
                    } else {
                        for row in &joined_rows {
                            budget.admit(&row.cells, Charge::Retained)?;
                        }
                        joined_rows
                    }
                }
            };
            result.try_reserve(new_rows.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
            if track && parents.len() != new_rows.len() {
                return Err(internal("recursive parent mapping length mismatch"));
            }
            if let Some(cp) = cycle {
                paths.try_reserve(new_rows.len()).map_err(|_| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?;
                hits.try_reserve(new_rows.len()).map_err(|_| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?;
                for (row, parent) in new_rows.iter().zip(parents.iter()) {
                    // `parent` は作業表（`start..end`）内の位置。経路・循環フラグは `result` と同じ添字。
                    let parent_path = start
                        .checked_add(*parent)
                        .and_then(|i| paths.get(i))
                        .ok_or_else(|| internal("recursive parent path out of range"))?;
                    let hit = cp.is_cycle(parent_path, row)?;
                    budget.reserve(cp.mark_bytes(hit), Charge::Retained)?;
                    let path = cp.path.child(parent_path, row, &mut budget)?;
                    paths.push(path);
                    hits.push(hit);
                }
            }
            if let Some(sp) = search {
                seqs.try_reserve(new_rows.len()).map_err(|_| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?;
                for (row, parent) in new_rows.iter().zip(parents.iter()) {
                    // `parent` は作業表（`start..end`）内の位置。順序列は `result` と同じ添字。
                    let parent_seq = start
                        .checked_add(*parent)
                        .and_then(|i| seqs.get(i))
                        .ok_or_else(|| internal("recursive parent sequence out of range"))?;
                    let seq = sp.child(parent_seq, row, &mut budget)?;
                    seqs.push(seq);
                }
            }
            start = end;
            result.extend(new_rows);
            end = result.len();
        }
        if track {
            if search.is_some() && seqs.len() != result.len() {
                return Err(internal("recursive sequence column length mismatch"));
            }
            if cycle.is_some() && (paths.len() != result.len() || hits.len() != result.len()) {
                return Err(internal("recursive cycle column length mismatch"));
            }
            // 結果列の末尾へ、順序列 → 印 → 経路の順に足す（公開列の並びと一致させる）。
            let extra =
                usize::from(search.is_some()).saturating_add(if cycle.is_some() { 2 } else { 0 });
            let mut seq_it = seqs.into_iter();
            let mut path_it = paths.into_iter();
            let mut hit_it = hits.into_iter();
            for row in result.iter_mut() {
                // 倍々の再確保で行のセル配列が膨らまないよう、足す個数ぶんだけ確保する。
                row.cells.try_reserve_exact(extra).map_err(|_| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?;
                if search.is_some() {
                    row.cells.push(
                        seq_it
                            .next()
                            .ok_or_else(|| internal("recursive sequence column length mismatch"))?,
                    );
                }
                if let Some(cp) = cycle {
                    let hit = hit_it
                        .next()
                        .ok_or_else(|| internal("recursive cycle column length mismatch"))?;
                    row.cells.push(cp.mark_cell(hit));
                    row.cells.push(
                        path_it
                            .next()
                            .ok_or_else(|| internal("recursive cycle column length mismatch"))?,
                    );
                }
            }
        }
        Ok(QueryResult {
            columns: p.columns.clone(),
            rows: result,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_bytes: usize) -> RecursiveLimits {
        RecursiveLimits {
            max_iterations: 10,
            max_rows: 100,
            max_bytes,
        }
    }

    fn text_cells(n: usize) -> Vec<Cell> {
        vec![Cell::Text("x".repeat(n))]
    }

    /// 結果・基底表側・一時行が 1 つのバイト予算を共有し、一時行は解放できる。
    #[test]
    fn budget_is_shared_across_retained_fixed_and_scratch() {
        let one = {
            let mut b = RowBudget::new(limits(usize::MAX));
            b.admit(&text_cells(100), Charge::Retained).unwrap();
            b.retained
        };
        // 3 行ぶんは入るが 4 行目は入らない予算。
        let mut b = RowBudget::new(limits(one * 3 + one / 2));
        b.admit(&text_cells(100), Charge::Retained).unwrap();
        b.admit(&text_cells(100), Charge::Fixed).unwrap();
        b.admit(&text_cells(100), Charge::Scratch).unwrap();
        let err = b.admit(&text_cells(100), Charge::Retained).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
        // 一時行を解放すれば再び確保できる。
        b.clear_scratch();
        b.admit(&text_cells(100), Charge::Retained).unwrap();
        // 基底表側・一時行は行数上限に数えない。
        assert_eq!(b.rows, 2);
    }

    /// `promote` は一時行の計上を結果へ付け替え、合計バイトは変えず、行数上限を超える付け替えは拒否する。
    #[test]
    fn promote_moves_scratch_to_retained_without_double_charging() {
        let mut b = RowBudget::new(RecursiveLimits {
            max_iterations: 10,
            max_rows: 1,
            max_bytes: usize::MAX,
        });
        let cells = text_cells(10);
        b.admit(&cells, Charge::Scratch).unwrap();
        let before = b.total();
        b.promote(&cells).unwrap();
        assert_eq!((b.total(), b.scratch, b.rows), (before, 0, 1));
        b.admit(&cells, Charge::Scratch).unwrap();
        assert_eq!(b.promote(&cells).unwrap_err().wire_code(), "54000");
        // 捨てる一時行は計上を解放する。
        b.discard_scratch(&cells);
        assert_eq!(b.scratch, 0);
    }

    /// 重複除去の集合: 同値の行は 2 度目に `false`、キーの計上は確保前に判定され、予算超過は `54000`。
    #[test]
    fn dedup_set_detects_duplicates_and_charges_keys_first() {
        let row = |n: i64| ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::SignedInteger(n)],
        };
        let mut b = RowBudget::new(limits(usize::MAX));
        let mut d = DedupSet::new();
        assert!(d.insert(&row(1), &mut b).unwrap());
        assert!(!d.insert(&row(1), &mut b).unwrap());
        assert!(d.insert(&row(2), &mut b).unwrap());
        // キーは行数に数えない。
        assert_eq!(b.rows, 0);
        assert!(b.dedup_keys > 0);
        // JOIN へ渡す残予算はキー保持量を差し引く。
        assert_eq!(
            b.remaining_for_join(),
            b.limits.max_bytes - b.retained - b.dedup_keys
        );
        let mut tight = RowBudget::new(limits(1));
        let mut d2 = DedupSet::new();
        assert_eq!(
            d2.insert(&row(1), &mut tight).unwrap_err().wire_code(),
            "54000"
        );
        assert!(d2.seen.is_empty());
    }

    /// 疑似列 `id` は作業表では `u64` 全域を運べる `id` 型（numeric）へ写る（BIGINT へは写さない）。
    #[test]
    fn work_column_maps_pseudo_id_to_the_lossless_id_type() {
        for m in [ColumnMeta::Id, ColumnMeta::AliasedId { name: "k".into() }] {
            let w = work_column(&m);
            assert!(matches!(w, ColumnMeta::AliasedId { .. }), "{w:?}");
            let (schema, types) = synth_schema("t", std::slice::from_ref(&w));
            assert!(matches!(
                types.first(),
                Some(Some(ColumnType::Numeric { .. }))
            ));
            assert_eq!(schema.columns.len(), 1);
        }
    }

    /// JOIN の再帰項で、保持済みの結果を差し引いた残予算が走査・JOIN の確保処理へ上限として渡る
    /// （確保前の拒否）。残予算が足りないときの拒否は走査・JOIN 側の確保前判定（`JOIN exceeds`／
    /// 走査の予算）から返り、確保後の事後計上（`recursive query exceeds`）には到達しない。
    #[test]
    fn join_and_base_scan_receive_the_remaining_budget() {
        use crate::catalog::{ColumnDef, TableSchema};
        use crate::core::{EngineCore, ParsedSql};
        use crate::kernel::CpuScalarProvider;
        use crate::policy::PolicyContext;
        use crate::recovery::required_op_id::OperationId;
        use crate::row_codec::Value;
        use crate::sql::allowlist::Statement;
        use crate::sql::exec::QueryResult;
        use crate::storage::{Storage, Visibility};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
        use redb::ReadableDatabase;

        let path = unique_db_path("recursive-eval-budget");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "edges",
            vec![
                ColumnDef::new("src", ColumnType::BigInt, false),
                ColumnDef::new("dst", ColumnType::BigInt, false),
            ],
        );
        storage.create_table(&schema).expect("create table");
        let ctx = PolicyContext::with_visibilities("t1", [Visibility::Public]).expect("ctx");
        for i in 0..50u64 {
            crate::tenant::insert_typed_row(
                &storage,
                "edges",
                &ctx,
                i + 1,
                Visibility::Public,
                &[Value::BigInt(1), Value::BigInt(1)],
                &OperationId::parse(&format!("edge-{i}")).expect("op id"),
            )
            .expect("insert edge");
        }
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let sql = "WITH RECURSIVE r(n) AS (SELECT CAST(1 AS BIGINT) UNION ALL \
                   SELECT e.dst FROM edges e JOIN r ON e.src = r.n) SELECT n FROM r";
        let parsed = core.parse_sql(sql).expect("parse");
        let ParsedSql::Statement(Statement::BufferedView(view)) = &parsed else {
            panic!("expected a buffered view");
        };
        let Statement::RecursiveCte(cte) = &*view.body else {
            panic!("expected a recursive cte");
        };
        let schemas: HashMap<String, TableSchema> =
            HashMap::from([("edges".to_string(), schema.clone())]);
        let anchor = || QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: "n".to_string(),
                ty: ColumnType::BigInt,
            }],
            rows: vec![ResultRow {
                id: 0,
                score: 0.0,
                cells: vec![Cell::SignedInteger(1)],
            }],
        };
        let row_bytes = |cells: &[Cell]| {
            let mut b = RowBudget::new(limits(usize::MAX));
            b.admit(cells, Charge::Retained).unwrap();
            b.retained
        };
        let anchor_bytes = row_bytes(&[Cell::SignedInteger(1)]);
        let edge_bytes = row_bytes(&[Cell::SignedInteger(1), Cell::SignedInteger(1)]);
        let read_txn = core.storage_for_test().db().begin_read().unwrap();
        let udfs = UdfRegistry::default();
        let join_limits = JoinLimits::default();
        let run = |max_bytes: usize| {
            execute(
                &read_txn,
                &ctx,
                cte,
                anchor(),
                &schemas,
                &udfs,
                &join_limits,
                RecursiveLimits {
                    max_iterations: 5,
                    max_rows: 1_000_000,
                    max_bytes,
                },
            )
        };
        // 基底表（50 行）が残予算に入らない: 走査の確保前判定で拒否される。
        let err = run(anchor_bytes + edge_bytes * 10).expect_err("scan must be refused");
        assert_eq!(err.wire_code(), "54000");
        assert!(
            !format!("{err:?}").contains("recursive query exceeds"),
            "{err:?}"
        );
        // 側の行（基底表・作業表側）は入るが JOIN の中間結果・出力は入らない: JOIN の確保前判定で拒否される
        // （残予算が渡らないと JOIN は満額で確保し、事後の計上でしか拒否されない）。
        let err = run(anchor_bytes * 2 + edge_bytes * 50 + 8).expect_err("join must be refused");
        assert_eq!(err.wire_code(), "54000");
        assert!(format!("{err:?}").contains("JOIN exceeds"), "{err:?}");
        // 十分なバイト予算では、バイト予算が原因では拒否されない（行数・反復の上限が先に効く）。
        let err = run(usize::MAX / 2).expect_err("row or iteration limit");
        assert!(!format!("{err:?}").contains("byte budget"), "{err:?}");
    }

    /// 確保の順序（見積もり → 計上 → 複製）: 残予算に入らない行は、計上の段階で `54000` になり、
    /// 計上済みの量は変わらない（複製・生成に進まない）。
    #[test]
    fn reserve_refuses_before_any_allocation_and_keeps_the_accounting() {
        let cells = text_cells(10_000);
        let need = RowBudget::row_overhead(1) + cell_payload_bytes(&cells[0]);
        let mut b = RowBudget::new(limits(need - 1));
        // 行の構造体は入るが、ペイロード（借用セルからの見積もり）が入らない。
        b.reserve_row(1, Charge::Retained).unwrap();
        let before = b.retained;
        let err = b
            .reserve(cell_payload_bytes(&cells[0]), Charge::Retained)
            .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
        assert_eq!(b.retained, before);
        // 式セルの生成上限は残予算（全体上限ではない）。残予算を超えるテキストは確保前に拒否される。
        let mut produced = 0usize;
        let value = crate::sql::udf_call::ExprValue::Text(std::borrow::Cow::Borrowed(
            "y".repeat(10_000).leak(),
        ));
        let err = crate::sql::scan::computed_value_to_cell(
            value,
            Some(&ColumnType::Text),
            &mut produced,
            b.headroom(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
        assert_eq!(produced, 0);
    }

    /// 保持量が上限近くのとき、大きな TEXT の直接射影・式射影（連結）は `54000` になる
    /// （Scan 版の作業表の射影。確保前の見積もり・残予算での生成上限は上の単体テストが固定する）。
    #[test]
    fn large_text_projection_near_the_limit_is_refused() {
        use crate::core::{EngineCore, ParsedSql};
        use crate::kernel::CpuScalarProvider;
        use crate::policy::PolicyContext;
        use crate::sql::allowlist::Statement;
        use crate::sql::exec::QueryResult;
        use crate::storage::{Storage, Visibility};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("recursive-eval-text-budget");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let ctx = PolicyContext::with_visibilities("t1", [Visibility::Public]).expect("ctx");
        let big = "z".repeat(200_000);
        let anchor_cells = vec![Cell::Text(big)];
        let anchor_bytes = {
            let mut b = RowBudget::new(limits(usize::MAX));
            b.admit(&anchor_cells, Charge::Retained).unwrap();
            b.retained
        };
        let read_txn = {
            use redb::ReadableDatabase;
            core.storage_for_test().db().begin_read().unwrap()
        };
        let udfs = UdfRegistry::default();
        let join_limits = JoinLimits::default();
        let schemas: HashMap<String, TableSchema> = HashMap::new();
        for body in [
            "SELECT t FROM r WHERE 1 = 1",
            "SELECT CONCAT(t, t) FROM r WHERE 1 = 1",
        ] {
            let sql = format!(
                "WITH RECURSIVE r(t) AS (SELECT CAST('a' AS TEXT) UNION ALL {body}) SELECT t FROM r"
            );
            let parsed = core.parse_sql(&sql).expect("parse");
            let ParsedSql::Statement(Statement::BufferedView(view)) = &parsed else {
                panic!("expected a buffered view");
            };
            let Statement::RecursiveCte(cte) = &*view.body else {
                panic!("expected a recursive cte");
            };
            let anchor = QueryResult {
                columns: vec![ColumnMeta::Scalar {
                    name: "t".to_string(),
                    ty: ColumnType::Text,
                }],
                rows: vec![ResultRow {
                    id: 0,
                    score: 0.0,
                    cells: anchor_cells.clone(),
                }],
            };
            // 保持量（アンカー 1 行）に、もう 1 行ぶん（直接射影は同じ大きさ、連結は 2 倍）は入らない。
            let err = execute(
                &read_txn,
                &ctx,
                cte,
                anchor,
                &schemas,
                &udfs,
                &join_limits,
                RecursiveLimits {
                    max_iterations: 5,
                    max_rows: 1_000,
                    max_bytes: anchor_bytes + 150_000,
                },
            )
            .expect_err("must be refused");
            assert_eq!(err.wire_code(), "54000", "{body}: {err:?}");
        }
    }
    fn search_row(n: i64) -> ResultRow {
        ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::SignedInteger(n)],
        }
    }

    fn search_columns() -> Vec<ColumnMeta> {
        vec![ColumnMeta::Scalar {
            name: "n".to_string(),
            ty: ColumnType::BigInt,
        }]
    }

    /// `SEARCH` の計画（SQL-35・Issue #2213）: 名前解決のエラー（PostgreSQL 17 の SQLSTATE）。
    #[test]
    fn plan_search_name_errors() {
        let clause = |by: &[&str], set: &str| SearchClause {
            breadth_first: false,
            by: by.iter().map(|s| (*s).to_string()).collect(),
            set: set.to_string(),
        };
        let cols = search_columns();
        assert_eq!(
            plan_search(&clause(&["x"], "o"), &cols)
                .err()
                .unwrap()
                .wire_code(),
            "42601"
        );
        assert_eq!(
            plan_search(&clause(&["n", "N"], "o"), &cols)
                .err()
                .unwrap()
                .wire_code(),
            "42701"
        );
        assert_eq!(
            plan_search(&clause(&["n"], "N"), &cols)
                .err()
                .unwrap()
                .wire_code(),
            "42601"
        );
        let (plan, meta) = plan_search(&clause(&["n"], "o"), &cols).unwrap();
        assert_eq!(plan.by_idx, vec![0]);
        assert!(matches!(meta, ColumnMeta::Record { array: true, .. }));
    }

    /// 深さ優先の経路は深さに比例して増えるため、複製の前に共有予算へ計上し、超過は `54000`。
    #[test]
    fn depth_first_path_growth_is_charged_before_cloning() {
        let plan = SearchPlan {
            breadth_first: false,
            by_idx: vec![0],
        };
        let mut budget = RowBudget::new(limits(usize::MAX));
        let mut seq = plan.initial(&search_row(1), &mut budget).unwrap();
        for n in 2..=5 {
            seq = plan.child(&seq, &search_row(n), &mut budget).unwrap();
        }
        match &seq {
            Cell::RecordArray(path) => assert_eq!(path.len(), 5),
            other => panic!("unexpected {other:?}"),
        }
        // 親の経路ぶんを含めて計上するため、予算がぎりぎり足りない状態では拒否され、計上は変わらない。
        let mut tight = RowBudget::new(limits(budget.retained / 4));
        let err = plan
            .child(&seq, &search_row(6), &mut tight)
            .expect_err("must be refused");
        assert_eq!(err.wire_code(), "54000");
        assert_eq!(tight.retained, 0);
    }

    /// 幅優先の深さは親の深さ + 1（アンカーは 0）。
    #[test]
    fn breadth_first_depth_increments() {
        let plan = SearchPlan {
            breadth_first: true,
            by_idx: vec![0],
        };
        let mut budget = RowBudget::new(limits(usize::MAX));
        let a = plan.initial(&search_row(7), &mut budget).unwrap();
        assert_eq!(
            a,
            Cell::Record(vec![Cell::SignedInteger(0), Cell::SignedInteger(7)])
        );
        let b = plan.child(&a, &search_row(8), &mut budget).unwrap();
        assert_eq!(
            b,
            Cell::Record(vec![Cell::SignedInteger(1), Cell::SignedInteger(8)])
        );
    }

    fn cycle_clause(
        columns: &[&str],
        mark: &str,
        path: &str,
        values: Option<(CycleConst, CycleConst)>,
    ) -> CycleClause {
        CycleClause {
            columns: columns.iter().map(|s| (*s).to_string()).collect(),
            mark: mark.to_string(),
            mark_values: values,
            path: path.to_string(),
        }
    }

    /// `CYCLE` の計画（SQL-35・Issue #2214）: 名前解決・型解決のエラー（PostgreSQL 17 の SQLSTATE）。
    #[test]
    fn plan_cycle_name_and_type_errors() {
        let cols = search_columns();
        let code = |c: &CycleClause, search_set: Option<&str>| {
            plan_cycle(c, &cols, search_set).err().unwrap().wire_code()
        };
        assert_eq!(code(&cycle_clause(&["x"], "c", "p", None), None), "42601");
        assert_eq!(
            code(&cycle_clause(&["n", "N"], "c", "p", None), None),
            "42701"
        );
        assert_eq!(code(&cycle_clause(&["n"], "N", "p", None), None), "42601");
        assert_eq!(code(&cycle_clause(&["n"], "c", "n", None), None), "42601");
        assert_eq!(code(&cycle_clause(&["n"], "c", "C", None), None), "42601");
        assert_eq!(
            code(&cycle_clause(&["n"], "o", "p", None), Some("o")),
            "42601"
        );
        let mixed = |to, default| code(&cycle_clause(&["n"], "c", "p", Some((to, default))), None);
        assert_eq!(
            mixed(CycleConst::Bool(true), CycleConst::Integer("1".into())),
            "42804"
        );
        assert_eq!(
            mixed(
                CycleConst::Integer("1".into()),
                CycleConst::Text("x".into())
            ),
            "22P02"
        );
        assert_eq!(mixed(CycleConst::Null, CycleConst::Bool(false)), "0A000");
        assert_eq!(
            mixed(
                CycleConst::Decimal("1.5".into()),
                CycleConst::Integer("0".into())
            ),
            "0A000"
        );
        // 比較できない型（JSON）の循環列は計画時に `0A000`。
        let json = vec![ColumnMeta::Scalar {
            name: "j".to_string(),
            ty: ColumnType::Json,
        }];
        assert_eq!(
            plan_cycle(&cycle_clause(&["j"], "c", "p", None), &json, None)
                .err()
                .unwrap()
                .wire_code(),
            "0A000"
        );
        let (plan, mark, path) =
            plan_cycle(&cycle_clause(&["n"], "c", "p", None), &cols, None).unwrap();
        assert_eq!(plan.path.by_idx, vec![0]);
        assert!(!plan.default_is_to);
        assert_eq!(plan.mark_to, Cell::Bool(true));
        assert!(matches!(
            mark,
            ColumnMeta::Computed {
                ty: Some(ColumnType::Boolean),
                ..
            }
        ));
        assert!(matches!(path, ColumnMeta::Record { array: true, .. }));
    }

    /// `TO`／`DEFAULT` の共通型: 整数は `i32` に収まらなければ bigint、文字列と整数は整数。
    #[test]
    fn resolve_cycle_marks_common_type() {
        let int = |s: &str| CycleConst::Integer(s.to_string());
        let (_, _, ty) = resolve_cycle_marks(&Some((int("1"), int("0")))).unwrap();
        assert_eq!(ty, ColumnType::Integer);
        let (to, _, ty) = resolve_cycle_marks(&Some((int("2147483648"), int("0")))).unwrap();
        assert_eq!(
            (to, ty),
            (Cell::SignedInteger(2_147_483_648), ColumnType::BigInt)
        );
        let (to, default, ty) =
            resolve_cycle_marks(&Some((CycleConst::Text(" 7 ".into()), int("5")))).unwrap();
        assert_eq!(
            (to, default, ty),
            (
                Cell::SignedInteger(7),
                Cell::SignedInteger(5),
                ColumnType::Integer
            )
        );
        // `i64` を超える整数リテラルは未対応（`0A000`）。
        assert_eq!(
            resolve_cycle_marks(&Some((int("9223372036854775808"), int("0"))))
                .unwrap_err()
                .wire_code(),
            "0A000"
        );
    }

    /// 循環判定は `=` の意味: 親の経路のどれかと全フィールドが非 NULL で等しければ循環、NULL を含めば一致しない。
    #[test]
    fn is_cycle_uses_equality_semantics() {
        let (plan, _, _) = plan_cycle(
            &cycle_clause(&["n"], "c", "p", None),
            &search_columns(),
            None,
        )
        .unwrap();
        let mut budget = RowBudget::new(limits(usize::MAX));
        let mut path = plan.path.initial(&search_row(1), &mut budget).unwrap();
        path = plan.path.child(&path, &search_row(2), &mut budget).unwrap();
        assert!(plan.is_cycle(&path, &search_row(1)).unwrap());
        assert!(plan.is_cycle(&path, &search_row(2)).unwrap());
        assert!(!plan.is_cycle(&path, &search_row(3)).unwrap());
        let null_row = ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        let null_path = Cell::RecordArray(vec![vec![Cell::Null]]);
        assert!(!plan.is_cycle(&null_path, &null_row).unwrap());
        // 経路の形が想定外なら `Internal`（fail-closed）。
        assert!(plan.is_cycle(&Cell::Null, &search_row(1)).is_err());
    }

    /// 経路の伸長は複製の前に共有予算へ計上され、狭い予算では `54000`。
    #[test]
    fn cycle_path_growth_is_charged_before_cloning() {
        let (plan, _, _) = plan_cycle(
            &cycle_clause(&["n"], "c", "p", None),
            &search_columns(),
            None,
        )
        .unwrap();
        let mut wide = RowBudget::new(limits(usize::MAX));
        let mut path = plan.path.initial(&search_row(1), &mut wide).unwrap();
        for n in 2..=5 {
            path = plan.path.child(&path, &search_row(n), &mut wide).unwrap();
        }
        let mut tight = RowBudget::new(limits(wide.retained / 4));
        let first = plan.path.initial(&search_row(1), &mut tight);
        let err = match first {
            Err(e) => e,
            Ok(p) => plan
                .path
                .child(&p, &search_row(2), &mut tight)
                .and_then(|p2| {
                    plan.path
                        .child(&p2, &search_row(3), &mut tight)
                        .and_then(|p3| plan.path.child(&p3, &search_row(4), &mut tight))
                })
                .unwrap_err(),
        };
        assert_eq!(err.wire_code(), "54000");
    }
}
