//! 別名つき単一表 `UPDATE`／`DELETE`（`FROM`／`USING` なし）の入口トークン正規化
//! （SQL-53・RLS-10・RECOVER-10/11 のポインタ。Issue #3291）。
//!
//! `UPDATE t [AS] x SET ... WHERE x.a = 1 RETURNING x.*` と `DELETE FROM t [AS] x WHERE ...` を、
//! 構文解析より前に「別名なしの同じ文」のトークン列へ書き換える（別名トークンの除去と、
//! `<別名>.<列>` から `<列>` への 1 対 1 置換）。以降の経路（構文解析・名前解決・一時表の振り分け・
//! `$n` の型推論・ビュー DML・台帳ハッシュ・RLS・分割実行）は、すべて既存の別名なし文として動く。
//! `parse_update`／`parse_delete` は変えない（`FROM`／`USING` 形は専用の解析器が別名を扱う）。
//!
//! 呼び出し元は `EngineCore::normalize_row_count_clauses`（簡易・拡張クエリの各パース入口が
//! 構文検証より前に呼ぶ。冪等）。公開名の照合は `FROM`／`USING` 形と同じ
//! [`TableRef::matches_qualifier`] で、別名を付けた表を本名で修飾すると `42P01`
//! （カタログを引かないため他テナント・他表の存在オラクルにならない）。
//!
//! 書き換えない（従来どおり既存解析器の拒否に委ねる＝fail-closed）形:
//! `colref_sink` の記録区間（PL/pgSQL の定義時検証）・`EXPLAIN`／`WITH` 先頭・`FROM`／`USING` 形・
//! 別名が予約語・別名の直後が想定外のトークン。サブクエリ括弧の中は書き換えない
//! （内側の検証が従来どおり判定する）。走査は反復で再帰せず、確保は入力長以下。
//! untrusted 入力経路のため添字アクセスは使わない。

use crate::sql::allowlist::{
    looks_like_delete_using, looks_like_update_from, probe_dml_target_alias, SqlSurfaceError,
};
use crate::sql::lexer::{quoted_bool_word_in_leaf_position, Keyword, Token};
use crate::sql::qualified_column::{
    is_contextual_syntax_use, is_non_column_position, is_syntax_word,
};
use crate::sql::relation::TableRef;

fn is_word(tok: Option<&Token>, w: &str) -> bool {
    matches!(tok, Some(Token::Ident(n)) if n.eq_ignore_ascii_case(w))
}

/// 別名の直後に許す句境界か（`UPDATE` は `SET` のみ、`DELETE` は文末・`;`・`WHERE`・`RETURNING`・
/// `USING OPERATION_ID`・`PARTITIONED`）。これ以外は別名の取り違えを避けるため書き換えない。
fn follow_ok(is_update: bool, tokens: &[Token], after: usize) -> bool {
    let next = tokens.get(after);
    if is_update {
        return is_word(next, "SET");
    }
    match next {
        None | Some(Token::Punct(';')) | Some(Token::Keyword(Keyword::Where)) => true,
        Some(Token::Ident(_)) => {
            is_word(next, "RETURNING")
                || is_word(next, "PARTITIONED")
                || (is_word(next, "USING")
                    && is_word(tokens.get(after.saturating_add(1)), "OPERATION_ID"))
        }
        _ => false,
    }
}

/// `SET` 区間の代入左辺（`x.a = ...`・`(x.a, x.b) = ...`）の位置を `keep` に印付ける。左辺の修飾は
/// 受理しない（後段のパーサーが `42601`。`FROM` 形の `SET t.a = ...` と同じコード）。
fn mark_set_lhs(tokens: &[Token], set_idx: usize, keep: &mut [bool]) {
    let n = tokens.len();
    let mut i = set_idx.saturating_add(1);
    loop {
        // 項目先頭が `(` なら対応する `)` までが行値の左辺候補。
        if matches!(tokens.get(i), Some(Token::Punct('('))) {
            let mut depth = 0usize;
            let mut j = i;
            while j < n {
                match tokens.get(j) {
                    Some(Token::Punct('(')) => depth = depth.saturating_add(1),
                    Some(Token::Punct(')')) => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j = j.saturating_add(1);
            }
            if matches!(tokens.get(j.saturating_add(1)), Some(Token::Punct('='))) {
                for k in i..=j.min(n.saturating_sub(1)) {
                    if let Some(f) = keep.get_mut(k) {
                        *f = true;
                    }
                }
            }
        } else if matches!(tokens.get(i), Some(Token::QualifiedIdent { .. }))
            && matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('=')))
        {
            if let Some(f) = keep.get_mut(i) {
                *f = true;
            }
        }
        // 次の深さ 0 の `,`（次の代入項目）まで進める。区間終端（WHERE 等）なら終了。
        let mut depth = 0usize;
        let mut next_item = None;
        let mut j = i;
        while j < n {
            match tokens.get(j) {
                Some(Token::Punct('(')) => depth = depth.saturating_add(1),
                Some(Token::Punct(')')) => depth = depth.saturating_sub(1),
                Some(Token::Punct(',')) if depth == 0 => {
                    next_item = Some(j.saturating_add(1));
                    break;
                }
                Some(Token::Punct(';')) | Some(Token::Keyword(Keyword::Where)) if depth == 0 => {
                    break
                }
                Some(Token::Ident(w))
                    if depth == 0
                        && (w.eq_ignore_ascii_case("RETURNING")
                            || w.eq_ignore_ascii_case("PARTITIONED")) =>
                {
                    break
                }
                _ => {}
            }
            j = j.saturating_add(1);
        }
        match next_item {
            Some(k) => i = k,
            None => break,
        }
    }
}

/// 別名つき単一表 `UPDATE`／`DELETE` のトークン列を別名なしの同じ文へ書き換える。
/// 対象外の入力は（所有権ごと）そのまま返す。冪等。
pub(crate) fn normalize(tokens: Vec<Token>) -> Result<Vec<Token>, SqlSurfaceError> {
    if crate::sql::colref_sink::is_active() {
        return Ok(tokens);
    }
    let (is_update, target_idx) = match tokens.first() {
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("UPDATE") => (true, 1usize),
        Some(Token::Ident(w))
            if w.eq_ignore_ascii_case("DELETE")
                && matches!(tokens.get(1), Some(Token::Keyword(Keyword::From))) =>
        {
            (false, 2usize)
        }
        _ => return Ok(tokens),
    };
    if looks_like_update_from(&tokens) || looks_like_delete_using(&tokens) {
        return Ok(tokens);
    }
    let target_name = match tokens.get(target_idx) {
        Some(Token::Ident(n)) => n.clone(),
        Some(Token::QualifiedIdent { name, .. }) => name.clone(),
        _ => return Ok(tokens),
    };
    let after_target = target_idx.saturating_add(1);
    let Some(rest) = tokens.get(after_target..) else {
        return Ok(tokens);
    };
    let Some((alias, consumed)) = probe_dml_target_alias(rest) else {
        return Ok(tokens);
    };
    let after = after_target.saturating_add(consumed);
    if !follow_ok(is_update, &tokens, after) {
        return Ok(tokens);
    }
    let target = TableRef::with_alias(target_name, alias);

    let n = tokens.len();
    let mut keep = vec![false; n];
    if is_update {
        mark_set_lhs(&tokens, after, &mut keep);
    }

    let mut out: Vec<Token> = Vec::with_capacity(n);
    out.extend(tokens.iter().take(after_target).cloned());
    // 括弧フレーム。真はサブクエリ括弧（中は書き換えない。入れ子は不透明を継承する）。
    let mut frames: Vec<bool> = Vec::new();
    let mut seen_where = false;
    let mut returning_seen = false;
    for i in after..n {
        let Some(tok) = tokens.get(i) else { break };
        let opaque = frames.last().copied().unwrap_or(false);
        match tok {
            Token::Punct('(') => {
                let sub = matches!(
                    tokens.get(i.saturating_add(1)),
                    Some(Token::Keyword(Keyword::Select))
                ) || is_word(tokens.get(i.saturating_add(1)), "WITH")
                    || is_word(tokens.get(i.saturating_add(1)), "VALUES");
                frames.push(opaque || sub);
                out.push(tok.clone());
            }
            Token::Punct(')') => {
                frames.pop();
                out.push(tok.clone());
            }
            Token::Keyword(Keyword::Where) => {
                seen_where = true;
                out.push(tok.clone());
            }
            Token::Ident(w) if frames.is_empty() && w.eq_ignore_ascii_case("RETURNING") => {
                returning_seen = true;
                out.push(tok.clone());
            }
            Token::QualifiedStar { qualifier } if !opaque => {
                if !target.matches_qualifier(qualifier) {
                    return Err(SqlSurfaceError::undefined_table(qualifier.to_string()));
                }
                // `x.*` は RETURNING の項目位置だけ `*` へ置き換える（乗算・`COUNT(*)` への再解釈を防ぐ）。
                let prev_ok = returning_seen
                    && frames.is_empty()
                    && (is_word(i.checked_sub(1).and_then(|p| tokens.get(p)), "RETURNING")
                        || matches!(
                            i.checked_sub(1).and_then(|p| tokens.get(p)),
                            Some(Token::Punct(','))
                        ));
                let next = tokens.get(i.saturating_add(1));
                let next_ok = matches!(next, None | Some(Token::Punct(',' | ';')))
                    || is_word(next, "PARTITIONED")
                    || is_word(next, "USING");
                if !prev_ok || !next_ok {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified star is only supported as a RETURNING item",
                    ));
                }
                out.push(Token::Punct('*'));
            }
            Token::QualifiedIdent { qualifier, name }
                if !opaque
                    && !keep.get(i).copied().unwrap_or(false)
                    && !is_non_column_position(&tokens, i) =>
            {
                if !target.matches_qualifier(qualifier) {
                    return Err(SqlSurfaceError::undefined_table(qualifier.to_string()));
                }
                let is_bool_word =
                    name.eq_ignore_ascii_case("true") || name.eq_ignore_ascii_case("false");
                let leaf =
                    is_bool_word && seen_where && quoted_bool_word_in_leaf_position(&tokens, i);
                if name.eq_ignore_ascii_case("null") {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified null column reference is not supported",
                    ));
                }
                if is_syntax_word(name) || is_contextual_syntax_use(&tokens, i, name) {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified column reference named like a SQL keyword is not supported",
                    ));
                }
                if is_bool_word && !leaf {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified boolean-word column reference is only supported as a WHERE predicate",
                    ));
                }
                out.push(if leaf {
                    Token::QuotedBoolWord(name.clone())
                } else if crate::sql::lexer::is_value_function_keyword(name) {
                    Token::QuotedNull(name.clone())
                } else {
                    Token::Ident(name.clone())
                });
            }
            _ => out.push(tok.clone()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize(sql).expect("tokenize")
    }

    fn norm(sql: &str) -> Result<Vec<Token>, SqlSurfaceError> {
        normalize(toks(sql))
    }

    fn code(sql: &str) -> &'static str {
        norm(sql).expect_err("must be rejected").wire_code()
    }

    #[test]
    fn unrelated_statements_are_unchanged() {
        for sql in [
            "UPDATE t SET a = 1 WHERE a = 2;",
            "DELETE FROM t WHERE a = 2",
            "SELECT * FROM t x WHERE x.a = 1",
            "EXPLAIN UPDATE t x SET a = 1",
            "WITH a AS (UPDATE t x SET a = 1) SELECT 1",
            "UPDATE t x SET a = 1 FROM u WHERE x.k = u.k",
            "DELETE FROM t x USING u WHERE x.k = u.k",
            "DELETE FROM t x y",
            "UPDATE t AS where SET a = 1",
            "DELETE FROM t AS set",
        ] {
            assert_eq!(norm(sql).unwrap(), toks(sql), "{sql}");
        }
    }

    #[test]
    fn update_alias_is_removed_and_qualifiers_resolved() {
        let got =
            norm("UPDATE t AS x SET a = x.b + 1 WHERE x.c = 'v' RETURNING x.a, x.*;").unwrap();
        assert_eq!(
            got,
            toks("UPDATE t SET a = b + 1 WHERE c = 'v' RETURNING a, *;")
        );
        let got = norm("UPDATE t x SET a = 1 WHERE x.c = 2").unwrap();
        assert_eq!(got, toks("UPDATE t SET a = 1 WHERE c = 2"));
    }

    #[test]
    fn delete_alias_is_removed_and_qualifiers_resolved() {
        assert_eq!(
            norm("DELETE FROM t AS x WHERE x.c = 1 RETURNING x.*").unwrap(),
            toks("DELETE FROM t WHERE c = 1 RETURNING *")
        );
        assert_eq!(norm("DELETE FROM t x;").unwrap(), toks("DELETE FROM t;"));
        assert_eq!(
            norm("DELETE FROM t x USING OPERATION_ID 'op1'").unwrap(),
            toks("DELETE FROM t USING OPERATION_ID 'op1'")
        );
        assert_eq!(
            norm("DELETE FROM t x WHERE x.a = 1 PARTITIONED").unwrap(),
            toks("DELETE FROM t WHERE a = 1 PARTITIONED")
        );
    }

    #[test]
    fn schema_qualified_target_is_accepted() {
        assert_eq!(
            norm("DELETE FROM public.t x WHERE x.a = 1").unwrap(),
            toks("DELETE FROM public.t WHERE a = 1")
        );
    }

    #[test]
    fn idempotent() {
        let once = norm("UPDATE t AS x SET a = x.b WHERE x.c = 1").unwrap();
        assert_eq!(normalize(once.clone()).unwrap(), once);
    }

    #[test]
    fn original_name_and_unknown_qualifiers_are_undefined_table() {
        assert_eq!(code("UPDATE t x SET a = 1 WHERE t.a = 1"), "42P01");
        assert_eq!(code("DELETE FROM t x WHERE u.a = 1"), "42P01");
        assert_eq!(code("DELETE FROM t x RETURNING t.*"), "42P01");
    }

    #[test]
    fn set_lhs_is_left_for_the_parser() {
        let got = norm("UPDATE t x SET x.a = 1").unwrap();
        assert!(got
            .iter()
            .any(|t| matches!(t, Token::QualifiedIdent { .. })));
        let got = norm("UPDATE t x SET (x.a, x.b) = (1, 2)").unwrap();
        assert!(got
            .iter()
            .any(|t| matches!(t, Token::QualifiedIdent { .. })));
    }

    #[test]
    fn sentinel_rejections() {
        assert_eq!(code("UPDATE t x SET a = x.null"), "42601");
        assert_eq!(code("UPDATE t x SET a = x.true"), "42601");
        assert_eq!(code("UPDATE t x SET a = 1 WHERE x.a = x.not"), "42601");
        assert_eq!(code("UPDATE t x SET a = x.* "), "42601");
        assert_eq!(code("DELETE FROM t x WHERE x.*"), "42601");
    }

    #[test]
    fn subquery_bodies_are_not_rewritten() {
        let sql = "DELETE FROM t x WHERE a IN (SELECT y.k FROM u y WHERE y.k = 1)";
        let got = norm(sql).unwrap();
        assert!(got
            .iter()
            .any(|t| matches!(t, Token::QualifiedIdent { .. })));
        assert!(!got.iter().any(|t| matches!(t, Token::Ident(w) if w == "x")));
    }

    #[test]
    fn function_name_positions_are_left_alone() {
        let got = norm("UPDATE t x SET a = s.lower(x.b)").unwrap();
        assert!(got
            .iter()
            .any(|t| matches!(t, Token::QualifiedIdent { qualifier, .. } if qualifier == "s")));
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        for sql in [
            "",
            "UPDATE",
            "DELETE",
            "DELETE FROM",
            "UPDATE t x",
            "DELETE FROM t AS",
        ] {
            let _ = normalize(tokenize(sql).unwrap_or_default());
        }
    }
}
