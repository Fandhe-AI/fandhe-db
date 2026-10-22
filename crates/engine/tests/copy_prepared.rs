//! 拡張クエリの Parse／Bind／Describe／Execute が `COPY` 文を扱う engine 層の結合テスト
//! （Issue #1759・WIRE-21 (1)・ERR-6 のポインタ）。wire-server 側のメッセージ層は
//! `crates/wire-server/tests/wire21_copy_parse_bind.rs` の担当。ここでは
//! `EngineCore::parse_sql_prepared`・`bind_prepared`・`describe_prepared_in_session` と
//! `execute_parsed_in_session` の契約（`ParsedSql::Copy` の保持・`$n` の `42P02`・
//! 簡易クエリ `begin_copy` と同じ検証分類・Execute の `0A000` スタブ）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, ParsedSql};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("ctx")
}

const FROM_SQL: &str = "COPY documents (id, embedding, body) FROM STDIN";
const TO_SQL: &str = "COPY (SELECT id FROM documents LIMIT 10) TO STDOUT";

#[test]
fn parse_keeps_validated_copy_with_original_sql() {
    let (core, _g) = open("copy-prepared-keep");
    for sql in [FROM_SQL, TO_SQL] {
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        assert_eq!(prepared.param_count(), 0);
        let Some(ParsedSql::Copy(copy)) = prepared.parsed_if_unparameterized() else {
            panic!("expected ParsedSql::Copy");
        };
        assert_eq!(copy.sql(), sql);
    }
}

#[test]
fn placeholder_is_42p02_but_not_inside_string_literal() {
    let (core, _g) = open("copy-prepared-param");
    let err = core
        .parse_sql_prepared("COPY (SELECT id FROM documents WHERE id = $2 LIMIT 1) TO STDOUT")
        .map(|_| ())
        .expect_err("must be rejected");
    assert_eq!(err.wire_code(), "42P02");
    assert!(err.to_string().contains("$2"));
    core.parse_sql_prepared(
        "COPY (SELECT id FROM documents WHERE body = 'costs $1' LIMIT 1) TO STDOUT",
    )
    .expect("literal is not a parameter");
}

#[test]
fn validation_class_matches_simple_query_begin_copy() {
    let (core, _g) = open("copy-prepared-class");
    let session = SessionState::default();
    for sql in [
        "COPY nosuch (id, embedding) FROM STDIN",
        "COPY documents (id, embedding, body) FROM STDIN WITH (FORMAT binary, DELIMITER ',')",
        "COPY documents (id, id, body) FROM STDIN",
    ] {
        let simple = core
            .begin_copy(&ctx(), &session, sql)
            .map(|_| ())
            .expect_err("simple");
        let prepared = core
            .parse_sql_prepared(sql)
            .map(|_| ())
            .expect_err("prepared");
        assert_eq!(prepared.wire_code(), simple.wire_code(), "{sql}");
    }
}

#[test]
fn bind_returns_copy_and_describe_has_no_columns() {
    let (core, _g) = open("copy-prepared-bind");
    let session = SessionState::default();
    let prepared = core.parse_sql_prepared(TO_SQL).expect("parse");
    let parsed = core.bind_prepared(&prepared, &[]).expect("bind");
    assert!(matches!(parsed, ParsedSql::Copy(_)));
    assert!(core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe")
        .is_none());
    assert!(core
        .bind_prepared(&prepared, &[Some(b"1".to_vec())])
        .is_err());
}

#[test]
fn execute_is_0a000_without_side_effects() {
    let (core, _g) = open("copy-prepared-exec");
    let mut session = SessionState::default();
    let prepared = core.parse_sql_prepared(FROM_SQL).expect("parse");
    let parsed = core.bind_prepared(&prepared, &[]).expect("bind");
    let err = core
        .execute_parsed_in_session(&ctx(), &mut session, &parsed)
        .expect_err("stub rejects");
    assert_eq!(err.wire_code(), "0A000");
}

/// 引用付き `"array"` の表名・列名は簡易クエリ `begin_copy` と拡張クエリ Parse の双方で
/// 同じく受理される（引用付き ARRAY が `QuotedNull` のまま 42601 にならない回帰）。
#[test]
fn quoted_array_identifier_is_accepted_in_both_paths() {
    let path = unique_db_path("copy-prepared-quoted-array");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "array",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("array", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let session = SessionState::default();
    let sql = r#"COPY "array" (id, embedding, "array") FROM STDIN"#;
    core.begin_copy(&ctx(), &session, sql).expect("simple");
    let prepared = core.parse_sql_prepared(sql).expect("prepared");
    assert!(matches!(
        prepared.parsed_if_unparameterized(),
        Some(ParsedSql::Copy(_))
    ));
}
