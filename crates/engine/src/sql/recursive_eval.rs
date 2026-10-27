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

    /// 一時行の計上を解放する（反復の終わり）。
    fn clear_scratch(&mut self) {
        self.scratch = 0;
    }

    /// 1 行（`cells`）を保持する前に計上する。超過は `54000`。
    fn admit(&mut self, cells: &[Cell], charge: Charge) -> Result<(), SqlSurfaceError> {
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
        let payload = cells
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(cell_payload_bytes(c)));
        let row_bytes = cells
            .len()
            .saturating_mul(std::mem::size_of::<Cell>())
            .saturating_add(std::mem::size_of::<ResultRow>())
            .saturating_add(payload);
        self.total()
            .checked_add(row_bytes)
            .filter(|n| *n <= self.limits.max_bytes)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(
                    "recursive query exceeds the shared result byte budget",
                )
            })?;
        self.rows = rows;
        match charge {
            Charge::Retained => self.retained = self.retained.saturating_add(row_bytes),
            Charge::Fixed => self.fixed = self.fixed.saturating_add(row_bytes),
            Charge::Scratch => self.scratch = self.scratch.saturating_add(row_bytes),
        }
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
        charge: Charge,
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
                budget.admit(&cells, charge)?;
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
/// 作業表では BIGINT の実列として扱う（`id` の値は `u64` のまま運ばれ、結合キー・述語に使える。
/// 作業表自身は行 id を持たない）。作業表スキーマ・列型（`work_types`）・型整合検査
/// （[`compose_columns`]）・再帰項の束縛のすべてがこの写像の結果を使う。
fn work_column(m: &ColumnMeta) -> ColumnMeta {
    match m {
        ColumnMeta::Id => ColumnMeta::Scalar {
            name: "id".to_string(),
            ty: ColumnType::BigInt,
        },
        ColumnMeta::AliasedId { name } => ColumnMeta::Scalar {
            name: name.clone(),
            ty: ColumnType::BigInt,
        },
        other => other.clone(),
    }
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
    let (work_schema, work_types) = synth_schema(&cte.name, &synth_input);
    // 作業表は行 id を持たない: 疑似列 `id` を持たない行識別方式（`Allocated`）にして、式・述語・
    // JOIN 条件・ORDER BY を含む全ての `id` 参照を束縛段の未知列として拒否する（実列 `id` は
    // 合成列として解決される）。直接投影の検査（`ScanTerm::new`）は二重の防御として残す。
    let work_schema = work_schema.with_row_identity(RowIdentity::Allocated);
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
    // 再帰項の疑似列 `id` も同じ写像で BIGINT の実列として扱う（アンカー側と対称にする）。
    let rec_work: Vec<ColumnMeta> = rec_columns.iter().map(work_column).collect();
    let columns = compose_columns(cte, &work_anchor, &rec_work)?;
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
        let mut sides: Vec<Vec<ResultRow>> = Vec::new();
        if let Step::Join {
            prepared, cte_side, ..
        } = &p.step
        {
            for side in 0..prepared.side_count() {
                if side == *cte_side {
                    sides.push(Vec::new());
                } else {
                    let rows = prepared.scan_side_once(read_txn, ctx, side, join_limits)?;
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
                    let joined = prepared.run_with_sides(&sides, join_limits)?;
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

    #[test]
    fn work_column_maps_pseudo_id_to_bigint() {
        for m in [ColumnMeta::Id, ColumnMeta::AliasedId { name: "k".into() }] {
            assert!(matches!(
                work_column(&m),
                ColumnMeta::Scalar {
                    ty: ColumnType::BigInt,
                    ..
                }
            ));
        }
    }
}
