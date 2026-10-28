//! `search_engine.rs` の選択・構築レイヤに対する結合テスト（TASK-131）。
//! 対象ビヘイビア: CORE-9（総当たり実装の差し替え可能なインターフェース越し呼び出し）・
//! CORE-13（`SearchProvider` trait への一本化）。
//!
//! `EngineCore::from_storage` で `Storage` の所有権を渡してから検索する構成は
//! `tests/vector_core.rs` と同じ手法（`EngineCore` がテナント境界を迂回する生ハンドルを
//! 公開しないため）。

use engine::core::{EngineCore, VectorCore};
use engine::kernel::{CandidateHit, CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::search_engine::{self, SearchEngineKind};
use engine::storage::{RowInput, Storage, Visibility};

// 一時ディレクトリ（`TempDir`）は Issue #173 で `crates/engine/src/test_util/temp_db.rs`
// へ一本化した（旧: `tests/vector_core.rs` と同型のローカル複製）。
use crate::temp_db;
use temp_db::TempDir;

fn schema_for(table_name: &str, dim: u32) -> engine::catalog::TableSchema {
    engine::catalog::TableSchema::new(
        table_name,
        vec![engine::catalog::ColumnDef::new(
            "embedding",
            engine::catalog::ColumnType::Vector(dim),
            false,
        )],
    )
}

fn seed_row(storage: &Storage, table: &str, id: u64, tenant: &str, embedding: &[f32]) {
    // テナント境界付き書き込みガード（TASK-95・RECOVER-4）経由で投入する。生の
    // `Storage::insert_row_into_table` は `pub(crate)` 化済みでクレート外から呼べない
    // （codex-review P0 指摘対応）。
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容ハッシュを
    // 持つため、同一テナント・同一テーブル内で内容の異なる複数行へ同一 operation_id を
    // 使い回すと 2 件目以降が OperationIdContentMismatch で拒否される。行ごとに一意の
    // operation_id を使う。
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "test-op-{tenant}-{table}-{id}"
    ))
    .expect("valid operation_id");
    engine::tenant::insert_row(
        storage,
        table,
        &ctx,
        id,
        &RowInput {
            tenant_id: tenant,
            visibility: Visibility::Public,
            embedding,
            metadata: &[],
        },
        &op_id,
    )
    .expect("seed row");
}

/// `docs` テーブル（dim=3）へ 6 行を投入した `Storage` を返す。同点スコアを含む
/// 入力にして、選出規約（スコア降順・同点 id 昇順）まで一致するかを検証できるようにする。
fn seed_storage(dir: &TempDir) -> Storage {
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 3))
        .expect("create table");
    seed_row(&storage, "docs", 1, "tenant-a", &[1.0, 0.0, 0.0]);
    seed_row(&storage, "docs", 2, "tenant-a", &[0.0, 1.0, 0.0]);
    seed_row(&storage, "docs", 3, "tenant-a", &[2.0, 0.0, 0.0]);
    seed_row(&storage, "docs", 4, "tenant-a", &[0.0, 0.0, 1.0]);
    // id=5, id=6 はクエリに対して同点スコアになる（タイブレーク: id 昇順）。
    seed_row(&storage, "docs", 5, "tenant-a", &[1.0, 1.0, 0.0]);
    seed_row(&storage, "docs", 6, "tenant-a", &[1.0, 1.0, 0.0]);
    storage
}

// CORE-9: `search_engine::default_engine()` で構築した provider を注入した
// `EngineCore` の検索結果が、参照実装 `CpuScalarProvider` を直接注入した場合と一致する
// ことを検証する（総当たり実装がインターフェース越しに差し替え・呼び出しされている
// ことの確認。既定エンジンはマルチスレッド並列実装だが、Top-k の選出規約
// （スコア降順・同点 id 昇順）は共有ヘルパ `TopKSelector` 経由で参照実装と揃う）。
#[test]
fn core9_default_engine_matches_cpu_scalar_reference() {
    let dir_default = TempDir::new("core9-default");
    let storage_default = seed_storage(&dir_default);
    let core_default = EngineCore::from_storage(storage_default, search_engine::default_engine());

    let dir_reference = TempDir::new("core9-reference");
    let storage_reference = seed_storage(&dir_reference);
    let core_reference = EngineCore::from_storage(storage_reference, Box::new(CpuScalarProvider));

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let query = [1.0, 0.0, 0.0];

    let hits_default = core_default
        .search(&ctx, "docs", &query, 4)
        .expect("default engine search");
    let hits_reference = core_reference
        .search(&ctx, "docs", &query, 4)
        .expect("reference engine search");

    assert_eq!(hits_default, hits_reference);
}

// CORE-9: `build(CpuScalarBruteForce)` と `build(ParallelBruteForce)` が同一入力で
// 同一の Top-k（スコア降順・同点 id 昇順）を返す差し替え等価性を検証する。
#[test]
fn core9_build_variants_agree_on_same_input() {
    let dir_cpu = TempDir::new("core9-build-cpu");
    let storage_cpu = seed_storage(&dir_cpu);
    let core_cpu = EngineCore::from_storage(
        storage_cpu,
        search_engine::build(SearchEngineKind::CpuScalarBruteForce),
    );

    let dir_parallel = TempDir::new("core9-build-parallel");
    let storage_parallel = seed_storage(&dir_parallel);
    let core_parallel = EngineCore::from_storage(
        storage_parallel,
        search_engine::build(SearchEngineKind::ParallelBruteForce),
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    // タイブレーク（id=5 と id=6 の同点）を Top-k 境界に含めるクエリ。
    let query = [1.0, 1.0, 0.0];

    let hits_cpu = core_cpu
        .search(&ctx, "docs", &query, 6)
        .expect("cpu search");
    let hits_parallel = core_parallel
        .search(&ctx, "docs", &query, 6)
        .expect("parallel search");

    assert_eq!(hits_cpu, hits_parallel);
}

/// ANN 実装を想定したモック provider。呼び出されたことを記録するだけで、実処理は
/// 参照実装 `CpuScalarProvider` へ委譲する（`tests/vector_core.rs::RecordingProvider` と
/// 同型の計装手法）。
struct MockAnnProvider {
    called: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SearchProvider for MockAnnProvider {
    fn search(
        &self,
        input: SearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, engine::kernel::KernelError> {
        self.called.store(true, std::sync::atomic::Ordering::SeqCst);
        CpuScalarProvider.search(input)
    }
}

// CORE-9: ANN 想定のモック provider が、同一の注入点（`Box<dyn SearchProvider>`）から
// `VectorCore::search` 経路で実際に呼ばれることを確認する（差し替え後も
// `core.rs` 側の fail-closed 検証・呼び出し経路が有効なことの確認。
// `tests/vector_core.rs` の CORE-13 テストとは別に、`search_engine.rs` が定義する
// 注入点（本モジュールの責務）を経由した呼び出しであることを固定する）。
#[test]
fn core9_mock_ann_provider_is_actually_invoked_through_injection_point() {
    let dir = TempDir::new("core9-mock-ann");
    let storage = seed_storage(&dir);
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Box::new(MockAnnProvider {
        called: called.clone(),
    });
    let core = EngineCore::from_storage(storage, provider);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let hits = core
        .search(&ctx, "docs", &[1.0, 0.0, 0.0], 2)
        .expect("mock ann search");
    assert_eq!(hits.len(), 2);
    assert!(
        called.load(std::sync::atomic::Ordering::SeqCst),
        "注入した MockAnnProvider::search が呼ばれていない"
    );
}

// Issue #407 受け入れ条件 (a): 既定エンジンが不変であることを `EngineCore::open` の
// 実体（`search_engine_kind()`）で固定する。`search_engine::default_kind()` も
// 同値であることを併せて確認し、既定値の源泉が [`search_engine::default_kind`]
// 1 箇所へ集約されていることを検証する。
#[test]
fn hnsw_407_default_engine_kind_is_unchanged() {
    let dir = TempDir::new("hnsw-407-default-kind");
    let core = EngineCore::open(dir.db_path()).expect("open default engine");
    assert_eq!(
        core.search_engine_kind(),
        Some(SearchEngineKind::ParallelBruteForce)
    );
    assert_eq!(
        search_engine::default_kind(),
        SearchEngineKind::ParallelBruteForce
    );
}

// Issue #407 受け入れ条件 (a) 続き: `open_with_engine` に既定 kind を明示的に渡した
// 場合でも、既定 provider（`ParallelSearchProvider`）と検索結果が一致することを確認する
// （既定経路そのものの回帰が無いことの確認。`with_provider` 経由〔`kind` 不明〕は
// `None` を返す非対称性も併せて固定する）。
#[test]
fn hnsw_407_with_provider_has_no_recorded_kind() {
    let dir = TempDir::new("hnsw-407-with-provider-kind");
    let storage = seed_storage(&dir);
    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    assert_eq!(core.search_engine_kind(), None);
}

// Issue #407 受け入れ条件 (b): opt-in（`SearchEngineKind::Hnsw`）を明示指定すると
// `search_engine_kind()` がその値を返し、かつ同一入力に対する Top-k
// （同点タイブレーク含む）が既定エンジンと完全一致すること（本タスク時点の
// 全件 brute-force フォールバック契約。`hnsw/provider.rs` モジュールドキュメント
// 「本タスク時点の契約」節）を確認する。
#[test]
fn hnsw_407_opt_in_engine_selected_and_matches_default_via_fallback() {
    let dir_hnsw = TempDir::new("hnsw-407-optin-hnsw");
    let storage_hnsw = seed_storage(&dir_hnsw);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core_hnsw = EngineCore::from_storage_with_engine(storage_hnsw, kind);
    assert_eq!(core_hnsw.search_engine_kind(), Some(kind));

    let dir_default = TempDir::new("hnsw-407-optin-default");
    let storage_default = seed_storage(&dir_default);
    let core_default = EngineCore::from_storage(storage_default, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    // タイブレーク（id=5 と id=6 の同点）を Top-k 境界に含めるクエリ。
    let query = [1.0, 1.0, 0.0];

    let hits_hnsw = core_hnsw
        .search(&ctx, "docs", &query, 6)
        .expect("hnsw opt-in search");
    let hits_default = core_default
        .search(&ctx, "docs", &query, 6)
        .expect("default engine search");

    assert_eq!(hits_hnsw, hits_default);
}

// Issue #407: `open_with_engine` を通した opt-in 構築でも `search_engine_kind()` が
// 正しく反映されることを、`from_storage_with_engine` とは別のコンストラクタ経路で
// 確認する。
#[test]
fn hnsw_407_open_with_engine_records_kind() {
    let dir = TempDir::new("hnsw-407-open-with-engine");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::open_with_engine(dir.db_path(), kind).expect("valid hnsw params");
    assert_eq!(core.search_engine_kind(), Some(kind));
}

// Issue #407・PR #433 追記: 不正な HNSW パラメータ（`m=1`）は
// `SearchEngineKind::Hnsw` へすら到達できず、唯一の検証入口である
// `search_engine::hnsw_kind` の時点で fail-closed に拒否されることを固定する
// （codex-review P1 指摘。`open_with_engine`／`from_storage_with_engine` は
// 検証済みの `SearchEngineKind` しか受け取れないため、ここで拒否された `kind` を
// 渡すコード自体がコンパイルできない）。
#[test]
fn hnsw_407_invalid_params_rejected_fail_closed() {
    let invalid = engine::hnsw::HnswParams::default().with_m(1);

    assert!(
        search_engine::hnsw_kind(invalid).is_err(),
        "invalid m=1 must be rejected"
    );
}
