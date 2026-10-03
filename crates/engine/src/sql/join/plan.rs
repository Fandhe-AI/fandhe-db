//! JOIN の束縛（実行計画の組み立て。Issue #925・#926・#1190、SQL-28・RLS-10、
//! TASK-212）。
//!
//! 責務境界: `sql::allowlist::ValidatedJoin`（構造検証済み）を、スキーマに対して
//! 列解決・型検証・外部結合の簡約・WHERE のプッシュダウン／残余の分離・側スキャン
//! 投影の確定まで行い、[`JoinPlan`] を返す。走査・結合・集計といった行の処理は
//! `sql::join::exec`／`residual`／`aggregate` の責務で、本モジュールは一切の行を
//! 読まない（Describe＝`sql::join::describe_columns` もこの計画だけを使う）。
//!
//! WHERE の意味論（`docs/design/multi-way-join.md`）:
//! - WHERE を最上位の `AND` で conjunct に分解し、各 conjunct の「参照 relation 集合」と
//!   「strict な relation 集合」（その relation が NULL 補完されているとき必ず偽に
//!   なる relation）を求める。受理する葉はすべて NULL 入力で偽になる（strict）ため、
//!   葉の strict 集合は参照集合と一致し、`AND` は和集合・`OR` は共通集合になる
//!   （relation を跨ぐ `OR` は strict ではない）。
//! - strict な relation を NULL 補完しうる結合段は保存側フラグを落とす（外部結合の
//!   簡約。落ちる行は必ずその述語で偽になるため結果は変わらない）。
//! - 単一 relation で完結し列同士の比較を含まない conjunct は、その relation の
//!   走査へプッシュダウンする（型解析・評価は単一テーブル経路を完全に共有する）。
//!   それ以外は結合後に行単位で評価する残余（[`ResNode`]）になる。

use std::collections::HashMap;

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{
    JoinAggregate, JoinKind, JoinProjection, JoinSelectItem, JoinWhereExpr, JoinWherePredicate,
    Projection, SqlSurfaceError, ValidatedJoin, ValidatedScan, WherePredicate,
};
use crate::sql::exec::ColumnMeta;
use crate::sql::parser::{AggregateInput, AggregateTarget, BoundAggregateItem};
use crate::sql::relation::{BindingScope, ColumnRef, ColumnSlot, TableRef};
use crate::sql::udf_call::{BinOp, UdfRegistry};

use super::values::{cmp_class, key_class, CmpClass, JoinKeyClass};

/// 解決済みの列参照（どの relation の、側スキャン結果の何番目のセルか）。
#[derive(Debug, Clone)]
pub(super) struct Col {
    pub(super) rel: usize,
    /// 側スキャン結果の行 `cells` 内の位置。
    pub(super) pos: usize,
    /// 列型（疑似列 `id` は `None`）。
    pub(super) ty: Option<ColumnType>,
    /// スキーマ列名（疑似列は `"id"`）。
    pub(super) name: String,
    /// `TableSchema::columns` の添字（疑似列は `None`）。
    pub(super) schema_idx: Option<usize>,
}

/// 結合キー 1 成分（これまでの結合結果側の列と、新しく加わる relation 側の列）。
pub(super) struct StepKey {
    pub(super) acc_rel: usize,
    pub(super) acc_pos: usize,
    pub(super) new_pos: usize,
    pub(super) class: JoinKeyClass,
}

/// left-deep 連鎖の 1 段の実行計画。
pub(super) struct StepPlan {
    /// これまでの結合結果側の未一致行を NULL 補完して残すか（外部結合の簡約後）。
    pub(super) preserve_acc: bool,
    /// 新しく加わる relation 側の未一致行を NULL 補完して残すか（同上）。
    pub(super) preserve_new: bool,
    pub(super) keys: Vec<StepKey>,
}

/// 結合後に行単位で評価する WHERE の残余ノード。
pub(super) enum ResNode {
    And(Vec<ResNode>),
    Or(Vec<ResNode>),
    /// 単一 relation で完結する部分木（束縛済みの 1 分岐 `OR` 群として保持）。
    Single(SingleFilter),
    /// 列同士の比較。
    Compare(ColCompare),
}

pub(super) struct SingleFilter {
    pub(super) rel: usize,
    pub(super) group: crate::sql::where_tree::BoundOrGroup,
    /// 述語が参照するスキーマ列（添字・側スキャン結果内の位置・列型）。
    pub(super) cols: Vec<(usize, usize, ColumnType)>,
    pub(super) schema_len: usize,
}

pub(super) struct ColCompare {
    pub(super) lhs: (usize, usize),
    pub(super) rhs: (usize, usize),
    pub(super) op: BinOp,
    pub(super) class: CmpClass,
}

/// 出力列 1 個（どの relation の、走査結果の何番目のセルかという位置つき）。
pub(super) struct OutputColumn {
    pub(super) rel: usize,
    pub(super) pos: usize,
    pub(super) meta: ColumnMeta,
}

/// 非集計形のスカラー `ORDER BY` の 1 キー。
pub(super) struct PlainOrder {
    pub(super) rel: usize,
    pub(super) pos: usize,
    pub(super) class: CmpClass,
    pub(super) descending: bool,
}

pub(super) enum Shape {
    Plain {
        output: Vec<OutputColumn>,
        order: Vec<PlainOrder>,
    },
    Aggregate(AggPlan),
}

/// 集計形の `GROUP BY` キー 1 列。
pub(super) struct KeyPlan {
    pub(super) rel: usize,
    pub(super) pos: usize,
    pub(super) key_class: JoinKeyClass,
    pub(super) cmp_class: CmpClass,
    pub(super) meta: ColumnMeta,
}

/// 集計項目 1 つ。`rel == None` は `COUNT(*)`。
pub(super) struct AggItemPlan {
    pub(super) item: BoundAggregateItem,
    pub(super) rel: Option<usize>,
    /// `scanned` へ値を置くスキーマ列添字と、側スキャン結果内の位置・列型
    /// （`IdU64`・`AllVisible` では `None`）。
    pub(super) column: Option<(usize, usize, ColumnType)>,
}

#[derive(Clone, Copy)]
pub(super) enum AggOut {
    Key(usize),
    Item(usize),
}

#[derive(Clone, Copy)]
pub(super) enum AggOrderTarget {
    Key(usize),
    Item(usize),
}

pub(super) struct AggOrder {
    pub(super) target: AggOrderTarget,
    pub(super) descending: bool,
}

pub(super) struct AggPlan {
    pub(super) keys: Vec<KeyPlan>,
    pub(super) items: Vec<AggItemPlan>,
    pub(super) out: Vec<AggOut>,
    pub(super) metas: Vec<ColumnMeta>,
    /// `HAVING`: `(items の添字, 演算子, リテラル)`。
    pub(super) having: Vec<(usize, BinOp, f64)>,
    pub(super) order: Vec<AggOrder>,
}

/// 束縛済みの JOIN 実行計画。[`crate::sql::join::execute_with_limits`]・
/// [`crate::sql::join::describe_columns`] が共有する（第 2 の束縛経路を作らない）。
pub(super) struct JoinPlan<'a> {
    pub(super) schemas: Vec<&'a TableSchema>,
    /// relation ごとの側スキャン（投影・プッシュダウン済み述語つき）。
    pub(super) scans: Vec<ValidatedScan>,
    pub(super) steps: Vec<StepPlan>,
    pub(super) residual: Vec<ResNode>,
    pub(super) shape: Shape,
}

impl JoinPlan<'_> {
    /// 結果列メタデータ（Describe と Execute が共有する）。
    pub(super) fn column_metas(&self) -> Vec<ColumnMeta> {
        match &self.shape {
            Shape::Plain { output, .. } => output.iter().map(|c| c.meta.clone()).collect(),
            Shape::Aggregate(agg) => agg.metas.clone(),
        }
    }
}

/// 解決済み列参照を、そのテーブルの列名（疑似列 `id` を含む）へ写像する。
fn slot_name(slot: ColumnSlot, schema: &TableSchema) -> Result<String, SqlSurfaceError> {
    match slot {
        ColumnSlot::Id => Ok("id".to_string()),
        ColumnSlot::Column(idx) => {
            schema
                .columns
                .get(idx)
                .map(|c| c.name.clone())
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN column slot index out of range".to_string(),
                })
        }
    }
}

fn push_dedup(v: &mut Vec<String>, name: &str) {
    if !v.iter().any(|s| s == name) {
        v.push(name.to_string());
    }
}

/// 1 辺の走査投影。`All` は疑似列 `id`（位置 0）＋スキーマ列順（位置 `1 + index`）
/// という [`crate::sql::parser::bind_projection`] の `Projection::All` 展開規則と
/// 一致させる（第 2 の展開規則を作らない）。`Columns` は要求された順に追記する
/// （位置は追記後も変わらない）。
pub(super) enum SideProjection {
    All,
    Columns(Vec<String>),
}

impl SideProjection {
    pub(super) fn position(&self, name: &str, schema: &TableSchema) -> Option<usize> {
        match self {
            // `bind_projection`（`Projection::All`）・`BindingScope::
            // resolve_in_relation` はいずれも実カラムを疑似列 `id` より優先して
            // 照合する（スキーマが `id` という実カラムを持つ場合、その値を
            // 指す）。ここで疑似列を先に判定すると、その規則と矛盾する誤った
            // 位置（実カラム `id` の値ではなく行キー）を返してしまう
            // （回帰: `tests::star_projection_key_position_prefers_real_id_column_over_pseudo_column`）。
            SideProjection::All => schema
                .columns
                .iter()
                .position(|c| c.name == name)
                .map(|i| i + 1)
                .or(if name == "id" { Some(0) } else { None }),
            SideProjection::Columns(v) => v.iter().position(|s| s == name),
        }
    }

    /// `name` を投影へ含め（未含なら追記し）、その位置を返す。
    fn ensure(&mut self, name: &str, schema: &TableSchema) -> Result<usize, SqlSurfaceError> {
        if let SideProjection::Columns(v) = self {
            push_dedup(v, name);
        }
        self.position(name, schema)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN column missing from side projection".to_string(),
            })
    }

    pub(super) fn to_projection(&self) -> Projection {
        match self {
            SideProjection::All => Projection::All,
            SideProjection::Columns(v) => Projection::Columns(v.clone()),
        }
    }
}

/// 束縛の作業用コンテキスト。
struct Binder<'a, 'b> {
    schemas: Vec<&'a TableSchema>,
    relations: &'b [TableRef],
    scope: BindingScope<'a>,
    sides: Vec<SideProjection>,
}

impl<'a, 'b> Binder<'a, 'b> {
    /// 列参照を解決し、側投影へ含めた [`Col`] を返す。
    fn col(&mut self, colref: &ColumnRef) -> Result<Col, SqlSurfaceError> {
        let resolved = self.scope.resolve(colref)?;
        self.col_from(resolved.relation(), resolved.slot(), resolved.column_type())
    }

    fn col_from(
        &mut self,
        rel: usize,
        slot: ColumnSlot,
        ty: Option<&ColumnType>,
    ) -> Result<Col, SqlSurfaceError> {
        let schema = *self
            .schemas
            .get(rel)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN relation index out of range".to_string(),
            })?;
        let name = slot_name(slot, schema)?;
        let side = self
            .sides
            .get_mut(rel)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN side projection index out of range".to_string(),
            })?;
        let pos = side.ensure(&name, schema)?;
        Ok(Col {
            rel,
            pos,
            ty: ty.cloned(),
            name,
            schema_idx: match slot {
                ColumnSlot::Id => None,
                ColumnSlot::Column(i) => Some(i),
            },
        })
    }

    fn meta(col: &Col) -> ColumnMeta {
        match (&col.ty, col.schema_idx) {
            (Some(ty), Some(_)) => ColumnMeta::Scalar {
                name: col.name.clone(),
                ty: ty.clone(),
            },
            _ => ColumnMeta::Id,
        }
    }

    /// 別名付き投影・GROUP BY キーの出力メタデータ。`Scalar` は名前を別名へ置換し、
    /// 疑似列 `id`（`ColumnMeta::Id` は名前を持てない）は numeric 静的型の
    /// `Computed` に載せ替えて指定名を公告する（wire 上の型 OID は `Id` と同じ numeric。
    /// 集計結果 `MIN(id)` 等の写像と同じ。Issue #1190 PR #1234 指摘）。
    fn meta_with_alias(base: ColumnMeta, alias: Option<&String>) -> ColumnMeta {
        let Some(a) = alias else { return base };
        match base {
            ColumnMeta::Scalar { ty, .. } => ColumnMeta::Scalar {
                name: a.clone(),
                ty,
            },
            ColumnMeta::Id => ColumnMeta::Computed {
                name: a.clone(),
                ty: Some(ColumnType::Numeric {
                    precision: 20,
                    scale: 0,
                }),
            },
            ColumnMeta::Computed { ty, .. } => ColumnMeta::Computed {
                name: a.clone(),
                ty,
            },
        }
    }
}

/// [`JoinWherePredicate`] の葉が参照する列参照。
fn leaf_colrefs(pred: &JoinWherePredicate) -> Vec<&ColumnRef> {
    match pred {
        JoinWherePredicate::Equality { column, .. }
        | JoinWherePredicate::Prefix { column, .. }
        | JoinWherePredicate::Compare { column, .. }
        | JoinWherePredicate::BoolEquality { column, .. }
        | JoinWherePredicate::BoolColumn { column }
        | JoinWherePredicate::InList { column, .. } => vec![column],
        JoinWherePredicate::ColumnCompare { lhs, rhs, .. } => vec![lhs, rhs],
    }
}

/// 述語が `NULL` に対して常に偽（strict）になるかを判定する（`docs/design/
/// outer-join.md`・`docs/design/multi-way-join.md` の簡約規則の前提）。現行で受理する
/// 全 variant は strict だが、ワイルドカード無しの網羅的 `match` にすることで、将来
/// `IS NULL` 等の非 strict な variant を追加した際にコンパイルエラーで検出させる
/// （簡約規則が黙って壊れるのを防ぐ）。
pub(super) fn is_null_rejecting(pred: &JoinWherePredicate) -> bool {
    match pred {
        JoinWherePredicate::Equality { .. }
        | JoinWherePredicate::Prefix { .. }
        | JoinWherePredicate::Compare { .. }
        | JoinWherePredicate::BoolEquality { .. }
        | JoinWherePredicate::BoolColumn { .. }
        | JoinWherePredicate::InList { .. }
        | JoinWherePredicate::ColumnCompare { .. } => true,
    }
}

/// relation 番号に対応するビット。32 relation 以上は表現できないため `Internal`
/// （通常は `MAX_TABLE_REFS` で先に弾かれる。0 へ倒すと述語の参照集合が空になる）。
fn relation_bit(rel: usize) -> Result<u32, SqlSurfaceError> {
    u32::try_from(rel)
        .ok()
        .and_then(|s| 1u32.checked_shl(s))
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN relation index exceeds mask width".to_string(),
        })
}

fn leaf_mask(pred: &JoinWherePredicate, scope: &BindingScope<'_>) -> Result<u32, SqlSurfaceError> {
    let mut mask = 0u32;
    for c in leaf_colrefs(pred) {
        let rel = scope.resolve(c)?.relation();
        mask |= relation_bit(rel)?;
    }
    Ok(mask)
}

/// 式が参照する relation 集合（ビットマスク）。
fn refs_mask(expr: &JoinWhereExpr, scope: &BindingScope<'_>) -> Result<u32, SqlSurfaceError> {
    match expr {
        JoinWhereExpr::Leaf(p) => leaf_mask(p, scope),
        JoinWhereExpr::And(ch) | JoinWhereExpr::Or(ch) => {
            let mut m = 0;
            for c in ch {
                m |= refs_mask(c, scope)?;
            }
            Ok(m)
        }
    }
}

/// 式が strict（その relation が NULL 補完されていれば偽）になる relation 集合。
/// 葉は参照集合（受理する全葉が strict）、`AND` は和集合、`OR` は共通集合。
fn strict_mask(expr: &JoinWhereExpr, scope: &BindingScope<'_>) -> Result<u32, SqlSurfaceError> {
    match expr {
        JoinWhereExpr::Leaf(p) => {
            if is_null_rejecting(p) {
                leaf_mask(p, scope)
            } else {
                Ok(0)
            }
        }
        JoinWhereExpr::And(ch) => {
            let mut m = 0;
            for c in ch {
                m |= strict_mask(c, scope)?;
            }
            Ok(m)
        }
        JoinWhereExpr::Or(ch) => {
            let mut m = u32::MAX;
            for c in ch {
                m &= strict_mask(c, scope)?;
            }
            Ok(if ch.is_empty() { 0 } else { m })
        }
    }
}

fn has_compare(expr: &JoinWhereExpr) -> bool {
    match expr {
        JoinWhereExpr::Leaf(p) => matches!(p, JoinWherePredicate::ColumnCompare { .. }),
        JoinWhereExpr::And(ch) | JoinWhereExpr::Or(ch) => ch.iter().any(has_compare),
    }
}

/// 最上位の `AND` を平坦化して conjunct を集める。
fn flatten_conjuncts<'e>(expr: &'e JoinWhereExpr, out: &mut Vec<&'e JoinWhereExpr>) {
    match expr {
        JoinWhereExpr::And(ch) => {
            for c in ch {
                flatten_conjuncts(c, out);
            }
        }
        other => out.push(other),
    }
}

fn leaf_to_where(
    pred: &JoinWherePredicate,
    name: String,
) -> Result<WherePredicate, SqlSurfaceError> {
    Ok(match pred {
        JoinWherePredicate::Equality { value, .. } => WherePredicate::Equality {
            column: name,
            value: value.clone(),
        },
        JoinWherePredicate::Prefix { pattern, .. } => WherePredicate::Prefix {
            column: name,
            pattern: pattern.clone(),
        },
        JoinWherePredicate::Compare { op, value, .. } => WherePredicate::Compare {
            column: name,
            op: *op,
            value: value.clone(),
        },
        JoinWherePredicate::BoolEquality { value, .. } => WherePredicate::BoolEquality {
            column: name,
            value: *value,
        },
        JoinWherePredicate::BoolColumn { .. } => WherePredicate::BoolColumn { column: name },
        JoinWherePredicate::InList { values, .. } => WherePredicate::InList {
            column: name,
            values: values.clone(),
        },
        JoinWherePredicate::ColumnCompare { .. } => {
            return Err(SqlSurfaceError::Internal {
                detail: "column comparison cannot be converted to a scan predicate".to_string(),
            })
        }
    })
}

/// 単一 relation で完結し列同士の比較を含まない式を、非修飾の [`WherePredicate`] 列へ
/// 変換する（`OR` は分岐ごとの `Vec` を持つ [`WherePredicate::Or`]）。
fn to_where_predicates(
    expr: &JoinWhereExpr,
    scope: &BindingScope<'_>,
    schemas: &[&TableSchema],
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    match expr {
        JoinWhereExpr::Leaf(pred) => {
            let colref =
                leaf_colrefs(pred)
                    .first()
                    .copied()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN WHERE leaf without a column".to_string(),
                    })?;
            let resolved = scope.resolve(colref)?;
            let schema =
                schemas
                    .get(resolved.relation())
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN relation index out of range".to_string(),
                    })?;
            let name = slot_name(resolved.slot(), schema)?;
            Ok(vec![leaf_to_where(pred, name)?])
        }
        JoinWhereExpr::And(ch) => {
            let mut out = Vec::new();
            for c in ch {
                out.extend(to_where_predicates(c, scope, schemas)?);
            }
            Ok(out)
        }
        JoinWhereExpr::Or(ch) => {
            let mut branches = Vec::with_capacity(ch.len());
            for c in ch {
                branches.push(to_where_predicates(c, scope, schemas)?);
            }
            Ok(vec![WherePredicate::Or(branches)])
        }
    }
}

fn collect_leaf_colrefs<'e>(expr: &'e JoinWhereExpr, out: &mut Vec<&'e ColumnRef>) {
    match expr {
        JoinWhereExpr::Leaf(p) => out.extend(leaf_colrefs(p)),
        JoinWhereExpr::And(ch) | JoinWhereExpr::Or(ch) => {
            for c in ch {
                collect_leaf_colrefs(c, out);
            }
        }
    }
}

/// 残余ノードを組み立てる。
fn build_residual(
    expr: &JoinWhereExpr,
    binder: &mut Binder<'_, '_>,
    udfs: &UdfRegistry,
) -> Result<ResNode, SqlSurfaceError> {
    let mask = refs_mask(expr, &binder.scope)?;
    if mask.count_ones() == 1 && !has_compare(expr) {
        let rel =
            usize::try_from(mask.trailing_zeros()).map_err(|_| SqlSurfaceError::Internal {
                detail: "JOIN relation mask overflow".to_string(),
            })?;
        let preds = to_where_predicates(expr, &binder.scope, &binder.schemas)?;
        let wrapped = vec![WherePredicate::Or(vec![preds])];
        let schema = *binder
            .schemas
            .get(rel)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN relation index out of range".to_string(),
            })?;
        let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let (metadata, expr_filters, _rls, mut or_filters) =
            crate::sql::parser::bind_where_predicates(
                &wrapped,
                schema,
                udfs,
                &mut node_budget,
                &[],
            )?;
        if !metadata.is_empty() || !expr_filters.is_empty() || or_filters.len() != 1 {
            return Err(SqlSurfaceError::Internal {
                detail: "JOIN residual predicate did not bind to a single OR group".to_string(),
            });
        }
        let group = or_filters.pop().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN residual OR group missing".to_string(),
        })?;
        if group.references_embedding() {
            return Err(SqlSurfaceError::unsupported(
                "JOIN WHERE predicate cannot reference a VECTOR column",
            ));
        }
        let mut refs = Vec::new();
        collect_leaf_colrefs(expr, &mut refs);
        let mut cols: Vec<(usize, usize, ColumnType)> = Vec::new();
        for r in refs {
            let c = binder.col(r)?;
            if let (Some(idx), Some(ty)) = (c.schema_idx, c.ty) {
                if !cols.iter().any(|(i, _, _)| *i == idx) {
                    cols.push((idx, c.pos, ty));
                }
            }
        }
        return Ok(ResNode::Single(SingleFilter {
            rel,
            group,
            cols,
            schema_len: schema.columns.len(),
        }));
    }
    match expr {
        JoinWhereExpr::And(ch) => Ok(ResNode::And(
            ch.iter()
                .map(|c| build_residual(c, binder, udfs))
                .collect::<Result<_, _>>()?,
        )),
        JoinWhereExpr::Or(ch) => Ok(ResNode::Or(
            ch.iter()
                .map(|c| build_residual(c, binder, udfs))
                .collect::<Result<_, _>>()?,
        )),
        JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare { lhs, op, rhs }) => {
            let l = binder.col(lhs)?;
            let r = binder.col(rhs)?;
            let lc = cmp_class(l.ty.as_ref()).ok_or_else(|| {
                SqlSurfaceError::unsupported("this column type cannot be compared in JOIN WHERE")
            })?;
            let rc = cmp_class(r.ty.as_ref()).ok_or_else(|| {
                SqlSurfaceError::unsupported("this column type cannot be compared in JOIN WHERE")
            })?;
            if lc != rc {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "JOIN WHERE comparison type mismatch between the two columns",
                ));
            }
            Ok(ResNode::Compare(ColCompare {
                lhs: (l.rel, l.pos),
                rhs: (r.rel, r.pos),
                op: *op,
                class: lc,
            }))
        }
        JoinWhereExpr::Leaf(_) => Err(SqlSurfaceError::Internal {
            detail: "multi-relation JOIN WHERE leaf other than a column comparison".to_string(),
        }),
    }
}

/// 集計項目の実効名（`AS` 指定値、無ければ関数名小文字）。
fn item_effective_name(
    func: crate::sql::allowlist::AggregateFunc,
    alias: &Option<String>,
) -> String {
    alias
        .clone()
        .unwrap_or_else(|| func.default_alias().to_string())
}

/// 結合連鎖の束縛（[`JoinPlan`] を返す）。列解決・結合キー型検証・外部結合簡約・
/// WHERE 分離・投影展開・並べ替え／集計の型検証をすべて行走査より前に確定させる。
pub(super) fn build_plan<'a>(
    schemas: &'a HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
) -> Result<JoinPlan<'a>, SqlSurfaceError> {
    let relations = validated.relations.as_slice();
    if relations.len() < 2
        || relations.len() > crate::sql::relation::MAX_TABLE_REFS
        || validated.steps.len() + 1 != relations.len()
    {
        return Err(SqlSurfaceError::Internal {
            detail: "JOIN relation/step count is inconsistent".to_string(),
        });
    }
    let mut rel_schemas: Vec<&'a TableSchema> = Vec::with_capacity(relations.len());
    for r in relations {
        rel_schemas.push(
            schemas
                .get(r.table())
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "schema missing for JOIN relation".to_string(),
                })?,
        );
    }
    let scope = BindingScope::new(
        relations
            .iter()
            .cloned()
            .zip(rel_schemas.iter().copied())
            .collect(),
    )?;
    let all_star =
        validated.aggregate.is_none() && matches!(validated.projection, JoinProjection::All);
    let sides: Vec<SideProjection> = relations
        .iter()
        .map(|_| {
            if all_star {
                SideProjection::All
            } else {
                SideProjection::Columns(Vec::new())
            }
        })
        .collect();
    let mut b = Binder {
        schemas: rel_schemas,
        relations,
        scope,
        sides,
    };

    // 結合段（ON）。段 k のスコープは relation 0..=k+1 だけで構築する——未出現の
    // relation を前方参照すると `BindingScope::resolve` が `42P01` で拒否する
    // （PostgreSQL と同じ分類）。
    let mut steps: Vec<StepPlan> = Vec::with_capacity(validated.steps.len());
    for (k, step) in validated.steps.iter().enumerate() {
        let upto = k + 2;
        let step_scope = BindingScope::new(
            b.relations
                .iter()
                .take(upto)
                .cloned()
                .zip(b.schemas.iter().take(upto).copied())
                .collect(),
        )?;
        let mut keys: Vec<StepKey> = Vec::with_capacity(step.on.len());
        for (lhs, rhs) in &step.on {
            let l = step_scope.resolve(lhs)?;
            let r = step_scope.resolve(rhs)?;
            let new_rel = k + 1;
            let (acc, new) = match (l.relation() == new_rel, r.relation() == new_rel) {
                (false, true) => (l, r),
                (true, false) => (r, l),
                _ => {
                    return Err(SqlSurfaceError::unsupported(
                        "JOIN ON condition must reference both sides of the join",
                    ))
                }
            };
            let acc_class = key_class(acc.column_type())?;
            let new_class = key_class(new.column_type())?;
            if acc_class != new_class {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "JOIN key type mismatch between the two sides",
                ));
            }
            let acc_col = b.col_from(acc.relation(), acc.slot(), acc.column_type())?;
            let new_col = b.col_from(new.relation(), new.slot(), new.column_type())?;
            keys.push(StepKey {
                acc_rel: acc_col.rel,
                acc_pos: acc_col.pos,
                new_pos: new_col.pos,
                class: acc_class,
            });
        }
        steps.push(StepPlan {
            preserve_acc: matches!(step.kind, JoinKind::Left | JoinKind::Full),
            preserve_new: matches!(step.kind, JoinKind::Right | JoinKind::Full),
            keys,
        });
    }

    // WHERE: conjunct ごとに外部結合を簡約し、プッシュダウンか残余へ振り分ける。
    let mut pushdown: Vec<Vec<WherePredicate>> = relations.iter().map(|_| Vec::new()).collect();
    let mut residual: Vec<ResNode> = Vec::new();
    if let Some(where_clause) = &validated.where_clause {
        let mut conjuncts: Vec<&JoinWhereExpr> = Vec::new();
        flatten_conjuncts(where_clause, &mut conjuncts);
        // 列参照エラー（未知列・曖昧列・未知修飾子）を、振り分けより前にすべて確定させる。
        for c in &conjuncts {
            refs_mask(c, &b.scope)?;
        }
        for c in conjuncts {
            let strict = strict_mask(c, &b.scope)?;
            for r in 0..relations.len() {
                let bit = relation_bit(r)?;
                if strict & bit == 0 {
                    continue;
                }
                for (j, step) in steps.iter_mut().enumerate() {
                    if r == j + 1 {
                        step.preserve_acc = false;
                    } else if r <= j {
                        step.preserve_new = false;
                    }
                }
            }
            let mask = refs_mask(c, &b.scope)?;
            if mask.count_ones() == 1 && !has_compare(c) {
                let rel = usize::try_from(mask.trailing_zeros()).map_err(|_| {
                    SqlSurfaceError::Internal {
                        detail: "JOIN relation mask overflow".to_string(),
                    }
                })?;
                let preds = to_where_predicates(c, &b.scope, &b.schemas)?;
                if let Some(slot) = pushdown.get_mut(rel) {
                    slot.extend(preds);
                }
            } else {
                residual.push(build_residual(c, &mut b, udfs)?);
            }
        }
    }

    let shape = match &validated.aggregate {
        None => build_plain_shape(validated, &mut b)?,
        Some(agg) => Shape::Aggregate(build_aggregate_shape(validated, agg, &mut b)?),
    };

    let mut scans: Vec<ValidatedScan> = Vec::with_capacity(relations.len());
    for (i, r) in relations.iter().enumerate() {
        let side = b.sides.get(i).ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN side projection missing".to_string(),
        })?;
        scans.push(ValidatedScan {
            table_name: r.table().to_string(),
            projection: side.to_projection(),
            where_predicates: pushdown.get_mut(i).map(std::mem::take).unwrap_or_default(),
            limit: 1,
            order_by: Vec::new(),
            offset: 0,
            window_items: Vec::new(),
            order_keys: Vec::new(),
            scalar_subquery_items: Vec::new(),
        });
    }

    Ok(JoinPlan {
        schemas: b.schemas,
        scans,
        steps,
        residual,
        shape,
    })
}

fn build_plain_shape(
    validated: &ValidatedJoin,
    b: &mut Binder<'_, '_>,
) -> Result<Shape, SqlSurfaceError> {
    let mut output: Vec<OutputColumn> = Vec::new();
    match &validated.projection {
        JoinProjection::All => {
            for rel in 0..b.schemas.len() {
                let schema = *b
                    .schemas
                    .get(rel)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN relation index out of range".to_string(),
                    })?;
                output.push(OutputColumn {
                    rel,
                    pos: 0,
                    meta: ColumnMeta::Id,
                });
                for (idx, col) in schema.columns.iter().enumerate() {
                    output.push(OutputColumn {
                        rel,
                        pos: idx + 1,
                        meta: ColumnMeta::Scalar {
                            name: col.name.clone(),
                            ty: col.ty.clone(),
                        },
                    });
                }
            }
        }
        JoinProjection::Columns(colrefs) => {
            for (i, colref) in colrefs.iter().enumerate() {
                let c = b.col(colref)?;
                let alias = validated.column_aliases.get(i).and_then(Option::as_ref);
                let meta = Binder::meta_with_alias(Binder::meta(&c), alias);
                output.push(OutputColumn {
                    rel: c.rel,
                    pos: c.pos,
                    meta,
                });
            }
        }
    }
    let mut order: Vec<PlainOrder> = Vec::with_capacity(validated.order_by.len());
    for key in &validated.order_by {
        // 非修飾の ORDER BY 対象が SELECT リストの別名に一致するときは、PostgreSQL と
        // 同じく出力列（別名の指す列）を優先する。実在列の同名があっても別名側を採る。
        let target = match (&validated.projection, key.target.qualifier()) {
            (JoinProjection::Columns(colrefs), None) if !validated.column_aliases.is_empty() => {
                let mut hits = colrefs
                    .iter()
                    .zip(validated.column_aliases.iter())
                    .filter(|(_, a)| a.as_deref() == Some(key.target.name()));
                match (hits.next(), hits.next()) {
                    (Some(_), Some(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "ORDER BY target {:?} is ambiguous",
                            key.target.name()
                        )))
                    }
                    (Some((colref, _)), None) => colref,
                    _ => &key.target,
                }
            }
            _ => &key.target,
        };
        let c = b.col(target)?;
        let class = cmp_class(c.ty.as_ref()).ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("unsupported ORDER BY column type: {}", c.name))
        })?;
        order.push(PlainOrder {
            rel: c.rel,
            pos: c.pos,
            class,
            descending: key.descending,
        });
    }
    Ok(Shape::Plain { output, order })
}

fn build_aggregate_shape(
    validated: &ValidatedJoin,
    agg: &JoinAggregate,
    b: &mut Binder<'_, '_>,
) -> Result<AggPlan, SqlSurfaceError> {
    // GROUP BY キー。
    let mut keys: Vec<KeyPlan> = Vec::with_capacity(agg.group_by.len());
    let mut key_cols: Vec<Col> = Vec::with_capacity(agg.group_by.len());
    for colref in &agg.group_by {
        let c = b.col(colref)?;
        let kc = key_class(c.ty.as_ref()).map_err(|_| {
            SqlSurfaceError::invalid_input(format!("unsupported GROUP BY column type: {}", c.name))
        })?;
        let cc = cmp_class(c.ty.as_ref()).ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("unsupported GROUP BY column type: {}", c.name))
        })?;
        keys.push(KeyPlan {
            rel: c.rel,
            pos: c.pos,
            key_class: kc,
            cmp_class: cc,
            meta: Binder::meta(&c),
        });
        key_cols.push(c);
    }

    let mut items: Vec<AggItemPlan> = Vec::new();
    let mut out: Vec<AggOut> = Vec::with_capacity(agg.items.len());
    let mut metas: Vec<ColumnMeta> = Vec::with_capacity(agg.items.len());
    // 出力名（キー別名・集計項目名）→ 出力の索引。ORDER BY／HAVING の名前解決に使う。
    let mut key_aliases: Vec<(usize, String)> = Vec::new();
    let mut item_names: Vec<String> = Vec::new();
    let star_schema = *b.schemas.first().ok_or_else(|| SqlSurfaceError::Internal {
        detail: "JOIN has no relations".to_string(),
    })?;
    for sel in &agg.items {
        match sel {
            JoinSelectItem::Key { column, alias } => {
                let c = b.col(column)?;
                let idx = key_cols
                    .iter()
                    .position(|k| k.rel == c.rel && k.name == c.name)
                    .ok_or_else(|| {
                        SqlSurfaceError::unsupported(
                            "JOIN SELECT column must appear in GROUP BY or inside an aggregate",
                        )
                    })?;
                let meta = Binder::meta_with_alias(
                    keys.get(idx).map(|k| k.meta.clone()).ok_or_else(|| {
                        SqlSurfaceError::Internal {
                            detail: "JOIN GROUP BY key index out of range".to_string(),
                        }
                    })?,
                    alias.as_ref(),
                );
                // 出力名は別名、無ければ列名（PostgreSQL と同じく ORDER BY の出力名照合に使う）。
                key_aliases.push((idx, alias.clone().unwrap_or_else(|| c.name.clone())));
                out.push(AggOut::Key(idx));
                metas.push(meta);
            }
            JoinSelectItem::Aggregate { func, arg, alias } => {
                let (mut item, rel, column) = match arg {
                    None => (
                        BoundAggregateItem::bind(*func, AggregateTarget::Star, star_schema)?,
                        None,
                        None,
                    ),
                    Some(colref) => {
                        let c = b.col(colref)?;
                        let schema =
                            *b.schemas
                                .get(c.rel)
                                .ok_or_else(|| SqlSurfaceError::Internal {
                                    detail: "JOIN relation index out of range".to_string(),
                                })?;
                        let item = BoundAggregateItem::bind(
                            *func,
                            AggregateTarget::Column(c.name.clone()),
                            schema,
                        )?;
                        if matches!(
                            item.input,
                            AggregateInput::ScalarExpr { .. }
                                | AggregateInput::VectorColumnPresence
                        ) {
                            return Err(SqlSurfaceError::unsupported(
                                "aggregate over this column type is not supported in JOIN",
                            ));
                        }
                        let column = match (c.schema_idx, c.ty.clone()) {
                            (Some(idx), Some(ty)) => Some((idx, c.pos, ty)),
                            _ => None,
                        };
                        (item, Some(c.rel), column)
                    }
                };
                let name = item_effective_name(*func, alias);
                item.name = name.clone();
                let ty = crate::sql::aggregate::aggregate_result_type(item.func, &item.input);
                metas.push(ColumnMeta::Computed {
                    name: name.clone(),
                    ty,
                });
                out.push(AggOut::Item(items.len()));
                item_names.push(name);
                items.push(AggItemPlan { item, rel, column });
            }
        }
    }

    // 非修飾名がキーの名前空間（`GROUP BY` 列の元の列名・SELECT リスト上のキー出力名〔別名〕）に
    // 一致するか。単一テーブル経路の `resolve_group_reference`（キー名＋キー別名）と同じ範囲で照合し、
    // 集計項目名との衝突を 42702 とするために使う。修飾名は列参照として別経路で解決するため対象外。
    let key_name_hit = |name: &str| -> bool {
        key_cols.iter().any(|k| k.name == name) || key_aliases.iter().any(|(_, a)| a == name)
    };

    // HAVING: 集計項目名のみを参照でき（キーの列名・別名との衝突は 42702）、数値として比較できる結果に限る。
    let mut having: Vec<(usize, BinOp, f64)> = Vec::with_capacity(agg.having.len());
    for h in &agg.having {
        // 同名の集計項目が複数あるときは曖昧として拒否する（先頭一致で黙って解決しない。
        // 単一テーブル集計の `resolve_group_reference` と同じ 42702。Issue #1270）。
        // GROUP BY キーの出力名（別名・列名）と集計項目名が衝突する場合も曖昧とする
        // （単一テーブル経路と同じ判定。キー名だけに一致するときは従来どおり集計項目でない扱い）。
        let key_hit = key_name_hit(h.item_name.as_str());
        let mut hits = item_names
            .iter()
            .enumerate()
            .filter(|(_, n)| *n == &h.item_name)
            .map(|(i, _)| i);
        let idx = match (hits.next(), hits.next()) {
            (Some(_), Some(_)) => {
                return Err(SqlSurfaceError::ambiguous_column(h.item_name.as_str()))
            }
            (Some(_), None) if key_hit => {
                return Err(SqlSurfaceError::ambiguous_column(h.item_name.as_str()))
            }
            (Some(i), None) => i,
            (None, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "HAVING target {:?} is not an aggregate item",
                    h.item_name
                )))
            }
        };
        if let Some(it) = items.get(idx) {
            crate::sql::parser::check_having_target_is_numeric(&it.item, &h.item_name)?;
        }
        having.push((idx, h.op, h.literal));
    }

    // ORDER BY: GROUP BY キー（修飾／非修飾／別名）か集計項目名。
    let mut order: Vec<AggOrder> = Vec::with_capacity(validated.order_by.len());
    for key in &validated.order_by {
        let target = &key.target;
        // 非修飾名は、非集計 JOIN 経路と同じく SELECT の出力名（キー別名・集計項目名）を
        // 実在列より優先する。一致が無い場合と修飾名は列参照として解決し、束縛エラー
        // （未知の修飾子 42P01・曖昧な列 42702 等）は握りつぶさず伝播する。
        let alias_keys: Vec<usize> = if target.qualifier().is_none() {
            let mut v: Vec<usize> = Vec::new();
            for (k, a) in &key_aliases {
                if a == target.name() && !v.contains(k) {
                    v.push(*k);
                }
            }
            v
        } else {
            Vec::new()
        };
        // 同名の集計項目が複数あるときは、先頭一致で解決せず曖昧として拒否する。
        let item_hits: Vec<usize> = if target.qualifier().is_none() {
            item_names
                .iter()
                .enumerate()
                .filter(|(_, n)| *n == target.name())
                .map(|(i, _)| i)
                .collect()
        } else {
            Vec::new()
        };
        let item_match: Option<usize> = match item_hits.as_slice() {
            [] => None,
            [i] => Some(*i),
            _ => return Err(SqlSurfaceError::ambiguous_column(target.name())),
        };
        let ambiguous = || SqlSurfaceError::ambiguous_column(target.name());
        // キーの元の列名（別名の有無を問わない）と集計項目名の衝突も曖昧（HAVING と同じ範囲）。
        if target.qualifier().is_none() && item_match.is_some() && key_name_hit(target.name()) {
            return Err(ambiguous());
        }
        let resolved = if alias_keys.is_empty() && item_match.is_none() {
            let r = b.scope.resolve(target)?;
            let name = slot_name(
                r.slot(),
                b.schemas
                    .get(r.relation())
                    .copied()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN relation index out of range".to_string(),
                    })?,
            )?;
            match key_cols
                .iter()
                .position(|k| k.rel == r.relation() && k.name == name)
            {
                Some(k) => AggOrderTarget::Key(k),
                None => {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "ORDER BY target {:?} is not a GROUP BY column or aggregate item",
                        target.name()
                    )))
                }
            }
        } else {
            match (alias_keys.as_slice(), item_match) {
                ([_, _, ..], _) | ([_], Some(_)) => return Err(ambiguous()),
                ([k], None) => AggOrderTarget::Key(*k),
                (_, Some(i)) => AggOrderTarget::Item(i),
                ([], None) => return Err(ambiguous()),
            }
        };
        order.push(AggOrder {
            target: resolved,
            descending: key.descending,
        });
    }

    Ok(AggPlan {
        keys,
        items,
        out,
        metas,
        having,
        order,
    })
}
