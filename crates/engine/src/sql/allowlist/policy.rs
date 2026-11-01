//! `CREATE POLICY`／`ALTER POLICY`／`DROP POLICY` の構文検証（RLS-13、Issue #2274。設計記録:
//! `docs/design/row-security-policy-composition.md`）。
//!
//! 役割: トークン列から定義の構造（名前・対象表・`AS`・`FOR`・`TO`・`USING`・`WITH CHECK`）だけを取り出す。
//! カタログは一切照会しない（`trigger` と同じ設計。存在・種別・列・式の束縛は `sql::policy_ddl` が権限ゲートの
//! 後で行う）。呼び出し元は `core.rs` の `parse_sql` 内の分岐だけ。
//!
//! 括弧内の式は既存の `Parser::parse_check_body` で解析する（第 2 のパーサーを作らない）。括弧の対応・
//! 入れ子の深さ・ノード予算は既存の Parser の上限に従う。
//!
//! 構文段で拒否する形: `FOR SELECT`／`FOR DELETE` の `WITH CHECK`、`FOR INSERT` の `USING`（`42601`）、
//! 句の順序違い・重複・未知語・修飾付きのポリシー名・`ON` の欠落（`42601`）、`TO` の `PUBLIC` 以外
//! （名前付きロール・`CURRENT_USER`・`SESSION_USER`。ロール機構が入るまで `0A000`。存在を確かめられない名前を
//! 保存すると fail-open になるため）。`FOR` と句の矛盾のうち既存ポリシーの `FOR` を読まないと判定できない
//! `ALTER POLICY` の形は実行段（`sql::policy_ddl`）で同じ `42601` にする。

use super::*;
use crate::catalog::policy::{PolicyCommand, PolicyRole};

/// 構造検証済みの `CREATE POLICY`（RLS-13、Issue #2274）。
///
/// 実行は `sql::policy_ddl::execute_create_policy`、呼び出し元は `core.rs` の `ParsedSql::CreatePolicy` 腕。
/// 名前は解決済み（対象表は裸の名前または SCHEMA 配下の内部キー）。式は未束縛の述語木で、列の存在・型の
/// 検査は実行段が write txn の中のスキーマに対して行う。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCreatePolicy {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) permissive: bool,
    pub(crate) command: PolicyCommand,
    pub(crate) roles: Vec<PolicyRole>,
    pub(crate) using: Option<Vec<WherePredicate>>,
    pub(crate) check: Option<Vec<WherePredicate>>,
}

/// `ALTER POLICY` の動作。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AlterPolicyAction {
    /// `RENAME TO <新名>`（他の句とは併用できない）。
    Rename(String),
    /// `[TO ...] [USING (...)] [WITH CHECK (...)]`。指定しなかった句は保持する。
    Modify {
        roles: Option<Vec<PolicyRole>>,
        using: Option<Vec<WherePredicate>>,
        check: Option<Vec<WherePredicate>>,
    },
}

/// 構造検証済みの `ALTER POLICY`（RLS-13、Issue #2274）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedAlterPolicy {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) action: AlterPolicyAction,
}

/// 構造検証済みの `DROP POLICY [IF EXISTS] <名前> ON <表> [CASCADE|RESTRICT]`（RLS-13、Issue #2274）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDropPolicy {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) if_exists: bool,
}

fn ident_at(tokens: &[Token], i: usize, w: &str) -> bool {
    matches!(tokens.get(i), Some(Token::Ident(n)) if n.eq_ignore_ascii_case(w))
}

/// 先頭 2 トークンが `CREATE POLICY` か（`CREATE TABLE`／`FUNCTION` の分岐より前に振り分けるために使う）。
pub(crate) fn is_create_policy_statement(tokens: &[Token]) -> bool {
    ident_at(tokens, 0, "CREATE") && ident_at(tokens, 1, "POLICY")
}

/// 先頭 2 トークンが `ALTER POLICY` か（汎用 `ALTER`〔`ALTER TABLE` 扱い〕より前に分岐するために使う）。
pub(crate) fn is_alter_policy_statement(tokens: &[Token]) -> bool {
    ident_at(tokens, 0, "ALTER") && ident_at(tokens, 1, "POLICY")
}

/// 先頭 2 トークンが `DROP POLICY` か（汎用 `DROP`〔`DROP TABLE` 扱い〕より前に分岐するために使う）。
pub(crate) fn is_drop_policy_statement(tokens: &[Token]) -> bool {
    ident_at(tokens, 0, "DROP") && ident_at(tokens, 1, "POLICY")
}

/// ポリシー名を読む。SCHEMA 修飾は PostgreSQL と同じ構文エラー（`42601`）。
fn parse_policy_name(p: &mut Parser<'_>) -> Result<String, SqlSurfaceError> {
    if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
        return Err(SqlSurfaceError::unsupported(
            "policy names cannot be schema-qualified",
        ));
    }
    let name = p.expect_ident()?;
    crate::catalog::validate_identifier(&name)
        .map_err(|_| SqlSurfaceError::unsupported("invalid policy name"))?;
    Ok(name)
}

/// `ON <表>` を読む。解決されずに残った修飾名（`s.t`）は `0A000`（fail-closed）。
fn parse_on_table(p: &mut Parser<'_>) -> Result<String, SqlSurfaceError> {
    p.expect_ident_matching("ON")?;
    if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
        return Err(SqlSurfaceError::feature_not_supported(
            "unresolved schema-qualified table names are not supported",
        ));
    }
    let table = p.expect_ident()?;
    crate::catalog::validate_relation_key(&table)
        .map_err(|_| SqlSurfaceError::unsupported("invalid table name in policy DDL"))?;
    Ok(table)
}

/// `TO <ロール>[, ...]` の `TO` の後を読む。受理するのは `PUBLIC` だけで、名前付きロール・
/// `CURRENT_USER`・`SESSION_USER` は `0A000`（ロール機構が入るまで。無検証で保存しない）。
fn parse_roles(p: &mut Parser<'_>) -> Result<Vec<PolicyRole>, SqlSurfaceError> {
    let mut roles: Vec<PolicyRole> = Vec::new();
    loop {
        if p.peek_ident_matches("USING") || p.peek_ident_matches("WITH") {
            return Err(SqlSurfaceError::unsupported(
                "expected a role name after TO",
            ));
        }
        match p.advance() {
            Some(Token::Ident(w)) if w.eq_ignore_ascii_case("PUBLIC") => {
                if !roles.contains(&PolicyRole::Public) {
                    roles.push(PolicyRole::Public);
                }
            }
            Some(Token::Ident(_)) => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "policy roles other than PUBLIC are not supported yet",
                ));
            }
            _ => {
                return Err(SqlSurfaceError::unsupported(
                    "expected a role name after TO",
                ));
            }
        }
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            return Ok(roles);
        }
    }
}

/// `(<式>)` を読む。式は `CHECK` 本体と同じ文法。式の位置にリテラル・算術などの非述語が来た場合は
/// PostgreSQL と同じく `42804`（真偽値でない式）へ寄せ、それ以外の構文エラーは `42601` のまま返す。
fn parse_paren_expr(p: &mut Parser<'_>) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    p.expect_punct('(')?;
    let start = p.pos;
    let preds = match p.parse_check_body() {
        Ok(v) => v,
        Err(e) => {
            if e.wire_code() == "42601" {
                p.pos = start;
                if p.parse_value_expr(0).is_ok() && matches!(p.peek(), Some(Token::Punct(')'))) {
                    return Err(SqlSurfaceError::datatype_mismatch(
                        "argument of policy expression must be type boolean",
                    ));
                }
            }
            return Err(e);
        }
    };
    p.expect_punct(')')?;
    Ok(preds)
}

/// `FOR` と式の組み合わせの検査（PostgreSQL と同じ `42601`）。
fn check_command_clauses(
    command: PolicyCommand,
    has_using: bool,
    has_check: bool,
) -> Result<(), SqlSurfaceError> {
    if has_check && !command.allows_check() {
        return Err(SqlSurfaceError::unsupported(
            "WITH CHECK cannot be applied to SELECT or DELETE",
        ));
    }
    if has_using && !command.allows_using() {
        return Err(SqlSurfaceError::unsupported(
            "only WITH CHECK expression allowed for INSERT",
        ));
    }
    Ok(())
}

/// [`ValidatedCreatePolicy`] の構造検証本体（RLS-13、Issue #2274）。句の順序は PostgreSQL と同じ
/// `AS` → `FOR` → `TO` → `USING` → `WITH CHECK`（順序違い・重複は `42601`）。
pub(crate) fn validate_create_policy_tokens(
    tokens: &[Token],
) -> Result<ValidatedCreatePolicy, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("CREATE")?;
    p.expect_ident_matching("POLICY")?;
    let name = parse_policy_name(&mut p)?;
    let table = parse_on_table(&mut p)?;

    let mut permissive = true;
    if p.eat_word("AS") {
        if p.eat_word("PERMISSIVE") {
            permissive = true;
        } else if p.eat_word("RESTRICTIVE") {
            permissive = false;
        } else {
            return Err(SqlSurfaceError::unsupported(
                "expected PERMISSIVE or RESTRICTIVE after AS",
            ));
        }
    }
    let mut command = PolicyCommand::All;
    if p.eat_word("FOR") {
        command = if p.eat_word("ALL") {
            PolicyCommand::All
        } else if matches!(p.peek(), Some(Token::Keyword(Keyword::Select))) {
            // `SELECT` は構文語トークン（`Token::Ident` ではない）。
            p.advance();
            PolicyCommand::Select
        } else if p.eat_word("INSERT") {
            PolicyCommand::Insert
        } else if p.eat_word("UPDATE") {
            PolicyCommand::Update
        } else if p.eat_word("DELETE") {
            PolicyCommand::Delete
        } else {
            return Err(SqlSurfaceError::unsupported(
                "expected ALL, SELECT, INSERT, UPDATE or DELETE after FOR",
            ));
        };
    }
    let mut roles = vec![PolicyRole::Public];
    if p.eat_word("TO") {
        roles = parse_roles(&mut p)?;
    }
    let mut using = None;
    if p.eat_word("USING") {
        using = Some(parse_paren_expr(&mut p)?);
    }
    let mut check = None;
    if p.peek_ident_matches("WITH") && p.peek_ident_matches_at(1, "CHECK") {
        p.advance();
        p.advance();
        check = Some(parse_paren_expr(&mut p)?);
    }
    p.expect_end_of_statement()?;
    check_command_clauses(command, using.is_some(), check.is_some())?;
    Ok(ValidatedCreatePolicy {
        name,
        table,
        permissive,
        command,
        roles,
        using,
        check,
    })
}

/// [`ValidatedAlterPolicy`] の構造検証本体（RLS-13、Issue #2274）。`RENAME TO` は他の句と併用できない
/// （`42601`）。句が 1 つも無い形は PostgreSQL と同じく受理する（実行段で対象の存在だけを確かめる）。
/// 既存ポリシーの `FOR` との矛盾は実行段で検査する。
pub(crate) fn validate_alter_policy_tokens(
    tokens: &[Token],
) -> Result<ValidatedAlterPolicy, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("ALTER")?;
    p.expect_ident_matching("POLICY")?;
    let name = parse_policy_name(&mut p)?;
    let table = parse_on_table(&mut p)?;
    if p.eat_word("RENAME") {
        p.expect_ident_matching("TO")?;
        let new_name = parse_policy_name(&mut p)?;
        p.expect_end_of_statement()?;
        return Ok(ValidatedAlterPolicy {
            name,
            table,
            action: AlterPolicyAction::Rename(new_name),
        });
    }
    let roles = if p.eat_word("TO") {
        Some(parse_roles(&mut p)?)
    } else {
        None
    };
    let using = if p.eat_word("USING") {
        Some(parse_paren_expr(&mut p)?)
    } else {
        None
    };
    let check = if p.peek_ident_matches("WITH") && p.peek_ident_matches_at(1, "CHECK") {
        p.advance();
        p.advance();
        Some(parse_paren_expr(&mut p)?)
    } else {
        None
    };
    p.expect_end_of_statement()?;
    Ok(ValidatedAlterPolicy {
        name,
        table,
        action: AlterPolicyAction::Modify {
            roles,
            using,
            check,
        },
    })
}

/// [`ValidatedDropPolicy`] の構造検証本体（RLS-13、Issue #2274）。`CASCADE`／`RESTRICT` は受理する
/// （ポリシーに依存するオブジェクトは無いため結果は同じ）。`ON <表>` の省略は `42601`。
pub(crate) fn validate_drop_policy_tokens(
    tokens: &[Token],
) -> Result<ValidatedDropPolicy, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("DROP")?;
    p.expect_ident_matching("POLICY")?;
    let if_exists = p.consume_if_exists();
    let name = parse_policy_name(&mut p)?;
    let table = parse_on_table(&mut p)?;
    let _ = p.parse_optional_drop_behavior();
    p.expect_end_of_statement()?;
    Ok(ValidatedDropPolicy {
        name,
        table,
        if_exists,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn create(sql: &str) -> Result<ValidatedCreatePolicy, SqlSurfaceError> {
        validate_create_policy_tokens(&tokenize(sql).expect("lex"))
    }

    fn alter(sql: &str) -> Result<ValidatedAlterPolicy, SqlSurfaceError> {
        validate_alter_policy_tokens(&tokenize(sql).expect("lex"))
    }

    fn drop_(sql: &str) -> Result<ValidatedDropPolicy, SqlSurfaceError> {
        validate_drop_policy_tokens(&tokenize(sql).expect("lex"))
    }

    fn code(r: Result<impl std::fmt::Debug, SqlSurfaceError>) -> &'static str {
        r.expect_err("must be rejected").wire_code()
    }

    #[test]
    fn full_create_form_is_parsed() {
        let v = create(
            "CREATE POLICY p ON t AS RESTRICTIVE FOR UPDATE TO PUBLIC \
             USING (owner = 'a') WITH CHECK (n > 1)",
        )
        .expect("parse");
        assert_eq!((v.name.as_str(), v.table.as_str()), ("p", "t"));
        assert!(!v.permissive);
        assert_eq!(v.command, PolicyCommand::Update);
        assert_eq!(v.roles, vec![PolicyRole::Public]);
        assert!(v.using.is_some() && v.check.is_some());
    }

    #[test]
    fn defaults_are_permissive_all_public_without_expressions() {
        let v = create("CREATE POLICY p ON t").expect("parse");
        assert!(v.permissive);
        assert_eq!(v.command, PolicyCommand::All);
        assert_eq!(v.roles, vec![PolicyRole::Public]);
        assert!(v.using.is_none() && v.check.is_none());
    }

    #[test]
    fn each_command_is_accepted() {
        for (kw, cmd) in [
            ("ALL", PolicyCommand::All),
            ("SELECT", PolicyCommand::Select),
            ("INSERT", PolicyCommand::Insert),
            ("UPDATE", PolicyCommand::Update),
            ("DELETE", PolicyCommand::Delete),
        ] {
            let v = create(&format!("CREATE POLICY p ON t FOR {kw}")).expect(kw);
            assert_eq!(v.command, cmd, "{kw}");
        }
    }

    #[test]
    fn command_and_clause_conflicts_are_42601() {
        for sql in [
            "CREATE POLICY p ON t FOR SELECT WITH CHECK (n > 1)",
            "CREATE POLICY p ON t FOR DELETE WITH CHECK (n > 1)",
            "CREATE POLICY p ON t FOR INSERT USING (n > 1)",
        ] {
            assert_eq!(code(create(sql)), "42601", "{sql}");
        }
        assert!(create("CREATE POLICY p ON t FOR INSERT WITH CHECK (n > 1)").is_ok());
        assert!(create("CREATE POLICY p ON t FOR SELECT USING (n > 1)").is_ok());
    }

    #[test]
    fn malformed_forms_are_42601() {
        for sql in [
            "CREATE POLICY p",
            "CREATE POLICY ON t",
            "CREATE POLICY s.p ON t",
            "CREATE POLICY p ON t AS SOMETIMES",
            "CREATE POLICY p ON t FOR TRUNCATE",
            "CREATE POLICY p ON t USING (n > 1) USING (n > 2)",
            "CREATE POLICY p ON t USING n > 1",
            "CREATE POLICY p ON t USING ()",
            "CREATE POLICY p ON t USING (n > 1) FOR SELECT",
            "CREATE POLICY p ON t TO USING (n > 1)",
            "CREATE POLICY p ON t x",
        ] {
            assert_eq!(code(create(sql)), "42601", "{sql}");
        }
    }

    #[test]
    fn named_roles_are_0a000() {
        for sql in [
            "CREATE POLICY p ON t TO alice",
            "CREATE POLICY p ON t TO PUBLIC, alice",
            "CREATE POLICY p ON t TO CURRENT_USER",
            "CREATE POLICY p ON t TO SESSION_USER",
        ] {
            assert_eq!(code(create(sql)), "0A000", "{sql}");
        }
        assert!(create("CREATE POLICY p ON t TO public, PUBLIC").is_ok());
    }

    #[test]
    fn non_boolean_expressions_are_42804() {
        for sql in [
            "CREATE POLICY p ON t USING (1)",
            "CREATE POLICY p ON t USING ('x')",
            "CREATE POLICY p ON t USING (n + 1)",
        ] {
            assert_eq!(code(create(sql)), "42804", "{sql}");
        }
    }

    #[test]
    fn alter_forms() {
        let v = alter("ALTER POLICY p ON t RENAME TO q").expect("rename");
        assert_eq!(v.action, AlterPolicyAction::Rename("q".to_string()));
        let v = alter("ALTER POLICY p ON t TO PUBLIC USING (n > 1) WITH CHECK (n > 2)")
            .expect("modify");
        assert!(matches!(
            v.action,
            AlterPolicyAction::Modify {
                roles: Some(_),
                using: Some(_),
                check: Some(_)
            }
        ));
        let v = alter("ALTER POLICY p ON t").expect("no-op form");
        assert!(matches!(
            v.action,
            AlterPolicyAction::Modify {
                roles: None,
                using: None,
                check: None
            }
        ));
        for sql in [
            "ALTER POLICY p ON t RENAME TO q USING (n > 1)",
            "ALTER POLICY p ON t RENAME TO",
            "ALTER POLICY p ON t RENAME q",
            "ALTER POLICY p",
            "ALTER POLICY s.p ON t RENAME TO q",
            "ALTER POLICY p ON t RENAME TO s.q",
        ] {
            assert_eq!(code(alter(sql)), "42601", "{sql}");
        }
        assert_eq!(code(alter("ALTER POLICY p ON t TO alice")), "0A000");
    }

    #[test]
    fn drop_forms() {
        let v = drop_("DROP POLICY IF EXISTS p ON t CASCADE").expect("parse");
        assert!(v.if_exists);
        assert_eq!((v.name.as_str(), v.table.as_str()), ("p", "t"));
        assert!(
            !drop_("DROP POLICY p ON t RESTRICT")
                .expect("parse")
                .if_exists
        );
        assert_eq!(code(drop_("DROP POLICY p")), "42601");
        assert_eq!(code(drop_("DROP POLICY p ON t x")), "42601");
    }

    #[test]
    fn statement_classifiers() {
        let t = |s: &str| tokenize(s).expect("lex");
        assert!(is_create_policy_statement(&t("CREATE POLICY p ON t")));
        assert!(!is_create_policy_statement(&t(
            "CREATE TABLE policy (a int)"
        )));
        assert!(is_alter_policy_statement(&t(
            "alter policy p on t rename to q"
        )));
        assert!(!is_alter_policy_statement(&t(
            "ALTER TABLE policy ADD COLUMN a int"
        )));
        assert!(is_drop_policy_statement(&t("DROP POLICY p ON t")));
        assert!(!is_drop_policy_statement(&t("DROP TABLE policy")));
    }
}
