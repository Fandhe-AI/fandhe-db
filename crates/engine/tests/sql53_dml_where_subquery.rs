//! 述語形 `UPDATE`／`DELETE` の `WHERE` 内サブクエリ（構文段の受理と実行経路）の結合テスト
//! （Issue #1750・親 #1749・SQL-53 (a)）。
//!
//! 契約: 構文段は SELECT と同じ AST で受理し、受理した文は #1751 の実行経路
//! （`resolve_predicate_*_subqueries`）が束縛前に解決する。`USING OPERATION_ID` 付きは暫定ゲートが
//! 内側の実行前に `0A000` で拒否し（副作用なし）、`operation_id` 省略（既定の `Optional` 台帳モード）
//! は実行して内側を評価する。`PARTITIONED` 併用・`$n` 併用は構文／Parse 段で `42601`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql53-dml-where-subquery");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create docs");
    storage
        .create_table(&TableSchema::new(
            "langs",
            vec![ColumnDef::new("lang", ColumnType::Text, false)],
        ))
        .expect("create langs");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
}

fn seed(core: &EngineCore) {
    for (id, lang) in [(1, "ja"), (2, "en")] {
        run(
            core,
            &format!(
                "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', '{lang}') \
                 USING OPERATION_ID 'doc-{id}'"
            ),
        )
        .expect("seed doc");
    }
    run(
        core,
        "INSERT INTO langs (id, lang) VALUES (1, 'ja') USING OPERATION_ID 'lang-1'",
    )
    .expect("seed lang");
}

fn doc_rows(core: &EngineCore) -> String {
    run(core, "SELECT id, lang FROM docs ORDER BY id LIMIT 10").expect("select docs")
}

const FORMS: [&str; 6] = [
    "lang IN (SELECT lang FROM langs LIMIT 10)",
    "lang NOT IN (SELECT lang FROM langs LIMIT 10)",
    "EXISTS (SELECT id FROM langs LIMIT 1)",
    "NOT EXISTS (SELECT id FROM langs LIMIT 1)",
    "lang = (SELECT lang FROM langs LIMIT 1)",
    "lang = 'en' OR EXISTS (SELECT id FROM langs LIMIT 1)",
];

fn assert_42601(err: &SqlSurfaceError) {
    assert_eq!(err.wire_code(), "42601", "{err:?}");
}

/// `USING OPERATION_ID` 付きの各形は内側の実行前に `0A000` で拒否され、表の内容を変えない。
#[test]
fn subquery_where_dml_with_operation_id_is_rejected_without_side_effects() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    let before = doc_rows(&core);
    for (i, w) in FORMS.iter().enumerate() {
        let err = run(
            &core,
            &format!("DELETE FROM docs WHERE {w} USING OPERATION_ID 'del-{i}'"),
        )
        .expect_err("delete gated");
        assert_eq!(err.wire_code(), "0A000", "{err:?}");
        let err = run(
            &core,
            &format!("UPDATE docs SET lang = 'xx' WHERE {w} USING OPERATION_ID 'upd-{i}'"),
        )
        .expect_err("update gated");
        assert_eq!(err.wire_code(), "0A000", "{err:?}");
    }
    assert_eq!(before, doc_rows(&core));
}

/// `operation_id` 省略（既定の `Optional` 台帳モード）では内側を評価して実行する。
#[test]
fn subquery_where_dml_without_operation_id_executes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    run(
        &core,
        "UPDATE docs SET lang = 'xx' WHERE lang IN (SELECT lang FROM langs LIMIT 10)",
    )
    .expect("update executes");
    let rows = doc_rows(&core);
    assert!(rows.contains("xx") && rows.contains("en"), "{rows}");
    run(
        &core,
        "DELETE FROM docs WHERE EXISTS (SELECT id FROM langs LIMIT 1) AND lang = 'xx'",
    )
    .expect("delete executes");
    let rows = doc_rows(&core);
    assert!(!rows.contains("xx") && rows.contains("en"), "{rows}");
}

/// `PARTITIONED` 併用（`EXPLAIN` 経路を含む）は構文段で `42601`。
#[test]
fn subquery_where_with_partitioned_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    for sql in [
        "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'p-1' PARTITIONED",
        "UPDATE docs SET lang = 'xx' WHERE EXISTS (SELECT id FROM langs LIMIT 1) USING OPERATION_ID 'p-2' PARTITIONED",
        "EXPLAIN DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'p-3' PARTITIONED",
    ] {
        let err = run(&core, sql).expect_err("partitioned + subquery");
        assert_42601(&err);
    }
}

/// `RETURNING` の投影位置サブクエリは従来どおり `42601`。
#[test]
fn subquery_in_returning_projection_stays_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    let err = run(
        &core,
        "DELETE FROM docs WHERE lang = 'en' RETURNING (SELECT lang FROM langs LIMIT 1) USING OPERATION_ID 'r-1'",
    )
    .expect_err("returning subquery");
    assert_42601(&err);
}

/// 拡張クエリプロトコル: `$n` との併用は Parse で `42601`、`$n` なしの `USING OPERATION_ID` 付きは
/// 実行で `0A000`（副作用なし）。
#[test]
fn subquery_where_dml_over_extended_protocol() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    let err = core
        .parse_sql_prepared(
            "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) AND id = $1 USING OPERATION_ID 'e-1'",
        )
        .expect_err("subquery with $n");
    assert_42601(&err);
    let before = doc_rows(&core);
    let err = run(
        &core,
        "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'e-2'",
    )
    .expect_err("gated");
    assert_eq!(err.wire_code(), "0A000", "{err:?}");
    assert_eq!(before, doc_rows(&core));
}
