//! `row_compare_relation_names`（`core.rs`）の単体テスト（Issue #3257）。

use super::row_compare_relation_names;
use crate::sql::lexer::tokenize;

fn names(sql: &str) -> Vec<String> {
    row_compare_relation_names(&tokenize(sql).expect("lex"))
}

#[test]
fn extract_from_is_not_a_relation() {
    assert_eq!(
        names("SELECT EXTRACT(year FROM ts) FROM t WHERE (id, a) > (1, 2) LIMIT 10"),
        vec!["t"]
    );
}

#[test]
fn is_distinct_from_is_not_a_relation() {
    assert_eq!(
        names("SELECT a FROM t WHERE a IS DISTINCT FROM b"),
        vec!["t"]
    );
}

#[test]
fn using_operation_id_is_not_a_relation() {
    assert_eq!(
        names("DELETE FROM t WHERE (id, a) > (1, 2) USING OPERATION_ID 'x'"),
        vec!["t"]
    );
}

#[test]
fn qualified_and_comma_separated_relations() {
    assert_eq!(names("SELECT a FROM public.u"), vec!["u"]);
    assert_eq!(
        names("SELECT a FROM a, b x, c AS y WHERE a.k = b.k"),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        names("DELETE FROM t USING u, public.v WHERE t.k = u.k"),
        vec!["t", "u", "v"]
    );
}

#[test]
fn join_relations() {
    assert_eq!(
        names("SELECT a FROM t JOIN u ON t.k = u.k LEFT JOIN v ON u.k = v.k"),
        vec!["t", "u", "v"]
    );
}

#[test]
fn quoted_alias_does_not_stop_scan() {
    assert_eq!(
        names(r#"UPDATE t SET a = 1 FROM u AS "X" , v "Y" WHERE t.k = u.k"#),
        vec!["u", "v"]
    );
    assert_eq!(
        names(r#"DELETE FROM t "T" USING u "U" WHERE t.k = u.k"#),
        vec!["t", "u"]
    );
}
