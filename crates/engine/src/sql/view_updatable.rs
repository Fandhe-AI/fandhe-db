//! ビューの自動更新可能性の判定（TABLE-43 (3)・Issue #1670）。
//!
//! 責務境界: 「ビューへの書き込みを基表へ書き換えてよい形か」を決定的に返すだけで、書き換え
//! 自体は行わない。呼び出し元は `core.rs` の書き込み経路（`INSERT`／`UPDATE`／`DELETE` の
//! 解析で `UndefinedTable` になった名前がビューだったとき。`EngineCore::
//! reclassify_write_to_view_error_for`）で、結果が [`ViewUpdatability::NotAutoUpdatable`] なら
//! `55000`（[`SqlSurfaceError::ViewNotAutoUpdatable`]）で拒否する。後続の書き換え実装
//! （INSERT: Issue #1671〔`sql::view_insert`で結線済み〕、UPDATE／DELETE: Issue #1672〔`sql::view_dml`で結線済み〕）と `WITH CHECK OPTION` の宣言検査
//! （Issue #1673）も、この判定を前提にする。
//!
//! 判定は第 2 の分類器を作らず、読み取り経路と同じ [`resolve_from`] の結果に乗る。
//! [`Resolved::View`]（連鎖の全段が単純形。単一 FROM・集計／DISTINCT／ウィンドウ／LIMIT／CTE／
//! 集合演算／サブクエリ／式項目を持たない）が自動更新可能、[`Resolved::Buffered`]
//! （評価後射影形）が自動更新不可に対応する。不可の理由は格納本文のトークン列から分類し、
//! 複数当てはまる場合の優先順位は固定する（決定性のため）。
//!
//! fail-closed: 連鎖の解決失敗（破損 `XX000`・深さ超過 `54000`・参照先未定義のうち名前自体が非ビューのもの `42P01`）は
//! 握りつぶさずそのまま返し（実在ビューの内側欠落は `55000` へ再分類）、「更新可能」にも「ビューではない」にも倒さない。

use super::allowlist::{SqlSurfaceError, TableLookup, ViewNotUpdatableReason};
use super::lexer::{Keyword, Token};
use super::view::{resolve_from, Resolved};

/// ビューの自動更新可能性の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewUpdatability {
    /// 連鎖の全段が単純形で、基表への書き換え対象にできる。
    AutoUpdatable,
    /// 自動更新できない（理由つき）。
    NotAutoUpdatable(ViewNotUpdatableReason),
}

/// 固定文言の `XX000`（本文のリテラルを含めない）。
fn invalid_view_definition() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "invalid view definition".to_string(),
    }
}

/// `name`（ビュー名）の自動更新可能性を返す。`name` がビューでなければ（表を解決した場合）
/// 呼び出し契約違反として固定文言の `XX000` を返す（fail-closed）。
pub(crate) fn view_updatability(
    lookup: &impl TableLookup,
    name: &str,
) -> Result<ViewUpdatability, SqlSurfaceError> {
    let resolved = match resolve_from(lookup, name) {
        Ok(r) => r,
        // 名前自体がビューとして実在するのに連鎖の内側（基表の欠落・ビュー経由の matview 基底）
        // が解決できない場合、`42P01` のまま返すと実在ビューが欠落テーブル扱いになる。
        // 書き込みは基表へ書き換えられないため自動更新不可（`55000`）へ再分類する（fail-closed）。
        // 名前自体がビューでない場合は従来どおり `42P01` を伝播する。
        Err(SqlSurfaceError::UndefinedTable { name: missing }) => {
            return match lookup.view_definition(name)? {
                Some(def) => {
                    let reason = reason_from_body(&def.body_sql)
                        .unwrap_or(ViewNotUpdatableReason::UnderlyingNotUpdatable);
                    Ok(ViewUpdatability::NotAutoUpdatable(reason))
                }
                None => Err(SqlSurfaceError::UndefinedTable { name: missing }),
            };
        }
        Err(e) => return Err(e),
    };
    match resolved {
        Resolved::View { .. } => Ok(ViewUpdatability::AutoUpdatable),
        Resolved::Buffered { .. } => {
            // 最も外側のビュー定義が無い（= matview を直接指した）場合は下位が更新不可。
            let reason = match lookup.view_definition(name)? {
                Some(def) => reason_from_body(&def.body_sql)?,
                None => ViewNotUpdatableReason::UnderlyingNotUpdatable,
            };
            Ok(ViewUpdatability::NotAutoUpdatable(reason))
        }
        Resolved::Table => Err(invalid_view_definition()),
    }
}

/// 格納本文（最も外側のビュー）から不可の理由を分類する。深さ 0（括弧の外）のキーワードだけを
/// 見るため、サブクエリ内の語には影響されない。どの規則にも当たらない本文（単純形が評価後
/// 射影形を包む連鎖）は [`ViewNotUpdatableReason::UnderlyingNotUpdatable`]。
/// 優先順位: DISTINCT → GROUP BY → HAVING → 集合演算 → WITH → LIMIT／OFFSET → 集計 →
/// ウィンドウ → JOIN → 下位。
pub(crate) fn reason_from_body(body_sql: &str) -> Result<ViewNotUpdatableReason, SqlSurfaceError> {
    let tokens =
        super::lexer::tokenize_stored_view_body(body_sql).map_err(|_| invalid_view_definition())?;
    let mut f = Flags::default();
    let mut depth: usize = 0;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Keyword(Keyword::Limit) if depth == 0 => f.limit = true,
            Token::Ident(w) if depth == 0 => {
                let next = tokens.get(i.saturating_add(1));
                let prev = i.checked_sub(1).and_then(|p| tokens.get(p));
                let is = |s: &str| w.eq_ignore_ascii_case(s);
                if i == 0 && is("WITH") {
                    f.with = true;
                } else if is("DISTINCT") && matches!(prev, Some(Token::Keyword(Keyword::Select))) {
                    f.distinct = true;
                } else if is("GROUP") && matches!(next, Some(Token::Keyword(Keyword::By))) {
                    f.group_by = true;
                } else if is("HAVING") {
                    f.having = true;
                } else if is("UNION") || is("INTERSECT") || is("EXCEPT") {
                    f.set_op = true;
                } else if is("OFFSET") {
                    f.limit = true;
                } else if is("JOIN") {
                    f.join = true;
                } else if is("OVER") && matches!(next, Some(Token::Punct('('))) {
                    f.window = true;
                } else if super::allowlist::is_aggregate_function_name(w)
                    && matches!(next, Some(Token::Punct('(')))
                {
                    f.aggregate = true;
                }
            }
            _ => {}
        }
    }
    Ok(f.reason())
}

/// 深さ 0 で観測したキーワードの有無。
#[derive(Default)]
struct Flags {
    distinct: bool,
    group_by: bool,
    having: bool,
    set_op: bool,
    with: bool,
    limit: bool,
    aggregate: bool,
    window: bool,
    join: bool,
}

impl Flags {
    /// 固定の優先順位で理由を 1 つ選ぶ。
    fn reason(&self) -> ViewNotUpdatableReason {
        use ViewNotUpdatableReason as R;
        if self.distinct {
            R::Distinct
        } else if self.group_by {
            R::GroupBy
        } else if self.having {
            R::Having
        } else if self.set_op {
            R::SetOperation
        } else if self.with {
            R::With
        } else if self.limit {
            R::LimitOffset
        } else if self.aggregate {
            R::Aggregate
        } else if self.window {
            R::Window
        } else if self.join {
            R::NotSingleRelation
        } else {
            R::UnderlyingNotUpdatable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ViewDef;
    use ViewNotUpdatableReason as R;

    /// 固定のビュー定義群を返すテスト用 lookup（表は `docs`・`other` のみ）。
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

    fn def(base: &str, body: &str) -> ViewDef {
        ViewDef {
            base_relation: base.to_string(),
            body_sql: body.to_string(),
            column_aliases: Vec::new(),
        }
    }

    fn check(views: Vec<(&'static str, ViewDef)>, name: &str) -> ViewUpdatability {
        view_updatability(&MapLookup(views), name).expect("resolvable view")
    }

    /// TABLE-43 (3): 単純形（単一 FROM・述語のみ）は自動更新可能。
    #[test]
    fn simple_views_are_auto_updatable() {
        for body in [
            "SELECT * FROM docs",
            "SELECT id , lang FROM docs WHERE lang = 'ja'",
            "SELECT * FROM docs WHERE FALSE",
        ] {
            assert_eq!(
                check(vec![("v", def("docs", body))], "v"),
                ViewUpdatability::AutoUpdatable,
                "{body}"
            );
        }
    }

    /// 列別名つきの単純形も自動更新可能（別名は `column_map` が物理名へ写す）。
    #[test]
    fn aliased_simple_view_is_auto_updatable() {
        let mut d = def("docs", "SELECT id , lang FROM docs");
        d.column_aliases = vec!["a".to_string(), "b".to_string()];
        assert_eq!(check(vec![("v", d)], "v"), ViewUpdatability::AutoUpdatable);
    }

    /// 単純形 → 単純形の連鎖は自動更新可能。
    #[test]
    fn simple_chain_is_auto_updatable() {
        let views = vec![
            ("v1", def("docs", "SELECT * FROM docs WHERE lang = 'ja'")),
            ("v2", def("v1", "SELECT id FROM v1")),
        ];
        assert_eq!(check(views, "v2"), ViewUpdatability::AutoUpdatable);
    }

    /// 評価後射影形の各形は理由つきで自動更新不可。
    #[test]
    fn buffered_shapes_are_not_auto_updatable_with_reason() {
        let cases = [
            ("SELECT * FROM docs LIMIT 5", R::LimitOffset),
            ("SELECT id FROM docs ORDER BY id LIMIT 5", R::LimitOffset),
            ("SELECT COUNT ( * ) AS n FROM docs", R::Aggregate),
            (
                "SELECT lang , COUNT ( * ) AS n FROM docs GROUP BY lang",
                R::GroupBy,
            ),
            ("SELECT DISTINCT lang FROM docs", R::Distinct),
            (
                "SELECT docs.id FROM docs JOIN other ON docs.id = other.id LIMIT 5",
                R::LimitOffset,
            ),
            (
                "( SELECT id FROM docs ) UNION ( SELECT id FROM other )",
                R::SetOperation,
            ),
            (
                "WITH x AS ( SELECT id FROM docs ) SELECT id FROM x LIMIT 5",
                R::With,
            ),
        ];
        for (body, want) in cases {
            let got = view_updatability(&MapLookup(vec![("v", def("docs", body))]), "v")
                .unwrap_or_else(|e| panic!("{body}: {e:?}"));
            assert_eq!(got, ViewUpdatability::NotAutoUpdatable(want), "{body}");
        }
    }

    /// 単純形が評価後射影形を包む連鎖は「下位が自動更新不可」。
    #[test]
    fn simple_over_buffered_is_underlying_not_updatable() {
        let views = vec![
            ("b", def("docs", "SELECT * FROM docs LIMIT 5")),
            ("v", def("b", "SELECT id FROM b")),
        ];
        assert_eq!(
            check(views, "v"),
            ViewUpdatability::NotAutoUpdatable(R::UnderlyingNotUpdatable)
        );
    }

    /// 評価後射影形 → 評価後射影形は外側自身の理由で不可。
    #[test]
    fn buffered_over_buffered_is_not_auto_updatable() {
        let views = vec![
            ("b", def("docs", "SELECT * FROM docs LIMIT 5")),
            ("v", def("b", "SELECT * FROM b LIMIT 3")),
        ];
        assert_eq!(
            check(views, "v"),
            ViewUpdatability::NotAutoUpdatable(R::LimitOffset)
        );
    }

    /// 破損した本文は固定文言の `XX000`（本文のリテラルを含めない）。「更新可能」には倒さない。
    #[test]
    fn corrupt_body_is_internal_without_literal() {
        let lookup = MapLookup(vec![(
            "v",
            def("docs", "SELECT 'secret-literal' FROM FROM"),
        )]);
        let err = view_updatability(&lookup, "v").expect_err("corrupt");
        assert!(matches!(err, SqlSurfaceError::Internal { .. }));
        assert!(!err.to_string().contains("secret-literal"));
    }

    /// 表名を渡すのは契約違反: `XX000`（fail-closed）。
    #[test]
    fn table_name_is_rejected() {
        let lookup = MapLookup(vec![]);
        assert!(matches!(
            view_updatability(&lookup, "docs"),
            Err(SqlSurfaceError::Internal { .. })
        ));
    }

    /// 存在しない名前は `42P01` のまま伝播する。
    #[test]
    fn unknown_name_propagates_undefined_table() {
        let lookup = MapLookup(vec![]);
        assert!(matches!(
            view_updatability(&lookup, "nope"),
            Err(SqlSurfaceError::UndefinedTable { .. })
        ));
    }

    /// 実在ビューの内側の基表が欠落している場合は `42P01` ではなく自動更新不可（`55000` 相当）。
    #[test]
    fn existing_view_with_missing_base_is_not_auto_updatable() {
        let lookup = MapLookup(vec![
            ("v_missing", def("gone", "SELECT * FROM gone")),
            ("v_outer", def("v_missing", "SELECT * FROM v_missing")),
        ]);
        for name in ["v_missing", "v_outer"] {
            assert!(matches!(
                view_updatability(&lookup, name),
                Ok(ViewUpdatability::NotAutoUpdatable(_))
            ));
        }
    }

    /// LIMIT のない JOIN 本文は理由分類のみ単体で確認する（実カタログでは LIMIT 必須で到達しない）。
    #[test]
    fn join_without_limit_is_not_single_relation() {
        let r = reason_from_body("SELECT docs.id FROM docs JOIN other ON docs.id = other.id")
            .expect("tokenizes");
        assert_eq!(r, R::NotSingleRelation);
    }

    /// 深さ 0 の語だけを見る: サブクエリ内の LIMIT は外側の理由に影響しない。
    #[test]
    fn nested_keywords_are_ignored_for_reason() {
        let r = reason_from_body("SELECT * FROM docs WHERE id IN ( SELECT id FROM other LIMIT 1 )")
            .expect("tokenizes");
        assert_eq!(r, R::UnderlyingNotUpdatable);
    }
}
