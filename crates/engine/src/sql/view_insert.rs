//! ビューへの `INSERT` の基表への書き換え（TABLE-43・SQL-49・RLS-10。Issue #1671）。
//!
//! 責務境界: `INSERT INTO <view> (<公開名>, ...) VALUES ... [ON CONFLICT ...] [RETURNING ...]`
//! の構造検証済みの形（[`ValidatedViewInsert`]）を、基表への [`ValidatedInsert`] へ名前の写像だけで
//! 書き換える。SQL 文字列は組み立てず、書き換え後の束縛・実行・制約・RLS・台帳は既存の基表 INSERT
//! 経路（`core.rs` の `execute_insert_form` 等）にそのまま乗る（第 2 の書き込み経路を作らない）。
//! 呼び出し元は `core.rs`: Parse 時は [`check_view_insert`]（判定だけ）、Execute／Describe 時は
//! [`rewrite_view_insert`]（現在のカタログで書き換え）を呼ぶ。Parse 時の写像は持ち越さない。
//!
//! 判定は第 2 の判定器を作らず `sql::view_updatable`（自動更新可能性）と `sql::view::resolve_from`
//! （連鎖の畳み込み・公開列集合・列別名の対応）に乗る。
//!
//! 順序の不変条件（`sql::view_alias` と同じ）: ビューの公開列集合の検査を**公開名に対して先に**行い、
//! そのあとで物理名へ写す。公開されていない基表の列への書き込みは `42703` で拒否する（ビューの
//! 列スコープ契約）。別名で疑似列 `id` へ写る公開名は、どの位置でも `0A000`（fail-closed）。
//!
//! 変えないもの: ビューの `WHERE` 述語は `INSERT` に適用しない（ビュー条件の外の行も挿入される。
//! 検査は `WITH CHECK OPTION`〔Issue #1673〕の担当）。ビューに現れない基表の列は、基表 `INSERT` で
//! 列を省略したときと同じく基表の `DEFAULT`（無ければ NULL）になる。
//!
//! RLS-10 (b): 名前の写像だけで行の可視性には関与しない。挿入行の所有テナントは書き込み
//! セッションの `PolicyContext` が決め、ビュー作成者の可視性は引き継がれない。

use super::allowlist::{
    ConflictTargetSpec, OnConflictAction, OnConflictClause, Projection, SqlSurfaceError,
    TableLookup, UpsertAddOperand, UpsertValue, ValidatedInsert, ValidatedViewInsert, ViewWriteOp,
    WherePredicate,
};
use super::exec::ColumnMeta;
use super::view::{resolve_from, Resolved};
use super::view_updatable::{view_updatability, ViewUpdatability};

/// 書き換え結果。
#[derive(Debug)]
pub(crate) struct RewrittenViewInsert {
    /// 基表形の INSERT（`table_name` は基表）。
    pub(crate) insert: ValidatedInsert,
    /// `RETURNING` の出力列名（ビューの公開名。`returning` の各項目と同じ並び）。基表の列名と
    /// 同じ（ビューが何も改名・絞り込みしない）ときは `None`。
    pub(crate) public_names: Option<Vec<String>>,
    /// `WITH CHECK OPTION` の検査対象の述語（TABLE-43 (2)・Issue #1673。内側のビューが先頭。
    /// 基表の物理列名）。`core::run_view_write` が書き込み区間のスコープへ張り、検査点
    /// （`constraint::enforce_row_constraints_in_txn`）が書いた行で評価する。
    pub(crate) check_predicates: Vec<Vec<WherePredicate>>,
}

/// Parse 時の検証（自動更新可能か・解決できるか・書き込む列が公開されているか）。結果は捨てる。
/// 判定は [`rewrite_view_insert`] と同じ関数を通す（第 2 の判定器を作らない）。
pub(crate) fn check_view_insert(
    lookup: &impl TableLookup,
    stmt: &ValidatedViewInsert,
) -> Result<(), SqlSurfaceError> {
    rewrite_view_insert(lookup, stmt).map(|_| ())
}

/// `stmt` を現在のカタログ `lookup` で基表への INSERT へ書き換える。自動更新不可は `55000`
/// （[`SqlSurfaceError::ViewNotAutoUpdatable`]）、連鎖の解決失敗はそのまま返す（「更新可能」に倒さない）。
pub(crate) fn rewrite_view_insert(
    lookup: &impl TableLookup,
    stmt: &ValidatedViewInsert,
) -> Result<RewrittenViewInsert, SqlSurfaceError> {
    let shape = &stmt.shape;
    // 列リストを省略したビュー INSERT（`INSERT INTO v VALUES (...)`。SQL-50・Issue #1730）は、
    // ビューの公開列での展開が要るため未対応。基表の `*` へ黙って展開する fail-open を作らず、
    // 展開前に固定の `0A000` で拒否する（`DEFAULT VALUES`・値位置の `DEFAULT` は下の名前写像を通る）。
    if shape.columns_omitted {
        return Err(SqlSurfaceError::feature_not_supported(
            "INSERT into a view without a column list is not supported",
        ));
    }
    match view_updatability(lookup, &shape.table_name)? {
        ViewUpdatability::AutoUpdatable => {}
        ViewUpdatability::NotAutoUpdatable(reason) => {
            return Err(SqlSurfaceError::ViewNotAutoUpdatable {
                op: ViewWriteOp::Insert,
                reason,
            });
        }
    }
    let (base_table, view_columns, column_map, check_predicates, base_allocated) =
        match resolve_from(lookup, &shape.table_name)? {
            Resolved::View {
                base_table,
                view_columns,
                column_map,
                check_predicates,
                base_allocated,
                ..
            } => (
                base_table,
                view_columns,
                column_map,
                check_predicates,
                base_allocated,
            ),
            // 自動更新可能と判定済みなのに `View` 以外になるのは不変条件違反（固定文言。fail-closed）。
            _ => {
                return Err(SqlSurfaceError::Internal {
                    detail: "invalid view definition".to_string(),
                })
            }
        };
    let mapper = Mapper {
        columns: view_columns.as_deref(),
        map: column_map.as_deref(),
        base_allocated,
    };

    // 列リスト。写像後に物理列が重複したら、公開名を示して拒否する（物理名は出さない）。
    let columns = if mapper.is_identity() {
        shape.columns.clone()
    } else {
        let mut out: Vec<String> = Vec::with_capacity(shape.columns.len());
        for public in &shape.columns {
            let physical = mapper.physical(public)?;
            if out.contains(&physical) {
                return Err(SqlSurfaceError::duplicate_column(public));
            }
            out.push(physical);
        }
        out
    };

    let on_conflict = match &shape.on_conflict {
        None => None,
        Some(clause) => Some(rewrite_on_conflict(&mapper, clause)?),
    };

    let (returning, public_names) = rewrite_returning(&mapper, shape.returning.as_ref())?;

    Ok(RewrittenViewInsert {
        insert: ValidatedInsert {
            table_name: base_table,
            columns,
            columns_omitted: false,
            rows: shape.rows.clone(),
            operation_id: shape.operation_id.clone(),
            returning,
            on_conflict,
        },
        public_names,
        check_predicates,
    })
}

/// ビューへの INSERT の列 `public`（公開名）が書き込む基表名と物理列名（Parse 時のパラメータ型推論用）。
/// 自動更新不可・解決失敗・公開されていない列・疑似列 `id` への写像は `None`（呼び出し元は型を
/// 決めず従来どおり `Text` へ倒す。公告だけの問題で値の検証は Execute 時の書き換えが担う）。
pub(crate) fn resolve_insert_column(
    lookup: &impl TableLookup,
    view_name: &str,
    public: &str,
) -> Option<(String, String)> {
    match view_updatability(lookup, view_name).ok()? {
        ViewUpdatability::AutoUpdatable => {}
        ViewUpdatability::NotAutoUpdatable(_) => return None,
    }
    match resolve_from(lookup, view_name).ok()? {
        Resolved::View {
            base_table,
            view_columns,
            column_map,
            base_allocated,
            ..
        } => {
            let mapper = Mapper {
                columns: view_columns.as_deref(),
                map: column_map.as_deref(),
                base_allocated,
            };
            let physical = mapper.physical(public).ok()?;
            Some((base_table, physical))
        }
        _ => None,
    }
}

/// `ON CONFLICT` の対象列・`DO UPDATE SET` の左辺・`EXCLUDED.<col>` を物理名へ写す。
fn rewrite_on_conflict(
    mapper: &Mapper<'_>,
    clause: &OnConflictClause,
) -> Result<OnConflictClause, SqlSurfaceError> {
    let target = match &clause.target {
        ConflictTargetSpec::Columns(columns) => ConflictTargetSpec::Columns(
            columns
                .iter()
                .map(|c| mapper.physical(c))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        // ビュー自身は制約を持たないため、制約名は対象リレーション上に存在しない
        // （SQL-49、Issue #1737。受理せず fail-closed の `42704`）。
        // 対象省略（SQL-49、Issue #1736）は列名を持たないためそのまま写す。
        ConflictTargetSpec::Omitted => ConflictTargetSpec::Omitted,
        ConflictTargetSpec::Constraint(name) => {
            return Err(SqlSurfaceError::UndefinedObject { name: name.clone() });
        }
    };
    let action = match &clause.action {
        OnConflictAction::DoNothing => OnConflictAction::DoNothing,
        OnConflictAction::DoUpdate {
            assignments,
            condition,
        } => {
            // 条件内の列（公開名）を物理名へ写す処理は未対応のため fail-closed で拒否する
            // （SQL-49 (3)、Issue #1740。`check_constraint::map_predicate_columns` が流用できる
            // 見込みだが、`EXCLUDED.` 接頭辞付き名の扱いを含めて将来の課題とする）。
            if !condition.is_empty() {
                return Err(SqlSurfaceError::feature_not_supported(
                    "ON CONFLICT DO UPDATE WHERE is not supported through a view",
                ));
            }
            let mut out = Vec::with_capacity(assignments.len());
            for (col, value) in assignments {
                let value = match value {
                    UpsertValue::Excluded(src) => UpsertValue::Excluded(mapper.physical(src)?),
                    UpsertValue::Literal(l) => UpsertValue::Literal(l.clone()),
                    UpsertValue::Add { column, operand } => UpsertValue::Add {
                        column: mapper.physical(column)?,
                        operand: match operand {
                            UpsertAddOperand::Excluded(src) => {
                                UpsertAddOperand::Excluded(mapper.physical(src)?)
                            }
                            UpsertAddOperand::Literal(l) => UpsertAddOperand::Literal(l.clone()),
                        },
                    },
                };
                out.push((mapper.physical(col)?, value));
            }
            OnConflictAction::DoUpdate {
                assignments: out,
                condition: Vec::new(),
            }
        }
    };
    Ok(OnConflictClause { target, action })
}

/// `RETURNING` をビューの公開列で解決して物理名の投影へ写す。戻り値の第 2 要素は出力列名
/// （公開名。写像が不要なら `None`）。`RETURNING *` は、ビューが列を絞り込む・改名するときだけ
/// 公開列リストへ展開する（それ以外は基表の `*` と同じ列になるため `All` のまま）。
pub(crate) fn rewrite_returning(
    mapper: &Mapper<'_>,
    returning: Option<&Projection>,
) -> Result<(Option<Projection>, Option<Vec<String>>), SqlSurfaceError> {
    match returning {
        None => Ok((None, None)),
        Some(Projection::Items(_)) => Err(SqlSurfaceError::unsupported(
            "RETURNING does not support function-call items",
        )),
        Some(Projection::All) => match mapper.columns {
            None => Ok((Some(Projection::All), None)),
            Some(public) => {
                let physical = public
                    .iter()
                    .map(|c| mapper.physical(c))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((Some(Projection::Columns(physical)), Some(public.to_vec())))
            }
        },
        Some(Projection::Columns(cols)) => {
            if mapper.is_identity() {
                return Ok((Some(Projection::Columns(cols.clone())), None));
            }
            let physical = cols
                .iter()
                .map(|c| mapper.physical(c))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((Some(Projection::Columns(physical)), Some(cols.clone())))
        }
    }
}

/// 結果列メタの列名をビューの公開名へ上書きする（`RETURNING` の `RowDescription` 用。位置対応）。
/// `public_names` が `None`、または並びが合わない（防御）ときは何もしない。
pub(crate) fn apply_public_names(columns: &mut [ColumnMeta], public_names: Option<&[String]>) {
    let Some(names) = public_names else {
        return;
    };
    if names.len() != columns.len() {
        return;
    }
    for (meta, public) in columns.iter_mut().zip(names) {
        match meta {
            ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. }
            | ColumnMeta::AliasedId { name } => *name = public.clone(),
            ColumnMeta::Id => {}
        }
    }
}

/// ビューの公開名 → 基表の物理名の写像（`Resolved::View` の `view_columns`／`column_map` の借用）。
pub(crate) struct Mapper<'a> {
    /// ビューが公開する列集合。`None` は連鎖のどの段も列を絞り込んでいない（基表の全列）。
    pub(crate) columns: Option<&'a [String]>,
    /// 列別名の対応。`None` は公開名＝物理名。
    pub(crate) map: Option<&'a [(String, String)]>,
    /// 基表が採番方式（疑似列 `id` を持たない。Issue #2199）か。真のとき `id` へ写る別名も通常列。
    pub(crate) base_allocated: bool,
}

impl Mapper<'_> {
    /// 公開名と物理名が常に一致し、列集合も絞られていない（基表の束縛段が列検査を担う）。
    pub(crate) fn is_identity(&self) -> bool {
        self.columns.is_none() && self.map.is_none()
    }

    /// 公開名 `public` の物理名。公開されていない列は `42703`、別名で疑似列 `id` へ写る公開名は
    /// `0A000`（行 identity を利用者列として書く取り違えを防ぐ。fail-closed）。
    pub(crate) fn physical(&self, public: &str) -> Result<String, SqlSurfaceError> {
        if let Some(columns) = self.columns {
            if !columns.iter().any(|c| c == public) {
                return Err(SqlSurfaceError::undefined_column(public));
            }
        }
        let physical = match self.map {
            Some(map) => map
                .iter()
                .find(|(p, _)| p == public)
                .map(|(_, phys)| phys.clone())
                .unwrap_or_else(|| public.to_string()),
            None => public.to_string(),
        };
        if !self.base_allocated && physical == "id" && public != "id" {
            return Err(SqlSurfaceError::feature_not_supported(
                "writing to a view column aliased to the row id is not supported",
            ));
        }
        Ok(physical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ViewDef;
    use crate::recovery::required_op_id::LedgerMode;
    use crate::sql::allowlist::{parse_insert_shape_tokens_with_limit, ViewNotUpdatableReason};

    /// 固定のビュー定義群を返すテスト用 lookup（表は `docs`・`other`）。
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

    fn view_insert(sql: &str) -> ValidatedViewInsert {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        let shape = parse_insert_shape_tokens_with_limit(
            &tokens,
            LedgerMode::CompareOnlyWithoutLedger,
            None,
        )
        .expect("parse insert");
        ValidatedViewInsert::new(shape)
    }

    fn rewrite(
        views: Vec<(&'static str, ViewDef)>,
        sql: &str,
    ) -> Result<RewrittenViewInsert, SqlSurfaceError> {
        rewrite_view_insert(&MapLookup(views), &view_insert(sql))
    }

    /// `SELECT *` の単純形ビューは列名をそのまま基表へ通す（基表の束縛段が列を検査する）。
    #[test]
    fn star_view_passes_through_to_base_table() {
        let r = rewrite(
            vec![(
                "v",
                def("docs", "SELECT * FROM docs WHERE lang = 'ja'", &[]),
            )],
            "INSERT INTO v (title) VALUES ('a') RETURNING *",
        )
        .expect("rewrite");
        assert_eq!(r.insert.table_name, "docs");
        assert_eq!(r.insert.columns, vec!["title".to_string()]);
        assert_eq!(r.insert.returning, Some(Projection::All));
        assert_eq!(r.public_names, None);
    }

    /// 列を絞ったビューでは、非公開列への書き込みは `42703`。
    #[test]
    fn hidden_column_is_rejected_with_undefined_column() {
        let views = vec![("v", def("docs", "SELECT id , title FROM docs", &[]))];
        let err = rewrite(
            views.clone(),
            "INSERT INTO v (title, secret) VALUES ('a', 'b')",
        )
        .expect_err("hidden column");
        assert!(matches!(err, SqlSurfaceError::UndefinedColumn { .. }));
        let ok = rewrite(views, "INSERT INTO v (title) VALUES ('a')").expect("visible column");
        assert_eq!(ok.insert.columns, vec!["title".to_string()]);
    }

    /// 絞り込みビューの `RETURNING *` は公開列へ展開する。
    #[test]
    fn returning_star_expands_to_public_columns() {
        let r = rewrite(
            vec![("v", def("docs", "SELECT id , title FROM docs", &[]))],
            "INSERT INTO v (title) VALUES ('a') RETURNING *",
        )
        .expect("rewrite");
        assert_eq!(
            r.insert.returning,
            Some(Projection::Columns(vec![
                "id".to_string(),
                "title".to_string()
            ]))
        );
        assert_eq!(
            r.public_names,
            Some(vec!["id".to_string(), "title".to_string()])
        );
    }

    /// `DO UPDATE ... WHERE`（SQL-49 (3)、Issue #1740）のビュー経由は未対応で、`0A000` で拒否する。
    #[test]
    fn do_update_where_through_a_view_is_not_supported() {
        let err = rewrite(
            vec![("v", def("docs", "SELECT id , title FROM docs", &[]))],
            "INSERT INTO v (title) VALUES ('a') ON CONFLICT (title) DO UPDATE SET \
             title = EXCLUDED.title WHERE title = 'a'",
        )
        .expect_err("where through a view");
        assert!(matches!(err, SqlSurfaceError::FeatureNotSupported { .. }));
    }

    /// 列別名つきビューは公開名で書き、列リスト・ON CONFLICT・`EXCLUDED`・`RETURNING` が物理名へ写る。
    #[test]
    fn aliased_view_maps_every_position_to_physical_names() {
        let views = vec![(
            "v",
            def("docs", "SELECT id , title FROM docs", &["doc_id", "name"]),
        )];
        // 別名で疑似列 `id` へ写る公開名（`doc_id`）はどの位置でも `0A000`。
        let err =
            rewrite(views.clone(), "INSERT INTO v (doc_id) VALUES (1)").expect_err("alias id");
        assert!(matches!(err, SqlSurfaceError::FeatureNotSupported { .. }));

        let r = rewrite(
            views.clone(),
            "INSERT INTO v (name) VALUES ('a') ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING name",
        )
        .expect("rewrite");
        assert_eq!(r.insert.columns, vec!["title".to_string()]);
        let oc = r.insert.on_conflict.expect("on conflict");
        assert_eq!(
            oc.target,
            ConflictTargetSpec::Columns(vec!["title".to_string()])
        );
        assert_eq!(
            oc.action,
            OnConflictAction::DoUpdate {
                assignments: vec![(
                    "title".to_string(),
                    UpsertValue::Excluded("title".to_string())
                )],
                condition: Vec::new(),
            }
        );
        assert_eq!(
            r.insert.returning,
            Some(Projection::Columns(vec!["title".to_string()]))
        );
        assert_eq!(r.public_names, Some(vec!["name".to_string()]));

        // 物理名（`title`）は公開されていない。
        let err = rewrite(views, "INSERT INTO v (title) VALUES ('a')").expect_err("physical name");
        assert!(matches!(err, SqlSurfaceError::UndefinedColumn { .. }));
    }

    /// 2 つの公開名が同じ物理列へ写る場合は、公開名を示して重複を拒否する。
    #[test]
    fn duplicate_physical_column_is_rejected_by_public_name() {
        let views = vec![(
            "v",
            def("docs", "SELECT title , title FROM docs", &["a", "b"]),
        )];
        let err = rewrite(views, "INSERT INTO v (a, b) VALUES ('x', 'y')").expect_err("dup");
        match err {
            SqlSurfaceError::DuplicateColumn { name } => assert_eq!(name, "b"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 連鎖（v2 → v1 → docs）への INSERT は基表へ書き換える。
    #[test]
    fn chain_rewrites_to_base_table() {
        let views = vec![
            (
                "v1",
                def("docs", "SELECT * FROM docs WHERE lang = 'ja'", &[]),
            ),
            ("v2", def("v1", "SELECT id , title FROM v1", &[])),
        ];
        let r = rewrite(views, "INSERT INTO v2 (title) VALUES ('a')").expect("rewrite");
        assert_eq!(r.insert.table_name, "docs");
    }

    /// 自動更新不可のビューは `55000`。
    #[test]
    fn non_auto_updatable_view_is_rejected() {
        let err = rewrite(
            vec![("v", def("docs", "SELECT * FROM docs LIMIT 5", &[]))],
            "INSERT INTO v (title) VALUES ('a')",
        )
        .expect_err("not updatable");
        match err {
            SqlSurfaceError::ViewNotAutoUpdatable { op, reason } => {
                assert_eq!(op, ViewWriteOp::Insert);
                assert_eq!(reason, ViewNotUpdatableReason::LimitOffset);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `RETURNING` の列メタは位置対応で公開名へ上書きされる。
    #[test]
    fn apply_public_names_renames_scalar_columns() {
        let mut cols = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "title".to_string(),
                ty: crate::catalog::ColumnType::Text,
            },
        ];
        apply_public_names(&mut cols, Some(&["id".to_string(), "name".to_string()]));
        assert!(matches!(&cols[1], ColumnMeta::Scalar { name, .. } if name == "name"));
        // 並びが合わない場合は何も変えない。
        apply_public_names(&mut cols, Some(&["x".to_string()]));
        assert!(matches!(&cols[1], ColumnMeta::Scalar { name, .. } if name == "name"));
    }
}
