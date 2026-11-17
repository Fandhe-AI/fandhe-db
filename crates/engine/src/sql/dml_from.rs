//! `UPDATE ... FROM`／`DELETE ... USING` の束縛（SQL-53 (b)、Issue #1754。親 #1753）。
//!
//! 責務境界: 構文は `sql::allowlist`（[`ValidatedUpdateFrom`]・[`ValidatedDeleteUsing`]）が保証し、
//! 本モジュールは対象表と FROM／USING 側の表を 1 つの [`BindingScope`] にまとめて、`WHERE` と
//! `SET` 右辺の列参照を解決・型検証する純関数だけを持つ。カタログ照会・行の読み取り・RLS・
//! 台帳には一切触れない（スキーマは呼び出し元が渡す）。結合の実行・影響行数上限は #1755、
//! 全参照への RLS 適用は #1756、台帳の content_hash は #1757（構文段で算出済み）が担う。
//! `RETURNING` の列解決は `sql::dml_from_returning` へ委譲して同じスコープで束縛する（Issue #3204）。
//!
//! 呼び出し元: `core/dml_from_exec.rs`（autocommit・明示トランザクション・Describe の各腕）と、
//! 束縛結果を `JoinedDmlPlan` へ変換する `sql::join::dml_plan`（Issue #3201）。
//!
//! 列解決の契約（`BindingScope::resolve` と同じ）:
//! - 修飾子が公開名（別名があれば別名）に一致しない → `42P01`（カタログを引かないので存在オラクルに
//!   ならない）
//! - 非修飾で 2 表以上に該当 → `42702`
//! - 未知の列 → `42703`（FROM 句の表・SET 対象とも。SET 対象列の重複は `22000`）
//!
//! SET 右辺の式（Issue #3296）: 構文段が列参照を予約接頭辞つきのプレースホルダ識別子
//! （[`SET_REF_PREFIX`]）へ置き換えて `(式, 元の参照)` で渡す。本モジュールが各参照を同じスコープで
//! 解決し、対象表の列だけなら元の列名へ戻して従来の式束縛へ、FROM 側を含むなら
//! [`BoundUpdateFrom::set_joined_exprs`] として変換器（`sql::join::dml_plan`）へ渡す。
//!
//! SET 左辺は対象表の列だけを指す（疑似列 `id`・`tenant_id`・`visibility` への代入は `42601`）。

use std::collections::HashSet;

use crate::catalog::TableSchema;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::{
    DmlFromSetValue, DmlFromSource, JoinCondition, JoinStep, JoinWhereExpr, JoinWherePredicate,
    SetValue, SqlSurfaceError, ValidatedDeleteUsing, ValidatedUpdateFrom,
};
use crate::sql::dml_from_returning::{bind_from_returning, BoundFromReturning};
use crate::sql::relation::{BindingScope, ColumnRef, ResolvedColumn, TableRef};
use crate::sql::set_expr::BoundSetExpr;
use crate::sql::udf_call::Expr;

/// `UPDATE ... FROM` の SET 右辺の式で、列参照の位置に置くプレースホルダ識別子の接頭辞
/// （Issue #3296。続けて参照の通し番号）。NUL を含むため利用者の識別子とは衝突せず、
/// 構文段は NUL 始まりの識別子を拒否する。
pub(crate) const SET_REF_PREFIX: &str = "\u{0}setref";

/// 束縛済みの `UPDATE ... FROM`。`relations[0]` が更新対象で、`where_columns`・`set_columns` の
/// [`ResolvedColumn::relation`] はこの並びへの添字（#1755 が JOIN 計画へ渡す材料）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BoundUpdateFrom {
    relations: Vec<TableRef>,
    /// FROM 側の結合段（Issue #3294。長さは `relations.len() - 2`、空なら FROM 側が 1 表）。
    steps: Vec<JoinStep>,
    where_columns: Vec<ResolvedColumn>,
    set_columns: Vec<(usize, ResolvedColumn)>,
    set_literals: Vec<(usize, Value)>,
    set_exprs: Vec<(usize, BoundSetExpr)>,
    set_joined_exprs: Vec<(usize, Expr, Vec<ResolvedColumn>)>,
    where_clause: Option<JoinWhereExpr>,
    returning: Option<BoundFromReturning>,
    operation_id: Option<OperationId>,
    source_visible: Vec<Option<Vec<String>>>,
}

impl BoundUpdateFrom {
    /// 参照の並び（先頭が更新対象、続いて FROM 側の宣言順）。
    pub fn relations(&self) -> &[TableRef] {
        &self.relations
    }

    /// FROM 側の結合段（Issue #3294）。`relations[k + 2]` を加える段が `steps[k]`。
    pub(crate) fn steps(&self) -> &[JoinStep] {
        &self.steps
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
    /// クレート内部型のため公開しない。実行は #1755 がクレート内で受け取る）。FROM 側の列を含む式
    /// （Issue #3296）はここに数えず、[`Self::set_joined_exprs`] が運ぶ。
    pub fn set_expr_count(&self) -> usize {
        self.set_exprs.len()
    }

    /// 束縛に使った `WHERE` の論理木（変換器 `sql::join::dml_plan` が conjunct を振り分ける）。
    pub(crate) fn where_clause(&self) -> Option<&JoinWhereExpr> {
        self.where_clause.as_ref()
    }

    /// 対象表の列だけを使う式・`DEFAULT` の SET 項目（(対象表の列添字, 束縛済み式)）。
    pub(crate) fn set_exprs(&self) -> &[(usize, BoundSetExpr)] {
        &self.set_exprs
    }

    /// FROM 側の列を含む SET 式（Issue #3296。(対象表の列添字, プレースホルダ入りの式, プレースホルダ
    /// `k` に対応する解決結果)）。変換器が合成スキーマ上で束縛する。
    pub(crate) fn set_joined_exprs(&self) -> &[(usize, Expr, Vec<ResolvedColumn>)] {
        &self.set_joined_exprs
    }

    /// 束縛済みの `RETURNING`（Issue #3204）。変換器が source 側の参照列を計画へ載せる。
    pub(crate) fn returning(&self) -> Option<&BoundFromReturning> {
        self.returning.as_ref()
    }

    /// FROM／USING 側の relation ごとの見える列（Issue #3292。ビュー source の公開列。`None` は制限なし）。
    /// 変換器が同じ束縛スコープを再構築するために使う。
    pub(crate) fn source_visible(&self) -> &[Option<Vec<String>>] {
        &self.source_visible
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
    /// USING 側の結合段（Issue #3294。[`BoundUpdateFrom`] の `steps` と同じ契約）。
    steps: Vec<JoinStep>,
    where_columns: Vec<ResolvedColumn>,
    where_clause: Option<JoinWhereExpr>,
    returning: Option<BoundFromReturning>,
    operation_id: Option<OperationId>,
    source_visible: Vec<Option<Vec<String>>>,
}

impl BoundDeleteUsing {
    /// 束縛済みの `RETURNING`（Issue #3204）。変換器が source 側の参照列を計画へ載せる。
    pub(crate) fn returning(&self) -> Option<&BoundFromReturning> {
        self.returning.as_ref()
    }

    /// 束縛に使った `WHERE` の論理木（変換器 `sql::join::dml_plan` が conjunct を振り分ける）。
    pub(crate) fn where_clause(&self) -> Option<&JoinWhereExpr> {
        self.where_clause.as_ref()
    }

    /// USING 側の結合段（Issue #3294）。
    pub(crate) fn steps(&self) -> &[JoinStep] {
        &self.steps
    }

    /// 参照の並び（先頭が削除対象、続いて USING 側の宣言順）。
    pub fn relations(&self) -> &[TableRef] {
        &self.relations
    }

    /// `WHERE` の各葉が参照する列の解決結果（葉の出現順）。
    pub fn where_columns(&self) -> &[ResolvedColumn] {
        &self.where_columns
    }

    /// FROM／USING 側の relation ごとの見える列（Issue #3292。ビュー source の公開列。`None` は制限なし）。
    /// 変換器が同じ束縛スコープを再構築するために使う。
    pub(crate) fn source_visible(&self) -> &[Option<Vec<String>>] {
        &self.source_visible
    }

    /// 検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }
}

/// 対象表と FROM／USING 側の表から束縛スコープを作る。参照数の超過は `54000`、公開名の重複は `42712`（Issue #3297）。
pub(crate) fn build_scope<'a>(
    target: &TableRef,
    target_schema: &'a TableSchema,
    others: &[TableRef],
    other_schemas: &[&'a TableSchema],
    other_visible: &[Option<Vec<String>>],
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
    // Issue #3297: 結合 DML の公開名重複は PG の `duplicate_alias`（`42712`）。参照数の超過（`54000`）を
    // 先に判定する順序は `BindingScope::new` と同じ。スキーマ修飾の解決後に初めて同名になる形も
    // ここで拾う。SELECT の JOIN が使う `BindingScope::new` 自体は従来どおり `42601`（ADR の判断を維持）。
    if relations.len() <= crate::sql::relation::MAX_TABLE_REFS {
        for (i, (r, _)) in relations.iter().enumerate() {
            let exposed = r.exposed_name();
            if relations
                .iter()
                .take(i)
                .any(|(other, _)| other.exposed_name() == exposed)
            {
                return Err(SqlSurfaceError::duplicate_alias(exposed));
            }
        }
    }
    // 先頭（対象表）は制限なし。FROM／USING 側はビューの公開列だけを見せる（Issue #3292）。
    let visible = std::iter::once(None)
        .chain(other_visible.iter().cloned())
        .collect();
    Ok(BindingScope::new(relations)?.with_visible(visible))
}

/// FROM／USING 側の解決結果から、relation ごとの見える列を作る（ビューは公開列、それ以外は制限なし）。
pub(crate) fn visible_of(sources: &[DmlFromSource]) -> Vec<Option<Vec<String>>> {
    sources
        .iter()
        .map(|s| match s {
            DmlFromSource::View(v) => v.view_columns.clone(),
            DmlFromSource::Table => None,
        })
        .collect()
}

/// FROM／USING 内の `JOIN ... ON` の列参照を、FROM 側の表だけのスコープで解決する（Issue #3294）。
/// PG と同じく `ON` からは更新・削除の対象表を参照できない（修飾参照は `42P01`、対象表にしか無い
/// 非修飾列は `42703`）。段 k の `ON` が参照できるのは FROM 側の先頭から k + 1 番目までの表で、
/// 後続の表への参照も `42P01` になる。非修飾で複数の FROM 側の表に該当すれば `42702`。
/// 列の型検証と結合キー／残余の分類は結合実行器（`sql::join`）が担う。
fn bind_join_steps(
    steps: &[JoinStep],
    others: &[TableRef],
    other_schemas: &[&TableSchema],
    other_visible: &[Option<Vec<String>>],
) -> Result<(), SqlSurfaceError> {
    for (k, step) in steps.iter().enumerate() {
        let JoinCondition::On(expr) = &step.condition else {
            continue;
        };
        let upto = k.saturating_add(2);
        let relations: Vec<(TableRef, &TableSchema)> = others
            .iter()
            .zip(other_schemas.iter())
            .take(upto)
            .map(|(r, s)| (r.clone(), *s))
            .collect();
        let visible = other_visible.iter().take(upto).cloned().collect();
        let scope = BindingScope::new(relations)?.with_visible(visible);
        resolve_where_columns(&scope, Some(expr))?;
    }
    Ok(())
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
                | JoinWherePredicate::InList { column, .. }
                | JoinWherePredicate::NumericCompare { column, .. }
                | JoinWherePredicate::IsNull { column, .. } => out.push(column),
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

/// 式に現れる非修飾の列名をスコープ全体で照合する。対象表と FROM 側の両方に該当すれば
/// `42702`、FROM 側だけなら `0A000`。`RETURNING` の式（`sql::dml_from_returning`）専用の境界で、SET 右辺は
/// Issue #3296 以降 [`bind_update_from_with_udfs`] が参照を直接解決する。どの表にも無い名前は
/// 既存の式束縛（対象表のスキーマ）が未知列として扱う。
pub(crate) fn check_set_expr_scope(
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

/// `UPDATE ... FROM` を束縛する。判定順は決定的: スコープ構築（`54000`・`42712`）→ `JOIN ... ON` の列解決（Issue #3294）→ `WHERE` の列解決 →
/// SET 左辺の検査（重複・疑似列・未知列・`GENERATED ALWAYS`）→ SET 右辺（列参照は解決と型照合〔`42804`〕、
/// 式はスコープ照合の後に既存の式束縛、リテラルは既存のリテラル束縛）。
pub fn bind_update_from(
    stmt: &ValidatedUpdateFrom,
    target_schema: &TableSchema,
    from_schemas: &[&TableSchema],
) -> Result<BoundUpdateFrom, SqlSurfaceError> {
    bind_update_from_with_udfs(
        stmt,
        target_schema,
        from_schemas,
        &crate::sql::udf_call::UdfRegistry::default(),
    )
}

/// [`bind_update_from`] のセッション UDF レジストリ付き版（Issue #3251）。`RETURNING` の式内の UDF を
/// 解決する。レジストリを渡さない公開版は空レジストリ（UDF は `42883`＝fail-closed）。
pub(crate) fn bind_update_from_with_udfs(
    stmt: &ValidatedUpdateFrom,
    target_schema: &TableSchema,
    from_schemas: &[&TableSchema],
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundUpdateFrom, SqlSurfaceError> {
    bind_update_from_describe(stmt, target_schema, from_schemas, udfs, &[])
}

/// [`bind_update_from_with_udfs`] の Describe 用版（Issue #3297）。`set_dummy` は SET 句の代入ごと
/// （宣言順）の「`$n` 由来のダミー値か」で、真の文字列リテラル項は左辺（重複・疑似列・未知列・
/// `GENERATED ALWAYS`）だけを検査し、値の検証（DATE・UUID・ENUM 等）は Bind まで遅延する。
/// 代入数と一致しないフラグ列は使わない（全て検証側へ倒す。fail-closed）。実リテラルは常に検証する。
pub(crate) fn bind_update_from_describe(
    stmt: &ValidatedUpdateFrom,
    target_schema: &TableSchema,
    from_schemas: &[&TableSchema],
    udfs: &crate::sql::udf_call::UdfRegistry,
    set_dummy: &[bool],
) -> Result<BoundUpdateFrom, SqlSurfaceError> {
    let usable_set_flags = set_dummy.len() == stmt.assignments.len();
    let scope = build_scope(
        &stmt.target,
        target_schema,
        &stmt.from,
        from_schemas,
        &visible_of(&stmt.sources),
    )?;
    bind_join_steps(
        &stmt.from_steps,
        &stmt.from,
        from_schemas,
        &visible_of(&stmt.sources),
    )?;
    let where_columns = resolve_where_columns(&scope, stmt.where_clause.as_deref())?;

    let mut seen: HashSet<&str> = HashSet::new();
    let mut set_columns: Vec<(usize, ResolvedColumn)> = Vec::new();
    let mut plain: Vec<(String, SetValue)> = Vec::new();
    let mut set_joined_exprs: Vec<(usize, Expr, Vec<ResolvedColumn>)> = Vec::new();
    for (ai, (name, value)) in stmt.assignments.iter().enumerate() {
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
            DmlFromSetValue::Expr { expr, refs } => {
                // 左辺の検査（重複・疑似列・未知列・GENERATED ALWAYS）を先に行い、続いて参照を解決する
                // （`42P01`・`42702`・`42703`）。型の照合は式の束縛（対象表のみなら下の
                // `bind_set_assignments`、FROM 側を含むなら変換器）が `42804`／`0A000` で行う。
                let idx = check_set_target(name, target_schema)?;
                let resolved = refs
                    .iter()
                    .map(|r| scope.resolve(r))
                    .collect::<Result<Vec<_>, _>>()?;
                if resolved.iter().all(|r| r.relation() == 0) {
                    // 対象表の列だけの式は、プレースホルダを列名へ戻して従来の経路へ載せる。
                    let names = resolved
                        .iter()
                        .map(|r| target_slot_name(r, target_schema))
                        .collect::<Result<Vec<_>, _>>()?;
                    plain.push((name.clone(), SetValue::Expr(restore_set_refs(expr, &names))));
                } else {
                    set_joined_exprs.push((idx, expr.clone(), resolved));
                }
            }
            DmlFromSetValue::Value(v, unqualified) => {
                if let SetValue::Expr(_) = v {
                    check_set_expr_scope(&scope, unqualified)?;
                }
                let is_dummy = usable_set_flags
                    && set_dummy.get(ai).copied().unwrap_or(false)
                    && matches!(
                        v,
                        SetValue::Literal(crate::sql::allowlist::InsertLiteral::String(_))
                    );
                if is_dummy {
                    check_set_target(name, target_schema)?;
                    continue;
                }
                plain.push((name.clone(), v.clone()));
            }
        }
    }
    let (set_literals, set_exprs) =
        crate::sql::parser::bind_set_assignments(&plain, target_schema)?;
    // RETURNING は SET の束縛の後に同じスコープで束縛する（束縛エラーは変換器の 0A000 より先に返る）。
    let returning = stmt
        .returning
        .as_ref()
        .map(|r| bind_from_returning(r, &scope, udfs))
        .transpose()?;

    Ok(BoundUpdateFrom {
        relations: std::iter::once(stmt.target.clone())
            .chain(stmt.from.iter().cloned())
            .collect(),
        steps: stmt.from_steps.clone(),
        where_columns,
        set_columns,
        set_literals,
        set_exprs,
        set_joined_exprs,
        where_clause: stmt.where_clause.as_deref().cloned(),
        returning,
        operation_id: stmt.operation_id.clone(),
        source_visible: visible_of(&stmt.sources),
    })
}

/// 対象表（relation 0）の解決結果の列名。疑似列 `id` は `"id"`。範囲外は `Internal`（fail-closed）。
fn target_slot_name(
    resolved: &ResolvedColumn,
    schema: &TableSchema,
) -> Result<String, SqlSurfaceError> {
    match resolved.slot() {
        crate::sql::relation::ColumnSlot::Id => Ok("id".to_string()),
        crate::sql::relation::ColumnSlot::Column(i) => schema
            .columns
            .get(i)
            .map(|c| c.name.clone())
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            }),
    }
}

/// 式中のプレースホルダ `k`（[`SET_REF_PREFIX`] + 通し番号）を `names[k]` へ置き換えた複製を返す。
/// 変換器（`sql::join::dml_plan`）も同じ置き換えで合成スキーマ上の名前へ写す。
pub(crate) fn restore_set_refs(expr: &Expr, names: &[String]) -> Expr {
    crate::sql::check_constraint::map_expr_idents(expr, &|ident| {
        ident
            .strip_prefix(SET_REF_PREFIX)
            .and_then(|k| k.parse::<usize>().ok())
            .and_then(|k| names.get(k))
            .cloned()
            .unwrap_or_else(|| ident.to_string())
    })
}

/// `DELETE ... USING` を束縛する（スコープ構築 → `WHERE` の列解決）。
pub fn bind_delete_using(
    stmt: &ValidatedDeleteUsing,
    target_schema: &TableSchema,
    using_schemas: &[&TableSchema],
) -> Result<BoundDeleteUsing, SqlSurfaceError> {
    bind_delete_using_with_udfs(
        stmt,
        target_schema,
        using_schemas,
        &crate::sql::udf_call::UdfRegistry::default(),
    )
}

/// [`bind_delete_using`] のセッション UDF レジストリ付き版（Issue #3251）。
pub(crate) fn bind_delete_using_with_udfs(
    stmt: &ValidatedDeleteUsing,
    target_schema: &TableSchema,
    using_schemas: &[&TableSchema],
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundDeleteUsing, SqlSurfaceError> {
    let scope = build_scope(
        &stmt.target,
        target_schema,
        &stmt.using,
        using_schemas,
        &visible_of(&stmt.sources),
    )?;
    bind_join_steps(
        &stmt.using_steps,
        &stmt.using,
        using_schemas,
        &visible_of(&stmt.sources),
    )?;
    let where_columns = resolve_where_columns(&scope, stmt.where_clause.as_deref())?;
    let returning = stmt
        .returning
        .as_ref()
        .map(|r| bind_from_returning(r, &scope, udfs))
        .transpose()?;
    Ok(BoundDeleteUsing {
        relations: std::iter::once(stmt.target.clone())
            .chain(stmt.using.iter().cloned())
            .collect(),
        steps: stmt.using_steps.clone(),
        where_columns,
        where_clause: stmt.where_clause.as_deref().cloned(),
        returning,
        operation_id: stmt.operation_id.clone(),
        source_visible: visible_of(&stmt.sources),
    })
}
