//! `CREATE ROLE`／`CREATE USER`／`DROP ROLE`／`DROP USER` の構文検証（SQL-39 のポインタ、Issue #1962。
//! 設計記録: `docs/design/role-privilege-model.md`）。
//!
//! 役割: トークン列からロール名・属性・パスワード（平文）・削除対象名だけを取り出す。カタログは一切
//! 照会しない（`policy` と同じ設計。存在・権限・検証子の生成は `sql::role_ddl` が権限ゲートの後で行う）。
//! 呼び出し元は `core.rs` の `parse_sql` 内の分岐だけ。
//!
//! メンバーシップ句 `IN ROLE`（`IN GROUP`）・`ROLE`（`USER`）・`ADMIN`（Issue #2232）は名前の列だけを取り出し、
//! 付与は `sql::role_ddl` がロール作成と同じ txn で行う（各句の重複は `42601`、件数は上限検証）。
//!
//! 受理しない形（すべて固定文言。パスワードのリテラル・名前以外のトークンはエラーに含めない）:
//! `SUPERUSER`／`BYPASSRLS`／`REPLICATION`（テナント境界を越える属性。`42501`）、`UNENCRYPTED PASSWORD`・
//! `VALID UNTIL`・`CONNECTION LIMIT`・`SYSID` など後続 Issue の担当（`0A000`）、
//! オプションの重複・矛盾・未知語・`$n` プレースホルダ・修飾名（`42601`）、予約名（`42939`）。
//!
//! パスワードの平文は [`RolePassword`] に閉じ込める。`Debug` は伏せ字で、`Display` は実装しない。

use super::*;
use crate::catalog::role::RoleAttributes;

/// `CREATE ROLE ... PASSWORD '<平文>'` の平文を運ぶ型（SQL-39、Issue #1962）。
///
/// `Debug` は常に伏せ字（`ParsedSql` やエラーの `{:?}` 経由でログに出ないため）。`Display` は実装しない。
/// `Drop` で内容を 0 で上書きする（best-effort。`unsafe` は使わない。字句段のトークン列は別に残る）。
#[derive(Clone, PartialEq, Eq)]
pub struct RolePassword(Vec<u8>);

impl RolePassword {
    fn new(s: &str) -> Self {
        Self(s.as_bytes().to_vec())
    }

    /// 平文のバイト列（検証子の生成にだけ使う）。
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for RolePassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RolePassword(<redacted>)")
    }
}

impl Drop for RolePassword {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            *b = 0;
        }
    }
}

/// 構造検証済みの `CREATE ROLE`／`CREATE USER`（SQL-39、Issue #1962）。実行は
/// `sql::role_ddl::execute_create_role`、呼び出し元は `core.rs` の `ParsedSql::CreateRole` 腕。
/// `attributes` は既定値（`CREATE USER` は `LOGIN`、`INHERIT` は既定で有効）を適用済み。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCreateRole {
    pub(crate) name: String,
    pub(crate) attributes: RoleAttributes,
    /// `None` はパスワード未指定または `PASSWORD NULL`。
    pub(crate) password: Option<RolePassword>,
    /// `IN ROLE a, ...`（`IN GROUP` も同じ）: 新しいロールを `a` のメンバーにする。
    pub(crate) in_roles: Vec<String>,
    /// `ROLE b, ...`（`USER` も同じ）: `b` を新しいロールのメンバーにする。
    pub(crate) member_roles: Vec<String>,
    /// `ADMIN c, ...`: `c` を新しいロールの ADMIN 付きメンバーにする。
    pub(crate) admin_roles: Vec<String>,
}

/// 構造検証済みの `DROP ROLE [IF EXISTS] <名前>[, ...]`（`DROP USER` も同じ。SQL-39、Issue #1962）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDropRole {
    pub(crate) names: Vec<String>,
    pub(crate) if_exists: bool,
}

fn ident_at(tokens: &[Token], i: usize, w: &str) -> bool {
    matches!(tokens.get(i), Some(Token::Ident(n)) if n.eq_ignore_ascii_case(w))
}

fn is_role_or_user(tokens: &[Token]) -> bool {
    ident_at(tokens, 1, "ROLE") || ident_at(tokens, 1, "USER")
}

/// 先頭 2 トークンが `CREATE ROLE`／`CREATE USER` か（`CREATE TABLE`／`FUNCTION` の分岐より前に振り分ける）。
pub(crate) fn is_create_role_statement(tokens: &[Token]) -> bool {
    ident_at(tokens, 0, "CREATE") && is_role_or_user(tokens)
}

/// 先頭 2 トークンが `DROP ROLE`／`DROP USER` か（汎用 `DROP`〔`DROP TABLE` 扱い〕より前に分岐する）。
pub(crate) fn is_drop_role_statement(tokens: &[Token]) -> bool {
    ident_at(tokens, 0, "DROP") && is_role_or_user(tokens)
}

/// ロール名を読む。修飾名・識別子でないトークンは固定文言の `42601`（トークンの内容を反射しない）。
fn parse_role_name(p: &mut Parser<'_>) -> Result<String, SqlSurfaceError> {
    match p.advance() {
        Some(Token::Ident(n)) => {
            let name = n.clone();
            crate::catalog::validate_identifier(&name)
                .map_err(|_| SqlSurfaceError::unsupported("invalid role name"))?;
            Ok(name)
        }
        Some(Token::QualifiedIdent { .. }) => Err(SqlSurfaceError::unsupported(
            "role names cannot be schema-qualified",
        )),
        _ => Err(SqlSurfaceError::unsupported("expected a role name")),
    }
}

/// 構文上は識別子だが、ロール名としては使えない語（PostgreSQL の予約語）。
fn is_reserved_syntax_word(name: &str) -> bool {
    ["current_user", "current_role", "session_user", "user"]
        .iter()
        .any(|w| name.eq_ignore_ascii_case(w))
}

/// 予約名（`pg_` 接頭辞・`public`・`none`）か。
fn is_reserved_role_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("pg_") || lower == "public" || lower == "none"
}

/// 同じ属性グループの 2 回目の指定（重複・矛盾）を拒否しつつ値を設定する。
fn set_once(slot: &mut Option<bool>, value: bool) -> Result<(), SqlSurfaceError> {
    if slot.is_some() {
        return Err(SqlSurfaceError::unsupported(
            "conflicting or redundant options",
        ));
    }
    *slot = Some(value);
    Ok(())
}

/// メンバーシップ句のロール名の列（`a, b, ...`）を読む。件数は上限検証してから積む。
fn parse_membership_names(p: &mut Parser<'_>) -> Result<Vec<String>, SqlSurfaceError> {
    let mut names = Vec::new();
    loop {
        if names.len() >= super::grant::MAX_GRANT_LIST_LEN {
            return Err(SqlSurfaceError::unsupported("too many roles in list"));
        }
        let name = parse_role_name(p)?;
        if is_reserved_syntax_word(&name) {
            return Err(SqlSurfaceError::unsupported("invalid role name"));
        }
        names.push(name);
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            return Ok(names);
        }
    }
}

/// メンバーシップ句の 2 回目以降の指定を拒否しつつ名前の列を読む（`conflicting or redundant options`）。
fn set_names_once(
    slot: &mut Option<Vec<String>>,
    p: &mut Parser<'_>,
) -> Result<(), SqlSurfaceError> {
    if slot.is_some() {
        return Err(SqlSurfaceError::unsupported(
            "conflicting or redundant options",
        ));
    }
    *slot = Some(parse_membership_names(p)?);
    Ok(())
}

/// `PASSWORD` の後ろ（`'<文字列>'` または `NULL`）を読む。エラー文言にリテラルを含めない。
fn parse_password_value(p: &mut Parser<'_>) -> Result<Option<RolePassword>, SqlSurfaceError> {
    match p.advance() {
        Some(Token::StringLiteral(s)) => Ok(Some(RolePassword::new(s))),
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("NULL") => Ok(None),
        _ => Err(SqlSurfaceError::unsupported(
            "expected a string literal or NULL after PASSWORD",
        )),
    }
}

/// [`ValidatedCreateRole`] の構造検証本体（SQL-39、Issue #1962）。
pub(crate) fn validate_create_role_tokens(
    tokens: &[Token],
) -> Result<ValidatedCreateRole, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("CREATE")?;
    let is_user = p.peek_ident_matches("USER");
    p.advance();
    let name = parse_role_name(&mut p)?;
    if is_reserved_syntax_word(&name) {
        return Err(SqlSurfaceError::unsupported("invalid role name"));
    }
    if is_reserved_role_name(&name) {
        return Err(SqlSurfaceError::ReservedRoleName {
            name: truncate_for_error(&name),
        });
    }
    let _ = p.eat_word("WITH");

    let mut login = None;
    let mut inherit = None;
    let mut createrole = None;
    let mut createdb = None;
    let mut superuser_group = None;
    let mut bypassrls_group = None;
    let mut replication_group = None;
    let mut password: Option<Option<RolePassword>> = None;
    let mut in_roles: Option<Vec<String>> = None;
    let mut member_roles: Option<Vec<String>> = None;
    let mut admin_roles: Option<Vec<String>> = None;

    while let Some(tok) = p.peek() {
        let word = match tok {
            Token::Ident(w) => w.to_ascii_uppercase(),
            Token::Punct(';') => break,
            _ => return Err(SqlSurfaceError::unsupported("unrecognized role option")),
        };
        p.advance();
        match word.as_str() {
            "LOGIN" => set_once(&mut login, true)?,
            "NOLOGIN" => set_once(&mut login, false)?,
            "INHERIT" => set_once(&mut inherit, true)?,
            "NOINHERIT" => set_once(&mut inherit, false)?,
            "CREATEROLE" => set_once(&mut createrole, true)?,
            "NOCREATEROLE" => set_once(&mut createrole, false)?,
            "CREATEDB" => set_once(&mut createdb, true)?,
            "NOCREATEDB" => set_once(&mut createdb, false)?,
            // 既定どおりなので何もしない（重複・矛盾だけ検出する）。
            "NOSUPERUSER" => set_once(&mut superuser_group, false)?,
            "NOBYPASSRLS" => set_once(&mut bypassrls_group, false)?,
            "NOREPLICATION" => set_once(&mut replication_group, false)?,
            // テナント境界を越える属性は与えられない（RLS-12 のポインタ）。
            "SUPERUSER" | "BYPASSRLS" | "REPLICATION" => {
                return Err(SqlSurfaceError::InsufficientPrivilege);
            }
            "PASSWORD" | "ENCRYPTED" => {
                if word == "ENCRYPTED" && !p.eat_word("PASSWORD") {
                    return Err(SqlSurfaceError::unsupported("unrecognized role option"));
                }
                if password.is_some() {
                    return Err(SqlSurfaceError::unsupported(
                        "conflicting or redundant options",
                    ));
                }
                password = Some(parse_password_value(&mut p)?);
            }
            "UNENCRYPTED" => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "UNENCRYPTED PASSWORD is not supported",
                ));
            }
            "IN" => {
                if !(p.eat_word("ROLE") || p.eat_word("GROUP")) {
                    return Err(SqlSurfaceError::unsupported("unrecognized role option"));
                }
                set_names_once(&mut in_roles, &mut p)?;
            }
            "ROLE" | "USER" => set_names_once(&mut member_roles, &mut p)?,
            "ADMIN" => set_names_once(&mut admin_roles, &mut p)?,
            // 認証側の意味づけ（#2233）は後続 Issue の担当。
            "SYSID" | "VALID" | "CONNECTION" => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "this role option is not supported yet",
                ));
            }
            _ => return Err(SqlSurfaceError::unsupported("unrecognized role option")),
        }
    }
    p.expect_end_of_statement()?;

    Ok(ValidatedCreateRole {
        name,
        attributes: RoleAttributes {
            login: login.unwrap_or(is_user),
            inherit: inherit.unwrap_or(true),
            createrole: createrole.unwrap_or(false),
            createdb: createdb.unwrap_or(false),
        },
        password: password.flatten(),
        in_roles: in_roles.unwrap_or_default(),
        member_roles: member_roles.unwrap_or_default(),
        admin_roles: admin_roles.unwrap_or_default(),
    })
}

/// [`ValidatedDropRole`] の構造検証本体（SQL-39、Issue #1962）。`DROP ROLE|USER [IF EXISTS] a[, b]...`。
pub(crate) fn validate_drop_role_tokens(
    tokens: &[Token],
) -> Result<ValidatedDropRole, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("DROP")?;
    p.advance();
    let if_exists = p.consume_if_exists();
    let mut names = Vec::new();
    loop {
        let name = parse_role_name(&mut p)?;
        if is_reserved_syntax_word(&name) {
            return Err(SqlSurfaceError::unsupported("invalid role name"));
        }
        names.push(name);
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            break;
        }
    }
    p.expect_end_of_statement()?;
    Ok(ValidatedDropRole { names, if_exists })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn create(sql: &str) -> Result<ValidatedCreateRole, SqlSurfaceError> {
        validate_create_role_tokens(&tokenize(sql).expect("lex"))
    }

    fn drop_(sql: &str) -> Result<ValidatedDropRole, SqlSurfaceError> {
        validate_drop_role_tokens(&tokenize(sql).expect("lex"))
    }

    fn code(r: Result<impl std::fmt::Debug, SqlSurfaceError>) -> &'static str {
        r.expect_err("must be rejected").wire_code()
    }

    #[test]
    fn create_role_defaults_to_nologin_inherit() {
        let v = create("CREATE ROLE r1").expect("ok");
        assert_eq!(v.name, "r1");
        assert!(!v.attributes.login && v.attributes.inherit);
        assert!(!v.attributes.createrole && !v.attributes.createdb);
        assert!(v.password.is_none());
    }

    #[test]
    fn create_user_defaults_to_login() {
        assert!(create("CREATE USER u1").expect("ok").attributes.login);
        assert!(
            !create("CREATE USER u1 NOLOGIN")
                .expect("ok")
                .attributes
                .login
        );
        assert!(
            create("CREATE ROLE r WITH LOGIN CREATEDB NOINHERIT CREATEROLE;")
                .map(|v| v.attributes.login
                    && v.attributes.createdb
                    && !v.attributes.inherit
                    && v.attributes.createrole)
                .expect("ok")
        );
    }

    #[test]
    fn password_forms() {
        let v = create("CREATE ROLE r PASSWORD 'pw-marker'").expect("ok");
        assert_eq!(
            v.password.as_ref().map(|p| p.as_bytes()),
            Some(&b"pw-marker"[..])
        );
        assert!(create("CREATE ROLE r ENCRYPTED PASSWORD 'x'")
            .expect("ok")
            .password
            .is_some());
        assert!(create("CREATE ROLE r PASSWORD NULL")
            .expect("ok")
            .password
            .is_none());
        assert_eq!(
            code(create("CREATE ROLE r UNENCRYPTED PASSWORD 'x'")),
            "0A000"
        );
        let with_param =
            crate::sql::lexer::tokenize_with_params("CREATE ROLE r PASSWORD $1").expect("lex");
        assert_eq!(code(validate_create_role_tokens(&with_param)), "42601");
        assert_eq!(
            code(create("CREATE ROLE r PASSWORD 'a' PASSWORD 'b'")),
            "42601"
        );
        assert_eq!(code(create("CREATE ROLE r PASSWORD")), "42601");
    }

    #[test]
    fn password_never_appears_in_debug_or_errors() {
        let marker = "ZZ-secret-marker-ZZ";
        let v = create(&format!("CREATE ROLE r PASSWORD '{marker}'")).expect("ok");
        assert!(!format!("{v:?}").contains(marker));
        for sql in [
            format!("CREATE ROLE r PASSWORD '{marker}' BOGUS"),
            format!("CREATE ROLE r PASSWORD '{marker}' '{marker}'"),
            format!("CREATE ROLE r PASSWORD '{marker}' LOGIN NOLOGIN"),
            format!("CREATE ROLE '{marker}'"),
            format!("CREATE ROLE r SUPERUSER PASSWORD '{marker}'"),
            format!("CREATE ROLE r UNENCRYPTED PASSWORD '{marker}'"),
            format!("CREATE ROLE r PASSWORD '{marker}' VALID UNTIL '2030-01-01'"),
            format!("CREATE ROLE r PASSWORD '{marker}'; DROP TABLE t"),
        ] {
            let e = create(&sql).expect_err("rejected");
            assert!(!e.to_string().contains(marker), "{e}");
            assert!(!format!("{e:?}").contains(marker), "{e:?}");
            assert!(!e.client_message().contains(marker));
        }
    }

    #[test]
    fn boundary_crossing_attributes_are_forbidden() {
        for kw in ["SUPERUSER", "BYPASSRLS", "REPLICATION"] {
            assert_eq!(code(create(&format!("CREATE ROLE r {kw}"))), "42501");
        }
        for kw in ["NOSUPERUSER", "NOBYPASSRLS", "NOREPLICATION"] {
            assert!(create(&format!("CREATE ROLE r {kw}")).is_ok());
        }
    }

    #[test]
    fn membership_clauses_are_parsed_and_duplicates_rejected() {
        let v = create("CREATE ROLE r IN ROLE a, b ROLE c ADMIN d, e").expect("parse");
        assert_eq!(v.in_roles, ["a", "b"]);
        assert_eq!(v.member_roles, ["c"]);
        assert_eq!(v.admin_roles, ["d", "e"]);
        let v = create("CREATE USER u IN GROUP g USER x").expect("parse");
        assert_eq!((v.in_roles.len(), v.member_roles.len()), (1, 1));
        for bad in [
            "IN ROLE a IN ROLE b",
            "ROLE a ROLE b",
            "ADMIN a ADMIN b",
            "IN a",
            "IN ROLE",
            "ROLE current_user",
            "IN ROLE a,",
        ] {
            assert_eq!(
                code(create(&format!("CREATE ROLE r {bad}"))),
                "42601",
                "{bad}"
            );
        }
    }

    #[test]
    fn unsupported_and_malformed_options() {
        for kw in ["SYSID 1", "VALID UNTIL 'infinity'", "CONNECTION LIMIT 1"] {
            assert_eq!(
                code(create(&format!("CREATE ROLE r {kw}"))),
                "0A000",
                "{kw}"
            );
        }
        for sql in [
            "CREATE ROLE r LOGIN NOLOGIN",
            "CREATE ROLE r LOGIN LOGIN",
            "CREATE ROLE r BOGUS",
            "CREATE ROLE",
            "CREATE ROLE s.r",
            "CREATE ROLE r LOGIN, NOLOGIN",
            "CREATE ROLE current_user",
            "CREATE ROLE r NOSUPERUSER NOSUPERUSER",
        ] {
            assert_eq!(code(create(sql)), "42601", "{sql}");
        }
    }

    #[test]
    fn reserved_names_are_rejected_with_42939() {
        for n in ["pg_x", "PG_Admin", "public", "none"] {
            assert_eq!(code(create(&format!("CREATE ROLE {n}"))), "42939", "{n}");
        }
    }

    #[test]
    fn drop_role_forms() {
        let v = drop_("DROP ROLE IF EXISTS a, b").expect("ok");
        assert!(v.if_exists);
        assert_eq!(v.names, vec!["a".to_string(), "b".to_string()]);
        assert!(!drop_("DROP USER a;").expect("ok").if_exists);
        assert_eq!(code(drop_("DROP ROLE")), "42601");
        assert_eq!(code(drop_("DROP ROLE a,")), "42601");
        assert_eq!(code(drop_("DROP ROLE a b")), "42601");
        assert_eq!(code(drop_("DROP ROLE s.a")), "42601");
    }

    #[test]
    fn statement_detection() {
        let t = |s: &str| tokenize(s).expect("lex");
        assert!(is_create_role_statement(&t("create user u")));
        assert!(is_drop_role_statement(&t("drop role r")));
        assert!(!is_create_role_statement(&t("CREATE TABLE t (a INT)")));
        assert!(!is_drop_role_statement(&t("DROP TABLE t")));
    }
}
