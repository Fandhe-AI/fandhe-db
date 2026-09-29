//! `engine::core::EngineCore::execute_sql_in_session` の結合テスト（TASK-79、対象
//! ビヘイビア: SQL-9。ポインタ: `docs/spec/05-tasks.md` TASK-79・
//! `docs/spec/04-behavior/sql-surface.md` SQL-9・`docs/spec/04-behavior/rls.md`
//! RLS-8）。
//!
//! `tests/sql_search_mode.rs`・`tests/sql_evaluation_order.rs` と同じ流儀
//! （`unique_db_path` / `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、独立
//! オラクル）で、宣言的 UDF（`CREATE FUNCTION`）を結果列・`WHERE` の両位置から
//! 呼び出せること、RLS（不可視行では UDF が一切評価されないこと）、セッション分離、
//! 拒否経路の wire_code 決定性を検証する。

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
        "vector-db-engine-sql-udf-call-{label}-{}-{seq}.redb",
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
/// 小規模コーパスを投入する（`tests/sql_search_mode.rs` と同一の投入手順）。
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

fn independent_norm(v: [f32; 3]) -> f64 {
    (v[0] as f64 * v[0] as f64 + v[1] as f64 * v[1] as f64 + v[2] as f64 * v[2] as f64).sqrt()
}

// --- 結果列位置（SQL-9） -----------------------------------------------------------

#[test]
fn udf_call_in_result_column_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION norm_scale(v, s) AS s * vec_sum(vec_div(v, vec_norm(v)))",
    )
    .expect("CREATE FUNCTION should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, norm_scale(embedding, 2.0) AS score FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with UDF result column should succeed");
    let result = expect_query(outcome);

    assert_eq!(result.columns.len(), 2);
    assert_eq!(
        result.columns[1],
        engine::sql::exec::ColumnMeta::Computed {
            name: "score".to_string()
        }
    );

    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    for row in &result.rows {
        let (_, emb) = corpus
            .iter()
            .find(|(id, _)| *id == row.id)
            .expect("known id");
        let norm = independent_norm(*emb);
        let expected: f64 =
            2.0 * (emb[0] as f64 / norm + emb[1] as f64 / norm + emb[2] as f64 / norm);
        assert!(
            (float_cell(row, 1) - expected).abs() < 1e-6,
            "row {}: expected {expected}, got {}",
            row.id,
            float_cell(row, 1)
        );
    }
}

#[test]
fn udf_alias_defaults_to_function_name_when_as_is_omitted() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT vec_norm(embedding) FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect("SELECT with builtin-only result column should succeed");
    let result = expect_query(outcome);
    assert_eq!(
        result.columns[0],
        engine::sql::exec::ColumnMeta::Computed {
            name: "vec_norm".to_string()
        }
    );
    assert!((float_cell(&result.rows[0], 0) - 5.0).abs() < 1e-6);
}

// --- WHERE 位置（SQL-9） -----------------------------------------------------------

#[test]
fn udf_call_in_where_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE vec_norm(embedding) > 2.0 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with UDF WHERE predicate should succeed");
    let result = expect_query(outcome);

    // 独立オラクル: 可視行全件を norm > 2.0 で判定 → 距離順。
    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    let expected_ids: Vec<u64> = corpus
        .iter()
        .filter(|(_, emb)| independent_norm(*emb) > 2.0)
        .map(|(id, _)| *id)
        .collect();
    let got_ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(got_ids, expected_ids);
}

#[test]
fn udf_call_in_where_matches_prefilter_result_when_distance_runs_first() {
    // TASK-79（SQL-9）: `HINT ORDER(DISTANCE, SCALAR, RLS)` は WHERE の式述語を
    // 事後フィルタ（`sql::exec::execute_statement` の post-filter 分岐）で適用する
    // 経路。既定順（事前フィルタ、`udf_call_in_where_matches_independent_oracle`）と
    // 同じ述語・同じコーパスで、両経路が食い違わないことを固定する。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE vec_norm(embedding) > 2.0 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3 HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect("SELECT with UDF WHERE predicate (DISTANCE-first) should succeed");
    let result = expect_query(outcome);

    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    let expected_ids: Vec<u64> = corpus
        .iter()
        .filter(|(_, emb)| independent_norm(*emb) > 2.0)
        .map(|(id, _)| *id)
        .collect();
    let got_ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(got_ids, expected_ids);
}

#[test]
fn dividing_by_a_zero_norm_vector_is_fail_closed_when_distance_runs_first() {
    // TASK-79（SQL-9）: 事前フィルタ経路の `dividing_by_a_zero_norm_vector_is_fail_closed_with_22000`
    // と対になる、事後フィルタ経路（`HINT ORDER(DISTANCE, ...)`）の同値テスト。
    // 事前フィルタは `Err` を伝播してクエリ全体を失敗させる（`on_visible_row` 内の
    // `?`）。事後フィルタ側も同様に `?` で伝播し、範囲外スロットの `continue`
    // （データ不整合用）と評価エラーの `continue` を取り違えて黙って行を
    // 落とさないことを固定する。
    let path = unique_db_path("div-zero-distance-first");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
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
        &[Value::Vector(vec![0.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert zero vector");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE vec_sum(vec_div(embedding, vec_norm(embedding))) > 0.0 \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1 HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect_err("division by a zero norm must be fail-closed via the post-filter path too");
    assert_eq!(err.wire_code(), "22012");
}

#[test]
fn udf_call_in_both_result_column_and_where_in_a_single_statement() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, vec_norm(embedding) AS n FROM docs WHERE vec_norm(embedding) > 2.0 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with UDF in both positions should succeed");
    let result = expect_query(outcome);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].id, 1);
    assert!((float_cell(&result.rows[0], 1) - 5.0).abs() < 1e-6);
}

#[test]
fn existing_equality_and_visible_predicates_still_combine_with_and() {
    // 非回帰: 既存の等価条件・`visible()` 呼び出し形は SQL-9 の追加後も従来どおり
    // 動く（`Statement::Select`・`WherePredicate::{Equality, PredicateCall}` は
    // 変更していない）。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE visible() ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("existing visible() predicate form should still be accepted");
    let result = expect_query(outcome);
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn equality_predicate_and_expr_predicate_combine_in_the_same_where_clause() {
    // 非回帰かつ SQL-9 の新規経路: 既存の等価条件（`scalar_filters`）と新設の式述語
    // （`expr_filters`）が同一 `on_visible_row` 呼び出しの中で共に適用されることを
    // 固定する（構文が受理されるだけでなく、両方のフィルタが実際に効くこと）。
    let path = unique_db_path("mixed-predicates");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let corpus: [(u64, [f32; 3], &str); 3] = [
        (1, [3.0, 4.0, 0.0], "ja"), // norm=5, lang=ja → 両方通過
        (2, [3.0, 4.0, 0.0], "en"), // norm=5 だが lang 不一致 → 除外
        (3, [0.0, 0.0, 1.0], "ja"), // lang=ja だが norm=1 → 除外
    ];
    for (id, emb, lang) in corpus {
        let op_id = format!("test-op-{id}");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&op_id)
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE lang = 'ja' AND vec_norm(embedding) > 2.0 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("mixed equality + expr WHERE predicates should succeed");
    let result = expect_query(outcome);
    assert_eq!(
        result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1]
    );
}

// --- RLS（RLS-8: 不可視行では UDF が一切評価されない） -----------------------------

#[test]
fn udf_expression_never_evaluates_on_invisible_rows() {
    let path = unique_db_path("rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");

    // tenant-a の可視行。
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_a,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert tenant-a row");

    // tenant-b の Private 行（tenant-a からは不可視）。零ベクトルにして
    // `vec_div(embedding, vec_norm(embedding))` が評価されれば 0 除算で失敗する
    // ように仕込む。
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_b,
        2,
        Visibility::Private,
        &[Value::Vector(vec![0.0, 0.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert tenant-b row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();

    // tenant-a のクエリは、tenant-b の不可視行にだけ 0 除算を起こす式を含んでいても
    // 成功する（不可視行では UDF が一切評価されないため）。
    let outcome = core
        .execute_sql_in_session(
            &ctx_a,
            &mut session,
            "SELECT id FROM docs WHERE vec_sum(vec_div(embedding, vec_norm(embedding))) > 0.5 \
             ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
        )
        .expect("tenant-a query must not fail due to tenant-b's invisible zero vector");
    let result = expect_query(outcome);
    assert_eq!(
        result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1]
    );
}

// --- セッション分離 ------------------------------------------------------------------

#[test]
fn udf_defined_in_one_session_is_not_visible_from_another_session() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let mut session_a = SessionState::default();
    core.execute_sql_in_session(
        &ctx,
        &mut session_a,
        "CREATE FUNCTION only_in_a(v) AS vec_norm(v)",
    )
    .expect("CREATE FUNCTION should succeed in session A");

    let mut session_b = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session_b,
            "SELECT only_in_a(embedding) FROM docs ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect_err("session B must not see session A's UDF");
    assert_eq!(err.wire_code(), "42883");
}

#[test]
fn create_function_is_rejected_on_the_sessionless_entry_point() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .execute_sql(&ctx, "CREATE FUNCTION f(v) AS vec_norm(v)")
        .expect_err("CREATE FUNCTION must be rejected on execute_sql (no session)");
    assert_eq!(err.wire_code(), "42601");
}

// --- 拒否経路（wire_code の決定性） -------------------------------------------------

#[test]
fn unknown_function_call_is_rejected_with_42883() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let sql = "SELECT mystery(embedding) FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1";
    let first = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err("unknown function must be rejected")
        .wire_code();
    let second = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err("unknown function must be rejected deterministically")
        .wire_code();
    assert_eq!(first, "42883");
    assert_eq!(first, second);
}

#[test]
fn where_comparison_between_a_vector_and_a_scalar_is_rejected_with_42804() {
    // `<expr> <cmp> <expr>` の両辺は `Scalar` でなければならない（束縛段の型検査）。
    // `embedding`（`Vector`）を直接比較に使うと `22000` で拒否される。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE embedding > 2.0 ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("comparing a vector to a scalar must be rejected");
    assert_eq!(err.wire_code(), "42804");
}

#[test]
fn where_clause_without_a_comparison_operator_is_a_syntax_error() {
    // 単独の呼び出し式（比較演算子を伴わない）は許可リストの `WHERE` 文法
    // （`<expr> <cmp> <expr>`）に一致しないため、束縛段（`22000`）ではなく構造検証
    // 段階（`42601`）で拒否される。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE vec_norm(embedding) ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("a bare call expression without a comparison operator is a syntax error");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn text_column_reference_in_expression_is_rejected_with_42804() {
    let path = unique_db_path("text-col");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("label", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT vec_norm(label) FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
        )
        .expect_err("TEXT column reference in an expression must be rejected");
    assert_eq!(err.wire_code(), "42804");
}

#[test]
fn dividing_by_a_zero_norm_vector_is_fail_closed_with_22012() {
    let path = unique_db_path("div-zero");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
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
        &[Value::Vector(vec![0.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert zero vector");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT vec_div(embedding, vec_norm(embedding)) FROM docs \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
        )
        .expect_err("division by a zero norm must be fail-closed, not silently zeroed");
    assert_eq!(err.wire_code(), "22012");
}

/// Issue #353（式評価のステップ列コンパイル化）: `WHERE` 式述語が定数のみから
/// なる部分式（例: `1/0`）を含む場合、束縛時のステップ列コンパイル
/// （`sql::expr_program::ExprProgram::compile`）は defer-on-error（畳み込み時に
/// エラーになる部分式は畳み込まず実行時評価に委ねる）を守るため、可視行が
/// 1 行も評価されなければクエリ全体は成功する（既存契約「可視行が 1 行も
/// 評価されなければエラーは発生しない」を、定数畳み込みの導入で変えない）。
#[test]
fn constant_subexpression_error_is_deferred_and_does_not_fail_a_query_with_no_visible_rows() {
    let path = unique_db_path("defer-on-error-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    // 行を 1 件も投入しない: `WHERE 1/0 > 0` の定数部分式は実行時にのみ評価
    // されうるが、可視行が存在しないため評価自体が発生しない。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let result = expect_query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE 1/0 > 0 \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
        )
        .expect("a WHERE clause with an unreached constant-error subexpression must not fail a query with no visible rows"),
    );
    assert!(result.rows.is_empty());
}

/// 上記のちょうど対偶: 可視行が実際に存在する場合、`WHERE 1/0 > 0` の定数
/// 部分式は行評価のタイミングで評価され、`22000` として fail-closed に拒否
/// される（畳み込みが「エラーになりうる式を黙って成功扱いにする」方向へ
/// 縮退していないことの確認）。
#[test]
fn constant_subexpression_error_still_fails_a_query_with_a_visible_row() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE 1/0 > 0 \
             ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("a constant 0-division in WHERE must be fail-closed once a row is evaluated");
    assert_eq!(err.wire_code(), "22012");
}

#[test]
fn redefining_a_function_in_the_same_session_is_rejected_with_42723() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION f(v) AS vec_norm(v)")
        .expect("first definition should succeed");
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION f(v) AS vec_norm(v)")
        .expect_err("redefinition in the same session must be rejected");
    assert_eq!(err.wire_code(), "42723");
}

#[test]
fn function_body_referencing_an_undefined_name_is_rejected_with_42883() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE FUNCTION f(v) AS undefined_name(v)",
        )
        .expect_err("undefined reference in function body must be rejected");
    assert_eq!(err.wire_code(), "42883");
}

#[test]
fn defining_a_function_named_after_a_builtin_is_rejected_with_42723() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION vec_norm(v) AS v")
        .expect_err("a UDF name colliding with a built-in must be rejected");
    assert_eq!(err.wire_code(), "42723");
}

#[test]
fn too_many_function_parameters_is_rejected_with_54000() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let params: Vec<String> = (0..40).map(|i| format!("p{i}")).collect();
    let sql = format!("CREATE FUNCTION f({}) AS p0", params.join(", "));
    let err = core
        .execute_sql_in_session(&ctx, &mut session, &sql)
        .expect_err("exceeding the parameter count limit must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn order_by_may_not_reference_a_udf_call() {
    // 範囲外（本タスクのスコープ外・§8 参照）: `ORDER BY` の関数呼び出し形は
    // 引き続き `hybrid_rrf`/`HYBRID` のみを許可名として受理し、UDF・組み込み
    // 関数の呼び出しは通さない（`allowlist::is_allowed_order_by_function_name`
    // は SQL-9 で変更していない）。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs ORDER BY vec_norm(embedding) LIMIT 1",
        )
        .expect_err("UDF/builtin call in ORDER BY must remain unsupported");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn dollar_parameter_placeholder_in_where_remains_unsupported() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE vec_norm(embedding) > $1 ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err("$n parameter placeholders remain unsupported");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn chained_udf_parameter_doubling_is_rejected_before_expansion_blows_up() {
    // Review 指摘（High）の回帰テスト: `g0(v) = v + v` から
    // `g{n}(v) = g{n-1}(v + v)` まで複数の `CREATE FUNCTION` 文にまたいで連鎖させると、
    // 各定義の本体は構文的に小さい（`g{n-1}(v + v)` の数ノード）ため
    // `validate_closed_expr` の構文ノード数チェックは連鎖長にほぼ線形にしか消費されない。
    // 一方、呼び出し（束縛）時のパラメータ参照展開はクローンする既展開済み部分木の
    // サイズを課金していなかったため、展開後の `BoundExpr` サイズは連鎖長に対して
    // 指数的に膨張しうる（`v` の参照が 2 回あるたびに倍加）。修正後は
    // `bind_expr_in` の `Expr::Ident` 分岐でクローンする展開後ノード数も
    // `node_budget` へ課金するため、連鎖を伸ばして呼び出すと実際に膨張が進む前に
    // `54000`（`payload_too_large`）で早期に拒否される
    // （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION g0(v) AS v + v")
        .expect("g0 definition should succeed (small syntactic body)");
    for n in 1..=20u32 {
        let prev = n - 1;
        let sql = format!("CREATE FUNCTION g{n}(v) AS g{prev}(v + v)");
        core.execute_sql_in_session(&ctx, &mut session, &sql)
            .unwrap_or_else(|e| {
                panic!("g{n} definition should succeed (syntactic body stays small): {e:?}")
            });
    }

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT g20(id) AS r FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
        )
        .expect_err(
            "expansion of the chained UDF call must be rejected before it can blow up memory",
        );
    assert_eq!(err.wire_code(), "54000");
}
/// Issue #1186（対象ビヘイビア: SQL-26・ERR-6）: 非決定的関数 `now()` は受理せず、
/// 未知関数と同じ `42883` で決定的に拒否する（2 回呼んでも同じコード）。
#[test]
fn non_deterministic_function_call_is_rejected_with_42883_deterministically() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let sql = "SELECT now() FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1";
    for _ in 0..2 {
        let err = core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .expect_err("now() must be rejected");
        assert_eq!(err.wire_code(), "42883");
    }
}

/// Issue #1186: 組み込み関数名（`lower`）・予約名（`hybrid`）・集計関数名（`sum`）と
/// 同名の UDF 登録は `42723`。拒否後も予約名でない UDF は登録できる。
#[test]
fn defining_a_udf_with_a_reserved_name_is_rejected_with_42723() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    for name in ["lower", "hybrid", "sum"] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}(x) AS x"),
            )
            .expect_err("reserved name must be rejected");
        assert_eq!(err.wire_code(), "42723", "name={name}");
    }
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION my_fn(x) AS x")
        .expect("a non-reserved name must still be accepted");
}
