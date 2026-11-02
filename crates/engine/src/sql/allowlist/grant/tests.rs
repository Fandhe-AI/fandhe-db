//! `GRANT`／`REVOKE` の構文検証の単体テスト（SQL-40、Issue #1965）。構文の受理・`42601`・`0LP01`・`0A000` を
//! 網羅する。カタログは照会しない層のため、ストレージ無しで動く。

use super::*;
use crate::sql::lexer::tokenize;

fn parse(sql: &str) -> Result<ValidatedGrantRevoke, SqlSurfaceError> {
    validate_grant_revoke_tokens(&tokenize(sql).expect("lex"))
}

fn objects(sql: &str) -> GrantObjects {
    match parse(sql).expect("parse").target {
        GrantTarget::Objects(o) => o,
        GrantTarget::Roles(_) => panic!("expected object form"),
    }
}

fn roles(sql: &str) -> GrantRoles {
    match parse(sql).expect("parse").target {
        GrantTarget::Roles(r) => r,
        GrantTarget::Objects(_) => panic!("expected role form"),
    }
}

fn code(sql: &str) -> &'static str {
    parse(sql).expect_err("must be rejected").wire_code()
}

#[test]
fn single_privileges_map_to_bits() {
    for (word, bit) in [
        ("SELECT", PRIV_SELECT),
        ("INSERT", PRIV_INSERT),
        ("UPDATE", PRIV_UPDATE),
        ("DELETE", PRIV_DELETE),
        ("TRUNCATE", PRIV_TRUNCATE),
        ("REFERENCES", PRIV_REFERENCES),
        ("TRIGGER", PRIV_TRIGGER),
        ("MAINTAIN", PRIV_MAINTAIN),
    ] {
        let o = objects(&format!("GRANT {word} ON t TO alice"));
        assert_eq!(
            o.items,
            vec![PrivItem {
                bits: bit,
                columns: vec![]
            }],
            "{word}"
        );
    }
}

#[test]
fn all_and_all_privileges_expand_to_table_set() {
    for sql in [
        "GRANT ALL ON t TO alice",
        "GRANT ALL PRIVILEGES ON TABLE t TO alice",
        "grant all privileges on t to alice",
    ] {
        let o = objects(sql);
        assert_eq!(o.items.len(), 1);
        assert_eq!(o.items[0].bits, TABLE_PRIVILEGES, "{sql}");
    }
}

#[test]
fn all_with_columns_expands_to_column_set() {
    let o = objects("GRANT ALL (a, b) ON t TO alice");
    assert_eq!(o.items[0].bits, COLUMN_PRIVILEGES);
    assert_eq!(o.items[0].columns, vec!["a", "b"]);
}

#[test]
fn multiple_privileges_columns_tables_and_grantees() {
    let o = objects(
        "GRANT SELECT (a, b, a), UPDATE (c), INSERT ON TABLE t1, t2, t1 TO alice, bob, alice",
    );
    assert_eq!(o.items.len(), 3);
    assert_eq!(o.items[0].columns, vec!["a", "b"], "duplicates removed");
    assert_eq!(o.items[1].bits, PRIV_UPDATE);
    assert!(o.items[2].columns.is_empty());
    assert_eq!(o.tables, vec!["t1", "t2"]);
    assert_eq!(
        o.grantees,
        vec![Grantee::Role("alice".into()), Grantee::Role("bob".into())]
    );
    assert!(!o.grant_option && !o.cascade);
}

#[test]
fn table_keyword_is_optional_and_a_table_may_be_named_table() {
    assert_eq!(objects("GRANT SELECT ON TABLE t TO a").tables, vec!["t"]);
    assert_eq!(objects("GRANT SELECT ON t TO a").tables, vec!["t"]);
    // 次が TO のときは表名 `table`（引用）。
    assert_eq!(
        objects("GRANT SELECT ON \"table\" TO a").tables,
        vec!["table"]
    );
}

#[test]
fn grantee_forms() {
    let o = objects(
        "GRANT SELECT ON t TO PUBLIC, CURRENT_USER, current_role, SESSION_USER, GROUP g, bob",
    );
    assert_eq!(
        o.grantees,
        vec![
            Grantee::Public,
            Grantee::CurrentRole,
            Grantee::SessionUser,
            Grantee::Role("g".into()),
            Grantee::Role("bob".into()),
        ]
    );
}

#[test]
fn with_grant_option_and_revoke_forms() {
    assert!(objects("GRANT SELECT ON t TO alice WITH GRANT OPTION").grant_option);
    let r = parse("REVOKE SELECT ON t FROM alice").expect("parse");
    assert!(r.is_revoke());
    let o = objects("REVOKE GRANT OPTION FOR SELECT ON t FROM alice CASCADE");
    assert!(o.grant_option && o.cascade);
    let o = objects("REVOKE SELECT ON t FROM alice, PUBLIC RESTRICT");
    assert!(!o.grant_option && !o.cascade);
    assert_eq!(o.grantees.len(), 2);
    assert!(!parse("GRANT SELECT ON t TO a").expect("parse").is_revoke());
    // 末尾のセミコロンは 1 つだけ許す。
    assert!(parse("GRANT SELECT ON t TO a;").is_ok());
}

#[test]
fn role_form_is_structured() {
    let r = roles("GRANT r1, r2 TO u WITH ADMIN OPTION, INHERIT FALSE, SET TRUE");
    assert_eq!(r.roles, vec!["r1", "r2"]);
    assert_eq!(
        r.with_options,
        vec![
            (RoleOption::Admin, true),
            (RoleOption::Inherit, false),
            (RoleOption::Set, true)
        ]
    );
    let r = roles("REVOKE ADMIN OPTION FOR r FROM u CASCADE");
    assert_eq!(r.option_for, Some(RoleOption::Admin));
    assert!(r.cascade);
    let r = roles("REVOKE SET OPTION FOR r FROM u");
    assert_eq!(r.option_for, Some(RoleOption::Set));
    assert!(roles("GRANT r TO PUBLIC").with_options.is_empty());
}

#[test]
fn syntax_errors_are_42601() {
    for sql in [
        "GRANT SELECT t TO a",
        "GRANT SELECT ON t",
        "GRANT SELECT ON t FROM a",
        "REVOKE SELECT ON t TO a",
        "GRANT SELECT ON TO a",
        "GRANT ON t TO a",
        "GRANT FROBNICATE ON t TO a",
        "GRANT SELECT () ON t TO a",
        "GRANT SELECT (a ON t TO a",
        "GRANT SELECT (a b) ON t TO a",
        "GRANT SELECT ON t TO",
        "GRANT SELECT ON t TO a,",
        "GRANT SELECT ON t TO a WITH GRANT",
        "GRANT SELECT ON t TO a WITH GRANT OPTION WITH GRANT OPTION",
        "GRANT SELECT ON t TO a extra",
        "GRANT SELECT ON t TO a; SELECT 1",
        "GRANT SELECT ON t TO a CASCADE",
        "GRANT r TO u WITH ADMIN",
        "GRANT r TO u WITH ADMIN OPTION, ADMIN FALSE",
        "GRANT r TO u WITH FROB OPTION",
        "REVOKE r FROM",
        "GRANT",
        "GRANT SELECT ON t TO s.alice",
        "GRANT SELECT ON t TO a GRANTED",
    ] {
        assert_eq!(code(sql), "42601", "{sql}");
    }
}

#[test]
fn invalid_privilege_for_object_is_0lp01() {
    for sql in [
        "GRANT USAGE ON TABLE t TO a",
        "GRANT CREATE ON t TO a",
        "GRANT EXECUTE ON t TO a",
        "GRANT CONNECT ON t TO a",
        "GRANT DELETE (a) ON t TO a",
        "GRANT TRUNCATE (a) ON t TO a",
        "GRANT SELECT ON t TO PUBLIC WITH GRANT OPTION",
    ] {
        assert_eq!(code(sql), "0LP01", "{sql}");
    }
    // REVOKE GRANT OPTION FOR は PUBLIC でも構文上は有効。
    assert!(parse("REVOKE GRANT OPTION FOR SELECT ON t FROM PUBLIC").is_ok());
}

#[test]
fn out_of_scope_forms_are_0a000() {
    for sql in [
        "GRANT USAGE ON SCHEMA public TO a",
        "GRANT SELECT ON SEQUENCE s TO a",
        "GRANT EXECUTE ON FUNCTION f TO a",
        "GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA s TO a",
        "GRANT SELECT ON ALL SEQUENCES IN SCHEMA s TO a",
        "GRANT SELECT ON ALL TABLES IN SCHEMA information_schema TO a",
        "GRANT CONNECT ON DATABASE d TO a",
        "REVOKE USAGE ON SCHEMA pg_temp FROM a",
        "GRANT SELECT ON t TO a GRANTED BY b",
        "REVOKE SELECT ON t FROM a GRANTED BY b",
        "GRANT r TO u GRANTED BY b",
        "GRANT SELECT ON s.t TO a",
    ] {
        assert_eq!(code(sql), "0A000", "{sql}");
    }
    assert!(is_alter_default_privileges_statement(
        &tokenize("ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO a").expect("lex")
    ));
    assert_eq!(reject_alter_default_privileges().wire_code(), "0A000");
    // 種別語と同名の表は表として扱う。
    assert_eq!(
        objects("GRANT SELECT ON \"schema\" TO a").tables,
        vec!["schema"]
    );
}

#[test]
fn list_lengths_are_bounded() {
    let names: Vec<String> = (0..=MAX_GRANT_LIST_LEN).map(|i| format!("t{i}")).collect();
    let sql = format!("GRANT SELECT ON {} TO a", names.join(", "));
    assert_eq!(code(&sql), "54000");
    let ok: Vec<String> = (0..MAX_GRANT_LIST_LEN).map(|i| format!("t{i}")).collect();
    assert!(parse(&format!("GRANT SELECT ON {} TO a", ok.join(", "))).is_ok());
}

#[test]
fn statement_heads_are_detected() {
    let t = |s: &str| tokenize(s).expect("lex");
    assert!(is_grant_statement(&t("GRANT SELECT ON t TO a")));
    assert!(is_revoke_statement(&t("revoke select on t from a")));
    assert!(!is_grant_statement(&t("SELECT 1")));
    assert!(!is_alter_default_privileges_statement(&t(
        "ALTER TABLE t ADD COLUMN c TEXT"
    )));
}

#[test]
fn schema_forms_are_structured() {
    use crate::catalog::privilege::{PRIV_CREATE, PRIV_SELECT, PRIV_USAGE};
    let o = objects("GRANT USAGE, CREATE ON SCHEMA s1, s2 TO a, PUBLIC");
    assert_eq!(o.scope, GrantScope::Schemas);
    assert_eq!(o.schemas, vec!["s1", "s2"]);
    assert!(o.tables.is_empty());
    assert_eq!(
        o.items.iter().map(|i| i.bits).collect::<Vec<_>>(),
        vec![PRIV_USAGE, PRIV_CREATE]
    );
    assert_eq!(
        objects("GRANT ALL PRIVILEGES ON SCHEMA s TO a").items[0].bits,
        PRIV_USAGE | PRIV_CREATE
    );
    let r = objects("REVOKE GRANT OPTION FOR USAGE ON SCHEMA s FROM a CASCADE");
    assert!(r.grant_option && r.cascade);
    let all = objects("GRANT SELECT ON ALL TABLES IN SCHEMA s TO a WITH GRANT OPTION");
    assert_eq!(all.scope, GrantScope::AllTablesInSchemas);
    assert_eq!(all.schemas, vec!["s"]);
    assert_eq!(all.items[0].bits, PRIV_SELECT);
    assert!(all.grant_option);
    // `schema` という名前の表は従来どおり表として扱う。
    assert_eq!(
        objects("GRANT SELECT ON TABLE \"schema\" TO a").scope,
        GrantScope::Tables
    );
}

#[test]
fn schema_forms_reject_bad_input() {
    // 構文: 修飾名・列リスト・句の欠落。
    for sql in [
        "GRANT USAGE ON SCHEMA a.b TO a",
        "GRANT USAGE (c) ON SCHEMA s TO a",
        "GRANT SELECT (c) ON ALL TABLES IN SCHEMA s TO a",
        "GRANT USAGE ON SCHEMA s",
        "GRANT USAGE ON SCHEMA s TO",
    ] {
        assert_eq!(code(sql), "42601", "{sql}");
    }
    // 種別に許されない権限・PUBLIC への GRANT OPTION。
    for sql in [
        "GRANT SELECT ON SCHEMA s TO a",
        "GRANT USAGE, INSERT ON SCHEMA s TO a",
        "GRANT USAGE ON SCHEMA s TO PUBLIC WITH GRANT OPTION",
        "GRANT USAGE ON ALL TABLES IN SCHEMA s TO a",
        "GRANT CONNECT ON SCHEMA s TO a",
    ] {
        assert_eq!(code(sql), "0LP01", "{sql}");
    }
}
