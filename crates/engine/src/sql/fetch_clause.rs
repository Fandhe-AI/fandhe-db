//! 標準 SQL の行数制限句 `FETCH {FIRST|NEXT} [n] {ROW|ROWS} {ONLY|WITH TIES}` と
//! `OFFSET m {ROW|ROWS}` を、構文検証（`sql::allowlist`）が解釈する正規形へ書き換える
//! トークン段の正規化器（SQL-56 (3)・TASK-298 のポインタ。Issue #1793）。
//!
//! 呼び出し元は `core::EngineCore` の 2 つの入口（単純問い合わせ `parse_tokens_in`・拡張
//! 問い合わせ `parse_sql_prepared_for_tenant_impl`）で、どちらも `schema.table` の名前解決
//! （`sql::relation_name`）より前に本関数を 1 回だけ適用する。正規化は冪等で、二重に適用
//! しても結果は変わらない。
//!
//! 設計方針:
//!
//! - `ONLY` は `LIMIT n` へ、`OFFSET m ROWS` は `OFFSET m` へ書き換える。構文検証の各経路
//!   （広域取得・kNN・集計・集合演算・CTE・サブクエリ・ビュー本文・DECLARE の内側など）は
//!   従来どおり `LIMIT`／`OFFSET` だけを解析するため、範囲・上限・エラー契約は `LIMIT` と
//!   完全に同一になる（独自の検証を持たない）。
//! - `WITH TIES` は正規形 `FETCH FIRST <n> ROWS WITH TIES` のまま残す。解釈するのは
//!   `sql::allowlist` の単一表スカラー `ORDER BY` 広域取得と `GROUP BY` 集計だけで、
//!   それ以外の経路は正規形を知らないため `42601` で拒否される（fail-closed）。
//!   書き換えが届かない文種・入れ子位置へ `WITH TIES` が黙って流れないよう、最上位／入れ子の
//!   使用有無を [`Normalized`] で呼び出し元へ返す（呼び出し元が `0A000` で拒否する）。
//! - 階層は括弧グループで分ける（サブクエリの `LIMIT` と外側の `FETCH` は併記にならない）。
//! - 文頭（先頭または最上位 `;` の直後）の `FETCH`／`MOVE` はカーソル文（`sql::cursor`）なので
//!   その文末まで書き換えない。
//!
//! untrusted な入力を扱うため、添字アクセス・`unwrap`・`expect` を使わず `get` で走査する。
//! 出力は入力長 + 定数で確保し、括弧スタックは括弧 1 組につき小さな 1 要素だけ増える。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{Keyword, Token};

/// [`normalize_fetch_clauses`] の結果。
#[derive(Debug)]
pub(crate) struct Normalized {
    /// 正規化後のトークン列。
    pub(crate) tokens: Vec<Token>,
    /// 最上位グループ（括弧の外）に `WITH TIES` がある。
    pub(crate) top_with_ties: bool,
    /// 括弧の内側のグループに `WITH TIES` がある。
    pub(crate) nested_with_ties: bool,
}

/// 1 つの括弧グループ（最上位を含む）の走査状態。
#[derive(Default)]
struct Group {
    has_order_by: bool,
    limit_seen: bool,
    fetch_seen: bool,
    /// 直前に出力した `OFFSET` 句の出力位置 `[start, end)`。直後に `FETCH` が来たとき、
    /// エンジンの文法（`LIMIT n OFFSET m` の順）へ並べ替えるために使う。
    offset_span: Option<(usize, usize)>,
}

fn is_word(token: &Token, word: &str) -> bool {
    matches!(token, Token::Ident(s) if s.eq_ignore_ascii_case(word))
}

fn is_row_word(token: Option<&Token>) -> bool {
    matches!(token, Some(t) if is_word(t, "ROW") || is_word(t, "ROWS"))
}

fn is_count_value(token: Option<&Token>) -> bool {
    matches!(token, Some(Token::Number(_) | Token::Param(_)))
}

/// `prev`（直前の入力トークン）が式・表参照・`LIMIT` 値などの「オペランドの終端」かどうか。
/// `OFFSET`／`FETCH` は予約語ではなく識別子（列名・表名）としても使えるため、句として
/// 解釈するのは直前がオペランド終端のときだけに限る（`SELECT offset + 1 rows`・
/// `FROM fetch first` のように直前が `SELECT`／`FROM`／演算子・`,` ・`(` なら識別子）。
fn follows_operand(prev: Option<&Token>) -> bool {
    matches!(
        prev,
        Some(
            Token::Ident(_)
                | Token::Number(_)
                | Token::Param(_)
                | Token::StringLiteral(_)
                | Token::QuotedBoolWord(_)
                | Token::QuotedNull(_)
                | Token::Punct(')' | ']')
        )
    )
}

fn syntax(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(detail)
}

/// `tokens` の `FETCH`／`OFFSET ... ROWS` を正規形へ書き換える（モジュール冒頭参照）。
///
/// `FETCH` と `OFFSET` を含まない文は入力をそのまま返す（clone なし）。同じ階層での
/// `LIMIT` と `FETCH` の併記、`FETCH` の重複、`PERCENT` など PostgreSQL 17 に無い形、
/// `ORDER BY` の無い `WITH TIES` は `42601`。
pub(crate) fn normalize_fetch_clauses(tokens: Vec<Token>) -> Result<Normalized, SqlSurfaceError> {
    if !tokens
        .iter()
        .any(|t| is_word(t, "FETCH") || is_word(t, "OFFSET"))
    {
        return Ok(Normalized {
            tokens,
            top_with_ties: false,
            nested_with_ties: false,
        });
    }

    let mut out: Vec<Token> = Vec::with_capacity(tokens.len().saturating_add(4));
    let mut stack: Vec<Group> = Vec::new();
    let mut top = Group::default();
    let mut at_start = true;
    let mut cursor_stmt = false;
    let mut top_with_ties = false;
    let mut nested_with_ties = false;
    let mut i = 0usize;

    while let Some(tok) = tokens.get(i) {
        if at_start {
            at_start = false;
            cursor_stmt = is_word(tok, "FETCH") || is_word(tok, "MOVE");
        }
        if cursor_stmt {
            if matches!(tok, Token::Punct(';')) {
                at_start = true;
                cursor_stmt = false;
                top = Group::default();
            }
            out.push(tok.clone());
            i += 1;
            continue;
        }
        match tok {
            Token::Punct('(') => {
                out.push(tok.clone());
                stack.push(Group::default());
                i += 1;
                continue;
            }
            Token::Punct(')') => {
                out.push(tok.clone());
                stack.pop();
                i += 1;
                continue;
            }
            Token::Punct(';') if stack.is_empty() => {
                out.push(tok.clone());
                top = Group::default();
                at_start = true;
                i += 1;
                continue;
            }
            _ => {}
        }
        let nested = !stack.is_empty();
        let g = match stack.last_mut() {
            Some(g) => g,
            None => &mut top,
        };

        match tok {
            Token::Keyword(Keyword::Limit) => {
                if g.fetch_seen {
                    return Err(syntax("LIMIT and FETCH cannot be used together"));
                }
                g.limit_seen = true;
                out.push(tok.clone());
                i += 1;
            }
            Token::Keyword(Keyword::Order)
                if matches!(tokens.get(i + 1), Some(Token::Keyword(Keyword::By))) =>
            {
                g.has_order_by = true;
                out.push(tok.clone());
                i += 1;
            }
            _ if is_word(tok, "OFFSET")
                && follows_operand(i.checked_sub(1).and_then(|p| tokens.get(p))) =>
            {
                // `OFFSET [+|-] <値>`。値の形の検証は従来の `OFFSET` 解析に任せ、ここでは
                // 直後の `ROW`／`ROWS` を取り除くだけにする。
                let mut j = i + 1;
                if matches!(tokens.get(j), Some(Token::Punct('+' | '-'))) {
                    j += 1;
                }
                if is_count_value(tokens.get(j)) {
                    let start = out.len();
                    for t in tokens.get(i..=j).unwrap_or(&[]) {
                        out.push(t.clone());
                    }
                    g.offset_span = Some((start, out.len()));
                    i = j + 1;
                    if is_row_word(tokens.get(i)) {
                        i += 1;
                    }
                } else {
                    out.push(tok.clone());
                    i += 1;
                }
            }
            _ if is_word(tok, "FETCH")
                && follows_operand(i.checked_sub(1).and_then(|p| tokens.get(p)))
                && matches!(tokens.get(i + 1), Some(t) if is_word(t, "FIRST") || is_word(t, "NEXT")) =>
            {
                if g.fetch_seen || g.limit_seen {
                    return Err(syntax("multiple row count clauses are not allowed"));
                }
                g.fetch_seen = true;
                let mut j = i + 2;
                let mut count: Vec<Token> = Vec::with_capacity(2);
                if is_row_word(tokens.get(j)) {
                    count.push(Token::Number("1".to_string()));
                } else {
                    if let Some(sign @ Token::Punct('+' | '-')) = tokens.get(j) {
                        count.push(sign.clone());
                        j += 1;
                    }
                    match tokens.get(j) {
                        Some(v @ (Token::Number(_) | Token::Param(_))) => {
                            count.push(v.clone());
                            j += 1;
                        }
                        _ => return Err(syntax("malformed FETCH FIRST row count")),
                    }
                    if !is_row_word(tokens.get(j)) {
                        return Err(syntax("FETCH FIRST requires ROW or ROWS"));
                    }
                }
                // `ROW`／`ROWS` の次。
                j += 1;
                let ties = match tokens.get(j) {
                    Some(t) if is_word(t, "ONLY") => {
                        j += 1;
                        false
                    }
                    Some(t)
                        if is_word(t, "WITH")
                            && matches!(tokens.get(j + 1), Some(t2) if is_word(t2, "TIES")) =>
                    {
                        j += 2;
                        true
                    }
                    _ => return Err(syntax("FETCH FIRST requires ONLY or WITH TIES")),
                };
                if ties {
                    if !g.has_order_by {
                        return Err(syntax(
                            "WITH TIES cannot be specified without ORDER BY clause",
                        ));
                    }
                    if nested {
                        nested_with_ties = true;
                    } else {
                        top_with_ties = true;
                    }
                }
                // 直前の `OFFSET` 句が直後に連なっていれば取り外し、`LIMIT/FETCH` を先にして
                // 末尾へ戻す（エンジンの文法は `LIMIT n OFFSET m` の順だけを受理する）。
                let held: Vec<Token> = match g.offset_span.take() {
                    Some((start, end)) if end == out.len() && start <= end => out.split_off(start),
                    Some(_) => return Err(syntax("OFFSET must directly precede FETCH")),
                    None => Vec::new(),
                };
                if ties {
                    out.push(Token::Ident("FETCH".to_string()));
                    out.push(Token::Ident("FIRST".to_string()));
                    out.extend(count);
                    out.push(Token::Ident("ROWS".to_string()));
                    out.push(Token::Ident("WITH".to_string()));
                    out.push(Token::Ident("TIES".to_string()));
                } else {
                    out.push(Token::Keyword(Keyword::Limit));
                    out.extend(count);
                }
                out.extend(held);
                i = j;
            }
            _ => {
                out.push(tok.clone());
                i += 1;
            }
        }
    }

    Ok(Normalized {
        tokens: out,
        top_with_ties,
        nested_with_ties,
    })
}

/// [`normalize_fetch_clauses`] に、`WITH TIES` の受理範囲のガードを加えた文トークン列の入口
/// （SQL-56 (3)・Issue #1793）。`WITH TIES` を解釈できるのは「`SELECT` で始まる最上位の問い合わせ」
/// だけなので、それ以外の文種（`WITH`・`EXPLAIN`・`CREATE VIEW`・`DECLARE`・`INSERT`・`COPY` など）と
/// 括弧内の使用は、黙って `ONLY` 扱いにせず `0A000` で拒否する（fail-closed）。
/// `core::EngineCore` の各パース入口と `sql::relation_name::tokenize_resolved_with` が、構文検証・
/// 名前解決より前に呼ぶ。
pub(crate) fn normalize_statement_tokens(
    tokens: Vec<Token>,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let normalized = normalize_fetch_clauses(tokens)?;
    let select_head = matches!(
        normalized.tokens.first(),
        Some(Token::Keyword(Keyword::Select))
    );
    if normalized.nested_with_ties || (normalized.top_with_ties && !select_head) {
        return Err(SqlSurfaceError::feature_not_supported(
            "FETCH FIRST ... WITH TIES is supported only in a top-level SELECT",
        ));
    }
    Ok(normalized.tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize_with_params;

    fn norm(sql: &str) -> Result<Normalized, SqlSurfaceError> {
        normalize_fetch_clauses(tokenize_with_params(sql).expect("tokenize"))
    }

    fn same(sql: &str, expected: &str) {
        let n = norm(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert_eq!(
            n.tokens,
            tokenize_with_params(expected).expect("tokenize expected"),
            "{sql}"
        );
    }

    fn rejected(sql: &str) {
        assert!(
            matches!(norm(sql), Err(SqlSurfaceError::UnsupportedSyntax { .. })),
            "{sql}"
        );
    }

    #[test]
    fn only_becomes_limit() {
        same(
            "SELECT id FROM t FETCH FIRST 3 ROWS ONLY",
            "SELECT id FROM t LIMIT 3",
        );
        same(
            "SELECT id FROM t fetch next 3 row only",
            "SELECT id FROM t LIMIT 3",
        );
        same(
            "SELECT id FROM t FETCH FIRST ROW ONLY",
            "SELECT id FROM t LIMIT 1",
        );
        same(
            "SELECT id FROM t FETCH NEXT ROWS ONLY",
            "SELECT id FROM t LIMIT 1",
        );
    }

    #[test]
    fn quoted_value_function_name_is_operand_end() {
        // 引用付きの値関数名（`"current_date"` 等）は QuotedNull になるが識別子の終端
        same(
            "SELECT id FROM q ORDER BY \"current_date\" FETCH FIRST 2 ROWS ONLY",
            "SELECT id FROM q ORDER BY \"current_date\" LIMIT 2",
        );
    }

    #[test]
    fn offset_rows_is_stripped_and_reordered() {
        same(
            "SELECT id FROM t LIMIT 3 OFFSET 2 ROWS",
            "SELECT id FROM t LIMIT 3 OFFSET 2",
        );
        same(
            "SELECT id FROM t LIMIT 3 OFFSET 2 ROW",
            "SELECT id FROM t LIMIT 3 OFFSET 2",
        );
        same(
            "SELECT id FROM t OFFSET 2 ROWS FETCH NEXT 3 ROWS ONLY",
            "SELECT id FROM t LIMIT 3 OFFSET 2",
        );
        same(
            "SELECT id FROM t FETCH FIRST 3 ROWS ONLY OFFSET 2 ROWS",
            "SELECT id FROM t LIMIT 3 OFFSET 2",
        );
    }

    #[test]
    fn with_ties_is_kept_in_canonical_form() {
        let n =
            norm("SELECT id FROM t ORDER BY id OFFSET 1 ROW FETCH NEXT ROW WITH TIES").expect("ok");
        assert!(n.top_with_ties && !n.nested_with_ties);
        assert_eq!(
            n.tokens,
            tokenize_with_params(
                "SELECT id FROM t ORDER BY id FETCH FIRST 1 ROWS WITH TIES OFFSET 1"
            )
            .expect("tokenize")
        );
        // 冪等。
        let again = normalize_fetch_clauses(n.tokens.clone()).expect("idempotent");
        assert_eq!(again.tokens, n.tokens);
        assert!(again.top_with_ties);
    }

    #[test]
    fn nested_with_ties_is_reported() {
        let n =
            norm("SELECT id FROM (SELECT id FROM t ORDER BY id FETCH FIRST 1 ROWS WITH TIES) s")
                .expect("ok");
        assert!(n.nested_with_ties && !n.top_with_ties);
    }

    #[test]
    fn invalid_forms_are_rejected() {
        rejected("SELECT id FROM t LIMIT 1 FETCH FIRST 1 ROWS ONLY");
        rejected("SELECT id FROM t FETCH FIRST 1 ROWS ONLY LIMIT 1");
        rejected("SELECT id FROM t FETCH FIRST 1 ROWS ONLY FETCH FIRST 1 ROWS ONLY");
        rejected("SELECT id FROM t FETCH FIRST 1 PERCENT ROWS ONLY");
        rejected("SELECT id FROM t FETCH FIRST 1 ROWS");
        rejected("SELECT id FROM t FETCH FIRST 1 ONLY");
        rejected("SELECT id FROM t FETCH FIRST (1) ROWS ONLY");
        rejected("SELECT id FROM t FETCH FIRST 1 ROWS WITH TIES");
        rejected("SELECT id FROM (SELECT id FROM t ORDER BY id) s FETCH FIRST 1 ROWS WITH TIES");
    }

    #[test]
    fn parenthesised_groups_are_independent() {
        same(
            "SELECT id FROM (SELECT id FROM t LIMIT 5) s FETCH FIRST 2 ROWS ONLY",
            "SELECT id FROM (SELECT id FROM t LIMIT 5) s LIMIT 2",
        );
    }

    #[test]
    fn cursor_statements_are_left_alone() {
        let sql = "FETCH NEXT FROM c";
        let n = norm(sql).expect("ok");
        assert_eq!(n.tokens, tokenize_with_params(sql).expect("tokenize"));
        let sql = "FETCH 2 FROM c; SELECT id FROM t FETCH FIRST 1 ROW ONLY";
        same(sql, "FETCH 2 FROM c; SELECT id FROM t LIMIT 1");
        let sql = "SELECT 1; FETCH FIRST FROM c";
        let n = norm(sql).expect("ok");
        assert_eq!(n.tokens, tokenize_with_params(sql).expect("tokenize"));
    }

    #[test]
    fn identifiers_named_offset_and_fetch_are_untouched() {
        for sql in [
            "SELECT offset + 1 rows FROM t FETCH FIRST 2 ROWS ONLY",
            "SELECT fetch first FROM t",
            "SELECT id FROM fetch first",
            "SELECT id FROM t WHERE offset + 1 = 2",
        ] {
            let n = norm(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            let expected = sql.replace("FETCH FIRST 2 ROWS ONLY", "LIMIT 2");
            assert_eq!(
                n.tokens,
                tokenize_with_params(&expected).expect("tokenize"),
                "{sql}"
            );
        }
    }

    #[test]
    fn literals_are_untouched() {
        let sql = "SELECT id FROM t WHERE lang = 'FETCH FIRST 1 ROWS ONLY' LIMIT 1";
        let n = norm(sql).expect("ok");
        assert_eq!(n.tokens, tokenize_with_params(sql).expect("tokenize"));
    }

    #[test]
    fn statements_without_the_clauses_pass_through() {
        let toks = tokenize_with_params("SELECT id FROM t LIMIT 3").expect("tokenize");
        let n = normalize_fetch_clauses(toks.clone()).expect("ok");
        assert_eq!(n.tokens, toks);
    }
}
