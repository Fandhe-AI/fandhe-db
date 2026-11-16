//! `UPDATE ... FROM`／`DELETE ... USING` の結合実行（Issue #1755、SQL-53 (b)・RLS-10・RECOVER-11。
//! 親: #1753）。
//!
//! 責務境界: 構文・束縛（FROM／USING の別名・修飾列の解決。Issue #1754）は別 Issue の担当で、
//! 本モジュールは「束縛済みの実行入力（[`JoinedDmlPlan`]）を受け取り、書き込みトランザクション内で
//! 対象表の各行に対し FROM／USING 側の一致行を決める」ことだけを担う。書き込み自体は既存の
//! 述語形 DML 経路（`tenant::update_rows_joined_unchecked`／`delete_rows_where_unchecked`）が行い、
//! 第 2 の書き込み経路は作らない。呼び出し元は `core::dml_join_exec`（`EngineCore` の結合形 DML 入口）。
//! #1754 の束縛器は本モジュールの契約型（[`JoinedDmlPlan`] ほか）を生成する。
//!
//! 方式（対象側を probe 側にしたハッシュ結合）:
//! - FROM／USING 側（build 側）は JOIN の側スキャンと同じ経路（`sql::scan::execute_scan_with_budget`）を
//!   書き込む側の `ctx`（RLS 暗黙適用）・同じ write txn で走査し、結合キーの正準バイト列
//!   （[`super::values::encode_key_component`]）でハッシュ表にする。NULL を含むキーは一致しない。
//! - 対象側（probe 側）は DML の候補列挙（`tenant::enumerate_dml_candidates_with`）をそのまま使う。
//!   対象表を JOIN の relation として走査しない理由: 可視スキャンは他テナントの `Public` 行を含み、
//!   `ResultRow.id` はテナント修飾されないため自テナントの同じ id の行と取り違えうる（P0）。また
//!   JOIN の入力行数上限・`LIMIT` が対象側の走査上限（1,000,000）と影響行数を黙って置き換えてしまう。
//!
//! 可視性の非対称（意図的）: 対象表は「所有∧可視」（書き込み認可）、FROM／USING 側は「可視」
//! （SELECT の JOIN と同じ読み取り意味論で、他テナントの `Public` 行を含みうる）。他テナントの
//! `Private` 行はいずれの側にも現れない。
//!
//! 多重一致の一意化: 候補列挙は対象行 1 件につき probe を 1 回だけ呼ぶため、対象行は構造的に 1 回だけ
//! 更新・削除される。採る source 行は一致位置（走査順）の先頭で決定的（PostgreSQL の「不定」の範囲）。
//!
//! 上限（超過は `54000`。呼び出し元が write txn を commit しないため副作用ゼロ）:
//! リレーション数（[`JoinLimits::max_relations`]）・FROM 側の可視行数（`max_input_rows`）・
//! 結合カーディナリティ（`max_output_rows`。**一意化の前**に一致ペアの累計で判定）・共有バイト予算
//! （[`JoinBudget`]。確保前に計上）。対象側の走査行数と影響行数は既存の DML 上限が適用する。
//!
//! 暫定事項: 台帳（RECOVER-11）の結線は Issue #1757 の担当。呼び出し元が暫定ゲートで台帳記録を
//! 伴う構成を拒否する。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::catalog::{ColumnType, TableSchema};
use crate::declarative_filter::MetadataFilter;
use crate::policy::PolicyContext;
use crate::row_codec::{ScalarRef, Value};
use crate::sql::allowlist::{
    JoinProjection, Projection, SqlSurfaceError, ValidatedJoin, ValidatedScan, WherePredicate,
};
use crate::sql::dml_from_returning::BoundFromReturning;
use crate::sql::exec::{Cell, ResultRow};
use crate::sql::relation::ColumnSlot;
use crate::sql::set_expr::BoundSetExpr;
use crate::sql::udf_call::{BoundExpr, UdfRegistry};
use crate::tenant::DmlCandidate;

use super::values::{
    cell_scalar, encode_key_component, key_cell_from_scalar, key_class, JoinKeyClass,
};
use super::{result_bytes, JoinBudget, JoinLimits};

/// 対象表（更新・削除する側）。WHERE は対象側だけで完結する部分で、既存の
/// `BoundPredicateUpdate`／`BoundPredicateDelete` の `metadata_filters`／`expr_filters`／
/// `or_filters` と同じ形（`sql::exec::run_where_predicate` がそのまま評価する）。
#[derive(Debug, Clone)]
pub(crate) struct JoinedDmlTarget {
    pub(crate) table: String,
    /// 束縛時スキーマ（実行時のスキーマと同一であることを書き込み側が照合する）。
    pub(crate) schema: TableSchema,
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    pub(crate) expr_filters: Vec<BoundExpr>,
    pub(crate) or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
}

/// FROM／USING 側の relation（1 個）。
#[derive(Debug, Clone)]
pub(crate) struct JoinedDmlSource {
    pub(crate) table: String,
    /// 束縛時スキーマ。
    pub(crate) schema: TableSchema,
    /// source 側だけで完結する WHERE（走査へ押し下げる）。
    pub(crate) where_predicates: Vec<WherePredicate>,
    /// 走査で取り出す source の列（`schema.columns` の添字）。SET 右辺が参照する列を並べる。
    /// 結合キーの列は不足分が末尾へ自動追加される（[`JoinedDmlPlan::source_columns`]）。
    pub(crate) columns: Vec<usize>,
    /// `RETURNING` だけが読む source の列（Issue #3204）。SET の式評価ビュー（`source_scalar`）へは
    /// 載せず、走査投影（[`JoinedDmlPlan::scan_columns`]）の末尾へだけ追記する。VECTOR 等の
    /// 式評価できない型でも、セルを複製して返せる。
    pub(crate) returning_columns: Vec<usize>,
    /// `RETURNING` が source の疑似列 `id` を参照するため、結合キーが使わなくても走査で `id` を
    /// 取り出すか（Issue #3204）。
    pub(crate) include_pseudo_id: bool,
    /// FROM／USING が 2 表以上、または JOIN の段を持つとき、source 側を結合実行器の出力 1 つへ
    /// 畳み込むための入力（Issue #3294）。`Some` のとき [`Self::table`] は空、[`Self::schema`] は
    /// 畳み込み列から合成したスキーマ（疑似列 `id` を持たない採番方式）で、`columns`・
    /// `returning_columns`・[`EquiKey::source_column`] はその添字系列を指す。
    pub(crate) fold: Option<Box<FoldedSource>>,
}

/// source 側の畳み込み入力（Issue #3294）。結合実行器（`sql::join`。SQL-28 の契約・RLS-10 の独立適用）へ
/// 渡す合成 JOIN 文と、畳み込み列の出自を持つ。第 2 の結合実行器は作らず、各リレーションを
/// 書き込むセッションの `PolicyContext` で独立に走査する。呼び出し元は `core::dml_join_exec`
/// （スキーマ照合に [`Self::real`] を使う）と、[`JoinedProbe`]（[`build_source_index`] が走査に使う）。
#[derive(Debug, Clone)]
pub(crate) struct FoldedSource {
    /// 合成した JOIN 文（FROM／USING 側の表だけ。射影は実行時に [`JoinedDmlPlan::scan_columns`] から
    /// 差し替えるためプレースホルダ）。
    pub(crate) join: ValidatedJoin,
    /// 畳み込み列の出自。畳み込みスキーマの列 i が、`join.relations[relation]`（0 起点）の
    /// `slot` 列であることを表す。
    pub(crate) cols: Vec<(usize, ColumnSlot)>,
    /// `join.relations` と同じ並びの実表名とスキーマ（束縛時。実行時の照合と結合実行器の入力に使う）。
    pub(crate) real: Vec<(String, TableSchema)>,
}

impl FoldedSource {
    /// 畳み込み列 `columns`（畳み込みスキーマの添字。走査投影の並び）を射影にした JOIN 文を返す。
    pub(crate) fn projected_join(
        &self,
        columns: &[usize],
    ) -> Result<ValidatedJoin, SqlSurfaceError> {
        let mut refs: Vec<crate::sql::relation::ColumnRef> = Vec::with_capacity(columns.len());
        for &c in columns {
            let (rel, slot) = self
                .cols
                .get(c)
                .ok_or_else(|| internal("folded source column out of range"))?;
            let table_ref = self
                .join
                .relations
                .get(*rel)
                .ok_or_else(|| internal("folded source relation out of range"))?;
            let (_, real_schema) = self
                .real
                .get(*rel)
                .ok_or_else(|| internal("folded source schema out of range"))?;
            let name = match slot {
                ColumnSlot::Id => "id".to_string(),
                ColumnSlot::Column(i) => real_schema
                    .columns
                    .get(*i)
                    .map(|col| col.name.clone())
                    .ok_or_else(|| internal("folded source real column out of range"))?,
            };
            refs.push(crate::sql::relation::ColumnRef::qualified(
                table_ref.exposed_name(),
                name,
            ));
        }
        let mut join = self.join.clone();
        join.projection = JoinProjection::Columns(refs);
        Ok(join)
    }

    /// 結合実行器へ渡す、表名 → 実表スキーマの対応。
    pub(crate) fn schema_map(&self) -> HashMap<String, TableSchema> {
        self.real.iter().cloned().collect()
    }

    /// 行を読まずに合成 JOIN 文を束縛する（`ON` の列・型・リレーション数の検証。変換の段階で
    /// Describe と Execute の拒否コードをそろえるために呼ぶ）。
    pub(crate) fn prepare(
        &self,
        columns: &[usize],
        udfs: &UdfRegistry,
    ) -> Result<(), SqlSurfaceError> {
        let join = self.projected_join(columns)?;
        let schemas = self.schema_map();
        super::prepare_join(
            &schemas,
            &super::CteIdColumns::new(),
            &join,
            udfs,
            &JoinLimits::default(),
        )
        .map(|_| ())
    }
}

/// 等値結合キー 1 本。`None` は疑似列 `id`（`has_pseudo_id` の表のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EquiKey {
    pub(crate) target_column: Option<usize>,
    pub(crate) source_column: Option<usize>,
}

/// `SET` の項（右辺が結合ビュー上の式）。`set.bound` の列参照は「対象列 ++ source 列
/// （[`JoinedDmlPlan::source_columns`] の並び）」を連結した結合スカラービューの添字で解決済み
/// （対象列は `0..T`、source 列は `T + 並び位置`）。
#[derive(Debug, Clone)]
pub(crate) struct JoinedSetItem {
    pub(crate) column_index: usize,
    pub(crate) set: BoundSetExpr,
}

/// source 列をそのまま対象列へ写す SET 項（`SET t.c = s.c`）。式評価（`bind_set_expr`）が受理しない
/// 列型（JSONB／JSON／UUID／BYTEA／NUMERIC／ENUM 等）の単純コピーを、型を制限せず運ぶ。
/// 対象列型と source 列型は変換段階で一致を確認済み（不一致は計画に載らない）。
#[derive(Debug, Clone)]
pub(crate) struct JoinedCopyItem {
    pub(crate) column_index: usize,
    /// [`JoinedDmlPlan::source_columns`] の並び位置。
    pub(crate) source_pos: usize,
    /// 代入先の列型（セルから行値への変換に使う）。
    pub(crate) target_ty: ColumnType,
    /// 代入先の列が NULL を許すか。
    pub(crate) nullable: bool,
    /// 代入先の列名（`23502` の文言用）。
    pub(crate) column_name: String,
}

/// `UPDATE` の SET 句。
#[derive(Debug, Clone, Default)]
pub(crate) struct JoinedUpdateSet {
    /// source 列の単純コピー項。
    pub(crate) copies: Vec<JoinedCopyItem>,
    /// 行によらず定まる代入（リテラル）。
    pub(crate) literal_assignments: Vec<(usize, Value)>,
    /// 結合ビュー上の式項。
    pub(crate) items: Vec<JoinedSetItem>,
}

/// 結合形 `UPDATE`／`DELETE` の束縛済み実行入力。
#[derive(Debug, Clone)]
pub(crate) struct JoinedDmlPlan {
    pub(crate) target: JoinedDmlTarget,
    pub(crate) source: JoinedDmlSource,
    /// 等値結合キー（1 本以上。複数なら複合キー）。
    pub(crate) keys: Vec<EquiKey>,
    /// `Some` なら `UPDATE`、`None` なら `DELETE`。
    pub(crate) update: Option<JoinedUpdateSet>,
    /// 束縛済みの `RETURNING`（Issue #3204）。source 側の列は計画段で
    /// [`Self::source_cell_position`] により位置が確定済み。
    pub(crate) returning: Option<BoundFromReturning>,
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

impl JoinedDmlPlan {
    /// FROM／USING 側の表の数（畳み込みなしは 1）。`RETURNING` の収集が relation ごとの行参照を
    /// 並べる数と、[`Self::validate`] のリレーション数上限の判定に使う。
    pub(crate) fn source_relation_count(&self) -> usize {
        self.source.fold.as_ref().map_or(1, |f| f.real.len().max(1))
    }

    /// source 側で実際に取り出す列。[`JoinedDmlSource::columns`] に、結合キーの列で未含のものを
    /// 末尾へ追加したもの（結合ビューの添字系列は先頭から不変）。
    pub(crate) fn source_columns(&self) -> Vec<usize> {
        let mut cols = self.source.columns.clone();
        for k in &self.keys {
            if let Some(c) = k.source_column {
                if !cols.contains(&c) {
                    cols.push(c);
                }
            }
        }
        cols
    }

    /// 走査で実際に取り出す列。[`Self::source_columns`]（SET の式評価ビューと同じ並び）の後ろへ、
    /// `RETURNING` 専用の列で未含のものを追記したもの。セル位置の解決（`RETURNING`・結合キー）は
    /// これを基準にし、SET ビューは [`Self::source_columns`] のみを使う。
    pub(crate) fn scan_columns(&self) -> Vec<usize> {
        let mut cols = self.source_columns();
        for &c in &self.source.returning_columns {
            if !cols.contains(&c) {
                cols.push(c);
            }
        }
        cols
    }

    /// 走査結果の `cells` が先頭に疑似列 `id` を持つか。`build_source_index`（結合キーの位置）・
    /// [`JoinedProbe`]（SET 用のセル開始位置）・`RETURNING`（[`Self::source_cell_position`]）が
    /// 同じ判定を共有し、位置のずれを構造的に防ぐ。
    pub(crate) fn source_id_in_cells(&self) -> bool {
        self.source.include_pseudo_id || self.keys.iter().any(|k| k.source_column.is_none())
    }

    /// source の列スロットが走査結果の `cells` 内で占める位置。走査で取り出していない列は `XX000`。
    pub(crate) fn source_cell_position(&self, slot: ColumnSlot) -> Result<usize, SqlSurfaceError> {
        let id_offset = usize::from(self.source_id_in_cells());
        match slot {
            ColumnSlot::Id if self.source_id_in_cells() => Ok(0),
            ColumnSlot::Id => Err(internal("source pseudo id is not in the scan projection")),
            ColumnSlot::Column(c) => self
                .scan_columns()
                .iter()
                .position(|&x| x == c)
                .and_then(|p| p.checked_add(id_offset))
                .ok_or_else(|| internal("source column is not in the scan projection")),
        }
    }

    /// データに依存しない形の検証。走査より前に呼ぶ（fail-closed）。
    pub(crate) fn validate(&self, limits: &JoinLimits) -> Result<(), SqlSurfaceError> {
        // 対象表＋ FROM／USING 側の全 relation（畳み込みでは結合する表の数）。
        let relation_count = 1usize.saturating_add(self.source_relation_count());
        if relation_count > limits.max_relations {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "too many tables in JOIN: {relation_count} exceeds limit {}",
                limits.max_relations
            )));
        }
        let (t_len, s_len) = (
            self.target.schema.columns.len(),
            self.source.schema.columns.len(),
        );
        for k in &self.keys {
            match k.target_column {
                Some(i) if i >= t_len => return Err(internal("join key column out of range")),
                None if !self.target.schema.has_pseudo_id() => {
                    return Err(internal("join key on a missing pseudo id column"))
                }
                _ => {}
            }
            match k.source_column {
                Some(i) if i >= s_len => return Err(internal("join key column out of range")),
                None if !self.source.schema.has_pseudo_id() => {
                    return Err(internal("join key on a missing pseudo id column"))
                }
                _ => {}
            }
        }
        if self
            .source
            .columns
            .iter()
            .chain(&self.source.returning_columns)
            .any(|&c| c >= s_len)
        {
            return Err(internal("source column out of range"));
        }
        if let Some(u) = &self.update {
            let in_range = |i: usize| i < t_len;
            let src_len = self.source_columns().len();
            if u.items.iter().any(|it| !in_range(it.column_index))
                || u.copies
                    .iter()
                    .any(|c| !in_range(c.column_index) || c.source_pos >= src_len)
                || u.literal_assignments.iter().any(|(i, _)| !in_range(*i))
            {
                return Err(internal("SET column out of range"));
            }
        }
        Ok(())
    }

    /// 結合キーごとの型クラス。両辺のクラスが異なれば `42804`、許可されない型は `42601`。
    fn key_classes(&self) -> Result<Vec<JoinKeyClass>, SqlSurfaceError> {
        fn class_of(
            schema: &TableSchema,
            column: Option<usize>,
        ) -> Result<JoinKeyClass, SqlSurfaceError> {
            match column {
                None => key_class(None),
                Some(i) => {
                    let col = schema
                        .columns
                        .get(i)
                        .ok_or_else(|| internal("join key column out of range"))?;
                    key_class(Some(&col.ty))
                }
            }
        }
        let mut out = Vec::with_capacity(self.keys.len());
        for k in &self.keys {
            let t = class_of(&self.target.schema, k.target_column)?;
            let s = class_of(&self.source.schema, k.source_column)?;
            if t != s {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "JOIN key columns have incompatible types",
                ));
            }
            out.push(t);
        }
        Ok(out)
    }
}

/// FROM／USING 側の可視行とその結合キーのハッシュ表（値は走査位置の昇順＝決定的）。
pub(crate) struct SourceIndex {
    pub(crate) rows: Vec<ResultRow>,
    table: HashMap<Vec<u8>, Vec<u32>>,
}

fn to_u32(v: usize) -> Result<u32, SqlSurfaceError> {
    u32::try_from(v).map_err(|_| SqlSurfaceError::payload_too_large("JOIN row count exceeds limit"))
}

/// source が 1 表で JOIN の段を持たない形の走査（従来方式）。JOIN の側スキャンと同じ経路
/// （`sql::scan::execute_scan_with_budget`）を、書き込む側の `ctx`（RLS 暗黙適用）・同じ write txn で
/// 実行する。
#[allow(clippy::too_many_arguments)]
fn scan_single_source(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    plan: &JoinedDmlPlan,
    columns: &[usize],
    id_needed: bool,
    udfs: &UdfRegistry,
    limits: &JoinLimits,
    budget: &JoinBudget,
) -> Result<crate::sql::exec::QueryResult, SqlSurfaceError> {
    let schema = &plan.source.schema;
    let mut names: Vec<String> = Vec::with_capacity(columns.len().saturating_add(1));
    if id_needed {
        names.push("id".to_string());
    }
    for &c in columns {
        let col = schema
            .columns
            .get(c)
            .ok_or_else(|| internal("source column out of range"))?;
        names.push(col.name.clone());
    }
    let scan = ValidatedScan {
        table_name: plan.source.table.clone(),
        projection: Projection::Columns(names),
        where_predicates: plan.source.where_predicates.clone(),
        limit: 1,
        limit_implicit: false,
        order_by: Vec::new(),
        offset: 0,
        window_items: Vec::new(),
        order_keys: Vec::new(),
        scalar_subquery_items: Vec::new(),
        physical_order_keys: Vec::new(),
        embedded_scalar_subqueries: Vec::new(),
        with_ties: false,
        distinct_on: None,
    };
    // 側スキャンの `LIMIT` は内部値（入力行数上限 + 1）に差し替える（JOIN の側スキャンと同じ）。
    let mut bound = crate::sql::parser::bind_scan_with_dummy_flags(
        &scan,
        schema,
        udfs,
        &[],
        &crate::result_limits::ResultLimits::default(),
    )?;
    bound.limit = limits.max_input_rows.saturating_add(1);
    crate::sql::scan::execute_scan_with_budget(read_txn, ctx, schema, &bound, budget.remaining())
}

/// source が 2 表以上または JOIN の段を持つ形の走査（Issue #3294）。合成した JOIN 文を結合実行器
/// （`sql::join::execute_with_limits`）で評価する。各リレーションは `ctx`（RLS 暗黙適用）で独立に
/// 走査され、結合の方式・入力行数・出力行数・NLJ の比較回数の上限と超過時の `54000` は SELECT の
/// JOIN と同じ契約に従う。畳み込んだ出力の行数は単一表 source と同じ可視行数上限（`max_input_rows`）に
/// そろえ、結合実行器の予算の上限は共有予算の残りに絞る。
fn scan_folded_source(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    fold: &FoldedSource,
    columns: &[usize],
    udfs: &UdfRegistry,
    limits: &JoinLimits,
    budget: &JoinBudget,
) -> Result<crate::sql::exec::QueryResult, SqlSurfaceError> {
    let join = fold.projected_join(columns)?;
    let schemas = fold.schema_map();
    let join_limits = limits
        .with_budget_cap(budget.remaining())
        .with_max_result_rows(limits.max_input_rows);
    super::execute_with_limits(read_txn, ctx, &schemas, &join, udfs, &join_limits)
}

/// FROM／USING 側を `ctx`（RLS 暗黙適用）で走査してハッシュ表にする。`read_txn` は呼び出し元の
/// 共有 write txn（自トランザクションの未 commit の変更が見え、文の開始時点のスナップショットになる）。
fn build_source_index(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    plan: &JoinedDmlPlan,
    classes: &[JoinKeyClass],
    udfs: &UdfRegistry,
    limits: &JoinLimits,
    budget: &mut JoinBudget,
) -> Result<SourceIndex, SqlSurfaceError> {
    let columns = plan.scan_columns();
    let id_needed = plan.source_id_in_cells();
    let id_offset = usize::from(id_needed);
    let result = match &plan.source.fold {
        Some(fold) => scan_folded_source(read_txn, ctx, fold, &columns, udfs, limits, budget)?,
        None => scan_single_source(
            read_txn, ctx, plan, &columns, id_needed, udfs, limits, budget,
        )?,
    };
    if result.rows.len() > limits.max_input_rows {
        return Err(SqlSurfaceError::payload_too_large(
            "JOIN input exceeds the visible row limit",
        ));
    }
    budget.charge(result_bytes(result.columns.len(), &result.rows))?;

    // 結合キー列のセル位置。
    let mut key_pos: Vec<usize> = Vec::with_capacity(plan.keys.len());
    for k in &plan.keys {
        let pos = match k.source_column {
            None => 0,
            Some(c) => columns
                .iter()
                .position(|&x| x == c)
                .map(|p| p + id_offset)
                .ok_or_else(|| internal("join key column missing from source projection"))?,
        };
        key_pos.push(pos);
    }

    let mut table: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
    'rows: for (idx, row) in result.rows.iter().enumerate() {
        let idx_u32 = to_u32(idx)?;
        let mut key: Vec<u8> = Vec::new();
        for (pos, class) in key_pos.iter().zip(classes) {
            let cell = row
                .cells
                .get(*pos)
                .ok_or_else(|| internal("JOIN key position out of range in result row"))?;
            if matches!(cell, Cell::Null) {
                // NULL を含むキーは一致しない（ハッシュ表へ入れない）。
                continue 'rows;
            }
            encode_key_component(cell, class, &mut key)?;
        }
        budget.charge(key.len().saturating_add(std::mem::size_of::<u32>()))?;
        table.entry(key).or_default().push(idx_u32);
    }
    Ok(SourceIndex {
        rows: result.rows,
        table,
    })
}

fn value_bytes(v: &Value) -> usize {
    match v {
        Value::Text(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Json(s) => s.len(),
        Value::Vector(x) => x.len().saturating_mul(std::mem::size_of::<f32>()),
        // 配列の SET 結果も候補数に比例して保持されるため共有予算へ計上する。
        Value::Array(a) => a.approx_heap_bytes(),
        _ => 0,
    }
}

/// 対象側の候補行を FROM／USING 側のハッシュ表へ突き合わせる probe。`tenant` の候補列挙の述語
/// クロージャ内で、対象側 WHERE を通った行ごとに 1 回だけ呼ぶ。
pub(crate) struct JoinedProbe<'a> {
    plan: &'a JoinedDmlPlan,
    classes: Vec<JoinKeyClass>,
    /// `RETURNING` の収集側（[`Self::index_handle`]）と共有する。probe 本体は読み取りしかしない。
    index: Rc<SourceIndex>,
    /// 結合ビューの source 列（[`JoinedDmlPlan::source_columns`]）と、そのセル開始位置。
    source_columns: Vec<usize>,
    cell_offset: usize,
    budget: JoinBudget,
    max_output_rows: usize,
    /// 一意化の前の一致ペアの累計（`max_output_rows` の判定対象）。
    pairs: usize,
    /// 一致した対象行 id と採った source の走査位置（`RETURNING` の収集側と共有する。Issue #3204）。
    /// 述語クロージャ（書き込み）と収集クロージャ（読み取り）が同じ文内で別々に借用するため
    /// `RefCell` に置く。借用の重なりは起きない設計だが、起きても panic せず `XX000` にする。
    matches: Rc<RefCell<HashMap<u64, u32>>>,
}

impl<'a> JoinedProbe<'a> {
    /// 形の検証 → source の走査 → ハッシュ表の構築までを行う。失敗は走査より前か走査中で、
    /// 書き込みは一切行われていない。
    pub(crate) fn new(
        read_txn: &impl crate::storage::read_source::ReadSource,
        ctx: &PolicyContext,
        plan: &'a JoinedDmlPlan,
        udfs: &UdfRegistry,
        limits: &JoinLimits,
    ) -> Result<Self, SqlSurfaceError> {
        plan.validate(limits)?;
        let classes = plan.key_classes()?;
        let mut budget = JoinBudget::new(limits.budget_cap);
        let index = build_source_index(read_txn, ctx, plan, &classes, udfs, limits, &mut budget)?;
        let source_columns = plan.source_columns();
        let cell_offset = usize::from(plan.source_id_in_cells());
        Ok(Self {
            plan,
            classes,
            index: Rc::new(index),
            source_columns,
            cell_offset,
            budget,
            max_output_rows: limits.max_output_rows,
            pairs: 0,
            matches: Rc::new(RefCell::new(HashMap::new())),
        })
    }

    /// SET 式が embedding を参照するか（候補列挙で embedding を復号する必要があるか）。
    pub(crate) fn needs_embedding(&self) -> bool {
        self.plan.update.as_ref().is_some_and(|u| {
            u.items
                .iter()
                .any(|it| crate::sql::udf_call::references_embedding(&it.set.bound))
        })
    }

    /// 走査結果の共有ハンドル（`RETURNING` の収集側が一致行を引くのに使う）。
    pub(crate) fn index_handle(&self) -> Rc<SourceIndex> {
        Rc::clone(&self.index)
    }

    /// 一致記録の共有ハンドル（対象行 id → source の走査位置）。
    pub(crate) fn matches_handle(&self) -> Rc<RefCell<HashMap<u64, u32>>> {
        Rc::clone(&self.matches)
    }

    /// 一致した対象行 id と採った source の走査位置（対象行 id 昇順＝候補列挙順）。
    #[cfg(test)]
    pub(crate) fn matches_sorted(&self) -> Result<Vec<(u64, u32)>, SqlSurfaceError> {
        let map = self
            .matches
            .try_borrow()
            .map_err(|_| internal("joined DML match log is busy"))?;
        let mut out: Vec<(u64, u32)> = map.iter().map(|(&k, &v)| (k, v)).collect();
        out.sort_unstable();
        Ok(out)
    }

    /// 一致した source 行の走査位置の先頭（決定的）を返す。一致なしは `None`。
    fn first_match(
        &mut self,
        c: &DmlCandidate<'_>,
        scanned: &[Option<ScalarRef<'_>>],
    ) -> Result<Option<u32>, SqlSurfaceError> {
        let mut key: Vec<u8> = Vec::new();
        for (k, class) in self.plan.keys.iter().zip(&self.classes) {
            let cell = match k.target_column {
                // 疑似列 `id`（`validate` で `has_pseudo_id` を確認済み）。
                None => Cell::Integer(c.id),
                Some(i) => match scanned.get(i) {
                    Some(Some(sr)) => key_cell_from_scalar(sr, class)?,
                    // NULL を含むキーは一致しない。
                    Some(None) => return Ok(None),
                    None => return Err(internal("join key column missing from the target row")),
                },
            };
            encode_key_component(&cell, class, &mut key)?;
        }
        let Some(matched) = self.index.table.get(&key) else {
            return Ok(None);
        };
        // 一意化の前に一致ペアを数え、上限超過は何も確保せずに拒否する。
        self.pairs = self.pairs.saturating_add(matched.len());
        if self.pairs > self.max_output_rows {
            return Err(SqlSurfaceError::payload_too_large(
                "JOIN result exceeds the row limit",
            ));
        }
        let first = matched.first().copied();
        if let Some(pos) = first {
            self.budget.charge(std::mem::size_of::<(u64, u32)>())?;
            let mut log = self
                .matches
                .try_borrow_mut()
                .map_err(|_| internal("joined DML match log is busy"))?;
            log.try_reserve(1).map_err(|_| {
                SqlSurfaceError::payload_too_large("JOIN result exceeds the row limit")
            })?;
            log.insert(c.id, pos);
        }
        Ok(first)
    }

    /// `DELETE ... USING` 用。対象行が一致すれば真（削除対象）。
    pub(crate) fn probe_delete(
        &mut self,
        schema: &TableSchema,
        c: &DmlCandidate<'_>,
    ) -> Result<bool, SqlSurfaceError> {
        let scanned = crate::row_codec::scan_scalar_columns(schema, c.metadata)?;
        Ok(self.first_match(c, &scanned)?.is_some())
    }

    /// `UPDATE ... FROM` 用。一致すれば、採った source 行から評価した行ごとの SET 値を返す。
    pub(crate) fn probe_update(
        &mut self,
        schema: &TableSchema,
        c: &DmlCandidate<'_>,
    ) -> Result<Option<Vec<(usize, Value)>>, SqlSurfaceError> {
        let scanned = crate::row_codec::scan_scalar_columns(schema, c.metadata)?;
        let Some(pos) = self.first_match(c, &scanned)? else {
            return Ok(None);
        };
        let items = match &self.plan.update {
            Some(u) => u.items.as_slice(),
            None => return Err(internal("joined UPDATE probe without a SET clause")),
        };
        self.budget
            .charge(std::mem::size_of::<Vec<(usize, Value)>>())?;
        let copies = self
            .plan
            .update
            .as_ref()
            .map_or(&[][..], |u| u.copies.as_slice());
        if items.is_empty() && copies.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let row = self
            .index
            .rows
            .get(pos as usize)
            .ok_or_else(|| internal("matched source row out of range"))?;
        // 結合スカラービュー: 対象列 ++ source 列（取り出した列の並び）。
        // 配列セルは借用形（`ArrayRef`）が正準ペイロードを要するため、先に所有バッファへ
        // 書き出す（ビューはそれを借用する）。
        let mut source_cells: Vec<(&Cell, &ColumnType)> =
            Vec::with_capacity(self.source_columns.len());
        let mut array_payloads: Vec<Option<Vec<u8>>> =
            Vec::with_capacity(self.source_columns.len());
        for (k, &col_idx) in self.source_columns.iter().enumerate() {
            let col = self
                .plan
                .source
                .schema
                .columns
                .get(col_idx)
                .ok_or_else(|| internal("source column out of range"))?;
            let cell = row
                .cells
                .get(self.cell_offset.saturating_add(k))
                .ok_or_else(|| internal("source cell out of range"))?;
            let payload = match cell {
                Cell::Array(a) => {
                    let mut buf = Vec::new();
                    crate::row_codec::write_array_elements_payload(&mut buf, a)
                        .map_err(|_| internal("source array column could not be re-encoded"))?;
                    self.budget.charge(buf.len())?;
                    Some(buf)
                }
                _ => None,
            };
            source_cells.push((cell, &col.ty));
            array_payloads.push(payload);
        }
        let mut view: Vec<Option<ScalarRef<'_>>> =
            Vec::with_capacity(scanned.len().saturating_add(source_cells.len()));
        view.extend(scanned.iter().copied());
        for ((cell, ty), payload) in source_cells.iter().zip(&array_payloads) {
            view.push(source_scalar(cell, ty, payload.as_deref())?);
        }
        let mut out: Vec<(usize, Value)> = Vec::with_capacity(items.len() + copies.len());
        for copy in copies {
            let (cell, _) = source_cells
                .get(copy.source_pos)
                .ok_or_else(|| internal("source copy position out of range"))?;
            if matches!(cell, Cell::Null) && !copy.nullable {
                return Err(SqlSurfaceError::not_null_violation(&copy.column_name));
            }
            let value = copy_cell_to_value((*cell).clone(), &copy.target_ty)?;
            self.budget.charge(
                std::mem::size_of::<(usize, Value)>().saturating_add(value_bytes(&value)),
            )?;
            out.push((copy.column_index, value));
        }
        for item in items {
            let value =
                crate::sql::set_expr::eval_set_expr_to_value(&item.set, c.id, c.embedding, &view)?;
            self.budget.charge(
                std::mem::size_of::<(usize, Value)>().saturating_add(value_bytes(&value)),
            )?;
            out.push((item.column_index, value));
        }
        Ok(Some(out))
    }
}

/// 単純コピーのセルを対象列型の行値へ変換する。`matview::cell_to_value` は ENUM のセルを扱わないため、
/// ここで語彙検証して `Value::Enum` にする（語彙外は `22P02`。行の値は固定文言に含めない）。
/// それ以外の型は `matview::cell_to_value` に委ねる。
fn copy_cell_to_value(cell: Cell, ty: &ColumnType) -> Result<Value, SqlSurfaceError> {
    match (ty, cell) {
        (ColumnType::Enum(def), Cell::Text(label)) => {
            if def.validate_label(&label).is_err() {
                return Err(SqlSurfaceError::invalid_text_representation(
                    "invalid input value for enum type",
                ));
            }
            Ok(Value::Enum(label))
        }
        (_, cell) => crate::sql::matview::cell_to_value(cell, ty),
    }
}

/// 結合 `UPDATE ... FROM` の SET 式が参照する source 列のセルを、借用形（[`ScalarRef`]）へ写像する。
/// [`cell_scalar`] が扱わない `TIME`／`TIMESTAMPTZ`／`INTERVAL`／`CHAR(n)`／配列を加える
/// （`bind_set_expr` が受理する列型と一致させる。対象表側は行 codec の走査結果をそのまま使う）。
/// 配列は呼び出し側が `write_array_elements_payload` で作った `array_payload` を借用する。
/// 想定外の型・セルの組は `cell_scalar` と同じく `Internal`（fail-closed）。
fn source_scalar<'a>(
    cell: &'a Cell,
    ty: &ColumnType,
    array_payload: Option<&'a [u8]>,
) -> Result<Option<ScalarRef<'a>>, SqlSurfaceError> {
    Ok(match (ty, cell) {
        (_, Cell::Null) => None,
        (ColumnType::Time, Cell::Time(t)) => Some(ScalarRef::Time(*t)),
        (ColumnType::TimestampTz, Cell::TimestampTz(t)) => Some(ScalarRef::TimestampTz(*t)),
        (ColumnType::Interval, Cell::Interval(i)) => Some(ScalarRef::Interval(*i)),
        (ColumnType::Bpchar(_), Cell::Text(s)) => Some(ScalarRef::Text(s)),
        // JSONB は JSON と値表現を共有するため、既存の式評価と同じ Json 借用形へ写す。
        (ColumnType::Jsonb, Cell::Json(s)) => Some(ScalarRef::Json(s)),
        (ColumnType::Array(_), Cell::Array(a)) => {
            let payload = array_payload.ok_or_else(|| internal("source array payload missing"))?;
            let count = u32::try_from(a.len())
                .map_err(|_| internal("source array length exceeds limit"))?;
            Some(ScalarRef::Array(crate::row_codec::ArrayRef::from_owned(
                a.elem(),
                count,
                a.frame_flags(),
                payload,
            )))
        }
        _ => cell_scalar(cell, ty)?,
    })
}

#[cfg(test)]
#[path = "dml_tests.rs"]
mod tests;
