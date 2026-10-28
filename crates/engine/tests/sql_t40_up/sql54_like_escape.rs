//! `WHERE <col> [NOT] LIKE|ILIKE '<pattern>' ESCAPE '<c>'` の結合テスト（SQL-54、
//! Issue #1779。ポインタ: `docs/spec/04-behavior/` SQL-54）。ESCAPE は構文層で既定の
//! `\` エスケープ形へ正規化される（ADR `docs/design/like-wildcard-patterns.md` の
//! 「実装メモ（Issue #1779）」参照）。末尾の孤立エスケープ・不正な ESCAPE 値は `22025`。
//!
//! `tests/sql54_ilike.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `engine::tenant::insert_typed_row` による投入）で `EngineCore::execute_sql` を検証する。

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

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

const OTHER_TENANT_ID: u64 = 100;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("name", ColumnType::Text, true),
        ],
    )
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, name: Option<&str>, vis: Visibility) {
    let name = match name {
        Some(s) => Value::Text(s.to_string()),
        None => Value::Null,
    };
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx(tenant),
        id,
        vis,
        &[Value::Vector(vec![id as f32, 0.0]), name],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// 値にワイルドカード・エスケープ候補・バックスラッシュ・マルチバイトを含む行集合。
const ROWS: &[(u64, Option<&str>)] = &[
    (1, Some("a%b")),
    (2, Some("a_b")),
    (3, Some("a!b")),
    (4, Some("axb")),
    (5, Some("a\\b")),
    (6, None),
    (7, Some("A%B")),
    (8, Some("x%y")),
];

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (id, name) in ROWS {
        insert_row(&storage, "tenant-a", *id, *name, Visibility::Public);
    }
    // 他テナントの Private 行（どの述語にも合う値）。可視性境界の確認用。
    insert_row(
        &storage,
        "tenant-b",
        OTHER_TENANT_ID,
        Some("a%b"),
        Visibility::Private,
    );
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ids(result: &QueryResult) -> Vec<u64> {
    assert!(
        result.rows.iter().all(|r| r.id != OTHER_TENANT_ID),
        "other tenant row leaked"
    );
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn select(core: &EngineCore, pred: &str) -> Vec<u64> {
    let sql =
        format!("SELECT * FROM docs WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100");
    let result = core
        .execute_sql(&ctx("tenant-a"), &sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
    ids(&result)
}

fn code_of_pred(core: &EngineCore, pred: &str) -> &'static str {
    let sql =
        format!("SELECT * FROM docs WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10");
    core.execute_sql(&ctx("tenant-a"), &sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

fn run_dml(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("tenant-a"), &mut SessionState::default(), sql)
}

#[test]
fn custom_escape_makes_wildcards_and_itself_literal() {
    let path = unique_db_path("sql54e-custom");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    // `%`・`_`・エスケープ文字自身のリテラル化。
    assert_eq!(select(&core, "name LIKE 'a!%b' ESCAPE '!'"), vec![1]);
    assert_eq!(select(&core, "name LIKE 'a!_b' ESCAPE '!'"), vec![2]);
    assert_eq!(select(&core, "name LIKE 'a!!b' ESCAPE '!'"), vec![3]);
    // エスケープ文字の直後のその他の文字はそのままリテラル。
    assert_eq!(select(&core, "name LIKE 'a!xb' ESCAPE '!'"), vec![4]);
    // エスケープされていない `_` はワイルドカードのまま。
    assert_eq!(
        select(&core, "name LIKE 'a_b' ESCAPE '!'"),
        vec![1, 2, 3, 4, 5]
    );
    // 独自のエスケープ文字のとき `\` はただの文字。
    assert_eq!(select(&core, "name LIKE 'a\\b' ESCAPE '!'"), vec![5]);
    // 前方一致・後方一致の一般形にも効く。
    assert_eq!(select(&core, "name LIKE 'a!%%' ESCAPE '!'"), vec![1]);
    assert_eq!(select(&core, "name LIKE '%!%b' ESCAPE '!'"), vec![1]);
}

#[test]
fn empty_escape_disables_escaping_and_backslash_escape_is_default() {
    let path = unique_db_path("sql54e-empty");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    // `ESCAPE ''`: `\` はリテラル、`%`／`_` はワイルドカードのまま。
    assert_eq!(select(&core, "name LIKE 'a\\%' ESCAPE ''"), vec![5]);
    assert_eq!(
        select(&core, "name LIKE 'a_b' ESCAPE ''"),
        vec![1, 2, 3, 4, 5]
    );
    // `ESCAPE '\'` は ESCAPE を書かない場合と同じ。
    assert_eq!(
        select(&core, "name LIKE 'a\\%b' ESCAPE '\\'"),
        select(&core, "name LIKE 'a\\%b'")
    );
    assert_eq!(select(&core, "name LIKE 'a\\%b' ESCAPE '\\'"), vec![1]);
}

#[test]
fn multibyte_and_wildcard_chars_can_be_the_escape() {
    let path = unique_db_path("sql54e-multibyte");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    assert_eq!(select(&core, "name LIKE 'x日%y' ESCAPE '日'"), vec![8]);
    // `%` 自身をエスケープ文字にすると `%%` が リテラルの `%` になる。
    assert_eq!(select(&core, "name LIKE 'a%%b' ESCAPE '%'"), vec![1]);
    assert_eq!(select(&core, "name LIKE 'a__b' ESCAPE '_'"), vec![2]);
}

#[test]
fn ilike_and_negations_honor_escape() {
    let path = unique_db_path("sql54e-ilike");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    assert_eq!(select(&core, "name ILIKE 'A!%B' ESCAPE '!'"), vec![1, 7]);
    // LIKE は大文字小文字を区別する。
    assert_eq!(select(&core, "name LIKE 'A!%B' ESCAPE '!'"), vec![7]);
    // NULL 行（id=6）は NOT でも不一致。
    assert_eq!(
        select(&core, "name NOT LIKE 'a!%b' ESCAPE '!'"),
        vec![2, 3, 4, 5, 7, 8]
    );
    assert_eq!(
        select(&core, "name NOT ILIKE 'a!%b' ESCAPE '!'"),
        vec![2, 3, 4, 5, 8]
    );
}

#[test]
fn trailing_escape_and_invalid_escape_value_are_22025() {
    let path = unique_db_path("sql54e-22025");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    for pred in [
        "name LIKE 'abc!' ESCAPE '!'",
        "name LIKE '!' ESCAPE '!'",
        "name ILIKE 'abc!' ESCAPE '!'",
        "name NOT LIKE 'abc!' ESCAPE '!'",
        "name LIKE 'a' ESCAPE 'ab'",
        // 既定エスケープの末尾単独 `\`。
        "name LIKE 'src\\'",
        "name LIKE 'src\\' ESCAPE '\\'",
    ] {
        assert_eq!(code_of_pred(&core, pred), "22025", "pred={pred}");
    }
}

#[test]
fn unsupported_escape_forms_stay_42601() {
    let path = unique_db_path("sql54e-42601");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    for pred in [
        "name LIKE 'a' ESCAPE name",
        "name LIKE 'a' ESCAPE NULL",
        "name LIKE 'a' ESCAPE 1",
        "name LIKE 'a' ESCAPE '!'::text",
        "name LIKE 'a' ESCAPE",
    ] {
        assert_eq!(code_of_pred(&core, pred), "42601", "pred={pred}");
    }
}

#[test]
fn pattern_length_limit_applies_before_escape_normalization() {
    let path = unique_db_path("sql54e-len");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    let over = "a".repeat(4097);
    assert_eq!(
        code_of_pred(&core, &format!("name LIKE '{over}' ESCAPE '!'")),
        "54000"
    );
    let at_limit = "a".repeat(4096);
    assert_eq!(
        select(&core, &format!("name LIKE '{at_limit}' ESCAPE '!'")),
        Vec::<u64>::new()
    );
}

#[test]
fn escape_is_a_contextual_word_not_a_reserved_keyword() {
    let path = unique_db_path("sql54e-ident");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "escdocs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("escape", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    engine::tenant::insert_typed_row(
        &storage,
        "escdocs",
        &ctx("tenant-a"),
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("x".to_string())],
        &OperationId::parse("seed-esc-1").expect("valid operation id"),
    )
    .expect("insert row");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    for pred in [
        "escape = 'x'",
        "escape LIKE 'x%'",
        "escape LIKE 'x' ESCAPE '!'",
    ] {
        let sql = format!(
            "SELECT * FROM escdocs WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        );
        let result = core
            .execute_sql(&ctx("tenant-a"), &sql)
            .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
        assert_eq!(ids(&result), vec![1], "pred={pred}");
    }
}

#[test]
fn simple_view_body_with_escape_round_trips() {
    let path = unique_db_path("sql54e-view");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    ddl(
        &core,
        "CREATE VIEW ve AS SELECT * FROM docs WHERE name LIKE 'a!%b' ESCAPE '!'",
    );
    let result = core
        .execute_sql(&ctx("tenant-a"), "SELECT id, name FROM ve LIMIT 100")
        .expect("view select");
    assert_eq!(ids(&result), vec![1]);
}

#[test]
fn dml_with_escape_shares_content_hash_with_backslash_form() {
    let path = unique_db_path("sql54e-dml");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    let del = "DELETE FROM docs WHERE name LIKE 'a!%b' ESCAPE '!' USING OPERATION_ID 'op-1779-del'";
    match run_dml(&core, del).expect("ESCAPE DELETE") {
        SqlOutcome::Delete(d) => assert_eq!(d.rows_affected, 1),
        other => panic!("expected Delete, got {other:?}"),
    }
    assert_eq!(
        select(&core, "name LIKE 'a_b' ESCAPE '!'"),
        vec![2, 3, 4, 5]
    );

    // 意味が同じ `\` エスケープ形の再送は同じ内容として冪等に扱われる。
    let replay = run_dml(
        &core,
        "DELETE FROM docs WHERE name LIKE 'a\\%b' USING OPERATION_ID 'op-1779-del'",
    );
    assert!(
        match &replay {
            Ok(SqlOutcome::Delete(d)) => d.rows_affected == 1,
            Err(e) => e.wire_code() == "23505",
            _ => false,
        },
        "equivalent backslash form must replay idempotently: {replay:?}"
    );

    // エスケープなしの形（意味が異なる）は内容不一致（`22023`）。
    let mismatch = run_dml(
        &core,
        "DELETE FROM docs WHERE name LIKE 'a!%b' USING OPERATION_ID 'op-1779-del'",
    )
    .expect_err("different meaning must mismatch");
    assert_eq!(mismatch.wire_code(), "22023");
}

#[test]
fn check_constraint_with_escape_round_trips() {
    let path = unique_db_path("sql54e-check");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(
        &ctx("tenant-a"),
        &mut session,
        "CREATE TABLE esc_ck (path TEXT CONSTRAINT esc_ck_path CHECK (path LIKE 'a!%%' ESCAPE '!'))",
    )
    .expect("create table with ESCAPE check");
    let err = core
        .execute_insert_sql(
            &ctx("tenant-a"),
            "INSERT INTO esc_ck (id, path) VALUES (1, 'axb') USING OPERATION_ID 'op-ck-1'",
        )
        .expect_err("value without a literal percent prefix must violate the CHECK");
    assert_eq!(err.wire_code(), "23514", "{err:?}");
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO esc_ck (id, path) VALUES (2, 'a%z') USING OPERATION_ID 'op-ck-2'",
    )
    .expect("conforming row must be accepted");
}
