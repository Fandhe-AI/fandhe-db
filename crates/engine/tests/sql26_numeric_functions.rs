//! `engine::core::EngineCore::execute_sql_in_session` の結合テスト（Issue #920、
//! 対象ビヘイビア: SQL-26。ポインタ: `docs/spec/05-tasks.md` TASK-210・
//! `docs/spec/04-behavior/sql-surface.md` SQL-26）。
//!
//! `tests/sql_udf_call.rs` と同じ流儀（`unique_db_path` / `CleanupGuard`、実
//! `Storage`＋`CpuScalarProvider`、独立オラクル）で、数値スカラー関数群
//! （`ABS`/`ROUND`/`FLOOR`/`CEIL`/`CEILING`/`MOD`/`POWER`/`SQRT`）を結果列・
//! `WHERE` の両位置から呼び出せること、エラーコードの決定性、予約名の衝突拒否を
//! 検証する。日時スカラー関数群（`EXTRACT`/`date_part`/`date_trunc`・日付算術）は
//! 本 Issue の対象外（実装ノート参照。後続課題）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-sql26-numeric-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `docs` テーブル（`embedding VECTOR(3)`）を持つ `EngineCore` を新設し、決定的な
/// 小規模コーパスを投入する（`tests/sql_udf_call.rs` と同一の投入手順）。
fn new_core_with_docs() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("docs");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let corpus: Vec<(u64, [f32; 3])> = vec![
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    for (id, emb) in &corpus {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_id = format!("test-op-{id}");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&op_id)
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, guard)
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn float_cell(row: &engine::sql::exec::ResultRow, idx: usize) -> f64 {
    match row.cells.get(idx) {
        Some(Cell::Float(v)) => *v,
        other => panic!("expected Cell::Float at index {idx}, got {other:?}"),
    }
}

// --- 正常系: 結果列位置 ------------------------------------------------------------

#[test]
fn numeric_builtin_functions_in_result_columns_match_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, abs(id - 2), round(vec_norm(embedding)), mod(id, 2), \
             power(2, id), sqrt(id) FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with numeric builtin result columns should succeed");
    let result = expect_query(outcome);

    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    for row in &result.rows {
        let (id, emb) = *corpus
            .iter()
            .find(|(id, _)| *id == row.id)
            .expect("known id");
        let norm = (emb[0] as f64 * emb[0] as f64
            + emb[1] as f64 * emb[1] as f64
            + emb[2] as f64 * emb[2] as f64)
            .sqrt();
        assert_eq!(float_cell(row, 1), (id as f64 - 2.0).abs(), "abs(id-2)");
        assert_eq!(float_cell(row, 2), norm.round(), "round(vec_norm)");
        assert_eq!(float_cell(row, 3), (id as f64) % 2.0, "mod(id,2)");
        assert_eq!(float_cell(row, 4), 2f64.powf(id as f64), "power(2,id)");
        assert_eq!(float_cell(row, 5), (id as f64).sqrt(), "sqrt(id)");
    }
}

#[test]
fn round_with_two_arguments_rounds_to_requested_decimal_places() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT round(vec_norm(embedding), 2) FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect("SELECT with round/2 should succeed");
    let result = expect_query(outcome);
    // corpus id=1: norm = 5.0 exactly, round(5.0, 2) = 5.0
    assert_eq!(float_cell(&result.rows[0], 0), 5.0);
}

#[test]
fn floor_and_ceiling_alias_both_resolve_to_the_same_semantics() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT floor(vec_norm(embedding)), ceil(vec_norm(embedding)), \
             ceiling(vec_norm(embedding)) FROM docs \
             ORDER BY embedding <=> '[0.0,0.0,1.0]' LIMIT 1",
        )
        .expect("SELECT with floor/ceil/ceiling should succeed");
    let result = expect_query(outcome);
    // corpus id=2: norm = 1.0 exactly.
    assert_eq!(float_cell(&result.rows[0], 0), 1.0);
    assert_eq!(float_cell(&result.rows[0], 1), 1.0);
    assert_eq!(float_cell(&result.rows[0], 2), 1.0);
}

// --- 正常系: WHERE 位置 -------------------------------------------------------------

#[test]
fn numeric_builtin_in_where_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE sqrt(vec_norm(embedding)) > 1.5 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with numeric builtin WHERE predicate should succeed");
    let result = expect_query(outcome);

    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    let expected_ids: Vec<u64> = corpus
        .iter()
        .filter(|(_, emb)| {
            let norm = (emb[0] as f64 * emb[0] as f64
                + emb[1] as f64 * emb[1] as f64
                + emb[2] as f64 * emb[2] as f64)
                .sqrt();
            norm.sqrt() > 1.5
        })
        .map(|(id, _)| *id)
        .collect();
    let got_ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(got_ids, expected_ids);
}

// --- UDF 本体からの利用 --------------------------------------------------------------

#[test]
fn udf_body_can_call_numeric_builtin_functions() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION rounded_norm(v) AS round(vec_norm(v))",
    )
    .expect("CREATE FUNCTION should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT rounded_norm(embedding) FROM docs \
             ORDER BY embedding <=> '[0.0,0.0,1.0]' LIMIT 1",
        )
        .expect("SELECT with UDF calling a numeric builtin should succeed");
    let result = expect_query(outcome);
    assert_eq!(float_cell(&result.rows[0], 0), 1.0);
}

// --- エラーコードの決定性 -------------------------------------------------------------

#[test]
fn power_overflow_is_rejected_with_22003() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT power(10, 400) FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("power(10, 400) must overflow");
    assert_eq!(err.wire_code(), "22003");
}

#[test]
fn round_second_argument_beyond_i32_range_is_rejected_with_22003() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT round(1, 3000000000) FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("round(1, 3000000000) must be rejected");
    assert_eq!(err.wire_code(), "22003");
}

#[test]
fn sqrt_of_negative_and_mod_by_zero_are_rejected_with_22000() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT sqrt(0 - 1) FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("sqrt(-1) must be rejected");
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT mod(1, 0) FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("mod(1, 0) must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn argument_type_mismatch_is_rejected_with_22000() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT abs(embedding) FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("abs(embedding) (Vector argument) must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn calling_a_non_deterministic_function_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT now() FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("now() must be rejected (non-deterministic function)");
    assert_eq!(err.wire_code(), "22000");
}

// --- 予約名の衝突 --------------------------------------------------------------------

#[test]
fn defining_a_udf_named_after_a_numeric_builtin_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    for name in [
        "abs", "round", "floor", "ceil", "ceiling", "mod", "power", "sqrt",
    ] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}(x) AS x"),
            )
            .expect_err(&format!(
                "CREATE FUNCTION {name} must be rejected as reserved"
            ));
        assert_eq!(err.wire_code(), "22000", "function name {name}");
    }
}

#[test]
fn defining_a_udf_named_after_a_non_deterministic_function_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION now(x) AS x")
        .expect_err("CREATE FUNCTION now must be rejected as reserved");
    assert_eq!(err.wire_code(), "22000");
}
