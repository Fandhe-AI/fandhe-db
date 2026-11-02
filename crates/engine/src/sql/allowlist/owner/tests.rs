//! `OWNER TO`・`AUTHORIZATION`・`REASSIGN／DROP OWNED` の構文検証の単体テスト（Issue #2236）。カタログは
//! 照会しない層のため、ストレージ無しで動く。

use super::*;
use crate::sql::lexer::tokenize;

fn parse(sql: &str) -> Result<ValidatedAlterOwner, SqlSurfaceError> {
    validate_alter_owner_tokens(&tokenize(sql).expect("lex"))
}

fn is_owner_stmt(sql: &str) -> bool {
    is_alter_owner_statement(&tokenize(sql).expect("lex"))
}

fn code(sql: &str) -> &'static str {
    parse(sql).expect_err("must be rejected").wire_code()
}

#[test]
fn accepts_each_supported_head() {
    let t = parse("ALTER TABLE docs OWNER TO alice").expect("table");
    assert_eq!(t.kind, AlterOwnerKind::Table);
    assert_eq!(t.name, "docs");
    assert_eq!(t.new_owner, RoleSpec::Name("alice".to_string()));
    assert_eq!(
        parse("ALTER TABLE ONLY docs OWNER TO alice")
            .expect("only")
            .kind,
        AlterOwnerKind::Table
    );
    assert_eq!(
        parse("ALTER VIEW v OWNER TO alice").expect("view").kind,
        AlterOwnerKind::View
    );
    assert_eq!(
        parse("ALTER MATERIALIZED VIEW m OWNER TO alice")
            .expect("mv")
            .kind,
        AlterOwnerKind::MaterializedView
    );
    let s = parse("ALTER SCHEMA s OWNER TO alice;").expect("schema");
    assert_eq!(s.kind, AlterOwnerKind::Schema);
    assert_eq!(s.relation_name(), None);
}

#[test]
fn role_specs() {
    assert_eq!(
        parse("ALTER TABLE t OWNER TO CURRENT_USER")
            .expect("p")
            .new_owner,
        RoleSpec::CurrentRole
    );
    assert_eq!(
        parse("ALTER TABLE t OWNER TO current_role")
            .expect("p")
            .new_owner,
        RoleSpec::CurrentRole
    );
    assert_eq!(
        parse("ALTER TABLE t OWNER TO SESSION_USER")
            .expect("p")
            .new_owner,
        RoleSpec::SessionUser
    );
}

#[test]
fn detection_only_matches_owner_to_right_after_the_name() {
    assert!(is_owner_stmt("ALTER TABLE t OWNER TO r"));
    assert!(is_owner_stmt("ALTER SCHEMA s OWNER TO r"));
    // 他のアクション・列名 `owner` は取り違えない。
    assert!(!is_owner_stmt("ALTER TABLE t ADD COLUMN owner TEXT"));
    assert!(!is_owner_stmt("ALTER TABLE t RENAME COLUMN owner TO o"));
    assert!(!is_owner_stmt("ALTER SCHEMA s RENAME TO s2"));
    assert!(!is_owner_stmt("ALTER SEQUENCE q OWNER TO r"));
    assert!(!is_owner_stmt("DROP TABLE t"));
}

#[test]
fn rejections_have_fixed_codes() {
    // 状態に依存しない `0A000`。
    assert_eq!(code("ALTER TABLE IF EXISTS t OWNER TO r"), "0A000");
    assert_eq!(code("ALTER TABLE t OWNER TO r, ADD COLUMN c TEXT"), "0A000");
    // 修飾名・余分なトークンは構文エラー側。
    assert_eq!(code("ALTER TABLE t OWNER TO r extra"), "42601");
    assert_eq!(code("ALTER TABLE t OWNER TO"), "42601");
    assert_eq!(code("ALTER TABLE t OWNER TO s.r"), "42601");
}

#[test]
fn owned_by_statements_are_rejected() {
    for sql in [
        "REASSIGN OWNED BY a TO b",
        "DROP OWNED BY a",
        "drop owned by a cascade",
    ] {
        assert!(is_owned_by_statement(&tokenize(sql).expect("lex")), "{sql}");
    }
    assert!(!is_owned_by_statement(
        &tokenize("DROP TABLE owned").expect("lex")
    ));
    assert_eq!(reject_owned_by().wire_code(), "0A000");
}

#[test]
fn create_schema_authorization_forms() {
    let parse_cs = |sql: &str| validate_create_schema_tokens(&tokenize(sql).expect("lex"));
    let a = parse_cs("CREATE SCHEMA s AUTHORIZATION alice").expect("named");
    assert_eq!(a.name, "s");
    assert_eq!(a.authorization, Some(RoleSpec::Name("alice".to_string())));
    let b = parse_cs("CREATE SCHEMA AUTHORIZATION alice").expect("anonymous");
    assert_eq!(b.name, "alice");
    let c = parse_cs("CREATE SCHEMA IF NOT EXISTS s").expect("plain");
    assert_eq!(c.authorization, None);
    assert!(c.if_not_exists);
    assert_eq!(
        parse_cs("CREATE SCHEMA AUTHORIZATION CURRENT_USER")
            .expect_err("no name")
            .wire_code(),
        "0A000"
    );
    assert_eq!(
        parse_cs("CREATE SCHEMA s AUTHORIZATION alice CREATE TABLE t (a INT)")
            .expect_err("elements")
            .wire_code(),
        "0A000"
    );
}

/// NoSQL（HTTP）の DDL 入口は `validate_alter_table_tokens` を直接呼ぶ。`OWNER TO` はそこへ届いても
/// 受理されず（所有者を変える経路にならない）、複数アクションの末尾に混ぜても同じ（Issue #2236。D13）。
#[test]
fn alter_table_entry_never_accepts_owner_to() {
    for sql in [
        "ALTER TABLE t OWNER TO r",
        "ALTER TABLE t ADD COLUMN c TEXT, OWNER TO r",
        "ALTER TABLE t OWNER TO r, ADD COLUMN c TEXT",
    ] {
        let tokens = tokenize(sql).expect("lex");
        assert!(validate_alter_table_tokens(&tokens).is_err(), "{sql}");
    }
}
