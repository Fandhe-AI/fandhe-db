//! 読み取り権限検査の収集の単体テスト（#1967）。分類は網羅 `match` がコンパイル時に強制する。

use crate::sql::lexer::{tokenize, Token};

fn refs(sql: &str) -> Vec<String> {
    crate::sql::search_path::resolved_relation_ref_names(&tokenize(sql).expect("lex"))
}

#[test]
fn collects_join_subquery_cte_and_set_operation_relations() {
    assert_eq!(
        refs("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id"),
        ["t1", "t2"]
    );
    assert!(refs("SELECT * FROM t1 WHERE id IN (SELECT id FROM t2)").contains(&"t2".to_string()));
    let cte = refs("WITH c AS (SELECT id FROM t3) SELECT * FROM c");
    assert!(cte.contains(&"t3".to_string()));
    assert!(!cte.contains(&"c".to_string()));
    assert!(refs("SELECT id FROM a UNION SELECT id FROM b").contains(&"b".to_string()));
    assert!(refs("SELECT * FROM a, b").contains(&"b".to_string()));
    assert!(refs("SELECT * FROM a LEFT JOIN b ON a.id = b.id").contains(&"b".to_string()));
}

#[test]
fn table_functions_are_not_relations() {
    assert!(refs("SELECT * FROM generate_series(1, 3)").is_empty());
}

#[test]
fn fetch_and_close_are_not_collected_as_relations() {
    assert!(refs("FETCH 1 FROM c").is_empty());
    assert!(refs("CLOSE c").is_empty());
}

#[test]
fn resolved_internal_keys_are_kept() {
    // 名前解決器が置き換えた内部キー（字句解析器は作れない）も収集する。
    let toks = vec![
        Token::Keyword(crate::sql::lexer::Keyword::Select),
        Token::Punct('*'),
        Token::Keyword(crate::sql::lexer::Keyword::From),
        Token::Ident("#5.t".to_string()),
    ];
    assert_eq!(
        crate::sql::search_path::resolved_relation_ref_names(&toks),
        ["#5.t"]
    );
}

#[test]
fn recursive_cte_self_reference_is_not_collected_for_dml_forms() {
    // 再帰項の `t` は CTE 自身（本文から可視）で実表ではない。書き込み系・データ変更 CTE の形でも、
    // 収集に残るのは実表（`ca`）だけ（Issue #1968）。
    let rec = "t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3)";
    for sql in [
        format!("WITH RECURSIVE {rec} INSERT INTO ca SELECT n FROM t"),
        format!("WITH RECURSIVE {rec} DELETE FROM ca WHERE n IN (SELECT n FROM t)"),
        format!("WITH RECURSIVE {rec} UPDATE ca SET n = 0 WHERE n IN (SELECT n FROM t)"),
        format!("WITH RECURSIVE d AS (DELETE FROM ca RETURNING id), {rec} SELECT n FROM t"),
    ] {
        let got = refs(&sql);
        assert!(!got.contains(&"t".to_string()), "{sql}: {got:?}");
        assert!(got.contains(&"ca".to_string()), "{sql}: {got:?}");
    }
}
