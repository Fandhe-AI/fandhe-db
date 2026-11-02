//! `ALTER DEFAULT PRIVILEGES` の構文検証（SQL-40・RLS-12 のポインタ、Issue #2237。設計記録:
//! `docs/design/role-privilege-model.md` §19）。
//!
//! 役割: トークン列から `[FOR {ROLE|USER} <ロール>[, ...]] [IN SCHEMA <名>[, ...]] {GRANT|REVOKE} ...` の
//! 構造だけを取り出す。カタログは一切照会しない（親の `grant` と同じ設計。ロール・SCHEMA の存在と認可は
//! `sql::default_privileges_ddl` が `catalog::privilege::default_acl` を呼んで判定する）。呼び出し元は
//! `core.rs` の `parse_sql` 内の分岐だけ。親モジュールの権限語・付与先・オプションの読み取りを再利用する。
//!
//! 構文段で拒否する形: 句の重複・欠落・順序違い・末尾の余り・列リスト（`42601`）、対象の種別に許されない
//! 権限語・PUBLIC への `WITH GRANT OPTION`・`IN SCHEMA` と `ON SCHEMAS` の併用（`0LP01`）、範囲外の対象
//! （`TYPES`・`LARGE OBJECTS`）・`GRANTED BY`・組み込み SCHEMA 名（`0A000`。状態に依存しない拒否のため
//! 存在オラクルにならない）。列挙の件数は `MAX_GRANT_LIST_LEN` で検証してから確保する。

use super::*;

/// 既定権限の対象オブジェクトの種別（`ON TABLES|SEQUENCES|FUNCTIONS|SCHEMAS`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultAclTarget {
    /// 表・ビュー・matview（PostgreSQL と同じく `TABLES` に含める）。
    Tables,
    /// SEQUENCE（SERIAL／IDENTITY の暗黙 SEQUENCE を含む）。
    Sequences,
    /// 関数（`ROUTINES` は同義として受理する）。
    Functions,
    /// SCHEMA（`IN SCHEMA` とは併用できない）。
    Schemas,
}

/// 構造検証済みの `ALTER DEFAULT PRIVILEGES`（SQL-40、Issue #2237）。
///
/// 実行は `sql::default_privileges_ddl::execute_alter_default_privileges`、呼び出し元は `core.rs` の
/// `ParsedSql::AlterDefaultPrivileges` 腕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAlterDefaultPrivileges {
    pub(crate) is_grant: bool,
    /// `FOR ROLE` の対象ロール名（空なら現在ロール。重複除去済み）。
    pub(crate) roles: Vec<String>,
    /// `IN SCHEMA` の SCHEMA 名（空なら全体。重複除去済み。解決は実行段の write txn の中で行う）。
    pub(crate) schemas: Vec<String>,
    pub(crate) target: DefaultAclTarget,
    /// 対象の種別の許可集合へ展開済みの権限ビットの和。
    pub(crate) privileges: u16,
    pub(crate) grantees: Vec<Grantee>,
    /// `GRANT` の `WITH GRANT OPTION`／`REVOKE` の `GRANT OPTION FOR`。
    pub(crate) grant_option: bool,
    /// `REVOKE ... CASCADE`（既定の付与者は常に対象ロールで下流の依存が生じないため、効果は無い）。
    pub(crate) cascade: bool,
}

impl ValidatedAlterDefaultPrivileges {
    /// `REVOKE` なら真（API 利用者向け。wire のコマンドタグは常に `ALTER DEFAULT PRIVILEGES`）。
    pub fn is_revoke(&self) -> bool {
        !self.is_grant
    }
}

fn syntax(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(detail)
}

/// カンマ区切りの名前の並びを読む（件数は `push_unique` が上限検証する）。
fn parse_name_list(p: &mut Parser<'_>, what: &str) -> Result<Vec<String>, SqlSurfaceError> {
    let mut out = Vec::new();
    loop {
        if matches!(p.peek(), Some(Token::QualifiedIdent { .. })) {
            return Err(syntax(&format!("{what} names cannot be schema-qualified")));
        }
        let n = parse_name(p, what)?;
        push_unique(&mut out, n)?;
        if matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
        } else {
            return Ok(out);
        }
    }
}

/// [`ValidatedAlterDefaultPrivileges`] の構造検証本体。先頭 3 語は `ALTER DEFAULT PRIVILEGES`。
pub(crate) fn validate_alter_default_privileges_tokens(
    tokens: &[Token],
) -> Result<ValidatedAlterDefaultPrivileges, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    for _ in 0..3 {
        p.advance();
    }
    let mut roles: Option<Vec<String>> = None;
    let mut schemas: Option<Vec<String>> = None;
    loop {
        if p.peek_ident_matches("FOR") {
            if roles.is_some() {
                return Err(syntax("conflicting or redundant options"));
            }
            p.advance();
            if !(p.eat_word("ROLE") || p.eat_word("USER")) {
                return Err(syntax("expected ROLE or USER after FOR"));
            }
            roles = Some(parse_name_list(&mut p, "role")?);
        } else if p.peek_ident_matches("IN") {
            if schemas.is_some() {
                return Err(syntax("conflicting or redundant options"));
            }
            p.advance();
            p.expect_ident_matching("SCHEMA")?;
            schemas = Some(parse_name_list(&mut p, "schema")?);
        } else {
            break;
        }
    }
    let is_grant = if p.eat_word("GRANT") {
        true
    } else if p.eat_word("REVOKE") {
        false
    } else {
        return Err(syntax("expected GRANT or REVOKE"));
    };
    let mut grant_option = false;
    if !is_grant
        && p.peek_ident_matches("GRANT")
        && p.peek_ident_matches_at(1, "OPTION")
        && p.peek_ident_matches_at(2, "FOR")
    {
        for _ in 0..3 {
            p.advance();
        }
        grant_option = true;
    }
    let raw_items = parse_privilege_items(&mut p)?;
    p.expect_ident_matching("ON")?;
    let target = match word_of(p.peek()).as_deref() {
        Some("TABLES") => DefaultAclTarget::Tables,
        Some("SEQUENCES") => DefaultAclTarget::Sequences,
        Some("FUNCTIONS" | "ROUTINES") => DefaultAclTarget::Functions,
        Some("SCHEMAS") => DefaultAclTarget::Schemas,
        Some("TYPES" | "LARGE") => {
            // 範囲外の対象は、区切り語の存在だけ確かめて `0A000`（状態に依存しない）。
            let sep = if is_grant { "TO" } else { "FROM" };
            if !p.tokens.iter().skip(p.pos).any(|t| is_word(Some(t), sep)) {
                return Err(syntax(&format!("expected {sep}")));
            }
            return Err(SqlSurfaceError::feature_not_supported(
                "ALTER DEFAULT PRIVILEGES on this object type is not supported yet",
            ));
        }
        _ => return Err(syntax("expected TABLES, SEQUENCES, FUNCTIONS or SCHEMAS")),
    };
    p.advance();
    if is_grant {
        p.expect_ident_matching("TO")?;
    } else {
        p.expect_keyword(Keyword::From)?;
    }
    let grantees = parse_grantees(&mut p)?;
    if is_grant {
        if p.peek_ident_matches("WITH") {
            p.advance();
            p.expect_ident_matching("GRANT")?;
            p.expect_ident_matching("OPTION")?;
            grant_option = true;
        }
        reject_granted_by(&mut p)?;
    }
    let mut cascade = false;
    if !is_grant {
        reject_granted_by(&mut p)?;
        cascade = parse_behavior(&mut p);
    }
    p.expect_end_of_statement()?;

    let schemas = schemas.unwrap_or_default();
    // 組み込み SCHEMA（行・内部 ID を持たない仮想 SCHEMA）は名前だけで決まる `0A000`（`GRANT ON SCHEMA` と同じ）。
    if schemas.iter().any(|n| {
        crate::catalog::namespace::is_builtin_schema_name(n) || n.eq_ignore_ascii_case("pg_temp")
    }) {
        return Err(SqlSurfaceError::feature_not_supported(
            "ALTER DEFAULT PRIVILEGES on built-in schemas is not supported",
        ));
    }
    if target == DefaultAclTarget::Schemas && !schemas.is_empty() {
        return Err(SqlSurfaceError::invalid_grant_operation(
            "cannot use IN SCHEMA clause when using ON SCHEMAS",
        ));
    }
    let allowed = match target {
        DefaultAclTarget::Tables => TABLE_PRIVILEGES,
        DefaultAclTarget::Sequences => SEQUENCE_PRIVILEGES,
        DefaultAclTarget::Functions => FUNCTION_PRIVILEGES,
        DefaultAclTarget::Schemas => SCHEMA_PRIVILEGES,
    };
    let mut privileges = 0u16;
    for (bits, is_all, columns) in raw_items {
        if !columns.is_empty() {
            return Err(syntax(
                "column lists are not allowed in ALTER DEFAULT PRIVILEGES",
            ));
        }
        let bits = if is_all { allowed } else { bits };
        if bits & !allowed != 0 {
            return Err(SqlSurfaceError::invalid_grant_operation(
                "invalid privilege type for this object type",
            ));
        }
        privileges |= bits;
    }
    if is_grant && grant_option && grantees.contains(&Grantee::Public) {
        return Err(SqlSurfaceError::invalid_grant_operation(
            "grant options can only be granted to roles",
        ));
    }
    Ok(ValidatedAlterDefaultPrivileges {
        is_grant,
        roles: roles.unwrap_or_default(),
        schemas,
        target,
        privileges,
        grantees,
        grant_option,
        cascade,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn parse(sql: &str) -> Result<ValidatedAlterDefaultPrivileges, SqlSurfaceError> {
        validate_alter_default_privileges_tokens(&tokenize(sql).expect("lex"))
    }

    fn code(sql: &str) -> &'static str {
        parse(sql).expect_err("must be rejected").wire_code()
    }

    #[test]
    fn accepts_all_targets_and_clauses() {
        let s = parse("ALTER DEFAULT PRIVILEGES GRANT SELECT, INSERT ON TABLES TO bob").unwrap();
        assert!(s.is_grant && s.roles.is_empty() && s.schemas.is_empty());
        assert_eq!(s.target, DefaultAclTarget::Tables);
        assert_eq!(s.privileges, PRIV_SELECT | PRIV_INSERT);
        assert_eq!(s.grantees, vec![Grantee::Role("bob".into())]);

        let s = parse(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA a, b FOR ROLE x, y \
             REVOKE ALL ON SEQUENCES FROM PUBLIC CASCADE",
        )
        .unwrap();
        assert!(s.is_revoke() && s.cascade);
        assert_eq!(s.roles, vec!["x", "y"]);
        assert_eq!(s.schemas, vec!["a", "b"]);
        assert_eq!(s.privileges, SEQUENCE_PRIVILEGES);

        let s = parse("ALTER DEFAULT PRIVILEGES FOR USER x GRANT EXECUTE ON ROUTINES TO PUBLIC")
            .unwrap();
        assert_eq!(s.target, DefaultAclTarget::Functions);
        let s = parse("ALTER DEFAULT PRIVILEGES GRANT USAGE ON SCHEMAS TO b WITH GRANT OPTION")
            .unwrap();
        assert_eq!(s.target, DefaultAclTarget::Schemas);
        assert!(s.grant_option);
        let s = parse("ALTER DEFAULT PRIVILEGES REVOKE GRANT OPTION FOR SELECT ON TABLES FROM b")
            .unwrap();
        assert!(s.grant_option && !s.is_grant);
    }

    #[test]
    fn rejects_malformed_shapes_with_42601() {
        for sql in [
            "ALTER DEFAULT PRIVILEGES",
            "ALTER DEFAULT PRIVILEGES SELECT ON TABLES TO a",
            "ALTER DEFAULT PRIVILEGES FOR ROLE a FOR ROLE b GRANT SELECT ON TABLES TO c",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA a IN SCHEMA b GRANT SELECT ON TABLES TO c",
            "ALTER DEFAULT PRIVILEGES FOR a GRANT SELECT ON TABLES TO c",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLE TO c",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES c",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT (a) ON TABLES TO c",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO c extra stuff",
            "ALTER DEFAULT PRIVILEGES GRANT FOO ON TABLES TO c",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA s.t GRANT SELECT ON TABLES TO c",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
    }

    #[test]
    fn rejects_invalid_grant_and_unsupported() {
        for sql in [
            "ALTER DEFAULT PRIVILEGES GRANT DELETE ON SEQUENCES TO a",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON FUNCTIONS TO a",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC WITH GRANT OPTION",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA s GRANT USAGE ON SCHEMAS TO a",
        ] {
            assert_eq!(code(sql), "0LP01", "{sql}");
        }
        for sql in [
            "ALTER DEFAULT PRIVILEGES GRANT USAGE ON TYPES TO a",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON LARGE OBJECTS TO a",
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO a GRANTED BY b",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO a",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA pg_catalog GRANT SELECT ON TABLES TO a",
        ] {
            assert_eq!(code(sql), "0A000", "{sql}");
        }
    }

    #[test]
    fn list_lengths_are_bounded() {
        let names: Vec<String> = (0..=MAX_GRANT_LIST_LEN).map(|i| format!("r{i}")).collect();
        let sql = format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {} GRANT SELECT ON TABLES TO a",
            names.join(", ")
        );
        assert_eq!(code(&sql), "54000");
    }
}
