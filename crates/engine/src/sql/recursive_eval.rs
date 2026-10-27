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
//! （`57014`）。暫定の固定上限は [`RecursiveLimits`] の既定値で、起動時設定への差し替えは
//! #1871 の担当。

use std::collections::HashMap;

use crate::catalog::{ColumnType, TableSchema};
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

/// 暫定の反復回数上限（再帰項の実行回数。最後の「0 行になる実行」を含む）。#1871 が起動時設定へ
/// 差し替える前提の固定値で、`crates/engine/src/recursive_cte_limits.rs`（`correlated_limits.rs`
/// 流儀）へ移す。
pub(crate) const DEFAULT_MAX_RECURSIVE_ITERATIONS: usize = 1_000;

/// 暫定の累積行数上限（アンカー行を含む全反復の合計）。#1871 が起動時設定へ差し替える前提。
pub(crate) const DEFAULT_MAX_RECURSIVE_ROWS: usize = 100_000;

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
    /// 呼び出し段の `--max-result-rows`（SQL-34）を累積行数の上限へ畳み込んだ上限を返す。
    /// 具体化 CTE と同じく、CTE 全体の行数が呼び出し段の上限を超えたら `54000` にする。
    pub(crate) fn with_result_cap(max_result_rows: usize) -> Self {
        Self {
            max_iterations: DEFAULT_MAX_RECURSIVE_ITERATIONS,
            max_rows: DEFAULT_MAX_RECURSIVE_ROWS.min(max_result_rows),
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

/// 行数・バイト数の累計予算（確保前に判定する）。
struct RowBudget {
    rows: usize,
    bytes: usize,
    limits: RecursiveLimits,
}

impl RowBudget {
    fn new(limits: RecursiveLimits) -> Self {
        Self {
            rows: 0,
            bytes: 0,
            limits,
        }
    }

    /// 1 行（`cells`）を結果へ加える前に計上する。超過は `54000`。
    fn admit(&mut self, cells: &[Cell]) -> Result<(), SqlSurfaceError> {
        let rows = self
            .rows
            .checked_add(1)
            .filter(|n| *n <= self.limits.max_rows)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
        let payload = cells
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(cell_payload_bytes(c)));
        let row_bytes = cells
            .len()
            .saturating_mul(std::mem::size_of::<Cell>())
            .saturating_add(std::mem::size_of::<ResultRow>())
            .saturating_add(payload);
        let bytes = self
            .bytes
            .checked_add(row_bytes)
            .filter(|n| *n <= self.limits.max_bytes)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the shared result byte budget",
                )
            })?;
        self.rows = rows;
        self.bytes = bytes;
        Ok(())
    }
}

/// 作業表上の走査（束縛済み述語・射影と、式項目のコンパイル済みプログラム）。
struct ScanTerm {
    bound: BoundScan,
    programs: Vec<Option<ExprProgram>>,
}

impl ScanTerm {
    fn new(bound: BoundScan) -> Result<Self, SqlSurfaceError> {
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
        Ok(Self { bound, programs })
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

    /// 作業表の行 `rows` に `WHERE` と射影を適用した行を返す。1 行ごとに、結果へ加える前に
    /// `budget` で行数・バイト数を判定する（超過は `54000`。部分結果は返さない）。
    fn eval_over_rows(
        &self,
        types: &[Option<ColumnType>],
        rows: &[ResultRow],
        budget: &mut RowBudget,
    ) -> Result<Vec<ResultRow>, SqlSurfaceError> {
        let referenced = vec![true; types.len()];
        let mut where_scratch: Vec<StackValue> = Vec::new();
        let mut proj_scratch: Vec<StackValue> = Vec::new();
        let mut cell_bytes = 0usize;
        let mut out: Vec<ResultRow> = Vec::new();
        for (n, row) in rows.iter().enumerate() {
            if n % 1024 == 0 {
                crate::interrupt::check()?;
            }
            let cells =
                with_scanned(types, &referenced, row, |scanned| {
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
                    let mut cells: Vec<Cell> = Vec::with_capacity(self.bound.projection.len());
                    for (i, col) in self.bound.projection.iter().enumerate() {
                        match col {
                            ProjectedColumn::Column { index, .. } => {
                                let cell = row.cells.get(*index).ok_or_else(|| {
                                    internal("projected column index out of range")
                                })?;
                                cells.push(match types.get(*index) {
                                    Some(Some(ty)) => normalize_cell(cell, ty)?.into_owned(),
                                    _ => cell.clone(),
                                });
                            }
                            ProjectedColumn::Computed { result_ty, .. } => {
                                let program =
                                    self.programs.get(i).and_then(|p| p.as_ref()).ok_or_else(
                                        || internal("computed projection program missing"),
                                    )?;
                                let value = program.eval(0, &[], scanned, &mut proj_scratch)?;
                                cells.push(crate::sql::scan::computed_value_to_cell(
                                    value,
                                    result_ty.as_ref(),
                                    &mut cell_bytes,
                                    budget.limits.max_bytes,
                                )?);
                            }
                            ProjectedColumn::Id | ProjectedColumn::AliasedId { .. } => {
                                return Err(internal("pseudo column in the recursive reference"));
                            }
                        }
                    }
                    Ok(Some(cells))
                })?;
            if let Some(cells) = cells {
                budget.admit(&cells)?;
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

/// 列整合の検査と結果列の合成（`UNION ALL` と同じ規則）。型は PostgreSQL と同じくアンカー項の
/// 列を正とし、再帰項は互換でなければ `42804`。
fn compose_columns(
    cte: &ValidatedRecursiveCte,
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
    let merged = super::set_op::merge_columns(anchor_columns, rec_columns);
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
    let named: Vec<ColumnMeta>;
    let exposed: &[ColumnMeta] = if cte.column_aliases.is_empty() {
        anchor_columns
    } else {
        named = rename_body_columns(anchor_columns, &cte.column_aliases)?;
        &named
    };
    // 疑似列 `id`（行 id）を結果列にしたアンカー項の列は、作業表では BIGINT の実列として扱う
    // （`id` の値は `u64` のまま運ばれ、結合キー・述語に使える。作業表自身は行 id を持たない）。
    let synth_input: Vec<ColumnMeta> = exposed
        .iter()
        .map(|m| match m {
            ColumnMeta::Id => ColumnMeta::Scalar {
                name: "id".to_string(),
                ty: ColumnType::BigInt,
            },
            ColumnMeta::AliasedId { name } => ColumnMeta::Scalar {
                name: name.clone(),
                ty: ColumnType::BigInt,
            },
            other => other.clone(),
        })
        .collect();
    let (work_schema, work_types) = synth_schema(&cte.name, &synth_input);

    let mut all: HashMap<String, TableSchema> = schemas.clone();
    all.insert(cte.name.clone(), work_schema.clone());

    let (rec_columns, step): (Vec<ColumnMeta>, Step<'_>) = match &*cte.recursive {
        Statement::Scan(scan) => {
            if !scan.table_name.eq_ignore_ascii_case(&cte.name) {
                return Err(unsupported_term(
                    "the recursive term must read the recursive reference",
                ));
            }
            check_scan_shape(scan)?;
            let bound = super::parser::bind_scan(scan, &work_schema, udfs)?;
            let term = ScanTerm::new(bound)?;
            (term.column_metas(&work_schema)?, Step::Scan(term))
        }
        Statement::Join(join) => {
            let mut cte_side: Option<usize> = None;
            for (i, rel) in join.relations.iter().enumerate() {
                if rel.table().eq_ignore_ascii_case(&cte.name) {
                    if cte_side.is_some() {
                        return Err(unsupported_term(
                            "the recursive reference must appear exactly once in the recursive term",
                        ));
                    }
                    cte_side = Some(i);
                    // 関係名の書き方（大文字小文字）に依らず合成スキーマへ解決させる（同名の実表を隠す）。
                    all.insert(rel.table().to_string(), work_schema.clone());
                }
            }
            let cte_side = cte_side.ok_or_else(|| {
                unsupported_term("the recursive term must read the recursive reference")
            })?;
            let prepared = prepare_join(&all, join, udfs, join_limits)?;
            if prepared.is_aggregate() || prepared.has_ordering_or_limit() {
                return Err(unsupported_term(
                    "aggregation, ORDER BY and LIMIT in a recursive term are not supported",
                ));
            }
            let (_, side_bound) = prepared
                .side_scan(cte_side)
                .ok_or_else(|| internal("recursive reference side out of range"))?;
            let side_term = ScanTerm::new(side_bound.clone())?;
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
    let columns = compose_columns(cte, anchor_columns, &rec_columns)?;
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
            budget.admit(&row.cells)?;
        }
        let mut result: Vec<ResultRow> = anchor_rows;
        let mut working: Vec<ResultRow> = result.clone();

        // JOIN の基底表側は反復によらず同じ行を返す（同一 `read_txn`）ため、ループの前に 1 度だけ読む。
        let mut sides: Vec<Vec<ResultRow>> = Vec::new();
        if let Step::Join {
            prepared, cte_side, ..
        } = &p.step
        {
            for side in 0..prepared.side_count() {
                if side == *cte_side {
                    sides.push(Vec::new());
                } else {
                    sides.push(prepared.scan_side_once(read_txn, ctx, side, join_limits)?);
                }
            }
        }

        let mut iterations = 0usize;
        while !working.is_empty() {
            crate::interrupt::check()?;
            iterations = iterations.saturating_add(1);
            if iterations > limits.max_iterations {
                return Err(SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the iteration limit",
                ));
            }
            let new_rows = match &p.step {
                Step::Scan(term) => term.eval_over_rows(&p.work_types, &working, &mut budget)?,
                Step::Join {
                    prepared,
                    cte_side,
                    side_term,
                } => {
                    // 作業表側の行は、側スキャンと同じ束縛（述語・射影）を作業表へ適用して作る。
                    // 結果への計上は結合結果の行で行うため、側の行は別予算（上限は同じ）で評価する。
                    let mut side_budget = RowBudget::new(limits);
                    let side_rows =
                        side_term.eval_over_rows(&p.work_types, &working, &mut side_budget)?;
                    let slot = sides
                        .get_mut(*cte_side)
                        .ok_or_else(|| internal("recursive reference side out of range"))?;
                    *slot = side_rows;
                    let joined = prepared.run_with_sides(&sides, join_limits)?;
                    for row in &joined.rows {
                        budget.admit(&row.cells)?;
                    }
                    joined.rows
                }
            };
            result.try_reserve(new_rows.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("recursive query exceeds the row limit")
            })?;
            result.extend(new_rows.iter().cloned());
            working = new_rows;
        }
        Ok(QueryResult {
            columns: p.columns.clone(),
            rows: result,
        })
    })
}
