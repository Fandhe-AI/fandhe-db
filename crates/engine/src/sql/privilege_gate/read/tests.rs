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
