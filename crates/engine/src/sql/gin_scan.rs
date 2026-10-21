//! jsonb の包含・キー存在・jsonpath 演算子を GIN 相当索引（`user_gin/{索引名}`）で解決する広域取得の
//! 候補探査（INDEX-8、Issue #2208）。
//!
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-8・INDEX-7、`data-model.md` TABLE-12、
//! `rls.md` RLS-9・RLS-10、`sql-surface.md` SQL-6・SQL-27（EXPLAIN の語彙）。設計記録は
//! `docs/design/gin-jsonb-index.md`（D5・D6・D9・§9）。`sql::pk_point`（Issue #2198）と同じ型で作る。
//!
//! # 責務境界
//!
//! - **静的分類**（[`classify_gin_scan`]）: 束縛済みの [`BoundScan`] と対象表の GIN 宣言から、AND 連言の
//!   述語のうち**最初に条件を満たした 1 本**が索引候補にできるかを、スキーマ・束縛結果・宣言だけで判定する。
//!   データ・テナントの状態には依存しない。`sql::scan` の経路選択と `core.rs::scan_explain_result` の
//!   `EXPLAIN` 表示が**同じ関数**を呼ぶ（2 行が矛盾しない。SQL-27 の「矛盾出力の防止」）。
//! - **候補探査**（[`for_each_candidate`]）: 分類が [`GinScanPlan::Index`] のときに限り、`sql::scan` が
//!   全行走査の代わりに呼ぶ。テナントごとにマーカーを読み取り専用で照合し、有効ならポスティング範囲を
//!   k-way マージ（積は leapfrog 交差、和はヒープ）して rowid の昇順に候補行を訪問者へ渡す。無効なら
//!   そのテナントの行範囲を走査して縮退する。物理形式の知識は `gin_index`／`gin_index::extract` に閉じる。
//!
//! # 適用範囲（v1）
//!
//! 順序なしの広域取得（`ORDER BY`・ウィンドウ無し）のみ。経路の優先順位は PK 点検索 → GIN → 全走査で、
//! 実行と `EXPLAIN` で同じ。`expr_filters`（トップレベルの AND 連言）の最初に条件を満たした 1 述語だけを
//! 使い、`or_filters`・`NOT` の内側・複数述語や複数索引の交差は見ない。使う索引は述語ごとに決定的に選ぶ
//! （`@>` は `jsonb_path_ops` を先に、次に `jsonb_ops`。どちらの中でも索引名の昇順。`?` 系は `jsonb_ops`、
//! `@?`／`@@` は `jsonb_path_ops` のみ）。
//!
//! # 正しさの不変条件
//!
//! 1. **偽陰性なし**: 述語を満たす行は、問い合わせキー集合の全て（積）または少なくとも 1 つ（和）を
//!    ポスティングに持つ。問い合わせ側のエントリは格納側と同じ規則・同じ共有関数
//!    （`gin_index::extract::query_*`）で作る。判断に迷う形・空集合・上限超過・NULL のみのキーは必ず
//!    [`GinScanPlan::NotApplicable`]（全走査）に倒す（fail-soft。「索引から全行」を返す経路を作らない）。
//! 2. **偽陽性・stale は除かれる**: 候補は全走査と同じ `with_visible_row`（デコード前の `is_visible`、
//!    TABLE-12 の整合、GIN 述語自身を含む `WHERE` 全体の再評価、可視性の再適用）を通る。ハッシュ衝突・
//!    stale なポスティング・削除済み行（行ストアに無い）はここで除かれる。
//! 3. **出力の同一性**: テナントを物理キーの昇順に処理し、各テナントの候補を rowid の昇順で渡すため、
//!    行・順序・`LIMIT`／`OFFSET` の数え方は順序なしの全走査と同一になる。
//!
//! # テナント境界（P0）
//!
//! 他テナントの索引パーティションを**読み取り専用**で参照するだけで、結果に出るかどうかは
//! `PolicyContext::is_visible` だけが決める（`sql::pk_point` と同じ前例）。`ctx` が他テナントの `Public` 行を
//! 可視としないときは自テナントだけを探査する。索引や行の読み取り失敗・破損は固定文言の内部エラー
//! （値・id・テナント・索引内容を含めない）で fail-closed に拒否する。`EXPLAIN` は静的判定だけで
//! `user_gin/*` を開かず、縮退・候補数・テナントの有無を出さない。

use crate::catalog::{self, ColumnType, DeclaredGinIndex, GinOpClass, TableSchema};
use crate::gin_index::extract::{query_contains_entries, query_key_entries, query_path_leaf_entry};
use crate::gin_index::{decode_posting_rowid, posting_range_bounds, GinIndexProbe, GinLimits};
use crate::json::jsonpath::{AccessorOp, CompareOp, PathMode, PathNode, SqlJsonPath};
use crate::json::JsonValue;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::jsonpath_fn::{BoundJsonPathFn, JsonPathFnKind, SilentArg};
use crate::sql::parser::BoundScan;
use crate::sql::udf_call::{BoundExpr, JsonPredicateOp, JsonPredicateRhs};
use crate::storage::read_source::ReadSource;
use crate::storage::rowid::{row_key, RowId};
use redb::ReadableTable;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Bound;

/// 問い合わせキー集合の結合規則。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GinKeys {
    /// 全エントリが必要（積。`@>`・`?&`・`@?`・`@@`・単一キーの `?`）。
    And(Vec<Vec<u8>>),
    /// いずれか 1 つ（和。`?|`）。
    Or(Vec<Vec<u8>>),
}

/// 静的分類の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GinScanPlan {
    /// 索引 `decl` のポスティングで候補を絞れる。
    Index {
        decl: DeclaredGinIndex,
        keys: GinKeys,
    },
    /// 索引にできない（従来どおり全走査）。
    NotApplicable,
}

impl GinScanPlan {
    /// 計画された索引名（`EXPLAIN` の ` index=<name>` 接尾辞に使う）。
    pub(crate) fn index_name(&self) -> Option<&str> {
        match self {
            GinScanPlan::Index { decl, .. } => Some(decl.index_name.as_str()),
            GinScanPlan::NotApplicable => None,
        }
    }
}

/// GIN の対象になりうる形かを、カタログを読む前に判定する軽い構文的事前判定。順序なしの広域取得で、
/// `expr_filters` に jsonb 列を左辺とする包含・存在・jsonpath 述語がある場合だけ `true`。それ以外の文で
/// 宣言（索引カタログ）の読み取りを増やさないために、`sql::scan` と `EXPLAIN` が分類の前に呼ぶ。
pub(crate) fn may_use_gin(bound: &BoundScan) -> bool {
    bound.windows.is_empty()
        && bound.order_by.is_empty()
        && bound
            .expr_filters
            .iter()
            .any(|e| jsonb_column_of(e).is_some())
}

/// 述語 `expr` の左辺が jsonb 列参照ならその列インデックス。
fn jsonb_column_of(expr: &BoundExpr) -> Option<usize> {
    match expr {
        BoundExpr::JsonPredicate { lhs, .. } => match lhs.as_ref() {
            BoundExpr::JsonbColumnRef { index } => Some(*index),
            _ => None,
        },
        BoundExpr::JsonPathFn(f) => match &f.target {
            BoundExpr::JsonbColumnRef { index } => Some(*index),
            _ => None,
        },
        _ => None,
    }
}

/// `bound` が GIN 索引候補になるかを静的に分類する（`gins` は対象表の宣言。索引名の昇順）。
/// 実行（`sql::scan`）と `EXPLAIN`（`core.rs`）が共有する。
pub(crate) fn classify_gin_scan(
    schema: &TableSchema,
    bound: &BoundScan,
    gins: &[DeclaredGinIndex],
) -> GinScanPlan {
    if gins.is_empty() || !bound.windows.is_empty() || !bound.order_by.is_empty() {
        return GinScanPlan::NotApplicable;
    }
    let limits = GinLimits::default();
    for expr in &bound.expr_filters {
        if let Some((decl, keys)) = plan_for_expr(schema, expr, gins, &limits) {
            return GinScanPlan::Index { decl, keys };
        }
    }
    GinScanPlan::NotApplicable
}

/// 1 本の述語から（索引宣言, 問い合わせキー）を決める。決められなければ `None`（次の述語へ）。
fn plan_for_expr(
    schema: &TableSchema,
    expr: &BoundExpr,
    gins: &[DeclaredGinIndex],
    limits: &GinLimits,
) -> Option<(DeclaredGinIndex, GinKeys)> {
    let column_index = jsonb_column_of(expr)?;
    let column = schema.columns.get(column_index)?;
    if !matches!(column.ty, ColumnType::Jsonb) {
        return None;
    }
    // 対象列の宣言（索引名の昇順）。
    let on_column = || gins.iter().filter(|d| d.column == column.name);
    match expr {
        BoundExpr::JsonPredicate { op, rhs, .. } => match (op, rhs) {
            (JsonPredicateOp::Contains, JsonPredicateRhs::Jsonb(literal)) => {
                let BoundExpr::JsonbLiteral(text) = literal.as_ref() else {
                    return None;
                };
                let value = crate::json::parse_json(text).ok()?;
                // `jsonb_path_ops` を先に、次に `jsonb_ops`（どちらの中でも索引名の昇順）。
                for opclass in [GinOpClass::JsonbPathOps, GinOpClass::JsonbOps] {
                    for decl in on_column().filter(|d| d.opclass == opclass) {
                        if let Some(keys) = query_contains_entries(opclass, &value, limits) {
                            return Some((decl.clone(), GinKeys::And(keys)));
                        }
                    }
                }
                None
            }
            (JsonPredicateOp::Exists, JsonPredicateRhs::Key(key)) => {
                let BoundExpr::Text(key) = key.as_ref() else {
                    return None;
                };
                let decl = on_column().find(|d| d.opclass == GinOpClass::JsonbOps)?;
                let keys = query_key_entries([key.as_str()], limits)?;
                Some((decl.clone(), GinKeys::And(keys)))
            }
            (JsonPredicateOp::ExistsAny, JsonPredicateRhs::Keys(keys)) => {
                let decl = on_column().find(|d| d.opclass == GinOpClass::JsonbOps)?;
                // NULL 要素は読み飛ばす（`json::jsonb_exists_any` と同じ規則）。
                let keys = query_key_entries(keys.iter().flatten().map(String::as_str), limits)?;
                Some((decl.clone(), GinKeys::Or(keys)))
            }
            (JsonPredicateOp::ExistsAll, JsonPredicateRhs::Keys(keys)) => {
                let decl = on_column().find(|d| d.opclass == GinOpClass::JsonbOps)?;
                let keys = query_key_entries(keys.iter().flatten().map(String::as_str), limits)?;
                Some((decl.clone(), GinKeys::And(keys)))
            }
            // `<@`、左辺が jsonb 列でない形（束縛段が左右を保持した `'{..}' @> col`）、右辺が
            // リテラルでない形は全走査へ倒す。
            _ => None,
        },
        BoundExpr::JsonPathFn(f) => {
            let entry = jsonpath_leaf_entry(f)?;
            let decl = on_column().find(|d| d.opclass == GinOpClass::JsonbPathOps)?;
            Some((decl.clone(), GinKeys::And(vec![entry])))
        }
        _ => None,
    }
}

/// 演算子形 `@?`／`@@`（`vars` なし・`silent` 定数 true・SQL/JSON 句なし）の許可形から、
/// `jsonb_path_ops` の葉エントリ 1 件を作る。関数形（`jsonb_path_exists(...)`）・strict・許可形以外は
/// `None`（全走査）。
fn jsonpath_leaf_entry(f: &BoundJsonPathFn) -> Option<Vec<u8>> {
    if f.vars.is_some() || f.sqljson.is_some() || f.silent != SilentArg::Const(true) {
        return None;
    }
    let path: &SqlJsonPath = f.path.as_deref()?;
    if path.mode != PathMode::Lax {
        return None;
    }
    let (members, scalar) = match f.kind {
        JsonPathFnKind::Match => match_shape(&path.expr)?,
        JsonPathFnKind::Exists => exists_shape(&path.expr)?,
        _ => return None,
    };
    query_path_leaf_entry(members.iter().map(String::as_str), &scalar)
}

/// `@@` の許可形 `$.a.b == <リテラル>`（左右どちらでも可）。メンバー列と比較スカラーを返す。
fn match_shape(expr: &PathNode) -> Option<(Vec<String>, JsonValue)> {
    let PathNode::Compare {
        op: CompareOp::Eq,
        lhs,
        rhs,
    } = expr
    else {
        return None;
    };
    let (path_side, literal_side) = if literal_scalar(rhs).is_some() {
        (lhs.as_ref(), rhs.as_ref())
    } else {
        (rhs.as_ref(), lhs.as_ref())
    };
    let members = member_chain(path_side, &PathNode::Root, false)?;
    Some((members, literal_scalar(literal_side)?))
}

/// `@?` の許可形 `$.a.b ? (@.c.d == <リテラル>)`（末尾にフィルタ 1 個。比較は左右どちらでも可）。
/// 外側のメンバー列と内側のメンバー列を連結した経路と、比較スカラーを返す。
fn exists_shape(expr: &PathNode) -> Option<(Vec<String>, JsonValue)> {
    let PathNode::Path { base, ops } = expr else {
        return None;
    };
    if !matches!(base.as_ref(), PathNode::Root) {
        return None;
    }
    let (last, outer_ops) = ops.split_last()?;
    let AccessorOp::Filter(inner) = last else {
        return None;
    };
    let mut members = Vec::new();
    for op in outer_ops {
        let AccessorOp::Member(name) = op else {
            return None;
        };
        members.push(name.to_string());
    }
    let PathNode::Compare {
        op: CompareOp::Eq,
        lhs,
        rhs,
    } = inner.as_ref()
    else {
        return None;
    };
    let (path_side, literal_side) = if literal_scalar(rhs).is_some() {
        (lhs.as_ref(), rhs.as_ref())
    } else {
        (rhs.as_ref(), lhs.as_ref())
    };
    members.extend(member_chain(path_side, &PathNode::Current, true)?);
    Some((members, literal_scalar(literal_side)?))
}

/// `base` から始まるメンバーアクセスだけの連鎖（`base` 単独は `allow_bare` のときだけ空列で受理）。
fn member_chain(node: &PathNode, base: &PathNode, allow_bare: bool) -> Option<Vec<String>> {
    match node {
        n if n == base => allow_bare.then(Vec::new),
        PathNode::Path { base: b, ops } if b.as_ref() == base && !ops.is_empty() => ops
            .iter()
            .map(|op| match op {
                AccessorOp::Member(name) => Some(name.to_string()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// jsonpath のリテラルを比較スカラーへ変える。`null`・演算を含む式・数値として読めない綴りは `None`。
fn literal_scalar(node: &PathNode) -> Option<JsonValue> {
    match node {
        PathNode::String(s) => Some(JsonValue::String(s.to_string())),
        PathNode::Bool(b) => Some(JsonValue::Bool(*b)),
        PathNode::Numeric(n) => match crate::json::parse_json(n.as_str()).ok()? {
            v @ JsonValue::Number(_) => Some(v),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 候補探査
// ---------------------------------------------------------------------------

/// 候補行 1 件（`key_tenant`・行 `id`・行バイト列）を受け取り、探査を打ち切るなら `true` を返す訪問者。
type CandidateVisitor<'a> = dyn FnMut(&str, u64, &[u8]) -> Result<bool, SqlSurfaceError> + 'a;

/// 索引テーブルの読み取りハンドルの型境界。
trait GinTable: ReadableTable<(&'static str, &'static [u8]), &'static [u8]> {}
impl<T: ReadableTable<(&'static str, &'static [u8]), &'static [u8]>> GinTable for T {}

/// 索引・行テーブルの内部エラーを固定文言へ写す（値・id・テナント・索引内容を含めない）。
fn probe_failure() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "gin index lookup failed".to_string(),
    }
}

fn storage_failure(e: impl std::fmt::Display) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("scan row scan failed: {e}"),
    }
}

// 単体テスト専用の計測フック（索引経由の探査に入ったテナント数・行範囲走査へ縮退したテナント数）。
// 「実際に索引経路を通ること」「縮退が起きたこと」を行数に依存しない証拠として示す。本番ビルドには
// 含まれない。`cargo test` はテストごとに別スレッドのためスレッドローカルにする。
#[cfg(test)]
thread_local! {
    pub(crate) static INDEX_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static RANGE_FALLBACKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_index_probe() {
    INDEX_PROBES.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
#[inline(always)]
fn count_index_probe() {}

#[cfg(test)]
fn count_range_fallback() {
    RANGE_FALLBACKS.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
#[inline(always)]
fn count_range_fallback() {}

/// GIN 候補行を、テナントの昇順・各テナント内は rowid の昇順で `visit` へ渡す。
///
/// `visit(key_tenant, id, buf)` は候補 1 行ごとに呼ばれ、`true` を返すと探査を打ち切る（`LIMIT` 到達）。
/// `rows` は呼び出し元が開いた `user_rows/{table}`。`source` から `user_gin/{索引名}` を**読み取り専用**で
/// 開く（`ReadSource::open_table` は書き込み txn でも表を作らない）。索引テーブルが無い・マーカーが無効な
/// テナントは、そのテナントの行範囲走査に縮退する（結果は同じ）。
pub(crate) fn for_each_candidate<R, T>(
    source: &R,
    rows: &T,
    ctx: &PolicyContext,
    schema: &TableSchema,
    decl: &DeclaredGinIndex,
    keys: &GinKeys,
    visit: &mut CandidateVisitor<'_>,
) -> Result<(), SqlSurfaceError>
where
    R: ReadSource,
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let gin_table_name = catalog::user_gin_table_name(&decl.index_name);
    let index_table = match source.open_table(catalog::user_gin_table_def(&gin_table_name)) {
        Ok(t) => Some(t),
        // 索引テーブルが未作成（まだ一度も維持されていない）なら全テナントが縮退する。
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(_) => return Err(probe_failure()),
    };
    let probe = match index_table {
        Some(_) => Some(GinIndexProbe::for_read(schema, decl).map_err(|_| probe_failure())?),
        None => None,
    };
    let index = match (&index_table, &probe) {
        (Some(t), Some(p)) => Some((t, p)),
        _ => None,
    };

    // `ctx` が他テナントの Public 行を可視としないなら、可視行は自テナントに閉じる。
    if !ctx.allows_public() {
        visit_tenant(rows, index, ctx.tenant_id(), keys, visit)?;
        return Ok(());
    }

    // 全テナントを物理キーの昇順に列挙する。行テーブルから 1 ホップずつ次のテナントへ seek する
    // （索引テーブルから列挙すると、行はあるがマーカーが無いテナントを見落とす）。
    let mut after: Option<String> = None;
    loop {
        let start: Bound<(&str, u64)> = match &after {
            None => Bound::Unbounded,
            Some(t) => Bound::Excluded((t.as_str(), u64::MAX)),
        };
        let tenant = {
            let mut iter = rows
                .range::<(&str, u64)>((start, Bound::Unbounded))
                .map_err(storage_failure)?;
            match iter.next() {
                None => break,
                Some(entry) => {
                    let (k, _) = entry.map_err(storage_failure)?;
                    k.value().0.to_string()
                }
            }
        };
        if visit_tenant(rows, index, &tenant, keys, visit)? {
            break;
        }
        after = Some(tenant);
    }
    Ok(())
}

/// 1 テナント分の候補探査。`visit` が打ち切りを要求したら `Ok(true)`。
fn visit_tenant<T, I>(
    rows: &T,
    index: Option<(&I, &GinIndexProbe)>,
    tenant: &str,
    keys: &GinKeys,
    visit: &mut CandidateVisitor<'_>,
) -> Result<bool, SqlSurfaceError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: GinTable,
{
    if let Some((index_table, probe)) = index {
        let valid = probe
            .marker_valid(index_table, tenant)
            .map_err(|_| probe_failure())?;
        if valid {
            count_index_probe();
            return merge_candidates(rows, index_table, tenant, keys, visit);
        }
    }
    // 縮退: このテナントだけを範囲走査する。
    count_range_fallback();
    let start = Bound::Included((tenant, 0u64));
    let end = Bound::Included((tenant, u64::MAX));
    for entry in rows
        .range::<(&str, u64)>((start, end))
        .map_err(storage_failure)?
    {
        let (k, v) = entry.map_err(storage_failure)?;
        let (key_tenant, id) = k.value();
        if visit(key_tenant, id, v.value())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 候補 rowid 1 件を行ストアから引いて訪問者へ渡す。行が無いポスティングは stale（削除済み）で何も出さない。
fn visit_row<T>(
    rows: &T,
    tenant: &str,
    raw: u64,
    visit: &mut CandidateVisitor<'_>,
) -> Result<bool, SqlSurfaceError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let rowid = RowId::from_index_raw(raw);
    let Some(guard) = rows.get(row_key(tenant, rowid)).map_err(storage_failure)? else {
        return Ok(false);
    };
    // `visit` は下流（可視性・述語の再評価と検索経路）が未移行のため、従来方式の `id` で受ける
    // （rowid → `id` の出口。#2195 以降が rowid 受けへ移す。Issue #2194。`sql::pk_point` と同じ）。
    visit(tenant, rowid.legacy_id(), guard.value())
}

/// 索引テーブルの範囲反復器の型（ポスティング範囲 1 本分）。
type PostingRange<'i> = redb::Range<'i, (&'static str, &'static [u8]), &'static [u8]>;

/// 同一テナント・同一エントリのポスティング範囲を rowid の昇順に読むカーソル。
struct PostingCursor<'i, I: GinTable> {
    index: &'i I,
    tenant: &'i str,
    entry: &'i [u8],
    /// 範囲の閉区間の下端・上端サブキー。
    lo: Vec<u8>,
    hi: Vec<u8>,
    iter: PostingRange<'i>,
    /// 現在の先頭 rowid（`None` は範囲の終端）。
    head: Option<u64>,
}

/// `seek` が先頭へ進めるときに `next()` で刻む最大回数。これを超えたら範囲を開き直して飛ぶ。
const SEEK_LINEAR_STEPS: usize = 16;

impl<'i, I: GinTable> PostingCursor<'i, I> {
    fn open(index: &'i I, tenant: &'i str, entry: &'i [u8]) -> Result<Self, SqlSurfaceError> {
        let (lo, hi) = posting_range_bounds(entry).ok_or_else(probe_failure)?;
        let iter = Self::open_range(index, tenant, &lo, &hi)?;
        let mut cursor = Self {
            index,
            tenant,
            entry,
            lo,
            hi,
            iter,
            head: None,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    fn open_range(
        index: &'i I,
        tenant: &str,
        lo: &[u8],
        hi: &[u8],
    ) -> Result<PostingRange<'i>, SqlSurfaceError> {
        index
            .range::<(&str, &[u8])>((Bound::Included((tenant, lo)), Bound::Included((tenant, hi))))
            .map_err(|_| probe_failure())
    }

    /// 次のポスティングへ進む（終端なら `head = None`）。
    fn advance(&mut self) -> Result<(), SqlSurfaceError> {
        self.head = match self.iter.next() {
            None => None,
            Some(item) => {
                let (k, _) = item.map_err(|_| probe_failure())?;
                let (key_tenant, subkey) = k.value();
                // 範囲の構築上到達しないはずだが defense-in-depth で形を検証する（破損は fail-closed）。
                if key_tenant != self.tenant {
                    return Err(probe_failure());
                }
                let rowid = decode_posting_rowid(subkey, self.entry).ok_or_else(probe_failure)?;
                Some(rowid.to_index_raw())
            }
        };
        Ok(())
    }

    /// 先頭を `target` 以上の最初のポスティングへ進める（既に `target` 以上なら何もしない）。
    fn seek(&mut self, target: u64) -> Result<(), SqlSurfaceError> {
        for _ in 0..SEEK_LINEAR_STEPS {
            match self.head {
                Some(h) if h < target => self.advance()?,
                _ => return Ok(()),
            }
        }
        match self.head {
            Some(h) if h < target => {
                // 遠い場合は範囲を `target` から開き直す（B-tree を 1 回降りる）。
                let mut lo = self.lo.clone();
                let base = lo.len().checked_sub(8).ok_or_else(probe_failure)?;
                lo.truncate(base);
                lo.extend_from_slice(&target.to_be_bytes());
                self.iter = Self::open_range(self.index, self.tenant, &lo, &self.hi)?;
                self.advance()
            }
            _ => Ok(()),
        }
    }
}

/// マーカー有効なテナントの候補を、キー集合の結合規則に従って rowid の昇順に訪問者へ渡す。
/// メモリは O(キー数)（候補を実体化しない）。`visit` が打ち切りを要求したら `Ok(true)`。
fn merge_candidates<T, I>(
    rows: &T,
    index: &I,
    tenant: &str,
    keys: &GinKeys,
    visit: &mut CandidateVisitor<'_>,
) -> Result<bool, SqlSurfaceError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: GinTable,
{
    let (entries, is_and) = match keys {
        GinKeys::And(e) => (e, true),
        GinKeys::Or(e) => (e, false),
    };
    let mut cursors: Vec<PostingCursor<'_, I>> = Vec::with_capacity(entries.len());
    for entry in entries {
        cursors.push(PostingCursor::open(index, tenant, entry)?);
    }
    if is_and {
        intersect(rows, tenant, &mut cursors, visit)
    } else {
        union(rows, tenant, &mut cursors, visit)
    }
}

/// 積（leapfrog 交差）。いずれかのカーソルが終端に達したら終了する。
fn intersect<T, I>(
    rows: &T,
    tenant: &str,
    cursors: &mut [PostingCursor<'_, I>],
    visit: &mut CandidateVisitor<'_>,
) -> Result<bool, SqlSurfaceError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: GinTable,
{
    if cursors.is_empty() {
        return Ok(false);
    }
    loop {
        let mut target = 0u64;
        for c in cursors.iter() {
            match c.head {
                None => return Ok(false),
                Some(h) => target = target.max(h),
            }
        }
        let mut agreed = true;
        for c in cursors.iter_mut() {
            c.seek(target)?;
            match c.head {
                None => return Ok(false),
                Some(h) if h != target => agreed = false,
                Some(_) => {}
            }
        }
        if agreed {
            if visit_row(rows, tenant, target, visit)? {
                return Ok(true);
            }
            for c in cursors.iter_mut() {
                c.advance()?;
                if c.head.is_none() {
                    return Ok(false);
                }
            }
        }
    }
}

/// 和（ヒープによる k-way マージ。重複 rowid は 1 回だけ出す）。
fn union<T, I>(
    rows: &T,
    tenant: &str,
    cursors: &mut [PostingCursor<'_, I>],
    visit: &mut CandidateVisitor<'_>,
) -> Result<bool, SqlSurfaceError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: GinTable,
{
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    for (i, c) in cursors.iter().enumerate() {
        if let Some(h) = c.head {
            heap.push(Reverse((h, i)));
        }
    }
    let mut last: Option<u64> = None;
    while let Some(Reverse((raw, i))) = heap.pop() {
        if last != Some(raw) {
            last = Some(raw);
            if visit_row(rows, tenant, raw, visit)? {
                return Ok(true);
            }
        }
        let Some(c) = cursors.get_mut(i) else {
            return Err(probe_failure());
        };
        c.advance()?;
        if let Some(h) = c.head {
            heap.push(Reverse((h, i)));
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    //! 分類（全走査へ倒れる形の網羅・許可形の偽陰性なし）と、計測フックによる構造的な証拠
    //! （実際に索引経路を通ったこと・縮退が起きたこと・積と和のマージが全走査と一致すること）。
    use super::*;
    use crate::core::EngineCore;
    use crate::kernel::CpuScalarProvider;
    use crate::sql::mode::SessionState;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn core(label: &str) -> (EngineCore, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open");
        (
            EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
            guard,
        )
    }

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("tenant")
    }

    fn run(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> Vec<String> {
        match core.execute_sql_in_session(&ctx(tenant), s, sql) {
            Ok(crate::sql::SqlOutcome::Query(r)) => r
                .rows
                .iter()
                .map(|row| format!("{:?}", row.cells))
                .collect(),
            Ok(_) => Vec::new(),
            Err(e) => panic!("{sql}: {} {e:?}", e.wire_code()),
        }
    }

    fn reset() {
        INDEX_PROBES.with(|c| c.set(0));
        RANGE_FALLBACKS.with(|c| c.set(0));
    }

    fn counts() -> (usize, usize) {
        (
            INDEX_PROBES.with(|c| c.get()),
            RANGE_FALLBACKS.with(|c| c.get()),
        )
    }

    const DOCS: &[&str] = &[
        r#"{"a":1,"b":"x"}"#,
        r#"{"a":1.0,"b":"y"}"#,
        r#"{"a":2,"b":"x","c":[1,2,3]}"#,
        r#"{"a":{"n":"p"},"b":"z"}"#,
        r#"{"a":[{"n":"p"},{"n":"q"}]}"#,
        r#"["x","y"]"#,
        r#""x""#,
        r#"{"c":[3]}"#,
        r#"{}"#,
        r#"[]"#,
    ];

    fn setup(label: &str) -> (EngineCore, CleanupGuard, SessionState) {
        let (core, g) = core(label);
        let mut s = SessionState::default();
        s.allow_ddl();
        run(
            &core,
            "t1",
            &mut s,
            "CREATE TABLE g (id BIGINT PRIMARY KEY, doc JSONB)",
        );
        run(
            &core,
            "t1",
            &mut s,
            "CREATE TABLE p (id BIGINT PRIMARY KEY, doc JSONB)",
        );
        run(
            &core,
            "t1",
            &mut s,
            "CREATE INDEX g_ops ON g USING gin (doc)",
        );
        run(
            &core,
            "t1",
            &mut s,
            "CREATE INDEX g_path ON g USING gin (doc jsonb_path_ops)",
        );
        for (i, d) in DOCS.iter().enumerate() {
            for t in ["g", "p"] {
                run(
                    &core,
                    "t1",
                    &mut s,
                    &format!("INSERT INTO {t} (id, doc) VALUES ({}, '{d}')", i + 1),
                );
            }
        }
        (core, g, s)
    }

    /// 索引あり（g）となし（p）で結果が一致し、g では索引経路を通ること。
    fn assert_same(
        core: &EngineCore,
        s: &mut SessionState,
        where_clause: &str,
        expect_index: bool,
    ) {
        let where_clause = &if where_clause.contains("LIMIT") {
            where_clause.to_string()
        } else {
            format!("{where_clause} LIMIT 1000")
        };
        reset();
        let with = run(
            core,
            "t1",
            s,
            &format!("SELECT id, doc FROM g WHERE {where_clause}"),
        );
        let c = counts();
        let without = run(
            core,
            "t1",
            s,
            &format!("SELECT id, doc FROM p WHERE {where_clause}"),
        );
        assert_eq!(with, without, "{where_clause}");
        if expect_index {
            assert_eq!(c, (1, 0), "index path expected for {where_clause}");
        } else {
            assert_eq!(c, (0, 0), "full scan expected for {where_clause}");
        }
    }

    #[test]
    fn indexed_results_match_full_scan_and_use_the_index() {
        let (core, _g, mut s) = setup("gin-unit-match");
        for w in [
            "doc @> '{\"a\":1}'",
            "doc @> '{\"a\":{\"n\":\"p\"}}'",
            "doc @> '{\"a\":[{\"n\":\"q\"}]}'",
            "doc @> '[\"x\"]'",
            "doc @> '\"x\"'",
            "doc @> '{\"c\":[1,3]}'",
            "doc ? 'b'",
            "doc ? 'x'",
            "doc ?| '{b,c,x}'",
            "doc ?& '{a,b}'",
            "doc @@ '$.a == 1'",
            "doc @? '$.a ? (@.n == \"p\")'",
            "doc @@ '$.b == \"x\"'",
        ] {
            assert_same(&core, &mut s, w, true);
        }
        // 全走査へ倒れる形。
        for w in [
            "doc @> '{}'",
            "doc <@ '{\"a\":1,\"b\":\"x\"}'",
            "'{\"a\":1}' @> doc",
            "doc ?& '{}'",
            "doc @@ '$.a > 1'",
            "doc @@ 'strict $.a == 1'",
            "doc @@ '$.a[0] == 1'",
            "doc @@ '$.a == null'",
            "doc @? '$.a.n'",
            "jsonb_path_exists(doc, '$.a ? (@ == 1)')",
            "doc @> '{\"a\":1}' OR doc ? 'b'",
        ] {
            assert_same(&core, &mut s, w, false);
        }
    }

    #[test]
    fn marker_loss_falls_back_to_range_scan_with_same_result() {
        let (core, _g, mut s) = setup("gin-unit-fallback");
        // RENAME COLUMN はシグネチャを不一致にする（次の書き込みまで遅延再構築されない）。
        run(
            &core,
            "t1",
            &mut s,
            "ALTER TABLE g RENAME COLUMN doc TO body",
        );
        run(
            &core,
            "t1",
            &mut s,
            "ALTER TABLE p RENAME COLUMN doc TO body",
        );
        reset();
        let with = run(
            &core,
            "t1",
            &mut s,
            "SELECT id, body FROM g WHERE body @> '{\"a\":1}' LIMIT 100",
        );
        assert_eq!(counts(), (0, 1));
        let without = run(
            &core,
            "t1",
            &mut s,
            "SELECT id, body FROM p WHERE body @> '{\"a\":1}' LIMIT 100",
        );
        assert_eq!(with, without);
        assert!(!with.is_empty());
    }

    #[test]
    fn stale_postings_are_rechecked_after_update_and_delete() {
        let (core, _g, mut s) = setup("gin-unit-stale");
        for t in ["g", "p"] {
            run(
                &core,
                "t1",
                &mut s,
                &format!("UPDATE {t} SET doc = '{{\"a\":9}}' WHERE id = 1"),
            );
            run(
                &core,
                "t1",
                &mut s,
                &format!("DELETE FROM {t} WHERE id = 2"),
            );
        }
        assert_same(&core, &mut s, "doc @> '{\"a\":1}'", true);
        assert_same(&core, &mut s, "doc @> '{\"a\":9}'", true);
    }

    #[test]
    fn many_rows_and_limit_offset_match_with_or_and_intersection() {
        let (core, _g, mut s) = setup("gin-unit-many");
        for i in 100..400u32 {
            let d = match i % 5 {
                0 => format!("{{\"a\":{},\"b\":\"x\"}}", i % 7),
                1 => format!("{{\"b\":\"x\",\"k{}\":1}}", i % 3),
                2 => "{\"c\":[1,2]}".to_string(),
                3 => format!("[\"x\",{}]", i % 4),
                _ => "{\"a\":1,\"c\":[3]}".to_string(),
            };
            for t in ["g", "p"] {
                run(
                    &core,
                    "t1",
                    &mut s,
                    &format!("INSERT INTO {t} (id, doc) VALUES ({i}, '{d}')"),
                );
            }
        }
        for w in [
            "doc @> '{\"a\":1,\"b\":\"x\"}'",
            "doc ?| '{k0,k1,c}' LIMIT 7",
            "doc ?| '{k0,k1,c}' LIMIT 7 OFFSET 5",
            "doc ?& '{a,c}' LIMIT 3 OFFSET 2",
            "doc @> '{\"c\":[1,2]}' LIMIT 10",
            "doc @@ '$.a == 1'",
        ] {
            assert_same(&core, &mut s, w, true);
        }
    }

    /// 一方のポスティングが極端に長く他方が短い積では、長い側が `SEEK_LINEAR_STEPS` を超えて飛ぶ
    /// （範囲を開き直す経路）。結果は全走査と一致する。
    #[test]
    fn intersection_with_a_long_and_a_short_posting_list_seeks_across_the_gap() {
        let (core, _g, mut s) = setup("gin-unit-seek");
        for i in 1000..1300u32 {
            let d = if i % 100 == 0 {
                r#"{"common":1,"rare":1}"#
            } else {
                r#"{"common":1}"#
            };
            for t in ["g", "p"] {
                run(
                    &core,
                    "t1",
                    &mut s,
                    &format!("INSERT INTO {t} (id, doc) VALUES ({i}, '{d}')"),
                );
            }
        }
        assert_same(&core, &mut s, "doc ?& '{common,rare}'", true);
        assert_same(
            &core,
            &mut s,
            "doc ?& '{common,rare}' LIMIT 2 OFFSET 1",
            true,
        );
        assert_same(
            &core,
            &mut s,
            "doc ?| '{common,rare}' LIMIT 50 OFFSET 40",
            true,
        );
    }

    #[test]
    fn other_tenants_private_rows_are_not_returned() {
        let (core, _g, mut s) = setup("gin-unit-tenant");
        run(
            &core,
            "t2",
            &mut s,
            "INSERT INTO g (id, doc) VALUES (500, '{\"a\":1}')",
        );
        run(
            &core,
            "t2",
            &mut s,
            "INSERT INTO p (id, doc) VALUES (500, '{\"a\":1}')",
        );
        for tenant in ["t1", "t2"] {
            let a = run(
                &core,
                tenant,
                &mut s,
                "SELECT id FROM g WHERE doc @> '{\"a\":1}' LIMIT 100",
            );
            let b = run(
                &core,
                tenant,
                &mut s,
                "SELECT id FROM p WHERE doc @> '{\"a\":1}' LIMIT 100",
            );
            assert_eq!(a, b, "tenant {tenant}");
        }
    }

    /// 許可形の jsonpath は、評価器が真を返す文書の `jsonb_path_ops` エントリに必ず含まれる葉エントリを作る
    /// （偽陰性なし）。許可形でない式は `None`（全走査）。
    #[test]
    fn jsonpath_allowed_shapes_never_miss_a_matching_document() {
        use crate::gin_index::extract::{extract_entries, GinLimits};
        use crate::json::jsonpath::{
            jsonpath_exists, jsonpath_match, parse_jsonpath, JsonPathEvalOptions,
        };
        let docs: Vec<&str> = DOCS
            .iter()
            .copied()
            .chain([
                r#"{"a":[[{"n":"p"}]]}"#,
                r#"{"a":[1,2],"b":true}"#,
                r#"{"a":{"b":{"c":1.0}}}"#,
                r#"[{"a":1},{"a":2}]"#,
                r#"{"a":"1"}"#,
                r#"{"a":10e-1}"#,
            ])
            .collect();
        let matches = [
            "$.a == 1",
            "1 == $.a",
            "$.a.b.c == 1",
            "$.b == true",
            "$.a == \"1\"",
            "$.a.n == \"p\"",
            "$.a == 1.0",
        ];
        let exists = [
            "$ ? (@.a == 1)",
            "$.a ? (@ == 1)",
            "$.a ? (@.n == \"p\")",
            "$.a ? (1 == @.n)",
            "$.a.b ? (@.c == 1)",
            "$ ? (@ == 1)",
        ];
        let limits = GinLimits::default();
        let mut hits = 0;
        for (exprs, is_match) in [(&matches[..], true), (&exists[..], false)] {
            for text in exprs {
                let path = parse_jsonpath(text).unwrap();
                let leaf = if is_match {
                    match_shape(&path.expr)
                } else {
                    exists_shape(&path.expr)
                };
                let (members, scalar) = leaf.unwrap_or_else(|| panic!("allowed shape: {text}"));
                let entry =
                    query_path_leaf_entry(members.iter().map(String::as_str), &scalar).unwrap();
                for d in &docs {
                    let root = crate::json::parse_json(d).unwrap();
                    let opts = || JsonPathEvalOptions::new().with_silent(true);
                    let r = if is_match {
                        jsonpath_match(&path, &root, opts())
                    } else {
                        jsonpath_exists(&path, &root, opts())
                    };
                    if r.ok().flatten() == Some(true) {
                        hits += 1;
                        let stored =
                            extract_entries(GinOpClass::JsonbPathOps, &root, &limits).unwrap();
                        assert!(stored.contains(&entry), "false negative: {text} on {d}");
                    }
                }
            }
        }
        assert!(hits > 10, "corpus must produce matches: {hits}");
        // 許可形でないものは全走査へ倒す。
        for text in [
            "$.a > 1",
            "$.a != 1",
            "$.a == null",
            "$.a[0] == 1",
            "$.* == 1",
            "$.a == $.b",
            "$.a == -1",
            "$.a == 1 && $.b == 2",
            "exists($.a)",
            "$.a.size() == 1",
            "$.a ? (@ > 1)",
        ] {
            let path = parse_jsonpath(text).unwrap();
            assert!(
                match_shape(&path.expr).is_none() && exists_shape(&path.expr).is_none(),
                "{text}"
            );
        }
    }
}
