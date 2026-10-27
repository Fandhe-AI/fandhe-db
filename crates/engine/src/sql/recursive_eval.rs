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

use std::collections::HashMap;

use crate::catalog::{ColumnType, RowIdentity, TableSchema};
use crate::policy::PolicyContext;

use super::allowlist::{
    SetOperator, SqlSurfaceError, Statement, ValidatedRecursiveCte, ValidatedScan,
};
use super::exec::{computed_column_meta, Cell, ColumnMeta, QueryResult, ResultRow};
use super::expr_program::{ExprProgram, StackValue};
use super::join::{cell_payload_bytes, prepare_join, JoinLimits, PreparedJoin};
use super::parser::{BoundScan, ProjectedColumn};
use super::udf_call::UdfRegistry;
use super::view_buffered::{
    normalize_cell, rename_body_columns, synth_schema, where_matches_for_row, with_scanned,
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
            limits,
        }
    }

    fn total(&self) -> usize {
        self.retained
            .saturating_add(self.fixed)
            .saturating_add(self.scratch)
    }

    /// 全確保経路の合計に対する残りバイト（走査の確保上限）。
    fn remaining(&self) -> usize {
        self.limits.max_bytes.saturating_sub(self.total())
    }

    /// JOIN へ渡す上限（JOIN は側の行を自分の予算へ計上し直すため、保持済みの結果だけを差し引く）。
    fn remaining_for_join(&self) -> usize {
        self.limits.max_bytes.saturating_sub(self.retained)
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
    /// `budget` で行数・バイト数を判定する（超過は `54000`。部分結果は返さない）。
    fn eval_over_rows(
        &self,
        types: &[Option<ColumnType>],
        rows: &[ResultRow],
        budget: &mut RowBudget,
        charge: Charge,
    ) -> Result<Vec<ResultRow>, SqlSurfaceError> {
        let referenced = vec![true; types.len()];
        let mut where_scratch: Vec<StackValue> = Vec::new();
        let mut proj_scratch: Vec<StackValue> = Vec::new();
        let mut out: Vec<ResultRow> = Vec::new();
        for (n, row) in rows.iter().enumerate() {
            if n % 1024 == 0 {
                crate::interrupt::check()?;
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
                out.try_reserve(1).map_err(|_| {
                    SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
                })?;
                out.push(ResultRow {
                    id: 0,
                    score: 0.0,
                    cells,
                });
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
    /// CTE の公開列（`column_aliases` 適用後・型合成後）。
    columns: Vec<ColumnMeta>,
    /// 作業表の列型（`synth_schema` の `types`）。
    work_types: Vec<Option<ColumnType>>,
    step: Step<'a>,
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
    super::set_op::check_branch_type_compatibility(
        SetOperator::UnionAll,
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
    let columns = compose_columns(cte, anchor_columns, &work_anchor, &rec_work)?;
    check_work_types(&rec_work)?;
    f(&Prepared {
        columns,
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
        for row in &anchor_rows {
            budget.admit(&row.cells, Charge::Retained)?;
        }
        // 結果は 1 本の行列で持ち、作業表（前回の反復で増えた行）はその末尾区間 `start..end` として
        // 参照する（`result.clone()` や各反復の深い複製を作らない）。
        let mut result: Vec<ResultRow> = anchor_rows;
        let mut start = 0usize;
        let mut end = result.len();

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
            let new_rows = match &p.step {
                Step::Scan(term) => {
                    term.eval_over_rows(&p.work_types, working, &mut budget, Charge::Retained)?
                }
                Step::Join {
                    prepared,
                    cte_side,
                    side_term,
                } => {
                    // 作業表側の行は、側スキャンと同じ束縛（述語・射影）を作業表へ適用して作る
                    // （反復内だけ保持する一時行として同じ共有予算へ計上する）。
                    let side_rows = side_term.eval_over_rows(
                        &p.work_types,
                        working,
                        &mut budget,
                        Charge::Scratch,
                    )?;
                    let slot = sides
                        .get_mut(*cte_side)
                        .ok_or_else(|| internal("recursive reference side out of range"))?;
                    *slot = side_rows;
                    // JOIN の中間結果・出力の確保上限は、保持済みの結果を差し引いた残予算。
                    let step_limits = join_limits.with_budget_cap(budget.remaining_for_join());
                    let joined = prepared.run_with_sides(&sides, &step_limits)?;
                    if let Some(slot) = sides.get_mut(*cte_side) {
                        slot.clear();
                    }
                    budget.clear_scratch();
                    for row in &joined.rows {
                        budget.admit(&row.cells, Charge::Retained)?;
                    }
                    joined.rows
                }
            };
            result.try_reserve(new_rows.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
            start = end;
            result.extend(new_rows);
            end = result.len();
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
}
