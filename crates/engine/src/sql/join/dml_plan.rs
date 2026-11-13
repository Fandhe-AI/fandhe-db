//! 束縛済み `UPDATE ... FROM`／`DELETE ... USING` から結合 DML の実行入力（[`JoinedDmlPlan`]）への
//! 変換器（SQL-53 (b)・RLS-10、Issue #3201。親 #3200）。
//!
//! 責務境界: `sql::dml_from` の束縛結果（[`BoundUpdateFrom`]・[`BoundDeleteUsing`]）と、呼び出し元が
//! 同じスナップショットから取得したスキーマだけを入力にする純関数で、行・RLS・台帳・書き込み
//! トランザクションには一切触れない。実行は `sql::join::dml`（[`JoinedProbe`]）と
//! `core::dml_join_exec`、SQL 経路への結線は `core::dml_from_exec`（この変換器を呼ぶ）が担う。
//!
//! WHERE の振り分け（最上位の `AND` で平坦化した conjunct ごと。判定は解決済みの relation 添字だけで
//! 行い、表名では行わない。自己結合と疑似列 `id`／実列 `id` の取り違えを防ぐ）:
//! - 別 relation の列同士の等値 → [`EquiKey`]（結合キー）
//! - 対象側だけを参照する述語（`OR` を含んでよい） → 対象側フィルタ（既存の述語形 DML と同じ束縛器）
//! - source 側だけを参照する述語 → [`JoinedDmlSource::where_predicates`]（走査へ押し下げ）
//! - それ以外（relation をまたぐ非等値・`OR`、同じ relation 内の列比較、等値キーの無い形）は
//!   すべて `0A000` で拒否する。取りこぼした述語は書き込み範囲を広げる（fail-open になる）ため、
//!   分類は全域にして未分類を必ず拒否する。
//!
//! SET: リテラルと対象表の列だけを使う式はそのまま運び、列参照（右辺が source 列のものを含む）は
//! 「対象列 ++ source の取り出し列」の合成スキーマ上の式として束縛する。エラー文言は固定文言と
//! 入力された名前だけで、他テナントの存在・件数・行 id を含めない（変換はデータに依存しない）。

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::allowlist::{JoinWhereExpr, JoinWherePredicate, SqlSurfaceError, WherePredicate};
use crate::sql::dml_from::{build_scope, BoundDeleteUsing, BoundUpdateFrom};
use crate::sql::relation::{ColumnRef, ColumnSlot, ResolvedColumn, TableRef};
use crate::sql::udf_call::{BinOp, Expr, UdfRegistry};

use super::dml::{
    EquiKey, JoinedDmlPlan, JoinedDmlSource, JoinedDmlTarget, JoinedSetItem, JoinedUpdateSet,
};
use super::plan::{collect_leaf_colrefs, flatten_conjuncts, slot_name, to_where_predicates};
use super::using::JoinScope;
use super::values::{cell_scalar_supports, cmp_class, key_class};
use super::JoinLimits;

/// FROM／USING が 2 表以上の形の固定文言（`0A000`）。
const MULTI_FROM: &str =
    "UPDATE ... FROM / DELETE ... USING with more than one FROM / USING table is not supported yet";
/// 等値結合キーが無い形の固定文言（`0A000`）。
const NO_EQUI_KEY: &str =
    "UPDATE ... FROM / DELETE ... USING requires an equality join condition between the tables";
/// 振り分けられない述語の固定文言（`0A000`）。
const UNSUPPORTED_WHERE: &str =
    "UPDATE ... FROM / DELETE ... USING WHERE supports only equality join keys and single-table predicates";

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// WHERE の振り分け結果。
struct Split {
    target: Vec<WherePredicate>,
    source: Vec<WherePredicate>,
    keys: Vec<EquiKey>,
}

/// 部分木に列同士の比較が含まれるか。
fn contains_column_compare(expr: &JoinWhereExpr) -> bool {
    match expr {
        JoinWhereExpr::Leaf(p) => matches!(p, JoinWherePredicate::ColumnCompare { .. }),
        JoinWhereExpr::And(ch) | JoinWhereExpr::Or(ch) => ch.iter().any(contains_column_compare),
    }
}

/// 部分木が参照する relation（対象側・source 側）。relation 添字が 2 以上なら `Internal`。
fn relation_set(
    expr: &JoinWhereExpr,
    scope: &JoinScope<'_>,
) -> Result<(bool, bool), SqlSurfaceError> {
    let mut refs: Vec<&ColumnRef> = Vec::new();
    collect_leaf_colrefs(expr, &mut refs);
    let (mut target, mut source) = (false, false);
    for c in refs {
        match scope.resolve(c)?.relation() {
            0 => target = true,
            1 => source = true,
            _ => return Err(internal("relation index out of range in DML join WHERE")),
        }
    }
    Ok((target, source))
}

/// 結合キー列 1 辺の検査（照合は 0A000）。
fn check_key_side(
    schema: &TableSchema,
    slot: ColumnSlot,
) -> Result<Option<usize>, SqlSurfaceError> {
    match slot {
        ColumnSlot::Id => Ok(None),
        ColumnSlot::Column(i) => {
            // ハッシュ照合はバイト等価のため、非バイト列順の照合の列はキーにできない。
            crate::sql::udf_call::require_wired_collation(
                schema.columns.get(i).and_then(|c| c.collation),
            )?;
            Ok(Some(i))
        }
    }
}

/// 別 relation の列同士の等値を [`EquiKey`] にする（向きは対象側 `target_column`・source 側
/// `source_column` に正規化）。型クラスが比較できなければ `42804`、キー化できなければ `0A000`。
fn equi_key(
    lhs: &ResolvedColumn,
    rhs: &ResolvedColumn,
    target_schema: &TableSchema,
    source_schema: &TableSchema,
) -> Result<EquiKey, SqlSurfaceError> {
    let (t, s) = match (lhs.relation(), rhs.relation()) {
        (0, 1) => (lhs, rhs),
        (1, 0) => (rhs, lhs),
        _ => return Err(internal("join key columns are not on distinct relations")),
    };
    match (cmp_class(t.column_type()), cmp_class(s.column_type())) {
        (Some(a), Some(b)) if a == b => {}
        (Some(_), Some(_)) => {
            return Err(SqlSurfaceError::datatype_mismatch(
                "JOIN key columns have incompatible types",
            ))
        }
        _ => {
            return Err(SqlSurfaceError::feature_not_supported(
                "this column type cannot be used as a JOIN key",
            ))
        }
    }
    let unkeyable =
        |_| SqlSurfaceError::feature_not_supported("this column type cannot be used as a JOIN key");
    let tc = key_class(t.column_type()).map_err(unkeyable)?;
    let sc = key_class(s.column_type()).map_err(unkeyable)?;
    if tc != sc {
        return Err(SqlSurfaceError::feature_not_supported(
            "this column type cannot be used as a JOIN key",
        ));
    }
    Ok(EquiKey {
        target_column: check_key_side(target_schema, t.slot())?,
        source_column: check_key_side(source_schema, s.slot())?,
    })
}

/// WHERE を結合キー・対象側述語・source 側述語へ振り分ける（判定順は文書冒頭のとおり決定的）。
fn split_where(
    relations: &[TableRef],
    where_clause: Option<&JoinWhereExpr>,
    target_schema: &TableSchema,
    other_schemas: &[&TableSchema],
) -> Result<Split, SqlSurfaceError> {
    let (Some(target_ref), [source_ref]) = (relations.first(), relations.get(1..).unwrap_or(&[]))
    else {
        return Err(SqlSurfaceError::feature_not_supported(MULTI_FROM));
    };
    let [source_schema] = other_schemas else {
        return Err(internal("DML join schema count mismatch"));
    };
    let Some(where_clause) = where_clause else {
        return Err(SqlSurfaceError::feature_not_supported(NO_EQUI_KEY));
    };
    let scope = JoinScope::plain(build_scope(
        target_ref,
        target_schema,
        std::slice::from_ref(source_ref),
        &[*source_schema],
    )?);

    let mut conjuncts: Vec<&JoinWhereExpr> = Vec::new();
    flatten_conjuncts(where_clause, &mut conjuncts);
    let mut split = Split {
        target: Vec::new(),
        source: Vec::new(),
        keys: Vec::new(),
    };
    let schemas = [target_schema, *source_schema];
    for c in conjuncts {
        if let JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare { lhs, op, rhs }) = c {
            let (l, r) = (scope.resolve(lhs)?, scope.resolve(rhs)?);
            if l.relation() != r.relation() && *op == BinOp::Eq {
                split
                    .keys
                    .push(equi_key(&l, &r, target_schema, source_schema)?);
                continue;
            }
            return Err(SqlSurfaceError::feature_not_supported(UNSUPPORTED_WHERE));
        }
        if contains_column_compare(c) {
            return Err(SqlSurfaceError::feature_not_supported(UNSUPPORTED_WHERE));
        }
        match relation_set(c, &scope)? {
            (true, false) => split
                .target
                .extend(to_where_predicates(c, &scope, &schemas)?),
            (false, true) => split
                .source
                .extend(to_where_predicates(c, &scope, &schemas)?),
            _ => return Err(SqlSurfaceError::feature_not_supported(UNSUPPORTED_WHERE)),
        }
    }
    if split.keys.is_empty() {
        return Err(SqlSurfaceError::feature_not_supported(NO_EQUI_KEY));
    }
    Ok(split)
}

/// 対象側述語を対象表のスキーマで束縛して [`JoinedDmlTarget`] を作る（既存の述語形 DML と同じ束縛器。
/// embedding を参照する述語は候補列挙の前提が崩れるため `0A000`）。
fn build_target(
    table: &str,
    schema: &TableSchema,
    preds: &[WherePredicate],
    udfs: &UdfRegistry,
) -> Result<JoinedDmlTarget, SqlSurfaceError> {
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (metadata_filters, expr_filters, _rls, or_filters) =
        crate::sql::parser::bind_where_predicates(preds, schema, udfs, &mut node_budget, &[])?;
    if expr_filters
        .iter()
        .any(crate::sql::udf_call::references_embedding)
        || or_filters.iter().any(|g| g.references_embedding())
    {
        return Err(SqlSurfaceError::feature_not_supported(
            "UPDATE ... FROM / DELETE ... USING WHERE cannot reference a VECTOR column",
        ));
    }
    Ok(JoinedDmlTarget {
        table: table.to_string(),
        schema: schema.clone(),
        metadata_filters,
        expr_filters,
        or_filters,
    })
}

/// source 側述語を検証する（束縛結果は捨て、実行時の側スキャンと同じ型解析をここで前倒しする）。
fn build_source(
    table: &str,
    schema: &TableSchema,
    preds: Vec<WherePredicate>,
    columns: Vec<usize>,
    udfs: &UdfRegistry,
) -> Result<JoinedDmlSource, SqlSurfaceError> {
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    crate::sql::parser::bind_where_predicates(&preds, schema, udfs, &mut node_budget, &[])?;
    Ok(JoinedDmlSource {
        table: table.to_string(),
        schema: schema.clone(),
        where_predicates: preds,
        columns,
    })
}

/// source 列が SET 式の入力として結合ビューへ写像できる型か（`sql::join::dml::source_scalar` が
/// 扱う型と一致させる。写像できない型は実行時の `XX000` にせず変換の段階で拒否する）。
fn source_type_supported(ty: &ColumnType) -> bool {
    cell_scalar_supports(ty)
        || matches!(
            ty,
            ColumnType::Time
                | ColumnType::TimestampTz
                | ColumnType::Interval
                | ColumnType::Jsonb
                | ColumnType::Array(_)
        )
}

/// 結合ビュー上の source 列の合成名。利用者の識別子と衝突しない綴り（NUL を含む）にする。
fn synthetic_name(position: usize) -> String {
    format!("\u{0}src{position}")
}

/// SET 句を [`JoinedUpdateSet`] へ変換し、取り出す source 列（初出順・重複なし）を返す。
fn plan_set(
    bound: &BoundUpdateFrom,
    target_schema: &TableSchema,
    source_schema: &TableSchema,
) -> Result<(JoinedUpdateSet, Vec<usize>), SqlSurfaceError> {
    let mut set = JoinedUpdateSet {
        literal_assignments: bound.set_literals().to_vec(),
        items: bound
            .set_exprs()
            .iter()
            .map(|(i, e)| JoinedSetItem {
                column_index: *i,
                set: e.clone(),
            })
            .collect(),
    };
    // 合成スキーマ: 対象表 ++ source の取り出し列。対象列の添字は不変で、末尾への追記だけを行う。
    let mut combined = target_schema.clone();
    let mut columns: Vec<usize> = Vec::new();
    for (target_idx, resolved) in bound.set_columns() {
        let target_col = target_schema
            .columns
            .get(*target_idx)
            .ok_or_else(|| internal("SET column out of range"))?;
        let name = match resolved.relation() {
            0 => slot_name(resolved.slot(), target_schema)?,
            1 => match resolved.slot() {
                // 結合ビューは source の疑似列 `id` を運ばない。
                ColumnSlot::Id => return Err(SqlSurfaceError::feature_not_supported(
                    "UPDATE ... FROM: the pseudo column id of the FROM table cannot be used in SET",
                )),
                ColumnSlot::Column(i) => {
                    let col = source_schema
                        .columns
                        .get(i)
                        .ok_or_else(|| internal("source column out of range"))?;
                    if !source_type_supported(&col.ty) {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "UPDATE ... FROM: the FROM-table column type is not supported in SET",
                        ));
                    }
                    let pos = match columns.iter().position(|&c| c == i) {
                        Some(p) => p,
                        None => {
                            columns.push(i);
                            combined.columns.push(ColumnDef::new(
                                synthetic_name(columns.len() - 1),
                                col.ty.clone(),
                                true,
                            ));
                            columns.len() - 1
                        }
                    };
                    synthetic_name(pos)
                }
            },
            _ => return Err(internal("SET relation index out of range")),
        };
        let bound_set = crate::sql::set_expr::bind_set_expr(
            &target_col.name,
            &Expr::Ident(name),
            target_col,
            &combined,
        )?;
        set.items.push(JoinedSetItem {
            column_index: *target_idx,
            set: bound_set,
        });
    }
    Ok((set, columns))
}

/// 組み立てた計画の形を検証して返す（添字の範囲外は変換の段階で `Internal` にする）。
fn finish(plan: JoinedDmlPlan) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    plan.validate(&JoinLimits::default())?;
    Ok(plan)
}

/// 束縛済みの `UPDATE ... FROM` を [`JoinedDmlPlan`] へ変換する。呼び出し元は
/// `core::dml_from_exec`（同じスナップショットのスキーマを渡す）。
pub(crate) fn plan_update_from(
    bound: &BoundUpdateFrom,
    target_schema: &TableSchema,
    from_schemas: &[&TableSchema],
    udfs: &UdfRegistry,
) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    let split = split_where(
        bound.relations(),
        bound.where_clause(),
        target_schema,
        from_schemas,
    )?;
    let ([target_ref, source_ref], [source_schema]) = (bound.relations(), from_schemas) else {
        return Err(SqlSurfaceError::feature_not_supported(MULTI_FROM));
    };
    let target = build_target(target_ref.table(), target_schema, &split.target, udfs)?;
    let (update, columns) = plan_set(bound, target_schema, source_schema)?;
    let source = build_source(
        source_ref.table(),
        source_schema,
        split.source,
        columns,
        udfs,
    )?;
    finish(JoinedDmlPlan {
        target,
        source,
        keys: split.keys,
        update: Some(update),
        returning_references_source: false,
    })
}

/// 束縛済みの `DELETE ... USING` を [`JoinedDmlPlan`] へ変換する（`update` は `None`）。
pub(crate) fn plan_delete_using(
    bound: &BoundDeleteUsing,
    target_schema: &TableSchema,
    using_schemas: &[&TableSchema],
    udfs: &UdfRegistry,
) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    let split = split_where(
        bound.relations(),
        bound.where_clause(),
        target_schema,
        using_schemas,
    )?;
    let ([target_ref, source_ref], [source_schema]) = (bound.relations(), using_schemas) else {
        return Err(SqlSurfaceError::feature_not_supported(MULTI_FROM));
    };
    let target = build_target(target_ref.table(), target_schema, &split.target, udfs)?;
    let source = build_source(
        source_ref.table(),
        source_schema,
        split.source,
        Vec::new(),
        udfs,
    )?;
    finish(JoinedDmlPlan {
        target,
        source,
        keys: split.keys,
        update: None,
        returning_references_source: false,
    })
}

#[cfg(test)]
#[path = "dml_plan_tests.rs"]
mod tests;
