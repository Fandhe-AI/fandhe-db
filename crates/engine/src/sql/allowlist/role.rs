//! `CREATE ROLE`／`CREATE USER`／`DROP ROLE`／`DROP USER` の構文検証（SQL-39 のポインタ、Issue #1962。
//! 設計記録: `docs/design/role-privilege-model.md`）。
//!
//! 役割: トークン列からロール名・属性・パスワード（平文）・削除対象名だけを取り出す。カタログは一切
//! 照会しない（`policy` と同じ設計。存在・権限・検証子の生成は `sql::role_ddl` が権限ゲートの後で行う）。
//! 呼び出し元は `core.rs` の `parse_sql` 内の分岐だけ。
//!
//! 受理しない形（すべて固定文言。パスワードのリテラル・名前以外のトークンはエラーに含めない）:
//! `SUPERUSER`／`BYPASSRLS`／`REPLICATION`（テナント境界を越える属性。`42501`）、`UNENCRYPTED PASSWORD`・
//! `IN ROLE`・`ROLE`・`ADMIN`・`VALID UNTIL`・`CONNECTION LIMIT` など後続 Issue の担当（`0A000`）、
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

    let opts = parse_role_options(&mut p, false)?;
    p.expect_end_of_statement()?;

    Ok(ValidatedCreateRole {
        name,
        attributes: RoleAttributes {
            login: opts.login.unwrap_or(is_user),
            inherit: opts.inherit.unwrap_or(true),
            createrole: opts.createrole.unwrap_or(false),
            createdb: opts.createdb.unwrap_or(false),
        },
        password: opts.password.flatten(),
    })
}

/// `CREATE ROLE`／`ALTER ROLE` が共有するオプション列の解析結果。`None` は「指定なし」で、既定値を当てるのは
/// CREATE 側、「変更なし」と読むのは ALTER 側（拒否規則を 1 か所に保つため解析は共有する。Issue #1963）。
#[derive(Default)]
struct ParsedRoleOptions {
    login: Option<bool>,
    inherit: Option<bool>,
    createrole: Option<bool>,
    createdb: Option<bool>,
    /// `None` は未指定、`Some(None)` は `PASSWORD NULL`。
    password: Option<Option<RolePassword>>,
}

/// ロールのオプション列（`LOGIN`・`PASSWORD '...'` など）を文末または `;` まで読む。`for_alter` が真のとき
/// CREATE 専用語（`ROLE`・`USER`・`ADMIN`・`SYSID`）は `42601`、ロール単位の設定・`IN DATABASE`（`IN`・`SET`・
/// `RESET`）は `0A000`。偽（CREATE）のときは従来どおり後続 Issue の担当語をまとめて `0A000`。
fn parse_role_options(
    p: &mut Parser<'_>,
    for_alter: bool,
) -> Result<ParsedRoleOptions, SqlSurfaceError> {
    let mut out = ParsedRoleOptions::default();
    let mut superuser_group = None;
    let mut bypassrls_group = None;
    let mut replication_group = None;

    while let Some(tok) = p.peek() {
        let word = match tok {
            Token::Ident(w) => w.to_ascii_uppercase(),
            Token::Punct(';') => break,
            _ => return Err(SqlSurfaceError::unsupported("unrecognized role option")),
        };
        p.advance();
        match word.as_str() {
            "LOGIN" => set_once(&mut out.login, true)?,
            "NOLOGIN" => set_once(&mut out.login, false)?,
            "INHERIT" => set_once(&mut out.inherit, true)?,
            "NOINHERIT" => set_once(&mut out.inherit, false)?,
            "CREATEROLE" => set_once(&mut out.createrole, true)?,
            "NOCREATEROLE" => set_once(&mut out.createrole, false)?,
            "CREATEDB" => set_once(&mut out.createdb, true)?,
            "NOCREATEDB" => set_once(&mut out.createdb, false)?,
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
                if out.password.is_some() {
                    return Err(SqlSurfaceError::unsupported(
                        "conflicting or redundant options",
                    ));
                }
                out.password = Some(parse_password_value(p)?);
            }
            "UNENCRYPTED" => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "UNENCRYPTED PASSWORD is not supported",
                ));
            }
            // ALTER ROLE のロール単位の設定・`IN DATABASE`（状態に依存しない拒否）。
            "IN" | "SET" | "RESET" if for_alter => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "per-role settings are not supported",
                ));
            }
            // CREATE 専用語は ALTER では構文エラー。
            "ROLE" | "USER" | "ADMIN" | "SYSID" if for_alter => {
                return Err(SqlSurfaceError::unsupported("unrecognized role option"));
            }
            // メンバーシップ（#2232）・認証側の意味づけ（#2233）は後続 Issue の担当。
            "IN" | "ROLE" | "USER" | "ADMIN" | "SYSID" | "VALID" | "CONNECTION" => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "this role option is not supported yet",
                ));
            }
            _ => return Err(SqlSurfaceError::unsupported("unrecognized role option")),
        }
    }
    Ok(out)
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

/// `ALTER ROLE` のオプション変更。`None` は「変更なし」（Issue #1963）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleOptionPatch {
    pub(crate) login: Option<bool>,
    pub(crate) inherit: Option<bool>,
    pub(crate) createrole: Option<bool>,
    pub(crate) createdb: Option<bool>,
    /// `None` は変更なし、`Some(None)` は `PASSWORD NULL`（検証子を空にする）。
    pub(crate) password: Option<Option<RolePassword>>,
}

/// `ALTER ROLE` の変更内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterRoleAction {
    /// 属性・パスワードの変更（空の指定は存在確認だけの no-op）。
    Options(RoleOptionPatch),
    /// `RENAME TO <新しい名前>`。
    Rename { new_name: String },
}

/// 構造検証済みの `ALTER ROLE`／`ALTER USER`（SQL-39、Issue #1963）。実行は
/// `sql::role_ddl::execute_alter_role`、呼び出し元は `core.rs` の `ParsedSql::AlterRole` 腕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAlterRole {
    pub(crate) name: String,
    pub(crate) action: AlterRoleAction,
}

/// `SET ROLE` の対象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetRoleTarget {
    /// 利用者が書いたロール名（存在・認可は実行段が判定する）。
    Role(String),
    /// `NONE`（セッションのロールへ戻す）。
    None,
}

/// 構造検証済みの `SET [SESSION] ROLE ...`／`RESET ROLE`（SQL-39、Issue #1963）。実行は
/// `sql::role_ddl::execute_set_role`、呼び出し元は `core.rs` の `ParsedSql::SetRole` 腕。
/// `target` が `None` のときセッションのロールへ戻す（`RESET ROLE`・`SET ROLE NONE`・`SET role TO DEFAULT`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSetRole {
    pub(crate) target: SetRoleTarget,
    /// 文が `RESET ROLE` か（コマンドタグ `RESET`）。
    pub(crate) is_reset_statement: bool,
}

/// 先頭 2 トークンが `ALTER ROLE`／`ALTER USER` か（汎用 `ALTER`〔`ALTER TABLE` 扱い〕より前に分岐する）。
pub(crate) fn is_alter_role_statement(tokens: &[Token]) -> bool {
    ident_at(tokens, 0, "ALTER") && is_role_or_user(tokens)
}

/// `SET [SESSION|LOCAL] ROLE ...` の頭か。`SET LOCAL` も真にして [`validate_set_role_tokens`] が
/// `0A000` で拒否する（別経路の `42601` へ流さない）。`SET SESSION AUTHORIZATION` は含まない。
pub(crate) fn is_set_role_statement(tokens: &[Token]) -> bool {
    if !ident_at(tokens, 0, "SET") {
        return false;
    }
    ident_at(tokens, 1, "ROLE")
        || ((ident_at(tokens, 1, "SESSION") || ident_at(tokens, 1, "LOCAL"))
            && ident_at(tokens, 2, "ROLE"))
}

/// 文が `RESET ROLE`（末尾 `;` 可）か。厳密一致のみ真。
pub(crate) fn is_reset_role_statement(tokens: &[Token]) -> bool {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("RESET").is_ok()
        && p.expect_ident_matching("ROLE").is_ok()
        && p.expect_end_of_statement().is_ok()
}

/// [`ValidatedAlterRole`] の構造検証本体（SQL-39、Issue #1963）。カタログは照会しない。
///
/// 対象に `CURRENT_USER`／`CURRENT_ROLE`／`SESSION_USER`・`ALL` を取る形は `0A000`（状態に依存しない拒否）。
pub(crate) fn validate_alter_role_tokens(
    tokens: &[Token],
) -> Result<ValidatedAlterRole, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("ALTER")?;
    p.advance();
    let name = parse_role_name(&mut p)?;
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "current_user" | "current_role" | "session_user"
    ) || lower == "all"
    {
        return Err(SqlSurfaceError::feature_not_supported(
            "this ALTER ROLE target form is not supported",
        ));
    }
    if is_reserved_syntax_word(&name) {
        return Err(SqlSurfaceError::unsupported("invalid role name"));
    }
    if p.eat_word("RENAME") {
        if !p.eat_word("TO") {
            return Err(SqlSurfaceError::unsupported("expected TO after RENAME"));
        }
        let new_name = parse_role_name(&mut p)?;
        if is_reserved_syntax_word(&new_name) {
            return Err(SqlSurfaceError::unsupported("invalid role name"));
        }
        if is_reserved_role_name(&new_name) {
            return Err(SqlSurfaceError::ReservedRoleName {
                name: truncate_for_error(&new_name),
            });
        }
        p.expect_end_of_statement()?;
        return Ok(ValidatedAlterRole {
            name,
            action: AlterRoleAction::Rename { new_name },
        });
    }
    let _ = p.eat_word("WITH");
    let opts = parse_role_options(&mut p, true)?;
    p.expect_end_of_statement()?;
    Ok(ValidatedAlterRole {
        name,
        action: AlterRoleAction::Options(RoleOptionPatch {
            login: opts.login,
            inherit: opts.inherit,
            createrole: opts.createrole,
            createdb: opts.createdb,
            password: opts.password,
        }),
    })
}

/// [`ValidatedSetRole`] の構造検証本体（`SET [SESSION] ROLE {名前|'名前'|NONE}`・
/// `SET [SESSION] role {TO|=} {名前|'名前'|NONE|DEFAULT}`。SQL-39、Issue #1963）。
/// `SET LOCAL` は `0A000`、`$n`・修飾名・その他は `42601`。
pub(crate) fn validate_set_role_tokens(
    tokens: &[Token],
) -> Result<ValidatedSetRole, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("SET")?;
    if p.eat_word("LOCAL") {
        return Err(SqlSurfaceError::feature_not_supported(
            "SET LOCAL is not supported",
        ));
    }
    let _ = p.eat_word("SESSION");
    p.expect_ident_matching("ROLE")?;
    let with_assign = if p.eat_word("TO") {
        true
    } else if matches!(p.peek(), Some(Token::Punct('='))) {
        p.advance();
        true
    } else {
        false
    };
    let target = match p.advance() {
        Some(Token::Ident(n)) if n.eq_ignore_ascii_case("NONE") => SetRoleTarget::None,
        Some(Token::Ident(n)) if with_assign && n.eq_ignore_ascii_case("DEFAULT") => {
            SetRoleTarget::None
        }
        Some(Token::Ident(n)) => {
            let name = n.clone();
            crate::catalog::validate_identifier(&name)
                .map_err(|_| SqlSurfaceError::unsupported("invalid role name"))?;
            // `DEFAULT` は予約語（PostgreSQL でも `SET ROLE DEFAULT` は構文エラー。`TO`／`=` 形のみ受理）。
            if is_reserved_syntax_word(&name) || name.eq_ignore_ascii_case("DEFAULT") {
                return Err(SqlSurfaceError::unsupported("invalid role name"));
            }
            SetRoleTarget::Role(name)
        }
        Some(Token::StringLiteral(s)) if s.eq_ignore_ascii_case("none") => SetRoleTarget::None,
        Some(Token::StringLiteral(s)) => {
            let name = s.clone();
            crate::catalog::validate_identifier(&name)
                .map_err(|_| SqlSurfaceError::unsupported("invalid role name"))?;
            SetRoleTarget::Role(name)
        }
        Some(Token::QualifiedIdent { .. }) => {
            return Err(SqlSurfaceError::unsupported(
                "role names cannot be schema-qualified",
            ))
        }
        _ => return Err(SqlSurfaceError::unsupported("expected a role name")),
    };
    p.expect_end_of_statement()?;
    Ok(ValidatedSetRole {
        target,
        is_reset_statement: false,
    })
}

/// `RESET ROLE` を [`ValidatedSetRole`]（`NONE` 相当・タグ `RESET`）にする。
pub(crate) fn reset_role_statement() -> ValidatedSetRole {
    ValidatedSetRole {
        target: SetRoleTarget::None,
        is_reset_statement: true,
    }
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
    fn unsupported_and_malformed_options() {
        for kw in [
            "IN ROLE x",
            "IN GROUP x",
            "ROLE x",
            "USER x",
            "ADMIN x",
            "SYSID 1",
            "VALID UNTIL 'infinity'",
            "CONNECTION LIMIT 1",
        ] {
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

    fn alter(sql: &str) -> Result<ValidatedAlterRole, SqlSurfaceError> {
        validate_alter_role_tokens(&tokenize(sql).expect("lex"))
    }

    fn set_role(sql: &str) -> Result<ValidatedSetRole, SqlSurfaceError> {
        validate_set_role_tokens(&tokenize(sql).expect("lex"))
    }

    #[test]
    fn alter_role_option_patch() {
        let v = alter("ALTER ROLE r WITH NOLOGIN CREATEDB").expect("ok");
        assert_eq!(v.name, "r");
        let AlterRoleAction::Options(p) = v.action else {
            panic!("options");
        };
        assert_eq!(p.login, Some(false));
        assert_eq!(p.createdb, Some(true));
        assert_eq!(p.inherit, None);
        assert_eq!(p.createrole, None);
        assert!(p.password.is_none());
        let AlterRoleAction::Options(p) = alter("ALTER USER r PASSWORD NULL;").expect("ok").action
        else {
            panic!("options");
        };
        assert_eq!(p.password, Some(None));
        let AlterRoleAction::Options(p) = alter("ALTER ROLE r").expect("ok").action else {
            panic!("options");
        };
        assert_eq!(p, RoleOptionPatch::default());
    }

    #[test]
    fn alter_role_rename_and_rejections() {
        let v = alter("ALTER ROLE a RENAME TO b").expect("ok");
        assert_eq!(
            v.action,
            AlterRoleAction::Rename {
                new_name: "b".to_string()
            }
        );
        assert_eq!(code(alter("ALTER ROLE a RENAME TO pg_x")), "42939");
        assert_eq!(code(alter("ALTER ROLE a RENAME TO public")), "42939");
        assert_eq!(code(alter("ALTER ROLE a RENAME TO current_user")), "42601");
        assert_eq!(code(alter("ALTER ROLE a RENAME b")), "42601");
        for sql in [
            "ALTER ROLE r SUPERUSER",
            "ALTER ROLE r BYPASSRLS",
            "ALTER ROLE r REPLICATION",
        ] {
            assert_eq!(code(alter(sql)), "42501", "{sql}");
        }
        for sql in [
            "ALTER ROLE r VALID UNTIL 'infinity'",
            "ALTER ROLE r CONNECTION LIMIT 1",
            "ALTER ROLE r SET search_path = x",
            "ALTER ROLE r RESET ALL",
            "ALTER ROLE r IN DATABASE d SET x = 1",
            "ALTER ROLE ALL SET x = 1",
            "ALTER ROLE current_user NOLOGIN",
            "ALTER ROLE session_user NOLOGIN",
        ] {
            assert_eq!(code(alter(sql)), "0A000", "{sql}");
        }
        for sql in [
            "ALTER ROLE r ROLE x",
            "ALTER ROLE r ADMIN x",
            "ALTER ROLE r LOGIN NOLOGIN",
            "ALTER ROLE r BOGUS",
            "ALTER ROLE",
            "ALTER ROLE s.r NOLOGIN",
            "ALTER ROLE r PASSWORD",
        ] {
            assert_eq!(code(alter(sql)), "42601", "{sql}");
        }
    }

    #[test]
    fn alter_role_password_never_appears_in_debug_or_errors() {
        let marker = "ZZ-alter-secret-ZZ";
        let v = alter(&format!("ALTER ROLE r PASSWORD '{marker}'")).expect("ok");
        assert!(!format!("{v:?}").contains(marker));
        for sql in [
            format!("ALTER ROLE r PASSWORD '{marker}' BOGUS"),
            format!("ALTER ROLE r PASSWORD '{marker}' SUPERUSER"),
            format!("ALTER ROLE r PASSWORD '{marker}' VALID UNTIL '2030-01-01'"),
            format!("ALTER ROLE r PASSWORD '{marker}' PASSWORD '{marker}'"),
        ] {
            let e = alter(&sql).expect_err("rejected");
            assert!(!e.to_string().contains(marker), "{e}");
            assert!(!format!("{e:?}").contains(marker), "{e:?}");
        }
    }

    #[test]
    fn set_role_forms() {
        assert_eq!(
            set_role("SET ROLE r1").expect("ok").target,
            SetRoleTarget::Role("r1".to_string())
        );
        assert_eq!(
            set_role("SET SESSION ROLE 'r1';").expect("ok").target,
            SetRoleTarget::Role("r1".to_string())
        );
        assert_eq!(
            set_role("SET role TO r1").expect("ok").target,
            SetRoleTarget::Role("r1".to_string())
        );
        assert_eq!(
            set_role("SET role = r1").expect("ok").target,
            SetRoleTarget::Role("r1".to_string())
        );
        for sql in [
            "SET ROLE NONE",
            "SET ROLE 'none'",
            "SET SESSION ROLE none",
            "SET role TO DEFAULT",
            "SET role = DEFAULT",
        ] {
            let v = set_role(sql).expect("ok");
            assert_eq!(v.target, SetRoleTarget::None, "{sql}");
            assert!(!v.is_reset_statement);
        }
        assert_eq!(code(set_role("SET LOCAL ROLE r")), "0A000");
        for sql in [
            "SET ROLE",
            "SET ROLE DEFAULT",
            "SET ROLE s.r",
            "SET ROLE r r2",
            "SET ROLE current_user",
            "SET ROLE 'bad name'",
            "SET ROLE 1",
        ] {
            assert_eq!(code(set_role(sql)), "42601", "{sql}");
        }
        let with_param = crate::sql::lexer::tokenize_with_params("SET ROLE $1").expect("lex");
        assert_eq!(code(validate_set_role_tokens(&with_param)), "42601");
    }

    #[test]
    fn set_role_detection() {
        let t = |s: &str| tokenize(s).expect("lex");
        assert!(is_set_role_statement(&t("set role x")));
        assert!(is_set_role_statement(&t("SET SESSION ROLE x")));
        assert!(is_set_role_statement(&t("SET LOCAL ROLE x")));
        assert!(!is_set_role_statement(&t("SET SESSION AUTHORIZATION x")));
        assert!(!is_set_role_statement(&t("SET search_mode = x")));
        assert!(is_reset_role_statement(&t("RESET ROLE;")));
        assert!(!is_reset_role_statement(&t("RESET ROLE x")));
        assert!(!is_reset_role_statement(&t("RESET row_security")));
        assert!(is_alter_role_statement(&t("alter user u")));
        assert!(!is_alter_role_statement(&t(
            "ALTER TABLE t ADD COLUMN c INT"
        )));
    }
}
