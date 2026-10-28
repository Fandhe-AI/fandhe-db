//! `batch_fallback.rs::FallbackBatchEngine` の結合テスト（TASK-129・対象
//! ビヘイビア: CORE-8）。
//!
//! クレート外部（`engine` の公開 API のみ）からエラー注入バックエンドを
//! 差し込み、GPU バックエンドの初期化失敗・実行時エラーの双方で CPU-SIMD
//! 縮退経路（`kernel.rs::CpuScalarProvider` と同じ選出規約）へ panic なしに
//! 切り替わること・Top-k がオラクルと一致すること・ログ可視化イベントが
//! 要因を正しく反映することを検証する。

use std::sync::{Arc, Mutex};

use engine::batch_fallback::{
    BatchBackend, BatchBackendError, BatchExecError, FallbackBatchEngine, FallbackEvent,
    FallbackObserver, FallbackReason,
};
use engine::batch_search::{BatchEngine, BatchHit, BatchQuery, BatchSearchError, ResidentMatrix};
use engine::kernel::{CpuScalarProvider, SearchHit, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::storage::Visibility;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant id")
}

/// 発生した縮退イベントを記録するオブザーバ（テストコード側の実装。本体へ
/// テスト専用 API を追加しない）。
#[derive(Clone, Default)]
struct RecordingObserver(Arc<Mutex<Vec<FallbackEvent>>>);

impl RecordingObserver {
    fn events(&self) -> Vec<FallbackEvent> {
        self.0.lock().expect("lock").clone()
    }
}

impl FallbackObserver for RecordingObserver {
    fn on_fallback(&self, event: FallbackEvent) {
        self.0.lock().expect("lock").push(event);
    }
}

/// primary バックエンドの実行時エラーを注入するモック。
struct FailingBackend(BatchBackendError);

impl BatchBackend for FailingBackend {
    fn batch_search(&self, _queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        Err(BatchExecError::Backend(self.0.clone()))
    }
}

/// primary バックエンドが入力エラー（`BatchSearchError`）を返すことを模した
/// モック（縮退トリガにならないことの検証用）。
struct InputErrorBackend(BatchSearchError);

impl BatchBackend for InputErrorBackend {
    fn batch_search(&self, _queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        Err(BatchExecError::Input(self.0.clone()))
    }
}

/// 4 行・dim=2・tenant-a/tenant-b が各 2 行のフィクスチャ。
fn fixture() -> ([u64; 4], [String; 4], [Visibility; 4], usize, [f32; 8]) {
    let ids = [1u64, 2, 3, 4];
    let tenant_ids = [
        "tenant-a".to_string(),
        "tenant-a".to_string(),
        "tenant-b".to_string(),
        "tenant-b".to_string(),
    ];
    let visibilities = [
        Visibility::Public,
        Visibility::Public,
        Visibility::Public,
        Visibility::Public,
    ];
    #[rustfmt::skip]
    let vectors = [
        1.0, 0.0,
        0.0, 1.0,
        2.0, 0.0,
        0.0, 2.0,
    ];
    (ids, tenant_ids, visibilities, 2, vectors)
}

/// オラクル: `PolicyContext::is_visible` で可視行だけへ絞り込んだうえで
/// `kernel::CpuScalarProvider::search`（CORE-3 と同一の選出規約: スコア降順・
/// 同点 id 昇順・非有限値除外）を呼ぶ。縮退経路の Top-k と完全一致することを
/// 期待する。
///
/// `batch_search.rs::run_batch_search` は行外側ループの計算量最適化として、
/// 行をその `tenant_id` に一致するクエリ集合に加え、TASK-89（TABLE-9）対応で
/// 他テナントの `Public` 許可クエリからも候補にする。最終判定は常に
/// `PolicyContext::is_visible` の単一照合パスへ委ねるため、本オラクルも
/// 同じ述語だけで可視行を絞り込む（テナント一致の事前フィルタは行わない。
/// `src/batch_fallback.rs` の同名オラクル参照）。
fn oracle_search(
    ids: &[u64],
    tenant_ids: &[String],
    dim: usize,
    vectors: &[f32],
    ctx: &PolicyContext,
    query: &[f32],
    k: usize,
) -> Vec<SearchHit> {
    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。候補識別子は行 `id` ではなく
    // 元配列内のスロット index を使う（本体コードと同じ「候補識別子＝スロット
    // 番号」方式）。行 `id` の一意性スコープはテナント内に閉じているため、
    // `id` 値そのものを識別子にすると異なるテナントが同じ `id` を共有する
    // フィクスチャで逆引きが曖昧になる。
    let mut visible_slots: Vec<u64> = Vec::new();
    let mut visible_vectors = Vec::new();
    for (row_idx, (_id, tenant)) in ids.iter().zip(tenant_ids).enumerate() {
        if ctx.is_visible(tenant, Visibility::Public) {
            visible_slots.push(row_idx as u64);
            let start = row_idx * dim;
            visible_vectors.extend_from_slice(&vectors[start..start + dim]);
        }
    }
    let hits = CpuScalarProvider
        .search(SearchInput {
            ids: &visible_slots,
            vectors: &visible_vectors,
            dim: dim as u32,
            query,
            k,
        })
        .expect("oracle search ok");
    hits.into_iter()
        .map(|hit| {
            let idx = hit.id as usize;
            SearchHit::new(tenant_ids[idx].clone(), ids[idx], hit.score)
        })
        .collect()
}

// CORE-8: 初期化失敗注入 → 構築 `Ok`（CPU 専用モード）・検索 `Ok`・イベント
// ちょうど 1 件（要因=init・切り替え先=cpu-simd）・Top-k がオラクル一致。
#[test]
fn init_failure_falls_back_to_cpu_and_matches_oracle() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let observer = RecordingObserver::default();

    let engine = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| Err(BatchBackendError::InitFailed("no gpu device".to_string())),
        Box::new(observer.clone()),
    )
    .expect("build succeeds in cpu-only mode despite init failure");

    let events = observer.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, FallbackReason::Init);
    assert_eq!(events[0].target, "cpu-simd");

    let ctx_a = ctx("tenant-a");
    let query = [1.0f32, 0.0];
    let queries = vec![BatchQuery {
        vector: &query,
        k: 2,
        ctx: &ctx_a,
    }];
    let hits = engine
        .batch_search(&queries)
        .expect("search does not surface an error to the client");
    assert_eq!(hits.len(), 1);
    let expected = oracle_search(&ids, &tenant_ids, dim, &vectors, &ctx_a, &query, 2);
    assert_eq!(hits[0].hits, expected);
}

// CORE-8: 実行時エラー注入（デバイスロスト・カーネル起動失敗・転送失敗の
// 各種別）→ 検索 `Ok`・イベント要因=Runtime・Top-k がオラクル一致。
#[test]
fn runtime_errors_fall_back_to_cpu_and_match_oracle() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();

    for backend_err in [
        BatchBackendError::DeviceLost("device lost".to_string()),
        BatchBackendError::KernelLaunchFailed("launch failed".to_string()),
        BatchBackendError::TransferFailed("transfer failed".to_string()),
    ] {
        let observer = RecordingObserver::default();
        let engine = FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            move |_matrix: ResidentMatrix| {
                Ok(Box::new(FailingBackend(backend_err.clone())) as Box<dyn BatchBackend>)
            },
            Box::new(observer.clone()),
        )
        .expect("build succeeds; primary initializes fine");

        let ctx_b = ctx("tenant-b");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx_b,
        }];
        let hits = engine.batch_search(&queries).expect("search ok");
        let expected = oracle_search(&ids, &tenant_ids, dim, &vectors, &ctx_b, &query, 2);
        assert_eq!(hits[0].hits, expected);

        let events = observer.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, FallbackReason::Runtime);
        assert_eq!(events[0].target, "cpu-simd");
    }
}

// 入力エラー（次元不一致・非有限値・k=0/上限超過・容量超過）は、primary が
// 健全なときも縮退が起きているときも同一の `Err` を返し、縮退イベントは
// 発生しない（不正入力を縮退で黙殺しない、という設計の要）。
#[test]
fn input_errors_are_identical_and_do_not_trigger_fallback_regardless_of_backend_health() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let ctx_a = ctx("tenant-a");

    // dim=2 の常駐行列に対して 3 次元クエリを渡す（次元不一致）。
    let bad_query = [1.0f32, 0.0, 0.0];
    let queries = vec![BatchQuery {
        vector: &bad_query,
        k: 1,
        ctx: &ctx_a,
    }];

    // primary 健全時。
    let observer_healthy = RecordingObserver::default();
    let engine_healthy = FallbackBatchEngine::build_with_gpu_reference(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        Box::new(observer_healthy.clone()),
    )
    .expect("build ok");
    let err_healthy = engine_healthy
        .batch_search(&queries)
        .expect_err("dim mismatch must be rejected");
    assert_eq!(
        err_healthy,
        BatchSearchError::DimMismatch {
            expected: dim,
            found: 3
        }
    );
    assert!(observer_healthy.events().is_empty());

    // primary が実行時エラーを起こす状態（`FailingBackend`。呼ばれれば常に
    // `BatchExecError::Backend` を返す）でも、不正入力は primary に到達する
    // 前に `FallbackBatchEngine::batch_search` の先行入力検証
    // （`batch_search.rs::validate_batch_queries`）で弾かれる（TASK-129・
    // CORE-8 レビュー起因の P1 指摘対応・PR #152）。primary を検証前に呼ぶと、
    // 不正入力 1 件で `runtime_latched` が恒久ラッチされ、以降の正当な検索
    // まで CPU 縮退経路へ固定される可用性バグになるため、`FailingBackend` は
    // 一度も呼ばれず、縮退イベントも一切発生しない（primary が健全なときと
    // 完全に同一の `Err`・観測結果になることが本テストの主眼）。
    let observer_degraded = RecordingObserver::default();
    let engine_degraded = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| {
            Ok(Box::new(FailingBackend(BatchBackendError::DeviceLost(
                "lost".to_string(),
            ))) as Box<dyn BatchBackend>)
        },
        Box::new(observer_degraded.clone()),
    )
    .expect("build ok");
    let err_degraded = engine_degraded.batch_search(&queries).expect_err(
        "dim mismatch must be rejected before the backend is ever invoked, \
         even when the backend would otherwise fail",
    );
    assert_eq!(err_degraded, err_healthy);
    assert!(
        observer_degraded.events().is_empty(),
        "input validation failures must not reach the primary backend or latch the runtime fallback"
    );
}

// 回帰テスト（TASK-129・CORE-8 レビュー起因の P1 指摘対応・PR #152）:
// 不正入力（先行入力検証で拒否される）は `runtime_latched` を先取りで
// ラッチしない。もし誤って先取りラッチしていた場合、後続の正当なクエリは
// 「既にラッチ済み」として primary を一切呼ばずに CPU 縮退経路へ直行して
// しまい、縮退イベントが発生しない（＝ここで primary が実際に呼ばれたことを
// 証明できない）。したがって「不正入力の直後に正当なクエリを送ると、
// primary が実際に呼ばれて実行時エラーによる縮退イベントがちょうど 1 件
// 発生する」ことを確認することで、不正入力側でラッチが起きていないことを
// 間接的に証明する。
#[test]
fn invalid_query_does_not_prematurely_latch_the_runtime_fallback() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| {
            Ok(Box::new(FailingBackend(BatchBackendError::DeviceLost(
                "lost".to_string(),
            ))) as Box<dyn BatchBackend>)
        },
        Box::new(observer.clone()),
    )
    .expect("build ok");

    // 1 回目: 不正入力（次元不一致）。先行入力検証で拒否され、primary には
    // 到達しない。
    let ctx_a = ctx("tenant-a");
    let bad_query = [1.0f32, 0.0, 0.0];
    let bad_queries = vec![BatchQuery {
        vector: &bad_query,
        k: 1,
        ctx: &ctx_a,
    }];
    engine
        .batch_search(&bad_queries)
        .expect_err("dim mismatch must be rejected");
    assert!(observer.events().is_empty());

    // 2 回目: 正当な入力。`runtime_latched` が 1 回目で先取りラッチされて
    // いなければ、primary（`FailingBackend`）が実際に呼ばれて実行時エラーを
    // 返し、CPU 縮退経路へ切り替わりつつ縮退イベントがちょうど 1 件発生する。
    let good_query = [1.0f32, 0.0];
    let good_queries = vec![BatchQuery {
        vector: &good_query,
        k: 1,
        ctx: &ctx_a,
    }];
    let hits = engine
        .batch_search(&good_queries)
        .expect("valid query must still reach the primary backend");
    assert_eq!(hits.len(), 1);
    let events = observer.events();
    assert_eq!(
        events.len(),
        1,
        "the primary backend must have been invoked exactly once, by the valid query"
    );
    assert_eq!(events[0].reason, FallbackReason::Runtime);
}

// 回帰テスト（Cursor Bugbot Medium 指摘対応・PR #158）: 空バッチ
// （`queries.is_empty()`）は `runtime_latched` の状態に関わらず常に成功
// （空の結果）を返す。`batch_search.rs::validate_batch_queries` は件数 0 を
// 有効な入力として受理する一方、`DispatchInput::for_batch` は
// `batch_size == 0` を不正入力として拒否するため、`FallbackBatchEngine::
// batch_search` が両者の間で経路選択（`dispatch::select_execution_path`）を
// 呼び出してしまうと、空バッチの成否が「これまでに primary が実行時失敗して
// `runtime_latched` 済みか」という無関係な状態に依存する非決定的な挙動に
// なってしまう。ラッチ未発火（primary 健全）・ラッチ発火済み（CPU 縮退済み）
// の双方で空バッチが同一の `Ok(vec![])` を返すことを確認する。
#[test]
fn empty_batch_always_succeeds_regardless_of_runtime_latch_state() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let empty_queries: Vec<BatchQuery<'_>> = Vec::new();

    // ラッチ未発火（primary 健全）。
    let observer_healthy = RecordingObserver::default();
    let engine_healthy = FallbackBatchEngine::build_with_gpu_reference(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        Box::new(observer_healthy.clone()),
    )
    .expect("build ok");
    assert!(engine_healthy
        .batch_search(&empty_queries)
        .expect("empty batch must succeed while primary is healthy")
        .is_empty());
    assert!(observer_healthy.events().is_empty());

    // ラッチ発火済み（実行時エラーで一度 CPU 縮退へ切り替わった後）。
    let observer_latched = RecordingObserver::default();
    let engine_latched = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| {
            Ok(Box::new(FailingBackend(BatchBackendError::DeviceLost(
                "lost".to_string(),
            ))) as Box<dyn BatchBackend>)
        },
        Box::new(observer_latched.clone()),
    )
    .expect("build ok");
    let ctx_a = ctx("tenant-a");
    let query = [1.0f32, 0.0];
    let queries = vec![BatchQuery {
        vector: &query,
        k: 1,
        ctx: &ctx_a,
    }];
    engine_latched
        .batch_search(&queries)
        .expect("valid query triggers runtime fallback and latches it");
    assert_eq!(observer_latched.events().len(), 1, "latch must be armed");

    assert!(engine_latched
        .batch_search(&empty_queries)
        .expect("empty batch must succeed identically after the runtime latch is armed")
        .is_empty());
    assert_eq!(
        observer_latched.events().len(),
        1,
        "the empty batch itself must not emit an additional fallback event"
    );
}

// primary が TenantMaskViolation 等の入力エラーを返した場合、縮退トリガには
// ならず fail-closed に `Err` をそのまま返すことを確認する（security.md
// 「不安全な設計」対応）。
#[test]
fn tenant_mask_violation_from_primary_is_not_treated_as_fallback_trigger() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| {
            Ok(
                Box::new(InputErrorBackend(BatchSearchError::TenantMaskViolation))
                    as Box<dyn BatchBackend>,
            )
        },
        Box::new(observer.clone()),
    )
    .expect("build ok");

    let ctx_a = ctx("tenant-a");
    let query = [1.0f32, 0.0];
    let queries = vec![BatchQuery {
        vector: &query,
        k: 1,
        ctx: &ctx_a,
    }];
    let err = engine
        .batch_search(&queries)
        .expect_err("tenant mask violation must not be masked by fallback");
    assert_eq!(err, BatchSearchError::TenantMaskViolation);
    assert!(observer.events().is_empty());
}

// マルチテナントバッチで縮退後も他テナント行の混入が 0 件であることを検証する
// （テナント境界。security.md P0）。越境しないことの検証は `Private` 行の
// フィクスチャで行う（ポインタ: TASK-89 / TABLE-9）。
#[test]
fn fallback_search_does_not_leak_rows_across_tenants() {
    let (ids, tenant_ids, _visibilities, dim, vectors) = fixture();
    let visibilities = [Visibility::Private; 4];
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| {
            Ok(Box::new(FailingBackend(BatchBackendError::DeviceLost(
                "lost".to_string(),
            ))) as Box<dyn BatchBackend>)
        },
        Box::new(observer),
    )
    .expect("build ok");

    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Private]).expect("valid tenant");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
    let query = [1.0f32, 1.0];
    let queries = vec![
        BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        },
        BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_b,
        },
    ];
    let hits = engine.batch_search(&queries).expect("search ok");
    assert_eq!(hits.len(), 2);
    for hit in &hits[0].hits {
        assert!(
            ids[..2].contains(&hit.id),
            "tenant-a result leaked a non-tenant-a row id={}",
            hit.id
        );
    }
    for hit in &hits[1].hits {
        assert!(
            ids[2..].contains(&hit.id),
            "tenant-b result leaked a non-tenant-b row id={}",
            hit.id
        );
    }
}

// 正常時（primary 成功）はイベント 0 件・縮退再実行が発生しないことを確認する。
#[test]
fn healthy_primary_emits_no_fallback_event() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build_with_gpu_reference(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        Box::new(observer.clone()),
    )
    .expect("build ok");

    let ctx_a = ctx("tenant-a");
    let query = [1.0f32, 0.0];
    let queries = vec![BatchQuery {
        vector: &query,
        k: 1,
        ctx: &ctx_a,
    }];
    let hits = engine.batch_search(&queries).expect("search ok");
    assert_eq!(hits.len(), 1);
    assert!(observer.events().is_empty());
}

// 決定性: 同一入力の再実行が同一結果を返す。
#[test]
fn fallback_search_is_deterministic_across_repeated_calls() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| {
            Ok(Box::new(FailingBackend(BatchBackendError::TransferFailed(
                "transfer".to_string(),
            ))) as Box<dyn BatchBackend>)
        },
        Box::new(observer),
    )
    .expect("build ok");

    let ctx_a = ctx("tenant-a");
    let query = [1.0f32, 1.0];
    let queries = vec![BatchQuery {
        vector: &query,
        k: 2,
        ctx: &ctx_a,
    }];
    let first = engine.batch_search(&queries).expect("search ok");
    let second = engine.batch_search(&queries).expect("search ok");
    assert_eq!(first[0].hits, second[0].hits);
}

// 対象ビヘイビア: TABLE-9（レビュー起因の回帰・クレート外部からの公開 API
// 経由確認）。`engine::batch_search::BatchEngine`（`batch_search.rs::
// run_batch_search` を直接呼ぶ経路）と `FallbackBatchEngine`（CPU 縮退経路。
// 内部で同じ `run_batch_search` を呼ぶ）の双方で、tenant-a のクエリが
// tenant-b の `Public` 行を実際に返すことを、engine 公開 API のみから確認
// する（`src/batch_search.rs`・`src/batch_fallback.rs` 内の同種ユニット
// テストは `pub(crate)` API 経由のため、`wire-server` から見える公開面の
// 回帰はこの結合テストが担う）。混入 0 件だけでは判定の正方向を保証しないため、
// tenant-b の id が実際に返ることまで確認する（ポインタ: TASK-89 / TABLE-9）。
#[test]
fn batch_search_and_fallback_both_include_other_tenant_public_rows() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let ctx_a = ctx("tenant-a");
    let query = [1.0f32, 0.0];
    let queries = vec![BatchQuery {
        vector: &query,
        k: 4,
        ctx: &ctx_a,
    }];

    // `BatchEngine`（primary 実装。`ResidentMatrix` を直接持つ）。
    let matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, &vectors)
        .expect("valid matrix");
    let primary_engine = BatchEngine::new(matrix);
    let primary_hits = primary_engine
        .batch_search(&queries)
        .expect("primary search ok");
    let primary_ids: std::collections::HashSet<u64> =
        primary_hits[0].hits.iter().map(|h| h.id).collect();
    assert!(
        primary_ids.contains(&3) || primary_ids.contains(&4),
        "BatchEngine must exercise TABLE-9 mutual visibility, not just absent"
    );

    // `FallbackBatchEngine`（CPU 縮退専用に構築。primary は常に失敗させ、
    // CPU 縮退経路の `run_batch_search` を確実に通す）。
    let observer = RecordingObserver::default();
    let fallback_engine = FallbackBatchEngine::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
        |_matrix: ResidentMatrix| Err(BatchBackendError::InitFailed("no gpu device".to_string())),
        Box::new(observer),
    )
    .expect("build succeeds in cpu-only mode");
    let fallback_hits = fallback_engine
        .batch_search(&queries)
        .expect("fallback search ok");
    let fallback_ids: std::collections::HashSet<u64> =
        fallback_hits[0].hits.iter().map(|h| h.id).collect();
    assert_eq!(
        primary_ids, fallback_ids,
        "BatchEngine and FallbackBatchEngine（CPU 縮退経路）must agree on TABLE-9 visibility"
    );
}

// 実 GPU バックエンド（`gpu_batch.rs::GpuBatchBackend`。TASK-128〜130・Issue #178
// ポインタ）を primary として構築する回帰テスト。GPU の有無を問わず
// `build_with_gpu` 自体が panic せず成立すること（初期化失敗時は CPU 専用
// モードへ fail-closed に縮退する契約。詳細な GPU 分岐の正しさ・テナント
// 混入 0 件の検証は `tests/gpu_batch.rs` が担う）。
#[test]
fn build_with_gpu_constructs_successfully_regardless_of_gpu_availability() {
    let (ids, tenant_ids, visibilities, dim, vectors) = fixture();
    let observer = RecordingObserver::default();
    let engine =
        FallbackBatchEngine::build_with_gpu(&ids, &tenant_ids, &visibilities, dim, &vectors, Box::new(observer))
            .expect("build_with_gpu should not fail regardless of gpu availability (CORE-8 fail-closed init contract)");

    let ctx_a = ctx("tenant-a");
    let query = [1.0f32, 0.0];
    let queries = vec![BatchQuery {
        vector: &query,
        k: 4,
        ctx: &ctx_a,
    }];
    let hits = engine
        .batch_search(&queries)
        .expect("batch_search should succeed via gpu primary or cpu fallback");
    assert_eq!(hits.len(), 1);
}
