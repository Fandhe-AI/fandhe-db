//! `WHERE <col> LIKE '<pattern>'` の中間一致・後方一致・ワイルドカードの結合テスト
//! （SQL-24・TASK-208、Issue #914。ポインタ: `docs/spec/04-behavior/sql-surface.md`
//! SQL-24・`docs/spec/05-tasks.md` TASK-208）。
//!
//! `tests/declarative_filter.rs`（TASK-147・EXT-3）と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`engine::tenant::insert_typed_row`
//! による投入）で実 `Storage` 上にテーブルを構築し、`EngineCore::execute_sql` を
//! 検証する。意味論の契約は ADR `docs/design/like-wildcard-patterns.md` 参照。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::row_codec::Value;
use engine::sql::exec::{ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// `embedding VECTOR(2)`・`path TEXT`・`kind TEXT` を持つテーブルへ単一テナント
/// （`tenant-a`・`Public`）の行を投入する。
fn setup_single_tenant_table(storage: &Storage, rows: &[(u64, &str, &str)]) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("kind", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for (id, path, kind) in rows {
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("sql24-op-{id}"))
            .expect("valid operation id");
        let kind_value = if kind.is_empty() {
            Value::Null
        } else {
            Value::Text((*kind).to_string())
        };
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text((*path).to_string()),
                kind_value,
            ],
            &op_id,
        )
        .expect("insert row");
    }
}

fn select_paths(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core.execute_sql(ctx, sql).expect("SELECT should succeed");
    result_ids(&result)
}

// --- 受け付けと結果集合（後方一致・中間一致・`_`・エスケープ・`%%` 正規化・`'%'`）------

#[test]
fn accepts_suffix_middle_wildcard_and_escape_forms() {
    let path = unique_db_path("sql24-accepts-general-forms");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            (1, "src/lib.rs", "code"),
            (2, "src/main.rs", "code"),
            (3, "README.md", "doc"),
            (4, "src/a.rs", "code"),
            (5, "src/ab.rs", "code"),
            (6, "100%", "amount"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 後方一致。
    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE '%.rs' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![1, 2, 4, 5]
    );

    // 中間一致。
    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE '%/lib%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![1]
    );

    // `_`（1 文字ワイルドカード）: `src/a.rs` のみ一致、`src/ab.rs` は不一致。
    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE 'src/_.rs' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![4]
    );

    // エスケープ: `\%` はリテラル `%` として扱う。
    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE '100\\%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![6]
    );

    // `%%` は `%` に正規化される（`%.rs` と同じ結果）。
    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE '%%.rs' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![1, 2, 4, 5]
    );

    // ワイルドカードなし（完全一致）は従来どおり。
    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE 'README.md' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![3]
    );

    // `'%'` 単独は非 NULL の全行に一致する（NULL の `kind` 列とは無関係。
    // `path` は常に非 NULL のため全 6 行が一致する）。
    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE '%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn percent_alone_does_not_match_null_column() {
    let path = unique_db_path("sql24-percent-alone-null-column");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    // id=1 は kind=NULL、id=2 は kind="code"。
    setup_single_tenant_table(&storage, &[(1, "a.rs", ""), (2, "b.rs", "code")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE kind LIKE '%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![2]
    );
}

#[test]
fn underscore_matches_exactly_one_unicode_scalar() {
    let path = unique_db_path("sql24-underscore-multibyte");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            (1, "日本語", "text"),
            (2, "日語", "text"),
            (3, "日本本語", "text"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    assert_eq!(
        select_paths(
            &core,
            &ctx,
            "SELECT * FROM docs WHERE path LIKE '日_語' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![1]
    );
}

// --- `ESCAPE` 句は未対応（42601） ------------------------------------------------------

#[test]
fn escape_clause_is_rejected_as_unsupported_syntax() {
    let path = unique_db_path("sql24-escape-clause-unsupported");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(&storage, &[(1, "a%b", "code")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE path LIKE 'a!%%' ESCAPE '!' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect_err("ESCAPE clause must be rejected (unsupported syntax)");
    assert_eq!(err.wire_code(), "42601");
}

// --- パターン長の上限（4096 バイト） ---------------------------------------------------

#[test]
fn pattern_length_at_limit_is_accepted_and_over_limit_is_rejected() {
    let path = unique_db_path("sql24-pattern-length-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(&storage, &[(1, "x", "code")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // ちょうど 4096 バイト（末尾 `%` を含む）は受理される（前方一致へ振り分け）。
    let at_limit_prefix = "a".repeat(4095);
    let sql_at_limit = format!(
        "SELECT * FROM docs WHERE path LIKE '{at_limit_prefix}%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
    );
    core.execute_sql(&ctx, &sql_at_limit)
        .expect("pattern at MAX_LIKE_PATTERN_LEN must be accepted");

    // 4097 バイトは `54000`。
    let over_limit_prefix = "a".repeat(4096);
    let sql_over_limit = format!(
        "SELECT * FROM docs WHERE path LIKE '{over_limit_prefix}%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
    );
    let err = core
        .execute_sql(&ctx, &sql_over_limit)
        .expect_err("pattern over MAX_LIKE_PATTERN_LEN must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- EXPLAIN の `scalar_plan:` 行 -------------------------------------------------------

struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

const EXPANSION_RESPONSE_NO_HINTS: &str =
    r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#;

fn explain_lines(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(result) => {
            assert_eq!(result.columns.len(), 1);
            assert_eq!(
                result.columns[0],
                ColumnMeta::Computed {
                    name: "QUERY PLAN".to_string()
                }
            );
            result
                .rows
                .into_iter()
                .map(|row| {
                    let cell = row.cells.into_iter().next().expect("one cell per row");
                    match cell {
                        engine::sql::exec::Cell::Text(s) => s,
                        other => panic!("expected text cell, got {other:?}"),
                    }
                })
                .collect()
        }
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

/// `body TEXT` 列を追加した EXPLAIN 専用テーブル（`USING PLAN(...)` が要求する
/// 列。`tests/core_explain_plan_entry.rs` と同じ理由）。
fn setup_explain_table(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("kind", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = engine::recovery::required_op_id::OperationId::parse("sql24-explain-op-1")
        .expect("valid op id");
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("src/lib.rs".to_string()),
            Value::Text("code".to_string()),
            Value::Text("alpha content".to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

#[test]
fn explain_reports_plain_scan_for_middle_match_and_index_prefix_for_pure_prefix() {
    let path = unique_db_path("sql24-explain-scalar-plan");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_explain_table(&storage);
    let core = new_core(storage).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE_NO_HINTS,
    }));
    let mut session = SessionState::default();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 中間一致は `plain_scan`（二次索引が対応しない一般形）。
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE path LIKE '%lib%' USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    assert!(explain_lines(outcome).contains(&"scalar_plan: plain_scan".to_string()));

    // 純粋な前方一致は従来どおり `index_prefix`。
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE path LIKE 'src/%' USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    assert!(explain_lines(outcome).contains(&"scalar_plan: index_prefix".to_string()));

    // ワイルドカードなし（完全一致）は従来どおり `index_equality`。
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE path LIKE 'src/lib.rs' USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    assert!(explain_lines(outcome).contains(&"scalar_plan: index_equality".to_string()));

    // 中間一致と他述語の複合も `plain_scan`（`Like` は複合述語に紛れて索引
    // 被覆済みと誤判定されない単一情報源）。
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE kind = 'code' AND path LIKE '%x%' USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    assert!(explain_lines(outcome).contains(&"scalar_plan: plain_scan".to_string()));
}

// --- テナント境界: LIKE の一致対象・件数に他テナント行が現れない ------------------------

#[test]
fn tenant_boundary_is_enforced_for_percent_wildcard() {
    let path = unique_db_path("sql24-tenant-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    // `Visibility::Public` はテナント横断で可視（RLS の設計上の既定）のため、
    // 非漏えいを検証するには tenant-b 側を `Private` にする（`tests/
    // declarative_filter.rs::ext3_rls_is_enforced_before_metadata_filter` と
    // 同じ構成）。
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let op_a = engine::recovery::required_op_id::OperationId::parse("sql24-tenant-op-a")
        .expect("valid operation id");
    let op_b = engine::recovery::required_op_id::OperationId::parse("sql24-tenant-op-b")
        .expect("valid operation id");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_a,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("tenant-a/secret.md".to_string()),
        ],
        &op_a,
    )
    .expect("insert tenant-a row");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_b,
        2,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("tenant-b/secret.md".to_string()),
        ],
        &op_b,
    )
    .expect("insert tenant-b row");

    let core = new_core(storage);
    // tenant-a から見て `'%'`（全行一致）でも tenant-b の Private 行は
    // 決して現れない。
    assert_eq!(
        select_paths(
            &core,
            &ctx_a,
            "SELECT * FROM docs WHERE path LIKE '%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        vec![1]
    );
}
