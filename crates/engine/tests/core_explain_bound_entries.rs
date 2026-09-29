//! `EngineCore::explain_bound_search_in_session`／
//! `explain_bound_scan_in_session`／`explain_bound_aggregate_in_session`
//! （Issue #948・NOSQL-16・SQL-27・TASK-186）が、単一の `Storage` を
//! `EngineCore` が所有したまま SQL テキストを経由せずに束縛済み
//! search／scan／aggregate 計画の `EXPLAIN` を実行できること、SQL 表層の
//! `EXPLAIN` 応答（`tests/sql27_explain_targets.rs`・`tests/sql_explain.rs`）
//! と行単位で完全一致すること、検索・走査・集計の本体を実行しないこと、
//! テナント間で応答がバイト一致することを固定する結合テスト。
//!
//! `tests/core_bound_plan_entry.rs`（scan／aggregate の execute 系。
//! Issue #728）・`tests/core_explain_plan_entry.rs`（`USING PLAN` 付き
//! 検索 EXPLAIN。Issue #765）と同じ流儀で、`Storage` を
//! `EngineCore::from_storage` へそのまま渡して単一 `Storage` 構成を保つ。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_aggregate, bind_in_session, bind_scan, BoundScan};
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
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// tenant-a に `Public` 行 id 1..=3 を投入する。tenant-b は本テスト群では
/// 「可視行 0 件のテナント」（バイト一致確認用）として使う。
fn seed_tenant_a(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    for id in 1..=3u64 {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-a-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant ctx")
}

fn open_engine_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed_tenant_a(&storage);
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `validate_sql` が要求する `TableLookup` の固定応答実装
/// （`tests/core_bound_plan_entry.rs::FixedTableLookup` と同型）。
struct FixedTableLookup;

impl TableLookup for FixedTableLookup {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}

/// 新エントリの `QueryResult` を `Vec<String>` へ揃える
/// （`QUERY PLAN` 単一列であることも固定する）。
fn entry_lines(result: Result<QueryResult, SqlSurfaceError>) -> Vec<String> {
    let result = result.expect("bound explain entry should succeed");
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

/// SQL `EXPLAIN` 経由（`execute_sql_in_session`）の行を `Vec<String>` へ
/// 揃える。
fn sql_explain_lines(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(ctx, &mut session, sql)
        .expect("SQL EXPLAIN should succeed");
    match outcome {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// search（vector 指定）
// ---------------------------------------------------------------------

#[test]
fn search_entry_matches_sql_explain_for_distance_only() {
    let path = unique_db_path("explain-bound-search-distance");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1,0,0,0]' LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let got = entry_lines(core.explain_bound_search_in_session(
        &ctx_a,
        &session,
        TABLE,
        |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Select(validated_select) = validated else {
                panic!("expected Statement::Select");
            };
            bind_in_session(&validated_select, schema, session.search_mode(), udfs)
        },
    ));

    assert_eq!(got, expected);
}

#[test]
fn search_entry_matches_sql_explain_with_where_filter() {
    let path = unique_db_path("explain-bound-search-filter");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let sql = "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1,0,0,0]' LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let got = entry_lines(core.explain_bound_search_in_session(
        &ctx_a,
        &session,
        TABLE,
        |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Select(validated_select) = validated else {
                panic!("expected Statement::Select");
            };
            bind_in_session(&validated_select, schema, session.search_mode(), udfs)
        },
    ));

    assert_eq!(got, expected);
}

#[test]
fn search_entry_does_not_execute_search_or_touch_caches() {
    let path = unique_db_path("explain-bound-search-no-exec");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let before = core.visible_bitmap_cache_stats();
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1,0,0,0]' LIMIT 5";
    let _ = entry_lines(core.explain_bound_search_in_session(
        &ctx_a,
        &session,
        TABLE,
        |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Select(validated_select) = validated else {
                panic!("expected Statement::Select");
            };
            bind_in_session(&validated_select, schema, session.search_mode(), udfs)
        },
    ));
    let after = core.visible_bitmap_cache_stats();

    assert_eq!(
        before.hits, after.hits,
        "EXPLAIN must not touch VisibleBitmapCache (hits)"
    );
    assert_eq!(
        before.misses, after.misses,
        "EXPLAIN must not touch VisibleBitmapCache (misses)"
    );
    assert_eq!(
        before.entries, after.entries,
        "EXPLAIN must not touch VisibleBitmapCache (entries)"
    );
}

#[test]
fn search_entry_rejects_undefined_table_before_invoking_binder() {
    let path = unique_db_path("explain-bound-search-undefined-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .explain_bound_search_in_session(&ctx_a, &session, "no_such_table", |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for an undefined table");
        })
        .expect_err("undefined table should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(err, SqlSurfaceError::UndefinedTable { .. }));
}

#[test]
fn search_entry_rejects_bound_plan_for_another_table() {
    let path = unique_db_path("explain-bound-search-table-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    // `table = "docs"` を要求しつつ、closure は無関係なテーブル名で束縛済み
    // 計画を返す（`BoundStatement::new` の直接構築。
    // `tests/core_bound_plan_entry.rs::scan_entry_rejects_bound_plan_for_another_table`
    // と同じ判断）。
    let err = core
        .explain_bound_search_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
            Ok(engine::sql::parser::BoundStatement::new(
                "other_table".to_string(),
                Vec::new(),
                Vec::new(),
                false,
                engine::sql::parser::Ranking::Distance {
                    query: vec![1.0, 0.0, 0.0, 0.0],
                },
                5,
                engine::sql::plan::EvaluationOrder::DEFAULT,
            ))
        })
        .expect_err("table mismatch should be rejected");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

// ---------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------

#[test]
fn scan_entry_matches_sql_explain() {
    let path = unique_db_path("explain-bound-scan");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let sql = "SELECT id, lang FROM docs LIMIT 10";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let got =
        entry_lines(
            core.explain_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
                let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
                let Statement::Scan(validated_scan) = validated else {
                    panic!("expected Statement::Scan");
                };
                bind_scan(&validated_scan, schema, udfs)
            }),
        );

    assert_eq!(got, expected);
    // scan の EXPLAIN は常に固定値（`plain_scan`／`full_scan`）。
    assert_eq!(
        got,
        vec![
            "scalar_plan: plain_scan".to_string(),
            "access_path: full_scan".to_string(),
        ]
    );
}

#[test]
fn scan_entry_does_not_execute_scan() {
    let path = unique_db_path("explain-bound-scan-no-exec");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let sql = "SELECT id FROM docs LIMIT 10";
    let result = core
        .explain_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Scan(validated_scan) = validated else {
                panic!("expected Statement::Scan");
            };
            bind_scan(&validated_scan, schema, udfs)
        })
        .expect("explain_bound_scan_in_session should succeed");

    // 実行本体（`run_scan_plan`）を呼んでいれば行データ列が返るはずだが、
    // `QUERY PLAN` 単一列以外は含まれない。
    assert_eq!(result.columns.len(), 1);
}

#[test]
fn scan_entry_rejects_bound_plan_for_another_table() {
    let path = unique_db_path("explain-bound-scan-table-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let err = core
        .explain_bound_scan_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
            Ok(BoundScan::new(
                "other_table".to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                10,
            ))
        })
        .expect_err("table mismatch should be rejected");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

#[test]
fn scan_entry_rejects_undefined_table_before_invoking_binder() {
    let path = unique_db_path("explain-bound-scan-undefined-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .explain_bound_scan_in_session(&ctx_a, &session, "no_such_table", |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for an undefined table");
        })
        .expect_err("undefined table should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(err, SqlSurfaceError::UndefinedTable { .. }));
}

// ---------------------------------------------------------------------
// aggregate
// ---------------------------------------------------------------------

#[test]
fn aggregate_entry_matches_sql_explain_for_count_star() {
    let path = unique_db_path("explain-bound-aggregate-count");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let sql = "SELECT COUNT(*) FROM docs";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let got = entry_lines(core.explain_bound_aggregate_in_session(
        &ctx_a,
        &session,
        TABLE,
        |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            bind_aggregate(&validated_aggregate, schema, udfs)
        },
    ));

    assert_eq!(got, expected);
}

#[test]
fn aggregate_entry_matches_sql_explain_with_where_filter() {
    let path = unique_db_path("explain-bound-aggregate-filter");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let sql = "SELECT COUNT(*) FROM docs WHERE lang = 'ja'";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let got = entry_lines(core.explain_bound_aggregate_in_session(
        &ctx_a,
        &session,
        TABLE,
        |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            bind_aggregate(&validated_aggregate, schema, udfs)
        },
    ));

    assert_eq!(got, expected);
}

#[test]
fn aggregate_entry_does_not_touch_visible_bitmap_cache() {
    let path = unique_db_path("explain-bound-aggregate-no-exec");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let before = core.visible_bitmap_cache_stats();
    let sql = "SELECT COUNT(*) FROM docs";
    let _ = entry_lines(core.explain_bound_aggregate_in_session(
        &ctx_a,
        &session,
        TABLE,
        |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            bind_aggregate(&validated_aggregate, schema, udfs)
        },
    ));
    let after = core.visible_bitmap_cache_stats();

    assert_eq!(
        before.hits, after.hits,
        "EXPLAIN must not touch VisibleBitmapCache (hits)"
    );
    assert_eq!(
        before.misses, after.misses,
        "EXPLAIN must not touch VisibleBitmapCache (misses)"
    );
    assert_eq!(
        before.entries, after.entries,
        "EXPLAIN must not touch VisibleBitmapCache (entries)"
    );
}

#[test]
fn aggregate_entry_is_byte_identical_across_different_visible_row_counts() {
    let path_a = unique_db_path("explain-bound-aggregate-tenant-a");
    let _guard_a = CleanupGuard(path_a.clone());
    let core_a = open_engine_core(&path_a);
    let ctx_a = ctx_for("tenant-a");

    let path_b = unique_db_path("explain-bound-aggregate-tenant-b-empty");
    let _guard_b = CleanupGuard(path_b.clone());
    // tenant-b は可視行 0 件（`seed_tenant_a` は tenant-a 専用データのみ投入）。
    let core_b = open_engine_core(&path_b);
    let ctx_b = ctx_for("tenant-b");

    let session = SessionState::default();
    let sql = "SELECT COUNT(*) FROM docs WHERE lang = 'ja'";

    let bind = |schema: &TableSchema, udfs: &_| {
        let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        bind_aggregate(&validated_aggregate, schema, udfs)
    };

    let lines_a =
        entry_lines(core_a.explain_bound_aggregate_in_session(&ctx_a, &session, TABLE, bind));
    let lines_b =
        entry_lines(core_b.explain_bound_aggregate_in_session(&ctx_b, &session, TABLE, bind));

    assert_eq!(
        lines_a, lines_b,
        "EXPLAIN output must not depend on visible row count (no cardinality leak)"
    );
}

#[test]
fn aggregate_entry_rejects_bound_plan_for_another_table() {
    let path = unique_db_path("explain-bound-aggregate-table-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    // `table = "docs"` を要求しつつ、closure は無関係なテーブル名で束縛済み
    // 計画を返す（`BoundAggregate::new` の直接構築。scan／search の
    // table-mismatch テストと同じ判断）。
    let err = core
        .explain_bound_aggregate_in_session(&ctx_a, &session, TABLE, |schema, _udfs| {
            let item = engine::sql::parser::BoundAggregateItem::bind(
                engine::sql::allowlist::AggregateFunc::Count,
                engine::sql::parser::AggregateTarget::Star,
                schema,
            )?;
            engine::sql::parser::BoundAggregate::new(
                "other_table".to_string(),
                vec![item],
                Vec::new(),
                Vec::new(),
            )
        })
        .expect_err("table mismatch should be rejected");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

#[test]
fn aggregate_entry_rejects_undefined_table_before_invoking_binder() {
    let path = unique_db_path("explain-bound-aggregate-undefined-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .explain_bound_aggregate_in_session(&ctx_a, &session, "no_such_table", |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for an undefined table");
        })
        .expect_err("undefined table should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(err, SqlSurfaceError::UndefinedTable { .. }));
}
