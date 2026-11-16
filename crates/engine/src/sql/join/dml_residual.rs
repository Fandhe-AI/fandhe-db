//! `UPDATE ... FROM`／`DELETE ... USING` の WHERE の述語木を、通常の WHERE と同じ束縛器で評価できる
//! 形へ変換するヘルパ（SQL-53 (b)・RLS-10、Issue #3295。親 #3290）。
//!
//! 責務境界: `sql::join::dml_plan`（変換器）が WHERE を振り分けたあとに呼ぶ純関数だけを持ち、行・RLS・
//! 台帳・書き込みトランザクションには触れない。実行は `sql::join::dml`（[`super::dml::JoinedProbe`]）が
//! 対象行 × source 行の組ごとに [`super::dml::JoinedResidual`] を評価する。
//!
//! 方式: 述語木の部分木（列同士の比較・数値リテラルとの比較・`IS [NOT] NULL`・`OR`）を
//! [`WherePredicate`] 列へ変換し、「対象表のスキーマ ++ 残余が参照する source 列」の合成スキーマで
//! `bind_where_predicates`（通常の単一表 WHERE と同じ束縛器）へ渡す。NULL の三値論理・型強制・照合・
//! 数値リテラルの解釈が通常の WHERE と構造的に一致し、第 2 の評価器を作らない。合成列名は NUL を含む
//! 綴りで利用者の識別子と衝突しない。
//!
//! fail-closed: source の疑似列 `id`・式評価ビューへ写像できない型（VECTOR・配列等）の参照は固定文言の
//! `0A000` で拒否する。エラー文言は固定文言と入力された名前だけで、件数・値・他テナントの存在を含めない。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{JoinWhereExpr, JoinWherePredicate, SqlSurfaceError, WherePredicate};
use crate::sql::relation::{ColumnRef, ColumnSlot};
use crate::sql::udf_call::{BinOp, Expr, UdfRegistry};

use super::dml::JoinedResidual;
use super::plan::{leaf_colrefs, leaf_to_where, slot_name};
use super::using::JoinScope;
use super::values::cell_scalar_supports;

/// 列名の解決関数（`ColumnRef` → 評価対象スキーマ上の列名）。
pub(super) type NameOf<'a> = dyn FnMut(&ColumnRef) -> Result<String, SqlSurfaceError> + 'a;

/// 述語木を [`WherePredicate`] 列へ変換する。`AND` は平坦化、`OR` は分岐ごとの `Vec` を持つ
/// [`WherePredicate::Or`]、列同士の比較は `Expression(Binary(Ident, Ident))`（通常の単一表 WHERE の
/// 列比較と同じ表現）、それ以外の葉は SELECT の JOIN と共有する [`leaf_to_where`] に委ねる。
pub(super) fn convert(
    expr: &JoinWhereExpr,
    name_of: &mut NameOf<'_>,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    match expr {
        JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare { lhs, op, rhs }) => {
            let (l, r) = (name_of(lhs)?, name_of(rhs)?);
            Ok(vec![column_compare(*op, l, r)])
        }
        JoinWhereExpr::Leaf(pred) => {
            let colref =
                leaf_colrefs(pred)
                    .first()
                    .copied()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN WHERE leaf without a column".to_string(),
                    })?;
            let name = name_of(colref)?;
            Ok(vec![leaf_to_where(pred, name)?])
        }
        JoinWhereExpr::And(ch) => {
            let mut out = Vec::new();
            for c in ch {
                out.extend(convert(c, name_of)?);
            }
            Ok(out)
        }
        JoinWhereExpr::Or(ch) => {
            let mut branches = Vec::with_capacity(ch.len());
            for c in ch {
                branches.push(convert(c, name_of)?);
            }
            Ok(vec![WherePredicate::Or(branches)])
        }
    }
}

fn column_compare(op: BinOp, lhs: String, rhs: String) -> WherePredicate {
    WherePredicate::Expression(Expr::Binary {
        op,
        lhs: Box::new(Expr::Ident(lhs)),
        rhs: Box::new(Expr::Ident(rhs)),
    })
}

/// 残余が参照する source 列の型を式評価ビュー（`source_scalar`）へ写像できるか。VECTOR・配列は
/// 写像できないため拒否する（実行時の `XX000` にせず変換の段階で `0A000` にする）。
fn residual_type_supported(ty: &ColumnType) -> bool {
    cell_scalar_supports(ty)
        || matches!(
            ty,
            ColumnType::Time | ColumnType::TimestampTz | ColumnType::Interval | ColumnType::Jsonb
        )
}

/// 残余の合成名（source 列の並び位置）。
fn synthetic_name(position: usize) -> String {
    format!("\u{0}res{position}")
}

/// 対象と source をまたぐ conjunct 群を、合成スキーマ上で束縛した [`JoinedResidual`] にする。
/// 呼び出し元は `dml_plan::split_where`（conjunct は最上位の `AND` で平坦化済み）。
pub(super) fn plan_residual(
    conjuncts: &[&JoinWhereExpr],
    scope: &JoinScope<'_>,
    target_schema: &TableSchema,
    source_schema: &TableSchema,
    udfs: &UdfRegistry,
) -> Result<JoinedResidual, SqlSurfaceError> {
    // 合成スキーマ: 対象表 ++ 参照される source 列。対象列の添字は不変で末尾へ追記する。
    let mut combined = target_schema.clone();
    let mut columns: Vec<usize> = Vec::new();
    let mut preds: Vec<WherePredicate> = Vec::new();
    for c in conjuncts {
        let mut name_of = |r: &ColumnRef| -> Result<String, SqlSurfaceError> {
            let resolved = scope.resolve(r)?;
            match resolved.relation() {
                0 => slot_name(resolved.slot(), target_schema),
                1 => match resolved.slot() {
                    ColumnSlot::Id => Err(SqlSurfaceError::feature_not_supported(
                        "UPDATE ... FROM / DELETE ... USING: the pseudo column id of the FROM table cannot be used in a cross-table condition",
                    )),
                    ColumnSlot::Column(i) => {
                        let col = source_schema.columns.get(i).ok_or_else(|| {
                            SqlSurfaceError::Internal {
                                detail: "source column out of range".to_string(),
                            }
                        })?;
                        if !residual_type_supported(&col.ty) {
                            return Err(SqlSurfaceError::feature_not_supported(
                                "UPDATE ... FROM / DELETE ... USING: the FROM-table column type is not supported in a cross-table condition",
                            ));
                        }
                        let pos = match columns.iter().position(|&x| x == i) {
                            Some(p) => p,
                            None => {
                                columns.push(i);
                                let pos = columns.len() - 1;
                                // 照合・型を引き継ぐ（source 列の比較規則を落とさない）。
                                let mut def = col.clone();
                                def.name = synthetic_name(pos);
                                def.nullable = true;
                                combined.columns.push(def);
                                pos
                            }
                        };
                        Ok(synthetic_name(pos))
                    }
                },
                _ => Err(SqlSurfaceError::Internal {
                    detail: "relation index out of range in DML join WHERE".to_string(),
                }),
            }
        };
        preds.extend(convert(c, &mut name_of)?);
    }
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (metadata_filters, expr_filters, _rls, or_filters) =
        crate::sql::parser::bind_where_predicates(&preds, &combined, udfs, &mut node_budget, &[])?;
    // 候補行ごとの評価では embedding を復号しないため、VECTOR 列の参照は拒否する。
    if expr_filters
        .iter()
        .any(crate::sql::udf_call::references_embedding)
        || or_filters.iter().any(|g| g.references_embedding())
    {
        return Err(SqlSurfaceError::feature_not_supported(
            "UPDATE ... FROM / DELETE ... USING WHERE cannot reference a VECTOR column",
        ));
    }
    Ok(JoinedResidual {
        source_columns: columns,
        metadata_filters,
        expr_filters,
        or_filters,
    })
}
