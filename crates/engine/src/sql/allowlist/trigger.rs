//! `CREATE [OR REPLACE] TRIGGER`／`DROP TRIGGER` の構文検証（TABLE-45、Issue #1722。設計記録:
//! `docs/design/trigger-catalog.md`、実行モデル: `docs/design/trigger-execution-model.md` の D8）。
//!
//! 役割: トークン列から定義の構造（名前・タイミング・イベント・対象表・粒度・実行する関数と引数）だけを
//! 取り出す。カタログは一切照会しない（`DROP TABLE` 等と同じ設計。存在・種別・列・関数の検査は
//! `sql::trigger_ddl` が権限ゲートの後で行う）。呼び出し元は `core.rs` の `parse_sql` 内の分岐だけ。
//!
//! 受理しない形（`INSTEAD OF`・`REFERENCING`・`CONSTRAINT TRIGGER`・`DEFERRABLE`・`FROM <表>`・
//! `WHEN`・`TRUNCATE` の行トリガ）は黙って無視せず `0A000` で拒否する（fail-closed。D8）。`TG_ARGV` の
//! 引数は値として読み、SQL テキストへ連結しない。

use super::*;
use crate::catalog::trigger::{TriggerEvents, TriggerLevel, TriggerTiming, MAX_TRIGGER_ARGS};

/// 構造検証済みの `CREATE [OR REPLACE] TRIGGER`（TABLE-45、Issue #1722）。
///
/// 実行は `sql::trigger_ddl::execute_create_trigger`、呼び出し元は `core.rs` の
/// `ParsedSql::CreateTrigger` 腕。名前は解決済み（対象表・関数は裸の名前または SCHEMA 配下の内部キー）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCreateTrigger {
    pub(crate) or_replace: bool,
    pub(crate) name: String,
    pub(crate) timing: TriggerTiming,
    /// [`TriggerEvents`] のビットマスク。
    pub(crate) events: u8,
    pub(crate) table: String,
    pub(crate) level: TriggerLevel,
    pub(crate) update_of: Vec<String>,
    pub(crate) function: String,
    pub(crate) tg_args: Vec<String>,
}

/// 構造検証済みの `DROP TRIGGER [IF EXISTS] <名前> ON <表> [CASCADE|RESTRICT]`（TABLE-45、Issue #1722）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDropTrigger {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) if_exists: bool,
}

/// 先頭が `CREATE [OR REPLACE] [CONSTRAINT] TRIGGER` か（`CREATE FUNCTION`／`CREATE TABLE` の分岐より前に
/// 振り分けるために使う。`CONSTRAINT TRIGGER` もここで捕まえ、検証本体で `0A000` にする）。
pub(crate) fn is_create_trigger_statement(tokens: &[Token]) -> bool {
    let ident_at = |i: usize, w: &str| matches!(tokens.get(i), Some(Token::Ident(n)) if n.eq_ignore_ascii_case(w));
    ident_at(0, "CREATE")
        && (ident_at(1, "TRIGGER")
            || (ident_at(1, "CONSTRAINT") && ident_at(2, "TRIGGER"))
            || (ident_at(1, "OR")
                && ident_at(2, "REPLACE")
                && (ident_at(3, "TRIGGER")
                    || (ident_at(3, "CONSTRAINT") && ident_at(4, "TRIGGER")))))
}

/// 先頭 2 トークンが `DROP TRIGGER` か（汎用 `DROP`〔`DROP TABLE` 扱い〕より前に分岐するために使う）。
pub(crate) fn is_drop_trigger_statement(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("DROP"))
        && matches!(tokens.get(1), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("TRIGGER"))
}

/// 関係名位置の識別子を 1 つ読む。解決されずに残った修飾名（`s.t`）は `0A000`（fail-closed）。
fn expect_relation_ident(p: &mut Parser<'_>, what: &str) -> Result<String, SqlSurfaceError> {
    if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
        return Err(SqlSurfaceError::feature_not_supported(format!(
            "unresolved schema-qualified {what} names are not supported"
        )));
    }
    let name = p.expect_ident()?;
    crate::catalog::validate_relation_key(&name)
        .map_err(|_| SqlSurfaceError::unsupported(format!("invalid {what} name in trigger DDL")))?;
    Ok(name)
}

/// `EXECUTE FUNCTION f(<引数>, ...)` の括弧内を `TG_ARGV` の文字列として読む。文字列・数値リテラルと
/// 単純な識別子だけを受理し、それ以外（式・NULL・`$n` 等）は `42601`。個数は上限検証する。
fn parse_tg_args(p: &mut Parser<'_>) -> Result<Vec<String>, SqlSurfaceError> {
    p.expect_punct('(')?;
    let mut args: Vec<String> = Vec::new();
    if matches!(p.peek(), Some(Token::Punct(')'))) {
        p.advance();
        return Ok(args);
    }
    loop {
        if args.len() >= MAX_TRIGGER_ARGS {
            return Err(SqlSurfaceError::payload_too_large(
                "too many trigger arguments",
            ));
        }
        let mut negative = false;
        if matches!(p.peek(), Some(Token::Punct('-'))) {
            p.advance();
            negative = true;
        }
        let value = match p.advance() {
            Some(Token::StringLiteral(s)) if !negative => s.clone(),
            Some(Token::Number(n)) => {
                if negative {
                    format!("-{n}")
                } else {
                    n.clone()
                }
            }
            // 引用符なしの NULL は識別子ではなく NULL リテラル（`TG_ARGV` に載せられない）ため拒否する。
            // 引用符付き `"NULL"` は通常の識別子として許容する（トークナイザが引用符を外すので
            // ここでは区別できず、大文字小文字を問わず拒否する側に倒す）。
            Some(Token::Ident(w)) if !negative && !w.eq_ignore_ascii_case("NULL") => w.clone(),
            _ => {
                return Err(SqlSurfaceError::unsupported(
                    "trigger arguments must be literals or simple names",
                ))
            }
        };
        if value.len() > crate::catalog::trigger::MAX_TRIGGER_ARG_BYTES {
            return Err(SqlSurfaceError::payload_too_large(
                "trigger argument is too long",
            ));
        }
        args.push(value);
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            break;
        }
    }
    p.expect_punct(')')?;
    Ok(args)
}

/// [`ValidatedCreateTrigger`] の構造検証本体（TABLE-45、Issue #1722）。判定は決定的で、構文の誤りは `42601`、
/// 未対応の形は `0A000`、引数の個数・長さ超過は `54000`。
pub(crate) fn validate_create_trigger_tokens(
    tokens: &[Token],
) -> Result<ValidatedCreateTrigger, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("CREATE")?;
    let or_replace = if p.peek_ident_matches("OR") {
        p.advance();
        p.expect_ident_matching("REPLACE")?;
        true
    } else {
        false
    };
    if p.peek_ident_matches("CONSTRAINT") {
        return Err(SqlSurfaceError::feature_not_supported(
            "CREATE CONSTRAINT TRIGGER is not supported",
        ));
    }
    p.expect_ident_matching("TRIGGER")?;
    if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
        // トリガ名は SCHEMA 修飾できない（PostgreSQL と同じ構文エラー）。
        return Err(SqlSurfaceError::unsupported(
            "trigger names cannot be schema-qualified",
        ));
    }
    let name = p.expect_ident()?;
    crate::catalog::validate_identifier(&name)
        .map_err(|_| SqlSurfaceError::unsupported("invalid trigger name"))?;

    let timing = if p.eat_word("BEFORE") {
        TriggerTiming::Before
    } else if p.eat_word("AFTER") {
        TriggerTiming::After
    } else if p.peek_ident_matches("INSTEAD") {
        return Err(SqlSurfaceError::feature_not_supported(
            "INSTEAD OF triggers are not supported",
        ));
    } else {
        return Err(SqlSurfaceError::unsupported(
            "expected BEFORE or AFTER in CREATE TRIGGER",
        ));
    };

    let mut events: u8 = 0;
    let mut update_of: Vec<String> = Vec::new();
    loop {
        let bit = if p.eat_word("INSERT") {
            TriggerEvents::INSERT
        } else if p.eat_word("DELETE") {
            TriggerEvents::DELETE
        } else if p.eat_word("TRUNCATE") {
            TriggerEvents::TRUNCATE
        } else if p.eat_word("UPDATE") {
            if p.eat_word("OF") {
                loop {
                    let col = p.expect_ident()?;
                    crate::catalog::validate_identifier(&col)
                        .map_err(|_| SqlSurfaceError::unsupported("invalid column name"))?;
                    if update_of.contains(&col) {
                        return Err(SqlSurfaceError::unsupported(
                            "duplicate column in UPDATE OF",
                        ));
                    }
                    update_of.push(col);
                    if matches!(p.peek(), Some(Token::Punct(','))) {
                        p.advance();
                    } else {
                        break;
                    }
                }
            }
            TriggerEvents::UPDATE
        } else {
            return Err(SqlSurfaceError::unsupported(
                "expected INSERT, UPDATE, DELETE or TRUNCATE in CREATE TRIGGER",
            ));
        };
        if events & bit != 0 {
            return Err(SqlSurfaceError::unsupported(
                "duplicate trigger events specified",
            ));
        }
        events |= bit;
        if p.eat_word("OR") {
            continue;
        }
        break;
    }
    p.expect_ident_matching("ON")?;
    let table = expect_relation_ident(&mut p, "table")?;

    let mut level: Option<TriggerLevel> = None;
    loop {
        if p.peek_ident_matches("REFERENCING") {
            return Err(SqlSurfaceError::feature_not_supported(
                "trigger REFERENCING (transition tables) is not supported",
            ));
        }
        if matches!(p.peek(), Some(Token::Keyword(Keyword::From)))
            || p.peek_ident_matches("DEFERRABLE")
            || p.peek_ident_matches("INITIALLY")
            || (p.peek_ident_matches("NOT") && p.peek_ident_matches_at(1, "DEFERRABLE"))
        {
            return Err(SqlSurfaceError::feature_not_supported(
                "deferrable or FROM-referencing triggers are not supported",
            ));
        }
        if p.eat_word("FOR") {
            if level.is_some() {
                return Err(SqlSurfaceError::unsupported(
                    "FOR EACH clause specified more than once",
                ));
            }
            let _ = p.eat_word("EACH");
            level = Some(if p.eat_word("ROW") {
                TriggerLevel::Row
            } else if p.eat_word("STATEMENT") {
                TriggerLevel::Statement
            } else {
                return Err(SqlSurfaceError::unsupported(
                    "expected ROW or STATEMENT after FOR EACH",
                ));
            });
            continue;
        }
        if p.peek_ident_matches("WHEN") {
            // D7（定義時検査）の束縛は未実装のため、黙って受理・保存しない。
            return Err(SqlSurfaceError::feature_not_supported(
                "trigger WHEN conditions are not supported yet",
            ));
        }
        break;
    }
    let level = level.unwrap_or(TriggerLevel::Statement);
    if level == TriggerLevel::Row && events & TriggerEvents::TRUNCATE != 0 {
        return Err(SqlSurfaceError::feature_not_supported(
            "TRUNCATE FOR EACH ROW triggers are not supported",
        ));
    }
    if !update_of.is_empty() && events & TriggerEvents::UPDATE == 0 {
        return Err(SqlSurfaceError::unsupported("UPDATE OF requires UPDATE"));
    }

    p.expect_ident_matching("EXECUTE")?;
    if !(p.eat_word("FUNCTION") || p.eat_word("PROCEDURE")) {
        return Err(SqlSurfaceError::unsupported(
            "expected FUNCTION after EXECUTE",
        ));
    }
    let function = expect_relation_ident(&mut p, "function")?;
    let tg_args = parse_tg_args(&mut p)?;
    p.expect_end_of_statement()?;
    Ok(ValidatedCreateTrigger {
        or_replace,
        name,
        timing,
        events,
        table,
        level,
        update_of,
        function,
        tg_args,
    })
}

/// [`ValidatedDropTrigger`] の構造検証本体（TABLE-45、Issue #1722）。`CASCADE`／`RESTRICT` は受理する
/// （トリガに依存するオブジェクトは無いため結果は同じ）。`ON <表>` の省略は `42601`。
pub(crate) fn validate_drop_trigger_tokens(
    tokens: &[Token],
) -> Result<ValidatedDropTrigger, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("DROP")?;
    p.expect_ident_matching("TRIGGER")?;
    let if_exists = p.consume_if_exists();
    if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
        return Err(SqlSurfaceError::unsupported(
            "trigger names cannot be schema-qualified",
        ));
    }
    let name = p.expect_ident()?;
    crate::catalog::validate_identifier(&name)
        .map_err(|_| SqlSurfaceError::unsupported("invalid trigger name"))?;
    p.expect_ident_matching("ON")?;
    let table = expect_relation_ident(&mut p, "table")?;
    let _ = p.parse_optional_drop_behavior();
    p.expect_end_of_statement()?;
    Ok(ValidatedDropTrigger {
        name,
        table,
        if_exists,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn create(sql: &str) -> Result<ValidatedCreateTrigger, SqlSurfaceError> {
        validate_create_trigger_tokens(&tokenize(sql).expect("lex"))
    }

    fn code(sql: &str) -> &'static str {
        create(sql).expect_err("must be rejected").wire_code()
    }

    #[test]
    fn full_form_is_parsed() {
        let v = create(
            "CREATE OR REPLACE TRIGGER trg BEFORE INSERT OR UPDATE OF a, b OR DELETE ON t \
             FOR EACH ROW EXECUTE FUNCTION f('x', 1, -2, word)",
        )
        .expect("parse");
        assert!(v.or_replace);
        assert_eq!(v.name, "trg");
        assert_eq!(v.timing, TriggerTiming::Before);
        assert_eq!(
            v.events,
            TriggerEvents::INSERT | TriggerEvents::UPDATE | TriggerEvents::DELETE
        );
        assert_eq!(v.update_of, vec!["a", "b"]);
        assert_eq!(v.level, TriggerLevel::Row);
        assert_eq!(v.function, "f");
        assert_eq!(v.tg_args, vec!["x", "1", "-2", "word"]);
    }

    /// 引用符なしの NULL は `TG_ARGV` の値にできず `42601`（後段のカタログエラーへ流さない）。
    #[test]
    fn bare_null_argument_is_42601() {
        for sql in [
            "CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f(NULL)",
            "CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f('a', null)",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
        // 文字列リテラルの 'NULL' は通常の値。
        let v =
            create("CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f('NULL')").expect("parse");
        assert_eq!(v.tg_args, vec!["NULL"]);
    }

    #[test]
    fn default_level_is_statement_and_procedure_is_accepted() {
        let v =
            create("CREATE TRIGGER trg AFTER TRUNCATE ON t EXECUTE PROCEDURE f()").expect("parse");
        assert_eq!(v.level, TriggerLevel::Statement);
        assert!(v.tg_args.is_empty());
        assert!(!v.or_replace);
    }

    #[test]
    fn unsupported_forms_are_0a000() {
        for sql in [
            "CREATE TRIGGER g INSTEAD OF INSERT ON t EXECUTE FUNCTION f()",
            "CREATE CONSTRAINT TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER INSERT ON t REFERENCING NEW TABLE AS n EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER INSERT ON t DEFERRABLE EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER INSERT ON t FROM u EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW WHEN (a > 1) EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER TRUNCATE ON t FOR EACH ROW EXECUTE FUNCTION f()",
        ] {
            assert_eq!(code(sql), "0A000", "{sql}");
        }
    }

    #[test]
    fn malformed_forms_are_42601() {
        for sql in [
            "CREATE TRIGGER g INSERT ON t EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER INSERT OR INSERT ON t EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f(a + 1)",
            "CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f(NULL, (1))",
            "CREATE TRIGGER g AFTER INSERT ON t",
            "CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f() x",
            "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW FOR EACH ROW EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER INSERT OF a ON t EXECUTE FUNCTION f()",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
    }

    #[test]
    fn too_many_args_is_54000() {
        let args = vec!["1"; MAX_TRIGGER_ARGS + 1].join(", ");
        let sql = format!("CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION f({args})");
        assert_eq!(code(&sql), "54000");
    }

    #[test]
    fn drop_trigger_forms() {
        let ok = |sql: &str| validate_drop_trigger_tokens(&tokenize(sql).expect("lex"));
        let v = ok("DROP TRIGGER IF EXISTS g ON t CASCADE").expect("parse");
        assert!(v.if_exists);
        assert_eq!((v.name.as_str(), v.table.as_str()), ("g", "t"));
        assert!(!ok("DROP TRIGGER g ON t RESTRICT").expect("parse").if_exists);
        assert_eq!(
            ok("DROP TRIGGER g").expect_err("no ON").wire_code(),
            "42601"
        );
        assert_eq!(
            ok("DROP TRIGGER g ON t x")
                .expect_err("trailing")
                .wire_code(),
            "42601"
        );
    }

    #[test]
    fn statement_classifiers() {
        let t = |s: &str| tokenize(s).expect("lex");
        assert!(is_create_trigger_statement(&t(
            "CREATE TRIGGER g AFTER INSERT ON t"
        )));
        assert!(is_create_trigger_statement(&t(
            "create or replace trigger g"
        )));
        assert!(is_create_trigger_statement(&t(
            "CREATE CONSTRAINT TRIGGER g"
        )));
        assert!(!is_create_trigger_statement(&t(
            "CREATE TABLE trigger (a int)"
        )));
        assert!(is_drop_trigger_statement(&t("DROP TRIGGER g ON t")));
        assert!(!is_drop_trigger_statement(&t("DROP TABLE trigger")));
    }
}
