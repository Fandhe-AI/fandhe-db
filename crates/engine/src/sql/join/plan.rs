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
//! - 側を跨ぐ `列 = 列` の conjunct でキー化可能な型のものは、両列が初めて揃う段が INNER
//!   なら、その段の結合キーへ昇格して残余から外す（Issue #1834・`where_promote`）。
//!   空キー段（CROSS／カンマ）はハッシュ結合になり、出力行数上限は一致件数で判定される。
//!
//! ON は `on_split` が結合キー／残余述語へ分類する（Issue #1829）。キー無しの段は NLJ で
//! 実行し（Issue #1839）、両側保存が残るキー無し FULL だけ `0A000` で拒否する。残余述語（左のみ・右のみ・混在）は一致条件の一部として
//! [`StepPlan`] の `on_*` へ束縛し、`exec::join_step` が評価する（Issue #1830）。
//! ON 残余は WHERE の strict マスク簡約へは流さない（外部結合の保存側を消さないため）。

use std::collections::HashMap;

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{
    JoinAggregate, JoinCondition, JoinKind, JoinProjection, JoinSelectItem, JoinWhereExpr,
    JoinWherePredicate, Projection, SqlSurfaceError, ValidatedJoin, ValidatedScan, WherePredicate,
};
use crate::sql::exec::ColumnMeta;
use crate::sql::order_value::NullPlacement;
use crate::sql::parser::{AggregateInput, AggregateTarget, BoundAggregateItem};
use crate::sql::relation::{BindingScope, ColumnRef, ColumnSlot, TableRef};
use crate::sql::udf_call::{BinOp, UdfRegistry};

use super::on_split::classify_on;
use super::using::{
    bind_using_step, natural_using_names, relation_columns, slot_type, JoinScope, MergedColumn,
    VisibleCol,
};
use super::values::{cmp_class, key_class, CmpClass, JoinKeyClass};
use super::where_promote::promotable_where_key;

/// CTE リレーションが公開した行 `id` を運ぶ列（`TableSchema::columns` の添字）の表。キーはリレーションの
/// 綴り（`TableRef::table`）。`cte_schema` が作り、`core.rs` が Execute・Describe の双方で同じものを渡す。
pub(crate) type CteIdColumns = HashMap<String, Vec<usize>>;

/// CTE の行 `id` を運ぶ列（`id_columns`）を、リテラル束縛用に疑似列 `id` と同じ numeric 型
/// （[`crate::sql::exec::ALIASED_ID_TYPE`]）へ差し替えたスキーマの写し。`CteIdColumns` を唯一の情報源として、
/// 側述語の事前束縛（`bind_side_scans`）と単一リレーションの残余述語（`build_residual`）が共有する。
/// 実行時のセルは変えない（`u64` のまま。残余の評価は `eval_single` が numeric へ写す）。
pub(super) fn cte_id_binding_schema(schema: &TableSchema, id_columns: &[usize]) -> TableSchema {
    let mut out = schema.clone();
    for &i in id_columns {
        if let Some(col) = out.columns.get_mut(i) {
            col.ty = crate::sql::exec::ALIASED_ID_TYPE.clone();
        }
    }
    out
}

/// 解決済みの列参照（どの relation の、側スキャン結果の何番目のセルか）。
#[derive(Debug, Clone)]
pub(super) struct Col {
    pub(super) rel: usize,
    /// 側スキャン結果の行 `cells` 内の位置。
    pub(super) pos: usize,
    /// 列型（疑似列 `id`・CTE が公開した行 `id` は `None`）。
    pub(super) ty: Option<ColumnType>,
    /// スキーマ列名（疑似列は `"id"`）。
    pub(super) name: String,
    /// `TableSchema::columns` の添字（疑似列は `None`）。
    pub(super) schema_idx: Option<usize>,
    /// CTE 本文が公開した行 `id`（`ColumnMeta::Id`／`AliasedId`）を運ぶ列か（Issue #1861）。セルは
    /// `Cell::Integer(u64)` のままで、公告は実 `BIGINT` ではなく `id` 型（`meta` が写す）。
    pub(super) cte_id: bool,
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
    /// ON の残余のうち、これまでの結合結果側だけを参照するもの（Issue #1830・SQL-60）。
    /// 一致条件の一部として `exec::join_step` がプローブ前に評価する。WHERE の残余とは
    /// 別物で、外部結合の strict マスク簡約には一切使わない。
    pub(super) on_acc: Vec<ResNode>,
    /// ON の残余のうち、新しく加わる relation だけを参照するもの（ビルド時に評価）。
    pub(super) on_new: Vec<ResNode>,
    /// ON の残余のうち、両側を跨ぐ非キー比較（キー一致の候補ペアごとに評価）。
    pub(super) on_mixed: Vec<ResNode>,
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
    /// FULL JOIN USING の統合列（`COALESCE`）の代替ソース `(rel, pos)`。`rel`／`pos` が
    /// NULL（NULL 補完または NULL セル）のとき先頭から順に最初の非 NULL を採る。通常列は空。
    pub(super) fallbacks: Vec<(usize, usize)>,
    pub(super) meta: ColumnMeta,
}

/// 非集計形のスカラー `ORDER BY` の 1 キー。
pub(super) struct PlainOrder {
    pub(super) rel: usize,
    pub(super) pos: usize,
    /// [`OutputColumn::fallbacks`] と同じ（統合列でキー値を組み立てるための代替ソース）。
    pub(super) fallbacks: Vec<(usize, usize)>,
    pub(super) class: CmpClass,
    pub(super) descending: bool,
    pub(super) nulls: NullPlacement,
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
    pub(super) nulls: NullPlacement,
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

/// 1 辺の走査投影。`All` は疑似列 `id`（位置 0。従来方式の表のみ）＋スキーマ列順（位置 `1 + index`、
/// 採番方式の表は `index`）
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
                .map(|i| i + usize::from(schema.has_pseudo_id()))
                .or(if name == "id" && schema.has_pseudo_id() {
                    Some(0)
                } else {
                    None
                }),
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
    scope: JoinScope<'a>,
    sides: Vec<SideProjection>,
    /// CTE リレーション（`relations` と同じ綴りのキー）ごとの、行 `id` を運ぶ列の `columns` 添字。
    cte_ids: &'b CteIdColumns,
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
        // JOIN の比較・並べ替え・グルーピングは列型だけで行い照合を運ばない（既定のバイト列順）。
        // 非バイト列順の照合（ICU。Issue #2190）の列の参照は `0A000` で拒否する（照合が黙って
        // 落ちる意味論の fail-open を作らない）。
        if let ColumnSlot::Column(i) = slot {
            crate::sql::udf_call::require_wired_collation(
                schema.columns.get(i).and_then(|c| c.collation),
            )?;
        }
        let side = self
            .sides
            .get_mut(rel)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN side projection index out of range".to_string(),
            })?;
        // `All` 投影は名前で再解決しない（疑似列 `id` と実カラム `id` が共存する表で、
        // 名前照合は実カラムを優先するため疑似列の位置が実カラムへ化ける）。
        // スロットから直接セル位置を計算し、疑似列と実カラムの区別を保つ。
        let pos = match side {
            SideProjection::All => match slot {
                ColumnSlot::Id => {
                    if schema.has_pseudo_id() {
                        0
                    } else {
                        return Err(SqlSurfaceError::Internal {
                            detail: "JOIN pseudo id slot on table without pseudo id".to_string(),
                        });
                    }
                }
                ColumnSlot::Column(i) => i.saturating_add(usize::from(schema.has_pseudo_id())),
            },
            SideProjection::Columns(_) => side.ensure(&name, schema)?,
        };
        // CTE が公開した行 `id` の列は、疑似列 `id` と同じ扱い（型なし＝符号なし整数のセル）にする。
        // 実 `BIGINT` として束縛すると、集計・比較の写像が `Cell::SignedInteger` を前提にして
        // `Cell::Integer(u64)` を扱えず、`i64::MAX` 超の行 id が壊れる／内部エラーになる（Issue #1861）。
        let cte_id = match (slot, self.relations.get(rel)) {
            (ColumnSlot::Column(i), Some(r)) => self
                .cte_ids
                .get(r.table())
                .is_some_and(|cols| cols.contains(&i)),
            _ => false,
        };
        Ok(Col {
            rel,
            pos,
            ty: if cte_id { None } else { ty.cloned() },
            name,
            schema_idx: match slot {
                ColumnSlot::Id => None,
                ColumnSlot::Column(i) => Some(i),
            },
            cte_id,
        })
    }

    fn meta(col: &Col) -> ColumnMeta {
        // CTE が公開した行 `id` は `BIGINT` ではなく `id` 型で公告する（単一表の具体化 CTE 経路と同じ。
        // `i64::MAX` 超の行 id は `int8` では符号化できない）。
        if col.cte_id {
            return if col.name == "id" {
                ColumnMeta::Id
            } else {
                crate::sql::exec::aliased_id_column_meta(&col.name)
            };
        }
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
            ColumnMeta::Id | ColumnMeta::AliasedId { .. } => {
                crate::sql::exec::aliased_id_column_meta(a)
            }
            ColumnMeta::Computed { ty, .. } => ColumnMeta::Computed {
                name: a.clone(),
                ty,
            },
        }
    }
}

/// [`JoinWherePredicate`] の葉が参照する列参照。
pub(super) fn leaf_colrefs(pred: &JoinWherePredicate) -> Vec<&ColumnRef> {
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
pub(super) fn relation_bit(rel: usize) -> Result<u32, SqlSurfaceError> {
    u32::try_from(rel)
        .ok()
        .and_then(|s| 1u32.checked_shl(s))
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN relation index exceeds mask width".to_string(),
        })
}

fn leaf_mask(pred: &JoinWherePredicate, scope: &JoinScope<'_>) -> Result<u32, SqlSurfaceError> {
    let mut mask = 0u32;
    for c in leaf_colrefs(pred) {
        let rel = scope.resolve(c)?.relation();
        mask |= relation_bit(rel)?;
    }
    Ok(mask)
}

/// 式が参照する relation 集合（ビットマスク）。
fn refs_mask(expr: &JoinWhereExpr, scope: &JoinScope<'_>) -> Result<u32, SqlSurfaceError> {
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
fn strict_mask(expr: &JoinWhereExpr, scope: &JoinScope<'_>) -> Result<u32, SqlSurfaceError> {
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
pub(super) fn flatten_conjuncts<'e>(expr: &'e JoinWhereExpr, out: &mut Vec<&'e JoinWhereExpr>) {
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
            lhs: crate::sql::udf_call::Expr::Ident(name),
            pattern: pattern.clone(),
            // JOIN の WHERE 文法は `ILIKE` を受理しない（SQL-54、Issue #1778 の対象外）。
            case_insensitive: false,
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
    scope: &JoinScope<'_>,
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

pub(super) fn collect_leaf_colrefs<'e>(expr: &'e JoinWhereExpr, out: &mut Vec<&'e ColumnRef>) {
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
        let base_schema = *binder
            .schemas
            .get(rel)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN relation index out of range".to_string(),
            })?;
        // CTE の行 `id` の列は numeric で束縛する（`BIGINT` の `i64` 解析で有効な行 id のリテラルを
        // `22003` にしない）。
        let id_typed = binder
            .relations
            .get(rel)
            .and_then(|r| binder.cte_ids.get(r.table()))
            .filter(|cols| !cols.is_empty())
            .map(|cols| cte_id_binding_schema(base_schema, cols));
        let schema = id_typed.as_ref().unwrap_or(base_schema);
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
            // CTE の行 `id` の列は型なし（符号なし整数のセル）として束縛済みだが、残余の評価では
            // numeric の列として渡す（`eval_single` が `u64` のセルを numeric へ写す）。
            let ty = if c.cte_id {
                Some(crate::sql::exec::ALIASED_ID_TYPE.clone())
            } else {
                c.ty
            };
            if let (Some(idx), Some(ty)) = (c.schema_idx, ty) {
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

/// 1 段分の ON 残余の束縛結果（宣言順を保つ）。
#[derive(Default)]
struct OnResiduals {
    acc: Vec<ResNode>,
    new: Vec<ResNode>,
    mixed: Vec<ResNode>,
}

/// ON 残余（左のみ・右のみ・混在）を段のスコープで [`ResNode`] へ束縛する（Issue #1830）。
///
/// `b.scope` は段のループ中は全 relation の plain スコープで、USING の統合列を含まず
/// 前方参照も許してしまうため、呼び出しの間だけ段のスコープへ差し替え、成否に関わらず
/// 元へ戻してから返す。`build_plan` からのみ呼ばれる。
fn bind_on_residuals<'a>(
    b: &mut Binder<'a, '_>,
    step_scope: JoinScope<'a>,
    split: &super::on_split::OnSplit<'_>,
    udfs: &UdfRegistry,
) -> Result<OnResiduals, SqlSurfaceError> {
    let saved = std::mem::replace(&mut b.scope, step_scope);
    let r = (|| {
        let mut bind = |conjs: &[&JoinWhereExpr]| -> Result<Vec<ResNode>, SqlSurfaceError> {
            conjs
                .iter()
                .map(|conj| build_residual(conj, b, udfs))
                .collect()
        };
        Ok(OnResiduals {
            acc: bind(&split.acc_only)?,
            new: bind(&split.new_only)?,
            mixed: bind(&split.mixed)?,
        })
    })();
    b.scope = saved;
    r
}

/// 結合連鎖の束縛（[`JoinPlan`] を返す）。列解決・結合キー型検証・外部結合簡約・
/// WHERE 分離・投影展開・並べ替え／集計の型検証をすべて行走査より前に確定させる。
pub(super) fn build_plan<'a>(
    schemas: &'a HashMap<String, TableSchema>,
    cte_ids: &CteIdColumns,
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
    let scope = JoinScope::plain(BindingScope::new(
        relations
            .iter()
            .cloned()
            .zip(rel_schemas.iter().copied())
            .collect(),
    )?);
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
        cte_ids,
    };

    // 結合段（ON）。段 k のスコープは relation 0..=k+1 だけで構築する——未出現の
    // relation を前方参照すると `BindingScope::resolve` が `42P01` で拒否する
    // （PostgreSQL と同じ分類）。
    //
    // `USING (...)` 段（Issue #1826・SQL-60 (1)）は等価結合キーへ脱糖し、結合列を 1 列に
    // 統合した可視列並び・統合列を段ごとに引き継ぐ（`sql::join::using`）。ON 段は従来どおり
    // 可視列を「左の可視列 ++ 右の全列」に伸ばすだけで、統合列の解決規則だけが共通になる。
    let mut steps: Vec<StepPlan> = Vec::with_capacity(validated.steps.len());
    let mut merged: Vec<MergedColumn> = Vec::new();
    let mut visible: Vec<VisibleCol> = match b.schemas.first() {
        Some(schema) => relation_columns(0, schema),
        None => Vec::new(),
    };
    let mut has_using = false;
    // キー無し ON の FULL の段番号（Issue #1838）。束縛完了後に判定する。
    let mut keyless_full_on: Vec<usize> = Vec::new();
    for (k, step) in validated.steps.iter().enumerate() {
        let upto = k + 2;
        let new_rel = k + 1;
        let step_base = BindingScope::new(
            b.relations
                .iter()
                .take(upto)
                .cloned()
                .zip(b.schemas.iter().take(upto).copied())
                .collect(),
        )?;
        let mut keys: Vec<StepKey> = Vec::new();
        let mut on_res: OnResiduals = OnResiduals::default();
        match &step.condition {
            JoinCondition::On(on) => {
                let new_schema =
                    *b.schemas
                        .get(new_rel)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "JOIN relation index out of range".to_string(),
                        })?;
                visible.extend(relation_columns(new_rel, new_schema));
                let step_scope =
                    JoinScope::new(step_base, merged.clone(), visible.clone(), has_using);
                // ON の AND 連鎖を結合キーと残余述語へ分解する（Issue #1829・SQL-60 ポインタ）。
                // 列解決エラーは分類より前に確定する。
                let split = classify_on(on, &step_scope, new_rel)?;
                if split.keys.is_empty() && step.kind == JoinKind::Full {
                    // Issue #1838（D11）: キー無し FULL はここでは拒否せず、束縛をすべて終えた後
                    // （WHERE の外部結合簡約の後）で `0A000` か従来の `42601` を決める。
                    keyless_full_on.push(k);
                }
                // Issue #1839: キー無しの非 FULL 段は拒否せず NLJ へ流す。ON には葉が 1 個以上あるため
                // キー無しは常に ON 残余ありで、`exec::join_step` が NLJ を選ぶ（D1）。
                keys.reserve(split.keys.len());
                for (acc, new) in &split.keys {
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
                // 残余述語は一致条件として段のスコープで束縛する（Issue #1830）。
                on_res = bind_on_residuals(&mut b, step_scope, &split, udfs)?;
            }
            JoinCondition::Cross => {
                // CROSS JOIN（Issue #1831）: 条件なしの直積。右 relation の全列を可視列へ足すだけで、
                // キー・ON 残余は空のまま（実行段は空キー段として全組み合わせを列挙する）。
                let new_schema =
                    *b.schemas
                        .get(new_rel)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "JOIN relation index out of range".to_string(),
                        })?;
                visible.extend(relation_columns(new_rel, new_schema));
            }
            JoinCondition::Using(_) | JoinCondition::Natural => {
                let left_base = BindingScope::new(
                    b.relations
                        .iter()
                        .take(new_rel)
                        .cloned()
                        .zip(b.schemas.iter().take(new_rel).copied())
                        .collect(),
                )?;
                let left_scope =
                    JoinScope::new(left_base, merged.clone(), visible.clone(), has_using);
                // NATURAL は両入力の共通列名を導出して USING と同じ束縛へ流す（Issue #1827）。
                let derived;
                let names: &[String] = match &step.condition {
                    JoinCondition::Using(names) => names,
                    _ => {
                        let new_schema =
                            *b.schemas
                                .get(new_rel)
                                .ok_or_else(|| SqlSurfaceError::Internal {
                                    detail: "JOIN relation index out of range".to_string(),
                                })?;
                        derived = natural_using_names(&left_scope, new_rel, new_schema)?;
                        &derived
                    }
                };
                let bound = bind_using_step(&left_scope, &step_base, step.kind, names, new_rel)?;
                keys.reserve(bound.keys.len());
                for key in &bound.keys {
                    let acc_col = b.col_from(key.acc.0, key.acc.1, key.acc.2.as_ref())?;
                    let new_col = b.col_from(key.new.0, key.new.1, key.new.2.as_ref())?;
                    keys.push(StepKey {
                        acc_rel: acc_col.rel,
                        acc_pos: acc_col.pos,
                        new_pos: new_col.pos,
                        class: key.class.clone(),
                    });
                }
                merged = bound.merged;
                visible = bound.visible;
                // 共通列 0 個の NATURAL は統合を伴わないので USING 扱いにしない（従来経路を保つ）。
                has_using = has_using || !names.is_empty();
            }
        }
        steps.push(StepPlan {
            preserve_acc: matches!(step.kind, JoinKind::Left | JoinKind::Full),
            preserve_new: matches!(step.kind, JoinKind::Right | JoinKind::Full),
            keys,
            on_acc: on_res.acc,
            on_new: on_res.new,
            on_mixed: on_res.mixed,
        });
    }
    // 以降（WHERE・投影・並べ替え・集計）は、全段適用後の統合列・可視列を持つスコープで解決する。
    let final_base = BindingScope::new(
        b.relations
            .iter()
            .cloned()
            .zip(b.schemas.iter().copied())
            .collect(),
    )?;
    b.scope = JoinScope::new(final_base, merged, visible, has_using);

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
            // 結合キーへの昇格（Issue #1834・SQL-60 ポインタ）。側を跨ぐ等価葉は `r1`・`r2` の
            // 両方で null-rejecting なので、上の簡約により段 `r2 - 1` は必ず INNER になる
            // （INNER 段ではキーと結合後フィルタは連言の順序違いで結果が同一）。それでも
            // 保存側フラグを実行時に確認し、満たさなければ残余へ回す（フォールバックは常に正しい）。
            if let Some(p) = promotable_where_key(c, &b.scope)? {
                let inner = steps
                    .get(p.step)
                    .is_some_and(|s| !s.preserve_acc && !s.preserve_new);
                if inner {
                    let acc_col =
                        b.col_from(p.acc.relation(), p.acc.slot(), p.acc.column_type())?;
                    let new_col =
                        b.col_from(p.new.relation(), p.new.slot(), p.new.column_type())?;
                    if let Some(step) = steps.get_mut(p.step) {
                        step.keys.push(StepKey {
                            acc_rel: acc_col.rel,
                            acc_pos: acc_col.pos,
                            new_pos: new_col.pos,
                            class: p.class,
                        });
                        continue;
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
            limit_implicit: false,
            order_by: Vec::new(),
            offset: 0,
            window_items: Vec::new(),
            order_keys: Vec::new(),
            scalar_subquery_items: Vec::new(),
            physical_order_keys: Vec::new(),
            embedded_scalar_subqueries: Vec::new(),
            with_ties: false,
        });
    }

    // Issue #1838（D11）: キー無し ON の FULL の判定。列解決・型エラー（解析段）がすべて済んだ後に、
    // WHERE の strict 述語による外部結合の簡約結果で決める（PostgreSQL も簡約を結合方式の選択より
    // 先に行う）。両側の保存が残るキー無し段だけ `0A000`。片側以下に簡約された段や WHERE の等価昇格で
    // キーを得た段は、NLJ／ハッシュ経路へそのまま流す（Issue #1839）。判定は束縛結果だけで決まり、行データ（テナントの可視行）に依存しない。
    for &k in &keyless_full_on {
        if let Some(s) = steps.get(k) {
            if s.keys.is_empty() && s.preserve_acc && s.preserve_new {
                return Err(SqlSurfaceError::feature_not_supported(
                    "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions",
                ));
            }
        }
    }

    Ok(JoinPlan {
        schemas: b.schemas,
        scans,
        steps,
        residual,
        shape,
    })
}

/// 解決済みの列から非集計 JOIN の並べ替えキーを作る（列キー・序数キー共通。並べ替え不能な型は `22000`）。
/// `fallbacks` は FULL JOIN USING の統合列の代替ソース（通常列は空）。
fn plain_order_for(
    c: &Col,
    fallbacks: Vec<(usize, usize)>,
    descending: bool,
    nulls: Option<NullPlacement>,
) -> Result<PlainOrder, SqlSurfaceError> {
    let class = cmp_class(c.ty.as_ref()).ok_or_else(|| {
        SqlSurfaceError::invalid_input(format!("unsupported ORDER BY column type: {}", c.name))
    })?;
    Ok(PlainOrder {
        rel: c.rel,
        pos: c.pos,
        fallbacks,
        class,
        descending,
        nulls: nulls.unwrap_or_else(|| NullPlacement::pg_default(descending)),
    })
}

impl Binder<'_, '_> {
    /// USING の統合列 `idx` の値ソースを側投影へ含め、先頭ソースの [`Col`] と代替ソース
    /// （FULL の `COALESCE` の 2 番目以降）を返す。投影・非集計 `ORDER BY` が使う。
    fn merged_cols(&mut self, idx: usize) -> Result<(Col, Vec<(usize, usize)>), SqlSurfaceError> {
        let sources = self.scope.merged(idx)?.sources.clone();
        let mut cols: Vec<Col> = Vec::with_capacity(sources.len());
        for (rel, slot) in sources {
            let schema = *self
                .schemas
                .get(rel)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN relation index out of range".to_string(),
                })?;
            let ty = slot_type(schema, slot);
            cols.push(self.col_from(rel, slot, ty.as_ref())?);
        }
        let mut it = cols.into_iter();
        let first = it.next().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "JOIN merged column has no source".to_string(),
        })?;
        Ok((first, it.map(|c| (c.rel, c.pos)).collect()))
    }

    /// 可視列 1 個を [`Col`]（と代替ソース）へ解決する（USING を含む結合の `*` 展開・序数 `ORDER BY`）。
    fn visible_cols(
        &mut self,
        v: VisibleCol,
    ) -> Result<(Col, Vec<(usize, usize)>), SqlSurfaceError> {
        match v {
            VisibleCol::Base { rel, slot } => {
                let schema = *self
                    .schemas
                    .get(rel)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN relation index out of range".to_string(),
                    })?;
                let ty = slot_type(schema, slot);
                Ok((self.col_from(rel, slot, ty.as_ref())?, Vec::new()))
            }
            VisibleCol::Merged(idx) => self.merged_cols(idx),
        }
    }
}

fn build_plain_shape(
    validated: &ValidatedJoin,
    b: &mut Binder<'_, '_>,
) -> Result<Shape, SqlSurfaceError> {
    let mut output: Vec<OutputColumn> = Vec::new();
    match &validated.projection {
        JoinProjection::All if b.scope.has_using() => {
            // USING を含む結合の `*`: 統合列（USING 順）→ 左の残り → 右の残り（PostgreSQL 17 と
            // 同じ可視列の並び。`sql::join::using`）。
            let visible: Vec<VisibleCol> = b.scope.visible().to_vec();
            for v in visible {
                let (c, fallbacks) = b.visible_cols(v)?;
                output.push(OutputColumn {
                    rel: c.rel,
                    pos: c.pos,
                    fallbacks,
                    meta: Binder::meta(&c),
                });
            }
        }
        JoinProjection::All => {
            for rel in 0..b.schemas.len() {
                let schema = *b
                    .schemas
                    .get(rel)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN relation index out of range".to_string(),
                    })?;
                // 疑似列 `id`（位置 0）は従来方式の表だけが持つ。採番方式の表（TABLE-23・Issue #2199）は
                // 宣言した列だけを展開し、位置は 0 始まり（`Projection::All` の展開規則と一致させる）。
                let id_offset = usize::from(schema.has_pseudo_id());
                if schema.has_pseudo_id() {
                    output.push(OutputColumn {
                        rel,
                        pos: 0,
                        fallbacks: Vec::new(),
                        meta: ColumnMeta::Id,
                    });
                }
                for (idx, col) in schema.columns.iter().enumerate() {
                    output.push(OutputColumn {
                        rel,
                        pos: idx + id_offset,
                        fallbacks: Vec::new(),
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
                // FULL JOIN USING の統合列は `COALESCE(左, 右)` の代替ソース付きで出力する。
                let (c, fallbacks) = match b.scope.resolve_coalesce(colref)? {
                    Some(idx) => b.merged_cols(idx)?,
                    None => (b.col(colref)?, Vec::new()),
                };
                let alias = validated.column_aliases.get(i).and_then(Option::as_ref);
                let meta = Binder::meta_with_alias(Binder::meta(&c), alias);
                output.push(OutputColumn {
                    rel: c.rel,
                    pos: c.pos,
                    fallbacks,
                    meta,
                });
            }
        }
    }
    let mut order: Vec<PlainOrder> = Vec::with_capacity(validated.order_by.len());
    for key in &validated.order_by {
        // SQL-56 (2)・Issue #1792: 序数は出力列（`*` 展開後・SELECT リスト順）の位置。
        // 範囲外は `42P10`（文とスキーマだけで決まり、行の内容に依存しない）。
        if let Some(position) = key.ordinal {
            let out_index = usize::try_from(position)
                .ok()
                .filter(|p| *p >= 1 && *p <= output.len())
                .map(|p| p - 1)
                .ok_or(SqlSurfaceError::OrderByPositionNotInSelectList { position })?;
            let (c, fallbacks) = match &validated.projection {
                JoinProjection::Columns(colrefs) => {
                    let colref =
                        colrefs
                            .get(out_index)
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "JOIN ORDER BY position out of range".to_string(),
                            })?;
                    match b.scope.resolve_coalesce(colref)? {
                        Some(idx) => b.merged_cols(idx)?,
                        None => (b.col(colref)?, Vec::new()),
                    }
                }
                // USING を含む結合の `*` は可視列の並び（統合列が先頭）に従う。スキーマから
                // 位置を再計算すると列並びがずれるため、可視列から直接引く。
                JoinProjection::All if b.scope.has_using() => {
                    let v = b.scope.visible().get(out_index).copied().ok_or_else(|| {
                        SqlSurfaceError::Internal {
                            detail: "JOIN ORDER BY position out of range".to_string(),
                        }
                    })?;
                    b.visible_cols(v)?
                }
                JoinProjection::All => {
                    // `*` は関係ごとに `id`（従来方式の表のみ）＋宣言列の順に展開済み。
                    let mut remaining = out_index;
                    let mut found = None;
                    for (rel, schema) in b.schemas.iter().enumerate() {
                        let width = usize::from(schema.has_pseudo_id())
                            .saturating_add(schema.columns.len());
                        if remaining < width {
                            found = Some((rel, *schema, remaining));
                            break;
                        }
                        remaining -= width;
                    }
                    let (rel, schema, local) = found.ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN ORDER BY position out of range".to_string(),
                    })?;
                    let (slot, ty) =
                        if schema.has_pseudo_id() && local == 0 {
                            (ColumnSlot::Id, None)
                        } else {
                            let idx = local - usize::from(schema.has_pseudo_id());
                            let def = schema.columns.get(idx).ok_or_else(|| {
                                SqlSurfaceError::Internal {
                                    detail: "JOIN ORDER BY position out of range".to_string(),
                                }
                            })?;
                            (ColumnSlot::Column(idx), Some(def.ty.clone()))
                        };
                    (b.col_from(rel, slot, ty.as_ref())?, Vec::new())
                }
            };
            order.push(plain_order_for(&c, fallbacks, key.descending, key.nulls)?);
            continue;
        }
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
        let (c, fallbacks) = match b.scope.resolve_coalesce(target)? {
            Some(idx) => b.merged_cols(idx)?,
            None => (b.col(target)?, Vec::new()),
        };
        order.push(plain_order_for(&c, fallbacks, key.descending, key.nulls)?);
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
                        // CTE が公開した行 `id`（符号なし整数のセル）への集計は、集計の入力写像が
                        // 符号付き `BIGINT` のセルを前提とするため未対応。内部エラーや `i64` への
                        // 変換で値を壊さず、固定文言の `0A000` で拒否する（fail-closed）。
                        if c.cte_id {
                            return Err(SqlSurfaceError::feature_not_supported(
                                "an aggregate over a CTE row id column is not supported in JOIN",
                            ));
                        }
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
                                | AggregateInput::TypedExpr { .. }
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
            // どの出力名にも一致しない＝未定義列（Issue #2332: 単一テーブル経路の
            // `resolve_group_reference` と同じ 42703）。キー名だけに一致する場合は従来どおり
            // 「集計項目でない」型不整合（22000）。
            (None, _) if !key_hit => {
                return Err(SqlSurfaceError::undefined_column(h.item_name.as_str()))
            }
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
        // SQL-56 (2)・Issue #1792: 序数は SELECT リスト（`out`）の位置。範囲外は `42P10`。
        if let Some(position) = key.ordinal {
            let out_index = usize::try_from(position)
                .ok()
                .filter(|p| *p >= 1 && *p <= out.len())
                .map(|p| p - 1)
                .ok_or(SqlSurfaceError::OrderByPositionNotInSelectList { position })?;
            let target = match out.get(out_index) {
                Some(AggOut::Key(k)) => AggOrderTarget::Key(*k),
                Some(AggOut::Item(i)) => AggOrderTarget::Item(*i),
                None => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "JOIN ORDER BY position out of range".to_string(),
                    })
                }
            };
            order.push(AggOrder {
                target,
                descending: key.descending,
                nulls: key
                    .nulls
                    .unwrap_or_else(|| NullPlacement::pg_default(key.descending)),
            });
            continue;
        }
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
            nulls: key
                .nulls
                .unwrap_or_else(|| NullPlacement::pg_default(key.descending)),
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
