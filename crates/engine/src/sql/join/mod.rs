//! `CROSS JOIN`・`[INNER|LEFT|RIGHT|FULL [OUTER]] JOIN`（等価結合の left-deep 連鎖。2 テーブル以上・
//! 結合での集計形・スカラー `ORDER BY`・`OR`／`IN`／列同士の比較）の束縛・実行本体
//! （SQL-28・RLS-10、TASK-212、Issue #925・#926・#1190）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` が構造検証した
//! [`crate::sql::allowlist::ValidatedJoin`] を受け取り、`core.rs::EngineCore`
//! の SQL 実行経路（session-less `execute_sql`・セッション経由
//! `execute_validated_in_session`）から呼ばれる。`sql::set_op`
//! （TASK-213・集合演算）と同じ設計判断を踏襲する:
//!
//! - 各 relation（テーブル参照）は既存の広域取得経路
//!   （[`crate::sql::scan::execute_scan_with_budget`]）でそれぞれ独立に評価する。
//!   これにより RLS 暗黙適用・fail-closed のエラー契約を第 2 の実行器を作らずに
//!   継承する（呼び出したセッション自身の [`crate::policy::PolicyContext`] で
//!   全 relation を独立に評価するため、他テナント行が中間結果・結合キー・カーディナ
//!   リティ判定・NULL 補完・集計値のいずれにも現れない。3 方向以上・自己結合でも
//!   参照ごとに独立に評価する）。
//! - 文全体（全 relation の走査・ハッシュ表・タプル・グループ表・出力行の生成）で
//!   1 つの累計バイト予算（[`JoinBudget`]）を共有する（`sql::set_op::SetOpBudget` と
//!   同じ理由。security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
//!
//! モジュール構成（責務ごとに分割）:
//! - `plan`: 束縛（列解決・型検証・外部結合の簡約・WHERE のプッシュダウン／残余の
//!   分離・側スキャン投影の確定・集計／並べ替えの束縛）。Describe も同じ計画を使う。
//! - `exec`: N 方向ハッシュ結合（段ごとのカーディナリティ上限・決定的順序）。
//! - `nlj`: 等価キーの無い ON 残余のある段のネステッドループ結合（INNER／LEFT／RIGHT。比較回数
//!   上限。Issue #1837・#1838。SQL からは #1839 で到達し、段ごとにハッシュ結合と自動で選ぶ。
//!   設計は `docs/design/nested-loop-join.md`）。
//! - `residual`: WHERE 残余の行単位評価（ON 残余の一致条件評価 `eval_all` も共有。Issue #1830）と
//!   スカラー `ORDER BY`。
//! - `aggregate`: 集計形（既存の `Accumulator` を再利用）。
//! - `values`: 型クラス・キー正準化・比較・`Cell` → `ScalarRef` アダプタ。
//! - `on_split`: ON の論理木（最上位 AND の conjunct 単位）を結合キー／残余述語（左のみ・右のみ・混在）へ分類する
//!   （Issue #1829・SQL-60 ポインタ）。
//! - `using`: `JOIN ... USING (...)` の束縛（等価 ON への脱糖）と結合列の統合規則・
//!   統合列を考慮した列解決（Issue #1826・SQL-60 (1)）。
//!
//! 設計判断の詳細は `docs/design/inner-join.md`・`docs/design/outer-join.md`・
//! `docs/design/multi-way-join.md`・`docs/design/join-using.md` 参照。
//!
//! `NATURAL JOIN`（Issue #1827）は共通列名を導出して USING と同じ束縛へ流す
//! （`using::natural_using_names`）。
//! `CROSS JOIN`（Issue #1831）は空キー段（条件なしの直積）で、件数 `左 × 右` を確保前に
//! 出力上限と照合する（`exec::join_step`）。
//! CTE 入力（Issue #1861・SQL-62）: `WITH` の主クエリの JOIN では、CTE のリレーションは `core.rs` が
//! 評価した行（`execute_with_cte_inputs` の `inputs`）に側スキャンを適用して側入力にする（記憶域を
//! 読まない）。合成スキーマは [`cte_schema`]、`docs/design/cte.md` の「Issue #1861 追記」節参照。
//! スコープ外: `USING (...) AS <alias>`・FULL JOIN USING の統合列の
//! WHERE／GROUP BY／集計／後続段の結合キーでの参照（`0A000`）・ベクトル順位付け
//! （`ORDER BY ... <=>` 等）との併用・FROM でのビュー参照・`RelationSnapshotCache`
//! （TASK-212 基盤）による結合入力のキャッシュ。

mod aggregate;
// `UPDATE ... FROM`/`DELETE ... USING` の結合実行（Issue #1755・SQL-53）。
pub(crate) mod dml;
mod exec;
mod nlj;
mod on_split;
mod plan;
mod residual;
mod using;
mod where_promote;
// `sql::view_buffered`（評価後射影形ビューの外側の WHERE・ORDER BY。Issue #1360）が
// 評価済みセルの比較・述語評価に同じ部品を再利用する（第 2 の比較器を作らない）。
pub(crate) mod values;

// 相関 `EXISTS` の半結合プローブ（Issue #1842）。`sql::correlated_exists` が組み立てる。
pub(crate) mod semi;

use std::collections::HashMap;

use crate::catalog::TableSchema;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedJoin};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::udf_call::UdfRegistry;

pub(crate) use plan::CteIdColumns;
use plan::{build_plan, JoinPlan, Shape};

pub(crate) use crate::join_limits::{
    MAX_JOIN_COMPARISONS, MAX_JOIN_INPUT_ROWS, MAX_JOIN_OUTPUT_ROWS,
};

/// テスト専用に上限を差し替えられるようにした構造体（`sql::scan::execute_scan`／
/// `sql::set_op::execute_with_budget` と同じ設計判断）。
pub(crate) struct JoinLimits {
    pub(crate) max_input_rows: usize,
    pub(crate) max_output_rows: usize,
    /// 非等価 ON のネステッドループ結合の比較回数上限（Issue #1837。既定は
    /// `MAX_JOIN_COMPARISONS`。文全体で共有し、超過は `54000`）。
    pub(crate) max_comparisons: usize,
    pub(crate) budget_cap: usize,
    /// 集計形のグループ数上限（既定は `sql::group_by::MAX_GROUPS`。超過は `54000`）。
    pub(crate) max_groups: usize,
    /// 集計形の `GROUP BY` キー列数上限（Issue #1550。既定は
    /// `sql::allowlist::MAX_GROUP_BY_COLUMNS`。超過は実行段で `54000`）。
    pub(crate) max_group_by_columns: usize,
    /// JOIN のリレーション数上限（Issue #1551。既定は `sql::relation::MAX_TABLE_REFS`。
    /// 超過は実行段で `54000`）。
    pub(crate) max_relations: usize,
    /// 文の `LIMIT` の範囲上限（Issue #1555。既定は従来の 10,000。超過は `22000`）。
    pub(crate) max_result_rows: usize,
}

impl Default for JoinLimits {
    fn default() -> Self {
        Self {
            max_input_rows: MAX_JOIN_INPUT_ROWS,
            max_output_rows: MAX_JOIN_OUTPUT_ROWS,
            max_comparisons: MAX_JOIN_COMPARISONS,
            budget_cap: crate::arena::MAX_ARENA_TOTAL_BYTES,
            max_groups: crate::sql::group_by::MAX_GROUPS,
            max_group_by_columns: crate::sql::allowlist::MAX_GROUP_BY_COLUMNS,
            max_relations: crate::sql::relation::MAX_TABLE_REFS,
            max_result_rows: crate::result_limits::DEFAULT_MAX_RESULT_ROWS,
        }
    }
}

/// 文全体（全 relation の走査・ハッシュ表・タプル・出力行の生成）で共有する累計
/// バイト予算（`sql::set_op::SetOpBudget` と同じ設計。モジュールドキュメント参照）。
pub(crate) struct JoinBudget {
    used: usize,
    cap: usize,
    /// NLJ の比較回数の文全体の累計（Issue #1837・D5）。バイト予算とは別軸。
    comparisons_used: usize,
}

impl JoinBudget {
    fn new(cap: usize) -> Self {
        Self {
            used: 0,
            cap,
            comparisons_used: 0,
        }
    }

    /// 比較回数を `n` 件計上する。オーバーフローも `cap` 超過も `54000`（部分結果は返さない）。
    pub(super) fn charge_comparisons(
        &mut self,
        n: usize,
        cap: usize,
    ) -> Result<(), SqlSurfaceError> {
        match self.comparisons_used.checked_add(n) {
            Some(t) if t <= cap => {
                self.comparisons_used = t;
                Ok(())
            }
            _ => Err(SqlSurfaceError::payload_too_large(
                "JOIN comparison count exceeds the limit",
            )),
        }
    }

    /// 比較回数の残り（D6 の事前判定用）。
    pub(super) fn remaining_comparisons(&self, cap: usize) -> usize {
        cap.saturating_sub(self.comparisons_used)
    }

    /// 起動時設定の既定上限で文全体の予算を作る。CTE を入力に持つ JOIN では、`core.rs` が CTE 本文の
    /// 評価・共有キャッシュ・参照ごとの複製を **確保の前に** この 1 つの予算へ計上し、続く JOIN の
    /// 走査・結合・出力生成も同じ予算で続ける（Issue #1861。独立した予算を複数持たない）。
    pub(crate) fn for_statement() -> Self {
        Self::new(JoinLimits::default().budget_cap)
    }

    /// 上限を指定して作る（テスト専用。実行経路は [`Self::for_statement`] を使う）。
    #[cfg(test)]
    pub(crate) fn with_cap(cap: usize) -> Self {
        Self::new(cap)
    }

    /// 残りのバイト予算。行を生成する走査へ上限としてそのまま渡す（生成中に打ち切らせる）。
    pub(crate) fn remaining(&self) -> usize {
        self.cap.saturating_sub(self.used)
    }

    /// 評価済みの行集合 `result`（保持・複製する分）のバイト量を計上する。複製・保持の **前に**
    /// 借用のまま呼び、超過なら `54000`（複製・確保そのものを行わない。PR #1110 と同じ設計判断）。
    pub(crate) fn charge_result(
        &mut self,
        result: &crate::sql::exec::QueryResult,
    ) -> Result<(), SqlSurfaceError> {
        self.charge(result_bytes(result.columns.len(), &result.rows))
    }

    /// 行形の層（射影・絞り込み）を `parent` に適用して作る出力行の分を、適用の **前** に計上する。
    /// 同じ列の複数回選択で出力が親より大きくなりうるため、親の分の再計上ではなく射影の出現回数から
    /// 見積もる（[`crate::sql::view_buffered::OuterPlan::rows_output_bytes`]）。見積もれない計画は
    /// `Internal`（fail-closed）。
    pub(crate) fn charge_layer_output(
        &mut self,
        parent: &crate::sql::exec::QueryResult,
        plan: &crate::sql::view_buffered::OuterPlan,
    ) -> Result<(), SqlSurfaceError> {
        let bytes = plan
            .rows_output_bytes(parent)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "layer output size is not estimable".to_string(),
            })?;
        self.charge(bytes)
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> Result<(), SqlSurfaceError> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.cap {
            return Err(SqlSurfaceError::payload_too_large(
                "JOIN exceeds the shared result byte budget",
            ));
        }
        Ok(())
    }
}

/// 行集合の推定バイト量（`sql::set_op::result_bytes` と同じ考え方。JOIN と
/// 集合演算は目的が異なる独立モジュールのため、予算計上のためだけに
/// モジュール境界を跨いで private 関数を共有しない——両モジュールが将来
/// 独立に変更されても互いに壊れない設計判断）。
fn result_bytes(column_count: usize, rows: &[ResultRow]) -> usize {
    let cell_struct_bytes = column_count.saturating_mul(std::mem::size_of::<Cell>());
    let result_row_struct_bytes = std::mem::size_of::<ResultRow>();
    let per_row_struct_bytes = cell_struct_bytes.saturating_add(result_row_struct_bytes);
    let mut total = per_row_struct_bytes.saturating_mul(rows.len());
    for row in rows {
        for cell in &row.cells {
            total = total.saturating_add(cell_payload_bytes(cell));
        }
    }
    total
}

pub(crate) fn cell_payload_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Text(s) => s.len(),
        Cell::Bytes(b) => b.len(),
        Cell::Json(s) => s.len(),
        Cell::Numeric(d) => d.to_string().len(),
        Cell::Vector(v) => v.len().saturating_mul(std::mem::size_of::<f32>()),
        Cell::Array(arr) => arr.approx_heap_bytes(),
        Cell::Null
        | Cell::Integer(_)
        | Cell::Float(_)
        | Cell::Bool(_)
        | Cell::SignedInteger(_)
        | Cell::Date(_)
        | Cell::Timestamp(_)
        | Cell::Time(_)
        | Cell::TimestampTz(_)
        | Cell::Interval(_)
        | Cell::Uuid(_) => 0,
    }
}

/// [`crate::sql::allowlist::ValidatedJoin`] が参照するテーブル名（重複除去済み。
/// 自己結合では 1 個になる）。`core.rs::EngineCore` が単一の `read_txn` 上で
/// 全 relation のスキーマを解決するために使う（`read_txn_with_schemas`）。
///
/// CTE を出所とするリレーション（`cte_inputs` の `relation_slot` が `Some`。Issue #1861）は
/// **含めない**: CTE 名はカタログの表名ではなく、同名の実表を黙って読む経路を作らないため
/// （この結果を使わずに CTE 入力つきの JOIN を実行すると、スキーマ欠落で `Internal`＝fail-closed）。
/// CTE 本文が読む表の解決は `core.rs` が slot ごとに行う。
pub(crate) fn collect_join_tables(validated: &ValidatedJoin) -> Vec<String> {
    let mut out = Vec::with_capacity(validated.relations.len());
    for (i, r) in validated.relations.iter().enumerate() {
        if relation_is_cte(validated, i) {
            continue;
        }
        if !out.iter().any(|t| t == r.table()) {
            out.push(r.table().to_string());
        }
    }
    out
}

/// リレーション `i` が CTE 入力か（Issue #1861）。
pub(crate) fn relation_is_cte(validated: &ValidatedJoin, i: usize) -> bool {
    validated
        .cte_inputs
        .as_ref()
        .and_then(|c| c.relation_slot.get(i))
        .is_some_and(Option::is_some)
}

/// 側スキャンの束縛（WHERE リテラルの型検証）を行う。Execute・Describe が共有し、
/// 束縛エラーを `LIMIT`／`OFFSET` の範囲検証（`22000`）より先に返す順序契約
/// （docs/design/inner-join.md）を両者で揃える。
fn bind_side_scans(
    plan: &JoinPlan<'_>,
    udfs: &UdfRegistry,
    scan_limit: usize,
) -> Result<Vec<crate::sql::parser::BoundScan>, SqlSurfaceError> {
    let mut out = Vec::with_capacity(plan.scans.len());
    for (scan, schema) in plan.scans.iter().zip(plan.schemas.iter()) {
        // 側スキャンの `LIMIT` は直後に `scan_limit` へ差し替える内部値のため既定の範囲検証のまま
        // （文の `LIMIT` は `validate_limit_offset` が設定値で判定する）。
        let mut bound = crate::sql::parser::bind_scan_with_dummy_flags(
            scan,
            schema,
            udfs,
            &[],
            &crate::result_limits::ResultLimits::default(),
        )?;
        bound.limit = scan_limit;
        out.push(bound);
    }
    Ok(out)
}

/// `LIMIT`／`OFFSET` の生値を検証する。非集計形は `LIMIT` 必須（構文段が保証）、
/// 集計形は省略可（`None` は無制限）。
fn validate_limit_offset(
    validated: &ValidatedJoin,
    max_result_rows: usize,
) -> Result<(Option<usize>, usize), SqlSurfaceError> {
    let result_limits = crate::result_limits::ResultLimits::new(max_result_rows);
    let limit = validated
        .limit
        .map(|raw| crate::sql::parser::validate_search_limit_with(raw, &result_limits))
        .transpose()?;
    let offset = crate::sql::parser::validate_search_offset(validated.offset)?;
    Ok((limit, offset))
}

/// [`ValidatedJoin`] を実行する（`core.rs::EngineCore::execute_validated_in_session`
/// の `Statement::Join` アーム・session-less `execute_sql` の同アームから
/// 呼ばれる）。`schemas` は呼び出し元が単一の `read_txn`（同一スナップショット）
/// 上で解決済みのものを渡す（`EngineCore::read_txn_with_schemas`）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
    // Issue #1550: 集計形の `GROUP BY` 列数・グループ数の起動時設定（`EngineCore` 保持）。
    group_limits: crate::group_by_limits::GroupByLimits,
    // Issue #1551: リレーション数・入出力行数の起動時設定（`EngineCore` 保持）。
    join_limits: crate::join_limits::JoinSizeLimits,
    // Issue #1555: 文の `LIMIT` の範囲上限（`EngineCore` 保持）。
    result_limits: crate::result_limits::ResultLimits,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_with_limits(
        read_txn,
        ctx,
        schemas,
        validated,
        udfs,
        &startup_limits(group_limits, join_limits, result_limits),
    )
}

/// 起動時設定から実行段の上限を組み立てる（[`execute`]・[`execute_with_cte_inputs`] が共有する）。
fn startup_limits(
    group_limits: crate::group_by_limits::GroupByLimits,
    join_limits: crate::join_limits::JoinSizeLimits,
    result_limits: crate::result_limits::ResultLimits,
) -> JoinLimits {
    JoinLimits {
        max_groups: group_limits.max_groups(),
        max_group_by_columns: group_limits.max_group_by_columns(),
        max_input_rows: join_limits.max_input_rows(),
        max_output_rows: join_limits.max_output_rows(),
        max_relations: join_limits.max_relations(),
        max_result_rows: result_limits.max_result_rows(),
        ..JoinLimits::default()
    }
}

/// CTE を入力に持つ JOIN の実行（Issue #1861・SQL-62）。`inputs` はリレーションと同じ並びで、
/// CTE のリレーションには `core.rs` が評価済みの CTE の行（列名リスト適用後）を渡す。`schemas` は
/// 実表に加えて CTE の結果列から合成したスキーマ（CTE 名で引く）を含む。CTE 本文は呼び出し側が
/// 参照者自身の `PolicyContext`・同一の `read_txn` で評価済みで、本関数は評価済みの行に側スキャンの
/// 射影・述語を適用するだけで記憶域を読まない。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_with_cte_inputs(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    join_limits: crate::join_limits::JoinSizeLimits,
    result_limits: crate::result_limits::ResultLimits,
    cte_ids: &CteIdColumns,
    inputs: Vec<Option<QueryResult>>,
    budget: JoinBudget,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_with_inputs(
        read_txn,
        ctx,
        schemas,
        validated,
        udfs,
        &startup_limits(group_limits, join_limits, result_limits),
        cte_ids,
        inputs,
        budget,
    )
}

/// [`execute`] の本体。`limits` はテスト専用に上限を差し替えられるようにした
/// もの（[`JoinLimits`]。`sql::scan::execute_scan_with_budget` と同じ設計）。
pub(crate) fn execute_with_limits(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
    limits: &JoinLimits,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_with_inputs(
        read_txn,
        ctx,
        schemas,
        validated,
        udfs,
        limits,
        &CteIdColumns::new(),
        Vec::new(),
        JoinBudget::new(limits.budget_cap),
    )
}

/// [`execute_with_limits`] の本体。`inputs[i]` が `Some` のリレーションは記憶域を走査せず、
/// 評価済みの CTE の行へ側スキャン（プッシュダウン済みの述語・側投影）を適用して側入力にする
/// （Issue #1861）。`None`・範囲外は従来どおり `execute_scan_with_budget` で走査する。
#[allow(clippy::too_many_arguments)]
fn execute_with_inputs(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
    limits: &JoinLimits,
    cte_ids: &CteIdColumns,
    mut inputs: Vec<Option<QueryResult>>,
    mut budget: JoinBudget,
) -> Result<QueryResult, SqlSurfaceError> {
    let plan = build_plan(schemas, cte_ids, validated, udfs)?;
    // §2.4/§2.5: 全 relation の束縛（列参照・WHERE 述語の型検証）を完了させてから
    // `LIMIT`／`OFFSET` の範囲検証（`22000`）を行う（束縛エラー優先。Describe と
    // 同じ判断順序）。
    let bound = bind_side_scans(&plan, udfs, limits.max_input_rows.saturating_add(1))?;
    let (limit, offset) = validate_limit_offset(validated, limits.max_result_rows)?;
    // Issue #1551: 起動時設定のリレーション数上限（データ非依存。走査より前に判定する）。
    if validated.relations.len() > limits.max_relations {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "too many tables in JOIN: {} exceeds limit {}",
            validated.relations.len(),
            limits.max_relations
        )));
    }
    // Issue #1550: 起動時設定の `GROUP BY` 列数上限。データに依存しない判定なので走査より前に行う。
    if let Shape::Aggregate(agg) = &plan.shape {
        if agg.keys.len() > limits.max_group_by_columns {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "GROUP BY column count {} exceeds limit {}",
                agg.keys.len(),
                limits.max_group_by_columns
            )));
        }
    }

    // §2.4: 各 relation の評価は既存の広域取得経路（`execute_scan_with_budget`）を
    // 通す——RLS 暗黙適用・fail-closed のエラー写像を第 2 の実行器なしに継承する。
    // 呼び出したセッションの `ctx`（`PolicyContext`）で全 relation を独立に評価する
    // ため、他テナント行は中間結果・結合キー・カーディナリティ判定のいずれにも
    // 現れない（RLS-10 相当）。
    let mut sides: Vec<Vec<ResultRow>> = Vec::with_capacity(plan.scans.len());
    for (i, (schema, bound_scan)) in plan.schemas.iter().zip(bound.iter()).enumerate() {
        // CTE 入力の側スキャンは、出力行の見積もり（射影の重複込みの上界）を適用の **前** に
        // 計上する（`apply_cte_side_scan`）ため、適用後の再計上は不要。
        let mut charged_before_apply = false;
        let result = match inputs.get_mut(i).and_then(Option::take) {
            // CTE の評価済みの行（Issue #1861）。側スキャンは `view_buffered` の外側計画と同じ
            // 束縛・評価の規則で適用する（第 2 の評価器を作らない）。入力は所有権ごと渡して消費し、
            // 複製を作らない（保持分は評価時に `budget` へ計上済み）。
            Some(body) => {
                charged_before_apply = true;
                apply_cte_side_scan(
                    body,
                    &mut budget,
                    plan.scans.get(i).ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN side scan missing".to_string(),
                    })?,
                    udfs,
                )?
            }
            None => crate::sql::scan::execute_scan_with_budget(
                read_txn,
                ctx,
                schema,
                bound_scan,
                budget.remaining(),
            )?,
        };
        if result.rows.len() > limits.max_input_rows {
            return Err(SqlSurfaceError::payload_too_large(
                "JOIN input exceeds the visible row limit",
            ));
        }
        if !charged_before_apply {
            budget.charge_result(&result)?;
        }
        sides.push(result.rows);
    }

    // Issue #1797・SQL-34: 非集計形・`LIMIT` 省略・残余述語なしのときに限り、最終段の一致ペアが
    // `offset + cap` を超えた時点で結合を中断できる（残余述語・集計で件数が減りうる形は
    // 完了後判定のまま）。
    let final_stop = match (&plan.shape, limit, plan.residual.is_empty()) {
        (Shape::Plain { .. }, None, true) => offset.checked_add(limits.max_result_rows),
        _ => None,
    };
    let (tuples, order) = exec::run_joins(&sides, &plan.steps, &mut budget, limits, final_stop)?;
    let order = residual::filter_tuples(order, &tuples, &plan.residual, &sides, &mut budget)?;

    match &plan.shape {
        Shape::Aggregate(agg) => {
            let result = aggregate::run_aggregate(
                agg,
                &sides,
                &tuples,
                &order,
                (limit, offset),
                limits.max_groups,
                &mut budget,
            )?;
            // Issue #1796・SQL-34: `LIMIT` 省略・`LIMIT ALL` は OFFSET 適用後の件数が結果行数上限を
            // 超えたら `54000`（黙って切り詰めない）。
            if limit.is_none() && result.rows.len() > limits.max_result_rows {
                return Err(crate::sql::scan::row_limit_exceeded());
            }
            Ok(result)
        }
        Shape::Plain {
            output,
            order: keys,
        } => {
            // Issue #1797: 件数判定（`LIMIT` 省略）は並べ替え・行組み立ての前に行う。
            if limit.is_none() && order.len().saturating_sub(offset) > limits.max_result_rows {
                return Err(crate::sql::scan::row_limit_exceeded());
            }
            let order = residual::sort_by_keys(order, &tuples, keys, &sides, &mut budget)?;
            // Issue #1796・SQL-34: `LIMIT` 省略・`LIMIT ALL`（`None`）は OFFSET 以降の全件を対象にし、
            // 行を組み立てる前に件数が結果行数上限を超えるかを判定する（超過は `54000`）。
            let start = offset.min(order.len());
            let end = match limit {
                Some(n) => start.saturating_add(n).min(order.len()),
                // 件数超過は上のソート前判定で拒否済み。
                None => order.len(),
            };
            let sliced = order
                .get(start..end)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN output window out of range".to_string(),
                })?;

            // 出力行 1 行あたりの構造体オーバーヘッド（`result_bytes` の
            // per_row_struct_bytes と同じ計算式）。行ごとの見積もりに使う。
            let per_row_struct_bytes = output
                .len()
                .saturating_mul(std::mem::size_of::<Cell>())
                .saturating_add(std::mem::size_of::<ResultRow>());

            let mut rows = Vec::with_capacity(sliced.len());
            for &ti in sliced {
                let tuple = tuples
                    .get(ti as usize)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN tuple index out of range".to_string(),
                    })?;
                // codex-review 指摘（PR #1110）: 大きなセルを持つ入力行が多数の出力行に
                // 一致すると、`.cloned()` で全行を複製してから予算照合すると `LIMIT`
                // の範囲内でも同じペイロードを大量に複製し、共有バイト予算を超える
                // メモリを確保した後にしか `54000` を返せない（security.md「不安全な
                // 設計｜無制限リソース確保（DoS）」に抵触）。複製前に借用のまま
                // この 1 行の見積もりを算出し、残予算と照合してから複製する
                // （超過時は複製・確保そのものを行わない fail-closed）。NULL 補完側
                // （タプルの該当 relation が `None`）は 0 バイトとして扱う
                // （Issue #926 §2.4）。
                let mut row_payload_bytes = 0usize;
                let mut picked: Vec<Option<&Cell>> = Vec::with_capacity(output.len());
                for out_col in output {
                    // FULL JOIN USING の統合列は代替ソースから `COALESCE` 相当で値を採る
                    // （借用のまま選ぶため、複製前の見積もり計上の順序は変わらない）。
                    let cell = residual::cell_with_fallbacks(
                        &sides,
                        tuple,
                        out_col.rel,
                        out_col.pos,
                        &out_col.fallbacks,
                    )?;
                    if let Some(c) = cell {
                        row_payload_bytes = row_payload_bytes.saturating_add(cell_payload_bytes(c));
                    }
                    picked.push(cell);
                }
                budget.charge(per_row_struct_bytes.saturating_add(row_payload_bytes))?;

                let cells: Vec<Cell> = picked
                    .into_iter()
                    .map(|c| c.cloned().unwrap_or(Cell::Null))
                    .collect();
                // `ResultRow.id`: 最初に存在する relation の `id`（左行があれば左、
                // 無ければ次の relation。`docs/design/outer-join.md`。wire・HTTP の
                // クエリ出力では `row.id` を使っていないため観測上の影響は無い）。
                let mut id = None;
                for rel in 0..tuple.len() {
                    if let Some(row) = residual::row_at(&sides, tuple, rel)? {
                        id = Some(row.id);
                        break;
                    }
                }
                let id = id.ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN output row has no relation row".to_string(),
                })?;
                rows.push(ResultRow {
                    id,
                    score: 0.0,
                    cells,
                });
            }

            Ok(QueryResult {
                columns: output.iter().map(|c| c.meta.clone()).collect(),
                rows,
            })
        }
    }
}

/// CTE の結果列（列名リスト適用後）から JOIN 用のスキーマを合成する（Issue #1861）。Execute と
/// Describe が同じ関数で作る（列メタの食い違いを作らない）。行識別方式は採番方式
/// （`RowIdentity::Allocated`）で、暗黙の疑似列 `id` を持たない: `*` は CTE の公開列だけを展開し、
/// `c.id` は CTE 本文が公開した `id` 列にだけ解決される（本文が公開しない行 id を述語や
/// 結合キーで読めない）。同名の列が 2 つ以上ある結果・型を持たない列は、どれを指すか決められず
/// 型も比較できないため固定文言の `0A000` で拒否する（入力の断片を含めない。fail-closed）。
/// 行 `id` を運ぶ結果列（`Id`・`AliasedId`）は、結合キー・比較を整数クラスで行うため NULL 可能な `BIGINT`
/// 列として載せるが、セルは `Cell::Integer(u64)` のままで（`i64` へ変換しない）、その列の添字を第 2 戻り値で
/// 返す。JOIN の束縛器はこの添字の列を `BIGINT` ではなく `id` 型（`ColumnMeta::Id`／`AliasedId`）で公告し、
/// 側述語は `view_buffered::plan_rows_layer` が符号なしのまま評価する（`i64::MAX` 超の行 id を壊さない）。
pub(crate) fn cte_schema(
    name: &str,
    columns: &[ColumnMeta],
) -> Result<(TableSchema, Vec<usize>), SqlSurfaceError> {
    let mut defs: Vec<crate::catalog::ColumnDef> = Vec::with_capacity(columns.len());
    let mut id_columns: Vec<usize> = Vec::new();
    for (pos, meta) in columns.iter().enumerate() {
        if matches!(meta, ColumnMeta::Id | ColumnMeta::AliasedId { .. }) {
            id_columns.push(pos);
        }
        let (col_name, ty) = match meta {
            ColumnMeta::Id => ("id", crate::catalog::ColumnType::BigInt),
            // 別名付きの疑似列 `id`（`WITH c(i) AS (SELECT id ...)`）。セルは `Cell::Integer(u64)` の
            // ままなので、JOIN の比較クラスが整数になる `BIGINT` で載せる（`NUMERIC` にすると
            // 整数セルとクラスが食い違う）。
            ColumnMeta::AliasedId { name } => (name.as_str(), crate::catalog::ColumnType::BigInt),
            ColumnMeta::Scalar { name, ty } => (name.as_str(), ty.clone()),
            ColumnMeta::Computed { name, ty: Some(ty) } => (name.as_str(), ty.clone()),
            ColumnMeta::Computed { ty: None, .. } => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "a CTE column without a static type cannot be a JOIN input",
                ));
            }
        };
        if defs.iter().any(|d| d.name.eq_ignore_ascii_case(col_name)) {
            return Err(SqlSurfaceError::feature_not_supported(
                "a CTE with duplicate column names cannot be a JOIN input",
            ));
        }
        defs.push(crate::catalog::ColumnDef::new(col_name, ty, true));
    }
    Ok((
        TableSchema::new(name.to_string(), defs)
            .with_row_identity(crate::catalog::RowIdentity::Allocated),
        id_columns,
    ))
}

/// 評価済みの CTE の行 `body` に、側スキャン `scan`（射影＋プッシュダウン済み述語）を適用する
/// （Issue #1861）。評価済みの行は文のローカルな所有値で、他の文・他セッションとは共有しない。
/// 述語・射影の束縛は `view_buffered::plan_rows_layer`（評価後射影形ビューの外側と同じ規則）で、
/// 本文が公開しない列は `22000` で拒否される。出力行の保持量は `budget` へ適用前に計上する。
fn apply_cte_side_scan(
    body: QueryResult,
    budget: &mut JoinBudget,
    scan: &crate::sql::allowlist::ValidatedScan,
    udfs: &UdfRegistry,
) -> Result<QueryResult, SqlSurfaceError> {
    let layer = crate::sql::allowlist::BufferedRows {
        projection: scan.projection.clone(),
        where_predicates: scan.where_predicates.clone(),
        order_by: Vec::new(),
        order_keys: Vec::new(),
        window_items: Vec::new(),
        limit: u32::MAX,
        limit_explicit: false,
        limit_implicit: false,
        offset: 0,
    };
    let plan =
        crate::sql::view_buffered::plan_rows_layer(&body.columns, scan.table_name(), &layer, udfs)?;
    // 出力行（射影の複製を含む）の分を、適用の前に共有予算へ計上する（唯一の見積もり）。
    budget.charge_layer_output(&body, &plan)?;
    crate::sql::view_buffered::apply_rows_layer(body, &plan)
}

/// Describe（拡張クエリプロトコル）向けに、走査を一切行わず結果列メタデータだけを
/// 導出する（`sql::set_op::describe_columns` と同じ方針）。JOIN は `$n` パラメータを
/// 受理しない（`sql::params::validate_param_positions` が構文段で拒否する）ため、
/// ダミー等価フラグは常に空でよい。
pub(crate) fn describe_columns(
    schemas: &HashMap<String, TableSchema>,
    cte_ids: &CteIdColumns,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
    result_limits: &crate::result_limits::ResultLimits,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    let plan = build_plan(schemas, cte_ids, validated, udfs)?;
    // 走査せずに束縛だけ行い、WHERE リテラルの型検証を Execute と同じ判定
    // 基準で確定させる（Describe は本体を実行しない契約）。束縛完了を
    // `LIMIT`／`OFFSET` 検証より前に行う（束縛エラー優先。Execute と同じ順序）。
    bind_side_scans(&plan, udfs, 1)?;
    validate_limit_offset(validated, result_limits.max_result_rows())?;
    Ok(plan.column_metas())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnType;
    use crate::catalog::{ColumnDef, TableSchema};
    use crate::recovery::required_op_id::OperationId;
    use crate::row_codec::Value;
    use crate::sql::allowlist::{
        JoinCondition, JoinKind, JoinProjection, JoinStep, JoinWhereExpr, JoinWherePredicate,
        Projection, ValidatedJoin,
    };
    use crate::sql::relation::{ColumnRef, TableRef};
    use plan::SideProjection;

    fn with_kind(mut v: ValidatedJoin, kind: JoinKind) -> ValidatedJoin {
        for s in &mut v.steps {
            s.kind = kind;
        }
        v
    }

    /// 2 テーブル（1 段）の JOIN 文を組み立てる。
    fn one_step(
        relations: Vec<TableRef>,
        kind: JoinKind,
        on: Vec<(ColumnRef, ColumnRef)>,
        projection: JoinProjection,
        where_clause: Option<JoinWhereExpr>,
        limit: u32,
    ) -> ValidatedJoin {
        ValidatedJoin {
            relations,
            steps: vec![JoinStep {
                kind,
                condition: JoinCondition::On(JoinWhereExpr::And(
                    on.into_iter()
                        .map(|(lhs, rhs)| {
                            JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare {
                                lhs,
                                op: crate::sql::udf_call::BinOp::Eq,
                                rhs,
                            })
                        })
                        .collect(),
                )),
            }],
            projection,
            column_aliases: Vec::new(),
            where_clause: where_clause.map(Box::new),
            order_by: Vec::new(),
            aggregate: None,
            limit: Some(limit),
            offset: 0,
            cte_inputs: None,
        }
    }
    use crate::sql::udf_call::UdfRegistry;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("tag", ColumnType::Text, false),
            ],
        )
    }

    fn insert_row(storage: &Storage, table: &str, ctx: &PolicyContext, id: u64, tag: &str) {
        let op_id = OperationId::parse(&format!("seed-{table}-{id}")).expect("valid operation_id");
        crate::tenant::insert_typed_row(
            storage,
            table,
            ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Text(tag.to_string())],
            &op_id,
        )
        .expect("insert row");
    }

    /// `id = id` の結合。両テーブルとも `id` を疑似列のまま使う最小構成
    /// （`build_plan` の整数クラス互換〔疑似列同士〕を経由する）。
    fn id_join(left: &str, right: &str) -> ValidatedJoin {
        one_step(
            vec![TableRef::new(left), TableRef::new(right)],
            JoinKind::Inner,
            vec![(
                ColumnRef::qualified(left, "id"),
                ColumnRef::qualified(right, "id"),
            )],
            JoinProjection::Columns(vec![ColumnRef::qualified(left, "tag")]),
            None,
            10,
        )
    }

    /// Issue #925 §2.4 の回帰: 側の可視行数が注入した上限を超えたら `54000`。
    #[test]
    fn input_row_overflow_is_rejected_with_injected_limits() {
        let path = unique_db_path("join-input-row-overflow");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for i in 0..5u64 {
            insert_row(&storage, "l", &ctx, i, "x");
        }
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_input_rows: 2,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("left side row count must exceed the injected input row limit");
        assert_eq!(err.wire_code(), "54000");
    }

    /// Issue #925 §2.4 の回帰: `LIMIT`／`OFFSET` の値によらず、結合カーディナリ
    /// ティ（`LIMIT` 適用前の一致ペア数）が注入した上限を超えたら `54000`。
    #[test]
    fn output_cardinality_overflow_is_rejected_regardless_of_limit() {
        let path = unique_db_path("join-output-cardinality-overflow");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 同一キー（id=0）を複数持たせて直積を発生させる代わりに、`ON` を
        // 常に真になる比較にはできないため、複数行が同じ疑似列 `id` を持つ
        // ことはできない。代わりに複数の一致候補を作るため、右側に複数行を
        // 挿入して結合キー列（`tag` を通さず `id` 疑似列）を直接一致させる
        // ことはできないので、右側は 1 行のみとし、`max_output_rows` を 0 に
        // 注入して「1 件の一致でも上限超過」を確認する。
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = ValidatedJoin {
            limit: Some(1),
            ..id_join("l", "r")
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_output_rows: 0,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("a single matching pair must exceed a zero output row limit");
        assert_eq!(err.wire_code(), "54000");
    }

    /// Issue #1830 の回帰: ON 残余（混在述語）で全候補が落ちて一致ペアが 0 件でも、キー一致の
    /// 候補数が出力上限を超えたら `54000`（残余が上限検査を素通りしない）。
    #[test]
    fn on_residual_dropping_all_candidates_still_hits_output_row_limit() {
        let path = unique_db_path("join-on-residual-candidate-cap");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 全行が同じ tag（結合キー）で 3x3=9 候補。l.id(10..) < r.id(0..) は常に偽。
        for i in 0..3u64 {
            insert_row(&storage, "l", &ctx, 10 + i, "k");
            insert_row(&storage, "r", &ctx, i, "k");
        }
        let eq = |a: &str, b: &str, op| JoinWherePredicate::ColumnCompare {
            lhs: ColumnRef::qualified(a, "tag"),
            op,
            rhs: ColumnRef::qualified(b, "tag"),
        };
        let mut validated = id_join("l", "r");
        let mixed = JoinWherePredicate::ColumnCompare {
            lhs: ColumnRef::qualified("l", "id"),
            op: crate::sql::udf_call::BinOp::Lt,
            rhs: ColumnRef::qualified("r", "id"),
        };
        if let Some(step) = validated.steps.first_mut() {
            step.condition = JoinCondition::On(JoinWhereExpr::And(vec![
                JoinWhereExpr::Leaf(eq("l", "r", crate::sql::udf_call::BinOp::Eq)),
                JoinWhereExpr::Leaf(mixed),
            ]));
        }
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();

        let ok = execute_with_limits(
            &read_txn,
            &ctx,
            &schemas,
            &validated,
            &udfs,
            &JoinLimits::default(),
        )
        .expect("residual drops every candidate under the default limit");
        assert!(ok.rows.is_empty());

        let limits = JoinLimits {
            max_output_rows: 4,
            ..JoinLimits::default()
        };
        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("9 key-matched candidates must exceed the injected output row limit");
        assert_eq!(err.wire_code(), "54000");
    }

    /// PR #1105（`sql::set_op`）と同じ設計判断の回帰: 文全体（両辺の走査・
    /// ハッシュ表・出力行）で 1 つの累計バイト予算を共有すること。1 辺だけなら
    /// 収まるが両辺合計では超える上限を注入すると `54000` になることを確認する。
    #[test]
    fn shared_byte_budget_rejects_when_combined_scans_exceed_the_injected_cap() {
        let path = unique_db_path("join-shared-budget");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, &"x".repeat(4096));
        insert_row(&storage, "r", &ctx, 0, &"y".repeat(4096));

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            budget_cap: 4096 + 512,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("combined side bytes must exceed the injected shared budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// codex-review 指摘（PR #1110・join.rs:786）の回帰: 1 件の大きなセルを
    /// 持つビルド側の行が、プローブ側の複数行と一致して出力側で多数回
    /// 複製される（fan-out）場合でも、走査段（両辺のスキャン・ハッシュキー）
    /// だけでは収まる予算を注入すると出力段の複製で `54000` に落ちること
    /// （出力行ごとの見積もりが複製前に残予算と照合されていること）を確認する。
    /// ビルド側（`tag` を結合キーに使うため走査段では小さい値のまま）1 行の
    /// 大きな `embedding`（VECTOR）だけがプローブ側の一致数だけ出力へ複製される
    /// ため、走査段の合計とは別に出力段だけが予算超過することを再現できる。
    #[test]
    fn output_row_fan_out_of_a_large_cell_is_rejected_before_exceeding_the_budget() {
        let path = unique_db_path("join-fan-out-large-cell");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        let big_schema = |name: &str| {
            TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(512), false),
                    ColumnDef::new("tag", ColumnType::Text, false),
                ],
            )
        };
        storage.create_table(&big_schema("l")).expect("create l");
        storage.create_table(&big_schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let op_id = OperationId::parse("seed-l-0").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &storage,
            "l",
            &ctx,
            0,
            Visibility::Public,
            &[Value::Vector(vec![1.0; 512]), Value::Text("k".to_string())],
            &op_id,
        )
        .expect("insert build-side row");
        // プローブ側は結合キー（`tag` = "k"）だけを共有する多数の小さな行。
        // 走査段の合計バイト量は小さいまま、出力段の一致ペア数（fan-out）だけが
        // 増える構図にする。
        const PROBE_ROWS: u64 = 32;
        for i in 0..PROBE_ROWS {
            let op_id = OperationId::parse(&format!("seed-r-{i}")).expect("valid operation_id");
            crate::tenant::insert_typed_row(
                &storage,
                "r",
                &ctx,
                i,
                Visibility::Public,
                &[Value::Vector(vec![0.0; 512]), Value::Text("k".to_string())],
                &op_id,
            )
            .expect("insert probe-side row");
        }

        let validated = one_step(
            vec![TableRef::new("l"), TableRef::new("r")],
            JoinKind::Inner,
            vec![(
                ColumnRef::qualified("l", "tag"),
                ColumnRef::qualified("r", "tag"),
            )],
            JoinProjection::Columns(vec![ColumnRef::qualified("l", "embedding")]),
            None,
            PROBE_ROWS as u32,
        );
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), big_schema("l"));
        schemas.insert("r".to_string(), big_schema("r"));
        let udfs = UdfRegistry::default();
        // 走査段（両辺のスキャン結果・結合キー）の合計は大きな embedding が
        // 1 個分（512 次元 * 4 byte = 2048 byte）＋プローブ側の小さな行群で
        // 収まるが、出力段は同じ embedding を `PROBE_ROWS` 回複製するため
        // 大きく超過する上限を注入する。
        let limits = JoinLimits {
            budget_cap: 8192,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("fan-out duplication of the large cell must exceed the injected budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 上のちょうど対照: 両辺合計が収まる上限を注入すれば成功することを確認する
    /// （回帰テストが常に失敗するだけの壊れた検証になっていないことの確認）。
    #[test]
    fn shared_byte_budget_succeeds_when_combined_scans_fit_the_injected_cap() {
        let path = unique_db_path("join-shared-budget-accept");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();

        let result = execute(
            &read_txn,
            &ctx,
            &schemas,
            &validated,
            &udfs,
            crate::group_by_limits::GroupByLimits::default(),
            crate::join_limits::JoinSizeLimits::default(),
            crate::result_limits::ResultLimits::default(),
        )
        .expect("small rows must fit the default budget");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn collect_join_tables_dedups_self_join() {
        let validated = one_step(
            vec![
                TableRef::with_alias("a", "x"),
                TableRef::with_alias("a", "y"),
            ],
            JoinKind::Inner,
            vec![(ColumnRef::unqualified("id"), ColumnRef::unqualified("id"))],
            JoinProjection::All,
            None,
            10,
        );
        assert_eq!(collect_join_tables(&validated), vec!["a".to_string()]);
    }

    /// `Projection` を直接構築しても [`SideProjection::to_projection`] が
    /// 同じ形へ写像すること（内部型の変換規則の回帰）。
    #[test]
    fn side_projection_to_projection_matches_variant() {
        let all = SideProjection::All;
        assert_eq!(all.to_projection(), Projection::All);
        let cols = SideProjection::Columns(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            cols.to_projection(),
            Projection::Columns(vec!["a".to_string(), "b".to_string()])
        );
    }

    /// advisor 指摘の回帰: スキーマが `id` という実カラムを持つ場合、
    /// `SideProjection::All::position` は疑似列（位置 0）ではなく実カラムの
    /// 位置（`1 + index`）を返さなければならない（`bind_projection`・
    /// `BindingScope::resolve_in_relation` と同じ優先規則）。
    #[test]
    fn side_projection_all_position_prefers_real_id_column_over_pseudo_column() {
        let schema = TableSchema::new(
            "t",
            vec![
                ColumnDef::new("id", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, false),
            ],
        );
        assert_eq!(SideProjection::All.position("id", &schema), Some(1));
        assert_eq!(SideProjection::All.position("tag", &schema), Some(2));
    }

    /// 実カラム `id` を持たないスキーマでは疑似列（位置 0）を返す
    /// （上のテストの対照。回帰が常に失敗するだけの壊れた検証になっていない
    /// ことの確認）。
    #[test]
    fn side_projection_all_position_falls_back_to_pseudo_id_when_no_real_column() {
        let schema = TableSchema::new("t", vec![ColumnDef::new("tag", ColumnType::Text, false)]);
        assert_eq!(SideProjection::All.position("id", &schema), Some(0));
    }

    /// advisor 指摘の統合回帰: 実カラム `id` を持つテーブルを `*` 投影で
    /// JOIN すると、結合キーの抽出・出力の両方が実カラムの値を使うこと
    /// （行キーではなく）。
    #[test]
    fn star_projection_join_key_uses_real_id_column_not_pseudo_row_key() {
        let path = unique_db_path("join-star-real-id-column");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        let left_schema = TableSchema::new(
            "l",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        );
        let right_schema = TableSchema::new(
            "r",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("xx", ColumnType::Text, false),
            ],
        );
        storage.create_table(&left_schema).expect("create l");
        storage.create_table(&right_schema).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 行キー（疑似列）は l=100・r=200（互いに一致しない）だが、実カラム
        // `id`／`xx` は "shared-key" で一致させる。誤って疑似列を結合キーに
        // 使うと 0 件、正しく実カラムを使うと 1 件になる。
        let op_id = OperationId::parse("seed-l-100").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &storage,
            "l",
            &ctx,
            100,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0, 0.0]),
                Value::Text("shared-key".to_string()),
            ],
            &op_id,
        )
        .expect("insert l row");
        let op_id = OperationId::parse("seed-r-200").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &storage,
            "r",
            &ctx,
            200,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0, 0.0]),
                Value::Text("shared-key".to_string()),
            ],
            &op_id,
        )
        .expect("insert r row");

        let validated = one_step(
            vec![TableRef::new("l"), TableRef::new("r")],
            JoinKind::Inner,
            vec![(
                ColumnRef::qualified("l", "id"),
                ColumnRef::qualified("r", "xx"),
            )],
            JoinProjection::All,
            None,
            10,
        );
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), left_schema);
        schemas.insert("r".to_string(), right_schema);
        let udfs = UdfRegistry::default();

        let result = execute(
            &read_txn,
            &ctx,
            &schemas,
            &validated,
            &udfs,
            crate::group_by_limits::GroupByLimits::default(),
            crate::join_limits::JoinSizeLimits::default(),
            crate::result_limits::ResultLimits::default(),
        )
        .expect("join on real id/xx columns should succeed");
        assert_eq!(
            result.rows.len(),
            1,
            "join must match on the real id/xx column values, not the pseudo row key"
        );
    }

    /// Issue #926 §2.3 の回帰: `is_null_rejecting` は現行の全 `JoinWherePredicate`
    /// variant で真を返す（WHERE 簡約規則の前提）。将来 variant が増えた際は
    /// 網羅的 `match` がコンパイルエラーで検出する（本テストの更新も必要）。
    #[test]
    fn is_null_rejecting_returns_true_for_all_current_variants() {
        use crate::sql::allowlist::CompareOp;
        let column = ColumnRef::unqualified("x");
        let preds = [
            JoinWherePredicate::Equality {
                column: column.clone(),
                value: "v".to_string(),
            },
            JoinWherePredicate::Prefix {
                column: column.clone(),
                pattern: "v".to_string(),
            },
            JoinWherePredicate::Compare {
                column: column.clone(),
                op: CompareOp::Gt,
                value: "v".to_string(),
            },
            JoinWherePredicate::BoolEquality {
                column: column.clone(),
                value: true,
            },
            JoinWherePredicate::BoolColumn {
                column: column.clone(),
            },
            JoinWherePredicate::InList {
                column: column.clone(),
                values: vec!["v".to_string()],
            },
            JoinWherePredicate::ColumnCompare {
                lhs: column.clone(),
                op: crate::sql::udf_call::BinOp::Lt,
                rhs: column,
            },
        ];
        for pred in &preds {
            assert!(plan::is_null_rejecting(pred), "pred={pred:?}");
        }
    }

    /// Issue #926 §2.4 の回帰: LEFT JOIN では未一致の左行が NULL 補完行として
    /// カーディナリティに加算されるため、一致ペア数だけなら収まる注入上限でも
    /// 全体では超過して `54000` になる。
    #[test]
    fn unmatched_left_row_pushes_cardinality_over_the_injected_limit_for_left_join() {
        let path = unique_db_path("outer-join-cardinality-overflow");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // id=0 は両側一致（1 ペア）。id=1 は左側のみ（NULL 補完で 1 行追加）。
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "l", &ctx, 1, "orphan");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = with_kind(id_join("l", "r"), JoinKind::Left);
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_output_rows: 1,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("1 matched pair + 1 NULL-padded row must exceed a limit of 1");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 上のちょうど対照: 同じデータを INNER JOIN で実行すると未一致の左行は
    /// 出力に含まれないため、一致ペア数（1 件）が注入上限（1 件）に収まって
    /// 成功する（回帰テストが常に失敗するだけの壊れた検証になっていない
    /// ことの確認）。
    #[test]
    fn same_data_fits_under_the_injected_limit_for_inner_join() {
        let path = unique_db_path("outer-join-cardinality-accept");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "l", &ctx, 1, "orphan");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_output_rows: 1,
            ..JoinLimits::default()
        };

        let result = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect("INNER JOIN cardinality (1 matched pair) must fit the limit of 1");
        assert_eq!(result.rows.len(), 1);
    }

    /// Issue #926 §2.3 の回帰: LEFT JOIN で欠損側（右）に WHERE 述語があると、
    /// NULL 補完行は必ずその述語で落ちるため簡約して INNER と同じ結果になる
    /// （`preserve_left` が偽になる）。
    #[test]
    fn left_join_with_predicate_on_missing_side_reduces_to_inner() {
        let path = unique_db_path("outer-join-where-reduction");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "l", &ctx, 1, "orphan");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = ValidatedJoin {
            where_clause: Some(Box::new(JoinWhereExpr::Leaf(
                JoinWherePredicate::Equality {
                    column: ColumnRef::qualified("r", "tag"),
                    value: "y".to_string(),
                },
            ))),
            ..with_kind(id_join("l", "r"), JoinKind::Left)
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();

        let result = execute(
            &read_txn,
            &ctx,
            &schemas,
            &validated,
            &udfs,
            crate::group_by_limits::GroupByLimits::default(),
            crate::join_limits::JoinSizeLimits::default(),
            crate::result_limits::ResultLimits::default(),
        )
        .expect("LEFT JOIN with a predicate on the missing side should succeed");
        assert_eq!(
            result.rows.len(),
            1,
            "the unmatched left row must not be NULL-padded when the right side has a WHERE predicate"
        );
    }
    /// 3 テーブルの ON 連鎖 `l.tag = r.tag AND r.tag = s.tag`（段 0・段 1 とも
    /// `tag` で等値結合する）。
    fn three_way(kind: JoinKind) -> ValidatedJoin {
        ValidatedJoin {
            relations: vec![TableRef::new("l"), TableRef::new("r"), TableRef::new("s")],
            steps: vec![
                JoinStep {
                    kind,
                    condition: JoinCondition::On(JoinWhereExpr::Leaf(
                        JoinWherePredicate::ColumnCompare {
                            lhs: ColumnRef::qualified("l", "tag"),
                            op: crate::sql::udf_call::BinOp::Eq,
                            rhs: ColumnRef::qualified("r", "tag"),
                        },
                    )),
                },
                JoinStep {
                    kind: JoinKind::Inner,
                    condition: JoinCondition::On(JoinWhereExpr::Leaf(
                        JoinWherePredicate::ColumnCompare {
                            lhs: ColumnRef::qualified("r", "tag"),
                            op: crate::sql::udf_call::BinOp::Eq,
                            rhs: ColumnRef::qualified("s", "tag"),
                        },
                    )),
                },
            ],
            projection: JoinProjection::Columns(vec![ColumnRef::qualified("l", "tag")]),
            column_aliases: Vec::new(),
            where_clause: None,
            order_by: Vec::new(),
            aggregate: None,
            limit: Some(10),
            offset: 0,
            cte_inputs: None,
        }
    }

    /// Issue #1190: 上限は結合の各段で判定する。段 0 のカーディナリティ（2×2=4 ペア）
    /// が注入した上限 3 を超えれば、最終結果（段 1 で 0 行）が小さくても `54000`。
    #[test]
    fn intermediate_step_cardinality_overflow_is_rejected_even_when_final_result_is_empty() {
        let path = unique_db_path("multi-way-step-cardinality");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        for t in ["l", "r", "s"] {
            storage.create_table(&schema(t)).expect("create table");
        }
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for i in 0..2u64 {
            insert_row(&storage, "l", &ctx, i, "k");
            insert_row(&storage, "r", &ctx, i, "k");
        }
        insert_row(&storage, "s", &ctx, 0, "other");

        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        for t in ["l", "r", "s"] {
            schemas.insert(t.to_string(), schema(t));
        }
        let udfs = UdfRegistry::default();
        let validated = three_way(JoinKind::Inner);

        let limits = JoinLimits {
            max_output_rows: 3,
            ..JoinLimits::default()
        };
        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("step 0 cardinality (4 pairs) must exceed the injected limit of 3");
        assert_eq!(err.wire_code(), "54000");

        // 対照: 上限 4 なら段 0 は収まり、最終結果は 0 行になる。
        let limits = JoinLimits {
            max_output_rows: 4,
            ..JoinLimits::default()
        };
        let result = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect("limit of 4 fits step 0");
        assert!(result.rows.is_empty());
    }

    /// 回帰: 2 relation のときの行順は `(左位置, 右位置)`（NULL 補完は「無し」が後）。
    /// N 方向化した決定的ソートが従来の並びと一致すること。
    #[test]
    fn two_relation_output_order_is_left_then_right_scan_position() {
        let path = unique_db_path("multi-way-two-relation-order");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 左 3 行（うち 1 行は右と一致しない）・右 3 行（うち 1 行は左と一致しない）。
        insert_row(&storage, "l", &ctx, 0, "a");
        insert_row(&storage, "l", &ctx, 1, "b");
        insert_row(&storage, "l", &ctx, 2, "z-left-only");
        insert_row(&storage, "r", &ctx, 0, "b");
        insert_row(&storage, "r", &ctx, 1, "a");
        insert_row(&storage, "r", &ctx, 2, "y-right-only");

        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let validated = with_kind(
            one_step(
                vec![TableRef::new("l"), TableRef::new("r")],
                JoinKind::Full,
                vec![(
                    ColumnRef::qualified("l", "tag"),
                    ColumnRef::qualified("r", "tag"),
                )],
                JoinProjection::Columns(vec![
                    ColumnRef::qualified("l", "tag"),
                    ColumnRef::qualified("r", "tag"),
                ]),
                None,
                10,
            ),
            JoinKind::Full,
        );
        let result = execute(
            &read_txn,
            &ctx,
            &schemas,
            &validated,
            &udfs,
            crate::group_by_limits::GroupByLimits::default(),
            crate::join_limits::JoinSizeLimits::default(),
            crate::result_limits::ResultLimits::default(),
        )
        .expect("full join");
        let rendered: Vec<String> = result
            .rows
            .iter()
            .map(|r| {
                r.cells
                    .iter()
                    .map(|c| match c {
                        Cell::Text(s) => s.clone(),
                        Cell::Null => "NULL".to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        assert_eq!(
            rendered,
            vec!["a|a", "b|b", "z-left-only|NULL", "NULL|y-right-only"]
        );
    }

    /// Issue #1190: 集計形のグループ数は注入した上限を超えたら `54000`
    /// （新しいグループの確保前に判定する）。
    #[test]
    fn group_count_overflow_is_rejected_with_injected_limits() {
        use crate::sql::allowlist::{AggregateFunc, JoinAggregate, JoinSelectItem};
        let path = unique_db_path("multi-way-group-limit");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for i in 0..4u64 {
            insert_row(&storage, "l", &ctx, i, &format!("g{i}"));
            insert_row(&storage, "r", &ctx, i, &format!("g{i}"));
        }
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let mut validated = one_step(
            vec![TableRef::new("l"), TableRef::new("r")],
            JoinKind::Inner,
            vec![(
                ColumnRef::qualified("l", "tag"),
                ColumnRef::qualified("r", "tag"),
            )],
            JoinProjection::Columns(Vec::new()),
            None,
            10,
        );
        validated.limit = None;
        validated.aggregate = Some(Box::new(JoinAggregate {
            items: vec![
                JoinSelectItem::Key {
                    column: ColumnRef::qualified("l", "tag"),
                    alias: None,
                },
                JoinSelectItem::Aggregate {
                    func: AggregateFunc::Count,
                    arg: None,
                    alias: None,
                },
            ],
            group_by: vec![ColumnRef::qualified("l", "tag")],
            having: Vec::new(),
        }));

        let limits = JoinLimits {
            max_groups: 3,
            ..JoinLimits::default()
        };
        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("4 groups must exceed the injected limit of 3");
        assert_eq!(err.wire_code(), "54000");

        let limits = JoinLimits {
            max_groups: 4,
            ..JoinLimits::default()
        };
        let result = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect("4 groups fit a limit of 4");
        assert_eq!(result.rows.len(), 4);
    }

    /// `a.id <op> b.id` の葉。
    fn id_cmp(a: &str, op: crate::sql::udf_call::BinOp, b: &str) -> JoinWhereExpr {
        JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare {
            lhs: ColumnRef::qualified(a, "id"),
            op,
            rhs: ColumnRef::qualified(b, "id"),
        })
    }

    /// Issue #1839: ON の `OR`／非等価（キー無し）段は NLJ で実行され、比較回数の上限は
    /// 文全体で共有する（超過は部分結果を返さず `54000`）。実 SQL からの到達経路と同じ
    /// 束縛結果（`JoinCondition::On` の論理木）を直接組み立てて確かめる。
    #[test]
    fn keyless_on_comparison_budget_is_enforced_per_statement() {
        use crate::sql::udf_call::BinOp;
        let path = unique_db_path("join-keyless-on-comparison-budget");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        for t in ["l", "r", "s"] {
            storage.create_table(&schema(t)).expect("create table");
        }
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for i in 0..3u64 {
            for t in ["l", "r", "s"] {
                insert_row(&storage, t, &ctx, i, "k");
            }
        }
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        for t in ["l", "r", "s"] {
            schemas.insert(t.to_string(), schema(t));
        }
        let udfs = UdfRegistry::default();
        let run = |v: &ValidatedJoin, max: usize| {
            let limits = JoinLimits {
                max_comparisons: max,
                ..JoinLimits::default()
            };
            execute_with_limits(&read_txn, &ctx, &schemas, v, &udfs, &limits)
        };

        // (a) 単段の非等価 ON: 3 x 3 = 9 比較。上限 8 は 54000、上限 9 は成功（l.id < r.id は 3 組）。
        let mut single = three_way(JoinKind::Inner);
        single.relations.truncate(2);
        single.steps.truncate(1);
        single.steps[0].condition = JoinCondition::On(id_cmp("l", BinOp::Lt, "r"));
        assert_eq!(
            run(&single, 8)
                .expect_err("9 comparisons exceed 8")
                .wire_code(),
            "54000"
        );
        assert_eq!(run(&single, 9).expect("fits 9").rows.len(), 3);

        // (b) OR を含む ON も同じ予算で数える。
        single.steps[0].condition = JoinCondition::On(JoinWhereExpr::Or(vec![
            id_cmp("l", BinOp::Lt, "r"),
            id_cmp("l", BinOp::Gt, "r"),
        ]));
        assert_eq!(
            run(&single, 8)
                .expect_err("OR on 9 pairs exceeds 8")
                .wire_code(),
            "54000"
        );
        assert_eq!(run(&single, 9).expect("fits 9").rows.len(), 6);

        // (c) 3 表で NLJ 段が 2 つ: 段 0 は 9 比較・段 1 は 3 x 3 = 9 比較。各段は 12 以内でも合計 18 が
        // 12 を超えるため、文全体で共有する予算により 54000。
        let mut chain = three_way(JoinKind::Inner);
        chain.steps[0].condition = JoinCondition::On(id_cmp("l", BinOp::Lt, "r"));
        chain.steps[1].condition = JoinCondition::On(id_cmp("r", BinOp::Lt, "s"));
        assert_eq!(
            run(&chain, 12)
                .expect_err("sum of both steps exceeds 12")
                .wire_code(),
            "54000"
        );
        assert!(run(&chain, 100).is_ok());
    }
}
