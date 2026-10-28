//! `EngineCore::execute_bound_scan_in_session`／`execute_bound_aggregate_in_session`
//! （TASK-186・NOSQL-3・NOSQL-4・NOSQL-5。Issue #728）が、単一の `Storage` を
//! `EngineCore` が所有したまま SQL テキストを経由せずに束縛済み scan／aggregate
//! 計画を実行できること、RLS（`PolicyContext`）を暗黙適用すること、
//! fail-closed に拒否すべき入力を拒否することを固定する結合テスト。
//!
//! `tests/sql_scan_public_api.rs`・`tests/sql_aggregate_public_api.rs` と異なり、
//! ここでは `Storage` を `EngineCore::from_storage` へそのまま渡して単一
//! `Storage` 構成を保つ（生 `redb::Database` の再オープンを使わない。本 Issue が
//! 固定したいのはまさに「単一 `Storage` 構成での実行」であるため）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, PlanSearchBinding};
use engine::embedding::{EmbedError, Embedder};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::row_codec::Value;
use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::parser::{
    bind_aggregate, bind_in_session, bind_scan, AggregateTarget, BoundAggregate,
    BoundAggregateItem, BoundScan, BoundStatement,
};
use engine::sql::udf_call::{define_function, Expr};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
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

/// tenant-a に `Public` 行 id 1..=5（`lang` = `"ja"` 3 件・`"en"` 2 件）、
/// tenant-b に `Private` 行 id 101..=103（`lang` = `"xx"`。tenant-b にしか
/// 存在しないグループ値）を投入する（`tests/sql_aggregate_public_api.rs::seed_two_tenants`
/// と同じ判断。RLS 境界確認用）。
fn seed_two_tenants(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");

    let langs = ["ja", "ja", "ja", "en", "en"];
    for (idx, lang) in langs.iter().enumerate() {
        let id = idx as u64 + 1;
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
                Value::Text((*lang).to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }
    for id in 101..=103u64 {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-b-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
                Value::Text("xx".to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-b row");
    }
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant ctx")
}

fn open_engine_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed_two_tenants(&storage);
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `validate_sql`（`bind_aggregate` 用の `Statement::Aggregate` を得るため）が
/// 要求する `TableLookup` の固定応答実装。`EngineCore::from_storage` が
/// `Storage` を所有してしまった後は `Storage`（`impl TableLookup`）に SQL
/// テキスト検証専用でアクセスする手段がないため、本テストではテーブル名が
/// `TABLE` と一致する場合のみ `Ok(true)` を返す最小実装で代替する
/// （束縛・実行そのものは `EngineCore` 側のスキーマ・redb 走査で行うため、
/// この固定応答は構文検証の入口としてのみ使う）。
struct FixedTableLookup;

impl TableLookup for FixedTableLookup {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}

#[test]
fn scan_entry_applies_rls_implicitly_with_single_storage() {
    let path = unique_db_path("bound-plan-scan-rls");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let result = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql = "SELECT id, lang FROM docs LIMIT 10";
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Scan(validated_scan) = validated else {
                panic!("expected Statement::Scan");
            };
            bind_scan(&validated_scan, schema, udfs)
        })
        .expect("execute_bound_scan_in_session should succeed");

    // tenant-a の可視行は id 1..=5 のみ（tenant-b の Private 行は非漏えい）。
    assert_eq!(result.columns.len(), 2);
    assert_eq!(result.rows.len(), 5);
    let mut ids: Vec<u64> = result
        .rows
        .iter()
        .map(|row| match row.cells[0] {
            Cell::Integer(id) => id,
            ref other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn scan_entry_rejects_undefined_table_before_invoking_binder() {
    let path = unique_db_path("bound-plan-scan-undefined-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, "no_such_table", |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for an undefined table");
        })
        .expect_err("undefined table should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(err, SqlSurfaceError::UndefinedTable { .. }));
}

/// PR #788 レビュー指摘（Issue #728）: `table` に識別子として形式不正な文字列
/// （`catalog::validate_identifier` が `CatalogError::Invalid` を返す入力。
/// ここでは英数字のみだが上限〔63 バイト〕超過の 64 バイト文字列を使う。
/// SQL テキスト経由〔`validate_sql` → `TableLookup::table_exists` →
/// `catalog::table_lookup_error`〕はこれを `wire_code` `42601`
/// （`UnsupportedSyntax`）へ分類するため、本エントリでも `Internal`（`XX000`）
/// ではなく同じ分類に丸め込まれることを固定する（`read_txn_with_schema` が
/// 単一の写像本体 `catalog::table_lookup_error` を共有する契約）。
#[test]
fn scan_entry_classifies_malformed_table_name_same_as_sql_path() {
    let path = unique_db_path("bound-plan-scan-malformed-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();
    let malformed_table = "a".repeat(64);

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, &malformed_table, |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for a malformed table name");
        })
        .expect_err("malformed table name should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }),
        "expected UnsupportedSyntax (42601), got {err:?}"
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn scan_entry_propagates_binder_error_unchanged() {
    let path = unique_db_path("bound-plan-scan-binder-error");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
            Err(SqlSurfaceError::InvalidInput {
                detail: "synthetic binder failure".to_string(),
            })
        })
        .expect_err("binder error should propagate unchanged");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

#[test]
fn scan_entry_rejects_bound_plan_for_another_table() {
    let path = unique_db_path("bound-plan-scan-table-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    // `table = "docs"` を要求しつつ、closure は無関係なテーブル名で束縛済み
    // 計画を返す（`BoundScan::new` の直接構築。TASK-186・NOSQL-3・Issue #726）。
    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
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
fn aggregate_entry_matches_sql_path_and_shares_visible_bitmap_cache() {
    let path = unique_db_path("bound-plan-aggregate-cache");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let run_once = || {
        core.execute_bound_aggregate_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql = "SELECT COUNT(*) FROM docs";
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            bind_aggregate(&validated_aggregate, schema, udfs)
        })
        .expect("execute_bound_aggregate_in_session should succeed")
    };

    // 同じセッション経由の SQL 実行結果（`VisibleBitmapCache`・Issue #478 の
    // 対象クエリ形）と `Cell` レベルで一致することを固定する。
    let mut sql_session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(&ctx_a, &mut sql_session, "SELECT COUNT(*) FROM docs")
        .expect("execute_sql_in_session should succeed");
    let engine::sql::SqlOutcome::Query(sql_result) = sql_outcome else {
        panic!("expected SqlOutcome::Query");
    };

    let bound_result_1 = run_once();
    assert_eq!(bound_result_1.rows, sql_result.rows);
    assert_eq!(bound_result_1.rows[0].cells[0], Cell::Integer(5));

    // 2 回目の呼び出しで `VisibleBitmapCache` がヒットすること
    // （公開ラッパー `sql::aggregate::execute_aggregate`〔キャッシュ非経由〕との
    // 差別化。SQL 経路と同じキャッシュ配線を共有する証拠）。
    let _bound_result_2 = run_once();
    let stats = core.visible_bitmap_cache_stats();
    assert!(
        stats.hits >= 1,
        "expected at least one VisibleBitmapCache hit, got {stats:?}"
    );
}

/// `scan_entry_classifies_malformed_table_name_same_as_sql_path` と同じ判断
/// （PR #788 レビュー指摘・Issue #728）を `execute_bound_aggregate_in_session`
/// 側にも固定する。
#[test]
fn aggregate_entry_classifies_malformed_table_name_same_as_sql_path() {
    let path = unique_db_path("bound-plan-aggregate-malformed-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();
    let malformed_table = "a".repeat(64);

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .execute_bound_aggregate_in_session(&ctx_a, &session, &malformed_table, |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for a malformed table name");
        })
        .expect_err("malformed table name should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }),
        "expected UnsupportedSyntax (42601), got {err:?}"
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn aggregate_entry_group_by_excludes_other_tenant_groups() {
    let path = unique_db_path("bound-plan-aggregate-group-by");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let result = core
        .execute_bound_aggregate_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY n DESC";
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            let bound = bind_aggregate(&validated_aggregate, schema, udfs)?;
            assert!(bound.has_group_by());
            Ok(bound)
        })
        .expect("execute_bound_aggregate_in_session should succeed for GROUP BY");

    assert_eq!(result.rows.len(), 2);
    let langs: Vec<String> = result
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(text) => text.clone(),
            other => panic!("expected Cell::Text for lang, got {other:?}"),
        })
        .collect();
    assert!(langs.contains(&"ja".to_string()));
    assert!(langs.contains(&"en".to_string()));
    // tenant-b 専有のグループ値（`"xx"`）は現れない（RLS-7・RLS-8）。
    assert!(!langs.contains(&"xx".to_string()));
}

#[test]
fn binder_receives_session_udf_registry() {
    let path = unique_db_path("bound-plan-scan-session-udf");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let mut session = SessionState::default();
    define_function(
        session.udfs_mut(),
        "answer",
        &[],
        &Expr::Number("42".to_string()),
    )
    .expect("define_function should succeed");

    let observed = std::sync::Mutex::new(false);
    let result = core.execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
        *observed.lock().expect("lock") = udfs.get("answer").is_some();
        let sql = "SELECT id FROM docs LIMIT 10";
        let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
        let Statement::Scan(validated_scan) = validated else {
            panic!("expected Statement::Scan");
        };
        bind_scan(&validated_scan, schema, udfs)
    });

    assert!(result.is_ok());
    assert!(
        *observed.lock().expect("lock"),
        "binder should observe the session's registered UDF"
    );
}

/// TASK-186・NOSQL-4（Issue #768）: `execute_bound_aggregate_in_session` の
/// binder closure が `BoundAggregate::new`（SQL テキスト非経由の直接構築）を
/// 返す形で、SQL テキスト経由（`bind_aggregate`）と `Cell` レベルで完全一致し
/// RLS（`ctx`）を暗黙適用することを固定する。`bound-plan-session-entry.md` が
/// 「TASK-177 へ申し送り」としていた `BoundAggregate::new` の到達性が、本 Issue
/// で初めて成立することの非 vacuous 証跡（`wire-server::http::query::aggregate`
/// が呼ぶ経路そのものを in-process で再現する）。
#[test]
fn aggregate_entry_accepts_binder_built_without_sql_text() {
    let path = unique_db_path("bound-plan-aggregate-no-sql-text");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let bound_result = core
        .execute_bound_aggregate_in_session(&ctx_a, &session, TABLE, |schema, _udfs| {
            let items = vec![
                BoundAggregateItem::bind(
                    engine::sql::allowlist::AggregateFunc::Count,
                    AggregateTarget::Star,
                    schema,
                )?,
                BoundAggregateItem::bind(
                    engine::sql::allowlist::AggregateFunc::Min,
                    AggregateTarget::Column("lang".to_string()),
                    schema,
                )?,
            ];
            BoundAggregate::new(TABLE.to_string(), items, Vec::new(), Vec::new())
        })
        .expect("execute_bound_aggregate_in_session should succeed without SQL text");

    let mut sql_session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(
            &ctx_a,
            &mut sql_session,
            "SELECT COUNT(*), MIN(lang) FROM docs",
        )
        .expect("execute_sql_in_session should succeed");
    let engine::sql::SqlOutcome::Query(sql_result) = sql_outcome else {
        panic!("expected SqlOutcome::Query");
    };

    // tenant-a の可視行は 5 件（`lang` = "ja" 3 件・"en" 2 件）。tenant-b の
    // Private 行（`lang = "xx"`）が RLS 越しに混入していれば `MIN(lang)` は
    // 辞書順最小の `"en"` ではなく別値になる（RLS-7・RLS-8 相当の非漏えい確認）。
    assert_eq!(bound_result.rows, sql_result.rows);
    assert_eq!(bound_result.rows[0].cells[0], Cell::Integer(5));
    assert_eq!(bound_result.rows[0].cells[1], Cell::Text("en".to_string()));
}
// --- search（TASK-186・NOSQL-2。Issue #764）: `execute_bound_search_in_session`
// （`vector` 指定）・`execute_bound_plan_search_in_session`（`plan` 指定）------

/// `body` 列を持つスキーマ（`USING PLAN` の本文列規約に必要。`schema()` は
/// scan／aggregate 専用のため search 系テストでは別スキーマを使う）。
fn search_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            // `USING PLAN` の辞書必須列（`path`／`body`。TASK-109・PLAN-5）を
            // 満たす（`plan_entry_*` テストが `dictionary_required_columns`
            // の `path` 欠落で `22000` へ落ちないようにする）。
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// tenant-a に `Public` 行 id 1・2（`alpha`／`beta` 語彙）、tenant-b に
/// `Private` 行 id 101（tenant-a のクエリ結果に混入してはならない RLS 境界
/// 対照。`lang` = `"xx"`）を投入する。
fn seed_search_two_tenants(storage: &Storage) {
    storage
        .create_table(&search_schema())
        .expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");

    let rows: [(u64, [f32; 4], &str, &str, &str); 2] = [
        (
            1,
            [0.1, 0.2, 0.3, 0.4],
            "ja",
            "docs/a.md",
            "alpha content in english",
        ),
        (
            2,
            [0.4, 0.3, 0.2, 0.1],
            "en",
            "docs/b.md",
            "beta content in english",
        ),
    ];
    for (id, emb, lang, path, body) in rows {
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
            "search-tenant-a-op-{id}"
        ))
        .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(path.to_string()),
                Value::Text(body.to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }
    let op_id_b = engine::recovery::required_op_id::OperationId::parse("search-tenant-b-op-101")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx_b,
        101,
        Visibility::Private,
        &[
            Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
            Value::Text("xx".to_string()),
            Value::Text("docs/private.md".to_string()),
            Value::Text("alpha content belonging to tenant-b".to_string()),
        ],
        &op_id_b,
    )
    .expect("insert tenant-b row");
}

fn open_search_engine_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed_search_two_tenants(&storage);
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// テキスト長だけを成分へ埋め込む決定的・ネットワーク不要な埋め込み
/// （`crates/wire-server/tests/nosql2_search.rs::DeterministicEmbedder` と同じ
/// 方針）。
struct DeterministicEmbedder {
    dim: u32,
}

impl Embedder for DeterministicEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| vec![t.len() as f32 * 0.01; self.dim as usize])
            .collect())
    }
}

/// 固定の展開結果を返し、呼び出し回数を記録するスタブ `LlmClient`
/// （`plan_entry_does_not_invoke_llm_when_binder_fails_pre_check` が非
/// vacuous な「呼ばれなかった」証跡を取るために使う）。
struct CountingStubLlmClient {
    response: &'static str,
    calls: std::sync::atomic::AtomicUsize,
}

impl LlmClient for CountingStubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.response.to_string())
    }
}

const SEARCH_EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#;

/// `vector` 指定エントリが RLS を暗黙適用し、SQL テキスト経由（`ORDER BY
/// <=>`）と `Cell` レベルで完全一致することを固定する。
#[test]
fn search_entry_applies_rls_implicitly_and_matches_sql_path() {
    let path = unique_db_path("bound-plan-search-rls");
    let _guard = CleanupGuard(path.clone());
    let core = open_search_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10";

    let result = core
        .execute_bound_search_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Select(validated_select) = validated else {
                panic!("expected Statement::Select");
            };
            bind_in_session(&validated_select, schema, session.search_mode(), udfs)
        })
        .expect("execute_bound_search_in_session should succeed");

    let mut sql_session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(&ctx_a, &mut sql_session, sql)
        .expect("execute_sql_in_session should succeed");
    let engine::sql::SqlOutcome::Query(sql_result) = sql_outcome else {
        panic!("expected SqlOutcome::Query");
    };
    assert_eq!(result.rows, sql_result.rows);

    // tenant-b の Private 行（id=101）は 2 件（tenant-a 可視行数）を超えて
    // 混入しない（RLS-7）。
    assert_eq!(result.rows.len(), 2);
    let ids: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match row.cells[0] {
            Cell::Integer(id) => id as i64,
            ref other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    assert!(!ids.contains(&101));
}

#[test]
fn search_entry_rejects_undefined_table_before_invoking_binder() {
    let path = unique_db_path("bound-plan-search-undefined-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_search_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .execute_bound_search_in_session(&ctx_a, &session, "no_such_table", |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for an undefined table");
        })
        .expect_err("undefined table should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(err, SqlSurfaceError::UndefinedTable { .. }));
}

#[test]
fn search_entry_propagates_binder_error_unchanged() {
    let path = unique_db_path("bound-plan-search-binder-error");
    let _guard = CleanupGuard(path.clone());
    let core = open_search_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let err = core
        .execute_bound_search_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
            Err(SqlSurfaceError::InvalidInput {
                detail: "synthetic binder failure".to_string(),
            })
        })
        .expect_err("binder error should propagate unchanged");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

#[test]
fn search_entry_rejects_bound_plan_for_another_table() {
    let path = unique_db_path("bound-plan-search-table-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = open_search_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let err = core
        .execute_bound_search_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
            Ok(BoundStatement::new(
                "other_table".to_string(),
                Vec::new(),
                Vec::new(),
                false,
                engine::sql::parser::Ranking::Distance {
                    query: vec![0.1, 0.2, 0.3, 0.4],
                },
                10,
                engine::sql::plan::EvaluationOrder::DEFAULT,
            ))
        })
        .expect_err("table mismatch should be rejected");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

/// `plan` 指定エントリが SQL `USING PLAN(...)` と `Cell` レベルで完全一致
/// することを固定する（決定的スタブ経由。実 Ollama 疎通は対象外）。
#[test]
fn plan_entry_matches_sql_using_plan_path() {
    let path = unique_db_path("bound-plan-plan-sql-parity");
    let _guard = CleanupGuard(path.clone());
    let core = open_search_engine_core(&path)
        .with_embedder(Box::new(DeterministicEmbedder { dim: 4 }))
        .with_query_planner(Box::new(CountingStubLlmClient {
            response: SEARCH_EXPANSION_RESPONSE,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }));
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();
    let question = "find content";

    let result = core
        .execute_bound_plan_search_in_session(
            &ctx_a,
            &session,
            TABLE,
            question,
            None,
            10,
            |_schema, _udfs| {
                Ok(PlanSearchBinding::new(
                    vec![engine::sql::parser::ProjectedColumn::Id],
                    Vec::new(),
                ))
            },
        )
        .expect("execute_bound_plan_search_in_session should succeed");

    let mut sql_session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(
            &ctx_a,
            &mut sql_session,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("execute_sql_in_session should succeed");
    let engine::sql::SqlOutcome::Query(sql_result) = sql_outcome else {
        panic!("expected SqlOutcome::Query");
    };
    assert_eq!(result.rows, sql_result.rows);
    assert_eq!(result.rows.len(), 2);
}

/// `pre_check`（I/O 前）で binder が拒否する場合、LLM 呼び出しが 0 回のまま
/// 拒否されることを固定する（`run_using_plan_select` の I/O 前拒否契約の
/// 非 vacuous な証跡）。
#[test]
fn plan_entry_does_not_invoke_llm_when_binder_fails_pre_check() {
    let path = unique_db_path("bound-plan-plan-pre-check-fails");
    let _guard = CleanupGuard(path.clone());
    let planner = std::sync::Arc::new(CountingStubLlmClient {
        response: SEARCH_EXPANSION_RESPONSE,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    // `LlmClient` 注入は所有権を要求する（`Box<dyn LlmClient>`）ため、
    // 呼び出し回数を後から観測できるよう別スレッド共有可能な `Arc` を経由し、
    // `EngineCore` へは `Arc` をラップする薄い転送実装を渡す。
    struct ForwardingLlmClient(std::sync::Arc<CountingStubLlmClient>);
    impl LlmClient for ForwardingLlmClient {
        fn complete(&self, prompt: &str) -> Result<String, PlanError> {
            self.0.complete(prompt)
        }
    }
    let core = open_search_engine_core(&path)
        .with_embedder(Box::new(DeterministicEmbedder { dim: 4 }))
        .with_query_planner(Box::new(ForwardingLlmClient(std::sync::Arc::clone(
            &planner,
        ))));
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let bind_call_count = std::sync::atomic::AtomicUsize::new(0);
    let err = core
        .execute_bound_plan_search_in_session(
            &ctx_a,
            &session,
            TABLE,
            "find content",
            None,
            10,
            |_schema, _udfs| {
                bind_call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(SqlSurfaceError::InvalidInput {
                    detail: "synthetic pre-check failure".to_string(),
                })
            },
        )
        .expect_err("pre_check failure should reject before I/O");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
    // `bind` は `pre_check` としての 1 回のみ呼ばれ（`Err` を返して即座に
    // 拒否するため `bind`〔本体〕としての 2 回目は呼ばれない）、I/O
    // （`plan_query`／LLM 呼び出し）へは一切進んでいない。
    assert_eq!(bind_call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        planner.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "LLM must not be invoked when the pre-check rejects the request"
    );
}

/// `pre_check`（binder）が返すエラーは、辞書必須列（`path`/`body`）を欠く
/// テーブルに対しても辞書必須列検証（`22000`）より優先される（codex-review
/// P1 指摘対応・PR #828。`run_using_plan_select` が辞書必須列検証を
/// `pre_check` より先に行うと、辞書必須列を欠くテーブルに対して `pre_check`
/// が返すべきエラー——ここでは `declarative_filter::check_filter_count` が
/// フィルタ件数超過時に返す `54000`——より先に `22000` が確定してしまう
/// 回帰。`crates/engine/tests/core_explain_plan_entry.rs::
/// explain_entry_prioritizes_binder_plan_missing_error_over_dictionary_columns`
/// と対になる検証で、`run_explain_plan`〔`EXPLAIN` 経路〕・
/// `run_using_plan_select`〔通常検索経路〕の双方が同一の優先順位を保つ
/// ことを固定する）。
#[test]
fn plan_entry_prioritizes_pre_check_error_over_dictionary_columns() {
    let path = unique_db_path("bound-plan-plan-pre-check-vs-dictionary");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // `path`/`body` 列を欠くスキーマ（辞書必須列検証の対象。`schema()` は
    // scan／aggregate 専用の最小スキーマで `embedding`／`lang` のみ持つ）。
    storage.create_table(&schema()).expect("create table");
    let ctx_a = ctx_for("tenant-a");
    let op_id = engine::recovery::required_op_id::OperationId::parse("dict-vs-pre-check-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
            Value::Text("ja".to_string()),
        ],
        &op_id,
    )
    .expect("insert tenant-a row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(CountingStubLlmClient {
            response: SEARCH_EXPANSION_RESPONSE,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
    );
    let session = SessionState::default();

    // `declarative_filter::check_filter_count` がフィルタ件数超過時に返す
    // `54000`（`pub(crate)` のため直接呼べず、同型のエラーをここで模す）。
    let err = core
        .execute_bound_plan_search_in_session(
            &ctx_a,
            &session,
            TABLE,
            "find content",
            None,
            10,
            |_schema, _udfs| {
                Err(SqlSurfaceError::PayloadTooLarge {
                    detail: "metadata filter count exceeds limit".to_string(),
                })
            },
        )
        .expect_err("pre_check payload-too-large error must win over dictionary column check");

    assert!(matches!(err, SqlSurfaceError::PayloadTooLarge { .. }));
    assert_eq!(err.wire_code(), "54000");
}

/// `query_planner`／`embedder` のいずれも未注入だと fail-closed（`XX000`）で
/// 拒否される（SQL 表層の既存契約と同一分類。`crates/engine/tests/
/// sql_using_plan.rs::using_plan_fails_closed_without_query_planner` と同型）。
#[test]
fn plan_entry_fails_closed_without_query_planner_or_embedder() {
    let path = unique_db_path("bound-plan-plan-unconfigured");
    let _guard = CleanupGuard(path.clone());
    let core = open_search_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let err = core
        .execute_bound_plan_search_in_session(
            &ctx_a,
            &session,
            TABLE,
            "find content",
            None,
            10,
            |_schema, _udfs| {
                Ok(PlanSearchBinding::new(
                    vec![engine::sql::parser::ProjectedColumn::Id],
                    Vec::new(),
                ))
            },
        )
        .expect_err("plan search without embedder/query_planner must fail closed");

    assert_eq!(err.wire_code(), "XX000");
}

/// `VECTOR` 列を持たないテーブルへの `plan` 指定検索は、LLM 展開・再埋め込み
/// （高コスト I/O）を一切行わずに `22000` で拒否される（`sql::using_plan::
/// pre_check_bindable` が SQL 表層 `USING PLAN` で保証する fail-closed 順序と
/// 同一。codex-review 指摘対応・PR #827。`crate::sql::parser::vector_column`
/// が pre_check クロージャ内で先に呼ばれることの非 vacuous な証跡として
/// `CountingStubLlmClient` の呼び出し回数を 0 のまま固定する）。
#[test]
fn plan_entry_rejects_table_without_vector_column_before_llm_call() {
    let path = unique_db_path("bound-plan-plan-no-vector-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let no_vector_schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    storage
        .create_table(&no_vector_schema)
        .expect("create table without VECTOR column");
    let planner = std::sync::Arc::new(CountingStubLlmClient {
        response: SEARCH_EXPANSION_RESPONSE,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    // `LlmClient` 注入は所有権を要求するため、呼び出し回数を後から観測できる
    // よう `Arc` 経由の薄い転送実装を渡す（`plan_entry_does_not_invoke_llm_
    // when_binder_fails_pre_check` と同じ理由）。
    struct ForwardingLlmClient(std::sync::Arc<CountingStubLlmClient>);
    impl LlmClient for ForwardingLlmClient {
        fn complete(&self, prompt: &str) -> Result<String, PlanError> {
            self.0.complete(prompt)
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder { dim: 4 }))
        .with_query_planner(Box::new(ForwardingLlmClient(std::sync::Arc::clone(
            &planner,
        ))));
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let err = core
        .execute_bound_plan_search_in_session(
            &ctx_a,
            &session,
            TABLE,
            "find content",
            None,
            10,
            |_schema, _udfs| {
                Ok(PlanSearchBinding::new(
                    vec![engine::sql::parser::ProjectedColumn::Id],
                    Vec::new(),
                ))
            },
        )
        .expect_err("plan search on a table without a VECTOR column must be rejected");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "LLM must not be invoked when the table has no VECTOR column"
    );
}
