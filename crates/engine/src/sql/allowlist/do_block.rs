//! `DO [LANGUAGE <名>] '<本体>'` の構文検証（SQL-68・Issue #2228。設計記録は
//! `docs/design/plpgsql-subset.md` の「Issue #2228 追記」）。
//!
//! 役割: トークン列から匿名ブロックの本体（文字列リテラルの内容）だけを取り出す。本体の PL/pgSQL
//! 構文・埋め込み SQL の検証と実行は core（`core::do_block`）が `plpgsql` へ委譲する。ここはカタログも
//! テナントも見ない純粋な処理で、呼び出し元は `core.rs` の `parse_tokens_gated_inner` 内の分岐だけ。
//!
//! 受理する言語は `plpgsql` のみ。`sql` は `0A000`、未知の言語は `42704`（言語名は応答へ含めない固定
//! 文言）。`USING OPERATION_ID`・`PARTITIONED`・`$n`・本体が文字列リテラルでない形は受理しない
//! （fail-closed。台帳のキーが表単位のため、表を持たない `DO` の再送を台帳で防げない）。

use super::*;

/// 構造検証済みの `DO` ブロック（SQL-68・Issue #2228）。実行は `core::do_block`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDo {
    /// 本体の原文（文字列リテラルの内容。`''` は字句化で解除済み）。
    pub(crate) body_source: String,
}

/// 先頭が `DO` か（汎用の文分類より前に振り分けるために使う）。
pub(crate) fn is_do_statement(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("DO"))
}

/// 言語名を検証する（`plpgsql` のみ）。
fn check_language(name: &str) -> Result<(), SqlSurfaceError> {
    if name.eq_ignore_ascii_case("plpgsql") {
        Ok(())
    } else if name.eq_ignore_ascii_case("sql") {
        Err(SqlSurfaceError::feature_not_supported(
            "DO supports only the plpgsql language",
        ))
    } else {
        Err(SqlSurfaceError::UndefinedObject {
            name: "language".to_string(),
        })
    }
}

/// `DO` のトークン列を検証する。構文は `DO [LANGUAGE <名>] <文字列> [LANGUAGE <名>] [;]`
/// （`LANGUAGE` は前後どちらかに高々 1 回）。
pub(crate) fn validate_do_tokens(tokens: &[Token]) -> Result<ValidatedDo, SqlSurfaceError> {
    let syntax = || SqlSurfaceError::unsupported("invalid DO statement");
    let mut i = 1usize;
    let mut language_seen = false;
    let mut body: Option<String> = None;
    loop {
        match tokens.get(i) {
            None | Some(Token::Punct(';')) => {
                // `;` の後ろに何かあれば不正（複数文は文分割が先に処理する）。
                if tokens.get(i.saturating_add(1)).is_some() {
                    return Err(syntax());
                }
                break;
            }
            Some(Token::Ident(w)) if w.eq_ignore_ascii_case("LANGUAGE") => {
                if language_seen {
                    return Err(syntax());
                }
                language_seen = true;
                let Some(Token::Ident(name)) = tokens.get(i.saturating_add(1)) else {
                    return Err(syntax());
                };
                check_language(name)?;
                i = i.saturating_add(2);
            }
            Some(Token::StringLiteral(s)) => {
                if body.is_some() {
                    return Err(syntax());
                }
                body = Some(s.clone());
                i = i.saturating_add(1);
            }
            Some(Token::Ident(w)) if w.eq_ignore_ascii_case("USING") => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "USING OPERATION_ID is not supported for DO",
                ));
            }
            Some(Token::Ident(w)) if w.eq_ignore_ascii_case("PARTITIONED") => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "PARTITIONED is not supported for DO",
                ));
            }
            Some(_) => return Err(syntax()),
        }
    }
    let body_source = body.ok_or_else(syntax)?;
    Ok(ValidatedDo { body_source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn v(sql: &str) -> Result<ValidatedDo, SqlSurfaceError> {
        validate_do_tokens(&tokenize(sql).expect("tokenize"))
    }

    fn code(sql: &str) -> &'static str {
        v(sql).expect_err("must fail").wire_code()
    }

    #[test]
    fn accepts_language_before_or_after_and_omitted() {
        let want = "BEGIN NULL; END";
        for s in [
            "DO 'BEGIN NULL; END'",
            "DO LANGUAGE plpgsql 'BEGIN NULL; END'",
            "do 'BEGIN NULL; END' language PLPGSQL",
            "DO 'BEGIN NULL; END';",
        ] {
            assert_eq!(v(s).expect(s).body_source, want, "{s}");
        }
    }

    #[test]
    fn language_errors() {
        assert_eq!(code("DO LANGUAGE sql 'SELECT 1'"), "0A000");
        assert_eq!(code("DO LANGUAGE nosuch 'x'"), "42704");
        assert_eq!(code("DO LANGUAGE plpgsql 'x' LANGUAGE plpgsql"), "42601");
    }

    #[test]
    fn malformed_forms_are_42601_and_options_are_0a000() {
        assert_eq!(code("DO"), "42601");
        assert_eq!(code("DO LANGUAGE plpgsql"), "42601");
        assert_eq!(code("DO 'a' 'b'"), "42601");
        assert_eq!(code("DO 1"), "42601");
        assert_eq!(code("DO 'a' extra"), "42601");
        assert_eq!(code("DO 'a'; 'b'"), "42601");
        assert_eq!(code("DO 'a' USING OPERATION_ID 'x'"), "0A000");
        assert_eq!(code("DO 'a' PARTITIONED"), "0A000");
    }
}
