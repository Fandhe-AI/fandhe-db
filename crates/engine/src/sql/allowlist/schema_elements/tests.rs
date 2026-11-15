//! 要素つき `CREATE SCHEMA` の構文段の単体テスト（TABLE-27・Issue #3286）。分割境界・受理する形・
//! `0A000`／`42601`／`42P15`／`54000` の拒否を固定する。

use super::*;
use crate::sql::lexer::tokenize;

fn parse(sql: &str) -> Result<ValidatedCreateSchema, SqlSurfaceError> {
    validate_create_schema_tokens(&tokenize(sql).expect("lex"))
}

fn code(sql: &str) -> &'static str {
    parse(sql).expect_err(sql).wire_code()
}

#[test]
fn schema_without_elements_is_unchanged() {
    let v = parse("CREATE SCHEMA s").expect("plain");
    assert!(v.elements.is_empty());
    let v = parse("CREATE SCHEMA s;").expect("semicolon");
    assert!(v.elements.is_empty());
    let v = parse("CREATE SCHEMA IF NOT EXISTS s").expect("if not exists");
    assert!(v.elements.is_empty());
}

#[test]
fn table_view_sequence_elements_are_split_in_order() {
    let v = parse(
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE VIEW v AS SELECT a FROM t \
         CREATE SEQUENCE q;",
    )
    .expect("elements");
    let kinds: Vec<_> = v.elements.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            SchemaElementKind::Table,
            SchemaElementKind::View,
            SchemaElementKind::Sequence
        ]
    );
    // 末尾の `;` は要素に含めない。
    assert!(v
        .elements
        .iter()
        .all(|e| !matches!(e.tokens.last(), Some(Token::Punct(';')))));
}

#[test]
fn authorization_and_anonymous_forms_accept_elements() {
    let v = parse("CREATE SCHEMA s AUTHORIZATION alice CREATE TABLE t (a INT)").expect("named");
    assert_eq!(v.elements.len(), 1);
    let v = parse("CREATE SCHEMA AUTHORIZATION alice CREATE TABLE t (a INT)").expect("anon");
    assert_eq!(v.name, "alice");
    assert_eq!(v.elements.len(), 1);
}

#[test]
fn quoted_create_identifiers_do_not_split() {
    // 括弧の中の列名 "create" と、直後が導入語でない表名は境界にならない。
    let v = parse("CREATE SCHEMA s CREATE TABLE t (\"create\" INT, b INT)").expect("column");
    assert_eq!(v.elements.len(), 1);
    let v = parse("CREATE SCHEMA s CREATE TABLE \"create\" (a INT)").expect("table name");
    assert_eq!(v.elements.len(), 1);
    // 括弧の中の `CREATE TABLE` 風の並びも境界にしない（深さ 0 だけが境界）。
    let v = parse("CREATE SCHEMA s CREATE TABLE t (a INT DEFAULT 1) CREATE TABLE u (b INT)")
        .expect("two tables");
    assert_eq!(v.elements.len(), 2);
}

#[test]
fn unsupported_element_kinds_are_0a000() {
    for sql in [
        "CREATE SCHEMA s CREATE INDEX i ON t (a)",
        "CREATE SCHEMA s CREATE UNIQUE INDEX i ON t (a)",
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE INDEX i ON t (a)",
        "CREATE SCHEMA s GRANT SELECT ON t TO r",
        "CREATE SCHEMA s CREATE TABLE t (a INT) GRANT SELECT ON t TO r",
        "CREATE SCHEMA s CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE SCHEMA s CREATE MATERIALIZED VIEW m AS SELECT a FROM t",
        "CREATE SCHEMA s CREATE TEMP TABLE t (a INT)",
        "CREATE SCHEMA s CREATE UNLOGGED TABLE t (a INT)",
        "CREATE SCHEMA s CREATE OR REPLACE VIEW v AS SELECT a FROM t",
        "CREATE SCHEMA s CREATE TYPE ty AS ENUM ('a')",
        "CREATE SCHEMA s CREATE FUNCTION f() RETURNS INT AS $$ SELECT 1 $$",
        "CREATE SCHEMA s CREATE TABLE IF NOT EXISTS t (a INT)",
        "CREATE SCHEMA s CREATE SEQUENCE IF NOT EXISTS q",
        "CREATE SCHEMA s CREATE TABLE t AS SELECT a FROM u",
        "CREATE SCHEMA s CREATE VIEW v (x) AS SELECT a FROM t",
        "CREATE SCHEMA s CREATE VIEW v AS SELECT a FROM t WITH CHECK OPTION",
    ] {
        assert_eq!(code(sql), "0A000", "{sql}");
    }
}

#[test]
fn if_not_exists_with_elements_is_0a000() {
    assert_eq!(
        code("CREATE SCHEMA IF NOT EXISTS s CREATE TABLE t (a INT)"),
        "0A000"
    );
    assert_eq!(
        code("CREATE SCHEMA IF NOT EXISTS AUTHORIZATION alice CREATE TABLE t (a INT)"),
        "0A000"
    );
}

#[test]
fn malformed_elements_are_42601() {
    for sql in [
        "CREATE SCHEMA s CREATE",
        "CREATE SCHEMA s CREATE TABLE",
        "CREATE SCHEMA s CREATE TABLE t",
        "CREATE SCHEMA s CREATE TABLE t (a INT) junk",
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE VIEW",
        "CREATE SCHEMA s CREATE SCHEMA u",
        "CREATE SCHEMA s CREATE foo",
        "CREATE SCHEMA s foo",
    ] {
        assert_eq!(code(sql), "42601", "{sql}");
    }
}

#[test]
fn creating_in_another_schema_is_42p15_and_same_schema_is_accepted() {
    for sql in [
        "CREATE SCHEMA s CREATE TABLE public.t (a INT)",
        "CREATE SCHEMA s CREATE TABLE other.t (a INT)",
        "CREATE SCHEMA s CREATE SEQUENCE other.q",
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE VIEW other.v AS SELECT a FROM t",
        // 大文字小文字だけが異なる修飾名も別スキーマ扱い（実行段の名前解決と同じ完全一致）。
        "CREATE SCHEMA \"S\" CREATE TABLE s.t (a INT)",
        "CREATE SCHEMA Public CREATE TABLE public.t (a INT)",
    ] {
        assert_eq!(code(sql), "42P15", "{sql}");
    }
    let v = parse("CREATE SCHEMA s CREATE TABLE s.t (a INT) CREATE VIEW s.v AS SELECT a FROM s.t")
        .expect("same schema");
    assert_eq!(v.elements.len(), 2);
    let err = parse("CREATE SCHEMA s CREATE TABLE x.t (a INT)").expect_err("42P15");
    assert_eq!(
        err.to_string(),
        "CREATE specifies a schema (x) different from the one being created (s)"
    );
}

#[test]
fn element_count_is_capped_before_allocation() {
    let build = |n: usize| {
        let mut sql = String::from("CREATE SCHEMA s");
        for i in 0..n {
            sql.push_str(&format!(" CREATE SEQUENCE q{i}"));
        }
        sql
    };
    let ok = parse(&build(MAX_SCHEMA_ELEMENTS)).expect("at the cap");
    assert_eq!(ok.elements.len(), MAX_SCHEMA_ELEMENTS);
    assert_eq!(code(&build(MAX_SCHEMA_ELEMENTS + 1)), "54000");
}

#[test]
fn qualified_sequence_literal_in_default_is_accepted_by_the_syntax_stage() {
    // 作成中の SCHEMA で修飾した `nextval('s.q')` は、実行段が内部キーへ解決する（構文段は形だけを確かめる）。
    let v = parse(
        "CREATE SCHEMA s CREATE SEQUENCE q CREATE TABLE t (id BIGINT DEFAULT nextval('s.q') PRIMARY KEY)",
    )
    .expect("qualified nextval literal");
    assert_eq!(v.elements.len(), 2);
}
