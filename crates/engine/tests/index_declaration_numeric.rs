//! 数値列（`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION`）への `CREATE INDEX`
//! 宣言の受理（Issue #1413・TASK-206・INDEX-7・TABLE-13）の結合テスト。
//!
//! `tests/index_declaration_bytea.rs`（Issue #1362）と同じ構成で、宣言付きテーブル
//! （HNSW opt-in）でも宣言した数値列が索引経路（Issue #1359 のレーン A）を使うこと、
//! 宣言外の数値列は従来どおり縮退すること、`EXPLAIN`（検索・集計・`USING PLAN`）の
//! 索引名が他型と同じ規則で付くこと、`Private` 行がテナントを越えないことを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;
use engine::storage::Visibility;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。公開行の id は 1..=ROWS。
const ROWS: u64 = 1_100;
// テナント境界確認用 Private 行の id（`qty = 3` に一致する id を選ぶ）。
const PRIVATE_ID: u64 = 1_111;

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("total", ColumnType::BigInt, true),
            ColumnDef::new("ratio", ColumnType::Real, true),
            ColumnDef::new("score", ColumnType::Double, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id` から決まる数値列。`id % 7 == 0` は全て `NULL`。負値・0・`-0.0` を含む。
fn values_of(id: u64) -> (Option<i32>, Option<i64>, Option<f32>, Option<f64>) {
    if id.is_multiple_of(7) {
        return (None, None, None, None);
    }
    let qty = (id % 12) as i32 - 4;
    let ratio = if qty == 0 && id.is_multiple_of(2) {
        -0.0f32
    } else {
        qty as f32 * 0.1
    };
    (
        Some(qty),
        Some(i64::from(qty) * 1000),
        Some(ratio),
        Some(f64::from(qty) * 0.25),
    )
}

fn insert(storage: &Storage, id: u64, vis: Visibility) {
    let (qty, total, ratio, score) = values_of(id);
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx("tenant-a"),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 1.0]),
            Value::Text(if id.is_multiple_of(2) { "a" } else { "b" }.to_string()),
            Value::Text(format!("docs/{id}.md")),
            Value::Text(format!("body {id}")),
            qty.map(Value::Integer).unwrap_or(Value::Null),
            total.map(Value::BigInt).unwrap_or(Value::Null),
            ratio.map(Value::Real).unwrap_or(Value::Null),
            score.map(Value::Double).unwrap_or(Value::Null),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// `USING PLAN` 用の決定的スタブ（検索語・ヒントなし）。
struct StubLlmClient;

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#.to_string())
    }
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=ROWS {
        insert(&storage, id, Visibility::Public);
    }
    insert(&storage, PRIVATE_ID, Visibility::Private);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    (
        EngineCore::from_storage_with_engine(storage, kind)
            .with_query_planner(Box::new(StubLlmClient)),
        guard,
    )
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("DDL must succeed: {sql}: {e:?}"));
}

fn ids(core: &EngineCore, tenant: &str, where_clause: &str) -> Vec<u64> {
    let sql = format!(
        "SELECT id FROM docs WHERE {where_clause} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2000"
    );
    let result = core
        .execute_sql(&ctx(tenant), &sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn explain_sql(core: &EngineCore, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("EXPLAIN must succeed: {sql}: {e:?}"))
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Text(s)) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

fn explain(core: &EngineCore, where_clause: &str) -> Vec<String> {
    explain_sql(
        core,
        &format!(
            "EXPLAIN SELECT id FROM docs WHERE {where_clause} \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
    )
}

fn scalar_plan(lines: &[String]) -> String {
    lines
        .iter()
        .find(|l| l.starts_with("scalar_plan: "))
        .cloned()
        .unwrap_or_else(|| panic!("no scalar_plan line: {lines:?}"))
}

/// 全走査オラクル（公開行のみ。`pred` に列値 `f64` を渡し、`NULL` は除外）。
fn oracle(col: &str, pred: impl Fn(f64) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| {
            let (qty, total, ratio, score) = values_of(*id);
            let v = match col {
                "qty" => qty.map(f64::from),
                "total" => total.map(|t| t as f64),
                "ratio" => ratio.map(f64::from),
                _ => score,
            };
            v.is_some_and(&pred)
        })
        .collect()
}

/// 宣言した数値 4 列の述語は宣言付きテーブルでも索引経路を使い、全走査オラクルと
/// 一致する。
#[test]
fn declared_numeric_columns_use_index_and_match_oracle() {
    let (core, _guard) = open_core("idx-decl-numeric-use");
    for col in ["qty", "total", "ratio", "score"] {
        ddl(&core, &format!("CREATE INDEX idx_{col} ON docs ({col})"));
    }
    let cases: Vec<(&str, Vec<u64>)> = vec![
        ("qty = 3", oracle("qty", |v| v == 3.0)),
        ("qty > 3", oracle("qty", |v| v > 3.0)),
        (
            "qty BETWEEN 0 AND 2",
            oracle("qty", |v| (0.0..=2.0).contains(&v)),
        ),
        ("total = 3000", oracle("total", |v| v == 3000.0)),
        ("total >= 5000", oracle("total", |v| v >= 5000.0)),
        (
            "total BETWEEN 0 AND 2000",
            oracle("total", |v| (0.0..=2000.0).contains(&v)),
        ),
        ("ratio > 0.25", oracle("ratio", |v| v > 0.25)),
        (
            "ratio BETWEEN 0 AND 0.15",
            oracle("ratio", |v| (0.0..=0.15).contains(&v)),
        ),
        ("score = 0.75", oracle("score", |v| v == 0.75)),
        ("score > 1.0", oracle("score", |v| v > 1.0)),
        (
            "score BETWEEN 0 AND 0.5",
            oracle("score", |v| (0.0..=0.5).contains(&v)),
        ),
    ];
    for (clause, expected) in cases {
        let _ = ids(&core, "tenant-a", clause);
        let before = core.scalar_index_cache_stats().index_scans;
        let got = ids(&core, "tenant-a", clause);
        let after = core.scalar_index_cache_stats();
        // tenant-a は Private 行も見えるため、それを除いて比較する。
        let got: Vec<u64> = got.into_iter().filter(|id| *id != PRIVATE_ID).collect();
        assert_eq!(got, expected, "oracle mismatch: {clause}");
        assert!(after.index_scans > before, "index not consumed: {clause}");
    }
}

/// 補集合: 数値列を宣言していなければ宣言外として縮退する（ゲートを緩めない）。
#[test]
fn undeclared_numeric_column_falls_back_to_plain_scan() {
    let (core, _guard) = open_core("idx-decl-numeric-undeclared");
    ddl(&core, "CREATE INDEX idx_kind ON docs (kind)");
    for (col, clause, pred) in [
        ("qty", "qty > 3", (|v: f64| v > 3.0) as fn(f64) -> bool),
        ("total", "total >= 5000", |v| v >= 5000.0),
        ("ratio", "ratio > 0.25", |v| v > 0.25),
        ("score", "score > 1.0", |v| v > 1.0),
    ] {
        let _ = ids(&core, "tenant-a", clause);
        let before = core.scalar_index_cache_stats().plain_scan_fallbacks;
        let got = ids(&core, "tenant-a", clause);
        let after = core.scalar_index_cache_stats().plain_scan_fallbacks;
        assert!(after > before, "undeclared {col} must fall back");
        let got: Vec<u64> = got.into_iter().filter(|id| *id != PRIVATE_ID).collect();
        assert_eq!(got, oracle(col, pred), "{clause}");
    }
}

/// RLS: 他テナントの `Private` 行は索引経路でも見えない。
#[test]
fn private_rows_do_not_leak_through_declared_numeric_index() {
    let (core, _guard) = open_core("idx-decl-numeric-rls");
    ddl(&core, "CREATE INDEX idx_qty ON docs (qty)");
    let clause = "qty = 3";
    let _ = ids(&core, "tenant-a", clause);
    let owner = ids(&core, "tenant-a", clause);
    assert!(owner.contains(&PRIVATE_ID));
    let _ = ids(&core, "tenant-b", clause);
    let other = ids(&core, "tenant-b", clause);
    assert!(!other.contains(&PRIVATE_ID), "private row leaked");
    assert_eq!(other, oracle("qty", |v| v == 3.0));
}

/// `EXPLAIN` の索引名は数値列でも他型と同じ規則で付く（検索・集計・`USING PLAN`）。
#[test]
fn explain_index_names_follow_same_rule_as_other_types() {
    let (core, _guard) = open_core("idx-decl-numeric-explain");
    ddl(&core, "CREATE INDEX idx_qty ON docs (qty)");
    ddl(&core, "CREATE INDEX idx_kind ON docs (kind)");

    assert_eq!(
        scalar_plan(&explain(&core, "qty > 3")),
        "scalar_plan: index_typed_range index=idx_qty"
    );
    assert_eq!(
        scalar_plan(&explain(&core, "kind = 'a' AND qty > 3")),
        "scalar_plan: index_conjunction index=idx_kind,idx_qty"
    );
    // `id` 述語は索引名の対象外（数値列だけが名前になる）。
    let with_id = scalar_plan(&explain(&core, "id > 5 AND qty > 3"));
    assert!(with_id.contains("index=idx_qty"), "{with_id}");
    assert!(!with_id.contains("idx_kind"), "{with_id}");
    // 未宣言の数値列は plain_scan（索引名なし）。
    assert_eq!(
        scalar_plan(&explain(&core, "total > 3")),
        "scalar_plan: plain_scan"
    );
    // 数値の IN は対象外のまま。
    assert_eq!(
        scalar_plan(&explain(&core, "qty IN (1, 2)")),
        "scalar_plan: plain_scan"
    );

    // 集計 EXPLAIN。
    let agg = explain_sql(&core, "EXPLAIN SELECT COUNT(*) FROM docs WHERE qty > 3");
    assert_eq!(
        scalar_plan(&agg),
        "scalar_plan: index_typed_range index=idx_qty"
    );

    // USING PLAN の EXPLAIN。
    let plan = explain_sql(
        &core,
        "EXPLAIN SELECT id FROM docs WHERE qty > 3 USING PLAN('find content') LIMIT 5",
    );
    assert_eq!(
        scalar_plan(&plan),
        "scalar_plan: index_typed_range index=idx_qty"
    );
}
