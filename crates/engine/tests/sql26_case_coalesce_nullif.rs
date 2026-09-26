//! `CASE` 式と `COALESCE`／`NULLIF` の結合テスト（Issue #921。対象ビヘイビア:
//! `docs/spec/04-behavior/sql-surface.md` SQL-26。ポインタ: `docs/spec/05-tasks.md`
//! TASK-210。関連: SQL-9・TASK-79）。
//!
//! `tests/sql_udf_call.rs`・`tests/sql_aggregate.rs` と同じ流儀
//! （`EngineCore::execute_sql_in_session`、実 `Storage`＋`CpuScalarProvider`、
//! 独立オラクル）で、検索形 `CASE`・`COALESCE`・`NULLIF` の SELECT 式項目・
//! `WHERE` 式述語・集計引数での評価、RLS（不可視行では式が一切評価されない
//! こと）、拒否経路の `wire_code` 決定性を検証する。共有成果物
//! （`sql/function.rs`・`tests/sql26_scalar_functions.rs`）には触れない
//! （並行実装中の Issue #919／#920 との競合回避）。

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
        "vector-db-engine-sql26-case-coalesce-nullif-{label}-{}-{seq}.redb",
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

/// `docs` テーブル（`embedding VECTOR(3)`）に 3 行（`id` 1〜3）を投入する
/// （`tests/sql_udf_call.rs::new_core_with_docs` と同一コーパス）。
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
        (1, [3.0, 4.0, 0.0]), // norm = 5
        (2, [0.0, 0.0, 1.0]), // norm = 1
        (3, [1.0, 1.0, 1.0]), // norm = sqrt(3)
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

/// [`new_core_with_docs`] と同じスキーマに、id=1 の行だけを private
/// （呼び出し元テナントには不可視）として投入する（RLS 検証用）。
fn new_core_with_one_private_row() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("rls");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Private,
        // 0 ベクトル: `vec_div`/`vec_norm` が評価されれば 0 除算で `22000` に
        // なるが、可視行が 0 件ならエラーにならない契約（既存 #353）を検証する。
        &[Value::Vector(vec![0.0, 0.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op-private")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, guard)
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn cell(row: &engine::sql::exec::ResultRow, idx: usize) -> &Cell {
    row.cells
        .get(idx)
        .unwrap_or_else(|| panic!("missing cell at index {idx}"))
}

fn ctx() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("valid tenant")
}

// --- SELECT 式項目 -----------------------------------------------------------

#[test]
fn case_select_item_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id, CASE WHEN id > 1 THEN vec_norm(embedding) ELSE 0 END AS c FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with CASE result column should succeed");
    let result = expect_query(outcome);
    assert_eq!(
        result.columns[1],
        engine::sql::exec::ColumnMeta::Computed {
            name: "c".to_string()
        }
    );

    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    for row in &result.rows {
        let (id, emb) = corpus.iter().find(|(id, _)| *id == row.id).unwrap();
        let expected: f64 = if *id > 1 {
            (emb[0] as f64 * emb[0] as f64
                + emb[1] as f64 * emb[1] as f64
                + emb[2] as f64 * emb[2] as f64)
                .sqrt()
        } else {
            0.0
        };
        match cell(row, 1) {
            Cell::Float(v) => assert!(
                (v - expected).abs() < 1e-6,
                "row {}: expected {expected}, got {v}",
                row.id
            ),
            other => panic!("expected Cell::Float, got {other:?}"),
        }
    }
}

#[test]
fn default_alias_for_case_coalesce_nullif_matches_postgresql_convention() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT CASE WHEN id > 1 THEN 1 ELSE 0 END, COALESCE(id, 0), NULLIF(id, 2) \
             FROM docs ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect("SELECT with default aliases should succeed");
    let result = expect_query(outcome);
    let names: Vec<String> = result
        .columns
        .iter()
        .map(|c| match c {
            engine::sql::exec::ColumnMeta::Computed { name } => name.clone(),
            other => panic!("expected Computed column, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["case", "coalesce", "nullif"]);
}

#[test]
fn nullif_returns_null_cell_when_operands_are_equal() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id, NULLIF(id, 2) AS n FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with NULLIF should succeed");
    let result = expect_query(outcome);
    for row in &result.rows {
        match (row.id, cell(row, 1)) {
            (2, Cell::Null) => {}
            (2, other) => panic!("expected Cell::Null for id=2, got {other:?}"),
            (_, Cell::Null) => panic!("row {} should not be NULL", row.id),
            (id, Cell::Float(v)) => assert!((v - id as f64).abs() < 1e-9),
            (id, other) => panic!("row {id}: unexpected cell {other:?}"),
        }
    }
}

#[test]
fn coalesce_of_nullif_falls_back_to_default_value() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id, COALESCE(NULLIF(id, 2), 0) AS v FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with COALESCE(NULLIF(...)) should succeed");
    let result = expect_query(outcome);
    for row in &result.rows {
        let expected = if row.id == 2 { 0.0 } else { row.id as f64 };
        match cell(row, 1) {
            Cell::Float(v) => assert!((v - expected).abs() < 1e-9),
            other => panic!("row {}: expected Cell::Float, got {other:?}", row.id),
        }
    }
}

// --- WHERE 式述語 -------------------------------------------------------------

#[test]
fn where_case_expr_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id FROM docs WHERE CASE WHEN id > 1 THEN 1 ELSE 0 END = 1 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with CASE WHERE predicate should succeed");
    let result = expect_query(outcome);
    let got_ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(got_ids, vec![3, 2]);
}

#[test]
fn where_nullif_excludes_null_rows_as_unknown() {
    // `NULLIF(id, 3) > 0` は id=3 の行で NULL（UNKNOWN）になり、非該当として
    // 除外される（対象ビヘイビア: SQL-26。PostgreSQL の 3 値論理と同じ扱い）。
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id FROM docs WHERE NULLIF(id, 3) > 0 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with NULLIF WHERE predicate should succeed");
    let result = expect_query(outcome);
    let got_ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(got_ids, vec![1, 2]);
}

// --- 集計 ----------------------------------------------------------------------

#[test]
fn sum_of_case_skips_rows_where_case_evaluates_to_null() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT SUM(CASE WHEN id > 1 THEN 1 END) AS s FROM docs",
        )
        .expect("SELECT SUM(CASE ...) should succeed");
    let result = expect_query(outcome);
    assert_eq!(result.rows.len(), 1);
    match cell(&result.rows[0], 0) {
        // id in {2,3} 満たす（NULL の id=1 はスキップされる）: 1 + 1 = 2。
        Cell::Float(v) => assert!((v - 2.0).abs() < 1e-9),
        other => panic!("expected Cell::Float, got {other:?}"),
    }
}

#[test]
fn count_of_nullif_counts_only_non_null_values() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT COUNT(NULLIF(id, 1)) AS c FROM docs",
        )
        .expect("SELECT COUNT(NULLIF(...)) should succeed");
    let result = expect_query(outcome);
    match cell(&result.rows[0], 0) {
        // id=1 は NULL になり数えられない: 3 行中 2 行のみ。
        Cell::Integer(v) => assert_eq!(*v, 2),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

#[test]
fn count_distinct_of_nullif_counts_only_non_null_distinct_values() {
    // Cursor Bugbot（PR #1101 レビュー指摘）: `COUNT(DISTINCT NULLIF(...))` の
    // ような、NULL を返しうるスカラー式を `COUNT(DISTINCT expr)` に渡すと、
    // `sql::aggregate` の distinct 観測経路が `ExprValue::Null` を「実装バグ」
    // 扱いして internal error（`accumulator_bug`）になっていた。非 distinct の
    // `COUNT(NULLIF(...))`（上の `count_of_nullif_counts_only_non_null_values`）
    // と同じ「NULL は対象から除外する」契約を distinct 側でも成立させる。
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT COUNT(DISTINCT NULLIF(id, 1)) AS c FROM docs",
        )
        .expect("SELECT COUNT(DISTINCT NULLIF(...)) should succeed");
    let result = expect_query(outcome);
    match cell(&result.rows[0], 0) {
        // id=1 は NULL になり除外される。残り distinct 値は {2, 3} の 2 件。
        Cell::Integer(v) => assert_eq!(*v, 2),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

// --- RLS（既存 #353 の契約: 可視行 0 件ならエラーにならない） -----------------

#[test]
fn invisible_row_with_division_by_zero_in_case_does_not_error() {
    let (core, _guard) = new_core_with_one_private_row();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id FROM docs WHERE CASE WHEN vec_norm(embedding) > 0 \
             THEN vec_sum(vec_div(embedding, vec_norm(embedding))) ELSE 0 END >= 0 \
             ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect(
            "query over an invisible row should not evaluate the expression and must not error",
        );
    let result = expect_query(outcome);
    assert!(result.rows.is_empty());
}

// --- 拒否経路（wire_code の決定性） --------------------------------------------

#[test]
fn simple_case_form_is_rejected_with_42601() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT CASE id WHEN 1 THEN 2 END FROM docs LIMIT 1",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn case_branch_type_mismatch_is_rejected_with_42804() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            // THEN 枝は Scalar（`1`）、ELSE 枝は Vector（`embedding`）で型が食い違う。
            "SELECT CASE WHEN id > 1 THEN 1 ELSE embedding END FROM docs LIMIT 1",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42804");
}

#[test]
fn coalesce_with_zero_arguments_is_rejected_with_42601() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx(), &mut session, "SELECT COALESCE() FROM docs LIMIT 1")
        .unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn nullif_with_vector_argument_is_rejected_with_42804() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT NULLIF(embedding, embedding) FROM docs LIMIT 1",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42804");
}

#[test]
fn case_with_only_null_branches_is_rejected_with_0a000() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT CASE WHEN id > 1 THEN NULL END FROM docs LIMIT 1",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn defining_a_udf_named_coalesce_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx(), &mut session, "CREATE FUNCTION coalesce(x) AS x")
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- 既存 NULL 構文の非回帰 -----------------------------------------------------

#[test]
fn existing_using_operation_id_null_literal_still_parses_unaffected() {
    // `Expr::Null`（本 Issue）は `sql::allowlist::Parser` の式文法にのみ影響し、
    // `USING OPERATION_ID` 句・INSERT の `VALUES` リテラルは独立した文法
    // （`Value` 型）のため非回帰であることを固定する。
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "UPDATE docs SET embedding = '[1.0,2.0,3.0]' WHERE id = 1 \
             USING OPERATION_ID NULL",
        )
        .unwrap_err();
    // 空文字列・NULL の USING OPERATION_ID は従来どおり `23502`（欠落扱い）。
    assert_eq!(err.wire_code(), "23502");
}
