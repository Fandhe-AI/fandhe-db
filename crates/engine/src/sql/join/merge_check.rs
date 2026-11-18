//! `MERGE` の `ON`・`WHEN ... AND` 条件と SET／INSERT 値の式を、合成スキーマで型照合する部品
//! （SQL-79・Issue #3352。親 #3349、設計記録は `docs/design/sql-merge.md` の D4）。
//!
//! 責務境界: `sql::merge_bind`（束縛の本体）が列参照を公開名で修飾して解決し終えたあとに呼ぶ純関数だけを
//! 持つ。`sql::join::dml_residual`（`UPDATE ... FROM` の残余束縛）と同じ方式で、「対象表のスキーマ ++
//! 参照される元の列」の合成スキーマに対し既存の束縛器（`bind_where_predicates`・`bind_set_expr`）を通す。
//! 第 2 の評価器は作らず、NULL の三値論理・型強制・数値リテラルの解釈は通常の WHERE／SET と一致する。
//! 行・RLS・台帳・書き込みトランザクションには触れない（束縛は読み取りスキーマだけを見る。テナント境界を
//! 緩める経路を作らない）。
//!
//! 呼び出し元: `sql::merge_bind::bind_merge` のみ。実行（押し下げ・結合キーの確定）は #3353・#3354 が
//! この束縛結果（[`BoundCondition`]）を消費する。
//!
//! 結合キー候補（対象と元をまたぐ列同士の等値）は型クラス（`values::cmp_class`）が一致するか確認し、
//! 比較できない組は `42804`。キー化できない型は残余側へ回して通常の WHERE と同じ束縛に判定を委ねる。
//! 文言は英語の固定文字列と入力された名前だけで、件数・値・他テナントの存在を含めない。

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::allowlist::{JoinWhereExpr, JoinWherePredicate, SqlSurfaceError};
use crate::sql::dml_from::{restore_set_refs, target_slot_name};
use crate::sql::relation::{ColumnRef, ColumnSlot, ResolvedColumn};
use crate::sql::set_expr::BoundSetExpr;
use crate::sql::udf_call::{BinOp, Expr, UdfRegistry};

use super::dml::JoinedResidual;
use super::dml_residual::{plan_residual_with, ResidualMsgs};
use super::values::{cell_scalar_supports, cmp_class, key_class};
use crate::sql::udf_call::require_wired_collation;

/// `MERGE` 用の残余束縛の固定文言。
const MERGE_MSGS: ResidualMsgs = ResidualMsgs {
    source_id: "MERGE: the pseudo column id of the USING source cannot be used in this condition",
    source_type: "MERGE: the USING source column type is not supported in this condition",
    vector: "MERGE conditions cannot reference a VECTOR column",
};

/// 束縛済みの条件（`ON` または `WHEN ... AND`）。
#[derive(Debug, Clone)]
pub(crate) struct BoundCondition {
    /// 列参照を公開名で修飾し直した論理木（#3353 が conjunct の振り分けに使う）。
    expr: JoinWhereExpr,
    /// 対象と元をまたぐ列同士の等値（(対象側, 元側)。型クラス照合済み）。
    keys: Vec<(ResolvedColumn, ResolvedColumn)>,
    /// 等値キー以外の conjunct を合成スキーマで束縛した結果。
    residual: Option<JoinedResidual>,
}

impl BoundCondition {
    /// 修飾済みの論理木。
    #[allow(dead_code)] // 押し下げ（D3）を入れる #3354 が消費するまでは単体テストのみが呼び出し元
    pub(crate) fn expr(&self) -> &JoinWhereExpr {
        &self.expr
    }

    /// 結合キー候補（(対象側, 元側)）。
    pub(crate) fn keys(&self) -> &[(ResolvedColumn, ResolvedColumn)] {
        &self.keys
    }

    /// 残余の束縛結果。
    pub(crate) fn residual(&self) -> Option<&JoinedResidual> {
        self.residual.as_ref()
    }
}

/// 論理木を最上位の `AND` で平坦化した conjunct 列を返す。
pub(crate) fn conjuncts(expr: &JoinWhereExpr) -> Vec<&JoinWhereExpr> {
    let mut out = Vec::new();
    fn walk<'e>(e: &'e JoinWhereExpr, out: &mut Vec<&'e JoinWhereExpr>) {
        match e {
            JoinWhereExpr::And(ch) => {
                for c in ch {
                    walk(c, out);
                }
            }
            other => out.push(other),
        }
    }
    walk(expr, &mut out);
    out
}

/// 論理木の全ての列参照を `f` で書き換えた複製を返す（出現順に `f` を呼ぶ。最初の失敗で止まる）。
pub(crate) fn map_refs(
    expr: &JoinWhereExpr,
    f: &dyn Fn(&ColumnRef) -> Result<ColumnRef, SqlSurfaceError>,
) -> Result<JoinWhereExpr, SqlSurfaceError> {
    Ok(match expr {
        JoinWhereExpr::And(ch) => JoinWhereExpr::And(
            ch.iter()
                .map(|e| map_refs(e, f))
                .collect::<Result<_, _>>()?,
        ),
        JoinWhereExpr::Or(ch) => JoinWhereExpr::Or(
            ch.iter()
                .map(|e| map_refs(e, f))
                .collect::<Result<_, _>>()?,
        ),
        JoinWhereExpr::Leaf(p) => JoinWhereExpr::Leaf(match p {
            JoinWherePredicate::Equality { column, value } => JoinWherePredicate::Equality {
                column: f(column)?,
                value: value.clone(),
            },
            JoinWherePredicate::Prefix { column, pattern } => JoinWherePredicate::Prefix {
                column: f(column)?,
                pattern: pattern.clone(),
            },
            JoinWherePredicate::Compare { column, op, value } => JoinWherePredicate::Compare {
                column: f(column)?,
                op: *op,
                value: value.clone(),
            },
            JoinWherePredicate::BoolEquality { column, value } => {
                JoinWherePredicate::BoolEquality {
                    column: f(column)?,
                    value: *value,
                }
            }
            JoinWherePredicate::BoolColumn { column } => {
                JoinWherePredicate::BoolColumn { column: f(column)? }
            }
            JoinWherePredicate::InList { column, values } => JoinWherePredicate::InList {
                column: f(column)?,
                values: values.clone(),
            },
            JoinWherePredicate::ColumnCompare { lhs, op, rhs } => {
                JoinWherePredicate::ColumnCompare {
                    lhs: f(lhs)?,
                    op: *op,
                    rhs: f(rhs)?,
                }
            }
            JoinWherePredicate::NumericCompare { column, op, value } => {
                JoinWherePredicate::NumericCompare {
                    column: f(column)?,
                    op: *op,
                    value: value.clone(),
                }
            }
            JoinWherePredicate::IsNull { column, negated } => JoinWherePredicate::IsNull {
                column: f(column)?,
                negated: *negated,
            },
        }),
    })
}

/// 修飾済みの条件を型照合して [`BoundCondition`] にする。`resolve` は修飾済みの参照を全体スコープ
/// （relation 0 = 対象、1 = 元）で解決する関数。判定順は決定的: conjunct を宣言順に見て、対象と元を
/// またぐ列同士の等値は型クラス照合（`42804`）、それ以外は束縛をまとめて残余として通す。
pub(crate) fn bind_condition(
    qualified: JoinWhereExpr,
    resolve: &dyn Fn(&ColumnRef) -> Result<ResolvedColumn, SqlSurfaceError>,
    target_schema: &TableSchema,
    source_schema: &TableSchema,
    udfs: &UdfRegistry,
) -> Result<BoundCondition, SqlSurfaceError> {
    let mut keys: Vec<(ResolvedColumn, ResolvedColumn)> = Vec::new();
    let mut rest: Vec<&JoinWhereExpr> = Vec::new();
    for c in conjuncts(&qualified) {
        if let JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare { lhs, op, rhs }) = c {
            if *op == BinOp::Eq {
                let (l, r) = (resolve(lhs)?, resolve(rhs)?);
                if l.relation() != r.relation() {
                    match (cmp_class(l.column_type()), cmp_class(r.column_type())) {
                        (Some(a), Some(b)) if a == b => {
                            // ハッシュ結合のキーにできない型（REAL／DOUBLE／NUMERIC 等）と、バイト列順でない
                            // 照合の列（ICU）は、`UPDATE ... FROM` と同じく結合キーにせず残余の束縛へ回す
                            // （バイト等価・浮動小数のハッシュで黙って誤結合しない。fail-closed）。
                            let hashable = key_class(l.column_type()).is_ok()
                                && key_class(r.column_type()).is_ok()
                                && require_wired_collation(slot_collation(
                                    &l,
                                    target_schema,
                                    source_schema,
                                ))
                                .is_ok()
                                && require_wired_collation(slot_collation(
                                    &r,
                                    target_schema,
                                    source_schema,
                                ))
                                .is_ok();
                            if !hashable {
                                rest.push(c);
                                continue;
                            }
                            let (t, s) = if l.relation() == 0 { (l, r) } else { (r, l) };
                            keys.push((t, s));
                            continue;
                        }
                        (Some(_), Some(_)) => {
                            return Err(SqlSurfaceError::datatype_mismatch(
                                "MERGE condition compares columns of incompatible types",
                            ));
                        }
                        // 比較できない型（VECTOR 等）は残余の束縛に判定を委ねる（fail-closed）。
                        _ => {}
                    }
                }
            }
        }
        rest.push(c);
    }
    let residual = if rest.is_empty() {
        None
    } else {
        Some(plan_residual_with(
            &rest,
            resolve,
            target_schema,
            source_schema,
            udfs,
            &MERGE_MSGS,
        )?)
    };
    Ok(BoundCondition {
        expr: qualified,
        keys,
        residual,
    })
}

/// 解決済みの列の照合（疑似列 `id`・範囲外は `None`）。
fn slot_collation(
    r: &ResolvedColumn,
    target_schema: &TableSchema,
    source_schema: &TableSchema,
) -> Option<crate::collation::Collation> {
    let schema = if r.relation() == 0 {
        target_schema
    } else {
        source_schema
    };
    match r.slot() {
        ColumnSlot::Id => None,
        ColumnSlot::Column(i) => schema.columns.get(i).and_then(|c| c.collation),
    }
}

/// 元の列を式評価ビューへ写像できる型か（`dml_residual`・`dml_plan` と同じ範囲）。
fn source_type_supported(ty: &ColumnType) -> bool {
    cell_scalar_supports(ty)
        || matches!(
            ty,
            ColumnType::Time | ColumnType::TimestampTz | ColumnType::Interval | ColumnType::Jsonb
        )
}

/// 元の列を含む SET／INSERT 値の式を、「対象表 ++ 参照される元の列」の合成スキーマで束縛する
/// （`bind_set_expr`。型の照合・暗黙キャスト・式非対応の代入先型の `0A000` は単一表 SET と同じ）。
/// 元の疑似列 `id` を指す参照と写像できない型は固定文言の `0A000`。戻り値の 2 つ目は参照された元の列の
/// 添字（初出順・重複なし）。`refs[k]` はプレースホルダ `k` の解決結果（relation 0 = 対象、1 = 元）。
pub(crate) fn bind_joined_set_expr(
    target_col: &ColumnDef,
    expr: &Expr,
    refs: &[ResolvedColumn],
    target_schema: &TableSchema,
    source_schema: &TableSchema,
) -> Result<(BoundSetExpr, Vec<usize>), SqlSurfaceError> {
    let internal = |d: &str| SqlSurfaceError::Internal {
        detail: d.to_string(),
    };
    let mut combined = target_schema.clone();
    let mut columns: Vec<usize> = Vec::new();
    let mut names: Vec<String> = Vec::with_capacity(refs.len().min(1024));
    for resolved in refs {
        let name = match resolved.relation() {
            0 => target_slot_name(resolved, target_schema)?,
            1 => {
                let idx = match resolved.slot() {
                    ColumnSlot::Id => {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "MERGE: the pseudo column id of the USING source cannot be used in an expression",
                        ));
                    }
                    ColumnSlot::Column(i) => i,
                };
                let col = source_schema
                    .columns
                    .get(idx)
                    .ok_or_else(|| internal("source column out of range"))?;
                if !source_type_supported(&col.ty) {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "MERGE: the USING source column type is not supported in an expression",
                    ));
                }
                let pos = match columns.iter().position(|&c| c == idx) {
                    Some(p) => p,
                    None => {
                        columns.push(idx);
                        let pos = columns.len().saturating_sub(1);
                        // 照合・型を引き継ぐ（元の列の比較規則を落とさない）。
                        let mut def = col.clone();
                        def.name = format!("\u{0}msrc{pos}");
                        def.nullable = true;
                        combined.columns.push(def);
                        pos
                    }
                };
                format!("\u{0}msrc{pos}")
            }
            _ => return Err(internal("relation index out of range in MERGE expression")),
        };
        names.push(name);
    }
    let mapped = restore_set_refs(expr, &names);
    let bound =
        crate::sql::set_expr::bind_set_expr(&target_col.name, &mapped, target_col, &combined)?;
    Ok((bound, columns))
}
