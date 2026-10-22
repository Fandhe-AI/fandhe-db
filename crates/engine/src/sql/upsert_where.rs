//! `INSERT ... ON CONFLICT ... DO UPDATE SET ... WHERE <条件>` の条件の束縛と評価
//! （SQL-49 (3)、Issue #1740。内容照合は (5)、書き込み失敗時の痕跡ゼロは RECOVER-11）。
//!
//! 責務境界:
//! - 構文段（`sql::allowlist::Parser::parse_upsert_where_condition`）が、通常の `WHERE` と
//!   同じ述語文法で条件を解析し、`EXCLUDED.<col>` を [`EXCLUDED_PREFIX`] 付きの列名へ
//!   1 対 1 に書き換えた `Vec<WherePredicate>` を作る。
//! - 本モジュールは、その述語列を**合成スキーマ**（[`synthetic_schema`]。衝突した既存行の
//!   列 `0..n` と `EXCLUDED` 側の列 `n..2n`）に対して束縛し、禁止要素を fail-closed で
//!   拒否し、行ごとに三値論理で評価する。束縛は既存の `sql::parser::bind_where_predicates`、
//!   評価は `sql::view_buffered::where_matches_for_row`（外側 `WHERE` と同じ規則）に
//!   委譲し、第 2 の述語束縛器・評価器は作らない。
//! - 呼び出し元: `sql::parser::bind_upsert_form` が束縛時に [`CompiledUpsertWhere::compile`] で
//!   検証し、`sql::exec::execute_upsert_inner` が実行時に文ごとに 1 回コンパイルして
//!   `tenant::upsert_typed_rows_impl` の `DoUpdate` 腕へ評価クロージャとして渡す
//!   （`tenant` は `sql` に依存しない）。
//!
//! 評価位置: 所有かつ可視と確定した衝突行だけが対象で、不可視の衝突は条件を評価する前に
//! `42501` で文全体が拒否される（値依存のエラーが存在オラクルにならない）。条件が偽または
//! NULL の行は更新せず、件数・`RETURNING` にも数えない。
//!
//! 拒否する要素（いずれも固定文言で、値・他テナント情報を含めない）:
//! `visible()`（RLS 述語）・サブクエリ・組み込み以外の関数（UDF・WASM を書き込み経路で
//! 実行しない）・`tenant_id`／`visibility`・`EXCLUDED.id`・VECTOR 列と `embedding`・
//! 未知の `EXCLUDED.<col>`。

use std::cell::RefCell;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::declarative_filter::MetadataFilter;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::{SqlSurfaceError, WherePredicate};
use crate::sql::expr_program::{ExprProgram, StackValue};
use crate::sql::udf_call::{Expr, UdfRegistry};
use crate::sql::where_tree::BoundOrGroup;

/// `EXCLUDED.<col>` を合成スキーマの列名へ写すときの予約接頭辞。NUL 文字を含むため
/// SQL の識別子としては書けず（構文段が引用識別子による偽装を拒否する）、利用者の列名と
/// 衝突しない。`DO UPDATE SET` 右辺の一般式（兄弟 Issue #1739）が同じ規約を使うときは、
/// 後からマージする側がこの定数に一本化する。
pub(crate) const EXCLUDED_PREFIX: &str = "\u{0}excluded.";

/// 束縛・コンパイル済みの `DO UPDATE ... WHERE` 条件。
pub(crate) struct CompiledUpsertWhere {
    metadata_filters: Vec<MetadataFilter>,
    or_groups: Vec<BoundOrGroup>,
    programs: Vec<ExprProgram>,
    /// 実表の列数 `n`（既存行ビューと `EXCLUDED` ビューが共に満たすべき長さ）。
    column_count: usize,
}

/// 合成スキーマ（既存行の列 `0..n`・`EXCLUDED` の列 `n..2n`）を作る。VECTOR 列は両側とも
/// 書けない名前のプレースホルダにして、参照を束縛段で拒否する（埋め込みを条件で扱わない）。
/// 行識別方式は実表と揃える（採用方式の表で疑似列 `id` を作らない）。
pub(crate) fn synthetic_schema(schema: &TableSchema) -> TableSchema {
    let n = schema.columns.len();
    let mut defs: Vec<ColumnDef> = Vec::with_capacity(n.saturating_mul(2));
    for (i, col) in schema.columns.iter().enumerate() {
        defs.push(synthetic_column(i, &col.name, col));
    }
    for (i, col) in schema.columns.iter().enumerate() {
        let name = format!("{EXCLUDED_PREFIX}{}", col.name);
        defs.push(synthetic_column(n.saturating_add(i), &name, col));
    }
    TableSchema::new(schema.name.clone(), defs).with_row_identity(schema.row_identity())
}

fn synthetic_column(position: usize, name: &str, col: &ColumnDef) -> ColumnDef {
    match col.ty {
        ColumnType::Vector(_) => ColumnDef::new(
            format!("\u{0}unreachable#{position}"),
            ColumnType::Text,
            true,
        ),
        _ => {
            // 照合順序は文字列比較の結果を左右するため、実表の列と同じものを引き継ぐ
            // （`DO UPDATE SET` 式側の列 clone と WHERE 側の比較結果を一致させる）。
            let mut def = ColumnDef::new(name.to_string(), col.ty.clone(), true);
            def.collation = col.collation;
            def
        }
    }
}

impl CompiledUpsertWhere {
    /// 構文段が作った述語列を `schema`（実表のスキーマ）に対して検証・束縛・コンパイルする。
    /// `predicates` は空でないこと（空は「`WHERE` なし」で、呼び出し元がこの関数を呼ばない）。
    pub(crate) fn compile(
        predicates: &[WherePredicate],
        schema: &TableSchema,
    ) -> Result<CompiledUpsertWhere, SqlSurfaceError> {
        reject_forbidden_elements(predicates)?;
        reject_forbidden_columns(predicates, schema)?;

        let synthetic = synthetic_schema(schema);
        let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let (metadata_filters, expr_filters, rls_predicate_present, or_groups) =
            crate::sql::parser::bind_where_predicates(
                predicates,
                &synthetic,
                &UdfRegistry::default(),
                &mut node_budget,
                &[],
            )
            .map_err(strip_reserved_prefix)?;
        if rls_predicate_present {
            // 多層防御（`reject_forbidden_elements` が先に拒否済み）。
            return Err(SqlSurfaceError::unsupported(
                "visible() is not allowed in ON CONFLICT DO UPDATE WHERE",
            ));
        }
        let programs = expr_filters.iter().map(ExprProgram::compile).collect();
        Ok(CompiledUpsertWhere {
            metadata_filters,
            or_groups,
            programs,
            column_count: schema.columns.len(),
        })
    }

    /// 衝突した既存行（`existing`）と挿入しようとした行（`excluded`）に対して条件を評価する。
    /// 真のときだけ `true`。偽と NULL（UNKNOWN）は `false`（更新しない側）。ビューの長さが
    /// 実表の列数と一致しない場合は内部不変条件違反として `Internal`（fail-closed）。
    pub(crate) fn matches(
        &self,
        existing: &[Option<ScalarRef<'_>>],
        excluded: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        if existing.len() != self.column_count || excluded.len() != self.column_count {
            return Err(SqlSurfaceError::Internal {
                detail: "ON CONFLICT DO UPDATE WHERE row view length mismatch".to_string(),
            });
        }
        let mut combined: Vec<Option<ScalarRef<'_>>> = Vec::new();
        combined
            .try_reserve_exact(self.column_count.saturating_mul(2))
            .map_err(|_| {
                SqlSurfaceError::payload_too_large(
                    "failed to reserve ON CONFLICT DO UPDATE WHERE row view",
                )
            })?;
        combined.extend(existing.iter().copied());
        combined.extend(excluded.iter().copied());
        let mut scratch: Vec<StackValue> = Vec::new();
        crate::sql::view_buffered::where_matches_for_row(
            &self.metadata_filters,
            &self.or_groups,
            &self.programs,
            &combined,
            id,
            &mut scratch,
        )
    }
}

/// 予約接頭辞を含むエラー文言をクライアントへ出さない（未知列など束縛段のエラーが
/// 合成スキーマの内部名を含み得るため。`reject_forbidden_columns` が先に拒否するので
/// 通常は到達しない多層防御）。
fn strip_reserved_prefix(e: SqlSurfaceError) -> SqlSurfaceError {
    if e.to_string().contains('\u{0}') {
        SqlSurfaceError::unsupported("invalid column reference in ON CONFLICT DO UPDATE WHERE")
    } else {
        e
    }
}

/// 述語木の式（`Expression`）に `f` を適用する（`Not`・`Or` の内側を含む）。
fn for_each_expr(
    predicates: &[WherePredicate],
    f: &mut dyn FnMut(&Expr) -> Result<(), SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    for predicate in predicates {
        match predicate {
            WherePredicate::Expression(expr) => f(expr)?,
            WherePredicate::Not(inner) => for_each_expr(std::slice::from_ref(inner), f)?,
            WherePredicate::Or(branches) => {
                for branch in branches {
                    for_each_expr(branch, f)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `visible()`・サブクエリ・組み込み以外の関数を `42601` で拒否する。
fn reject_forbidden_elements(predicates: &[WherePredicate]) -> Result<(), SqlSurfaceError> {
    fn walk(predicates: &[WherePredicate]) -> Result<(), SqlSurfaceError> {
        for predicate in predicates {
            match predicate {
                WherePredicate::PredicateCall { .. } => {
                    return Err(SqlSurfaceError::unsupported(
                        "visible() is not allowed in ON CONFLICT DO UPDATE WHERE",
                    ));
                }
                WherePredicate::InSubquery { .. }
                | WherePredicate::Exists { .. }
                | WherePredicate::ScalarSubqueryCompare { .. } => {
                    return Err(SqlSurfaceError::unsupported(
                        "subquery is not allowed in ON CONFLICT DO UPDATE WHERE",
                    ));
                }
                WherePredicate::Not(inner) => walk(std::slice::from_ref(inner))?,
                WherePredicate::Or(branches) => {
                    for branch in branches {
                        walk(branch)?;
                    }
                }
                WherePredicate::IdCompare { .. } => {
                    return Err(SqlSurfaceError::unsupported(
                        "unsupported predicate in ON CONFLICT DO UPDATE WHERE",
                    ));
                }
                WherePredicate::Equality { .. }
                | WherePredicate::Prefix { .. }
                | WherePredicate::Expression(_)
                | WherePredicate::BoolEquality { .. }
                | WherePredicate::BoolColumn { .. }
                | WherePredicate::Compare { .. }
                | WherePredicate::InList { .. }
                | WherePredicate::Between { .. }
                | WherePredicate::IsNull { .. } => {}
            }
        }
        Ok(())
    }
    walk(predicates)?;
    for_each_expr(
        predicates,
        &mut |expr| match crate::sql::udf_call::first_non_builtin_call(expr) {
            Some(_) => Err(SqlSurfaceError::unsupported(
                "only built-in functions are allowed in ON CONFLICT DO UPDATE WHERE",
            )),
            None => Ok(()),
        },
    )
}

/// 参照される列名を走査し、書けない列（`tenant_id`／`visibility`・`EXCLUDED.id`・
/// VECTOR 列・未知の `EXCLUDED.<col>`）を拒否する。走査は `check_constraint` と
/// ビュー別名が共有する `map_predicate_columns`（列名を変えずに記録するだけ）を使う。
fn reject_forbidden_columns(
    predicates: &[WherePredicate],
    schema: &TableSchema,
) -> Result<(), SqlSurfaceError> {
    let names: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let record = |n: &str| {
        names.borrow_mut().push(n.to_string());
        n.to_string()
    };
    for predicate in predicates {
        crate::sql::check_constraint::map_predicate_columns(predicate, &record, &|| {
            SqlSurfaceError::unsupported("subquery is not allowed in ON CONFLICT DO UPDATE WHERE")
        })?;
    }
    for name in names.into_inner() {
        let (is_excluded, base) = match name.strip_prefix(EXCLUDED_PREFIX) {
            Some(rest) => (true, rest),
            None => (false, name.as_str()),
        };
        if base == "tenant_id" || base == "visibility" {
            return Err(SqlSurfaceError::unsupported(format!(
                "column {base:?} cannot be referenced in ON CONFLICT DO UPDATE WHERE"
            )));
        }
        if is_excluded && base == "id" && schema.has_pseudo_id() {
            return Err(SqlSurfaceError::unsupported(
                "column \"id\" cannot be referenced through EXCLUDED",
            ));
        }
        match schema.columns.iter().find(|c| c.name == base) {
            Some(col) if matches!(col.ty, ColumnType::Vector(_)) => {
                return Err(SqlSurfaceError::feature_not_supported(format!(
                    "VECTOR column {base:?} is not supported in ON CONFLICT DO UPDATE WHERE"
                )));
            }
            Some(_) => {}
            None if is_excluded => {
                return Err(SqlSurfaceError::undefined_column(format!(
                    "EXCLUDED.{base}"
                )));
            }
            // 修飾なしの未知列・`id`・`embedding` は束縛段（合成スキーマ）が拒否または解決する。
            None => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod collation_tests {
    use super::*;
    use crate::collation::Collation;

    /// 合成スキーマは既存行側・`EXCLUDED` 側の双方で実表列の照合順序を保持する。
    #[test]
    fn synthetic_schema_preserves_collation() {
        let col = ColumnDef::new("name", ColumnType::Text, true).with_collation(Collation::C);
        let schema = TableSchema::new("t".to_string(), vec![col]);
        let synth = synthetic_schema(&schema);
        assert_eq!(synth.columns.len(), 2);
        for c in &synth.columns {
            assert_eq!(c.collation, Some(Collation::C));
        }
    }
}
