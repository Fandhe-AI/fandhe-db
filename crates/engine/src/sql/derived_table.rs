//! FROM 句の派生表 `(SELECT ...) [[AS] 別名 [(列名, ...)]]` の抽出と静的解決
//! （SQL-62・Issue #1865。親 #1864。実行は #1866・JOIN 入力と深さ上限は #1867）。
//!
//! 呼び出し元は `sql::allowlist::validate_sql_tokens_impl`（SELECT。[`gate`]）と `validate_with_statement`
//! （非再帰 `WITH`。[`gate_with`]）の冒頭で、セッション最上位の
//! 入口（`CteEvaluation::AllowMaterialize`。`EngineCore` の
//! セッション SQL 入口だけが該当）に限って呼ばれる。ビュー本文・cursor・COPY・CTAS・
//! サブクエリ内側・`EXPLAIN`・`WITH RECURSIVE` はこの経路に入らず、従来どおり既存パーサーが
//! `42601` で拒否する（fail-closed）。
//!
//! 方式は「前置き抽出」。深さ 0 の FROM 句から派生表を検出して本文を切り出し、外側のトークン列
//! では派生表を内部名のプレースホルダ 1 トークンへ置き換える（#1848 のスカラーサブクエリと同型）。
//! 外側の文の構文検証は本 Issue では行わず、#1866 が置換後のトークン列で行う。本文の解析・検証は
//! CTE 本文と同じ部品（`parse_cte_select_body`・`validate_cte_definition_at`）を共有する。
//!
//! 検証をすべて通過した文は [`execution_unsupported`] の固定文言 `0A000` で止める（単一ゲート）。
//! 派生表の別名を実表名として走査へ流す経路は存在しない。
//!
//! RLS-10 (b) の不変条件: 本モジュールの型はテナント・`PolicyContext` を持たない。解決時の
//! `42P01`／`42P10` 等はカタログの構造だけで決まり、行やテナントに依存しない。

use crate::nesting_limits::NestingLimits;
use crate::sql::allowlist::{
    derived_output_names, is_set_operator_ident, parse_cte_select_body, parse_derived_alias_head,
    parse_with_clause_and_main, set_operator_is_followed_by_branch, split_parenthesized,
    validate_cte_definition_at, validate_cte_definitions, SqlSurfaceError, TableLookup,
};
use crate::sql::cte::{CteDef, CteMaterialization};
use crate::sql::lexer::{Keyword, Token};

/// 派生表の内部名の接頭辞。`catalog::validate_identifier` を通らない（`?` を含む）ため利用者が
/// 書けず、カタログにも当たらない。
const PLACEHOLDER_PREFIX: &str = "?derived";

/// 派生表が FROM 句のどの位置に現れたか（#1866・#1867 が外側の束縛に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // 参照側は #1866・#1867。本 Issue は解決結果の保持まで。
pub(crate) enum DerivedPosition {
    /// `FROM (SELECT ...)`。
    From,
    /// `... JOIN (SELECT ...)`。
    Join,
    /// `FROM a, (SELECT ...)`（カンマ区切り）。
    CommaList,
}

/// 解決済みの派生表 1 件。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // 参照側は #1866・#1867。本 Issue は解決結果の保持まで。
pub(crate) struct DerivedTable {
    /// 利用者が書いた別名（無ければ `None`。名前を合成しない）。
    pub(crate) alias: Option<String>,
    /// 列名リスト（空ならリストなし）。
    pub(crate) column_aliases: Vec<String>,
    /// 本文を CTE 定義の形で保持したもの（`name` は内部プレースホルダ）。
    pub(crate) def: CteDef,
    /// 公開列名（本文の出力名へ列名リストを先頭から当てたもの。静的に決まらない列は `None`）。
    pub(crate) exposed_columns: Vec<Option<String>>,
    pub(crate) position: DerivedPosition,
}

/// 抽出・解決の結果。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // 参照側は #1866・#1867。
pub(crate) struct DerivedFromClause {
    pub(crate) tables: Vec<DerivedTable>,
    /// 派生表を内部名の `Token::Ident` 1 つへ置き換えた外側のトークン列。
    pub(crate) outer_tokens: Vec<Token>,
}

/// 抽出直後（解決前）の派生表。
#[derive(Debug)]
struct RawDerived {
    body: Vec<Token>,
    alias: Option<String>,
    column_aliases: Vec<String>,
    position: DerivedPosition,
}

/// 抽出結果（派生表の並びと、派生表をプレースホルダへ置き換えた外側のトークン列）。
type Extracted = (Vec<RawDerived>, Vec<Token>);

/// 派生表を含む文の実行が未対応であることを示す固定エラー（`0A000`）。入力の断片・別名を含めない。
/// #1866 はこの関数の呼び出しを実行経路への接続へ置き換える。
pub(crate) fn execution_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("derived tables in FROM are not supported yet")
}

fn is_ident(t: Option<&Token>, word: &str) -> bool {
    matches!(t, Some(Token::Ident(n)) if n.eq_ignore_ascii_case(word))
}

/// `tokens[idx]` の直前が派生表を置ける位置（`FROM`／`JOIN`／FROM 句内の `,`）なら、その種別を返す。
fn position_before(tokens: &[Token], idx: usize) -> Option<DerivedPosition> {
    let prev = idx.checked_sub(1).and_then(|p| tokens.get(p))?;
    match prev {
        Token::Keyword(Keyword::From) => Some(DerivedPosition::From),
        Token::Ident(n) if n.eq_ignore_ascii_case("JOIN") => Some(DerivedPosition::Join),
        Token::Punct(',') => Some(DerivedPosition::CommaList),
        _ => None,
    }
}

/// 深さ 0 の FROM 句を終わらせるトークンか。
fn ends_from_clause(tokens: &[Token], idx: usize) -> bool {
    match tokens.get(idx) {
        Some(Token::Keyword(Keyword::Where | Keyword::Order | Keyword::Limit)) => true,
        Some(Token::Punct(';')) => true,
        Some(Token::Ident(n)) => {
            ["GROUP", "HAVING", "OFFSET", "FETCH", "WINDOW"]
                .iter()
                .any(|w| n.eq_ignore_ascii_case(w))
                // `JOIN ... USING (a, b)` の `USING (` は終端ではない。`USING PLAN`・
                // `USING OPERATION_ID` が終端。
                || (n.eq_ignore_ascii_case("USING")
                    && !matches!(tokens.get(idx.saturating_add(1)), Some(Token::Punct('('))))
        }
        _ => false,
    }
}

/// 深さ 0 の FROM 句から派生表を検出・切り出す。見つからなければ `Ok(None)`。
/// 深さ 0 に集合演算子がある文は対象外（`Ok(None)`。従来どおり既存パーサーが拒否する）。
/// `LATERAL (SELECT ...)` は固定 `0A000`（SQL-62 の対象外）。走査は O(トークン数)・添字は `get()`。
fn extract(tokens: &[Token], max_tables: usize) -> Result<Option<Extracted>, SqlSurfaceError> {
    let mut found: Vec<RawDerived> = Vec::new();
    let mut outer: Vec<Token> = Vec::new();
    let mut depth: usize = 0;
    let mut in_from = false;
    let mut i: usize = 0;
    while let Some(tok) = tokens.get(i) {
        if depth == 0 {
            // 後続に枝（`SELECT` 等）が続く場合のみ集合演算子とみなす（`union` 等は通常の識別子）。
            if matches!(tok, Token::Ident(n) if is_set_operator_ident(n))
                && set_operator_is_followed_by_branch(tokens, i)
            {
                return Ok(None);
            }
            if matches!(tok, Token::Keyword(Keyword::From)) {
                // `IS [NOT] DISTINCT FROM` は FROM 句の開始ではない。
                let after_distinct = i
                    .checked_sub(1)
                    .and_then(|p| tokens.get(p))
                    .is_some_and(|t| is_ident(Some(t), "DISTINCT"));
                in_from = !after_distinct;
            } else if in_from && ends_from_clause(tokens, i) {
                in_from = false;
            }
            if in_from {
                if is_ident(Some(tok), "LATERAL")
                    && matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('(')))
                    && position_before(tokens, i).is_some()
                {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "LATERAL derived tables are not supported",
                    ));
                }
                if matches!(tok, Token::Punct('('))
                    && matches!(
                        tokens.get(i.saturating_add(1)),
                        Some(Token::Keyword(Keyword::Select))
                    )
                {
                    if let Some(position) = position_before(tokens, i) {
                        let rest = tokens.get(i..).unwrap_or(&[]);
                        let (inner, after) = split_parenthesized(rest)?;
                        if inner.iter().any(|t| matches!(t, Token::Punct(';'))) {
                            return Err(SqlSurfaceError::unsupported(
                                "derived table body must not contain a statement separator",
                            ));
                        }
                        if found.len() >= max_tables {
                            return Err(SqlSurfaceError::payload_too_large(
                                "too many derived tables",
                            ));
                        }
                        let (alias, column_aliases, consumed) = parse_derived_alias_head(after)?;
                        // 同じ FROM 句内の別名重複は `42601`（公開名の重複と同じ方針）。
                        if let Some(a) = &alias {
                            if found.iter().any(|d| {
                                d.alias
                                    .as_deref()
                                    .is_some_and(|o| o.eq_ignore_ascii_case(a))
                            }) {
                                return Err(SqlSurfaceError::unsupported(
                                    "table name specified more than once",
                                ));
                            }
                        }
                        outer.push(Token::Ident(format!(
                            "{PLACEHOLDER_PREFIX}{}",
                            found.len().saturating_add(1)
                        )));
                        found.push(RawDerived {
                            body: inner.to_vec(),
                            alias,
                            column_aliases,
                            position,
                        });
                        // 本文（`(` 〜 `)`）と別名部を読み飛ばす。
                        let skipped = rest
                            .len()
                            .saturating_sub(after.len())
                            .saturating_add(consumed);
                        i = i.saturating_add(skipped);
                        continue;
                    }
                }
            }
        }
        match tok {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ => {}
        }
        outer.push(tok.clone());
        i = i.saturating_add(1);
    }
    if found.is_empty() {
        Ok(None)
    } else {
        Ok(Some((found, outer)))
    }
}

/// 抽出した派生表を 1 件ずつ独立に解決する（派生表どうしは互いに見えない。`ctes` は先行する
/// 利用者の CTE で、本文から見える）。`42P10` は派生表用の variant へ写す。
fn resolve(
    raw: Vec<RawDerived>,
    outer_tokens: Vec<Token>,
    ctes: &[CteDef],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<DerivedFromClause, SqlSurfaceError> {
    let mut tables: Vec<DerivedTable> = Vec::new();
    for (n, r) in raw.into_iter().enumerate() {
        let (body, tail) = parse_cte_select_body(&r.body, limits)?;
        let placeholder = format!("{PLACEHOLDER_PREFIX}{}", n.saturating_add(1));
        let def = CteDef {
            name: placeholder.clone(),
            column_aliases: r.column_aliases.clone(),
            body,
            materialization: CteMaterialization::Default,
            tail,
        };
        let mut scope: Vec<CteDef> = ctes.to_vec();
        scope.push(def);
        let idx = scope.len().saturating_sub(1);
        let remap = |e: SqlSurfaceError| match e {
            SqlSurfaceError::CteColumnListTooLong {
                name,
                available,
                specified,
            } if name == placeholder => SqlSurfaceError::DerivedTableColumnListTooLong {
                alias: r.alias.clone().unwrap_or_default(),
                available,
                specified,
            },
            other => other,
        };
        validate_cte_definition_at(&scope, idx, lookup, limits).map_err(remap)?;
        let exposed_columns = derived_output_names(&scope, idx, lookup, limits).map_err(remap)?;
        let def = scope.pop().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "derived table scope was empty".to_string(),
        })?;
        tables.push(DerivedTable {
            alias: r.alias,
            column_aliases: r.column_aliases,
            def,
            exposed_columns,
            position: r.position,
        });
    }
    Ok(DerivedFromClause {
        tables,
        outer_tokens,
    })
}

/// セッション最上位の入口で、FROM に派生表を持つ `SELECT` 文を検出して検証し、固定 `0A000` で止める。
/// 派生表を含まない文は `Ok(())`（呼び出し元は従来の検証へ進む）。派生表を含む文は必ず `Err`
/// （構文 `42601` → 解決 `42P01`／`42703`／`42P10`／`42702` 等 → 実行未対応の `0A000` の順）。
/// 先頭が `SELECT` でない文は対象外（`WITH` は [`gate_with`]）。
pub(crate) fn gate(
    tokens: &[Token],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<(), SqlSurfaceError> {
    if !matches!(tokens.first(), Some(Token::Keyword(Keyword::Select))) {
        return Ok(());
    }
    let Some((raw, outer)) = extract(tokens, crate::sql::relation::MAX_TABLE_REFS)? else {
        return Ok(());
    };
    resolve(raw, outer, &[], lookup, limits)?;
    Err(execution_unsupported())
}

/// [`gate`] の非再帰 `WITH` 版。呼び出し元は `allowlist::validate_with_statement`（セッション最上位の
/// 入口だけが `AllowMaterialize` で呼ぶ。`EngineCore` の WITH 入口は `validate_sql_tokens_impl` を
/// 経由しないため、ここで受ける）。主クエリの FROM から派生表を抽出し、利用者の CTE を先に検証してから
/// 派生表ごとに解決する。データ変更 CTE を含む文は、書き込みを発生させないよう解決の前に固定
/// `0A000` で止める。
pub(crate) fn gate_with(
    tokens: &[Token],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<(), SqlSurfaceError> {
    let (ctes, main_tokens) = parse_with_clause_and_main(tokens, limits)?;
    let Some((raw, outer)) = extract(&main_tokens, crate::sql::relation::MAX_TABLE_REFS)? else {
        return Ok(());
    };
    if ctes.iter().any(|d| d.is_data_modifying()) {
        return Err(crate::sql::cte::data_modifying_cte_unsupported());
    }
    validate_cte_definitions(&ctes, lookup, limits)?;
    resolve(raw, outer, &ctes, lookup, limits)?;
    Err(execution_unsupported())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn found(sql: &str) -> Option<Vec<RawDerived>> {
        let tokens = tokenize(sql).expect("tokenize");
        extract(&tokens, 8).expect("extract").map(|(r, _)| r)
    }

    #[test]
    fn detects_from_join_and_comma_positions() {
        let r = found("SELECT * FROM (SELECT a FROM t) x").expect("from");
        assert_eq!(r.first().map(|d| d.position), Some(DerivedPosition::From));
        let r = found("SELECT * FROM t JOIN (SELECT a FROM t) x ON t.a = x.a").expect("join");
        assert_eq!(r.first().map(|d| d.position), Some(DerivedPosition::Join));
        let r = found("SELECT * FROM t, (SELECT a FROM t) AS x(b)").expect("comma");
        let d = r.first().expect("one");
        assert_eq!(d.position, DerivedPosition::CommaList);
        assert_eq!(d.alias.as_deref(), Some("x"));
        assert_eq!(d.column_aliases, vec!["b".to_string()]);
    }

    #[test]
    fn replaces_derived_table_with_placeholder() {
        let tokens = tokenize("SELECT * FROM (SELECT a FROM t) x WHERE b = 1").expect("tokenize");
        let (_, outer) = extract(&tokens, 8).expect("extract").expect("found");
        assert!(outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.starts_with(PLACEHOLDER_PREFIX))));
        assert!(!outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n == "x")));
    }

    #[test]
    fn does_not_misdetect_other_subquery_positions() {
        for sql in [
            "SELECT (SELECT 1), a FROM t",
            "SELECT a, (SELECT 1) FROM t",
            "SELECT extract(year FROM ts) FROM t",
            "SELECT * FROM t WHERE a IN (SELECT a FROM t)",
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t)",
            "SELECT * FROM t WHERE a IS DISTINCT FROM (SELECT 1)",
            "SELECT * FROM t JOIN u USING (a)",
            "SELECT * FROM t UNION SELECT * FROM (SELECT a FROM t) x",
        ] {
            assert!(found(sql).is_none(), "{sql}");
        }
    }

    #[test]
    fn limits_and_lateral_are_rejected() {
        let tokens = tokenize("SELECT * FROM (SELECT 1 FROM t) a, (SELECT 1 FROM t) b").expect("t");
        assert_eq!(extract(&tokens, 1).expect_err("limit").wire_code(), "54000");
        let tokens = tokenize("SELECT * FROM LATERAL (SELECT 1 FROM t) a").expect("t");
        assert_eq!(
            extract(&tokens, 8).expect_err("lateral").wire_code(),
            "0A000"
        );
    }
}
