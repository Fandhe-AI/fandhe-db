//! `ALTER {TABLE|VIEW|MATERIALIZED VIEW|SCHEMA} <名> OWNER TO <ロール>` と `CREATE SCHEMA ... AUTHORIZATION`
//! の構文検証、および `REASSIGN OWNED`／`DROP OWNED` の拒否（SQL-40・RLS-12 のポインタ、Issue #2236。
//! 設計記録: `docs/design/role-privilege-model.md` §14）。
//!
//! 役割: トークン列から対象の種別・名前・新しい所有者の指定だけを取り出す。カタログは一切照会しない
//! （対象・ロールの存在と所有者の判定は `sql::owner_ddl` が権限ゲートの後で行う）。呼び出し元は
//! `core.rs` の `parse_sql` 内の分岐だけで、汎用 `ALTER`（`ALTER TABLE` 扱い）・`ALTER SCHEMA` より前に
//! 置く。表・ビュー・matview の名前は、名前解決器が `schema.name` を内部キーへ置き換えた後のトークンを
//! 受け取る（解決されずに残った修飾名は `0A000`。状態に依存しない拒否のため存在オラクルにならない）。
//!
//! 本 Issue で受理しない形（すべて状態に依存しない拒否）: `IF EXISTS`・`ALTER TABLE` の複数アクション・
//! `ALTER SEQUENCE`／`ALTER FUNCTION` の `OWNER TO`・`REASSIGN OWNED`・`DROP OWNED`（`0A000`）、
//! 句の欠落・余分なトークン（`42601`）。

use super::*;

/// `OWNER TO` の対象種別（コマンドタグの選択にも使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterOwnerKind {
    /// 表（`ALTER TABLE`）。
    Table,
    /// ビュー（`ALTER VIEW`）。
    View,
    /// マテリアライズドビュー（`ALTER MATERIALIZED VIEW`）。
    MaterializedView,
    /// SCHEMA（`ALTER SCHEMA`）。
    Schema,
}

/// 新しい所有者の指定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoleSpec {
    /// ロール名（識別子検証済み。解決は実行段）。
    Name(String),
    /// `CURRENT_USER`／`CURRENT_ROLE`（現在のロール）。
    CurrentRole,
    /// `SESSION_USER`（セッションのロール）。
    SessionUser,
}

/// `ALTER ... OWNER TO` の構造検証結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAlterOwner {
    pub(crate) kind: AlterOwnerKind,
    /// 対象名（表・ビュー・matview は内部キー、SCHEMA は裸の名前）。
    pub(crate) name: String,
    pub(crate) new_owner: RoleSpec,
}

impl ValidatedAlterOwner {
    /// 対象の種別。
    pub fn kind(&self) -> AlterOwnerKind {
        self.kind
    }

    /// 表・ビュー・matview の対象名（SCHEMA は `None`。一時表との衝突判定用）。
    pub(crate) fn relation_name(&self) -> Option<&str> {
        match self.kind {
            AlterOwnerKind::Schema => None,
            _ => Some(&self.name),
        }
    }
}

fn word_at(tokens: &[Token], i: usize) -> Option<&str> {
    match tokens.get(i) {
        Some(Token::Ident(w)) => Some(w.as_str()),
        _ => None,
    }
}

fn is_word(tokens: &[Token], i: usize, expected: &str) -> bool {
    word_at(tokens, i).is_some_and(|w| w.eq_ignore_ascii_case(expected))
}

/// 先頭の `ALTER <種別>` を読み、対象名の位置を返す（`ONLY`・`IF EXISTS` は位置を進めるだけで、
/// `IF EXISTS` かどうかも返す）。
fn head(tokens: &[Token]) -> Option<(AlterOwnerKind, usize, bool)> {
    if !is_word(tokens, 0, "ALTER") {
        return None;
    }
    let (kind, mut i) = if is_word(tokens, 1, "TABLE") {
        (AlterOwnerKind::Table, 2)
    } else if is_word(tokens, 1, "VIEW") {
        (AlterOwnerKind::View, 2)
    } else if is_word(tokens, 1, "MATERIALIZED") && is_word(tokens, 2, "VIEW") {
        (AlterOwnerKind::MaterializedView, 3)
    } else if is_word(tokens, 1, "SCHEMA") {
        (AlterOwnerKind::Schema, 2)
    } else {
        return None;
    };
    let mut if_exists = false;
    if is_word(tokens, i, "IF") && is_word(tokens, i.saturating_add(1), "EXISTS") {
        if_exists = true;
        i = i.saturating_add(2);
    }
    if kind == AlterOwnerKind::Table && is_word(tokens, i, "ONLY") {
        i = i.saturating_add(1);
    }
    Some((kind, i, if_exists))
}

/// 対象名の直後が `OWNER TO` の `ALTER {TABLE|VIEW|MATERIALIZED VIEW|SCHEMA}` か。他のアクション
/// （`ADD COLUMN`・`RENAME TO` 等）と列名 `owner` を取り違えないよう、名前の直後の位置だけを見る。
pub(crate) fn is_alter_owner_statement(tokens: &[Token]) -> bool {
    let Some((_, i, _)) = head(tokens) else {
        return false;
    };
    matches!(
        tokens.get(i),
        Some(Token::Ident(_) | Token::QualifiedIdent { .. })
    ) && is_word(tokens, i.saturating_add(1), "OWNER")
        && is_word(tokens, i.saturating_add(2), "TO")
}

/// 新しい所有者（`名前 | CURRENT_USER | CURRENT_ROLE | SESSION_USER`）を読む。
pub(super) fn parse_role_spec(p: &mut Parser<'_>) -> Result<RoleSpec, SqlSurfaceError> {
    match p.peek() {
        Some(Token::Ident(w))
            if w.eq_ignore_ascii_case("CURRENT_USER") || w.eq_ignore_ascii_case("CURRENT_ROLE") =>
        {
            p.advance();
            Ok(RoleSpec::CurrentRole)
        }
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("SESSION_USER") => {
            p.advance();
            Ok(RoleSpec::SessionUser)
        }
        Some(Token::QualifiedIdent { .. }) => Err(SqlSurfaceError::unsupported(
            "role names cannot be schema-qualified",
        )),
        Some(Token::Ident(_)) => {
            let name = p.expect_ident()?;
            crate::catalog::validate_identifier(&name)
                .map_err(|_| SqlSurfaceError::unsupported("invalid role name"))?;
            Ok(RoleSpec::Name(name))
        }
        _ => Err(SqlSurfaceError::unsupported("expected a role name")),
    }
}

/// [`ValidatedAlterOwner`] の構造検証本体。[`is_alter_owner_statement`] が真の文だけに使う。
pub(crate) fn validate_alter_owner_tokens(
    tokens: &[Token],
) -> Result<ValidatedAlterOwner, SqlSurfaceError> {
    let Some((kind, name_pos, if_exists)) = head(tokens) else {
        return Err(SqlSurfaceError::unsupported("unsupported ALTER statement"));
    };
    if if_exists {
        return Err(SqlSurfaceError::feature_not_supported(
            "ALTER ... IF EXISTS ... OWNER TO is not supported",
        ));
    }
    let mut p = Parser::new(tokens);
    p.pos = name_pos;
    if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
        return Err(SqlSurfaceError::feature_not_supported(
            "unresolved schema-qualified names are not supported in OWNER TO",
        ));
    }
    let name = p.expect_ident()?;
    match kind {
        AlterOwnerKind::Schema => crate::catalog::validate_identifier(&name)
            .map_err(|_| SqlSurfaceError::unsupported("invalid schema name in ALTER SCHEMA"))?,
        _ => crate::catalog::validate_relation_key(&name)
            .map_err(|_| SqlSurfaceError::unsupported("invalid relation name in ALTER"))?,
    }
    p.expect_ident_matching("OWNER")?;
    p.expect_ident_matching("TO")?;
    let new_owner = parse_role_spec(&mut p)?;
    if matches!(p.peek(), Some(Token::Punct(','))) {
        return Err(SqlSurfaceError::feature_not_supported(
            "ALTER TABLE with multiple actions including OWNER TO is not supported",
        ));
    }
    p.expect_end_of_statement()?;
    Ok(ValidatedAlterOwner {
        kind,
        name,
        new_owner,
    })
}

/// `REASSIGN OWNED`／`DROP OWNED` か（汎用 `DROP` より前に分岐して拒否するために使う）。
pub(crate) fn is_owned_by_statement(tokens: &[Token]) -> bool {
    (is_word(tokens, 0, "REASSIGN") && is_word(tokens, 1, "OWNED"))
        || (is_word(tokens, 0, "DROP") && is_word(tokens, 1, "OWNED"))
}

/// `REASSIGN OWNED`／`DROP OWNED` の固定の拒否（状態に依存しないため存在オラクルにならない。
/// 実装は後続 Issue）。
pub(crate) fn reject_owned_by() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("REASSIGN OWNED and DROP OWNED are not supported yet")
}

#[cfg(test)]
mod tests;
