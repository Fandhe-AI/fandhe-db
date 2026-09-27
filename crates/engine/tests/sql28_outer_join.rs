//! `LEFT`／`RIGHT`／`FULL [OUTER]` JOIN（2 テーブル外部結合。Issue #926。
//! ポインタ: SQL-28・RLS-10・TASK-212）の結合テスト。
//!
//! `tests/sql28_inner_join.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore` を production 経路として使う）。
//! `CROSS`／`NATURAL`・単独の `OUTER JOIN`・3 テーブル連鎖等の対象外事項の
//! 拒否確認は `sql28_inner_join.rs` 側に既存のものを残す（INNER・OUTER で共通の
//! 構文検証のため二重化しない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const AUTHORS: &str = "authors";
const DOCS: &str = "documents";

fn authors_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("name", ColumnType::Text, false),
        ],
    )
}

/// `author_id` を NULL 許容にした `documents`（Issue #926: 保存側の NULL
/// キー行が NULL 補完されることの確認に使う。INNER の既定スキーマは非 NULL
/// 制約のため専用に用意する）。
fn documents_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("title", ColumnType::Text, false),
            ColumnDef::new("author_id", ColumnType::BigInt, true),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_author(
    storage: &Storage,
    table: &str,
    tenant_ctx: &PolicyContext,
    id: u64,
    name: &str,
    visibility: Visibility,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{table}-{id}"))
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(name.to_string()),
        ],
        &op_id,
    )
    .expect("insert author row");
}

fn insert_document(
    storage: &Storage,
    table: &str,
    tenant_ctx: &PolicyContext,
    id: u64,
    title: &str,
    author_id: Option<i64>,
    visibility: Visibility,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{table}-{id}"))
        .expect("valid operation_id");
    let author_id_value = match author_id {
        Some(v) => Value::BigInt(v),
        None => Value::Null,
    };
    engine::tenant::insert_typed_row(
        storage,
        table,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(title.to_string()),
            author_id_value,
        ],
        &op_id,
    )
    .expect("insert document row");
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    expect_query(
        core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}")),
    )
}

fn titles(result: &QueryResult) -> Vec<Option<String>> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => Some(s.clone()),
            Some(Cell::Null) => None,
            other => panic!("expected Text or Null cell in first position, got {other:?}"),
        })
        .collect()
}

fn names(result: &QueryResult, idx: usize) -> Vec<Option<String>> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.get(idx) {
            Some(Cell::Text(s)) => Some(s.clone()),
            Some(Cell::Null) => None,
            other => panic!("expected Text or Null cell at position {idx}, got {other:?}"),
        })
        .collect()
}

/// authors: 1=alice, 2=bob。documents: 10/11 → alice、12 → bob、
/// 13 は authors に存在しない author_id（999）を指す孤児行。
fn seeded_basic() -> (Storage, std::path::PathBuf) {
    let path = unique_db_path("outer-join-basic");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&documents_schema(DOCS))
        .expect("create documents");
    let tenant_ctx = ctx("tenant-a");
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        1,
        "alice",
        Visibility::Public,
    );
    insert_author(&storage, AUTHORS, &tenant_ctx, 2, "bob", Visibility::Public);
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        10,
        "doc-a1",
        Some(1),
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        11,
        "doc-a2",
        Some(1),
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        12,
        "doc-b1",
        Some(2),
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        13,
        "doc-orphan",
        Some(999),
        Visibility::Public,
    );
    (storage, path)
}

// ---------- 受理・基本の意味論 ----------

#[test]
fn left_join_pads_unmatched_left_row_with_null() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 4, "all 4 documents must appear");
    let mut got: Vec<(Option<String>, Option<String>)> =
        titles(&result).into_iter().zip(names(&result, 1)).collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (Some("doc-a1".to_string()), Some("alice".to_string())),
            (Some("doc-a2".to_string()), Some("alice".to_string())),
            (Some("doc-b1".to_string()), Some("bob".to_string())),
            (Some("doc-orphan".to_string()), None),
        ]
    );
}

#[test]
fn left_outer_join_keyword_form_is_equivalent_to_left_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents LEFT OUTER JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 4);
}

#[test]
fn right_join_pads_unmatched_right_row_with_null() {
    // RIGHT JOIN: authors 側を保存する。authors には一致する document を持た
    // ない carol(3) を追加する専用データセット（`seeded_basic` は流用しない）。
    let path = unique_db_path("outer-join-right");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&documents_schema(DOCS))
        .expect("create documents");
    let tenant_ctx = ctx("tenant-a");
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        1,
        "alice",
        Visibility::Public,
    );
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        3,
        "carol",
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        10,
        "doc-a1",
        Some(1),
        Visibility::Public,
    );
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents RIGHT JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 2);
    let mut got: Vec<(Option<String>, Option<String>)> =
        titles(&result).into_iter().zip(names(&result, 1)).collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (None, Some("carol".to_string())),
            (Some("doc-a1".to_string()), Some("alice".to_string())),
        ]
    );
}

#[test]
fn right_outer_join_keyword_form_is_accepted() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT authors.name FROM documents RIGHT OUTER JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    // alice(1) は 2 件（10, 11）、bob(2) は 1 件（12）の documents と一致する。
    // documents 13（orphan）は authors 側に一致が無く、RIGHT JOIN では
    // authors 側だけを保存するため出力に現れない。
    assert_eq!(
        result.rows.len(),
        3,
        "each matched (author, document) pair is a separate output row"
    );
}

#[test]
fn full_join_pads_both_sides() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents FULL JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    // 4 documents（うち 1 件は孤児で authors 側 NULL）＋ authors は全員一致
    // 済みなので追加行なし。
    assert_eq!(result.rows.len(), 4);
    let mut got: Vec<(Option<String>, Option<String>)> =
        titles(&result).into_iter().zip(names(&result, 1)).collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (Some("doc-a1".to_string()), Some("alice".to_string())),
            (Some("doc-a2".to_string()), Some("alice".to_string())),
            (Some("doc-b1".to_string()), Some("bob".to_string())),
            (Some("doc-orphan".to_string()), None),
        ]
    );
}

#[test]
fn full_outer_join_keyword_form_pads_unmatched_rows_on_both_sides() {
    let path = unique_db_path("outer-join-full-both");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&documents_schema(DOCS))
        .expect("create documents");
    let tenant_ctx = ctx("tenant-a");
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        1,
        "alice",
        Visibility::Public,
    );
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        2,
        "carol-unmatched",
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        10,
        "doc-a1",
        Some(1),
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        13,
        "doc-orphan",
        Some(999),
        Visibility::Public,
    );
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents FULL OUTER JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(
        result.rows.len(),
        3,
        "1 matched + 1 left-only + 1 right-only"
    );
    let mut got: Vec<(Option<String>, Option<String>)> =
        titles(&result).into_iter().zip(names(&result, 1)).collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (None, Some("carol-unmatched".to_string())),
            (Some("doc-a1".to_string()), Some("alice".to_string())),
            (Some("doc-orphan".to_string()), None),
        ]
    );
}

#[test]
fn star_projection_pads_id_and_scalar_columns_with_null() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT * FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    // documents: id, embedding, title, author_id (4) + authors: id, embedding, name (3) = 7
    assert_eq!(result.columns.len(), 7);
    let orphan = result
        .rows
        .iter()
        .find(|r| matches!(&r.cells[2], Cell::Text(s) if s == "doc-orphan"))
        .expect("orphan document row must be present");
    assert!(
        matches!(orphan.cells[4], Cell::Null),
        "authors.id must be NULL for the unmatched left row, got {:?}",
        orphan.cells[4]
    );
    assert!(matches!(orphan.cells[5], Cell::Null));
    assert!(matches!(orphan.cells[6], Cell::Null));
}

// ---------- NULL キー ----------

#[test]
fn null_join_key_on_preserved_side_is_null_padded() {
    let path = unique_db_path("outer-join-null-key");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&documents_schema(DOCS))
        .expect("create documents");
    let tenant_ctx = ctx("tenant-a");
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        1,
        "alice",
        Visibility::Public,
    );
    // author_id が NULL の文書は、NULL キーが決して一致しないため
    // LEFT JOIN で NULL 補完されて出力される（`sql28_inner_join.rs::
    // null_join_key_never_matches` の INNER 版と対照）。
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        50,
        "doc-null",
        None,
        Visibility::Public,
    );

    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        titles(&result),
        vec![Some("doc-null".to_string())],
        "the NULL-key row must still appear (LEFT preserves it) with a NULL-padded right side"
    );
    assert_eq!(names(&result, 1), vec![None]);
}

// ---------- WHERE 簡約 ----------

#[test]
fn left_join_with_predicate_on_right_side_reduces_to_inner() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    // 欠損側（右＝authors）に述語があるため、NULL 補完行は必ず落ちる
    // （INNER と同じ結果になる。docs/design/outer-join.md の簡約規則）。
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents LEFT JOIN authors ON documents.author_id = authors.id WHERE authors.name = 'alice' LIMIT 10",
    );
    let mut got = titles(&result);
    got.sort();
    assert_eq!(
        got,
        vec![Some("doc-a1".to_string()), Some("doc-a2".to_string())],
        "orphan document must not appear once a predicate on the missing side is present"
    );
}

#[test]
fn left_join_with_predicate_on_left_side_still_pads_unmatched_rows() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    // 保存側（左＝documents）への述語はプッシュダウンされ、NULL 補完の対象
    // 集合（左側スキャン結果）に影響するだけで、簡約規則の対象ではない。
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id WHERE documents.title LIKE 'doc-orphan' LIMIT 10",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(titles(&result), vec![Some("doc-orphan".to_string())]);
    assert_eq!(names(&result, 1), vec![None]);
}

#[test]
fn full_join_with_predicate_on_one_side_behaves_like_one_sided_outer_join() {
    let path = unique_db_path("outer-join-full-where-reduction");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&documents_schema(DOCS))
        .expect("create documents");
    let tenant_ctx = ctx("tenant-a");
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        1,
        "alice",
        Visibility::Public,
    );
    insert_author(
        &storage,
        AUTHORS,
        &tenant_ctx,
        2,
        "carol-unmatched",
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        10,
        "doc-a1",
        Some(1),
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        13,
        "doc-orphan",
        Some(999),
        Visibility::Public,
    );
    let core = new_core(storage);
    // 述語が左側（documents）にあるため、`preserve_right`（右側〔authors〕の
    // 保存条件は「左側〔documents〕に述語が無いこと」）が偽になり、FULL は
    // 「左側だけを保存する」LEFT と同じ結果になる（右側だけの未一致行
    // 〔carol-unmatched〕は現れない）。述語自体は 'doc%' で始まる全文書に
    // 一致するため、保存側（左）の未一致行〔doc-orphan〕は引き続き NULL
    // 補完されて出力に残る。
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents FULL JOIN authors ON documents.author_id = authors.id WHERE documents.title LIKE 'doc%' LIMIT 10",
    );
    let mut got: Vec<(Option<String>, Option<String>)> =
        titles(&result).into_iter().zip(names(&result, 1)).collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (Some("doc-a1".to_string()), Some("alice".to_string())),
            (Some("doc-orphan".to_string()), None),
        ],
        "carol-unmatched must not be NULL-padded once a predicate on documents (the missing side for right-preservation) is present"
    );
}

// ---------- 決定性 ----------

#[test]
fn repeated_execution_yields_identical_results() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let sql = "SELECT documents.title, authors.name FROM documents FULL JOIN authors ON documents.author_id = authors.id LIMIT 100";
    let first = run(&core, "tenant-a", sql);
    let second = run(&core, "tenant-a", sql);
    assert_eq!(first, second);
}

#[test]
fn left_join_and_right_join_with_swapped_sides_produce_the_same_multiset() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let left_result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 100",
    );
    let right_result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM authors RIGHT JOIN documents ON authors.id = documents.author_id LIMIT 100",
    );
    let mut left_pairs: Vec<(Option<String>, Option<String>)> = titles(&left_result)
        .into_iter()
        .zip(names(&left_result, 1))
        .collect();
    let mut right_pairs: Vec<(Option<String>, Option<String>)> = titles(&right_result)
        .into_iter()
        .zip(names(&right_result, 1))
        .collect();
    left_pairs.sort();
    right_pairs.sort();
    assert_eq!(
        left_pairs, right_pairs,
        "A LEFT JOIN B and B RIGHT JOIN A must be the same multiset after re-ordering columns"
    );
}

// ---------- RLS（AC3） ----------

#[test]
fn cross_tenant_rows_never_appear_as_null_padded_matches() {
    let path = unique_db_path("outer-join-rls");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&documents_schema(DOCS))
        .expect("create documents");

    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");

    // tenant-a: 一致する author が無い（NULL 補完される想定）。
    insert_document(
        &storage,
        DOCS,
        &tenant_a,
        10,
        "doc-a1",
        Some(1),
        Visibility::Private,
    );
    // tenant-b: 同じ id=1 の author を Private で持つ（tenant-a からは不可視
    // でなければならない。可視だと誤って一致してしまう）。
    insert_author(
        &storage,
        AUTHORS,
        &tenant_b,
        1,
        "mallory",
        Visibility::Private,
    );

    let core = new_core(storage);
    let sql = "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10";
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(titles(&result), vec![Some("doc-a1".to_string())]);
    assert_eq!(
        names(&result, 1),
        vec![None],
        "tenant-a must never match tenant-b's private author row; the result must be NULL-padded"
    );
}

#[test]
fn cross_tenant_flood_does_not_affect_unmatched_row_count() {
    let path = unique_db_path("outer-join-rls-flood");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&documents_schema(DOCS))
        .expect("create documents");

    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");

    insert_document(
        &storage,
        DOCS,
        &tenant_a,
        10,
        "doc-a1",
        None,
        Visibility::Private,
    );
    // tenant-b が大量の author 行を Private で挿入しても、tenant-a の RIGHT
    // 側未一致件数（NULL 補完対象）には影響しない。
    for i in 0..20u64 {
        insert_author(
            &storage,
            AUTHORS,
            &tenant_b,
            100 + i,
            "flood",
            Visibility::Private,
        );
    }

    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents FULL JOIN authors ON documents.author_id = authors.id LIMIT 100",
    );
    assert_eq!(
        result.rows.len(),
        1,
        "tenant-b's flood of private author rows must not appear as right-only NULL-padded rows"
    );
}

// ---------- 明示トランザクション ----------

#[test]
fn outer_join_read_succeeds_inside_explicit_transaction() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let outcome = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SELECT documents.title FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10",
        )
        .expect("LEFT JOIN read inside BEGIN should succeed");
    assert_eq!(expect_query(outcome).rows.len(), 4);
}

// ---------- Describe ----------

#[test]
fn describe_matches_execute_columns_for_left_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let sql = "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10";

    let describe_session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
    let described = core
        .describe_parsed_in_session(&describe_session, &parsed)
        .expect("describe should succeed")
        .expect("LEFT JOIN must produce result columns");

    let mut exec_session = SessionState::default();
    let executed = expect_query(
        core.execute_sql_in_session(&ctx("tenant-a"), &mut exec_session, sql)
            .expect("execute should succeed"),
    );

    assert_eq!(described, executed.columns);
}

#[test]
fn describe_matches_execute_columns_for_full_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let sql =
        "SELECT * FROM documents FULL JOIN authors ON documents.author_id = authors.id LIMIT 10";

    let describe_session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
    let described = core
        .describe_parsed_in_session(&describe_session, &parsed)
        .expect("describe should succeed")
        .expect("FULL JOIN must produce result columns");

    let mut exec_session = SessionState::default();
    let executed = expect_query(
        core.execute_sql_in_session(&ctx("tenant-a"), &mut exec_session, sql)
            .expect("execute should succeed"),
    );

    assert_eq!(described, executed.columns);
}
