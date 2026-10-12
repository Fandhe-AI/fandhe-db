//! 明示 `LIMIT` の範囲上限の起動時設定（Issue #1555。ポインタ: `docs/spec/04-behavior/`
//! SQL-34・SQL-46）の結合テスト。
//!
//! `EngineCore::with_result_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server の simple／extended プロトコルが共有する実行入口）を production 経路として
//! 検証する。確認する契約:
//!
//! - 既定（未注入）は従来どおり `LIMIT 10000` まで受理し `10001` は `22000`
//! - 設定値を下げた場合、ちょうどは受理・+1 は `22000`（走査・kNN・`FETCH`・集合演算全体・
//!   JOIN 文・評価後射影ビュー・束縛済み計画の再検査）
//! - 設定値を上げた場合、kNN（総当たり・hybrid・precision・HNSW）が 10001 以上でも失敗しない
//! - `OFFSET` の範囲は設定値と連動しない

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::{ResultLimits, MAX_RESULT_ROWS_HARD_CAP};
use engine::row_codec::Value;
use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

struct DocsOnly;

impl TableLookup for DocsOnly {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == "docs")
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage, rows: u64) {
    storage.create_table(&schema()).expect("create table");
    for id in 1..=rows {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx(),
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 1.0]),
                Value::Text(format!("term{id} common")),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("insert row");
    }
}

fn core_with(path: &std::path::Path, limit: Option<usize>) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed(&storage, 5);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match limit {
        Some(n) => core.with_result_limits(ResultLimits::new(n)),
        None => core,
    }
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut session, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    match run(core, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// `LIMIT {max}` は受理し、`LIMIT {max + 1}` は `22000` になることを `template` で確かめる。
fn assert_boundary(core: &EngineCore, max: usize, template: impl Fn(usize) -> String) {
    ok(core, &template(max));
    assert_eq!(
        code(core, &template(max + 1)),
        "22000",
        "{}",
        template(max + 1)
    );
}

fn scan(n: usize) -> String {
    format!("SELECT id FROM docs LIMIT {n}")
}

fn knn(n: usize) -> String {
    format!("SELECT id FROM docs ORDER BY embedding <=> '[1,1]' LIMIT {n}")
}

fn hybrid(n: usize) -> String {
    format!("SELECT id FROM docs ORDER BY HYBRID(embedding, '[1,1]', body, 'common') LIMIT {n}")
}

fn precision(n: usize) -> String {
    format!("SELECT id FROM docs ORDER BY embedding <=> '[1,1]' LIMIT {n} USING MODE 'precision'")
}

#[test]
fn default_matches_legacy_ten_thousand() {
    let path = unique_db_path("result-limits-default");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, None);
    assert_eq!(core.result_limits(), ResultLimits::default());
    assert_boundary(&core, 10_000, scan);
    assert_boundary(&core, 10_000, knn);
    assert_boundary(&core, 10_000, hybrid);
}

#[test]
fn lowered_limit_applies_to_scan_knn_hybrid_and_precision() {
    let path = unique_db_path("result-limits-lowered");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, Some(50));
    assert_boundary(&core, 50, scan);
    assert_boundary(&core, 50, knn);
    assert_boundary(&core, 50, hybrid);
    assert_boundary(&core, 50, precision);
}

#[test]
fn lowered_limit_applies_to_set_operation_join_and_view() {
    let path = unique_db_path("result-limits-lowered-others");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, Some(50));
    assert_boundary(&core, 50, |n| {
        format!("SELECT id FROM docs UNION ALL SELECT id FROM docs LIMIT {n}")
    });
    assert_boundary(&core, 50, |n| {
        format!("SELECT a.id FROM docs a JOIN docs b ON a.id = b.id LIMIT {n}")
    });
    // 評価後射影形ビュー（本文が LIMIT 付き）の外側 LIMIT。
    ok(&core, "CREATE VIEW v AS SELECT id FROM docs LIMIT 3");
    assert_boundary(&core, 50, |n| format!("SELECT id FROM v LIMIT {n}"));
}

#[test]
fn lowered_limit_applies_to_declare_and_fetch() {
    let path = unique_db_path("result-limits-cursor");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, Some(50));
    let caller = ctx();
    let declare = |n: usize| format!("DECLARE c CURSOR FOR SELECT id FROM docs LIMIT {n}");
    // 各ケースは独立したトランザクションで実行する（失敗した文は txn を異常終了にするため）。
    let in_txn = |statements: &[String]| -> Result<(), SqlSurfaceError> {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")?;
        for sql in statements {
            core.execute_sql_in_txn(&caller, &mut session, &mut txn, sql)?;
        }
        Ok(())
    };
    in_txn(&[declare(50), "FETCH 50 FROM c".to_string()]).expect("at the limit");
    let err = in_txn(&[declare(51)]).expect_err("declare above the limit");
    assert_eq!(err.wire_code(), "22000");
    let err =
        in_txn(&[declare(50), "FETCH 51 FROM c".to_string()]).expect_err("fetch above the limit");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn raised_limit_accepts_large_limits_and_knn_does_not_fail() {
    let path = unique_db_path("result-limits-raised");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, Some(MAX_RESULT_ROWS_HARD_CAP));
    assert_boundary(&core, MAX_RESULT_ROWS_HARD_CAP, scan);
    assert_boundary(&core, MAX_RESULT_ROWS_HARD_CAP, knn);
    assert_boundary(&core, MAX_RESULT_ROWS_HARD_CAP, hybrid);
    assert_boundary(&core, MAX_RESULT_ROWS_HARD_CAP, precision);
    // 受理した大きな LIMIT でも行数は実データの 5 件で打ち切られる。
    let SqlOutcome::Query(result) = run(&core, &knn(20_000)).expect("knn") else {
        panic!("expected query outcome");
    };
    assert_eq!(result.rows.len(), 5);
}

#[test]
fn raised_limit_with_hnsw_engine_does_not_fail_above_max_ef() {
    let path = unique_db_path("result-limits-hnsw");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // HNSW 索引は可視行 1,024 件以上で有効になる（`sql::hnsw_cache::MIN_INDEXED_ROWS`）。
    seed(&storage, 1_100);
    let kind = engine::search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
        .expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind)
        .with_result_limits(ResultLimits::new(MAX_RESULT_ROWS_HARD_CAP));
    // k > MAX_EF（10,000）は索引探索を使わず総当たりへ縮退し、可視全件を返す。
    for sql in [knn(10_001), knn(MAX_RESULT_ROWS_HARD_CAP), hybrid(10_001)] {
        let SqlOutcome::Query(result) = run(&core, &sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        else {
            panic!("expected query outcome");
        };
        assert_eq!(result.rows.len(), 1_100, "{sql}");
    }
}

#[test]
fn offset_range_is_not_linked_to_the_setting() {
    let path = unique_db_path("result-limits-offset");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, Some(MAX_RESULT_ROWS_HARD_CAP));
    ok(&core, "SELECT id FROM docs LIMIT 10 OFFSET 10000");
    assert_eq!(
        code(&core, "SELECT id FROM docs LIMIT 10 OFFSET 10001"),
        "22000"
    );
    let lowered = unique_db_path("result-limits-offset-lowered");
    let _guard2 = CleanupGuard(lowered.clone());
    let core = core_with(&lowered, Some(50));
    ok(&core, "SELECT id FROM docs LIMIT 10 OFFSET 10000");
}

#[test]
fn bound_plan_entry_rechecks_against_configured_limit() {
    let path = unique_db_path("result-limits-bound-entry");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, Some(50));
    let session = SessionState::default();
    // 既定の範囲（10,000）で束縛した計画（limit=51）を設定 50 の core へ渡しても拒否される。
    let err = core
        .execute_bound_scan_in_session(&ctx(), &session, "docs", |schema, udfs| {
            let validated = validate_sql("SELECT id FROM docs LIMIT 51", &DocsOnly)?;
            let Statement::Scan(validated) = validated else {
                panic!("expected Statement::Scan");
            };
            engine::sql::parser::bind_scan(&validated, schema, udfs)
        })
        .expect_err("limit above the configured range must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

/// 上げた設定でも評価後射影形ビューの外側 `LIMIT` が構文段の既定値で先に拒否されない。
#[test]
fn raised_limit_accepts_buffered_view_outer_limit_above_default() {
    let path = unique_db_path("result-limits-raised-view");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, Some(MAX_RESULT_ROWS_HARD_CAP));
    ok(&core, "CREATE VIEW v AS SELECT id FROM docs LIMIT 3");
    assert_boundary(&core, MAX_RESULT_ROWS_HARD_CAP, |n| {
        format!("SELECT id FROM v LIMIT {n}")
    });
}

/// ビュー本文の内側 `LIMIT` は外側の設定値と連動せず、既定値（10,000）で独立に検証される
/// （実行・Describe の両方）。
#[test]
fn lowered_limit_does_not_apply_to_view_body_limit() {
    let path = unique_db_path("result-limits-view-body");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, None);
    ok(&core, "CREATE VIEW v AS SELECT id FROM docs LIMIT 100");
    let core = core.with_result_limits(ResultLimits::new(50));
    ok(&core, "SELECT id FROM v LIMIT 10");
    let parsed = core.parse_sql("SELECT id FROM v LIMIT 10").expect("parse");
    let session = SessionState::default();
    core.describe_parsed_in_session(&session, &parsed)
        .expect("describe must not apply the outer setting to the view body");
}

/// 利用者が明示した `LIMIT 4294967295` は内部の「LIMIT なし」値と同一視されず、
/// 設定上限で検査される（評価後射影形ビュー外側。実行・Describe の両方）。
#[test]
fn explicit_u32_max_view_outer_limit_is_rejected() {
    let path = unique_db_path("result-limits-view-u32max");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, None);
    ok(&core, "CREATE VIEW v AS SELECT id FROM docs LIMIT 3");
    let core = core.with_result_limits(ResultLimits::new(50));
    assert_eq!(code(&core, "SELECT id FROM v LIMIT 4294967295"), "22000");
    let parsed = core
        .parse_sql("SELECT id FROM v LIMIT 4294967295")
        .expect("parse");
    let session = SessionState::default();
    let err = core
        .describe_parsed_in_session(&session, &parsed)
        .expect_err("describe must reject an explicit LIMIT above the setting");
    assert_eq!(err.wire_code(), "22000");
}

/// ビュー連鎖の内側 `LIMIT` は設定値を下げても既定値で独立に検証される。
#[test]
fn lowered_limit_does_not_apply_to_nested_view_body_limit() {
    let path = unique_db_path("result-limits-nested-view");
    let _guard = CleanupGuard(path.clone());
    let core = core_with(&path, None);
    ok(&core, "CREATE VIEW v1 AS SELECT id FROM docs LIMIT 100");
    ok(&core, "CREATE VIEW v2 AS SELECT id FROM v1 LIMIT 100");
    let core = core.with_result_limits(ResultLimits::new(50));
    ok(&core, "SELECT id FROM v2 LIMIT 10");
    let parsed = core.parse_sql("SELECT id FROM v2 LIMIT 10").expect("parse");
    let session = SessionState::default();
    core.describe_parsed_in_session(&session, &parsed)
        .expect("describe must not apply the setting to nested view bodies");
}
