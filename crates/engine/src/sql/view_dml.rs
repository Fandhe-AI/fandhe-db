//! ビューへの `UPDATE`／`DELETE` の基表への書き換え（TABLE-43・SQL-49・RLS-10。Issue #1672）。
//!
//! 責務境界: `UPDATE <view> SET <公開名> = ... WHERE ...`・`DELETE FROM <view> WHERE ...` の構造検証
//! 済みの形（[`ValidatedViewUpdate`]・[`ValidatedViewDelete`]）を、基表への
//! [`ValidatedUpdateForm`]・[`DeleteStatement`] へ名前の写像と述語の合成だけで書き換える。SQL 文字列は
//! 組み立てず、書き換え後の束縛・RLS・制約・影響行数の上限・台帳・`RETURNING` の捕捉は既存の基表 DML
//! 経路（`core.rs` の `execute_*_form` 系）にそのまま乗る（第 2 の書き込み経路を作らない）。
//! 呼び出し元は `core.rs`: Parse 時は [`check_view_update`]／[`check_view_delete`]（判定だけ）、
//! Execute／Describe 時は [`rewrite_view_update`]／[`rewrite_view_delete`]（現在のカタログで書き換え）
//! を呼ぶ。Parse 時の写像は持ち越さない。INSERT 側は `sql::view_insert` で、写像器と `RETURNING` の
//! 書き換えを共用する。
//!
//! 述語の合成: ビュー連鎖の `WHERE` 述語（`sql::view::resolve_from` の `view_predicates`。内側の段が先）
//! を前に置き、利用者の述語（公開名→物理名へ書き換え済み）を後ろへ追記する（読み取り経路の
//! `build_scan_from_resolved` と同じ順）。ビューの外にある行は更新も削除もされない。
//!
//! 順序の不変条件（`sql::view_alias`・`sql::view_insert` と同じ）: 更新可能性 → 解決 → `PARTITIONED`
//! 拒否 → 公開列集合の検査（**公開名に対して先に**）→ 物理名への写像。公開されていない基表の列を
//! `WHERE`／`SET`／`RETURNING` に使うと `42703`（filter oracle の防止）。別名で疑似列 `id` へ写る公開名は
//! どの位置でも `0A000`（fail-closed）。
//!
//! 単一行形 `WHERE id = <n>` は、ビューが `id` を公開していなければ `42703`、利用者列を公開名 `id` で
//! 公開していれば `0A000`、ビュー述語が空なら単一行形のまま基表名へ、空でなければ述語形へ変換する
//! （`id` の葉は構文解析器が `WHERE <述語> AND id = <n>` を読んだときと同じ AST）。
//!
//! 対象外: `PARTITIONED` 修飾はビュー経由では `0A000`（分割実行はチャンクごとに commit し、指紋照合と
//! 再試行の意味が定まらない）。`WITH CHECK OPTION`（ビュー条件の外へ行を動かす UPDATE の検査）は
//! Issue #1673 の担当。
//!
//! RLS-10 (b): 名前の写像と述語の追加（AND）だけで行の可視性を緩めない。更新・削除される行の所有
//! テナントは書き込みセッションの `PolicyContext` が決め、ビュー作成者の可視性は引き継がれない。

use super::allowlist::{
    id_equality_predicates, DeleteStatement, Projection, SetValue, SqlSurfaceError, TableLookup,
    ValidatedDelete, ValidatedPredicateDelete, ValidatedPredicateUpdate, ValidatedUpdate,
    ValidatedUpdateForm, ValidatedViewDelete, ValidatedViewUpdate, ViewWriteOp, WherePredicate,
};
use super::view::{check_columns_within_view, expr_columns_within, resolve_from, Resolved};
use super::view_alias::{rewrite_set_value, rewrite_where, ColumnMap};
use super::view_insert::{rewrite_returning, Mapper};
use super::view_updatable::{view_updatability, ViewUpdatability};

/// 書き換え結果（`DELETE`）。
#[derive(Debug)]
pub(crate) struct RewrittenViewDelete {
    /// 基表形の DELETE（`table_name` は基表）。
    pub(crate) delete: DeleteStatement,
    /// `RETURNING` の出力列名（ビューの公開名。写像が不要なら `None`）。
    pub(crate) public_names: Option<Vec<String>>,
}

/// 書き換え結果（`UPDATE`）。
#[derive(Debug)]
pub(crate) struct RewrittenViewUpdate {
    /// 基表形の UPDATE（`table_name` は基表）。
    pub(crate) update: ValidatedUpdateForm,
    /// `RETURNING` の出力列名（ビューの公開名。写像が不要なら `None`）。
    pub(crate) public_names: Option<Vec<String>>,
    /// `WITH CHECK OPTION` の検査対象の述語（TABLE-43 (2)・Issue #1673。内側が先頭・物理列名）。
    pub(crate) check_predicates: Vec<Vec<WherePredicate>>,
}

/// 連鎖を畳み込んだビューの解決結果（`Resolved::View` の所有版）。
struct ResolvedView {
    base_table: String,
    view_predicates: Vec<WherePredicate>,
    view_columns: Option<Vec<String>>,
    column_map: Option<Vec<(String, String)>>,
    /// `WITH CHECK OPTION` の検査対象の述語（内側が先。[`Resolved::View::check_predicates`]）。
    check_predicates: Vec<Vec<WherePredicate>>,
    /// 基表が採番方式（疑似列 `id` を持たない。Issue #2199）か。
    base_allocated: bool,
}

impl ResolvedView {
    fn mapper(&self) -> Mapper<'_> {
        Mapper {
            columns: self.view_columns.as_deref(),
            map: self.column_map.as_deref(),
            base_allocated: self.base_allocated,
        }
    }

    /// 利用者列を公開名 `id` で公開しているか（疑似列 `id` と取り違えないための判定）。
    fn exposes_user_column_as_id(&self) -> bool {
        !self.base_allocated
            && self.column_map.as_deref().is_some_and(|m| {
                m.iter()
                    .any(|(public, phys)| public == "id" && phys != "id")
            })
    }
}

/// 自動更新可能性を判定してビューを解決する。自動更新不可は `55000`、連鎖の解決失敗はそのまま返す
/// （「更新可能」側へ倒さない）。
fn resolve_for_write(
    lookup: &impl TableLookup,
    view_name: &str,
    op: ViewWriteOp,
) -> Result<ResolvedView, SqlSurfaceError> {
    match view_updatability(lookup, view_name)? {
        ViewUpdatability::AutoUpdatable => {}
        ViewUpdatability::NotAutoUpdatable(reason) => {
            return Err(SqlSurfaceError::ViewNotAutoUpdatable { op, reason });
        }
    }
    match resolve_from(lookup, view_name)? {
        Resolved::View {
            base_table,
            view_predicates,
            view_columns,
            column_map,
            check_predicates,
            base_allocated,
        } => Ok(ResolvedView {
            base_table,
            view_predicates,
            view_columns,
            column_map,
            check_predicates,
            base_allocated,
        }),
        // 自動更新可能と判定済みなのに `View` 以外になるのは不変条件違反（固定文言。fail-closed）。
        _ => Err(SqlSurfaceError::Internal {
            detail: "invalid view definition".to_string(),
        }),
    }
}

/// `PARTITIONED` 修飾はビュー経由では未対応（固定文言。fail-closed）。
fn reject_partitioned() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("PARTITIONED is not supported on views")
}

/// 述語形 `WHERE` を公開名で検査してから物理名へ写し、ビューの述語を前に合成する。
fn merge_predicates(
    rv: &ResolvedView,
    predicates: &[WherePredicate],
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    // Issue #1750・SQL-53: ビュー経由の DML の WHERE 内サブクエリは未対応。内側の識別子が
    // ビュー公開名か基表名か定まらないため、column_map の有無によらず入口で一律に拒否する。
    if predicates
        .iter()
        .any(crate::sql::subquery::predicate_contains_subquery)
    {
        return Err(SqlSurfaceError::unsupported(
            "subquery in WHERE is not supported for DML through a view",
        ));
    }
    check_columns_within_view(
        rv.view_columns.as_deref(),
        &Projection::All,
        predicates,
        &[],
    )?;
    let user = match rv.column_map.as_deref() {
        Some(map) => rewrite_where(
            &ColumnMap::new(map, !rv.base_allocated),
            predicates.to_vec(),
        )?,
        None => predicates.to_vec(),
    };
    let mut merged = rv.view_predicates.clone();
    merged.extend(user);
    Ok(merged)
}

/// 単一行形 `WHERE id = <n>` の基表側の形。
enum BaseIdForm {
    /// ビュー述語が無いので単一行形のまま基表へ写す。
    Single(String),
    /// ビュー述語と AND 合成した述語形。
    Predicates(Vec<WherePredicate>),
}

fn rewrite_id_form(
    lookup: &impl TableLookup,
    rv: &ResolvedView,
    id_literal: &str,
) -> Result<BaseIdForm, SqlSurfaceError> {
    if let Some(columns) = rv.view_columns.as_deref() {
        if !columns.iter().any(|c| c == "id") {
            return Err(SqlSurfaceError::undefined_column("id"));
        }
    }
    if rv.exposes_user_column_as_id() {
        return Err(SqlSurfaceError::feature_not_supported(
            "WHERE id = <n> on a view that exposes a user column as id is not supported",
        ));
    }
    // 基表が採番方式（暗黙の `id` を持たない。TABLE-23・Issue #2199）なら、`id` は基表の通常の列なので
    // 物理キーの直接指定（単一行形）ではなく述語形へ戻す。宣言の無い `id` は述語の束縛が未定義列にする。
    let base_allocated = lookup.table_is_allocated(&rv.base_table)?;
    if rv.view_predicates.is_empty() && !base_allocated {
        return Ok(BaseIdForm::Single(id_literal.to_string()));
    }
    // 生成した述語は公開名 `id` で組まれているため、述語形 DML と同じ公開名→物理名の解決経路を通す。
    let merged = merge_predicates(rv, &id_equality_predicates(id_literal)?)?;
    Ok(BaseIdForm::Predicates(merged))
}

/// Parse 時の検証（`DELETE`）。結果は捨てる。判定は [`rewrite_view_delete`] と同じ関数を通す。
pub(crate) fn check_view_delete(
    lookup: &impl TableLookup,
    stmt: &ValidatedViewDelete,
) -> Result<(), SqlSurfaceError> {
    rewrite_view_delete(lookup, stmt).map(|_| ())
}

/// Parse 時の検証（`UPDATE`）。結果は捨てる。判定は [`rewrite_view_update`] と同じ関数を通す。
pub(crate) fn check_view_update(
    lookup: &impl TableLookup,
    stmt: &ValidatedViewUpdate,
) -> Result<(), SqlSurfaceError> {
    rewrite_view_update(lookup, stmt).map(|_| ())
}

/// `stmt` を現在のカタログ `lookup` で基表への DELETE へ書き換える。
pub(crate) fn rewrite_view_delete(
    lookup: &impl TableLookup,
    stmt: &ValidatedViewDelete,
) -> Result<RewrittenViewDelete, SqlSurfaceError> {
    let rv = resolve_for_write(lookup, stmt.view_name(), ViewWriteOp::Delete)?;
    let mapper = rv.mapper();
    match &stmt.shape {
        DeleteStatement::SingleRow(v) => {
            let (returning, public_names) = rewrite_returning(&mapper, v.returning.as_ref())?;
            let delete = match rewrite_id_form(lookup, &rv, &v.id_literal)? {
                BaseIdForm::Single(id_literal) => DeleteStatement::SingleRow(ValidatedDelete {
                    table_name: rv.base_table.clone(),
                    id_literal,
                    operation_id: v.operation_id.clone(),
                    returning,
                }),
                BaseIdForm::Predicates(where_predicates) => {
                    DeleteStatement::Predicate(ValidatedPredicateDelete {
                        table_name: rv.base_table.clone(),
                        where_predicates,
                        operation_id: v.operation_id.clone(),
                        returning,
                        partitioned: None,
                    })
                }
            };
            Ok(RewrittenViewDelete {
                delete,
                public_names,
            })
        }
        DeleteStatement::Predicate(v) => {
            if v.partitioned.is_some() {
                return Err(reject_partitioned());
            }
            let where_predicates = merge_predicates(&rv, &v.where_predicates)?;
            let (returning, public_names) = rewrite_returning(&mapper, v.returning.as_ref())?;
            Ok(RewrittenViewDelete {
                delete: DeleteStatement::Predicate(ValidatedPredicateDelete {
                    table_name: rv.base_table.clone(),
                    where_predicates,
                    operation_id: v.operation_id.clone(),
                    returning,
                    partitioned: None,
                }),
                public_names,
            })
        }
    }
}

/// `SET` の左辺（公開名→物理名。写像後に物理列が重複したら公開名を示して拒否）と右辺の式
/// （公開列の範囲を検査してから物理名へ写す）を書き換える。
fn rewrite_assignments(
    rv: &ResolvedView,
    assignments: &[(String, SetValue)],
) -> Result<Vec<(String, SetValue)>, SqlSurfaceError> {
    let mapper = rv.mapper();
    let mut out: Vec<(String, SetValue)> = Vec::with_capacity(assignments.len());
    for (public, value) in assignments {
        let physical = mapper.physical(public)?;
        if !mapper.is_identity() && out.iter().any(|(c, _)| *c == physical) {
            return Err(SqlSurfaceError::duplicate_column(public));
        }
        let value = match value {
            SetValue::Literal(l) => SetValue::Literal(l.clone()),
            // 基表の列 DEFAULT が効く（列名の写像のみ。Issue #1746・SQL-50）。
            SetValue::Default => SetValue::Default,
            SetValue::Expr(expr) => {
                if let Some(columns) = rv.view_columns.as_deref() {
                    expr_columns_within(columns, expr)?;
                }
                match rv.column_map.as_deref() {
                    Some(map) => SetValue::Expr(rewrite_set_value(
                        &ColumnMap::new(map, !rv.base_allocated),
                        expr,
                    )?),
                    None => SetValue::Expr(expr.clone()),
                }
            }
        };
        out.push((physical, value));
    }
    Ok(out)
}

/// `stmt` を現在のカタログ `lookup` で基表への UPDATE へ書き換える。
pub(crate) fn rewrite_view_update(
    lookup: &impl TableLookup,
    stmt: &ValidatedViewUpdate,
) -> Result<RewrittenViewUpdate, SqlSurfaceError> {
    let rv = resolve_for_write(lookup, stmt.view_name(), ViewWriteOp::Update)?;
    let mapper = rv.mapper();
    match &stmt.shape {
        ValidatedUpdateForm::Single(v) => {
            let assignments = rewrite_assignments(&rv, &v.assignments)?;
            let (returning, public_names) = rewrite_returning(&mapper, v.returning.as_ref())?;
            let update = match rewrite_id_form(lookup, &rv, &v.id_literal)? {
                BaseIdForm::Single(id_literal) => ValidatedUpdateForm::Single(ValidatedUpdate {
                    table_name: rv.base_table.clone(),
                    assignments,
                    id_literal,
                    operation_id: v.operation_id.clone(),
                    returning,
                }),
                BaseIdForm::Predicates(where_predicates) => {
                    ValidatedUpdateForm::Predicate(ValidatedPredicateUpdate {
                        table_name: rv.base_table.clone(),
                        assignments,
                        where_predicates,
                        operation_id: v.operation_id.clone(),
                        returning,
                        partitioned: None,
                    })
                }
            };
            Ok(RewrittenViewUpdate {
                update,
                public_names,
                check_predicates: rv.check_predicates.clone(),
            })
        }
        ValidatedUpdateForm::Predicate(v) => {
            if v.partitioned.is_some() {
                return Err(reject_partitioned());
            }
            let where_predicates = merge_predicates(&rv, &v.where_predicates)?;
            let assignments = rewrite_assignments(&rv, &v.assignments)?;
            let (returning, public_names) = rewrite_returning(&mapper, v.returning.as_ref())?;
            Ok(RewrittenViewUpdate {
                update: ValidatedUpdateForm::Predicate(ValidatedPredicateUpdate {
                    table_name: rv.base_table.clone(),
                    assignments,
                    where_predicates,
                    operation_id: v.operation_id.clone(),
                    returning,
                    partitioned: None,
                }),
                public_names,
                check_predicates: rv.check_predicates.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ViewDef;
    use crate::recovery::required_op_id::LedgerMode;
    use crate::sql::allowlist::{
        parse_delete_statement_shape_tokens, parse_update_form_shape_tokens, ViewNotUpdatableReason,
    };

    /// 固定のビュー定義群を返すテスト用 lookup（表は `docs`・`other`）。
    #[derive(Clone)]
    struct MapLookup(Vec<(&'static str, ViewDef)>);

    impl TableLookup for MapLookup {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(name == "docs" || name == "other")
        }

        fn view_definition(&self, name: &str) -> Result<Option<ViewDef>, SqlSurfaceError> {
            Ok(self
                .0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, d)| d.clone()))
        }
    }

    fn def(base: &str, body: &str, aliases: &[&str]) -> ViewDef {
        ViewDef {
            check_option: crate::catalog::ViewCheckOption::None,
            base_relation: base.to_string(),
            body_sql: body.to_string(),
            column_aliases: aliases.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn mode() -> LedgerMode {
        LedgerMode::CompareOnlyWithoutLedger
    }

    fn view_delete(sql: &str) -> ValidatedViewDelete {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        ValidatedViewDelete::new(
            parse_delete_statement_shape_tokens(&tokens, mode()).expect("parse"),
        )
    }

    fn view_update(sql: &str) -> ValidatedViewUpdate {
        let tokens = crate::sql::lexer::tokenize_marked(sql).expect("tokenize");
        ValidatedViewUpdate::new(parse_update_form_shape_tokens(&tokens, mode()).expect("parse"))
    }

    fn base_delete(sql: &str) -> DeleteStatement {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        parse_delete_statement_shape_tokens(&tokens, mode()).expect("parse base")
    }

    fn base_update(sql: &str) -> ValidatedUpdateForm {
        let tokens = crate::sql::lexer::tokenize_marked(sql).expect("tokenize");
        parse_update_form_shape_tokens(&tokens, mode()).expect("parse base")
    }

    fn ja_view() -> Vec<(&'static str, ViewDef)> {
        vec![(
            "sv",
            def("docs", "SELECT * FROM docs WHERE lang = 'ja'", &[]),
        )]
    }

    /// ビューの述語は利用者の述語の前に AND 合成され、基表名へ書き換わる。
    #[test]
    fn delete_merges_view_predicate_before_user_predicate() {
        let r = rewrite_view_delete(
            &MapLookup(ja_view()),
            &view_delete("DELETE FROM sv WHERE body LIKE 'b%'"),
        )
        .expect("rewrite");
        assert_eq!(
            r.delete,
            base_delete("DELETE FROM docs WHERE lang = 'ja' AND body LIKE 'b%'")
        );
        assert_eq!(r.public_names, None);
    }

    #[test]
    fn update_merges_view_predicate_and_maps_to_base_table() {
        let r = rewrite_view_update(
            &MapLookup(ja_view()),
            &view_update("UPDATE sv SET body = 'y' WHERE body LIKE 'b%'"),
        )
        .expect("rewrite");
        assert_eq!(
            r.update,
            base_update("UPDATE docs SET body = 'y' WHERE lang = 'ja' AND body LIKE 'b%'")
        );
    }

    /// ビュー述語が無いときの単一行形は単一行形のまま基表名へ写る。
    #[test]
    fn single_row_form_without_view_predicate_stays_single_row() {
        let views = vec![("v", def("docs", "SELECT * FROM docs", &[]))];
        let d = rewrite_view_delete(
            &MapLookup(views.clone()),
            &view_delete("DELETE FROM v WHERE id = 7"),
        )
        .expect("delete");
        assert_eq!(d.delete, base_delete("DELETE FROM docs WHERE id = 7"));
        let u = rewrite_view_update(
            &MapLookup(views),
            &view_update("UPDATE v SET body = 'x' WHERE id = 7"),
        )
        .expect("update");
        assert_eq!(
            u.update,
            base_update("UPDATE docs SET body = 'x' WHERE id = 7")
        );
    }

    /// ビュー述語があるときの単一行形は、利用者が `WHERE <述語> AND id = <n>` と書いた形と同じ AST の
    /// 述語形へ変換される。
    #[test]
    fn single_row_form_with_view_predicate_becomes_predicate_form() {
        let d = rewrite_view_delete(
            &MapLookup(ja_view()),
            &view_delete("DELETE FROM sv WHERE id = 7"),
        )
        .expect("delete");
        assert_eq!(
            d.delete,
            base_delete("DELETE FROM docs WHERE lang = 'ja' AND id = 7")
        );
        let u = rewrite_view_update(
            &MapLookup(ja_view()),
            &view_update("UPDATE sv SET body = 'x' WHERE id = 7"),
        )
        .expect("update");
        assert_eq!(
            u.update,
            base_update("UPDATE docs SET body = 'x' WHERE lang = 'ja' AND id = 7")
        );
    }

    /// 列を絞ったビューの非公開列は WHERE・SET 左辺・SET 式・RETURNING のどこでも `42703`。
    #[test]
    fn hidden_columns_are_rejected_in_every_position() {
        let views = vec![("v", def("docs", "SELECT id , body FROM docs", &[]))];
        let l = MapLookup(views);
        for sql in [
            "UPDATE v SET body = 'x' WHERE lang = 'ja'",
            "UPDATE v SET lang = 'x' WHERE body = 'a'",
            "UPDATE v SET body = lang || 'x' WHERE body = 'a'",
            "UPDATE v SET body = 'x' WHERE body = 'a' RETURNING lang",
        ] {
            let err = rewrite_view_update(&l, &view_update(sql)).expect_err(sql);
            assert!(
                matches!(err, SqlSurfaceError::UndefinedColumn { .. }),
                "{sql}"
            );
        }
        for sql in [
            "DELETE FROM v WHERE lang = 'ja'",
            "DELETE FROM v WHERE body = 'a' RETURNING lang",
        ] {
            let err = rewrite_view_delete(&l, &view_delete(sql)).expect_err(sql);
            assert!(
                matches!(err, SqlSurfaceError::UndefinedColumn { .. }),
                "{sql}"
            );
        }
    }

    /// `id` を公開しないビューでの単一行形は `42703`。
    #[test]
    fn single_row_form_requires_exposed_id() {
        let views = vec![("v", def("docs", "SELECT body FROM docs", &[]))];
        let err = rewrite_view_delete(
            &MapLookup(views),
            &view_delete("DELETE FROM v WHERE id = 1"),
        )
        .expect_err("id not exposed");
        assert!(matches!(err, SqlSurfaceError::UndefinedColumn { .. }));
    }

    /// 列別名ビューは公開名で書き、SET 左辺・SET 式・WHERE・RETURNING が物理名へ写る。別名で `id` へ
    /// 写る公開名は `0A000`。
    #[test]
    fn aliased_view_maps_names_and_rejects_aliased_id() {
        let views = vec![(
            "v",
            def("docs", "SELECT id , title FROM docs", &["doc_id", "name"]),
        )];
        let l = MapLookup(views);
        let r = rewrite_view_update(
            &l,
            &view_update("UPDATE v SET name = name || 'x' WHERE name = 'a' RETURNING name"),
        )
        .expect("rewrite");
        assert_eq!(
            r.update,
            base_update("UPDATE docs SET title = title || 'x' WHERE title = 'a' RETURNING title")
        );
        assert_eq!(r.public_names, Some(vec!["name".to_string()]));

        for sql in [
            "UPDATE v SET name = 'x' WHERE doc_id > 1",
            "UPDATE v SET doc_id = 1 WHERE name = 'a'",
        ] {
            let err = rewrite_view_update(&l, &view_update(sql)).expect_err(sql);
            assert!(
                matches!(err, SqlSurfaceError::FeatureNotSupported { .. }),
                "{sql}"
            );
        }
        let err = rewrite_view_delete(&l, &view_delete("DELETE FROM v WHERE doc_id > 1"))
            .expect_err("aliased id");
        assert!(matches!(err, SqlSurfaceError::FeatureNotSupported { .. }));
    }

    /// 利用者列を公開名 `id` で公開するビューの単一行形（疑似列 `id` との取り違え）は `0A000`。
    #[test]
    fn user_column_exposed_as_id_rejects_single_row_form() {
        let views = vec![(
            "v",
            def("docs", "SELECT title , body FROM docs", &["id", "b"]),
        )];
        let err = rewrite_view_delete(
            &MapLookup(views),
            &view_delete("DELETE FROM v WHERE id = 1"),
        )
        .expect_err("exposed as id");
        assert!(matches!(err, SqlSurfaceError::FeatureNotSupported { .. }));
    }

    /// 2 つの公開名が同じ物理列へ写る SET は公開名を示して拒否する。
    #[test]
    fn duplicate_physical_set_column_is_rejected_by_public_name() {
        let views = vec![(
            "v",
            def("docs", "SELECT title , title FROM docs", &["a", "b"]),
        )];
        let err = rewrite_view_update(
            &MapLookup(views),
            &view_update("UPDATE v SET a = 'x', b = 'y' WHERE a = 'z'"),
        )
        .expect_err("dup");
        match err {
            SqlSurfaceError::DuplicateColumn { name } => assert_eq!(name, "b"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `PARTITIONED` はビュー経由では `0A000`。
    #[test]
    fn partitioned_is_rejected_on_views() {
        let l = MapLookup(ja_view());
        let err = rewrite_view_delete(
            &l,
            &view_delete(
                "DELETE FROM sv WHERE body = 'a' USING OPERATION_ID 'p1' PARTITIONED CHUNK 2",
            ),
        )
        .expect_err("partitioned delete");
        assert!(matches!(err, SqlSurfaceError::FeatureNotSupported { .. }));
        let err = rewrite_view_update(
            &l,
            &view_update(
                "UPDATE sv SET body = 'x' WHERE body = 'a' USING OPERATION_ID 'p2' PARTITIONED CHUNK 2",
            ),
        )
        .expect_err("partitioned update");
        assert!(matches!(err, SqlSurfaceError::FeatureNotSupported { .. }));
    }

    /// 自動更新不可のビューは `55000`（op 付き）。連鎖のビュー述語は内側から順にすべて合成される。
    #[test]
    fn non_updatable_is_55000_and_chain_predicates_are_all_merged() {
        let l = MapLookup(vec![
            (
                "v1",
                def("docs", "SELECT * FROM docs WHERE lang = 'ja'", &[]),
            ),
            (
                "v2",
                def("v1", "SELECT * FROM v1 WHERE body LIKE 'b%'", &[]),
            ),
            ("agg", def("docs", "SELECT DISTINCT body FROM docs", &[])),
        ]);
        let r =
            rewrite_view_delete(&l, &view_delete("DELETE FROM v2 WHERE id > 1")).expect("chain");
        assert_eq!(
            r.delete,
            base_delete("DELETE FROM docs WHERE lang = 'ja' AND body LIKE 'b%' AND id > 1")
        );
        let err = rewrite_view_update(
            &l,
            &view_update("UPDATE agg SET body = 'x' WHERE body = 'a'"),
        )
        .expect_err("not updatable");
        assert!(matches!(
            err,
            SqlSurfaceError::ViewNotAutoUpdatable {
                op: ViewWriteOp::Update,
                reason: ViewNotUpdatableReason::Distinct
            }
        ));
    }
}
