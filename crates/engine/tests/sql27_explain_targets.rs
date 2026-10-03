//! `EXPLAIN` の対象文を通常検索・集計・広域取得へ拡大した結合テスト
//! （Issue #922・SQL-27。ポインタ: SQL-6・TASK-78・SQL-13・SQL-14・SQL-15・
//! SQL-25）。`crates/engine/tests/sql_explain.rs`（`USING PLAN` 付き検索・
//! 既存契約）・`sql_scan.rs`（広域取得の受理反転）とは別に、本ファイルは
//! 新設の 2 経路（`USING PLAN` なし検索・集計）の静的判定と、対象拡大が
//! 既存の非実行契約（行走査・キャッシュ消費・LLM/Embedder 呼び出しを一切
//! 行わない）を壊していないことに焦点を当てる。Issue #1351 では集計文＋
//! スカラー ORDER BY の EXPLAIN（本体非実行・テナント間一致・ORDER BY 非依存）も固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 4;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `EXPLAIN` の行（`QUERY PLAN` 単一列）をテキスト列へ変換する（本ファイル内の
/// 全テストが共有する。`sql_explain.rs::explain_result_lines` と同構成）。
fn explain_lines(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(result) => {
            assert_eq!(result.columns.len(), 1);
            assert_eq!(
                result.columns[0],
                ColumnMeta::Computed {
                    name: "QUERY PLAN".to_string(),
                    ty: Some(engine::catalog::ColumnType::Text),
                }
            );
            result
                .rows
                .iter()
                .map(|row| match &row.cells[0] {
                    Cell::Text(s) => s.clone(),
                    other => panic!("expected Cell::Text, got {other:?}"),
                })
                .collect()
        }
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

fn insert_row(
    storage: &Storage,
    table: &str,
    tenant: &str,
    id: u64,
    embedding: Vec<f32>,
    lang: &str,
    visibility: Visibility,
) {
    let op_id =
        engine::recovery::required_op_id::OperationId::parse(&format!("sql27-explain-op-{id}"))
            .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        visibility,
        &[Value::Vector(embedding), Value::Text(lang.to_string())],
        &op_id,
    )
    .expect("insert row");
}

/// `insert_row` の 3 列（`embedding`・`lang`・`kind`）版
/// （[`schema_with_vector_and_two_group_columns`] 用）。
fn insert_row_with_kind(
    storage: &Storage,
    table: &str,
    tenant: &str,
    id: u64,
    embedding: Vec<f32>,
    // `(lang, kind)`（複数列 GROUP BY のキー 2 本）。clippy::too_many_arguments
    // を避けるためタプルへまとめる。
    lang_and_kind: (&str, &str),
    visibility: Visibility,
) {
    let (lang, kind) = lang_and_kind;
    let op_id =
        engine::recovery::required_op_id::OperationId::parse(&format!("sql27-explain-op-{id}"))
            .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(embedding),
            Value::Text(lang.to_string()),
            Value::Text(kind.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

/// `embedding`（VECTOR）・`lang`（TEXT）を持つ標準スキーマ。
fn schema_with_vector(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// `VECTOR` 列を持たない（`lang` TEXT のみの）スキーマ（SQL-13 の対象。
/// 集計の `access_path` 判定が `has_vector` でゲートされることの確認に使う）。
fn schema_without_vector(table: &str) -> TableSchema {
    TableSchema::new(table, vec![ColumnDef::new("lang", ColumnType::Text, false)])
}

/// `embedding`（VECTOR）・`lang`・`kind`（いずれも TEXT）を持つ複数列
/// `GROUP BY` 検証用スキーマ（`sql25_multi_group_by.rs::schema` と同構成）。
fn schema_with_vector_and_two_group_columns(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("kind", ColumnType::Text, false),
        ],
    )
}

// ---------- USING PLAN なし検索 EXPLAIN ----------

#[test]
fn search_explain_distance_reports_static_judgement_without_llm_io() {
    let path = unique_db_path("sql27-search-distance");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    // `with_query_planner` を注入しない: `USING PLAN` なし検索 EXPLAIN は
    // LLM I/O を必要としないため、プランナー未注入でも成功することが受け入れ
    // 条件 9 の担保（Embedder 未注入でも `EXPLAIN` 可能な既存契約の踏襲）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0,0.0]' LIMIT 10",
        )
        .expect("EXPLAIN over a plain search SELECT must succeed without a query planner");
    let lines = explain_lines(outcome);
    assert_eq!(
        lines,
        vec![
            "mode: recall",
            "mode_source: default",
            "engine: (custom_provider)",
            "ann_plan: unknown_custom_provider",
            "scalar_plan: plain_scan",
        ]
    );
}

#[test]
fn search_explain_hybrid_reports_hybrid_shape() {
    let path = unique_db_path("sql27-search-hybrid");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // HYBRID の疎側テキスト列は固定で `body`（`crates/wire-server/docs/
    // nosql-api.md`「疎側テキスト列は固定で `body` 列」と同じ規約）。
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let op_id = engine::recovery::required_op_id::OperationId::parse("sql27-hybrid-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx("tenant-a"),
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            Value::Text("alpha content".to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0,0.0,0.0]', body, 'alpha') LIMIT 10",
        )
        .expect("EXPLAIN over a HYBRID search SELECT must succeed");
    let lines = explain_lines(outcome);
    assert!(!lines.is_empty());
    assert_eq!(lines[0], "mode: recall");
    assert!(lines.iter().any(|l| l == "scalar_plan: plain_scan"));
}

// ---------- 集計 EXPLAIN ----------

#[test]
fn aggregate_explain_count_star_without_where_uses_visible_bitmap_cache() {
    let path = unique_db_path("sql27-aggregate-fast");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT COUNT(*) FROM docs",
        )
        .expect("EXPLAIN over COUNT(*) without WHERE must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec![
            "scalar_plan: plain_scan",
            "access_path: visible_bitmap_cache"
        ]
    );
}

#[test]
fn aggregate_explain_with_index_eligible_where_reports_scalar_index_candidates() {
    let path = unique_db_path("sql27-aggregate-candidates");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT COUNT(id) FROM docs WHERE lang = 'ja'",
        )
        .expect("EXPLAIN over an index-eligible WHERE must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec![
            "scalar_plan: index_equality",
            "access_path: scalar_index_candidates"
        ]
    );
}

#[test]
fn aggregate_explain_on_table_without_vector_column_is_full_scan() {
    // SQL-13: `VECTOR` 列を持たないテーブルの集計は索引の構築材料を持てない
    // ため、`WHERE` の形状によらず常に `plain_scan`／`full_scan`（D5: 矛盾出力
    // の防止）。
    let path = unique_db_path("sql27-aggregate-no-vector");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_without_vector("docs"))
        .expect("create table");
    let op_id = engine::recovery::required_op_id::OperationId::parse("sql27-no-vector-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx("tenant-a"),
        1,
        Visibility::Public,
        &[Value::Text("ja".to_string())],
        &op_id,
    )
    .expect("insert row");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT COUNT(id) FROM docs WHERE lang = 'ja'",
        )
        .expect("EXPLAIN over a vector-less table aggregate must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );
}

#[test]
fn group_by_explain_without_where_uses_scalar_index_group_enumeration() {
    let path = unique_db_path("sql27-groupby-enumeration");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        2,
        vec![0.0, 1.0, 0.0, 0.0],
        "en",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT lang, COUNT(*) FROM docs GROUP BY lang",
        )
        .expect("EXPLAIN over a WHERE-less GROUP BY must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec![
            "scalar_plan: plain_scan",
            "access_path: scalar_index_group_enumeration"
        ]
    );
}

#[test]
fn group_by_explain_with_text_min_max_blocks_enumeration() {
    // codex-review P1（PR #603）の踏襲: `MIN`/`MAX(<TEXT 列>)` を含む
    // `GROUP BY` は列挙形にならない（`full_scan`）。
    let path = unique_db_path("sql27-groupby-text-minmax");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT lang, MAX(lang) FROM docs GROUP BY lang",
        )
        .expect("EXPLAIN over a GROUP BY with TEXT MIN/MAX must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );
}

#[test]
fn group_by_explain_with_or_only_where_does_not_become_enumeration() {
    // Issue #912 回帰: OR だけの `WHERE` は「WHERE なし」と誤判定されず
    // （`where_less` が `false`）、索引未対応（`classify_scalar_plan` が
    // `PlainScan`）のため列挙形にも候補削減形にもならず `full_scan`。
    // 同じ列への等価 OR は Issue #1305 で束縛時に `IN` へ畳まれるため、
    // 畳めない OR（`LIKE` 前方一致を含む）でこの回帰を維持し、畳める OR は
    // 同じ文脈の `IN` 形と EXPLAIN 行が完全一致することを固定する。
    let path = unique_db_path("sql27-groupby-or-only");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let mut explain = |predicate: &str| {
        let outcome = core
            .execute_sql_in_session(
                &ctx("tenant-a"),
                &mut session,
                &format!("EXPLAIN SELECT lang, COUNT(*) FROM docs WHERE {predicate} GROUP BY lang"),
            )
            .expect("EXPLAIN over an OR-only WHERE GROUP BY must succeed");
        explain_lines(outcome)
    };
    assert_eq!(
        explain("lang = 'ja' OR lang LIKE 'e%'"),
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );
    assert_eq!(
        explain("lang = 'ja' OR lang = 'en'"),
        explain("lang IN ('ja', 'en')")
    );
}

#[test]
fn group_by_explain_with_multi_column_key_and_index_eligible_where_is_full_scan() {
    // codex-review P1（PR #1102）回帰: `execute_grouped_aggregate` は複数列
    // `GROUP BY`（`key_count != 1`）を索引経路の対象外とし常に全走査へ倒す
    // （`ScalarIndex::column_groups` が単一キー専用）。索引対応述語
    // （`lang = 'ja'`）だけを見て `scalar_plan: index_equality` を返すと、
    // 実行が常に `full_scan` になる複数列 `GROUP BY` で
    // `scalar_plan: index_equality` と `access_path: full_scan` が同時に
    // 出て D5「矛盾出力の防止」に反する。両方が `plain_scan`／`full_scan` に
    // 揃うことを確認する。
    let path = unique_db_path("sql27-groupby-multi-key-index-eligible");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector_and_two_group_columns("docs"))
        .expect("create table");
    insert_row_with_kind(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        ("ja", "blog"),
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT lang, kind, COUNT(*) FROM docs WHERE lang = 'ja' GROUP BY lang, kind",
        )
        .expect("EXPLAIN over a multi-column GROUP BY with index-eligible WHERE must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );
}

#[test]
fn distinct_explain_is_accepted_via_aggregate_desugaring() {
    // SQL-25 (c)・TASK-209: `SELECT DISTINCT` は集計へ脱糖されるため、
    // `EXPLAIN` は他の集計と同じ判定経路（`ExplainTarget::Aggregate`）を通る。
    let path = unique_db_path("sql27-distinct");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT DISTINCT lang FROM docs",
        )
        .expect("EXPLAIN over SELECT DISTINCT must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec![
            "scalar_plan: plain_scan",
            "access_path: scalar_index_group_enumeration"
        ]
    );
}

// ---------- 広域取得 EXPLAIN ----------

#[test]
fn scan_explain_with_offset_is_full_scan() {
    let path = unique_db_path("sql27-scan-offset");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs LIMIT 10 OFFSET 1",
        )
        .expect("EXPLAIN over a wide-retrieval scan with OFFSET must succeed");
    assert_eq!(
        explain_lines(outcome),
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );
}

// ---------- 本体非実行・エラーパリティ・拒否の維持 ----------

#[test]
fn aggregate_and_scan_explain_do_not_write_or_execute() {
    // `EXPLAIN` はテーブル行データを一切返さない・書き込まないことを、実行前後で
    // 行数（`SELECT COUNT(*)`）が変化しないことにより確認する
    // （`sql_explain.rs::explain_does_not_write_or_execute_search` と同構成）。
    let path = unique_db_path("sql27-no-side-effects");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "EXPLAIN SELECT COUNT(*) FROM docs",
    )
    .expect("aggregate EXPLAIN should succeed");
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "EXPLAIN SELECT id FROM docs LIMIT 10",
    )
    .expect("scan EXPLAIN should succeed");

    let count_after = match core
        .execute_sql(&ctx("tenant-a"), "SELECT COUNT(id) FROM docs")
        .expect("count query should succeed")
        .rows
        .into_iter()
        .next()
        .expect("count row")
        .cells
        .into_iter()
        .next()
        .expect("count cell")
    {
        Cell::Integer(n) => n,
        other => panic!("expected Cell::Integer, got {other:?}"),
    };
    assert_eq!(count_after, 1, "EXPLAIN must not write or delete rows");
}

#[test]
fn aggregate_explain_is_byte_identical_across_different_visible_row_counts() {
    // security.md「テナント境界」: `EXPLAIN` の出力（静的判定のみ）は可視行数
    // に依存しない。可視行 0 件のテナントと複数行が可視なテナントで同じ SQL の
    // `EXPLAIN` がバイト一致することを固定する。
    let path = unique_db_path("sql27-tenant-non-exposure");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        1,
        vec![1.0, 0.0, 0.0, 0.0],
        "ja",
        Visibility::Private,
    );
    insert_row(
        &storage,
        "docs",
        "tenant-a",
        2,
        vec![0.0, 1.0, 0.0, 0.0],
        "en",
        Visibility::Private,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session_a = SessionState::default();
    let lines_visible = explain_lines(
        core.execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session_a,
            "EXPLAIN SELECT COUNT(id) FROM docs WHERE lang = 'ja'",
        )
        .expect("tenant-a EXPLAIN should succeed"),
    );

    let mut session_b = SessionState::default();
    let lines_invisible = explain_lines(
        core.execute_sql_in_session(
            &ctx("tenant-b"),
            &mut session_b,
            "EXPLAIN SELECT COUNT(id) FROM docs WHERE lang = 'ja'",
        )
        .expect("tenant-b EXPLAIN should succeed (0 visible rows)"),
    );

    assert_eq!(lines_visible, lines_invisible);
}

#[test]
fn aggregate_and_scan_explain_error_parity_with_unknown_table() {
    let path = unique_db_path("sql27-error-parity");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    for sql in [
        "SELECT COUNT(*) FROM missing",
        "EXPLAIN SELECT COUNT(*) FROM missing",
        "SELECT id FROM missing LIMIT 10",
        "EXPLAIN SELECT id FROM missing LIMIT 10",
    ] {
        let err = core
            .execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
            .expect_err(&format!("{sql} must be rejected"));
        assert_eq!(err.wire_code(), "42P01", "mismatch for {sql}");
    }
}

#[test]
fn explain_still_rejects_non_select_targets() {
    let path = unique_db_path("sql27-rejects-non-select");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    for sql in [
        "EXPLAIN EXPLAIN SELECT id FROM docs LIMIT 10",
        "EXPLAIN SET search_mode = 'recall'",
        "EXPLAIN CREATE FUNCTION f(x) AS x",
        "EXPLAIN INSERT INTO docs (embedding, lang) VALUES ('[1,0,0,0]', 'ja')",
    ] {
        let err = core
            .execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
            .expect_err(&format!("{sql} must still be rejected"));
        assert_eq!(err.wire_code(), "42601", "mismatch for {sql}");
    }
}

#[test]
fn scalar_order_by_scan_explain_is_identical_across_tenants_and_does_not_execute() {
    // Issue #1189・SQL-25・RLS-10: スカラー ORDER BY 付き広域取得の EXPLAIN は静的な
    // 固定出力で、可視行数の異なるテナント間でバイト一致する（存在情報を漏らさない）。
    let path = unique_db_path("sql27-scalar-order-by-explain");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    for id in 1..=5u64 {
        insert_row(
            &storage,
            "docs",
            "tenant-a",
            id,
            vec![1.0, 0.0, 0.0, 0.0],
            "ja",
            Visibility::Private,
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let sql = "EXPLAIN SELECT id FROM docs ORDER BY lang DESC LIMIT 3 OFFSET 1";
    let mut session = SessionState::default();
    let with_rows = explain_lines(
        core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
            .expect("EXPLAIN over scalar ORDER BY scan must succeed"),
    );
    let mut session = SessionState::default();
    let without_rows = explain_lines(
        core.execute_sql_in_session(&ctx("tenant-b"), &mut session, sql)
            .expect("EXPLAIN over scalar ORDER BY scan must succeed"),
    );
    assert_eq!(with_rows, without_rows);
    assert_eq!(
        with_rows,
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );

    // 本体を実行していない: EXPLAIN 後も行はそのまま読める。
    let after = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs ORDER BY lang LIMIT 10",
        )
        .expect("scan after EXPLAIN");
    assert_eq!(after.rows.len(), 5);
}

/// 集計文＋スカラー ORDER BY の `EXPLAIN` を、新しいセッションで 1 回実行して
/// 結果行を返す（Issue #1351。`EXPLAIN` を受理するのは `execute_sql_in_session`
/// のみで、`execute_sql` は入口の契約で 42601 になるため必ずこちらを使う）。
fn explain_in_fresh_session(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    explain_lines(
        core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed: {}", e.wire_code())),
    )
}

#[test]
fn scalar_order_by_aggregate_explain_is_identical_across_tenants_and_independent_of_order_by() {
    // Issue #1351・SQL-25・SQL-27・RLS-10: 集計文のスカラー ORDER BY 付き EXPLAIN は
    // 静的判定のみで、(a) 可視行の異なるテナント間でバイト一致し、(b) ORDER BY を除いた
    // 同じ文の EXPLAIN と一致し（ORDER BY は走査方式の判定に影響しない）、(c) 固定出力になる。
    let path = unique_db_path("sql27-agg-order-by-explain");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector_and_two_group_columns("docs"))
        .expect("create table");
    for (id, kind_pair) in [
        (1u64, ("ja", "blog")),
        (2, ("ja", "news")),
        (3, ("en", "blog")),
        (4, ("en", "news")),
    ] {
        insert_row_with_kind(
            &storage,
            "docs",
            "tenant-a",
            id,
            vec![1.0, 0.0, 0.0, 0.0],
            kind_pair,
            Visibility::Private,
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    // (ORDER BY 付き EXPLAIN, ORDER BY を除いた基準 EXPLAIN, 期待行)
    let cases: [(&str, &str, [&str; 2]); 4] = [
        (
            "EXPLAIN SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY c DESC, lang LIMIT 3 OFFSET 1",
            "EXPLAIN SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang LIMIT 3 OFFSET 1",
            [
                "scalar_plan: plain_scan",
                "access_path: scalar_index_group_enumeration",
            ],
        ),
        (
            "EXPLAIN SELECT lang, kind, COUNT(*) AS c FROM docs GROUP BY lang, kind ORDER BY c DESC, lang ASC, kind DESC",
            "EXPLAIN SELECT lang, kind, COUNT(*) AS c FROM docs GROUP BY lang, kind",
            ["scalar_plan: plain_scan", "access_path: full_scan"],
        ),
        (
            "EXPLAIN SELECT lang, COUNT(*) AS c FROM docs WHERE lang = 'ja' GROUP BY lang ORDER BY lang DESC",
            "EXPLAIN SELECT lang, COUNT(*) AS c FROM docs WHERE lang = 'ja' GROUP BY lang",
            [
                "scalar_plan: index_equality",
                "access_path: scalar_index_candidates",
            ],
        ),
        (
            "EXPLAIN SELECT DISTINCT lang FROM docs ORDER BY lang DESC",
            "EXPLAIN SELECT DISTINCT lang FROM docs",
            [
                "scalar_plan: plain_scan",
                "access_path: scalar_index_group_enumeration",
            ],
        ),
    ];
    for (sql, baseline, expected) in cases {
        let visible = explain_in_fresh_session(&core, "tenant-a", sql);
        let invisible = explain_in_fresh_session(&core, "tenant-b", sql);
        assert_eq!(visible, invisible, "tenant mismatch for {sql}");
        let plain = explain_in_fresh_session(&core, "tenant-a", baseline);
        assert_eq!(visible, plain, "ORDER BY changed EXPLAIN for {sql}");
        assert_eq!(visible, expected, "unexpected plan for {sql}");

        // 入口の契約: `execute_sql` は EXPLAIN を 42601 で拒否する。
        let err = core
            .execute_sql(&ctx("tenant-a"), sql)
            .expect_err("execute_sql must reject EXPLAIN");
        assert_eq!(err.wire_code(), "42601", "mismatch for {sql}");
    }
}

#[test]
fn scalar_order_by_aggregate_explain_does_not_execute_body() {
    // Issue #1351・SQL-25・SQL-27: 本体を実行すると SUM(id) が u64 であふれて 22003 に
    // なるデータで、EXPLAIN は成功する（評価・ソートを行わない）ことを固定する。
    let path = unique_db_path("sql27-agg-order-by-no-exec");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    for id in [u64::MAX, 1] {
        insert_row(
            &storage,
            "docs",
            "tenant-a",
            id,
            vec![1.0, 0.0, 0.0, 0.0],
            "ja",
            Visibility::Private,
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let body = "SELECT lang, SUM(id) AS s FROM docs GROUP BY lang ORDER BY s DESC";
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx("tenant-a"), &mut session, body)
        .expect_err("body must overflow");
    assert_eq!(err.wire_code(), "22003", "precondition: body overflows");

    let lines = explain_in_fresh_session(&core, "tenant-a", &format!("EXPLAIN {body}"));
    assert_eq!(
        lines,
        vec![
            "scalar_plan: plain_scan",
            "access_path: scalar_index_group_enumeration"
        ]
    );
    let other = explain_in_fresh_session(&core, "tenant-b", &format!("EXPLAIN {body}"));
    assert_eq!(lines, other);

    // 書き込みしない: 件数は不変。
    let count = core
        .execute_sql(&ctx("tenant-a"), "SELECT COUNT(id) FROM docs")
        .expect("count after EXPLAIN");
    let cell = count
        .rows
        .into_iter()
        .next()
        .expect("count row")
        .cells
        .into_iter()
        .next()
        .expect("count cell");
    assert_eq!(cell, Cell::Integer(2));
}

#[test]
fn scalar_order_by_aggregate_explain_error_codes_match_non_explain() {
    // Issue #1351・SQL-25: EXPLAIN を前置しても ORDER BY の束縛検証（fail-closed）を
    // 迂回できず、同じ wire_code で拒否される。
    let path = unique_db_path("sql27-agg-order-by-error-parity");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let nine_keys = format!(
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY {}",
        ["c"; 9].join(", ")
    );
    let cases: Vec<(String, &str)> = vec![
        (
            "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY lang NULLS LAST".into(),
            "42601",
        ),
        (
            "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY 1".into(),
            "42601",
        ),
        (
            "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY c, nope".into(),
            "22000",
        ),
        (nine_keys, "54000"),
        (
            "SELECT lang, COUNT(*) AS c FROM missing GROUP BY lang ORDER BY c".into(),
            "42P01",
        ),
    ];
    for (body, expected) in cases {
        let mut session = SessionState::default();
        let plain = core
            .execute_sql_in_session(&ctx("tenant-a"), &mut session, &body)
            .expect_err(&format!("{body} must be rejected"));
        let mut session = SessionState::default();
        let explained = core
            .execute_sql_in_session(&ctx("tenant-a"), &mut session, &format!("EXPLAIN {body}"))
            .expect_err(&format!("EXPLAIN {body} must be rejected"));
        assert_eq!(plain.wire_code(), expected, "plain mismatch for {body}");
        assert_eq!(
            explained.wire_code(),
            plain.wire_code(),
            "parity mismatch for {body}"
        );
    }
}
