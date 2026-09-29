//! `INNER JOIN`（2 テーブル等価結合。Issue #925。ポインタ: SQL-28・RLS-10・
//! TASK-212）の結合テスト。
//!
//! `tests/sql29_set_operations.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、`EngineCore` を
//! production 経路として使う）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
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

/// `author_id` が `authors.id`（疑似列）を指す `BIGINT` 外部キー相当の列
/// （実際の FK 制約は付けない。JOIN の結合キー整数クラス互換の確認用）。
fn documents_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("title", ColumnType::Text, false),
            ColumnDef::new("author_id", ColumnType::BigInt, false),
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
    author_id: i64,
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
            Value::Text(title.to_string()),
            Value::BigInt(author_id),
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

fn run_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Ok(outcome) => panic!("expected error, got {outcome:?}: sql={sql:?}"),
        Err(e) => e,
    }
}

fn assert_rejected(core: &EngineCore, tenant: &str, sql: &str, wire_code: &str) {
    let err = run_err(core, tenant, sql);
    assert_eq!(err.wire_code(), wire_code, "sql={sql:?} err={err:?}");
}

fn titles(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => s.clone(),
            other => panic!("expected Text cell in first position, got {other:?}"),
        })
        .collect()
}

fn seeded_basic() -> (Storage, std::path::PathBuf) {
    let path = unique_db_path("join-basic");
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
        1,
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        11,
        "doc-a2",
        1,
        Visibility::Public,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        12,
        "doc-b1",
        2,
        Visibility::Public,
    );
    // author_id が存在しない authors 行を指す文書（結合されずに落ちることを確認）。
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        13,
        "doc-orphan",
        999,
        Visibility::Public,
    );
    (storage, path)
}

// ---------- 受理・意味論 ----------

#[test]
fn basic_inner_join_matches_on_id() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title, authors.name FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    let mut got: Vec<(String, String)> = result
        .rows
        .iter()
        .map(|r| {
            let title = match &r.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected Text, got {other:?}"),
            };
            let name = match &r.cells[1] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected Text, got {other:?}"),
            };
            (title, name)
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("doc-a1".to_string(), "alice".to_string()),
            ("doc-a2".to_string(), "alice".to_string()),
            ("doc-b1".to_string(), "bob".to_string()),
        ],
        "orphaned author_id must not appear (INNER JOIN semantics)"
    );
}

#[test]
fn bare_join_keyword_is_accepted_as_inner_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn table_aliases_and_as_keyword_are_accepted() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT d.title, a.name FROM documents AS d JOIN authors a ON d.author_id = a.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn star_projection_expands_left_then_right_with_id_columns() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    // documents: id, embedding, title, author_id (4) + authors: id, embedding, name (3) = 7
    assert_eq!(result.columns.len(), 7);
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn where_pushdown_filters_each_side_independently() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id WHERE authors.name = 'alice' LIMIT 10",
    );
    let mut got = titles(&result);
    got.sort();
    assert_eq!(got, vec!["doc-a1".to_string(), "doc-a2".to_string()]);
}

#[test]
fn multiple_and_joined_on_conditions_are_accepted() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id AND documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn self_join_with_distinct_aliases_is_accepted() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT x.name, y.name FROM authors x JOIN authors y ON x.id = y.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn limit_and_offset_apply_after_full_result_is_computed() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 1 OFFSET 1",
    );
    assert_eq!(result.rows.len(), 1);
}

// ---------- 拒否（`42601`） ----------

// `LEFT`／`RIGHT`／`FULL [OUTER]` JOIN は Issue #926 で受理対象になった
// （結合テストは `tests/sql28_outer_join.rs` 参照）。ここでは引き続き対象外の
// `CROSS`／`NATURAL`・単独の `OUTER JOIN` だけを確認する。

#[test]
fn rejects_bare_outer_join_without_side() {
    // `LEFT`／`RIGHT`／`FULL` を伴わない単独の `OUTER JOIN` は対象外
    // （Issue #926 §2.1）。
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents OUTER JOIN authors ON documents.author_id = authors.id LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_natural_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents NATURAL JOIN authors LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_cross_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents CROSS JOIN authors LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_join_using_clause() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors USING (id) LIMIT 10",
        "42601",
    );
}

/// Issue #1190: 3 テーブル以上の連鎖 JOIN を受理する（旧 `rejects_three_table_join_chain`
/// を反転）。詳細な意味論は `tests/sql28_multi_way_join.rs`。
#[test]
fn accepts_three_table_join_chain() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id JOIN authors AS a2 ON authors.id = a2.id LIMIT 10",
    );
    assert_eq!(result.rows.len(), 3);
    // documents(4 列: id + 3) + authors(3 列) + a2(3 列)
    assert_eq!(result.columns.len(), 10);
}

#[test]
fn rejects_non_equality_on_condition() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.author_id > authors.id LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_on_condition_referencing_only_one_side() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents d1 JOIN documents d2 ON d1.author_id = d1.author_id LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_duplicate_exposed_relation_names() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM authors JOIN authors ON authors.id = authors.id LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_vector_column_as_join_key() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.embedding = authors.embedding LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_order_by_combined_with_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id ORDER BY embedding <=> '[0.1,0.1]' LIMIT 10",
        "42601",
    );
}

/// Issue #1190: JOIN の WHERE で `OR` を受理する（旧 `rejects_where_or_in_join` を反転）。
#[test]
fn accepts_where_or_in_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id WHERE authors.name = 'alice' OR authors.name = 'bob' LIMIT 10",
    );
    assert_eq!(titles(&result), vec!["doc-a1", "doc-a2", "doc-b1"]);
}

#[test]
fn rejects_join_where_with_excessive_and_conjuncts() {
    // untrusted 入力の長い `AND` チェーンでパース時に `where_conjuncts` を
    // 無制限確保しない（共有 WHERE パースの `MAX_WHERE_LEAVES`・ON 句の
    // `MAX_JOIN_CONDITIONS` と同じ判断。coding-rust.md「長さフィールドは
    // 上限検証してからアロケーションに使う」）。
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let clause = (0..300)
        .map(|_| "authors.name = 'alice'")
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!(
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id WHERE {clause} LIMIT 10"
    );
    assert_rejected(&core, "tenant-a", &sql, "54000");
}

/// Issue #1190: JOIN の WHERE で列同士の比較を受理する（旧
/// `rejects_where_column_to_column_comparison_in_join` を反転）。
#[test]
fn accepts_where_column_to_column_comparison_in_join() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id WHERE documents.title = authors.name LIMIT 10",
    );
    assert!(result.rows.is_empty(), "no title equals an author name");
}

#[test]
fn rejects_parameter_placeholder_in_join() {
    // `$n` はワイヤ側の拡張クエリプロトコル経由のみ生成されるため、簡易クエリ
    // プロトコル（`execute_sql_in_session`）では `$1` はリテラルにならず構文
    // エラーになるが、ここでは `sql::params::validate_param_positions` の
    // JOIN 拒否を直接確認する（Issue #925 §2.7）。
    let tokens = engine::sql::lexer::tokenize_with_params(
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id WHERE authors.name = $1 LIMIT 10",
    )
    .expect("tokenize should succeed");
    let err = engine::sql::params::validate_param_positions(&tokens)
        .expect_err("JOIN statements must reject $n placeholders");
    assert_eq!(err.wire_code(), "42601");
}

// ---------- その他の分類 ----------

#[test]
fn rejects_unknown_table_with_undefined_table() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN missing_table ON documents.author_id = missing_table.id LIMIT 10",
        "42P01",
    );
}

#[test]
fn rejects_unknown_qualifier_in_on_condition() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON nope.author_id = authors.id LIMIT 10",
        "42P01",
    );
}

#[test]
fn rejects_ambiguous_unqualified_column_in_where() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    // `id` は両辺の疑似列のため非修飾では常に曖昧。
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id WHERE id = 'x' LIMIT 10",
        "42702",
    );
}

#[test]
fn rejects_unknown_column_in_projection() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT documents.nope FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
        "22000",
    );
}

#[test]
fn rejects_join_key_type_mismatch_with_datatype_mismatch() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.title = authors.id LIMIT 10",
        "42804",
    );
}

#[test]
fn bind_error_takes_priority_over_limit_out_of_range() {
    // docs/design/inner-join.md の順序契約: 両辺の束縛（`sql::parser::
    // bind_scan_with_dummy_flags`。ここでは WHERE 述語の型検証）を完了させて
    // から `LIMIT`／`OFFSET` の範囲検証を行う。束縛エラー・LIMIT エラーは
    // いずれも `wire_code` 上は `22000`（`InvalidInput`）だが、束縛未完了の
    // まま LIMIT を先に検証する実装では detail が LIMIT 側の文言になって
    // しまう（Execute/Describe 双方で束縛側の文言を確認する）。
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    // `author_id`（`BigInt`）への範囲比較（`>`）は束縛段（`declarative_filter
    // ::bind_impl`）でのみ「range comparison 非対応」として拒否される
    // （`build_plan` の列解決は通過する）。LIMIT 0 と共存させ、束縛完了が
    // LIMIT 検証より先であることを確認する。
    let sql = "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id \
               WHERE documents.author_id > '5' LIMIT 0";
    let err = run_err(&core, "tenant-a", sql);
    assert_eq!(err.wire_code(), "22000", "sql={sql:?} err={err:?}");
    let detail = format!("{err:?}");
    assert!(
        detail.contains("range comparison"),
        "expected the bind-time range-comparison error to take priority over the LIMIT range error, got {detail}"
    );

    let describe_session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
    let describe_err = core
        .describe_parsed_in_session(&describe_session, &parsed)
        .expect_err("describe must reject the same bind error as execute");
    assert_eq!(describe_err.wire_code(), "22000", "err={describe_err:?}");
    let describe_detail = format!("{describe_err:?}");
    assert!(
        describe_detail.contains("range comparison"),
        "expected describe to report the same bind-time error, got {describe_detail}"
    );
}

#[test]
fn rejects_limit_out_of_range() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 0",
        "22000",
    );
}

// ---------- RLS（AC3） ----------

#[test]
fn cross_tenant_rows_never_appear_in_join_results_or_counts() {
    let path = unique_db_path("join-rls");
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
    let tenant_c = ctx("tenant-c");

    // tenant-a: 1 件の正当な結合ペア。
    insert_author(
        &storage,
        AUTHORS,
        &tenant_a,
        1,
        "alice",
        Visibility::Private,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_a,
        10,
        "doc-a1",
        1,
        Visibility::Private,
    );

    // tenant-b: 同じ id（1）で全く別の author 行・大量の一致キーを持つ document
    // 行を挿入する（越境・カーディナリティ汚染の双方を試みる）。
    insert_author(
        &storage,
        AUTHORS,
        &tenant_b,
        1,
        "mallory",
        Visibility::Private,
    );
    for i in 0..20u64 {
        insert_document(
            &storage,
            DOCS,
            &tenant_b,
            100 + i,
            "doc-b-flood",
            1,
            Visibility::Private,
        );
    }

    // tenant-c: 別テナントのセッションでは結果が入れ替わることの確認用。
    insert_author(
        &storage,
        AUTHORS,
        &tenant_c,
        2,
        "carol",
        Visibility::Private,
    );
    insert_document(
        &storage,
        DOCS,
        &tenant_c,
        200,
        "doc-c1",
        2,
        Visibility::Public,
    );

    let core = new_core(storage);
    let sql = "SELECT documents.title, authors.name FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 100";

    let result_a = run(&core, "tenant-a", sql);
    assert_eq!(
        result_a.rows.len(),
        1,
        "tenant-b's flood of matching keys must not affect tenant-a's join cardinality"
    );
    let title_a = match &result_a.rows[0].cells[0] {
        Cell::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    };
    let name_a = match &result_a.rows[0].cells[1] {
        Cell::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert_eq!(title_a, "doc-a1");
    assert_eq!(
        name_a, "alice",
        "tenant-a must never see tenant-b's author row"
    );

    let result_c = run(&core, "tenant-c", sql);
    assert_eq!(result_c.rows.len(), 1);
    let name_c = match &result_c.rows[0].cells[1] {
        Cell::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert_eq!(
        name_c, "carol",
        "tenant-c's own session must see its own join result"
    );
}

// ---------- 明示トランザクション ----------

#[test]
fn join_read_succeeds_inside_explicit_transaction() {
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
            "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
        )
        .expect("join read inside BEGIN should succeed");
    assert_eq!(expect_query(outcome).rows.len(), 3);
}

/// 自トランザクションが書き込んだテーブルを JOIN しても、未 commit の変更が結果に
/// 反映される（Issue #1179。書き込みトランザクションを読み取り源にする）。孤児
/// 文書 `doc-orphan`（`author_id = 999`）は、トランザクション内で `authors` に
/// 追加した 999 番の行と結合され 4 行になる。別セッション（autocommit）からは
/// 未 commit の行が見えず、結合されないまま 3 行のまま。
#[test]
fn join_read_after_writing_one_of_its_tables_sees_the_uncommitted_row() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "INSERT INTO authors (id, embedding, name) VALUES (999, '[9.0,9.0]', 'zed') USING OPERATION_ID 'op-zed'",
    )
    .expect("insert into authors");

    let join_sql = "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10";
    let outcome = core
        .execute_sql_in_txn(&caller, &mut session, &mut txn, join_sql)
        .expect("join over a written table must see the uncommitted row");
    assert_eq!(expect_query(outcome).rows.len(), 4);

    // 別セッションからは未 commit の行が見えない。
    // （書き込みゲートは autocommit の読み取りを妨げない。）
    let other = core
        .execute_sql_in_session(&caller, &mut SessionState::default(), join_sql)
        .expect("autocommit join from another session");
    assert_eq!(expect_query(other).rows.len(), 3);

    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    let after = core
        .execute_sql_in_session(&caller, &mut SessionState::default(), join_sql)
        .expect("autocommit join after rollback");
    assert_eq!(expect_query(after).rows.len(), 3);
}

// ---------- Describe ----------

#[test]
fn describe_matches_execute_columns() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let sql = "SELECT documents.title, authors.name FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10";

    let describe_session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
    let described = core
        .describe_parsed_in_session(&describe_session, &parsed)
        .expect("describe should succeed")
        .expect("JOIN must produce result columns");

    let mut exec_session = SessionState::default();
    let executed = expect_query(
        core.execute_sql_in_session(&ctx("tenant-a"), &mut exec_session, sql)
            .expect("execute should succeed"),
    );

    assert_eq!(
        described, executed.columns,
        "describe columns must match execute columns for a JOIN"
    );
}

#[test]
fn describe_rejects_type_mismatch_like_execute() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let sql = "SELECT * FROM documents JOIN authors ON documents.title = authors.id LIMIT 10";

    let describe_session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
    let err = core
        .describe_parsed_in_session(&describe_session, &parsed)
        .expect_err("describe must reject the same type mismatch as execute");
    assert_eq!(err.wire_code(), "42804");
}

// ---------- JOIN と他構文の組み合わせ（対象外事項の防御的確認） ----------

/// Issue #925 §2.6・対象外事項: `DECLARE ... FOR` の内側は `Statement::Scan`／
/// `Statement::Aggregate` のみを受理するため（`sql::cursor::
/// validate_declare_inner`）、JOIN を含む内側は `42601`。
#[test]
fn declare_cursor_for_join_is_rejected() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "DECLARE c CURSOR FOR SELECT * FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
        )
        .expect_err("DECLARE ... FOR JOIN must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// Issue #1192（TABLE-18）: `CREATE VIEW` 本文に JOIN を書けるようになった
/// （評価後射影形ビュー）。DDL 権限のない既定セッションは構造検証の後で `42501`、
/// 権限のあるセッションでは作成でき、参照結果は本文を直接実行した結果と一致する
/// （参照セッションの RLS が両辺に独立して適用される）。ビューへの JOIN の辺
/// としての参照は引き続き `42601`。
#[test]
fn create_view_with_join_body_is_accepted_and_readable() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let create =
        "CREATE VIEW v AS SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10";
    assert_rejected(&core, "tenant-a", create, "42501");

    let mut ddl_session = SessionState::default();
    ddl_session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut ddl_session, create)
        .expect("create join view");

    let direct = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    let via_view = run(&core, "tenant-a", "SELECT title FROM v LIMIT 10");
    assert_eq!(titles(&via_view), titles(&direct));

    assert_rejected(
        &core,
        "tenant-a",
        "SELECT * FROM documents JOIN v ON documents.title = v.title LIMIT 10",
        "42601",
    );
}

/// Issue #925 §2.6・対象外事項: `IN (SELECT ...)` の内側は `Statement::Scan`
/// のみを受理するため（`sql::subquery::resolve_where_predicates`）、JOIN を
/// 含む内側は `42601`。
#[test]
fn in_subquery_with_join_body_is_rejected() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT title FROM documents WHERE author_id IN (SELECT documents.author_id FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10) LIMIT 10",
        "42601",
    );
}

/// Issue #925 §2.6・対象外事項: `EXPLAIN` は `validate_select_statement`
/// （単一テーブル専用の文法）を直接呼ぶため（`looks_like_join` を経由しない）、
/// JOIN 形の入力は単一テーブル文法の解析失敗として `42601` になる。
#[test]
fn explain_with_join_is_rejected() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "EXPLAIN SELECT * FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
        "42601",
    );
}

/// Issue #925 §2.6・対象外事項: 集合演算の枝パーサー（`parse_set_branch`）は
/// 専用の単一テーブル文法のため、JOIN を含む枝は `42601`。
#[test]
fn union_branch_with_join_is_rejected() {
    let (storage, path) = seeded_basic();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "tenant-a",
        "SELECT title FROM documents UNION SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
        "42601",
    );
}

// ---------- NULL・不一致キーの意味論 ----------

#[test]
fn null_join_key_never_matches() {
    // `documents_schema` の `author_id` は非 NULL 制約付きのため、この検証専用に
    // NULL を許容するスキーマを使う。
    fn nullable_documents_schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("title", ColumnType::Text, false),
                ColumnDef::new("author_id", ColumnType::BigInt, true),
            ],
        )
    }

    let path = unique_db_path("join-null-key");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&authors_schema(AUTHORS))
        .expect("create authors");
    storage
        .create_table(&nullable_documents_schema(DOCS))
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
    // `author_id` を NULL にした文書は、`authors.id = 1` と一致する値を持たない
    // ため、NULL キーの行はビルド・プローブいずれからも除外され結合されない。
    let op_id = engine::recovery::required_op_id::OperationId::parse("seed-null-doc")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        DOCS,
        &tenant_ctx,
        50,
        Visibility::Public,
        &[
            Value::Vector(vec![50.0, 0.0]),
            Value::Text("doc-null".to_string()),
            Value::Null,
        ],
        &op_id,
    )
    .expect("insert document with NULL author_id");

    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(
        result.rows.len(),
        0,
        "a NULL join key must never match, even against a row with the same pseudo id"
    );
}

#[test]
fn mismatched_negative_join_key_never_matches() {
    let path = unique_db_path("join-negative-key");
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
    insert_document(
        &storage,
        DOCS,
        &tenant_ctx,
        60,
        "doc-neg",
        -1,
        Visibility::Public,
    );

    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT documents.title FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
    );
    assert_eq!(
        result.rows.len(),
        0,
        "a negative BIGINT key must never match the unsigned pseudo id column"
    );
}
