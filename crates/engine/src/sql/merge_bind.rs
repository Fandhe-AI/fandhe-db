//! `MERGE` の束縛（対象と元の列解決・型照合。SQL-79・Issue #3352。親 #3349、設計記録は
//! `docs/design/sql-merge.md` の D2・D4）。
//!
//! 責務境界: 構文は `sql::allowlist::merge`（[`ValidatedMerge`]）が保証し、本モジュールは対象・元の
//! スキーマを受け取って `ON`・`WHEN ... AND` の条件・`UPDATE SET` の右辺・`INSERT` の値の列参照を
//! 解決し、型を照合する純関数だけを持つ。カタログ照会・行の読み取り・RLS・台帳には一切触れない
//! （スキーマは呼び出し元 `core::merge_exec` が同一スナップショットから渡す）。束縛は書き込みより前に
//! 完了するため、束縛エラーは行・台帳・世代へ何も残さない。
//!
//! relation 添字の規約（#3353・#3354 が消費する）: **0 = 対象、1 = 元**。
//!
//! `WHEN` 句ごとの名前空間（D4）:
//! - `ON`・`MATCHED`（条件・SET 右辺）: 対象と元の両方
//! - `NOT MATCHED [BY TARGET]`（条件・INSERT の値）: 元だけ
//! - `NOT MATCHED BY SOURCE`（条件・SET 右辺）: 対象だけ
//!
//! 見えない側への修飾参照は `42P01`、見えない側にしか無い非修飾の列は `42703`（存在オラクルにならない）。
//! 解決した参照は公開名で修飾し直してから両側スコープの型照合へ渡すため、狭いスコープで一意だった
//! 非修飾名が両側スコープで `42702` に誤判定されない。
//!
//! 判定順は決定的（同じ入力には常に同じコードと文言）: 対象がビューの形（`0A000`）→ スコープ構築
//! （`54000`・`42712`）→ `ON`（列解決の後に型）→ `WHEN` 句を宣言順に（条件の後に操作）。
//! SET の左辺は常に対象の列（疑似列・未知列・`GENERATED ALWAYS`・重複は単独の UPDATE と同じコード）。
//! `INSERT` は単独の INSERT と同じ規則（列リストの重複は `22000`・未知列は `42703`・従来方式の表で
//! 疑似列 `id` を省くと `22000`・個数不一致は `42601`・型不一致は `42804`）。NOT NULL・CHECK・UNIQUE・
//! 外部キーは実行時の検査（#3354 の INSERT は既存の挿入経路が行う）で本モジュールの範囲外。`RETURNING`（`merge_action()` を含む）は #3356 が両側スコープで束縛する。
//! 文言は英語の固定文字列と入力された名前だけで、件数・値・他テナントの存在を含めない。

use crate::catalog::TableSchema;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::{
    DmlFromSetValue, InsertLiteral, JoinWhereExpr, MergeAction, MergeInsertValues, MergeMatchKind,
    MergeSource, SetValue, SqlSurfaceError, ValidatedMerge,
};
use crate::sql::dml_from::{build_scope_with_target_visible, check_set_target};
use crate::sql::join::merge_check::{
    bind_condition, bind_joined_set_expr, map_refs, BoundCondition,
};
use crate::sql::relation::{BindingScope, ColumnRef, ColumnSlot, ResolvedColumn, TableRef};
use crate::sql::set_expr::BoundSetExpr;
use crate::sql::udf_call::UdfRegistry;
use crate::sql::view_dml::MergeTargetView;

/// 対象の書き込みに使う列の代入先（`INSERT` の列リストの 1 要素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeInsertSlot {
    /// 疑似列 `id`。
    Id,
    /// 通常の列（対象スキーマの `columns` の添字）。
    Column(usize),
}

/// `INSERT` の疑似列 `id` へ与える値。
#[derive(Debug, Clone)]
pub enum MergeIdValue {
    /// 元の列（整数型または疑似列 `id`）。
    Source(ResolvedColumn),
    /// 非負整数のリテラル。
    Literal(u64),
}

/// 束縛済みの代入群（`UPDATE SET` と `INSERT` の値が共有する形）。添字は対象スキーマの `columns`。
#[derive(Debug, Clone, Default)]
pub struct BoundAssignments {
    columns: Vec<(usize, ResolvedColumn)>,
    literals: Vec<(usize, Value)>,
    exprs: Vec<(usize, BoundSetExpr)>,
    joined_exprs: Vec<(usize, BoundSetExpr, Vec<usize>)>,
    /// 構文上 SET に挙げられた全列の添字（`SET g = DEFAULT` のように代入計画から除かれる生成列も含む）。
    /// 文トリガの `UPDATE OF` 判定（Issue #3355）が使う。
    declared_columns: Vec<usize>,
}

impl BoundAssignments {
    /// 構文上 SET に挙げられた全列の添字（代入計画から除かれた生成列への `DEFAULT` を含む）。
    pub(crate) fn declared_columns(&self) -> &[usize] {
        &self.declared_columns
    }

    /// 右辺が列参照の項目（(対象の列添字, 解決結果)）。
    pub fn columns(&self) -> &[(usize, ResolvedColumn)] {
        &self.columns
    }

    /// 右辺がリテラルの項目（(対象の列添字, 束縛済み値)）。
    pub fn literals(&self) -> &[(usize, Value)] {
        &self.literals
    }

    /// 対象の列だけを使う式・`DEFAULT` の項目の数。
    pub fn expr_count(&self) -> usize {
        self.exprs.len()
    }

    /// 元の列を含む式の項目の数。
    pub fn joined_expr_count(&self) -> usize {
        self.joined_exprs.len()
    }

    /// 対象の列だけを使う式・`DEFAULT` の項目（束縛済み式）。
    pub(crate) fn exprs(&self) -> &[(usize, BoundSetExpr)] {
        &self.exprs
    }

    /// 元の列を含む式の項目（(対象の列添字, 束縛済み式, 参照された元の列の添字)）。
    pub(crate) fn joined_exprs(&self) -> &[(usize, BoundSetExpr, Vec<usize>)] {
        &self.joined_exprs
    }
}

/// 束縛済みの `INSERT` 操作。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BoundMergeInsert {
    slots: Vec<MergeInsertSlot>,
    id_omitted: bool,
    id_value: Option<MergeIdValue>,
    default_values: bool,
    values: BoundAssignments,
}

impl BoundMergeInsert {
    /// 列リスト（省略時は展開後）の代入先。
    pub fn slots(&self) -> &[MergeInsertSlot] {
        &self.slots
    }

    /// 疑似列 `id` を省いたか（移行済みの表の採番。書き込み段が採番する）。
    pub fn id_omitted(&self) -> bool {
        self.id_omitted
    }

    /// 疑似列 `id` へ与える値。
    pub fn id_value(&self) -> Option<&MergeIdValue> {
        self.id_value.as_ref()
    }

    /// `DEFAULT VALUES` か。
    pub fn default_values(&self) -> bool {
        self.default_values
    }

    /// 通常の列への値。
    pub fn values(&self) -> &BoundAssignments {
        &self.values
    }
}

/// 束縛済みの `WHEN` 句の操作。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum BoundMergeAction {
    /// `UPDATE SET ...`。
    Update(BoundAssignments),
    /// `DELETE`。
    Delete,
    /// `INSERT ...`。
    Insert(BoundMergeInsert),
    /// `DO NOTHING`。
    DoNothing,
}

/// 束縛済みの `WHEN` 句。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BoundMergeClause {
    kind: MergeMatchKind,
    condition: Option<BoundCondition>,
    action: BoundMergeAction,
}

impl BoundMergeClause {
    /// 照合の種類。
    pub fn kind(&self) -> MergeMatchKind {
        self.kind
    }

    /// 操作。
    pub fn action(&self) -> &BoundMergeAction {
        &self.action
    }

    /// `AND <条件>` の結合キー候補の数（条件なしは 0）。
    pub fn condition_key_count(&self) -> usize {
        self.condition.as_ref().map_or(0, |c| c.keys().len())
    }

    /// 束縛済みの条件。
    pub(crate) fn condition(&self) -> Option<&BoundCondition> {
        self.condition.as_ref()
    }
}

/// 対象がビューのときの束縛結果（実行と `WITH CHECK OPTION` は #3353〜#3355 が使う）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BoundMergeView {
    base_table: String,
    view_predicate_groups: usize,
    check_predicate_groups: usize,
    base_allocated: bool,
}

impl BoundMergeView {
    /// 連鎖を辿った先の実表名。
    pub fn base_table(&self) -> &str {
        &self.base_table
    }

    /// ビュー定義由来の述語の段数。
    pub fn view_predicate_groups(&self) -> usize {
        self.view_predicate_groups
    }

    /// `WITH CHECK OPTION` の検査対象の段数。
    pub fn check_predicate_groups(&self) -> usize {
        self.check_predicate_groups
    }

    /// 基底表が採番方式か。
    pub fn base_allocated(&self) -> bool {
        self.base_allocated
    }
}

/// 束縛済みの `MERGE`。`relations[0]` が対象、`relations[1]` が元で、解決結果の
/// [`ResolvedColumn::relation`] はこの並びへの添字。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BoundMerge {
    relations: Vec<TableRef>,
    target_view: Option<BoundMergeView>,
    on: BoundCondition,
    clauses: Vec<BoundMergeClause>,
    operation_id: Option<OperationId>,
    /// 束縛済みの `RETURNING`（Issue #3356。relation 0 = 対象・1 = 元。無ければ `None`）。
    returning: Option<crate::sql::dml_from_returning::BoundFromReturning>,
}

impl BoundMerge {
    /// 束縛済みの `RETURNING`（元側のセル位置は計画段 `plan_merge` が確定する）。
    pub(crate) fn returning(&self) -> Option<&crate::sql::dml_from_returning::BoundFromReturning> {
        self.returning.as_ref()
    }

    /// 参照の並び（0 = 対象、1 = 元）。
    pub fn relations(&self) -> &[TableRef] {
        &self.relations
    }

    /// 対象がビューのときの束縛結果。
    pub fn target_view(&self) -> Option<&BoundMergeView> {
        self.target_view.as_ref()
    }

    /// `ON` の結合キー候補（(対象側, 元側)）。
    pub fn on_keys(&self) -> &[(ResolvedColumn, ResolvedColumn)] {
        self.on.keys()
    }

    /// 束縛済みの `WHEN` 句（宣言順）。
    pub fn clauses(&self) -> &[BoundMergeClause] {
        &self.clauses
    }

    /// 検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }

    /// 束縛済みの `ON`。
    pub(crate) fn on(&self) -> &BoundCondition {
        &self.on
    }
}

/// [`bind_merge`] への入力（呼び出し元 `core::merge_exec` が同一スナップショットから用意する）。
pub(crate) struct MergeBindInput<'a> {
    /// 対象の（基底）表スキーマ。
    pub(crate) target_schema: &'a TableSchema,
    /// 対象がビューのときの解決結果。
    pub(crate) target_view: Option<&'a MergeTargetView>,
    /// 元の表スキーマ（実表・単純形ビューの基底表・部分クエリの合成スキーマ）。
    pub(crate) source_schema: &'a TableSchema,
    /// 元が単純形ビューのときの公開列（`None` は制限なし）。
    pub(crate) source_visible: Option<Vec<String>>,
    /// Describe 用の「`$n` 由来のダミー値か」フラグ（`WHEN` 句ごとに SET の代入／INSERT の値の宣言順。
    /// 個数が合わなければ全て検証側へ倒す）。Execute は空。
    pub(crate) dummy_flags: &'a [bool],
}

/// 参照できる側（D4）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Visible {
    Both,
    TargetOnly,
    SourceOnly,
}

/// 束縛の作業域（全体スコープと、片側だけのスコープ）。
struct Ctx<'a> {
    full: BindingScope<'a>,
    target_only: BindingScope<'a>,
    source_only: BindingScope<'a>,
    target: &'a TableRef,
    source: &'a TableRef,
    input: &'a MergeBindInput<'a>,
    udfs: &'a UdfRegistry,
}

impl<'a> Ctx<'a> {
    /// 可視範囲 `vis` で `r` を解決し、全体スコープの解決結果（0 = 対象、1 = 元）へ写す。
    fn resolve(&self, vis: Visible, r: &ColumnRef) -> Result<ResolvedColumn, SqlSurfaceError> {
        let q = self.qualify(vis, r)?;
        self.full.resolve(&q)
    }

    /// 可視範囲 `vis` で `r` を解決し、公開名で修飾した参照へ書き換える。
    fn qualify(&self, vis: Visible, r: &ColumnRef) -> Result<ColumnRef, SqlSurfaceError> {
        let (scope, rels): (&BindingScope<'_>, [(&TableRef, &TableSchema); 1]) = match vis {
            Visible::Both => {
                let resolved = self.full.resolve(r)?;
                return self.qualified_from_full(&resolved);
            }
            Visible::TargetOnly => (&self.target_only, [(self.target, self.input.target_schema)]),
            Visible::SourceOnly => (&self.source_only, [(self.source, self.input.source_schema)]),
        };
        let resolved = scope.resolve(r)?;
        let [(table, schema)] = rels;
        Ok(ColumnRef::qualified(
            table.exposed_name(),
            slot_name(resolved.slot(), schema)?,
        ))
    }

    fn qualified_from_full(&self, resolved: &ResolvedColumn) -> Result<ColumnRef, SqlSurfaceError> {
        let (table, schema) = match resolved.relation() {
            0 => (self.target, self.input.target_schema),
            1 => (self.source, self.input.source_schema),
            _ => return Err(internal()),
        };
        Ok(ColumnRef::qualified(
            table.exposed_name(),
            slot_name(resolved.slot(), schema)?,
        ))
    }

    /// 論理木の列参照を全て解決し、修飾済みの複製を型照合して束縛する。
    fn bind_cond(
        &self,
        vis: Visible,
        expr: &JoinWhereExpr,
    ) -> Result<BoundCondition, SqlSurfaceError> {
        let qualified = map_refs(expr, &|r| self.qualify(vis, r))?;
        bind_condition(
            qualified,
            &|r| self.full.resolve(r),
            self.input.target_schema,
            self.input.source_schema,
            self.udfs,
        )
    }

    /// 対象がビューのとき、公開列に無い名前は `42703`（公開されていない基底列の存在を示さない）。
    fn check_target_visible(&self, name: &str) -> Result<(), SqlSurfaceError> {
        if let Some(cols) = self
            .input
            .target_view
            .and_then(|v| v.view_columns.as_deref())
        {
            if !cols.iter().any(|c| c == name) {
                return Err(SqlSurfaceError::undefined_column(name));
            }
        }
        Ok(())
    }
}

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

fn slot_name(slot: ColumnSlot, schema: &TableSchema) -> Result<String, SqlSurfaceError> {
    match slot {
        ColumnSlot::Id => Ok("id".to_string()),
        ColumnSlot::Column(i) => schema
            .columns
            .get(i)
            .map(|c| c.name.clone())
            .ok_or_else(internal),
    }
}

fn is_integer_type(ty: &crate::catalog::ColumnType) -> bool {
    use crate::catalog::ColumnType;
    matches!(
        ty,
        ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt
    )
}

/// `MERGE` を束縛する（判定順は文書冒頭のとおり決定的）。UDF レジストリは式内の UDF の解決に使う
/// （空なら UDF は `42883` で fail-closed）。
pub(crate) fn bind_merge(
    stmt: &ValidatedMerge,
    input: &MergeBindInput<'_>,
    udfs: &UdfRegistry,
) -> Result<BoundMerge, SqlSurfaceError> {
    // 対象がビューの形（TABLE-43）。列別名つき・利用者列を `id` として公開するビューは本 Issue では未対応。
    let target_view = match input.target_view {
        Some(v) => {
            if v.has_column_map {
                return Err(SqlSurfaceError::feature_not_supported(
                    "MERGE into a view with column aliases is not supported",
                ));
            }
            if v.exposes_user_column_as_id {
                return Err(SqlSurfaceError::feature_not_supported(
                    "MERGE into a view that exposes a user column as id is not supported",
                ));
            }
            Some(BoundMergeView {
                base_table: v.base_table.clone(),
                view_predicate_groups: v.view_predicates.len(),
                check_predicate_groups: v.check_predicates.len(),
                base_allocated: v.base_allocated,
            })
        }
        None => None,
    };

    let source_rel: TableRef = match &stmt.source {
        MergeSource::Table(rel) => rel.clone(),
        MergeSource::Subquery { alias, .. } => TableRef::new(alias.clone()),
    };
    let target_visible = input.target_view.and_then(|v| v.view_columns.clone());
    let full = build_scope_with_target_visible(
        &stmt.target,
        input.target_schema,
        target_visible.clone(),
        std::slice::from_ref(&source_rel),
        &[input.source_schema],
        std::slice::from_ref(&input.source_visible),
    )?;
    let target_only = BindingScope::new(vec![(stmt.target.clone(), input.target_schema)])?
        .with_visible(vec![target_visible]);
    let source_only = BindingScope::new(vec![(source_rel.clone(), input.source_schema)])?
        .with_visible(vec![input.source_visible.clone()]);
    let ctx = Ctx {
        full,
        target_only,
        source_only,
        target: &stmt.target,
        source: &source_rel,
        input,
        udfs,
    };

    let on = ctx.bind_cond(Visible::Both, &stmt.on)?;

    // Describe 用フラグは構文木から求めた総数と一致するときだけ使う（fail-closed）。
    let total_values: usize = stmt
        .when_clauses
        .iter()
        .map(|c| match &c.action {
            MergeAction::Update { assignments } => assignments.len(),
            MergeAction::Insert {
                values: MergeInsertValues::Values(v),
                ..
            } => v.len(),
            _ => 0,
        })
        .fold(0usize, usize::saturating_add);
    let usable_flags = input.dummy_flags.len() == total_values;
    let mut flag_pos: usize = 0;

    let mut clauses: Vec<BoundMergeClause> = Vec::with_capacity(stmt.when_clauses.len());
    for clause in &stmt.when_clauses {
        let vis = match clause.kind {
            MergeMatchKind::Matched => Visible::Both,
            MergeMatchKind::NotMatchedByTarget => Visible::SourceOnly,
            MergeMatchKind::NotMatchedBySource => Visible::TargetOnly,
        };
        let condition = clause
            .condition
            .as_ref()
            .map(|c| ctx.bind_cond(vis, c))
            .transpose()?;
        let n_values = match &clause.action {
            MergeAction::Update { assignments } => assignments.len(),
            MergeAction::Insert {
                values: MergeInsertValues::Values(v),
                ..
            } => v.len(),
            _ => 0,
        };
        let flags: Vec<bool> = if usable_flags {
            input
                .dummy_flags
                .get(flag_pos..flag_pos.saturating_add(n_values))
                .map(<[bool]>::to_vec)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        flag_pos = flag_pos.saturating_add(n_values);
        let action = match &clause.action {
            MergeAction::Update { assignments } => {
                BoundMergeAction::Update(bind_update(&ctx, vis, assignments, &flags)?)
            }
            MergeAction::Delete => BoundMergeAction::Delete,
            MergeAction::DoNothing => BoundMergeAction::DoNothing,
            MergeAction::Insert { columns, values } => {
                BoundMergeAction::Insert(bind_insert(&ctx, columns.as_deref(), values, &flags)?)
            }
        };
        clauses.push(BoundMergeClause {
            kind: clause.kind,
            condition,
            action,
        });
    }

    // `RETURNING`（Issue #3356）は両側スコープで束縛する。未知列・曖昧列・未知の修飾・集計・部分クエリ・
    // 元の列を式の中で参照する形のエラーは、ここ（書き込みより前）で返る。
    let returning = stmt
        .returning
        .as_ref()
        .map(|r| crate::sql::dml_from_returning::bind_merge_returning(r, &ctx.full, udfs))
        .transpose()?;

    Ok(BoundMerge {
        relations: vec![stmt.target.clone(), source_rel],
        target_view,
        on,
        clauses,
        operation_id: stmt.operation_id.clone(),
        returning,
    })
}

/// `flags[i]` が真で、値が文字列リテラル（`$n` 由来のダミー）なら、値の検証を Bind まで遅延する。
fn is_dummy(flags: &[bool], i: usize, v: &SetValue) -> bool {
    flags.get(i).copied().unwrap_or(false)
        && matches!(v, SetValue::Literal(InsertLiteral::String(_)))
}

/// `UPDATE SET` を束縛する（`dml_from::bind_update_from_describe` の代入ループと同じ規則）。
fn bind_update(
    ctx: &Ctx<'_>,
    vis: Visible,
    assignments: &[(String, DmlFromSetValue)],
    flags: &[bool],
) -> Result<BoundAssignments, SqlSurfaceError> {
    let schema = ctx.input.target_schema;
    let mut out = BoundAssignments::default();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut plain: Vec<(String, SetValue)> = Vec::new();
    for (ai, (name, value)) in assignments.iter().enumerate() {
        if !seen.insert(name.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in SET clause: {name}"
            )));
        }
        ctx.check_target_visible(name)?;
        // 値（リテラル・`DEFAULT`・対象だけの式）は `bind_set_assignments` が左辺の検査を含めて束縛する
        // （`SET g = DEFAULT` は生成列で受理・GENERATED ALWAYS の IDENTITY で 0A000。`UPDATE ... FROM` と
        // 同じ規則）。そのため `check_set_target`（値の指定を 428C9 で拒否する）より先に振り分ける。
        if let DmlFromSetValue::Value(v, _) = value {
            if is_dummy(flags, ai, v) {
                check_set_target(name, schema)?;
                continue;
            }
            plain.push((name.clone(), v.clone()));
            continue;
        }
        let idx = check_set_target(name, schema)?;
        let target_col = schema
            .columns
            .get(idx)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
        match value {
            DmlFromSetValue::Column(source) => {
                let resolved = ctx.resolve(vis, source)?;
                let source_ty = resolved
                    .column_type()
                    .unwrap_or(&crate::sql::exec::ALIASED_ID_TYPE);
                if !crate::sql::insert_select::assignable(source_ty, &target_col.ty) {
                    return Err(SqlSurfaceError::datatype_mismatch(format!(
                        "column {name:?} cannot be assigned a value of a different type"
                    )));
                }
                out.columns.push((idx, resolved));
            }
            DmlFromSetValue::Expr { expr, refs } => {
                let resolved = refs
                    .iter()
                    .map(|r| ctx.resolve(vis, r))
                    .collect::<Result<Vec<_>, _>>()?;
                if resolved.iter().all(|r| r.relation() == 0) {
                    let names = resolved
                        .iter()
                        .map(|r| crate::sql::dml_from::target_slot_name(r, schema))
                        .collect::<Result<Vec<_>, _>>()?;
                    plain.push((
                        name.clone(),
                        SetValue::Expr(crate::sql::dml_from::restore_set_refs(expr, &names)),
                    ));
                } else {
                    let (bound, cols) = bind_joined_set_expr(
                        target_col,
                        expr,
                        &resolved,
                        schema,
                        ctx.input.source_schema,
                    )?;
                    out.joined_exprs.push((idx, bound, cols));
                }
            }
            DmlFromSetValue::Value(..) => {}
        }
    }
    let (literals, exprs) = crate::sql::parser::bind_set_assignments(&plain, schema)?;
    out.literals = literals;
    out.exprs = exprs;
    out.declared_columns = assignments
        .iter()
        .filter_map(|(name, _)| schema.columns.iter().position(|c| &c.name == name))
        .collect();
    Ok(out)
}

/// `INSERT` を束縛する（単独の INSERT と同じ規則。値は元だけを参照できる）。
fn bind_insert(
    ctx: &Ctx<'_>,
    columns: Option<&[String]>,
    values: &MergeInsertValues,
    flags: &[bool],
) -> Result<BoundMergeInsert, SqlSurfaceError> {
    let schema = ctx.input.target_schema;
    let vis = Visible::SourceOnly;
    let value_list: &[DmlFromSetValue] = match values {
        MergeInsertValues::Values(v) => v,
        MergeInsertValues::DefaultValues => &[],
    };
    // `DEFAULT VALUES` は列リストを取らない（列を挙げて値を与えない形は構文エラー）。
    if matches!(values, MergeInsertValues::DefaultValues) && columns.is_some_and(|c| !c.is_empty())
    {
        return Err(SqlSurfaceError::unsupported(
            "INSERT ... DEFAULT VALUES does not take a column list",
        ));
    }
    let names: Vec<String> = match columns {
        Some(c) => c.to_vec(),
        None => match values {
            MergeInsertValues::DefaultValues => Vec::new(),
            MergeInsertValues::Values(v) => {
                // 公開列を絞るビューは列リストの省略を受理しない（公開列の並びを決められない。fail-closed）。
                if ctx
                    .input
                    .target_view
                    .is_some_and(|tv| tv.view_columns.is_some())
                {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "MERGE INSERT into a view with a column list restriction requires a column list",
                    ));
                }
                crate::sql::parser::expand_omitted_insert_columns(schema, v.len())?
            }
        },
    };
    crate::sql::parser::check_insert_column_list_duplicates(&names)?;

    let has_pseudo_id = schema.has_pseudo_id();
    let mut slots: Vec<MergeInsertSlot> = Vec::with_capacity(names.len());
    for (pos, name) in names.iter().enumerate() {
        ctx.check_target_visible(name)?;
        if has_pseudo_id && name == "id" {
            slots.push(MergeInsertSlot::Id);
            continue;
        }
        let idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
        let column = schema
            .columns
            .get(idx)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
        // 裸の `DEFAULT` は通常の INSERT と同じく列の省略と等価（GENERATED ALWAYS 列でも許される）ため、
        // 利用者指定の値だけを 428C9 で拒否する。`DEFAULT` の補完は実行側（#3353 以降）が省略列と同じ経路で行う。
        let is_bare_default = matches!(
            value_list.get(pos),
            Some(DmlFromSetValue::Value(SetValue::Default, _))
        );
        if !is_bare_default {
            crate::sql::parser::reject_generated_always_value(column)?;
        }
        slots.push(MergeInsertSlot::Column(idx));
    }
    // 移行済みの表で `id` に裸の `DEFAULT` を与えた形は、単発の INSERT と同じく `id` の省略として扱う。
    let id_defaulted = schema.is_migrated_id()
        && slots.iter().enumerate().any(|(pos, slot)| {
            *slot == MergeInsertSlot::Id
                && matches!(
                    value_list.get(pos),
                    Some(DmlFromSetValue::Value(SetValue::Default, _))
                )
        });
    let id_listed = slots.contains(&MergeInsertSlot::Id) && !id_defaulted;
    let id_omitted = has_pseudo_id && !id_listed && schema.is_migrated_id();
    if has_pseudo_id && !id_listed && !id_omitted {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT column list must include the id pseudo-column",
        ));
    }
    if let MergeInsertValues::Values(_) = values {
        if value_list.len() != slots.len() {
            return Err(SqlSurfaceError::unsupported(
                if value_list.len() > slots.len() {
                    "INSERT has more expressions than target columns"
                } else {
                    "INSERT has more target columns than expressions"
                },
            ));
        }
    }

    let mut out = BoundAssignments::default();
    let mut id_value: Option<MergeIdValue> = None;
    let mut plain: Vec<(String, SetValue)> = Vec::new();
    for (i, (slot, value)) in slots.iter().zip(value_list.iter()).enumerate() {
        match slot {
            MergeInsertSlot::Id => match value {
                DmlFromSetValue::Column(source) => {
                    let resolved = ctx.resolve(vis, source)?;
                    if resolved.column_type().is_some_and(|t| !is_integer_type(t)) {
                        return Err(SqlSurfaceError::datatype_mismatch(
                            "INSERT value type does not match the id pseudo-column",
                        ));
                    }
                    id_value = Some(MergeIdValue::Source(resolved));
                }
                DmlFromSetValue::Value(v, _) if is_dummy(flags, i, v) => {}
                // 移行済みの表の `id DEFAULT` は省略と等価（束縛済みの値を持たない。実行側が採番する）。
                DmlFromSetValue::Value(SetValue::Default, _) if schema.is_migrated_id() => {}
                DmlFromSetValue::Value(SetValue::Literal(InsertLiteral::Number(n)), _) => {
                    let parsed: u64 = n.parse().map_err(|_| {
                        SqlSurfaceError::invalid_input(
                            "id pseudo-column value must be a non-negative integer",
                        )
                    })?;
                    id_value = Some(MergeIdValue::Literal(parsed));
                }
                _ => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "MERGE INSERT of the id pseudo-column supports only a source column or an integer literal",
                    ));
                }
            },
            MergeInsertSlot::Column(idx) => {
                let target_col = schema.columns.get(*idx).ok_or_else(internal)?;
                match value {
                    DmlFromSetValue::Column(source) => {
                        let resolved = ctx.resolve(vis, source)?;
                        let source_ty = resolved
                            .column_type()
                            .unwrap_or(&crate::sql::exec::ALIASED_ID_TYPE);
                        if !crate::sql::insert_select::assignable(source_ty, &target_col.ty) {
                            return Err(SqlSurfaceError::datatype_mismatch(format!(
                                "column {:?} cannot be assigned a value of a different type",
                                target_col.name
                            )));
                        }
                        out.columns.push((*idx, resolved));
                    }
                    DmlFromSetValue::Expr { expr, refs } => {
                        let resolved = refs
                            .iter()
                            .map(|r| ctx.resolve(vis, r))
                            .collect::<Result<Vec<_>, _>>()?;
                        let (bound, cols) = bind_joined_set_expr(
                            target_col,
                            expr,
                            &resolved,
                            schema,
                            ctx.input.source_schema,
                        )?;
                        out.joined_exprs.push((*idx, bound, cols));
                    }
                    // 裸の `DEFAULT`: 束縛済みの値を持たない省略列として扱う（SET の `bind_set_default` 規則
                    // = 時刻・IDENTITY・SEQUENCE の 0A000 には従わない）。
                    DmlFromSetValue::Value(SetValue::Default, _) => {}
                    DmlFromSetValue::Value(v, _) => {
                        if is_dummy(flags, i, v) {
                            continue;
                        }
                        plain.push((target_col.name.clone(), v.clone()));
                    }
                }
            }
        }
    }
    let (literals, exprs) = crate::sql::parser::bind_set_assignments(&plain, schema)?;
    out.literals = literals;
    out.exprs = exprs;
    Ok(BoundMergeInsert {
        slots,
        id_omitted,
        id_value,
        default_values: matches!(values, MergeInsertValues::DefaultValues),
        values: out,
    })
}

#[cfg(test)]
#[path = "merge_bind_tests.rs"]
mod tests;
