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
//! 検証を通過した「FROM 直後の単一の派生表」（#1866）は、合成 CTE として既存の単一表 `WITH`
//! 主クエリ経路でインライン展開または具体化して実行する（第 3 の実行器を作らない）。JOIN／カンマ区切り・
//! 複数件（#1867）は、同じ合成 CTE を `WITH`＋JOIN 主クエリ経路（`allowlist::validate_with_join_main`）
//! の slot として評価する。派生表の本文に入れ子の派生表を置けるが、深さは運用者設定の
//! サブクエリ深さ上限（`NestingLimits::max_subquery_depth`）で打ち切り、超過は `54000`。外側へは
//! 別名だけを残し、派生表の別名を実表名として走査へ流す経路は存在しない（合成名は `?` を含む。
//! JOIN パーサーが合成名だけをカタログ検証の対象外にする例外は [`is_derived_placeholder`] で判定）。
//!
//! RLS-10 (b) の不変条件: 本モジュールの型はテナント・`PolicyContext` を持たない。解決時の
//! `42P01`／`42P10` 等はカタログの構造だけで決まり、行やテナントに依存しない。

use crate::nesting_limits::NestingLimits;
use crate::sql::allowlist::Statement;
use crate::sql::allowlist::{
    derived_output_names, is_set_operator_ident, looks_like_join, parse_cte_select_body,
    parse_derived_alias_head, parse_with_clause_and_main, set_operator_is_followed_by_branch,
    single_relation_main_over_ctes, split_parenthesized, validate_cte_definition_at,
    validate_cte_definitions, validate_with_join_main, SqlSurfaceError, TableLookup,
};
use crate::sql::cte::{CteDef, CteMaterialization};
use crate::sql::lexer::{Keyword, Token};

/// 派生表の内部名の接頭辞。`catalog::validate_identifier` を通らない（`?` を含む）ため利用者が
/// 書けず、カタログにも当たらない。
const PLACEHOLDER_PREFIX: &str = "?derived";

/// 派生表が FROM 句のどの位置に現れたか（#1866・#1867 が外側の束縛に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub(crate) struct DerivedTable {
    /// 利用者が書いた別名（無ければ `None`。名前を合成しない）。
    pub(crate) alias: Option<String>,
    /// 列名リスト（空ならリストなし）。
    pub(crate) column_aliases: Vec<String>,
    pub(crate) position: DerivedPosition,
}

/// 抽出・解決の結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedFromClause {
    /// 最上位の FROM 句にある派生表（書かれた順）。
    pub(crate) tables: Vec<DerivedTable>,
    /// 合成 CTE（本文を CTE 定義の形で保持。`name` は内部プレースホルダ）。入れ子の内側が必ず
    /// 外側より前に並ぶ依存順で、最上位の派生表も含む。利用者の CTE の後ろへ付けて使う。
    pub(crate) scope_defs: Vec<CteDef>,
    /// 派生表を内部名の `Token::Ident` 1 つへ置き換えた外側のトークン列。
    pub(crate) outer_tokens: Vec<Token>,
}

/// 抽出直後（解決前）の派生表。
#[derive(Debug)]
struct RawDerived {
    /// 文全体で一意な番号（プレースホルダ `?derived<番号>` になる。入れ子でも重ならない）。
    number: usize,
    body: Vec<Token>,
    alias: Option<String>,
    column_aliases: Vec<String>,
    position: DerivedPosition,
}

/// 抽出結果（派生表の並びと、派生表をプレースホルダへ置き換えた外側のトークン列）。
type Extracted = (Vec<RawDerived>, Vec<Token>);

/// 抽出中の文全体で共有する状態（番号の採番・総数と入れ子深さの上限）。
struct ExtractState {
    /// これまでに切り出した派生表の総数（入れ子を含む。番号の採番にも使う）。
    count: usize,
    max_tables: usize,
    max_depth: usize,
}

impl ExtractState {
    /// 運用者設定の入れ子上限と、表参照数の上限（`relation::MAX_TABLE_REFS`）で初期化する。
    fn for_limits(limits: NestingLimits) -> Self {
        Self {
            count: 0,
            max_tables: crate::sql::relation::MAX_TABLE_REFS,
            max_depth: limits.max_subquery_depth(),
        }
    }
}

/// `name` が派生表の合成名（`?derived` ＋ 数字）か。`allowlist` の JOIN パーサーが、字句解析器が
/// 作れない `?` を含むこの名前だけをカタログ検証の対象外にするために使う（利用者は引用形でも
/// 書けない）。
pub(crate) fn is_derived_placeholder(name: &str) -> bool {
    name.strip_prefix(PLACEHOLDER_PREFIX)
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// 複数リレーションの FROM にある別名なしの派生表を示す固定エラー（`0A000`）。公開名が内部名に
/// なるため受け付けない。入力の断片を含めない。
fn alias_required_in_join() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("a derived table in a JOIN must have an alias")
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
#[cfg(test)]
fn extract(tokens: &[Token], max_tables: usize) -> Result<Option<Extracted>, SqlSurfaceError> {
    let mut st = ExtractState {
        count: 0,
        max_tables,
        max_depth: crate::sql::allowlist::MAX_SUBQUERY_DEPTH,
    };
    extract_at(tokens, &mut st, 1, true)
}

/// [`extract`] の本体。`depth_level` はここで見つかる派生表の入れ子深さ（最上位は 1）で、運用者設定の
/// 上限を超えたら本文の解析より前に `54000` で拒否する。`keep_alias` が偽なら別名を外側へ残さない
/// （派生表の本文側。CTE 本文は修飾列の正規化を持たない）。
fn extract_at(
    tokens: &[Token],
    st: &mut ExtractState,
    depth_level: usize,
    keep_alias: bool,
) -> Result<Option<Extracted>, SqlSurfaceError> {
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
                        if depth_level > st.max_depth {
                            return Err(SqlSurfaceError::payload_too_large(format!(
                                "derived table nesting exceeds limit {}",
                                st.max_depth
                            )));
                        }
                        if st.count >= st.max_tables {
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
                        st.count = st.count.saturating_add(1);
                        let number = st.count;
                        outer.push(Token::Ident(format!("{PLACEHOLDER_PREFIX}{number}")));
                        // 別名は暗黙別名の形で外側へ残す（`t.col` の修飾参照を外側の既存の
                        // 修飾列正規化が解決できるようにする。列名リストは合成 CTE が適用する）。
                        if let (Some(a), true) = (&alias, keep_alias) {
                            outer.push(Token::Ident(a.clone()));
                        }
                        found.push(RawDerived {
                            number,
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

/// 抽出した派生表を解決する。`ctes` は先行する利用者の CTE で、本文から見える。本文に入れ子の
/// 派生表があれば内側を先に解決して `scope_defs` へ積み、プレースホルダへ置き換えた本文で外側を
/// 解決する（外側の合成 CTE が必ず内側より後ろに並ぶ）。`42P10` は派生表用の variant へ写す。
fn resolve(
    raw: Vec<RawDerived>,
    outer_tokens: Vec<Token>,
    ctes: &[CteDef],
    st: &mut ExtractState,
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<DerivedFromClause, SqlSurfaceError> {
    let mut scope_defs: Vec<CteDef> = Vec::new();
    let tables = resolve_level(raw, ctes, &mut scope_defs, st, 1, lookup, limits)?;
    Ok(DerivedFromClause {
        tables,
        scope_defs,
        outer_tokens,
    })
}

/// [`resolve`] の 1 階層分（`depth_level` は `raw` の入れ子深さ）。再帰の段数は
/// `ExtractState::max_depth`（運用者設定のサブクエリ深さ上限。ハード上限 4）で抑えられる。
fn resolve_level(
    raw: Vec<RawDerived>,
    ctes: &[CteDef],
    acc: &mut Vec<CteDef>,
    st: &mut ExtractState,
    depth_level: usize,
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Vec<DerivedTable>, SqlSurfaceError> {
    let mut tables: Vec<DerivedTable> = Vec::new();
    for r in raw {
        // 入れ子の派生表は本文の解析より前に切り出す（深さ超過 `54000` が本文の構文 `42601` に先行）。
        let body_tokens = match extract_at(&r.body, st, depth_level.saturating_add(1), false)? {
            Some((inner, replaced)) => {
                resolve_level(
                    inner,
                    ctes,
                    acc,
                    st,
                    depth_level.saturating_add(1),
                    lookup,
                    limits,
                )?;
                replaced
            }
            None => r.body.clone(),
        };
        let (body, tail) = parse_cte_select_body(&body_tokens, limits)?;
        let placeholder = format!("{PLACEHOLDER_PREFIX}{}", r.number);
        let def = CteDef {
            name: placeholder.clone(),
            column_aliases: r.column_aliases.clone(),
            body,
            materialization: CteMaterialization::Default,
            tail,
        };
        if ctes.len().saturating_add(acc.len()).saturating_add(1) > limits.max_cte_definitions() {
            return Err(SqlSurfaceError::payload_too_large(
                "too many CTE definitions",
            ));
        }
        let mut scope: Vec<CteDef> = ctes.to_vec();
        scope.extend(acc.iter().cloned());
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
        // 公開列名の静的な決定可否もここで検査する（解決できない列は `42703` 等）。
        derived_output_names(&scope, idx, lookup, limits).map_err(remap)?;
        let def = scope.pop().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "derived table scope was empty".to_string(),
        })?;
        acc.push(def);
        tables.push(DerivedTable {
            alias: r.alias,
            column_aliases: r.column_aliases,
            position: r.position,
        });
    }
    Ok(tables)
}

/// 派生表を含むか（`$n` 束縛の fail-closed ガード用。`sql::params` から呼ばれる。第 2 の検出器を
/// 作らず [`extract`] を共有する。抽出が拒否した形も「含む」とみなす）。
pub(crate) fn contains_derived_table(tokens: &[Token]) -> bool {
    let mut st = ExtractState {
        count: 0,
        max_tables: crate::sql::relation::MAX_TABLE_REFS,
        max_depth: crate::sql::allowlist::MAX_SUBQUERY_DEPTH,
    };
    !matches!(extract_at(tokens, &mut st, 1, true), Ok(None))
}

/// 解決済みの派生表を実行形へ組む（#1866・#1867）。派生表は名前を利用者が書けない合成 CTE として、
/// 利用者の CTE の後ろへ付けて既存の経路へ流す（第 3 の実行器を作らない）。「派生表 1 件・`FROM` 直後・
/// 外側が JOIN／カンマ区切りでない」文は単一表 `WITH` 主クエリ経路
/// （`allowlist::single_relation_main_over_ctes`）、それ以外（派生表と実表・派生表どうしの JOIN、
/// カンマ区切り、複数件）は `WITH`＋JOIN 主クエリ経路（`allowlist::validate_with_join_main`。slot の
/// 評価・RLS・`JoinBudget` を CTE と共有）へ流す。いずれも参照した側セッションの `PolicyContext` で
/// 評価され、テナント情報は持ち込まない。
fn build_statement(
    clause: DerivedFromClause,
    ctes: &[CteDef],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Statement, SqlSurfaceError> {
    let DerivedFromClause {
        tables,
        scope_defs,
        outer_tokens,
    } = clause;
    let mut scope: Vec<CteDef> = ctes.to_vec();
    scope.extend(scope_defs);
    let single = matches!(tables.as_slice(), [t] if t.position == DerivedPosition::From);
    if single && !looks_like_join(&outer_tokens) {
        return single_relation_main_over_ctes(&scope, &outer_tokens, lookup, limits);
    }
    // 別名なしの派生表は公開名が内部名になるため、複数リレーションの FROM では受け付けない。
    if tables.iter().any(|t| t.alias.is_none()) {
        return Err(alias_required_in_join());
    }
    validate_with_join_main(&outer_tokens, &scope, lookup, limits)
}

/// セッション最上位の入口で、FROM に派生表を持つ `SELECT` 文を検出して検証し、実行形へ組む。
/// 派生表を含まない文は `Ok(None)`（呼び出し元は従来の検証へ進む）。派生表を含む文は
/// 構文 `42601` → 解決 `42P01`／`42703`／`42P10`／`42702` 等 → 実行対象外の `0A000` の順に判定する。
/// 先頭が `SELECT` でない文は対象外（`WITH` は [`gate_with`]）。
pub(crate) fn gate(
    tokens: &[Token],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Option<Statement>, SqlSurfaceError> {
    if !matches!(tokens.first(), Some(Token::Keyword(Keyword::Select))) {
        return Ok(None);
    }
    let mut st = ExtractState::for_limits(limits);
    let Some((raw, outer)) = extract_at(tokens, &mut st, 1, true)? else {
        return Ok(None);
    };
    let clause = resolve(raw, outer, &[], &mut st, lookup, limits)?;
    build_statement(clause, &[], lookup, limits).map(Some)
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
) -> Result<Option<Statement>, SqlSurfaceError> {
    let (ctes, main_tokens) = parse_with_clause_and_main(tokens, limits)?;
    let mut st = ExtractState::for_limits(limits);
    let Some((raw, outer)) = extract_at(&main_tokens, &mut st, 1, true)? else {
        return Ok(None);
    };
    if ctes.iter().any(|d| d.is_data_modifying()) {
        return Err(crate::sql::cte::data_modifying_cte_unsupported());
    }
    validate_cte_definitions(&ctes, lookup, limits)?;
    let clause = resolve(raw, outer, &ctes, &mut st, lookup, limits)?;
    build_statement(clause, &ctes, lookup, limits).map(Some)
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
        // 別名は暗黙別名の形でプレースホルダの直後に残る。
        let pos = outer
            .iter()
            .position(|t| matches!(t, Token::Ident(n) if n.starts_with(PLACEHOLDER_PREFIX)))
            .expect("placeholder");
        assert!(matches!(outer.get(pos + 1), Some(Token::Ident(n)) if n == "x"));
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

    #[test]
    fn placeholder_detection_is_narrow() {
        assert!(is_derived_placeholder("?derived1"));
        assert!(is_derived_placeholder("?derived42"));
        for n in [
            "?derived",
            "?derivedx",
            "derived1",
            "?derived1 ",
            "docs",
            "?Derived1",
        ] {
            assert!(!is_derived_placeholder(n), "{n}");
        }
    }

    #[test]
    fn users_cannot_write_a_placeholder_name() {
        // 字句解析器は `?` を含む識別子を作らず、引用形も受理しない（JOIN パーサーの例外の根拠）。
        assert!(tokenize("SELECT * FROM \"?derived1\"").is_err());
        let toks = tokenize("SELECT * FROM ?derived1");
        assert!(toks.map_or(true, |t| !t
            .iter()
            .any(|x| matches!(x, Token::Ident(n) if is_derived_placeholder(n)))));
    }

    #[test]
    fn nested_extraction_numbers_are_unique_and_body_drops_alias() {
        let tokens =
            tokenize("SELECT * FROM (SELECT a FROM (SELECT a FROM t) y) x, (SELECT 1 FROM t) z")
                .expect("tokenize");
        let mut st = ExtractState {
            count: 0,
            max_tables: 8,
            max_depth: 4,
        };
        let (raws, _) = extract_at(&tokens, &mut st, 1, true)
            .expect("extract")
            .expect("found");
        assert_eq!(
            raws.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let (inner, replaced) = extract_at(&raws[0].body, &mut st, 2, false)
            .expect("nested")
            .expect("found");
        // 入れ子の番号は最上位と重ならず、本文側にはプレースホルダだけが残る（別名なし）。
        assert_eq!(inner.iter().map(|r| r.number).collect::<Vec<_>>(), vec![3]);
        assert!(!replaced
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n == "y")));
        assert!(replaced
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n == "?derived3")));
    }

    #[test]
    fn depth_limit_is_checked_before_parsing_the_body() {
        let tokens = tokenize("SELECT * FROM (SELECT a FROM t) x").expect("tokenize");
        let mut st = ExtractState {
            count: 0,
            max_tables: 8,
            max_depth: 1,
        };
        assert!(extract_at(&tokens, &mut st, 1, true).is_ok());
        let err = extract_at(&tokens, &mut st, 2, true).expect_err("depth 2 > 1");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn contains_derived_table_sees_join_and_nested_forms() {
        for sql in [
            "SELECT * FROM t JOIN (SELECT a FROM t) x ON t.a = x.a",
            "SELECT * FROM (SELECT a FROM (SELECT a FROM t) y) x",
        ] {
            assert!(contains_derived_table(&tokenize(sql).expect("t")), "{sql}");
        }
        assert!(!contains_derived_table(
            &tokenize("SELECT * FROM t WHERE a IN (SELECT a FROM t)").expect("t")
        ));
    }
}
