//! 数値列（`INTEGER`／`BIGINT`／`REAL`／`DOUBLE`）の等価・範囲比較が二次索引経路
//! （`OrderedColumnIndex::I64`／`F64Sortable`）を使うことの SQL 表層結合テスト
//! （Issue #1359。ポインタ: TABLE-13・INDEX-5。`IN`／`BETWEEN` の脱糖は
//! SQL-24・TASK-208）。
//!
//! 期待値は Rust 側で f64 比較から導出する全走査オラクルに加え、同じクエリへ
//! 索引非対応の述語（`id + 0 > 0`）を足して全走査へ強制した結果とも比較し、
//! 索引経路と全走査の結果が完全一致することを固定する。RLS（他テナントの行）・
//! `2^53` 超過の `BIGINT`（列が索引から落ち全走査と同じ 22003 になる fail-closed
//! の連鎖）・索引宣言（`Declared` では未宣言の数値列は構築されず `plain_scan`）も扱う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ROWS: u64 = 60;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
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

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn kind_of(id: u64) -> &'static str {
    if id % 2 == 1 {
        "a"
    } else {
        "b"
    }
}

/// `id` から決まる行の数値列。`id % 7 == 0` は全数値列が `NULL`。`qty` は
/// 負値・0 を含み、`ratio` は `0.1` 系と `-0.0`、`score` は小数を含む。
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

fn insert_row(
    storage: &Storage,
    c: &PolicyContext,
    id: u64,
    total_override: Option<i64>,
    visibility: Visibility,
    label: &str,
) {
    let (qty, total, ratio, score) = values_of(id);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        c,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(kind_of(id).to_string()),
            qty.map(Value::Integer).unwrap_or(Value::Null),
            total_override
                .or(total)
                .map(Value::BigInt)
                .unwrap_or(Value::Null),
            ratio.map(Value::Real).unwrap_or(Value::Null),
            score.map(Value::Double).unwrap_or(Value::Null),
        ],
        &op_id(label),
    )
    .expect("insert row");
}

fn seed(storage: &Storage, tenant: &str) {
    let c = ctx(tenant);
    for id in 1..=ROWS {
        insert_row(
            storage,
            &c,
            id,
            None,
            Visibility::Public,
            &format!("seed-{id}"),
        );
    }
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "tenant-a");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn ids(result: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

/// 列値を f64 へ広げた値（評価側の `numeric_scalar_from_ref` と同じ変換）。
fn column_f64(col: &str, id: u64) -> Option<f64> {
    let (qty, total, ratio, score) = values_of(id);
    match col {
        "qty" => qty.map(f64::from),
        "total" => total.map(|v| v as f64),
        "ratio" => ratio.map(f64::from),
        "score" => score,
        other => panic!("unknown column {other}"),
    }
}

fn oracle(col: &str, pred: impl Fn(f64) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| column_f64(col, *id).is_some_and(&pred))
        .collect()
}

/// 述語 `where_clause` を DISTANCE 経路（信頼マスク）・非 DISTANCE 経路・
/// 索引無効化（`id + 0 > 0` 追加）の 3 通りで実行し、期待 id 集合と一致する
/// ことを固定する。索引消費は選択度閾値（1/2 以下）の述語に限り検証する。
fn assert_indexed(core: &EngineCore, where_clause: &str, expected: &[u64]) {
    let vec_sql = format!(
        "SELECT id FROM {TABLE} WHERE {where_clause} \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100"
    );
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, &vec_sql);
    let hot = run(core, &vec_sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(ids(&cold), ids(&hot), "cold/hot mismatch: {where_clause}");
    assert_eq!(ids(&hot), expected, "oracle mismatch: {where_clause}");
    if expected.len() * 2 <= ROWS as usize {
        assert!(after > before, "index not consumed: {where_clause}");
    }
    // 索引を無効にした全走査（算術式の述語は索引非対応）と一致する。
    let scan_sql = format!(
        "SELECT id FROM {TABLE} WHERE {where_clause} AND id + 0 > 0 \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100"
    );
    assert_eq!(
        ids(&run(core, &scan_sql)),
        expected,
        "full scan mismatch: {where_clause}"
    );
    // 非 DISTANCE 経路。
    let plain_sql = format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100");
    assert_eq!(
        ids(&run(core, &plain_sql)),
        expected,
        "plain select mismatch: {where_clause}"
    );
    // COUNT(*)。
    let count_sql = format!("SELECT COUNT(*) AS n FROM {TABLE} WHERE {where_clause}");
    let counted = run(core, &count_sql);
    assert_eq!(
        counted.rows.first().map(|r| r.cells.clone()),
        Some(vec![Cell::Integer(expected.len() as u64)]),
        "COUNT(*) mismatch: {where_clause}"
    );
}

#[test]
fn all_four_types_all_operators_match_oracle() {
    let (core, _guard) = open_core("scalar-index-numeric-ops");
    // (列, SQL リテラル, f64 リテラル)。`ratio` の 0.1 は REAL を広げた値と
    // 比較される（`0.1` リテラルは f64 の 0.1 であり REAL の広げ値とは異なる）。
    let probes: [(&str, &str, f64); 19] = [
        ("qty", "3", 3.0),
        ("qty", "0", 0.0),
        ("qty", "2.5", 2.5),
        ("qty", "100", 100.0),
        ("total", "3000", 3000.0),
        ("total", "2999.5", 2999.5),
        ("total", "0", 0.0),
        ("ratio", "0.3", 0.3),
        ("ratio", "0", 0.0),
        ("ratio", "0.1", 0.1),
        ("score", "0.75", 0.75),
        ("score", "0.5", 0.5),
        ("score", "0", 0.0),
        ("score", "1000", 1000.0),
        // Issue #1438: 大きな数値リテラル（2^53 超・f32 範囲外）でも索引経路と全走査が一致する。
        ("ratio", "1e21", 1e21),
        ("ratio", "1e39", 1e39),
        ("score", "1e21", 1e21),
        ("score", "-1e300", -1e300),
        ("score", "9007199254740993", 9007199254740992.0),
    ];
    for (col, lit, l) in probes {
        assert_indexed(&core, &format!("{col} = {lit}"), &oracle(col, |v| v == l));
        assert_indexed(&core, &format!("{col} < {lit}"), &oracle(col, |v| v < l));
        assert_indexed(&core, &format!("{col} <= {lit}"), &oracle(col, |v| v <= l));
        assert_indexed(&core, &format!("{col} > {lit}"), &oracle(col, |v| v > l));
        assert_indexed(&core, &format!("{col} >= {lit}"), &oracle(col, |v| v >= l));
        // リテラルが左辺（`5 < n` は `n > 5`）。
        assert_indexed(&core, &format!("{lit} < {col}"), &oracle(col, |v| v > l));
        assert_indexed(&core, &format!("{lit} >= {col}"), &oracle(col, |v| v <= l));
    }
}

#[test]
fn between_and_not_use_index_and_match_oracle() {
    let (core, _guard) = open_core("scalar-index-numeric-between");
    assert_indexed(
        &core,
        "qty BETWEEN 2 AND 4",
        &oracle("qty", |v| (2.0..=4.0).contains(&v)),
    );
    assert_indexed(&core, "qty BETWEEN 4 AND 2", &[]);
    assert_indexed(
        &core,
        "score BETWEEN 0.25 AND 0.5",
        &oracle("score", |v| (0.25..=0.5).contains(&v)),
    );
    assert_indexed(&core, "NOT (qty > 5)", &oracle("qty", |v| v <= 5.0));
}

#[test]
fn conjunction_with_text_equality_and_id_range_intersects() {
    let (core, _guard) = open_core("scalar-index-numeric-conjunction");
    let expected: Vec<u64> = oracle("qty", |v| v >= 3.0)
        .into_iter()
        .filter(|id| kind_of(*id) == "a")
        .collect();
    assert_indexed(&core, "kind = 'a' AND qty >= 3", &expected);
    let expected: Vec<u64> = oracle("total", |v| v > 4000.0)
        .into_iter()
        .filter(|id| *id > 20)
        .collect();
    assert_indexed(&core, "id > 20 AND total > 4000", &expected);
    let expected: Vec<u64> = oracle("qty", |v| v >= 0.0)
        .into_iter()
        .filter(|id| oracle("score", |s| s < 1.0).contains(id))
        .collect();
    assert_indexed(&core, "qty >= 0 AND score < 1", &expected);
}

#[test]
fn null_rows_never_match() {
    let (core, _guard) = open_core("scalar-index-numeric-null");
    // id % 7 == 0 は全数値列が NULL。どの述語にも現れない。
    assert_indexed(&core, "qty < 100", &oracle("qty", |_| true));
    assert_indexed(&core, "ratio <= 100", &oracle("ratio", |_| true));
}

#[test]
fn other_tenant_rows_never_leak_through_index() {
    let path = unique_db_path("scalar-index-numeric-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "tenant-a");
    // tenant-b の private 行は tenant-a の述語に一致する値を持つが見えない。
    insert_row(
        &storage,
        &ctx("tenant-b"),
        1000,
        Some(5000),
        Visibility::Private,
        "seed-b-1000",
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let probe = ctx("tenant-a");
    let core_ref = &core;
    let hit = core_ref
        .execute_sql(
            &probe,
            &format!("SELECT id FROM {TABLE} WHERE total = 5000 LIMIT 100"),
        )
        .expect("query");
    assert!(
        hit.rows.iter().all(|r| r.id != 1000),
        "other tenant row leaked"
    );
    assert_indexed(&core, "total >= 5000", &oracle("total", |v| v >= 5000.0));
}

#[test]
fn generation_bump_rebuilds_numeric_index() {
    let (core, _guard) = open_core("scalar-index-numeric-generation");
    assert_indexed(&core, "qty = 50", &[]);
    let builds = core.scalar_index_cache_stats().builds;
    core.execute_insert_sql(
        &ctx("tenant-a"),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, kind, qty) \
             VALUES (500, '[1.0,0.0]', 'a', 50) USING OPERATION_ID 'seed-500'"
        ),
    )
    .expect("insert");
    let got = run(
        &core,
        &format!(
            "SELECT id FROM {TABLE} WHERE qty = 50 \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
    );
    assert_eq!(ids(&got), vec![500]);
    assert!(core.scalar_index_cache_stats().builds > builds);
}

/// `2^53` 超過の `BIGINT` を含む列は索引から落ち、全走査と同じ 22003 になる
/// （索引経路が行を評価せず誤って結果を返す fail-open にならない）。
#[test]
fn bigint_beyond_exact_range_falls_back_and_fails_closed() {
    let path = unique_db_path("scalar-index-numeric-big");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "tenant-a");
    insert_row(
        &storage,
        &ctx("tenant-a"),
        900,
        Some((1i64 << 53) + 1),
        Visibility::Public,
        "seed-900",
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        format!(
            "SELECT id FROM {TABLE} WHERE total > 5000 \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
        format!("SELECT COUNT(*) FROM {TABLE} WHERE total > 5000"),
    ] {
        let err = core
            .execute_sql(&ctx("tenant-a"), &sql)
            .expect_err("inexact BIGINT must fail closed");
        assert_eq!(err.wire_code(), "22003", "{sql}");
    }
    // 他の数値列は影響を受けず索引経路のまま結果が一致する。
    let got = run(
        &core,
        &format!(
            "SELECT id FROM {TABLE} WHERE qty >= 5 \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100"
        ),
    );
    let mut expected = oracle("qty", |v| v >= 5.0);
    // id 900 は qty が NULL ではなく values_of(900) 由来の値を持つ。
    if values_of(900).0.is_some_and(|q| q >= 5) {
        expected.push(900);
    }
    assert_eq!(ids(&got), expected);
}

fn explain(core: &EngineCore, session: &mut SessionState, sql: &str) -> Vec<String> {
    match core
        .execute_sql_in_session(&ctx("tenant-a"), session, sql)
        .expect("EXPLAIN must succeed")
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

fn has_line(lines: &[String], want: &str) -> bool {
    lines.iter().any(|l| l == want)
}

#[test]
fn explain_reports_index_for_numeric_predicates() {
    let (core, _guard) = open_core("scalar-index-numeric-explain");
    let mut session = SessionState::default();
    let q = |w: &str| {
        format!(
            "EXPLAIN SELECT id FROM {TABLE} WHERE {w} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        )
    };
    assert!(has_line(
        &explain(&core, &mut session, &q("qty > 3")),
        "scalar_plan: index_typed_range"
    ));
    assert!(has_line(
        &explain(&core, &mut session, &q("score = 0.5")),
        "scalar_plan: index_typed_range"
    ));
    assert!(has_line(
        &explain(&core, &mut session, &q("qty BETWEEN 1 AND 3")),
        "scalar_plan: index_conjunction"
    ));
    assert!(has_line(
        &explain(&core, &mut session, &q("kind = 'a' AND qty > 3")),
        "scalar_plan: index_conjunction"
    ));
    // OR 群（`IN` の脱糖）・算術式は索引非対応。
    assert!(has_line(
        &explain(&core, &mut session, &q("qty IN (1, 2)")),
        "scalar_plan: plain_scan"
    ));
    assert!(has_line(
        &explain(&core, &mut session, &q("qty + 1 > 5")),
        "scalar_plan: plain_scan"
    ));
}

/// 索引宣言（`Declared`）の下では、宣言していない数値列は構築されないため、
/// `EXPLAIN` も実行時も全走査になり、結果は全走査と一致する。
#[test]
fn declared_scalar_index_target_excludes_numeric_columns() {
    let path = unique_db_path("scalar-index-numeric-declared");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "tenant-a");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare scalar index on kind only");

    let mut plan_line = |w: &str| {
        let lines = explain(
            &core,
            &mut session,
            &format!(
                "EXPLAIN SELECT id FROM {TABLE} WHERE {w} \
                 ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
            ),
        );
        lines
            .iter()
            .find_map(|l| l.strip_prefix("scalar_plan: ").map(str::to_string))
            .expect("scalar_plan line")
    };
    assert_eq!(plan_line("qty > 3"), "plain_scan");
    assert_eq!(plan_line("kind = 'a' AND qty > 3"), "plain_scan");

    let expected: Vec<u64> = oracle("qty", |v| v > 3.0)
        .into_iter()
        .filter(|id| kind_of(*id) == "a")
        .collect();
    let got = run(
        &core,
        &format!(
            "SELECT id FROM {TABLE} WHERE kind = 'a' AND qty > 3 \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100"
        ),
    );
    assert_eq!(ids(&got), expected);
}
