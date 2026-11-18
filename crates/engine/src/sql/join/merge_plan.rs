//! `MERGE` の実行計画と、組ごとの WHEN 選択・SET／INSERT 値の評価（SQL-79・Issue #3353・#3354。親 #3349、
//! 設計記録は `docs/design/sql-merge.md` の D3・D4・D5・§10）。
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
//! 押し下げ（D3）は行わない（#3353・#3354 とも）: `ON` の等値キー以外はすべて残余に残す。正しさが押し下げの
//! 有無に依存しないうえ、片側だけを参照する conjunct を押し下げると BY TARGET／BY SOURCE の「未一致」になる行
//! の集合が変わるため（最適化のみで対応しない。理由は `sql-merge.md` §10）。
//!
//! 句の種類（#3354）: 評価は種類ごとに分ける。`WHEN MATCHED` は対象行 ++ 元行のビュー（[`MergeEval::choose_matched`]）、
//! `WHEN NOT MATCHED BY SOURCE` は対象行だけ（元の列は空。[`MergeEval::choose_by_source`]）、
//! `WHEN NOT MATCHED [BY TARGET]` は元の行だけ（対象の列はすべて NULL の位置埋め。[`MergeEval::choose_by_target`]）。
//! 束縛段（`merge_bind`）が各句の可視範囲を片側に限っているため、反対側の列を参照する計画は存在しない。
//! INSERT 句の値（[`MergeEval::eval_insert`]）は `UPDATE SET` と同じ 4 種（列参照・リテラル・定数式・元の列を
//! 含む式）で、同じ [`SetPlan`] と評価関数を共有する（第 2 の評価器を作らない）。
//!
//! テナント境界（P0）: 対象側の候補（所有かつ可視）は呼び出し元が `tenant` の列挙で渡す。本モジュールは
//! 他テナントの行を引く経路を持たない。エラー文言は固定文言と入力された名前だけで、値・件数を含めない。

use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec::{ScalarRef, Value};
use crate::sql::allowlist::{MergeMatchKind, SqlSurfaceError, WherePredicate};
use crate::sql::exec::{Cell, ResultRow};
use crate::sql::merge_bind::{
    BoundAssignments, BoundMerge, BoundMergeAction, BoundMergeInsert, MergeIdValue,
};
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

/// `MERGE` の実行計画（`WHEN MATCHED`・`NOT MATCHED BY SOURCE`・`NOT MATCHED [BY TARGET]` の句を宣言順に持つ）。
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
    kind: MergeMatchKind,
    cond: Option<CondPlan>,
    action: ActionPlan,
}

#[derive(Debug, Clone)]
enum ActionPlan {
    Update(SetPlan),
    Delete,
    Insert(InsertPlan),
    DoNothing,
}

/// 1 句分の `INSERT`（BY TARGET 専用。値の 4 種は [`SetPlan`] を共有する）。
#[derive(Debug, Clone)]
struct InsertPlan {
    /// 列リストの値。`DEFAULT` の裸の指定・省略列は含まない（書き込み段が省略列として補完する）。
    set: SetPlan,
    /// 疑似列 `id` を省略したか（移行済み表。採番は書き込み段）。
    id_omitted: bool,
    /// 疑似列 `id` へ与える値（省略なら `None`）。
    id_value: Option<IdPlan>,
}

/// 疑似列 `id` へ与える値の出所。
#[derive(Debug, Clone)]
enum IdPlan {
    /// 非負整数のリテラル。
    Literal(u64),
    /// 元の列（`None` は元の疑似列 `id`）。
    Source(Option<usize>),
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

/// 束縛済みの `MERGE` を実行計画へ変換する。句の種類と操作の組（MATCHED・BY SOURCE は UPDATE／DELETE／
/// DO NOTHING、BY TARGET は INSERT／DO NOTHING）は構文段が保証済みだが、本関数も固定文言の `0A000` で
/// fail-closed にする（多層防御）。
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
        let kind = clause.kind();
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
        let by_target = kind == MergeMatchKind::NotMatchedByTarget;
        let action = match clause.action() {
            BoundMergeAction::Update(a) if !by_target => {
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
            BoundMergeAction::Delete if !by_target => ActionPlan::Delete,
            BoundMergeAction::DoNothing => ActionPlan::DoNothing,
            BoundMergeAction::Insert(ins) if by_target => {
                let plan = plan_insert(ins, target_schema, source_schema)?;
                for it in &plan.set.items {
                    for &i in &it.src_cols {
                        push_col(&mut columns, i);
                    }
                }
                for cp in &plan.set.copies {
                    push_col(&mut columns, cp.source_column);
                }
                match &plan.id_value {
                    Some(IdPlan::Source(Some(i))) => push_col(&mut columns, *i),
                    Some(IdPlan::Source(None)) => include_pseudo_id = true,
                    _ => {}
                }
                ActionPlan::Insert(plan)
            }
            _ => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "MERGE WHEN clause does not support this action",
                ));
            }
        };
        clauses.push(MergeClausePlan { kind, cond, action });
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

/// `INSERT` 句を [`InsertPlan`] へ写す。値の 4 種は `UPDATE SET` と同じ [`plan_set`] を通す（`BoundAssignments` が
/// 同じ形のため。元の疑似列 `id` を通常の列へ入れる形は `plan_set` が固定文言の `0A000` で拒否する）。
fn plan_insert(
    ins: &BoundMergeInsert,
    target_schema: &TableSchema,
    source_schema: &TableSchema,
) -> Result<InsertPlan, SqlSurfaceError> {
    let set = plan_set(ins.values(), target_schema, source_schema)?;
    let id_value = match ins.id_value() {
        None => None,
        Some(MergeIdValue::Literal(n)) => Some(IdPlan::Literal(*n)),
        Some(MergeIdValue::Source(r)) => {
            if r.relation() != 1 {
                return Err(internal("MERGE INSERT id must come from the source"));
            }
            Some(IdPlan::Source(slot_column(r.slot())))
        }
    };
    Ok(InsertPlan {
        set,
        id_omitted: ins.id_omitted(),
        id_value,
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
    /// `INSERT` の句が選ばれた（句の添字。BY TARGET だけ）。
    Insert(usize),
}

/// 実行時の評価器（残余の評価器・セル位置を解決済み）。計画 1 つにつき 1 つ作る。
pub(crate) struct MergeEval<'p> {
    plan: &'p MergeExecPlan,
    clauses: Vec<ClauseEval<'p>>,
    /// 対象表の列数（BY TARGET の評価ビューの対象側を NULL で埋める長さ）。
    target_columns: usize,
}

struct ClauseEval<'p> {
    cond: Option<CondEval>,
    set: Option<SetEval<'p>>,
    /// INSERT 句の `id` の出所の元セル位置（元の列・疑似列 `id` のとき）。
    id_pos: Option<usize>,
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
            let set_plan = match &c.action {
                ActionPlan::Update(sp) => Some(sp),
                ActionPlan::Insert(ip) => Some(&ip.set),
                _ => None,
            };
            let id_pos = match &c.action {
                ActionPlan::Insert(InsertPlan {
                    id_value: Some(IdPlan::Source(col)),
                    ..
                }) => Some(joined.source_cell_position(match col {
                    Some(i) => ColumnSlot::Column(*i),
                    None => ColumnSlot::Id,
                })?),
                _ => None,
            };
            let set = match set_plan {
                Some(sp) => {
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
                None => None,
            };
            clauses.push(ClauseEval { cond, set, id_pos });
        }
        Ok(Self {
            plan,
            clauses,
            target_columns: joined.target.schema.columns.len(),
        })
    }

    /// 対象行（`scanned`・`c`）と元の 1 行 `row` の組について、`WHEN MATCHED` の句だけを宣言順に評価し、
    /// 最初に真になった句の操作を返す（真になる句が無ければ [`Choice::None`]。D4）。
    pub(crate) fn choose_matched(
        &mut self,
        scanned: &[Option<ScalarRef<'_>>],
        c: &DmlCandidate<'_>,
        row: &ResultRow,
    ) -> Result<Choice, SqlSurfaceError> {
        self.choose_in(MergeMatchKind::Matched, scanned, c.id, row)
    }

    /// 元に一致する行が無かった対象行（`scanned`・`c`）について、`WHEN NOT MATCHED BY SOURCE` の句だけを
    /// 宣言順に評価する（#3354）。条件は対象の列だけを参照できる（束縛段が保証）ため、元の行は空で評価する。
    pub(crate) fn choose_by_source(
        &mut self,
        scanned: &[Option<ScalarRef<'_>>],
        c: &DmlCandidate<'_>,
    ) -> Result<Choice, SqlSurfaceError> {
        let empty = ResultRow {
            id: 0,
            score: 0.0,
            cells: Vec::new(),
        };
        self.choose_in(MergeMatchKind::NotMatchedBySource, scanned, c.id, &empty)
    }

    /// 対象に一致する行が無かった元の 1 行 `row` について、`WHEN NOT MATCHED [BY TARGET]` の句だけを宣言順に
    /// 評価する（#3354）。条件は元の列だけを参照できる（束縛段が保証）ため、対象側は全列 NULL で評価する。
    pub(crate) fn choose_by_target(&mut self, row: &ResultRow) -> Result<Choice, SqlSurfaceError> {
        let scanned: Vec<Option<ScalarRef<'_>>> = vec![None; self.target_columns];
        self.choose_in(MergeMatchKind::NotMatchedByTarget, &scanned, 0, row)
    }

    /// `kind` の句だけを宣言順に評価して、最初に条件が真になった句の操作を返す。
    fn choose_in(
        &mut self,
        kind: MergeMatchKind,
        scanned: &[Option<ScalarRef<'_>>],
        id: u64,
        row: &ResultRow,
    ) -> Result<Choice, SqlSurfaceError> {
        for (i, clause) in self.clauses.iter_mut().enumerate() {
            let cp = self
                .plan
                .clauses
                .get(i)
                .ok_or_else(|| internal("MERGE clause out of range"))?;
            if cp.kind != kind {
                continue;
            }
            let hit = match clause.cond.as_mut() {
                None => true,
                Some(cond) => cond.matches(scanned, id, row)?,
            };
            if !hit {
                continue;
            }
            return Ok(match &cp.action {
                ActionPlan::Update(_) => Choice::Update(i),
                ActionPlan::Delete => Choice::Delete,
                ActionPlan::Insert(_) => Choice::Insert(i),
                ActionPlan::DoNothing => Choice::DoNothing,
            });
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
        self.eval_set(
            clause,
            scanned,
            c.id,
            c.embedding,
            row,
            target_schema,
            charge,
        )
    }

    /// 句 `clause`（BY TARGET の `INSERT`）の挿入行を元の 1 行 `row` から評価する（#3354）。戻り値は
    /// `(疑似列 id, 列ごとの値〔未指定は NULL の仮値〕, 列ごとの「値が指定されたか」)`。省略列の `DEFAULT` 補完と
    /// 採番の印は書き込み段へ渡す前に `sql::insert_select::finish_bound_insert` が行う。疑似列 `id` が元の列なら
    /// `cell_to_row_id` と同じ規則（NULL・負数は `22000`）で求める。保持する値は `charge` で共有予算へ計上する。
    pub(crate) fn eval_insert(
        &self,
        clause: usize,
        row: &ResultRow,
        target_schema: &TableSchema,
        charge: &mut dyn FnMut(usize) -> Result<(), SqlSurfaceError>,
    ) -> Result<(u64, Vec<Value>, Vec<bool>), SqlSurfaceError> {
        let ip = match self.plan.clauses.get(clause).map(|c| &c.action) {
            Some(ActionPlan::Insert(ip)) => ip,
            _ => return Err(internal("MERGE clause is not an INSERT")),
        };
        let ce = self
            .clauses
            .get(clause)
            .ok_or_else(|| internal("MERGE clause out of range"))?;
        let scanned: Vec<Option<ScalarRef<'_>>> = vec![None; self.target_columns];
        let pairs = self.eval_set(clause, &scanned, 0, &[], row, target_schema, charge)?;
        let ncols = target_schema.columns.len();
        charge(
            std::mem::size_of::<Value>()
                .saturating_add(1)
                .saturating_mul(ncols),
        )?;
        let mut values: Vec<Value> = Vec::new();
        values.try_reserve_exact(ncols).map_err(|_| {
            SqlSurfaceError::payload_too_large("MERGE INSERT values exceed the limit")
        })?;
        values.resize(ncols, Value::Null);
        let mut provided = vec![false; ncols];
        for (idx, v) in pairs {
            if let (Some(slot), Some(flag)) = (values.get_mut(idx), provided.get_mut(idx)) {
                *slot = v;
                *flag = true;
            } else {
                return Err(internal("MERGE INSERT column out of range"));
            }
        }
        let id = match &ip.id_value {
            None => {
                if ip.id_omitted || !target_schema.has_pseudo_id() {
                    0
                } else {
                    return Err(internal("MERGE INSERT id is missing"));
                }
            }
            Some(IdPlan::Literal(n)) => *n,
            Some(IdPlan::Source(_)) => {
                let pos = ce
                    .id_pos
                    .ok_or_else(|| internal("MERGE INSERT id position is missing"))?;
                let cell = row
                    .cells
                    .get(pos)
                    .ok_or_else(|| internal("source cell out of range"))?;
                crate::sql::insert_select::cell_to_row_id(cell.clone())?
            }
        };
        Ok((id, values, provided))
    }

    /// 句 `clause` の代入群（`UPDATE SET` と `INSERT` の値で共通）を `(列, 値)` の列へ評価する。ビューは
    /// 「`scanned` ++ 項ごとの元の列」。保持する値のバイト数は `charge` で共有予算へ計上する（超過は `54000`）。
    /// NULL を NOT NULL 列へ入れる組は `23502`。
    #[allow(clippy::too_many_arguments)]
    fn eval_set(
        &self,
        clause: usize,
        scanned: &[Option<ScalarRef<'_>>],
        id: u64,
        embedding: &[f32],
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
                crate::sql::set_expr::eval_set_expr_to_value(&it.set, id, embedding, &view)?;
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
        id: u64,
        row: &ResultRow,
    ) -> Result<bool, SqlSurfaceError> {
        for (target_column, source_pos, class) in &self.keys {
            let target_cell = match target_column {
                None => Cell::Integer(id),
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
            if !res.matches_row(scanned, id, row)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
