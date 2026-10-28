//! FROM の派生表 `(SELECT ...) [[AS] 別名 [(列名, ...)]]` の構文受理と別名・列名リストの解決の結合テスト
//! （SQL-62・Issue #1865、親 Issue #1864）。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62・
//! `docs/spec/04-behavior/error-format.md`（`42P10`・ERR-6）・RLS-10 (b)。
//!
//! 実行は #1866（FROM 直後の単一の派生表。`sql62_derived_table_exec.rs`）・#1867（JOIN 入力・複数件・
//! 入れ子と深さ上限。`sql62_derived_table_join.rs`）で接続済み。
//! 本テストは「構文を受理して解決まで進む」「解決で拒否される形は専用の SQLSTATE」
//! 「非セッションの入口は従来どおり `42601`」「テナントで結果が変わらない」を固定する。
//! `sql62_cte_column_list.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn query_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    query(core, tenant, sql).expect_err("must be rejected")
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件
/// （ja）・private 1 件（ja）。carol: public 1 件（ja）。
/// `table18_view.rs::seed_base_fixture` と同一のフィクスチャ構成
/// （テナント境界検証を同条件で比較できるようにするため）。
fn seed_base_fixture(storage: &Storage) {
    insert_row(
        storage,
        &ctx("alice"),
        1,
        "ja",
        "alice public ja 1",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        2,
        "ja",
        "alice public ja 2",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        3,
        "en",
        "alice public en",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        4,
        "ja",
        "alice private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("bob"),
        5,
        "ja",
        "bob public ja",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("bob"),
        6,
        "ja",
        "bob private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("carol"),
        7,
        "ja",
        "carol public ja",
        Visibility::Public,
    );
}

fn open_seeded() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-derived-syntax");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    (new_core(storage), guard)
}

const UNSUPPORTED: &str = "0A000";

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).wire_code().to_string()
}

fn msg(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).to_string()
}

#[test]
fn derived_table_syntax_is_accepted_and_executes() {
    let (core, _g) = open_seeded();
    for sql in [
        "SELECT * FROM (SELECT lang FROM docs) t",
        "SELECT * FROM (SELECT lang FROM docs)",
        "SELECT * FROM (SELECT lang FROM docs) AS t",
        "SELECT * FROM (SELECT lang, body FROM docs) t(a)",
        "SELECT * FROM (SELECT lang, body FROM docs) t(a, b)",
        "SELECT * FROM (SELECT * FROM docs) t(a)",
        "SELECT * FROM (SELECT lang, count(*) FROM docs GROUP BY lang) d",
        "SELECT * FROM (SELECT lang FROM docs ORDER BY lang LIMIT 2) d",
        "SELECT * FROM (SELECT lang, row_number() OVER (ORDER BY lang) FROM docs) d",
    ] {
        assert!(query(&core, "alice", sql).is_ok(), "{sql}");
    }
    // 存在しない列（元の名前は列名リストで隠れる）は `42703`。
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT * FROM (SELECT lang FROM docs) t WHERE a = 1"
        ),
        "42703"
    );
    // 別名が実表名と同じでも実表は読まれず、本文の結果（1 列）だけを返す。
    let r = query(
        &core,
        "alice",
        "SELECT * FROM (SELECT lang FROM docs WHERE lang = 'en') AS docs",
    )
    .expect("select");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.columns.len(), 1);
}

/// `union` 等は通常の識別子であり、後続に枝が無ければ集合演算とみなさず派生表の抽出を続ける。
#[test]
fn set_operator_words_as_identifiers_do_not_abort_extraction() {
    let (core, _g) = open_seeded();
    for sql in [
        "SELECT lang AS union FROM (SELECT lang FROM docs) t",
        "SELECT lang AS except FROM (SELECT lang FROM docs) t",
    ] {
        assert!(query(&core, "alice", sql).is_ok(), "{sql}");
    }
    // 別名の位置の `union` は外側の構文検証が拒否する（実測。PostgreSQL も構文エラー）。
    assert!(query(
        &core,
        "alice",
        "SELECT * FROM (SELECT lang FROM docs) union"
    )
    .is_err());
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT lang AS union FROM (SELECT lang FROM docs) t(a, b, c)"
        ),
        "42P10"
    );
}

#[test]
fn derived_table_column_list_too_long_is_42p10() {
    let (core, _g) = open_seeded();
    let sql = "SELECT * FROM (SELECT lang FROM docs) t(a, b, c)";
    assert_eq!(code(&core, "alice", sql), "42P10");
    assert_eq!(
        msg(&core, "alice", sql),
        "table \"t\" has 1 columns available but 3 columns specified"
    );
    for sql in [
        "SELECT * FROM (SELECT * FROM docs) t(a,b,c,d,e,f,g,h,i,j)",
        "SELECT * FROM (SELECT lang, count(*) FROM docs GROUP BY lang) t(a, b, c)",
        "SELECT * FROM (SELECT lang FROM docs LIMIT 1) t(a, b)",
        "SELECT * FROM (SELECT lang, row_number() OVER (ORDER BY lang) FROM docs) t(a, b, c)",
    ] {
        assert_eq!(code(&core, "alice", sql), "42P10", "{sql}");
    }
}

#[test]
fn derived_table_syntax_and_resolution_errors() {
    let (core, _g) = open_seeded();
    for (sql, expected) in [
        ("SELECT * FROM (SELECT lang FROM docs) (a)", "42601"),
        (
            "SELECT * FROM (SELECT lang, body FROM docs) t(a, a)",
            "42701",
        ),
        ("SELECT * FROM (SELECT lang, body FROM docs) t()", "42601"),
        (
            "SELECT * FROM (SELECT lang, body FROM docs) t(body)",
            "42702",
        ),
        ("SELECT * FROM (SELECT lang FROM nope) t", "42P01"),
        ("SELECT * FROM (SELECT nope FROM docs) t", "42703"),
        (
            "SELECT * FROM (SELECT lang FROM docs) t, (SELECT body FROM docs) t",
            "42601",
        ),
        ("SELECT * FROM (SELECT lang FROM docs;) t", "42601"),
        (
            "SELECT * FROM LATERAL (SELECT lang FROM docs) t",
            UNSUPPORTED,
        ),
    ] {
        assert_eq!(code(&core, "alice", sql), expected, "{sql}");
    }
}

#[test]
fn derived_table_in_join_positions() {
    let (core, _g) = open_seeded();
    for sql in [
        "SELECT * FROM docs d JOIN (SELECT lang FROM docs) t ON d.lang = t.lang",
        "SELECT * FROM docs d, (SELECT lang FROM docs) t",
        "SELECT * FROM (SELECT lang FROM docs) a JOIN (SELECT body FROM docs) b ON a.lang = b.body",
    ] {
        assert!(query(&core, "alice", sql).is_ok(), "{sql}");
    }
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT * FROM docs d JOIN (SELECT lang FROM docs) t(a, b) ON d.lang = t.a"
        ),
        "42P10"
    );
}

#[test]
fn derived_table_with_cte_and_data_modifying_cte() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "WITH c AS (SELECT lang FROM docs) SELECT * FROM (SELECT * FROM c) d",
    )
    .expect("with + derived");
    assert_eq!(r.rows.len(), 6);
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c AS (SELECT lang FROM docs) SELECT * FROM (SELECT * FROM c) d(x, y, z)"
        ),
        "42P10"
    );
    let before = query(&core, "alice", "SELECT lang FROM docs")
        .expect("select")
        .rows
        .len();
    let sql = "WITH w AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT * FROM (SELECT lang FROM docs) d";
    assert_eq!(code(&core, "alice", sql), UNSUPPORTED);
    // 書き込みは発生しない。
    let r = query(&core, "alice", "SELECT lang FROM docs").expect("select");
    assert_eq!(r.rows.len(), before);
}

#[test]
fn derived_table_not_recognized_outside_session_select() {
    let (core, _g) = open_seeded();
    for sql in [
        "EXPLAIN SELECT * FROM (SELECT lang FROM docs) t",
        "SELECT * FROM docs WHERE lang IN (SELECT lang FROM (SELECT lang FROM docs) s)",
    ] {
        assert_eq!(code(&core, "alice", sql), "42601", "{sql}");
    }
}

#[test]
fn non_derived_forms_are_unchanged() {
    let (core, _g) = open_seeded();
    let r = query(&core, "alice", "SELECT lang FROM docs WHERE lang = 'en'").expect("select");
    assert_eq!(r.rows.len(), 1);
    let r = query(
        &core,
        "alice",
        "SELECT lang FROM docs WHERE lang IN (SELECT lang FROM docs WHERE lang = 'en')",
    )
    .expect("subquery");
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn derived_table_errors_do_not_depend_on_tenant() {
    let (core, _g) = open_seeded();
    for sql in [
        "SELECT * FROM (SELECT lang FROM docs) t(a, b)",
        "SELECT * FROM (SELECT lang FROM nope) t",
        "SELECT * FROM (SELECT lang FROM docs) a, (SELECT lang FROM nope) b",
    ] {
        let a = (code(&core, "alice", sql), msg(&core, "alice", sql));
        for tenant in ["bob", "carol"] {
            assert_eq!(
                a,
                (code(&core, tenant, sql), msg(&core, tenant, sql)),
                "{sql}"
            );
        }
    }
}
