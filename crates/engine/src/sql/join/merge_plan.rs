//! `MERGE ... WHEN MATCHED` の実行計画と、組ごとの WHEN 選択・SET 評価（SQL-79・Issue #3353。親 #3349、
//! 設計記録は `docs/design/sql-merge.md` の D3・D4・D5）。
//!
//! 責務境界: 束縛済みの [`BoundMerge`]（#3352。relation 添字は 0 = 対象、1 = 元）を、既存の結合 DML の実行入力
//! （[`JoinedDmlPlan`]。`UPDATE ... FROM` の probe・ハッシュ結合をそのまま使う。第 2 の結合実行器は作らない）と、
//! WHEN 句ごとの評価器へ変換する。行・台帳・世代・RLS・書き込みトランザクションには触れない（純関数と、
//! 渡された 1 組の行に対する評価だけ）。呼び出し元は `core::merge_exec`（第 1 段〔操作列の決定〕で
//! [`MergeEval`] を使い、第 3 段の適用は既存の書き込み経路へ渡す）。
//!
//! 評価ビューの並び（評価単位ごとに独立）: 各条件の残余（[`JoinedResidual::source_columns`]）と各 SET 式の
//! 元の列リストは、それぞれ独自の並びを持つ。ビューは常に「対象行の走査結果 ++ その評価単位が参照する元の列
//! （その単位の並び）」を単位ごとに組み立てる（計画全体で 1 つの並びを共有しない）。走査の投影
//! （[`JoinedDmlSource::columns`]）には全評価単位の参照列の和集合を入れ、各単位はセル位置を
//! [`JoinedDmlPlan::source_cell_position`] で引く（投影に無い列は `XX000`）。
//!
//! `WHEN ... AND` の条件は、束縛器が対象と元をまたぐ列同士の等値を結合キー（[`CondKey`]）へ、それ以外を残余へ
//! 振り分けている。両方を評価しないと条件が黙って広がるため、[`CondPlan`] は両方を持つ。キーの比較は
//! `ON` の結合と同じ正準バイト列（`encode_key_component`）の一致で行い、どちらかが NULL なら真にしない。
//!
//! 押し下げ（D3）は本 Issue では行わない: `ON` の等値キー以外はすべて残余に残す。正しさが押し下げの有無に
//! 依存しないため。BY TARGET／BY SOURCE の有無を見て最適化する判断は #3354 が担う。
//!
//! テナント境界（P0）: 対象側の候補（所有かつ可視）は呼び出し元が `tenant` の列挙で渡す。本モジュールは
//! 他テナントの行を引く経路を持たない。エラー文言は固定文言と入力された名前だけで、値・件数を含めない。

use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec::{ScalarRef, Value};
use crate::sql::allowlist::{MergeMatchKind, SqlSurfaceError, WherePredicate};
use crate::sql::exec::{Cell, ResultRow};
use crate::sql::merge_bind::{BoundAssignments, BoundMerge, BoundMergeAction};
use crate::sql::relation::ColumnSlot;
use crate::sql::set_expr::BoundSetExpr;
use crate::sql::udf_call::Expr;
use crate::tenant::DmlCandidate;

use super::dml::{
    copy_cell_to_value, source_scalar, value_bytes, EquiKey, JoinedDmlPlan, JoinedDmlSource,
    JoinedDmlTarget, JoinedResidual, ResidualEval,
};
use super::dml_plan::{set_expr_accepts_target, source_type_supported};
use super::values::{encode_key_component, key_cell_from_scalar, key_class, JoinKeyClass};

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// 元の形（計画の入力）。実表・単純形ビューは基底表の名前とスキーマ（ビューはビュー定義の述語つき）、
/// 部分クエリは実体化した行の合成スキーマ。
pub(crate) struct MergeSourceInput {
    /// 実表（ビューなら基底表）の名前。部分クエリは空。
    pub(crate) table: String,
    /// 束縛時のスキーマ。
    pub(crate) schema: TableSchema,
    /// 単純形ビューの定義由来の述語（基底表の物理列名。内側が先頭）。ビューでなければ空。
    pub(crate) view_predicates: Vec<WherePredicate>,
}

/// `WHEN MATCHED` だけで書かれた `MERGE` の実行計画。
#[derive(Debug, Clone)]
pub(crate) struct MergeExecPlan {
    /// 結合の実行入力（`update`・`returning` は `None`。対象側の述語は空）。
    pub(crate) joined: JoinedDmlPlan,
    clauses: Vec<MergeClausePlan>,
    /// SET 式が対象行の embedding を参照するか（候補列挙で復号する必要があるか）。
    pub(crate) needs_embedding: bool,
}

#[derive(Debug, Clone)]
struct MergeClausePlan {
    cond: Option<CondPlan>,
    action: ActionPlan,
}

#[derive(Debug, Clone)]
enum ActionPlan {
    Update(SetPlan),
    Delete,
    DoNothing,
}

/// `WHEN ... AND` 条件。対象と元をまたぐ列同士の等値（キー）と、それ以外（残余）の両方を持つ。
#[derive(Debug, Clone)]
struct CondPlan {
    keys: Vec<CondKey>,
    residual: Option<JoinedResidual>,
}

/// 条件内の対象と元をまたぐ列同士の等値（型クラス照合済み）。`None` は疑似列 `id`。
#[derive(Debug, Clone)]
struct CondKey {
    target_column: Option<usize>,
    source_column: Option<usize>,
    class: JoinKeyClass,
}

/// 1 句分の `UPDATE SET`。
#[derive(Debug, Clone, Default)]
struct SetPlan {
    /// 行によらず定まる代入（リテラル）。
    literals: Vec<(usize, Value)>,
    /// 評価ビュー上の式項。
    items: Vec<SetItem>,
    /// 式を経由できない代入先の型への、元の列の単純コピー。
    copies: Vec<CopyItem>,
}

/// 式項。`set` の列参照は「対象列 ++ `src_cols`（この項自身の並び）」のビューの添字で解決済み。
#[derive(Debug, Clone)]
struct SetItem {
    column_index: usize,
    set: BoundSetExpr,
    /// 参照する元の列（`source.schema.columns` の添字。この項のビューの並び）。
    src_cols: Vec<usize>,
}

/// 元の列の単純コピー項（対象列型と元の列型は一致を確認済み）。
#[derive(Debug, Clone)]
struct CopyItem {
    column_index: usize,
    source_column: usize,
    target_ty: ColumnType,
    nullable: bool,
    column_name: String,
}

/// 元の列を、重複なしで初出順に `cols` へ足す。
fn push_col(cols: &mut Vec<usize>, c: usize) {
    if !cols.contains(&c) {
        cols.push(c);
    }
}

fn slot_column(slot: ColumnSlot) -> Option<usize> {
    match slot {
        ColumnSlot::Id => None,
        ColumnSlot::Column(i) => Some(i),
    }
}

/// 束縛済みの `MERGE`（MATCHED の句だけ）を実行計画へ変換する。MATCHED 以外の句・INSERT は呼び出し元が
/// 先に拒否する前提だが、本関数も固定文言の `0A000` で fail-closed にする。
pub(crate) fn plan_merge(
    bound: &BoundMerge,
    target_table: &str,
    target_schema: &TableSchema,
    source: MergeSourceInput,
) -> Result<MergeExecPlan, SqlSurfaceError> {
    if bound.relations().len() != 2 {
        return Err(internal("MERGE relation count"));
    }
    let source_schema = &source.schema;

    // ON の結合キー（そのまま既存の結合の等値キー）。
    let keys: Vec<EquiKey> = bound
        .on_keys()
        .iter()
        .map(|(t, s)| EquiKey {
            target_column: slot_column(t.slot()),
            source_column: slot_column(s.slot()),
        })
        .collect();

    let mut columns: Vec<usize> = Vec::new();
    let mut include_pseudo_id = false;
    let mut clauses: Vec<MergeClausePlan> = Vec::with_capacity(bound.clauses().len());
    let mut needs_embedding = false;
    for clause in bound.clauses() {
        if clause.kind() != MergeMatchKind::Matched {
            return Err(SqlSurfaceError::feature_not_supported(
                "MERGE WHEN NOT MATCHED is not supported yet",
            ));
        }
        let cond = match clause.condition() {
            None => None,
            Some(c) => {
                let mut ckeys = Vec::with_capacity(c.keys().len());
                for (t, s) in c.keys() {
                    let (tc, sc) = (slot_column(t.slot()), slot_column(s.slot()));
                    let (tclass, sclass) =
                        (key_class(t.column_type())?, key_class(s.column_type())?);
                    if tclass != sclass {
                        return Err(SqlSurfaceError::datatype_mismatch(
                            "MERGE condition compares columns of incompatible types",
                        ));
                    }
                    match sc {
                        Some(i) => push_col(&mut columns, i),
                        None => include_pseudo_id = true,
                    }
                    ckeys.push(CondKey {
                        target_column: tc,
                        source_column: sc,
                        class: tclass,
                    });
                }
                if let Some(r) = c.residual() {
                    for &i in &r.source_columns {
                        push_col(&mut columns, i);
                    }
                }
                Some(CondPlan {
                    keys: ckeys,
                    residual: c.residual().cloned(),
                })
            }
        };
        let action = match clause.action() {
            BoundMergeAction::Update(a) => {
                let set = plan_set(a, target_schema, source_schema)?;
                for it in &set.items {
                    for &i in &it.src_cols {
                        push_col(&mut columns, i);
                    }
                    needs_embedding |= crate::sql::udf_call::references_embedding(&it.set.bound);
                }
                for cp in &set.copies {
                    push_col(&mut columns, cp.source_column);
                }
                ActionPlan::Update(set)
            }
            BoundMergeAction::Delete => ActionPlan::Delete,
            BoundMergeAction::DoNothing => ActionPlan::DoNothing,
            _ => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "MERGE WHEN MATCHED supports only UPDATE, DELETE and DO NOTHING",
                ));
            }
        };
        clauses.push(MergeClausePlan { cond, action });
    }

    let joined = JoinedDmlPlan {
        target: JoinedDmlTarget {
            table: target_table.to_string(),
            schema: target_schema.clone(),
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            or_filters: Vec::new(),
        },
        source: JoinedDmlSource {
            table: source.table,
            schema: source.schema,
            where_predicates: source.view_predicates,
            columns,
            returning_columns: Vec::new(),
            include_pseudo_id,
            fold: None,
        },
        keys,
        residual: bound.on().residual().cloned(),
        update: None,
        returning: None,
    };
    Ok(MergeExecPlan {
        joined,
        clauses,
        needs_embedding,
    })
}

/// `UPDATE SET` の 4 種（列参照・リテラル・対象だけの式・元の列を含む式）を [`SetPlan`] へ写す。
fn plan_set(
    a: &BoundAssignments,
    target_schema: &TableSchema,
    source_schema: &TableSchema,
) -> Result<SetPlan, SqlSurfaceError> {
    let mut set = SetPlan {
        literals: a.literals().to_vec(),
        ..SetPlan::default()
    };
    for (idx, e) in a.exprs() {
        set.items.push(SetItem {
            column_index: *idx,
            set: e.clone(),
            src_cols: Vec::new(),
        });
    }
    for (idx, e, cols) in a.joined_exprs() {
        set.items.push(SetItem {
            column_index: *idx,
            set: e.clone(),
            src_cols: cols.clone(),
        });
    }
    for (idx, resolved) in a.columns() {
        let target_col = target_schema
            .columns
            .get(*idx)
            .ok_or_else(|| internal("SET column out of range"))?;
        match resolved.relation() {
            0 => {
                // 対象の列から対象の列への代入は、対象だけの式として評価する（`UPDATE ... FROM` と同じ）。
                let name = crate::sql::dml_from::target_slot_name(resolved, target_schema)?;
                let bound = crate::sql::set_expr::bind_set_expr(
                    &target_col.name,
                    &Expr::Ident(name),
                    target_col,
                    target_schema,
                )?;
                set.items.push(SetItem {
                    column_index: *idx,
                    set: bound,
                    src_cols: Vec::new(),
                });
            }
            1 => {
                let src_idx = match resolved.slot() {
                    ColumnSlot::Id => {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "MERGE: the pseudo column id of the USING source cannot be used in SET",
                        ));
                    }
                    ColumnSlot::Column(i) => i,
                };
                let src_col = source_schema
                    .columns
                    .get(src_idx)
                    .ok_or_else(|| internal("source column out of range"))?;
                if set_expr_accepts_target(&target_col.ty) {
                    if !source_type_supported(&src_col.ty) {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "MERGE: the USING source column type is not supported in SET",
                        ));
                    }
                    // 「対象表 ++ この 1 列」の合成スキーマで束縛する（照合・型を引き継ぐ）。
                    let mut combined = target_schema.clone();
                    let mut def = src_col.clone();
                    let name = "\u{0}msrc0".to_string();
                    def.name = name.clone();
                    def.nullable = true;
                    combined.columns.push(def);
                    let bound = crate::sql::set_expr::bind_set_expr(
                        &target_col.name,
                        &Expr::Ident(name),
                        target_col,
                        &combined,
                    )?;
                    set.items.push(SetItem {
                        column_index: *idx,
                        set: bound,
                        src_cols: vec![src_idx],
                    });
                } else {
                    // 式経路が受理しない代入先の型（JSONB・UUID・BYTEA・NUMERIC・ENUM 等）。型が一致する
                    // 組だけを単純コピーで運ぶ。
                    if src_col.ty != target_col.ty {
                        return Err(SqlSurfaceError::datatype_mismatch(format!(
                            "column {:?} cannot be assigned a value of a different type",
                            target_col.name
                        )));
                    }
                    set.copies.push(CopyItem {
                        column_index: *idx,
                        source_column: src_idx,
                        target_ty: target_col.ty.clone(),
                        nullable: target_col.nullable,
                        column_name: target_col.name.clone(),
                    });
                }
            }
            _ => return Err(internal("SET relation index out of range")),
        }
    }
    Ok(set)
}

/// 1 組に対して選ばれた操作（第 1 段が数える対象）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    /// どの句の条件も真にならなかった（何もしない）。
    None,
    /// `DO NOTHING` の句が選ばれた（何もしない）。
    DoNothing,
    /// `DELETE` の句が選ばれた。
    Delete,
    /// `UPDATE` の句が選ばれた（句の添字）。
    Update(usize),
}

/// 実行時の評価器（残余の評価器・セル位置を解決済み）。計画 1 つにつき 1 つ作る。
pub(crate) struct MergeEval<'p> {
    plan: &'p MergeExecPlan,
    clauses: Vec<ClauseEval<'p>>,
}

struct ClauseEval<'p> {
    cond: Option<CondEval>,
    set: Option<SetEval<'p>>,
}

struct CondEval {
    /// (対象の列, 元のセル位置, 型クラス)。
    keys: Vec<(Option<usize>, usize, JoinKeyClass)>,
    residual: Option<ResidualEval>,
}

struct SetEval<'p> {
    plan: &'p SetPlan,
    /// 項ごとの、元の列のセル位置と列型（`SetItem::src_cols` と同じ並び）。
    items: Vec<Vec<(usize, ColumnType)>>,
    /// コピー項ごとのセル位置。
    copies: Vec<usize>,
}

impl<'p> MergeEval<'p> {
    /// 計画から評価器を作る。走査の投影に無い列を参照していれば `XX000`（計画の組み立ての不整合）。
    pub(crate) fn new(plan: &'p MergeExecPlan) -> Result<Self, SqlSurfaceError> {
        let joined = &plan.joined;
        let source_schema = &joined.source.schema;
        let ty_of = |c: usize| -> Result<ColumnType, SqlSurfaceError> {
            source_schema
                .columns
                .get(c)
                .map(|col| col.ty.clone())
                .ok_or_else(|| internal("source column out of range"))
        };
        let mut clauses = Vec::with_capacity(plan.clauses.len());
        for c in &plan.clauses {
            let cond = match &c.cond {
                None => None,
                Some(cp) => {
                    let mut keys = Vec::with_capacity(cp.keys.len());
                    for k in &cp.keys {
                        let slot = match k.source_column {
                            Some(i) => ColumnSlot::Column(i),
                            None => ColumnSlot::Id,
                        };
                        keys.push((
                            k.target_column,
                            joined.source_cell_position(slot)?,
                            k.class.clone(),
                        ));
                    }
                    let residual = match &cp.residual {
                        None => None,
                        Some(r) => Some(ResidualEval::new(joined, r)?),
                    };
                    Some(CondEval { keys, residual })
                }
            };
            let set = match &c.action {
                ActionPlan::Update(sp) => {
                    let mut items = Vec::with_capacity(sp.items.len());
                    for it in &sp.items {
                        let mut cells = Vec::with_capacity(it.src_cols.len());
                        for &col in &it.src_cols {
                            cells.push((
                                joined.source_cell_position(ColumnSlot::Column(col))?,
                                ty_of(col)?,
                            ));
                        }
                        items.push(cells);
                    }
                    let mut copies = Vec::with_capacity(sp.copies.len());
                    for cp in &sp.copies {
                        copies.push(
                            joined.source_cell_position(ColumnSlot::Column(cp.source_column))?,
                        );
                    }
                    Some(SetEval {
                        plan: sp,
                        items,
                        copies,
                    })
                }
                _ => None,
            };
            clauses.push(ClauseEval { cond, set });
        }
        Ok(Self { plan, clauses })
    }

    /// 対象行（`scanned`・`c`）と元の 1 行 `row` の組について、宣言順に条件を評価し、最初に真になった句の
    /// 操作を返す（真になる句が無ければ [`Choice::None`]。D4）。
    pub(crate) fn choose(
        &mut self,
        scanned: &[Option<ScalarRef<'_>>],
        c: &DmlCandidate<'_>,
        row: &ResultRow,
    ) -> Result<Choice, SqlSurfaceError> {
        for (i, clause) in self.clauses.iter_mut().enumerate() {
            let hit = match clause.cond.as_mut() {
                None => true,
                Some(cond) => cond.matches(scanned, c, row)?,
            };
            if !hit {
                continue;
            }
            return Ok(
                match self
                    .plan
                    .clauses
                    .get(i)
                    .map(|cp| &cp.action)
                    .ok_or_else(|| internal("MERGE clause out of range"))?
                {
                    ActionPlan::Update(_) => Choice::Update(i),
                    ActionPlan::Delete => Choice::Delete,
                    ActionPlan::DoNothing => Choice::DoNothing,
                },
            );
        }
        Ok(Choice::None)
    }

    /// 句 `clause` の `UPDATE SET` を、対象行と元の行から評価して `(列, 値)` の列にする。保持する値の
    /// バイト数は `charge` で共有予算へ計上する（超過は `54000`）。NULL を NOT NULL 列へ入れる組は `23502`。
    pub(crate) fn eval_update(
        &self,
        clause: usize,
        scanned: &[Option<ScalarRef<'_>>],
        c: &DmlCandidate<'_>,
        row: &ResultRow,
        target_schema: &TableSchema,
        charge: &mut dyn FnMut(usize) -> Result<(), SqlSurfaceError>,
    ) -> Result<Vec<(usize, Value)>, SqlSurfaceError> {
        let ce = self
            .clauses
            .get(clause)
            .ok_or_else(|| internal("MERGE clause out of range"))?;
        let se = ce
            .set
            .as_ref()
            .ok_or_else(|| internal("MERGE UPDATE clause without a SET plan"))?;
        let sp = se.plan;
        let mut out: Vec<(usize, Value)> = Vec::new();
        out.try_reserve(
            sp.literals
                .len()
                .saturating_add(sp.items.len())
                .saturating_add(sp.copies.len()),
        )
        .map_err(|_| SqlSurfaceError::payload_too_large("MERGE SET values exceed the limit"))?;
        for (idx, v) in &sp.literals {
            push_value(&mut out, *idx, v.clone(), target_schema, charge)?;
        }
        for (cp, &pos) in sp.copies.iter().zip(&se.copies) {
            let cell = row
                .cells
                .get(pos)
                .ok_or_else(|| internal("source cell out of range"))?;
            if matches!(cell, Cell::Null) && !cp.nullable {
                return Err(SqlSurfaceError::not_null_violation(&cp.column_name));
            }
            let value = copy_cell_to_value(cell.clone(), &cp.target_ty)?;
            push_value(&mut out, cp.column_index, value, target_schema, charge)?;
        }
        for (it, cells) in sp.items.iter().zip(&se.items) {
            // 配列セルは借用形が正準ペイロードを要するため、先に所有バッファへ書き出す（ビューが借用する）。
            let mut payloads: Vec<Option<Vec<u8>>> = Vec::with_capacity(cells.len());
            for (pos, _) in cells {
                let cell = row
                    .cells
                    .get(*pos)
                    .ok_or_else(|| internal("source cell out of range"))?;
                payloads.push(match cell {
                    Cell::Array(a) => {
                        let mut buf = Vec::new();
                        crate::row_codec::write_array_elements_payload(&mut buf, a)
                            .map_err(|_| internal("source array column could not be re-encoded"))?;
                        charge(buf.len())?;
                        Some(buf)
                    }
                    _ => None,
                });
            }
            let mut view: Vec<Option<ScalarRef<'_>>> =
                Vec::with_capacity(scanned.len().saturating_add(cells.len()));
            view.extend(scanned.iter().copied());
            for ((pos, ty), payload) in cells.iter().zip(&payloads) {
                let cell = row
                    .cells
                    .get(*pos)
                    .ok_or_else(|| internal("source cell out of range"))?;
                view.push(source_scalar(cell, ty, payload.as_deref())?);
            }
            let value =
                crate::sql::set_expr::eval_set_expr_to_value(&it.set, c.id, c.embedding, &view)?;
            push_value(&mut out, it.column_index, value, target_schema, charge)?;
        }
        Ok(out)
    }
}

/// SET の 1 値を共有予算へ計上して `out` へ積む。NULL を NOT NULL 列（VECTOR 列を除く。VECTOR の NULL は
/// 書き込み段が検査する）へ入れる組は `23502`。
fn push_value(
    out: &mut Vec<(usize, Value)>,
    idx: usize,
    value: Value,
    target_schema: &TableSchema,
    charge: &mut dyn FnMut(usize) -> Result<(), SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    charge(std::mem::size_of::<(usize, Value)>().saturating_add(value_bytes(&value)))?;
    if matches!(value, Value::Null) {
        if let Some(col) = target_schema.columns.get(idx) {
            if !col.nullable && !col.ty.is_vector() {
                return Err(SqlSurfaceError::not_null_violation(&col.name));
            }
        }
    }
    out.push((idx, value));
    Ok(())
}

impl CondEval {
    /// 条件が真か（キーの等値と残余の両方が真。NULL は偽）。
    fn matches(
        &mut self,
        scanned: &[Option<ScalarRef<'_>>],
        c: &DmlCandidate<'_>,
        row: &ResultRow,
    ) -> Result<bool, SqlSurfaceError> {
        for (target_column, source_pos, class) in &self.keys {
            let target_cell = match target_column {
                None => Cell::Integer(c.id),
                Some(i) => match scanned.get(*i) {
                    Some(Some(sr)) => key_cell_from_scalar(sr, class)?,
                    // NULL との等値は真にならない。
                    Some(None) => return Ok(false),
                    None => {
                        return Err(internal("condition key column missing from the target row"))
                    }
                },
            };
            let source_cell = row
                .cells
                .get(*source_pos)
                .ok_or_else(|| internal("source cell out of range"))?;
            if matches!(source_cell, Cell::Null) {
                return Ok(false);
            }
            let (mut a, mut b) = (Vec::new(), Vec::new());
            encode_key_component(&target_cell, class, &mut a)?;
            encode_key_component(source_cell, class, &mut b)?;
            if a != b {
                return Ok(false);
            }
        }
        if let Some(res) = self.residual.as_mut() {
            if !res.matches_row(scanned, c.id, row)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
