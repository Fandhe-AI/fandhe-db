//! `GRANT`／`REVOKE` の構文検証（SQL-40、Issue #1965。設計記録: `docs/design/role-privilege-model.md` §8）。
//!
//! 役割: トークン列から権限・対象・付与先・オプションの構造だけを取り出す。カタログは一切照会しない
//! （`policy` と同じ設計。ロール・表・列の存在と付与権限の判定は `sql::grant_ddl` が権限ゲートの後で
//! 行う）。呼び出し元は `core.rs` の `parse_sql` 内の分岐だけ。
//!
//! 受理する形:
//! - 権限形: `GRANT <権限>[, ...] ON [TABLE] <表>[, ...] TO <付与先>[, ...] [WITH GRANT OPTION]`・
//!   `REVOKE [GRANT OPTION FOR] <権限>[, ...] ON [TABLE] <表>[, ...] FROM <付与先>[, ...]
//!   [CASCADE|RESTRICT]`。権限は表の `SELECT`・`INSERT`・`UPDATE`・`DELETE`・`TRUNCATE`・`REFERENCES`・
//!   `TRIGGER`・`MAINTAIN` と `ALL [PRIVILEGES]`、列リスト `<権限> (列, ...)`。
//! - ロール形: `GRANT <ロール>[, ...] TO <付与先>[, ...] [WITH {ADMIN|INHERIT|SET} {OPTION|TRUE|FALSE}[, ...]]`・
//!   `REVOKE [{ADMIN|INHERIT|SET} OPTION FOR] <ロール>[, ...] FROM <付与先>[, ...] [CASCADE|RESTRICT]`。
//!   構造化だけを行い、実行（メンバーシップの保存・循環拒否・継承）は #2232 で実装済み（`sql::grant_ddl`）。
//!
//! 構文段で拒否する形: 句の欠落・重複・順序違い・未知の権限語・空の列リスト・末尾の余り（`42601`）、
//! 対象の種別に許されない権限・PUBLIC への `WITH GRANT OPTION`（`0LP01`）、範囲外の対象種別
//! （`SCHEMA`・`SEQUENCE`・`FUNCTION`・`ALL ... IN SCHEMA`・`DATABASE` 等）・`GRANTED BY`・解決されずに
//! 残った修飾名（`0A000`。状態に依存しない拒否のため存在オラクルにならない）。
//!
//! 列挙の件数は [`MAX_GRANT_LIST_LEN`] で検証してから確保する（無制限確保の防止）。

use super::*;
use crate::catalog::privilege::{
    PRIV_CREATE, PRIV_DELETE, PRIV_EXECUTE, PRIV_INSERT, PRIV_MAINTAIN, PRIV_REFERENCES,
    PRIV_SELECT, PRIV_TRIGGER, PRIV_TRUNCATE, PRIV_UPDATE, PRIV_USAGE,
};

/// 1 つのリスト（権限・列・表・付与先・ロール）の要素数の上限（実装既定値）。
pub(crate) const MAX_GRANT_LIST_LEN: usize = 256;

/// 表に許す権限ビットの全体（`ALL [PRIVILEGES]` の展開先。`PrivilegeObject::Table` の許可集合と一致させる）。
const TABLE_PRIVILEGES: u16 = PRIV_SELECT
    | PRIV_INSERT
    | PRIV_UPDATE
    | PRIV_DELETE
    | PRIV_TRUNCATE
    | PRIV_REFERENCES
    | PRIV_TRIGGER
    | PRIV_MAINTAIN;

/// 列に許す権限ビットの全体（`PrivilegeObject::Column` の許可集合と一致させる）。
const COLUMN_PRIVILEGES: u16 = PRIV_SELECT | PRIV_INSERT | PRIV_UPDATE | PRIV_REFERENCES;

/// 付与先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Grantee {
    /// ロール名（識別子検証済み。解決は実行段）。
    Role(String),
    /// `PUBLIC`（同じテナントのロール全体）。
    Public,
    /// `CURRENT_USER`／`CURRENT_ROLE`（現在のロール）。
    CurrentRole,
    /// `SESSION_USER`（セッションのロール）。
    SessionUser,
}

/// 権限 1 項目（`SELECT`・`UPDATE (a, b)`・`ALL`）。`ALL` は表または列の許可集合へ展開済み。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrivItem {
    pub(crate) bits: u16,
    /// 空なら表単位、非空なら列単位（重複除去済み）。
    pub(crate) columns: Vec<String>,
}

/// 権限形の `GRANT`／`REVOKE`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrantObjects {
    pub(crate) items: Vec<PrivItem>,
    /// 解決済みの表名（裸の名前または SCHEMA 配下の内部キー。重複除去済み）。
    pub(crate) tables: Vec<String>,
    pub(crate) grantees: Vec<Grantee>,
    /// `GRANT` の `WITH GRANT OPTION`／`REVOKE` の `GRANT OPTION FOR`。
    pub(crate) grant_option: bool,
    /// `REVOKE ... CASCADE`（既定は `RESTRICT`）。
    pub(crate) cascade: bool,
}

/// ロール形のオプション名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoleOption {
    Admin,
    Inherit,
    Set,
}

/// ロール形の `GRANT`／`REVOKE`（構造化だけを行う。実行は `sql::grant_ddl`、Issue #2232）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrantRoles {
    pub(crate) roles: Vec<String>,
    pub(crate) grantees: Vec<Grantee>,
    /// `GRANT` の `WITH ...` で明示したオプション（重複なし）。
    pub(crate) with_options: Vec<(RoleOption, bool)>,
    /// `REVOKE` の `<オプション> OPTION FOR`。
    pub(crate) option_for: Option<RoleOption>,
    pub(crate) cascade: bool,
}

/// 検証済みの対象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GrantTarget {
    Objects(GrantObjects),
    Roles(GrantRoles),
}

/// 構造検証済みの `GRANT`／`REVOKE`（SQL-40、Issue #1965）。
///
/// 実行は `sql::grant_ddl::execute_grant_revoke`、呼び出し元は `core.rs` の `ParsedSql::GrantRevoke` 腕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedGrantRevoke {
    pub(crate) is_grant: bool,
    pub(crate) target: GrantTarget,
}

impl ValidatedGrantRevoke {
    /// 権限形なら対象表名（解決済み）を返す。一時表との衝突検査（core.rs の parse）で使う。
    pub(crate) fn object_table_names(&self) -> Option<&[String]> {
        match &self.target {
            GrantTarget::Objects(o) => Some(&o.tables),
            GrantTarget::Roles(_) => None,
        }
    }

    /// `REVOKE` なら真（wire のコマンドタグの選択に使う）。
    pub fn is_revoke(&self) -> bool {
        !self.is_grant
    }
}

/// トークンが `Ident` か構文語（`SELECT`・`FROM`）で、大文字へ正規化した語を返す（引用付き識別子も
/// 同じ `Ident` になるため、語の判定は文脈で限定された位置だけで使う）。
fn word_of(tok: Option<&Token>) -> Option<String> {
    match tok? {
        Token::Ident(w) => Some(w.to_ascii_uppercase()),
        Token::Keyword(Keyword::Select) => Some("SELECT".to_string()),
        Token::Keyword(Keyword::From) => Some("FROM".to_string()),
        _ => None,
    }
}

fn is_word(tok: Option<&Token>, w: &str) -> bool {
    word_of(tok).is_some_and(|x| x == w)
}

/// 先頭が `GRANT` か（汎用の文種判定より前に振り分けるために使う）。
pub(crate) fn is_grant_statement(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("GRANT"))
}

/// 先頭が `REVOKE` か。
pub(crate) fn is_revoke_statement(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("REVOKE"))
}

/// 先頭 3 語が `ALTER DEFAULT PRIVILEGES` か（汎用 `ALTER`〔`ALTER TABLE` 扱い〕より前に分岐して
/// `0A000` にする。#2237 の担当）。
pub(crate) fn is_alter_default_privileges_statement(tokens: &[Token]) -> bool {
    is_word(tokens.first(), "ALTER")
        && is_word(tokens.get(1), "DEFAULT")
        && is_word(tokens.get(2), "PRIVILEGES")
}

/// 先頭 3 語が `ALTER DEFAULT PRIVILEGES` の文を `0A000` で拒否する（#2237）。
pub(crate) fn reject_alter_default_privileges() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("ALTER DEFAULT PRIVILEGES is not supported yet")
}

fn syntax(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(detail)
}

/// 権限語をビットへ写す。表・列に許されない既知の権限語（`USAGE` 等）もビットを返し、対象が確定した
/// 後に許可集合との差で `0LP01` へ倒す（未知の語だけが `42601`）。
fn privilege_bit(word: &str) -> Option<u16> {
    Some(match word {
        "SELECT" => PRIV_SELECT,
        "INSERT" => PRIV_INSERT,
        "UPDATE" => PRIV_UPDATE,
        "DELETE" => PRIV_DELETE,
        "TRUNCATE" => PRIV_TRUNCATE,
        "REFERENCES" => PRIV_REFERENCES,
        "TRIGGER" => PRIV_TRIGGER,
        "MAINTAIN" => PRIV_MAINTAIN,
        // 表・列に許されない既知の権限語（スキーマ・シーケンス・関数・データベース向け）。
        "USAGE" => PRIV_USAGE,
        "CREATE" => PRIV_CREATE,
        "EXECUTE" => PRIV_EXECUTE,
        // データベース・パラメータ向け（ビットを持たない。表に対しては常に拒否）。
        "CONNECT" | "TEMPORARY" | "TEMP" | "SET" => 0x8000,
        _ => return None,
    })
}

/// 識別子を読む（修飾名・非識別子は `42601`）。
fn parse_name(p: &mut Parser<'_>, what: &str) -> Result<String, SqlSurfaceError> {
    match p.peek() {
        Some(Token::Ident(_)) => {
            let name = p.expect_ident()?;
            crate::catalog::validate_identifier(&name)
                .map_err(|_| syntax(&format!("invalid {what} name")))?;
            Ok(name)
        }
        _ => Err(syntax(&format!("expected a {what} name"))),
    }
}

fn push_unique<T: PartialEq>(v: &mut Vec<T>, item: T) -> Result<(), SqlSurfaceError> {
    if !v.contains(&item) {
        if v.len() >= MAX_GRANT_LIST_LEN {
            return Err(SqlSurfaceError::payload_too_large(
                "too many items in GRANT/REVOKE list",
            ));
        }
        v.push(item);
    }
    Ok(())
}

/// 付与先の並びを読む（`[GROUP] 名前 | PUBLIC | CURRENT_USER | CURRENT_ROLE | SESSION_USER` のカンマ区切り）。
fn parse_grantees(p: &mut Parser<'_>) -> Result<Vec<Grantee>, SqlSurfaceError> {
    let mut out = Vec::new();
    loop {
        // 旧来の `GROUP <名前>` は PostgreSQL と同じく読み飛ばす（次が名前のときだけ。名前 `group` と区別）。
        if p.peek_ident_matches("GROUP")
            && matches!(p.tokens.get(p.pos.saturating_add(1)), Some(Token::Ident(_)))
        {
            p.advance();
        }
        let tok = p.peek().cloned();
        let g = match tok {
            Some(Token::Ident(w)) if w.eq_ignore_ascii_case("PUBLIC") => {
                p.advance();
                Grantee::Public
            }
            Some(Token::Ident(w))
                if w.eq_ignore_ascii_case("CURRENT_USER")
                    || w.eq_ignore_ascii_case("CURRENT_ROLE") =>
            {
                p.advance();
                Grantee::CurrentRole
            }
            Some(Token::Ident(w)) if w.eq_ignore_ascii_case("SESSION_USER") => {
                p.advance();
                Grantee::SessionUser
            }
            Some(Token::QualifiedIdent { .. }) => {
                return Err(syntax("role names cannot be schema-qualified"));
            }
            _ => Grantee::Role(parse_name(p, "role")?),
        };
        push_unique(&mut out, g)?;
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            return Ok(out);
        }
    }
}

/// 省略可能な `GRANTED BY <ロール>` を読む。構文は検証するが実行は未対応のため `0A000`（#2236）。
fn reject_granted_by(p: &mut Parser<'_>) -> Result<(), SqlSurfaceError> {
    if p.peek_ident_matches("GRANTED") {
        p.advance();
        // `BY` は構文語トークン（`Token::Ident` ではない）。
        p.expect_keyword(Keyword::By)?;
        let _ = parse_grantees(p)?;
        return Err(SqlSurfaceError::feature_not_supported(
            "GRANTED BY is not supported yet",
        ));
    }
    Ok(())
}

/// 省略可能な `CASCADE`／`RESTRICT` を読む。
fn parse_behavior(p: &mut Parser<'_>) -> bool {
    if p.eat_word("CASCADE") {
        true
    } else {
        let _ = p.eat_word("RESTRICT");
        false
    }
}

/// 先頭から最初の最上位（括弧の外）の `ON`／`TO`（REVOKE は `FROM`）を探し、権限形か判定する。
fn is_object_form(tokens: &[Token], is_grant: bool) -> bool {
    let mut depth = 0usize;
    for t in tokens.iter().skip(1) {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                if is_word(Some(t), "ON") {
                    return true;
                }
                if is_word(Some(t), if is_grant { "TO" } else { "FROM" }) {
                    return false;
                }
            }
            _ => {}
        }
    }
    // どちらも無い形は権限形として扱い、欠落を `42601` にする。
    true
}

/// 権限の並びを読む（`ON` の手前まで）。
fn parse_privilege_items(
    p: &mut Parser<'_>,
) -> Result<Vec<(u16, bool, Vec<String>)>, SqlSurfaceError> {
    // (ビット, ALL か, 列)。ALL の展開は対象が確定してから行う（表か列かは列リストで決まる）。
    let mut items: Vec<(u16, bool, Vec<String>)> = Vec::new();
    loop {
        let word = word_of(p.peek()).ok_or_else(|| syntax("expected a privilege name"))?;
        let (bits, is_all) = if word == "ALL" {
            p.advance();
            let _ = p.eat_word("PRIVILEGES");
            (0u16, true)
        } else if let Some(bits) = privilege_bit(&word) {
            p.advance();
            (bits, false)
        } else {
            return Err(syntax("unrecognized privilege type"));
        };
        let mut columns: Vec<String> = Vec::new();
        if matches!(p.peek(), Some(Token::Punct('('))) {
            p.advance();
            loop {
                let c = parse_name(p, "column")?;
                push_unique(&mut columns, c)?;
                match p.advance() {
                    Some(Token::Punct(',')) => {}
                    Some(Token::Punct(')')) => break,
                    _ => return Err(syntax("expected ',' or ')' in column list")),
                }
            }
        }
        if items.len() >= MAX_GRANT_LIST_LEN {
            return Err(SqlSurfaceError::payload_too_large(
                "too many privileges in GRANT/REVOKE list",
            ));
        }
        items.push((bits, is_all, columns));
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            return Ok(items);
        }
    }
}

/// 範囲外の対象種別（`ON SCHEMA`・`ON ALL TABLES IN SCHEMA` 等）の語か。
fn is_unsupported_object_kind(word: &str) -> bool {
    [
        "SCHEMA",
        "SEQUENCE",
        "FUNCTION",
        "PROCEDURE",
        "ROUTINE",
        "DATABASE",
        "DOMAIN",
        "TYPE",
        "LANGUAGE",
        "TABLESPACE",
        "FOREIGN",
        "LARGE",
        "PARAMETER",
        "ALL",
    ]
    .contains(&word)
}

/// `TO`／`FROM`／`,` の手前に名前が続く位置か（種別語と同名の表の区別に使う）。
fn name_follows(p: &Parser<'_>, offset: usize) -> bool {
    match p.tokens.get(p.pos.saturating_add(offset)) {
        Some(Token::Ident(w)) => !(w.eq_ignore_ascii_case("TO")),
        _ => false,
    }
}

/// 権限形の本体（先頭の `GRANT`／`REVOKE`・`GRANT OPTION FOR` を読んだ後）。
fn parse_object_form(
    p: &mut Parser<'_>,
    is_grant: bool,
    option_for: bool,
) -> Result<GrantObjects, SqlSurfaceError> {
    let raw_items = parse_privilege_items(p)?;
    p.expect_ident_matching("ON")?;
    // 対象種別。範囲外の種別は構文（`TO`／`FROM` の存在）だけ確かめて `0A000`。
    if let Some(w) = word_of(p.peek()) {
        let is_kind = is_unsupported_object_kind(&w)
            && (name_follows(p, 1)
                || matches!(
                    p.tokens.get(p.pos.saturating_add(1)),
                    Some(Token::QualifiedIdent { .. })
                ));
        if is_kind {
            let sep = if is_grant { "TO" } else { "FROM" };
            let has_sep = p.tokens.iter().skip(p.pos).any(|t| is_word(Some(t), sep));
            if !has_sep {
                return Err(syntax(&format!("expected {sep}")));
            }
            return Err(SqlSurfaceError::feature_not_supported(
                "GRANT/REVOKE on this object type is not supported yet",
            ));
        }
    }
    // 省略可能な `TABLE`（次が名前のときだけ。名前 `table` と区別）。
    if p.peek_ident_matches("TABLE")
        && (name_follows(p, 1)
            || matches!(
                p.tokens.get(p.pos.saturating_add(1)),
                Some(Token::QualifiedIdent { .. })
            ))
    {
        p.advance();
    }
    let mut tables: Vec<String> = Vec::new();
    loop {
        if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
            return Err(SqlSurfaceError::feature_not_supported(
                "unresolved schema-qualified table names are not supported",
            ));
        }
        let t = p
            .expect_ident()
            .map_err(|_| syntax("expected a table name"))?;
        crate::catalog::validate_relation_key(&t)
            .map_err(|_| syntax("invalid table name in GRANT/REVOKE"))?;
        push_unique(&mut tables, t)?;
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            break;
        }
    }
    if is_grant {
        p.expect_ident_matching("TO")?;
    } else {
        p.expect_keyword(Keyword::From)?;
    }
    let grantees = parse_grantees(p)?;
    let mut grant_option = option_for;
    if is_grant {
        if p.peek_ident_matches("WITH") {
            p.advance();
            p.expect_ident_matching("GRANT")?;
            p.expect_ident_matching("OPTION")?;
            grant_option = true;
        }
        reject_granted_by(p)?;
    }
    let mut cascade = false;
    if !is_grant {
        reject_granted_by(p)?;
        cascade = parse_behavior(p);
    }
    p.expect_end_of_statement()?;

    // 権限の展開と、対象の種別（表／列）に許される権限かの検査（`0LP01`）。
    let mut items: Vec<PrivItem> = Vec::new();
    for (bits, is_all, columns) in raw_items {
        let allowed = if columns.is_empty() {
            TABLE_PRIVILEGES
        } else {
            COLUMN_PRIVILEGES
        };
        let bits = if is_all { allowed } else { bits };
        if bits & !allowed != 0 {
            return Err(SqlSurfaceError::invalid_grant_operation(
                if columns.is_empty() {
                    "invalid privilege type for table"
                } else {
                    "invalid privilege type for column"
                },
            ));
        }
        items.push(PrivItem { bits, columns });
    }
    if is_grant && grant_option && grantees.contains(&Grantee::Public) {
        return Err(SqlSurfaceError::invalid_grant_operation(
            "grant options can only be granted to roles",
        ));
    }
    Ok(GrantObjects {
        items,
        tables,
        grantees,
        grant_option,
        cascade,
    })
}

/// ロール名の並びを読む（`TO`／`FROM` の手前まで）。
fn parse_role_names(p: &mut Parser<'_>) -> Result<Vec<String>, SqlSurfaceError> {
    let mut roles = Vec::new();
    loop {
        if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
            return Err(syntax("role names cannot be schema-qualified"));
        }
        let r = parse_name(p, "role")?;
        push_unique(&mut roles, r)?;
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            return Ok(roles);
        }
    }
}

fn role_option_of(word: &str) -> Option<RoleOption> {
    match word {
        "ADMIN" => Some(RoleOption::Admin),
        "INHERIT" => Some(RoleOption::Inherit),
        "SET" => Some(RoleOption::Set),
        _ => None,
    }
}

/// ロール形の本体。
fn parse_role_form(
    p: &mut Parser<'_>,
    is_grant: bool,
    option_for: Option<RoleOption>,
) -> Result<GrantRoles, SqlSurfaceError> {
    let roles = parse_role_names(p)?;
    if is_grant {
        p.expect_ident_matching("TO")?;
    } else {
        p.expect_keyword(Keyword::From)?;
    }
    let grantees = parse_grantees(p)?;
    let mut with_options: Vec<(RoleOption, bool)> = Vec::new();
    if is_grant && p.peek_ident_matches("WITH") {
        p.advance();
        loop {
            let opt = word_of(p.peek())
                .and_then(|w| role_option_of(&w))
                .ok_or_else(|| syntax("expected ADMIN, INHERIT or SET after WITH"))?;
            p.advance();
            let value = if p.eat_word("OPTION") || p.eat_word("TRUE") {
                true
            } else if p.eat_word("FALSE") {
                false
            } else {
                return Err(syntax("expected OPTION, TRUE or FALSE"));
            };
            if with_options.iter().any(|(o, _)| *o == opt) {
                return Err(syntax("conflicting or redundant options"));
            }
            with_options.push((opt, value));
            if matches!(p.peek(), Some(Token::Punct(','))) {
                p.advance();
            } else {
                break;
            }
        }
    }
    reject_granted_by(p)?;
    let cascade = if is_grant { false } else { parse_behavior(p) };
    p.expect_end_of_statement()?;
    Ok(GrantRoles {
        roles,
        grantees,
        with_options,
        option_for,
        cascade,
    })
}

/// [`ValidatedGrantRevoke`] の構造検証本体（SQL-40、Issue #1965）。先頭は `GRANT` か `REVOKE`。
pub(crate) fn validate_grant_revoke_tokens(
    tokens: &[Token],
) -> Result<ValidatedGrantRevoke, SqlSurfaceError> {
    let is_grant = is_grant_statement(tokens);
    if !is_grant && !is_revoke_statement(tokens) {
        return Err(syntax("expected GRANT or REVOKE"));
    }
    let mut p = Parser::new(tokens);
    p.advance();
    let object_form = is_object_form(tokens, is_grant);
    // REVOKE の `GRANT OPTION FOR`／`<オプション> OPTION FOR` 接頭辞。
    let mut grant_option_for = false;
    let mut role_option_for: Option<RoleOption> = None;
    if !is_grant {
        let w0 = word_of(p.peek());
        let opt_for = is_word(p.tokens.get(p.pos.saturating_add(1)), "OPTION")
            && is_word(p.tokens.get(p.pos.saturating_add(2)), "FOR");
        if opt_for {
            if object_form && w0.as_deref() == Some("GRANT") {
                grant_option_for = true;
                p.advance();
                p.advance();
                p.advance();
            } else if !object_form {
                if let Some(o) = w0.as_deref().and_then(role_option_of) {
                    role_option_for = Some(o);
                    p.advance();
                    p.advance();
                    p.advance();
                }
            }
        }
    }
    let target = if object_form {
        GrantTarget::Objects(parse_object_form(&mut p, is_grant, grant_option_for)?)
    } else {
        GrantTarget::Roles(parse_role_form(&mut p, is_grant, role_option_for)?)
    };
    Ok(ValidatedGrantRevoke { is_grant, target })
}

#[cfg(test)]
mod tests;
