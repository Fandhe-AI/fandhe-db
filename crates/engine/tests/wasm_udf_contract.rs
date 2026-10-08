//! `engine::sql::mode::SessionState::register_wasm_udf` の結合テスト（TASK-149、
//! 対象ビヘイビア: EXT-5, EXT-6。ポインタ: `docs/spec/05-tasks.md` TASK-149・
//! `docs/spec/04-behavior/extensions.md` EXT-5, EXT-6・`docs/spec/04-behavior/rls.md`
//! RLS-8）。
//!
//! `tests/sql_udf_call.rs` と同じ流儀（`unique_db_path` / `CleanupGuard`、実
//! `Storage`＋`CpuScalarProvider`、独立オラクル）で、WASM UDF 呼び出しが結果列・
//! `WHERE` の両位置から動作すること、宣言的 UDF・組み込み関数と同じ値へ収束する
//! こと（3 層一致）、RLS（不可視行では WASM UDF が一切評価されないこと）、拒否経路
//! の wire_code 決定性を検証する。
//!
//! wasmtime バックエンドは依存追加のユーザー承認待ちのため未実装（`engine::wasm_udf`
//! の契約層モジュールドキュメント参照）。本ファイルは
//! [`WasmUdfBackend`](engine::wasm_udf::WasmUdfBackend) trait のモック実装で
//! `sql::udf_call`／`sql::mode` 側の配線（束縛・評価・登録・エラー写像）だけを
//! 検証する。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::wasm_udf::{TrapKind, WasmUdfBackend, WasmUdfError};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fandhe-db-engine-wasm-udf-contract-{label}-{}-{seq}.redb",
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

/// [`WasmUdfBackend`] のモック実装。`s * vec_sum(vec_div(v, vec_norm(v)))`
/// （`tests/sql_udf_call.rs` の宣言的 UDF `norm_scale` と同一の式）を計算し、
/// 宣言的 UDF・独立オラクルとの 3 層一致を検証できるようにする。
/// `call_count` は呼び出し回数を数え、RLS-8（不可視行では評価されない）の検証に使う。
#[derive(Debug)]
struct MockNormScaleBackend {
    call_count: Arc<AtomicUsize>,
    force_err: bool,
}

impl WasmUdfBackend for MockNormScaleBackend {
    fn call_vector_scalar(&self, v: &[f32], scalar: f64) -> Result<f64, WasmUdfError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if self.force_err {
            return Err(WasmUdfError::Trap(TrapKind::Other));
        }
        let sum_sq: f64 = v.iter().map(|&x| (x as f64) * (x as f64)).sum();
        let norm = sum_sq.sqrt();
        if norm == 0.0 {
            // 実バックエンドの 0 除算相当の失敗を模す（`vec_div` の 0 除算 fail-closed
            // と同じ経路をエラー写像側〔`udf_call::eval`〕で確認するための仕込み）。
            return Err(WasmUdfError::Trap(TrapKind::Unreachable));
        }
        let sum: f64 = v.iter().map(|&x| x as f64 / norm).sum();
        Ok(scalar * sum)
    }
}

fn mock_backend(call_count: &Arc<AtomicUsize>) -> Arc<dyn WasmUdfBackend> {
    Arc::new(MockNormScaleBackend {
        call_count: call_count.clone(),
        force_err: false,
    })
}

fn failing_mock_backend(call_count: &Arc<AtomicUsize>) -> Arc<dyn WasmUdfBackend> {
    Arc::new(MockNormScaleBackend {
        call_count: call_count.clone(),
        force_err: true,
    })
}

/// `docs` テーブル（`embedding VECTOR(3)`）を持つ `EngineCore` を新設し、決定的な
/// 小規模コーパスを投入する（`tests/sql_udf_call.rs::new_core_with_docs` と同一の
/// 投入手順）。
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
        // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
        // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
        // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &op_id,
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

fn independent_norm_scale(v: [f32; 3], scale: f64) -> f64 {
    let norm =
        (v[0] as f64 * v[0] as f64 + v[1] as f64 * v[1] as f64 + v[2] as f64 * v[2] as f64).sqrt();
    scale * (v[0] as f64 / norm + v[1] as f64 / norm + v[2] as f64 / norm)
}

// --- 結果列位置（EXT-5） -----------------------------------------------------------

#[test]
fn wasm_call_in_result_column_matches_independent_oracle_and_declarative_udf() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    // 同じ数式の宣言的 UDF も同一セッションへ登録し、3 層（独立オラクル・宣言的
    // UDF・WASM UDF）が一致することを固定する。
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION norm_scale_decl(v, s) AS s * vec_sum(vec_div(v, vec_norm(v)))",
    )
    .expect("CREATE FUNCTION should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, norm_scale_wasm(embedding, 2.0) AS wasm_score, \
                    norm_scale_decl(embedding, 2.0) AS decl_score \
             FROM docs ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with WASM UDF result column should succeed");
    let result = expect_query(outcome);

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
        let expected = independent_norm_scale(*emb, 2.0);
        assert!(
            (float_cell(row, 1) - expected).abs() < 1e-6,
            "row {}: wasm expected {expected}, got {}",
            row.id,
            float_cell(row, 1)
        );
        assert!(
            (float_cell(row, 2) - expected).abs() < 1e-6,
            "row {}: decl expected {expected}, got {}",
            row.id,
            float_cell(row, 2)
        );
    }
    assert_eq!(call_count.load(Ordering::SeqCst), result.rows.len());
}

// --- WHERE 位置（EXT-5） -----------------------------------------------------------

#[test]
fn wasm_call_in_where_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE norm_scale_wasm(embedding, 1.0) > 1.5 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with WASM UDF WHERE predicate should succeed");
    let result = expect_query(outcome);

    // 閾値 1.5 は 3 行の値（1.4 / 1.0 / 1.732）のうち 1 行だけを通す（「常に真」
    // 「常に偽」のいずれでもこのテストが偽陽性で通らないようにする）。
    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    let expected_ids: Vec<u64> = corpus
        .iter()
        .filter(|(_, emb)| independent_norm_scale(*emb, 1.0) > 1.5)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(expected_ids, vec![3]);
    let got_ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(got_ids, expected_ids);
    assert!(call_count.load(Ordering::SeqCst) > 0);
}

#[test]
fn wasm_call_in_both_result_column_and_where_in_a_single_statement() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, norm_scale_wasm(embedding, 2.0) AS n FROM docs \
             WHERE norm_scale_wasm(embedding, 2.0) > 2.5 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect("SELECT with WASM UDF in both positions should succeed");
    let result = expect_query(outcome);

    let corpus: [(u64, [f32; 3]); 3] = [
        (1, [3.0, 4.0, 0.0]),
        (2, [0.0, 0.0, 1.0]),
        (3, [1.0, 1.0, 1.0]),
    ];
    let expected_ids: Vec<u64> = corpus
        .iter()
        .filter(|(_, emb)| independent_norm_scale(*emb, 2.0) > 2.5)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(
        result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        expected_ids
    );
    for row in &result.rows {
        let (_, emb) = corpus
            .iter()
            .find(|(id, _)| *id == row.id)
            .expect("known id");
        let expected = independent_norm_scale(*emb, 2.0);
        assert!((float_cell(row, 1) - expected).abs() < 1e-6);
    }
}

// --- RLS（RLS-8: 不可視行では WASM UDF が一切評価されない） -----------------------

#[test]
fn wasm_call_never_evaluates_on_invisible_rows() {
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
    // `MockNormScaleBackend`（`vec_norm(v) == 0` を 0 除算相当として拒否する）が
    // 評価されればクエリ全体が失敗するようにする。
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
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    // tenant-a のクエリは、tenant-b の不可視行（零ベクトル）にだけ 0 除算相当の
    // エラーを起こす WASM UDF を含んでいても成功する（不可視行では WASM UDF が
    // 一切評価されないため）。
    let outcome = core
        .execute_sql_in_session(
            &ctx_a,
            &mut session,
            "SELECT id FROM docs WHERE norm_scale_wasm(embedding, 1.0) > 0.0 \
             ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
        )
        .expect("tenant-a query must not fail due to tenant-b's invisible row");
    let result = expect_query(outcome);
    assert_eq!(
        result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1]
    );
    // 可視行 1 件のみ評価される（不可視行分は呼ばれない）。
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

// --- 拒否経路（EXT-6: バックエンド失敗・非有限値は行単位で 22000） -----------------

#[test]
fn backend_error_maps_to_invalid_input_in_where_position() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("always_fails", failing_mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM docs WHERE always_fails(embedding, 1.0) > 0.0 \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect_err("backend failure must be fail-closed");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn backend_error_maps_to_invalid_input_in_result_column_position() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("always_fails", failing_mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, always_fails(embedding, 1.0) AS n FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect_err("backend failure must be fail-closed");
    assert_eq!(err.wire_code(), "22000");
}

#[derive(Debug)]
struct NonFiniteBackend;
impl WasmUdfBackend for NonFiniteBackend {
    fn call_vector_scalar(&self, _v: &[f32], _scalar: f64) -> Result<f64, WasmUdfError> {
        Ok(f64::INFINITY)
    }
}

#[test]
fn non_finite_backend_result_is_rejected_fail_closed() {
    // fail-closed: バックエンドの `Ok` 戻り値であっても非有限（NaN/∞）は黙って
    // 通さず `22000` で拒否する（`sql::udf_call::finite_scalar` と同じ契約を
    // WASM 呼び出し経路にも適用する）。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    session
        .register_wasm_udf("non_finite", Arc::new(NonFiniteBackend))
        .expect("register_wasm_udf should succeed");

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, non_finite(embedding, 1.0) AS n FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect_err("non-finite backend result must be fail-closed");
    assert_eq!(err.wire_code(), "22000");
}

// --- セッション分離 ------------------------------------------------------------------

#[test]
fn wasm_udf_registration_does_not_leak_across_sessions() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session_a = SessionState::default();
    let session_b = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session_a
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    let mut session_b_mut = session_b;
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session_b_mut,
            "SELECT norm_scale_wasm(embedding, 1.0) FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect_err("session_b must not see session_a's WASM UDF registration");
    assert_eq!(err.wire_code(), "42883");
}

// --- 登録時の拒否経路 ----------------------------------------------------------------

#[test]
fn registering_wasm_udf_with_builtin_name_is_rejected() {
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    let err = session
        .register_wasm_udf("vec_norm", mock_backend(&call_count))
        .expect_err("collision with builtin name must be rejected");
    assert_eq!(err.wire_code(), "42723");
}

#[test]
fn registering_wasm_udf_with_name_used_by_declarative_udf_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION shared_name(v) AS vec_norm(v)",
    )
    .expect("CREATE FUNCTION should succeed");

    let call_count = Arc::new(AtomicUsize::new(0));
    let err = session
        .register_wasm_udf("shared_name", mock_backend(&call_count))
        .expect_err("collision with declarative UDF name must be rejected");
    assert_eq!(err.wire_code(), "42723");
}

#[test]
fn redefining_the_same_wasm_udf_name_is_rejected() {
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("first registration should succeed");
    let err = session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect_err("redefinition must be rejected");
    assert_eq!(err.wire_code(), "42723");
}

#[test]
fn wasm_udf_session_limit_is_enforced() {
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    // `engine::sql::udf_call::MAX_SESSION_UDFS`（宣言的・WASM 合算の上限）まで
    // ちょうど登録できて、上限+1 本目は `54000`（payload_too_large）へ切り替わる
    // 境界を直接固定する。
    for i in 0..engine::sql::udf_call::MAX_SESSION_UDFS {
        let name = format!("wasm_fn_{i}");
        session
            .register_wasm_udf(&name, mock_backend(&call_count))
            .expect("registration within the session limit should succeed");
    }
    let err = session
        .register_wasm_udf("wasm_fn_over_limit", mock_backend(&call_count))
        .expect_err("registration beyond the session limit must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- 引数個数・型不一致（束縛時に 22000） ------------------------------------------

#[test]
fn wasm_call_with_wrong_argument_count_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT norm_scale_wasm(embedding) FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect_err("wrong argument count must be rejected at bind time");
    assert_eq!(err.wire_code(), "42883");
    assert_eq!(call_count.load(Ordering::SeqCst), 0);
}

#[test]
fn wasm_call_with_wrong_argument_type_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let call_count = Arc::new(AtomicUsize::new(0));
    session
        .register_wasm_udf("norm_scale_wasm", mock_backend(&call_count))
        .expect("register_wasm_udf should succeed");

    // 第 1 引数は Vector を要求するが Scalar を渡す。
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT norm_scale_wasm(1.0, 2.0) FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1",
        )
        .expect_err("argument type mismatch must be rejected at bind time");
    assert_eq!(err.wire_code(), "42804");
    assert_eq!(call_count.load(Ordering::SeqCst), 0);
}
