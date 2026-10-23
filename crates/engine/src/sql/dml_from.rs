//! `UPDATE ... FROM`／`DELETE ... USING` の束縛（SQL-53 (b)、Issue #1754。親 #1753）。
//!
//! 責務境界: 構文は `sql::allowlist`（[`ValidatedUpdateFrom`]・[`ValidatedDeleteUsing`]）が保証し、
//! 本モジュールは対象表と FROM／USING 側の表を 1 つの [`BindingScope`] にまとめて、`WHERE` と
//! `SET` 右辺の列参照を解決・型検証する純関数だけを持つ。カタログ照会・行の読み取り・RLS・
//! 台帳には一切触れない（スキーマは呼び出し元が渡す）。結合の実行・影響行数上限は #1755、
//! 全参照への RLS 適用と `RETURNING` は #1756、台帳の content_hash は #1757（構文段で算出済み）で、
//! 本 Issue の範囲では束縛を通った文も実行腕が `0A000` で止める（副作用ゼロ）。
//!
//! 呼び出し元: `core/dml_from_exec.rs`（autocommit・明示トランザクション・Describe の各腕）。
//!
//! 列解決の契約（`BindingScope::resolve` と同じ）:
//! - 修飾子が公開名（別名があれば別名）に一致しない → `42P01`（カタログを引かないので存在オラクルに
//!   ならない）
//! - 非修飾で 2 表以上に該当 → `42702`
//! - 未知の列 → `42703`（FROM 句の表・SET 対象とも。SET 対象列の重複は `22000`）
//!
//! SET 左辺は対象表の列だけを指す（疑似列 `id`・`tenant_id`・`visibility` への代入は `42601`）。

use std::collections::HashSet;

use crate::catalog::TableSchema;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::{
    DmlFromSetValue, JoinWhereExpr, JoinWherePredicate, SetValue, SqlSurfaceError,
    ValidatedDeleteUsing, ValidatedUpdateFrom,
};
use crate::sql::relation::{BindingScope, ColumnRef, ResolvedColumn, TableRef};
use crate::sql::set_expr::BoundSetExpr;

/// 束縛済みの `UPDATE ... FROM`。`relations[0]` が更新対象で、`where_columns`・`set_columns` の
/// [`ResolvedColumn::relation`] はこの並びへの添字（#1755 が JOIN 計画へ渡す材料）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BoundUpdateFrom {
    relations: Vec<TableRef>,
    where_columns: Vec<ResolvedColumn>,
    set_columns: Vec<(usize, ResolvedColumn)>,
    set_literals: Vec<(usize, Value)>,
    set_exprs: Vec<(usize, BoundSetExpr)>,
    operation_id: Option<OperationId>,
}

impl BoundUpdateFrom {
    /// 参照の並び（先頭が更新対象、続いて FROM 側の宣言順）。
    pub fn relations(&self) -> &[TableRef] {
        &self.relations
    }

    /// `WHERE` の各葉が参照する列の解決結果（葉の出現順）。
    pub fn where_columns(&self) -> &[ResolvedColumn] {
        &self.where_columns
    }

    /// 右辺が列参照の SET 項目（(対象表の列添字, 右辺の解決結果)）。
    pub fn set_columns(&self) -> &[(usize, ResolvedColumn)] {
        &self.set_columns
    }

    /// 右辺がリテラルの SET 項目（(対象表の列添字, 束縛済み値)）。
    pub fn set_literals(&self) -> &[(usize, Value)] {
        &self.set_literals
    }

    /// 右辺が対象表の列だけを使う式・`DEFAULT` の SET 項目の数（束縛済みの式そのものは
    /// クレート内部型のため公開しない。実行は #1755 がクレート内で受け取る）。
    pub fn set_expr_count(&self) -> usize {
        self.set_exprs.len()
    }

    /// 検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }
}

/// 束縛済みの `DELETE ... USING`。`relations[0]` が削除対象。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BoundDeleteUsing {
    relations: Vec<TableRef>,
    where_columns: Vec<ResolvedColumn>,
    operation_id: Option<OperationId>,
}

impl BoundDeleteUsing {
    /// 参照の並び（先頭が削除対象、続いて USING 側の宣言順）。
    pub fn relations(&self) -> &[TableRef] {
        &self.relations
    }

    /// `WHERE` の各葉が参照する列の解決結果（葉の出現順）。
    pub fn where_columns(&self) -> &[ResolvedColumn] {
        &self.where_columns
    }

    /// 検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }
}

/// 対象表と FROM／USING 側の表から束縛スコープを作る。参照数の超過は `54000`、公開名の重複は `42601`。
fn build_scope<'a>(
    target: &TableRef,
    target_schema: &'a TableSchema,
    others: &[TableRef],
    other_schemas: &[&'a TableSchema],
) -> Result<BindingScope<'a>, SqlSurfaceError> {
    if others.len() != other_schemas.len() {
        return Err(SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        });
    }
    let mut relations: Vec<(TableRef, &TableSchema)> = Vec::with_capacity(
        others
            .len()
            .saturating_add(1)
            .min(crate::sql::relation::MAX_TABLE_REFS),
    );
    relations.push((target.clone(), target_schema));
    for (r, s) in others.iter().zip(other_schemas.iter()) {
        relations.push((r.clone(), *s));
    }
    BindingScope::new(relations)
}

/// `WHERE` の論理木が参照する列を出現順に集め、スコープで解決する。
fn resolve_where_columns(
    scope: &BindingScope<'_>,
    where_clause: Option<&JoinWhereExpr>,
) -> Result<Vec<ResolvedColumn>, SqlSurfaceError> {
    fn walk<'e>(expr: &'e JoinWhereExpr, out: &mut Vec<&'e ColumnRef>) {
        match expr {
            JoinWhereExpr::And(children) | JoinWhereExpr::Or(children) => {
                for c in children {
                    walk(c, out);
                }
            }
            JoinWhereExpr::Leaf(pred) => match pred {
                JoinWherePredicate::Equality { column, .. }
                | JoinWherePredicate::Prefix { column, .. }
                | JoinWherePredicate::Compare { column, .. }
                | JoinWherePredicate::BoolEquality { column, .. }
                | JoinWherePredicate::BoolColumn { column }
                | JoinWherePredicate::InList { column, .. } => out.push(column),
                JoinWherePredicate::ColumnCompare { lhs, rhs, .. } => {
                    out.push(lhs);
                    out.push(rhs);
                }
            },
        }
    }
    let mut refs: Vec<&ColumnRef> = Vec::new();
    if let Some(expr) = where_clause {
        walk(expr, &mut refs);
    }
    refs.into_iter().map(|c| scope.resolve(c)).collect()
}

/// SET 左辺の列を対象表のスキーマで検査し、列添字を返す（`bind_set_assignments` と同じ規則。
/// 右辺が列参照の項目は `bind_set_assignments` を通らないため、ここで同じ検査を行う）。
fn check_set_target(name: &str, schema: &TableSchema) -> Result<usize, SqlSurfaceError> {
    if (name == "id" && schema.has_pseudo_id()) || name == "tenant_id" || name == "visibility" {
        return Err(SqlSurfaceError::unsupported(format!(
            "column {name:?} cannot be targeted by SET"
        )));
    }
    let idx = schema
        .columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
    let column = schema
        .columns
        .get(idx)
        .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
    crate::sql::parser::reject_generated_always_value(column)?;
    Ok(idx)
}

/// SET 右辺の式に現れる非修飾の列名をスコープ全体で照合する。対象表と FROM 側の両方に該当すれば
/// `42702`、FROM 側だけなら `0A000`（FROM 表の列を使う式は #1755 以降）。どの表にも無い名前は
/// 既存の式束縛（対象表のスキーマ）が未知列として扱う。
fn check_set_expr_scope(
    scope: &BindingScope<'_>,
    unqualified: &[String],
) -> Result<(), SqlSurfaceError> {
    // 対象表修飾つきの列は構文段で非修飾へ正規化済みのため、実際に非修飾で書かれた名前
    // （`unqualified`）だけを検査する。重複を除き、同じ入力には常に同じ順序・同じエラーを返す。
    let mut names: Vec<String> = unqualified.to_vec();
    names.sort();
    names.dedup();
    for name in names {
        match scope.resolve(&ColumnRef::unqualified(name)) {
            Ok(r) if r.relation() == 0 => {}
            Ok(_) => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "UPDATE ... FROM: expressions referencing FROM-clause tables are not supported yet",
                ));
            }
            Err(e) if e.wire_code() == "42702" => return Err(e),
            Err(_) => {}
        }
    }
    Ok(())
}

/// `UPDATE ... FROM` を束縛する。判定順は決定的: スコープ構築（`54000`・`42601`）→ `WHERE` の列解決 →
/// SET 左辺の検査（重複・疑似列・未知列・`GENERATED ALWAYS`）→ SET 右辺（列参照は解決と型照合〔`42804`〕、
/// 式はスコープ照合の後に既存の式束縛、リテラルは既存のリテラル束縛）。
pub fn bind_update_from(
    stmt: &ValidatedUpdateFrom,
    target_schema: &TableSchema,
    from_schemas: &[&TableSchema],
) -> Result<BoundUpdateFrom, SqlSurfaceError> {
    let scope = build_scope(&stmt.target, target_schema, &stmt.from, from_schemas)?;
    let where_columns = resolve_where_columns(&scope, stmt.where_clause.as_deref())?;

    let mut seen: HashSet<&str> = HashSet::new();
    let mut set_columns: Vec<(usize, ResolvedColumn)> = Vec::new();
    let mut plain: Vec<(String, SetValue)> = Vec::new();
    for (name, value) in &stmt.assignments {
        if !seen.insert(name.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in SET clause: {name}"
            )));
        }
        match value {
            DmlFromSetValue::Column(source) => {
                let idx = check_set_target(name, target_schema)?;
                let resolved = scope.resolve(source)?;
                let target_ty = target_schema
                    .columns
                    .get(idx)
                    .map(|c| &c.ty)
                    .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
                let source_ty = resolved
                    .column_type()
                    .unwrap_or(&crate::sql::exec::ALIASED_ID_TYPE);
                if !crate::sql::insert_select::assignable(source_ty, target_ty) {
                    return Err(SqlSurfaceError::datatype_mismatch(format!(
                        "column {name:?} cannot be assigned a value of a different type"
                    )));
                }
                set_columns.push((idx, resolved));
            }
            DmlFromSetValue::Value(v, unqualified) => {
                if let SetValue::Expr(_) = v {
                    check_set_expr_scope(&scope, unqualified)?;
                }
                plain.push((name.clone(), v.clone()));
            }
        }
    }
    let (set_literals, set_exprs) =
        crate::sql::parser::bind_set_assignments(&plain, target_schema)?;

    Ok(BoundUpdateFrom {
        relations: std::iter::once(stmt.target.clone())
            .chain(stmt.from.iter().cloned())
            .collect(),
        where_columns,
        set_columns,
        set_literals,
        set_exprs,
        operation_id: stmt.operation_id.clone(),
    })
}

/// `DELETE ... USING` を束縛する（スコープ構築 → `WHERE` の列解決）。
pub fn bind_delete_using(
    stmt: &ValidatedDeleteUsing,
    target_schema: &TableSchema,
    using_schemas: &[&TableSchema],
) -> Result<BoundDeleteUsing, SqlSurfaceError> {
    let scope = build_scope(&stmt.target, target_schema, &stmt.using, using_schemas)?;
    let where_columns = resolve_where_columns(&scope, stmt.where_clause.as_deref())?;
    Ok(BoundDeleteUsing {
        relations: std::iter::once(stmt.target.clone())
            .chain(stmt.using.iter().cloned())
            .collect(),
        where_columns,
        operation_id: stmt.operation_id.clone(),
    })
}
