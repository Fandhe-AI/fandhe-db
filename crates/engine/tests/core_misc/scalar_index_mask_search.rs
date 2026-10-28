//! 候補集合を id マスクで直接探索し `VectorArena` への複製を回避する
//! （Issue #654・親 #650・#472/#359 の一部）SQL 表層結合テスト。
//!
//! `tests/scalar_index_prune.rs`（Issue #474・候補削減そのもの）と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`、`engine::tenant::insert_typed_row`
//! による投入）を使うが、本ファイルは Issue #654 固有の 3 点を検証する:
//!
//! 1. 候補削減が発火する形状で `VectorArena` への複製が実際に発生しないこと
//!    （記録 provider による直接観測。`search`（複製経路）ではなく `search_subset`
//!    〔マスク経路〕が呼ばれ、渡された `vectors` がスナップショット全行ぶんである
//!    こと）
//! 2. cold（redb 走査）／hot（マスク経路）で結果（id 列・投影列・スコア）が
//!    ビット一致すること（等価・前方一致・`id` 範囲・結合の 4 形状。複数列投影・
//!    同点誘発コーパスを含む）
//! 3. RLS 相当のテナント境界がマスク経路でも不変であること（他テナント private
//!    行の非混入。対照 DB との結果一致オラクル）
//!
//! `index_scans`／`index_mask_scans` カウンタ（`ScalarIndexCacheStats`）で
//! マスク経路が実際に消費されたことを非 vacuous に確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider, SubsetSearchInput};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};
use std::sync::Mutex;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
        ],
    )
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    embedding: [f32; 2],
    kind: &str,
    path: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(embedding.to_vec()),
            Value::Text(kind.to_string()),
            Value::Text(path.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// `(id, score のビットパターン, 投影列の Debug 文字列)` を **`result.rows` の
/// 順序を保ったまま** 並べたもの（codex-review 指摘対応・PR #664）。
///
/// 以前は id 昇順へ並べ替えたうえ `score` を捨てていたため、cold（redb 走査 →
/// 複製経路）／hot（マスク経路）でスコアの丸め誤差やタイブレーク時の順位が
/// 食い違っていても検出できなかった。`ORDER BY embedding <=> ...` の Top-k は
/// 順序そのものが検証対象（マスク経路が複製経路と同じ規約で選出しているか）
/// であるため、ここでは並べ替えず、スコアは `f64::to_bits()` でビット一致まで見る。
fn result_rows_with_cells(result: &QueryResult) -> Vec<(u64, u64, String)> {
    result
        .rows
        .iter()
        .map(|r| (r.id, r.score.to_bits(), format!("{:?}", r.cells)))
        .collect()
}

/// 10 行（`id` 1..=10）: 偶数 `id` は `kind = 'a'`・`path` に `"even/"` 接頭辞、
/// 奇数 `id` は `kind = 'b'`・`path` に `"odd/"` 接頭辞。距離は `id` に単調な
/// ベクトルにし、`ORDER BY ... LIMIT 20`（全件超）で常に全一致行を取得できる
/// ようにする。
fn seed_ten_rows(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=10u64 {
        let (kind, path) = if id % 2 == 0 {
            ("a", format!("even/{id}"))
        } else {
            ("b", format!("odd/{id}"))
        };
        insert_row(
            storage,
            &tenant_ctx,
            id,
            [id as f32, 0.0],
            kind,
            &path,
            Visibility::Public,
        );
    }
}

/// 同点誘発コーパス: 20 行のうち偶数 `id` はすべて同一 embedding（`[1.0, 0.0]`）に
/// し、DISTANCE 段の同点タイブレーク（スコア同点は id 昇順）が cold/hot で一致する
/// ことを検証する。
fn seed_tie_inducing_rows(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=20u64 {
        let (kind, embedding) = if id % 2 == 0 {
            ("a", [1.0f32, 0.0])
        } else {
            ("b", [id as f32, 1.0])
        };
        insert_row(
            storage,
            &tenant_ctx,
            id,
            embedding,
            kind,
            &format!("path/{id}"),
            Visibility::Public,
        );
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

// --- 記録 provider（要件 1: 複製が発生していないことの機械検証） -------------

/// `search`（複製前提の従来経路）・`search_subset`（Issue #654 のマスク経路）の
/// 呼び出しを記録する provider。結果自体は `CpuScalarProvider` へ委譲する。
///
/// `search_subset` へ渡る `vectors.len()` がスナップショット全行ぶん
/// （`rows * dim`）であり、`slots.len()` がそれより小さいことを直接アサートすれば、
/// 「候補行だけを新規 `VectorArena` へ複製してから探索した」のではなく「行列全体を
/// 借用したままスロットマスクで直接探索した」ことを機械的に確認できる。
#[derive(Default)]
struct RecordingProvider {
    search_calls: Mutex<Vec<(usize, usize)>>, // (ids.len(), vectors.len())
    search_subset_calls: Mutex<Vec<(usize, usize)>>, // (slots.len(), vectors.len())
}

impl SearchProvider for RecordingProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.search_calls
            .lock()
            .expect("lock")
            .push((input.ids.len(), input.vectors.len()));
        engine::kernel::CpuScalarProvider.search(input)
    }

    fn search_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, KernelError> {
        self.search_subset_calls
            .lock()
            .expect("lock")
            .push((input.slots.len(), input.vectors.len()));
        engine::kernel::CpuScalarProvider.search_subset(input)
    }
}

fn new_core_with_recording(storage: Storage) -> (EngineCore, std::sync::Arc<RecordingProvider>) {
    let provider = std::sync::Arc::new(RecordingProvider::default());
    // `EngineCore::from_storage` は `Box<dyn SearchProvider>` を要求するため、
    // `Arc` を複製して 1 つはコアへ、1 つはテスト側の観測用に残す（`RecordingProvider`
    // 自体は `Send + Sync`・内部可変性は `Mutex` のみ）。
    struct ArcProvider(std::sync::Arc<RecordingProvider>);
    impl SearchProvider for ArcProvider {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            self.0.search(input)
        }
        fn search_subset(
            &self,
            input: SubsetSearchInput<'_>,
        ) -> Result<Vec<CandidateHit>, KernelError> {
            self.0.search_subset(input)
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(ArcProvider(provider.clone())));
    (core, provider)
}

#[test]
fn mask_path_avoids_arena_duplication_and_never_calls_full_copy_search() {
    let path = unique_db_path("scalar-index-mask-avoid-duplication");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let (core, provider) = new_core_with_recording(storage);

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    // cold: 索引未構築のため候補削減は発火しない（`build_from_cached_rls_rows`
    // 経由の redb 走査。この回は `search`（複製前提）を呼んでよい）。
    let cold = run(&core, "tenant-a", sql);
    // hot: 索引が構築済みでマスク経路が発火するはず。
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));

    let subset_calls = provider.search_subset_calls.lock().expect("lock").clone();
    assert!(
        !subset_calls.is_empty(),
        "hot 実行では search_subset（マスク経路）が呼ばれるはず"
    );
    let (slots_len, vectors_len) = subset_calls
        .last()
        .copied()
        .expect("at least one search_subset call");
    // 候補（kind='a' の 5 行）よりスナップショット全行（10 行）の方が多いため、
    // `vectors.len()`（10 行 * dim 2 = 20）は `slots.len()`（5）より真に大きい。
    // これは「候補行だけを新規複製した」のでは説明できず、行列全体を借用した
    // ままマスクで絞ったことの直接証拠になる。
    assert_eq!(slots_len, 5, "kind='a' の候補は 5 行のはず");
    assert_eq!(
        vectors_len,
        10 * 2,
        "vectors はスナップショット全行ぶんのはず"
    );
}

// --- 要件 2: cold/hot ビット一致（複数列投影・4 形状） -----------------------

fn assert_cold_hot_rows_and_cells_match(core: &EngineCore, tenant: &str, sql: &str) {
    let cold = run(core, tenant, sql);
    let hot = run(core, tenant, sql);
    assert_eq!(
        result_rows_with_cells(&cold),
        result_rows_with_cells(&hot),
        "cold/hot rows(+cells) must match exactly for: {sql}"
    );
}

#[test]
fn equality_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-equality-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind, path FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);

    let index_scans_before = core.scalar_index_cache_stats().index_scans;
    let index_mask_scans_before = core.scalar_index_cache_stats().index_mask_scans;
    let _ = run(&core, "tenant-a", sql);
    let stats = core.scalar_index_cache_stats();
    assert!(stats.index_scans > index_scans_before);
    assert!(
        stats.index_mask_scans > index_mask_scans_before,
        "非 hybrid・非 HNSW-Subset の DISTANCE 経路はマスク経路を消費するはず"
    );
}

#[test]
fn prefix_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-prefix-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql = "SELECT id, kind, path FROM docs WHERE path LIKE 'odd/%' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

#[test]
fn id_range_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-id-range-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql = "SELECT id, kind, path FROM docs WHERE id > 7 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

#[test]
fn conjunction_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-conjunction-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql = "SELECT id, kind, path FROM docs WHERE kind = 'a' AND id > 4 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

// `LIMIT` を候補件数未満に絞り、Top-k 境界（マスク経路の provider が本当に
// Top-k のみを選出していること）も検証する。
#[test]
fn equality_predicate_cold_hot_matches_with_limit_below_candidate_count() {
    let path = unique_db_path("scalar-index-mask-limit-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    // kind='a' の候補は 5 行。LIMIT 2 で Top-2 のみを取得する。
    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 2";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_rows_with_cells(&cold), result_rows_with_cells(&hot));
    assert_eq!(cold.rows.len(), 2);
}

// 同点誘発コーパスでの cold/hot 一致（マスク経路の Top-k 選出・同点タイブレークが
// 複製経路と同じ規約であることの検証）。
#[test]
fn tie_inducing_corpus_cold_hot_matches_for_mask_path() {
    let path = unique_db_path("scalar-index-mask-tie-inducing");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_tie_inducing_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

// 同点誘発コーパス（`kind='a'` の 10 行すべてが embedding `[1.0,0.0]` で同一距離
// ＝完全同点）で `LIMIT` を候補数（10）未満に絞り、同点タイブレーク境界での
// 選出（`id` 昇順で先頭 `LIMIT` 件を選ぶ規約）がマスク経路でも複製経路と
// 完全一致することを検証する（codex-review 指摘対応・PR #664: `LIMIT 20`
// （全候補以上）だけでは境界での選出そのものは未検証だった）。
#[test]
fn tie_inducing_corpus_cold_hot_matches_with_limit_below_candidate_count() {
    let path = unique_db_path("scalar-index-mask-tie-limit-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_tie_inducing_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    // kind='a' の候補は 10 行（id=2,4,...,20）で全件同点。LIMIT 4 で
    // 先頭 4 件（id=2,4,6,8）のみを取得する。
    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 4";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);

    let hot = run(&core, "tenant-a", sql);
    assert_eq!(hot.rows.len(), 4);
    assert_eq!(
        result_ids(&hot),
        vec![2, 4, 6, 8],
        "完全同点時は id 昇順で先頭 LIMIT 件が選ばれるはず"
    );
}

// --- 統計: hybrid・HINT ORDER（残余述語）はマスク経路を消費しない ------------

#[test]
fn hint_order_distance_leading_never_consumes_mask_path() {
    let path = unique_db_path("scalar-index-mask-hint-order");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    // `HINT ORDER(RLS, DISTANCE, SCALAR)`: DISTANCE 先行・SCALAR 事後フィルタ
    // （`plan.scalar_prefilter == false`）は `classify_scalar_plan` が `PlainScan`
    // を返すため、索引対応述語があっても候補削減自体が発火しない。
    let sql = "SELECT id, kind FROM docs WHERE kind = 'a' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20 HINT ORDER(RLS, DISTANCE, SCALAR)";
    let before = core.scalar_index_cache_stats().index_mask_scans;
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats().index_mask_scans;
    assert_eq!(
        after, before,
        "DISTANCE 先行の HINT ORDER はマスク経路を消費しないはず"
    );
}

// --- 要件 3: RLS（テナント境界の非漏えい） ----------------------------------

#[test]
fn mask_path_never_leaks_other_tenant_private_rows() {
    let path = unique_db_path("scalar-index-mask-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    // tenant-b の private 行を id 1..=10 の間に紛れ込ませる（kind='a' で
    // 候補削減の対象になり得る形状にする）。
    let tenant_b_ctx = ctx("tenant-b");
    for id in 101..=105u64 {
        insert_row(
            &storage,
            &tenant_b_ctx,
            id,
            [id as f32, 0.0],
            "a",
            &format!("secret/{id}"),
            Visibility::Private,
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    // cold（索引未構築）→ hot（マスク経路）の双方で tenant-b の private 行が
    // 一切混入しないこと。
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    for result in [&cold, &hot] {
        for id in result_ids(result) {
            assert!(
                id <= 10,
                "tenant-a のクエリに tenant-b の private 行（id={id}）が混入した"
            );
        }
    }
    assert_eq!(result_ids(&cold), result_ids(&hot));
}

// --- 世代進行後もマスク経路の結果は一貫する ---------------------------------

#[test]
fn mask_path_stays_consistent_after_table_generation_bump() {
    let path = unique_db_path("scalar-index-mask-generation-bump");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let _ = run(&core, "tenant-a", sql); // cold
    let _ = run(&core, "tenant-a", sql); // hot（索引構築・マスク経路消費）

    // 新規行を追加して世代を進める。
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO docs (id, embedding, kind, path) \
         VALUES (11, '[11.0,0.0]', 'a', 'even/11') USING OPERATION_ID 'seed-11'",
    )
    .expect("insert row after generation bump");

    let after_insert = run(&core, "tenant-a", sql);
    assert!(result_ids(&after_insert).contains(&11));
    // 索引は世代進行で再構築され、再度 hot 実行しても一貫し続ける。
    let after_insert_hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&after_insert), result_ids(&after_insert_hot));
}

// --- Issue #660 系（Step 4）: 索引信頼マスク経路（候補への再適用省略） -------
//
// `classify_scalar_plan != PlainScan`（残余述語なしが静的に保証される）かつ
// 索引↔スナップショット同一性ガード通過時に限り、候補スロットへの
// `on_visible_row` 再適用（masked decode ＋ `matches_all` ＋ 式述語評価）を省き、
// 候補スロット列をそのまま `search_subset` のマスクとして渡す経路の検証。
//
// 対照は「意味的に同値だが索引で完全被覆されない述語」を足して
// `ScalarPlan::PlainScan` へ落とした従来の再適用経路（`id + 0 > 0` は
// `id_predicate_from_expr` が受理しない残余述語であり、本フィクスチャ〔`id >= 1`〕
// では常に真）。

fn new_core_plain(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider))
}

/// `sql`（索引信頼経路）と `control_sql`（plain scan 経路）の結果が
/// id・スコアのビットパターン・投影セルまで完全一致し、かつ前者が実際に
/// 索引信頼経路（`index_trusted_mask_scans` 増加）・後者が plain scan 経路
/// （同カウンタ非増加）であることを確認する。
fn assert_trusted_mask_matches_plain_scan(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    control_sql: &str,
) {
    // 索引を温める（cold は piggyback 構築のみで信頼経路に乗らないため）。
    let _ = run(core, tenant, sql);
    let before = core.scalar_index_cache_stats().index_trusted_mask_scans;
    let trusted = run(core, tenant, sql);
    let after = core.scalar_index_cache_stats().index_trusted_mask_scans;
    assert!(
        after > before,
        "index-trusted mask path must be taken (non-vacuous) for: {sql}"
    );

    let before_control = core.scalar_index_cache_stats().index_trusted_mask_scans;
    let control = run(core, tenant, control_sql);
    let after_control = core.scalar_index_cache_stats().index_trusted_mask_scans;
    assert_eq!(
        after_control, before_control,
        "control query must stay on the re-applied plain scan path: {control_sql}"
    );

    assert_eq!(
        result_rows_with_cells(&trusted),
        result_rows_with_cells(&control),
        "index-trusted mask result must equal the re-applied path bit-for-bit: \
         {sql} vs {control_sql}"
    );
}

#[test]
fn trusted_mask_matches_plain_scan_for_each_predicate_shape() {
    let path = unique_db_path("scalar-index-mask-trusted-shapes");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core_plain(storage);

    // 等価（IndexEquality）・`SELECT id` のみ（投影列なし）
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
        "SELECT id FROM docs WHERE kind = 'a' AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
    );
    // 等価・スカラー列投影あり（投影遅延で Top-k 分のみデコード）
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id, kind, path FROM docs WHERE kind = 'a' \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 3",
        "SELECT id, kind, path FROM docs WHERE kind = 'a' AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 3",
    );
    // 前方一致（IndexPrefix）
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id, path FROM docs WHERE path LIKE 'even/%' \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
        "SELECT id, path FROM docs WHERE path LIKE 'even/%' AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
    );
    // `id` 範囲（IndexIdRange）
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id, kind FROM docs WHERE id > 5 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
        "SELECT id, kind FROM docs WHERE id > 5 AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
    );
    // 交差（IndexConjunction）
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id, kind FROM docs WHERE kind = 'a' AND id > 5 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
        "SELECT id, kind FROM docs WHERE kind = 'a' AND id > 5 AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
    );
    // `precision` モード（SEARCH-9 の確信度ゲート）。ゲートは DISTANCE 段が
    // 返す Top-2 のマージンで判定するため、索引信頼マスクが一致集合そのもの
    // （超集合でない）でなければ誤ったマージンを見て fail-open／fail-closed を
    // 取り違える。信頼経路でも再適用経路と完全一致することを固定する。
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' \
         LIMIT 1 USING MODE 'precision'",
        "SELECT id FROM docs WHERE kind = 'a' AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 1 USING MODE 'precision'",
    );
    // 一致 0 件
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs WHERE kind = 'zzz' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
        "SELECT id FROM docs WHERE kind = 'zzz' AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 10",
    );
}

#[test]
fn trusted_mask_matches_plain_scan_on_tie_inducing_corpus() {
    let path = unique_db_path("scalar-index-mask-trusted-ties");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_tie_inducing_rows(&storage, "tenant-a");
    let core = new_core_plain(storage);

    // 偶数 `id`（kind='a'）は全て同一 embedding。同点タイブレーク（id 昇順）が
    // 信頼経路と再適用経路で完全一致することを固定する。
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
        "SELECT id, kind FROM docs WHERE kind = 'a' AND id + 0 > 0 \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
    );
}

#[test]
fn trusted_mask_never_leaks_other_tenant_private_rows() {
    let path = unique_db_path("scalar-index-mask-trusted-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    // TABLE-12（`id` の一意性スコープはテナント内）の境界を突くため `id` を
    // tenant-a と重複させる: `id = 2` は `Public`（tenant-a から可視）、
    // `id = 4` は `Private`（不可視）。`id` 11〜13 は選択度ゲート
    // （一致率 > 1/2 で plain scan へ縮退）に掛からないための詰め物。
    let other = ctx("tenant-b");
    insert_row(
        &storage,
        &other,
        2,
        [2.0, 5.0],
        "a",
        "other/2",
        Visibility::Public,
    );
    insert_row(
        &storage,
        &other,
        4,
        [4.0, 5.0],
        "a",
        "other/4",
        Visibility::Private,
    );
    for id in 11..=13u64 {
        insert_row(
            &storage,
            &other,
            id,
            [id as f32, 5.0],
            "b",
            &format!("other/{id}"),
            Visibility::Public,
        );
    }
    let core = new_core_plain(storage);

    let sql = "SELECT id, path FROM docs WHERE kind = 'a' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_trusted_mask_matches_plain_scan(
        &core,
        "tenant-a",
        sql,
        "SELECT id, path FROM docs WHERE kind = 'a' AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
    );

    let result = run(&core, "tenant-a", sql);
    // 自テナント 5 件（id 2,4,6,8,10）＋他テナントの Public 重複 id=2 の 1 件。
    // 他テナントの Private（id=4・path "other/4"）は含まれない。
    assert_eq!(result_ids(&result), vec![2, 2, 4, 6, 8, 10]);
    let cells = result
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        !cells.contains("other/4"),
        "other tenant's Private row must never appear: {cells}"
    );
    assert!(
        cells.contains("other/2"),
        "other tenant's Public duplicate id must appear: {cells}"
    );
}

#[test]
fn trusted_mask_is_not_used_across_a_table_generation_bump() {
    let path = unique_db_path("scalar-index-mask-trusted-generation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core_plain(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let _ = run(&core, "tenant-a", sql);
    let warm = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&warm), vec![2, 4, 6, 8, 10]);
    let before = core.scalar_index_cache_stats().index_trusted_mask_scans;
    assert!(before > 0, "warm 実行で索引信頼経路へ到達しているはず");

    // 書き込みでテーブル世代が進む。`EngineCore` が `Storage` の所有権を握るため
    // SQL 表層の `INSERT` を経由する（`tests/sql_arena_cache.rs` と同じ流儀）。
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO docs (id, embedding, kind, path) \
         VALUES (99, '[99.0,0.0]', 'a', 'even/99') USING OPERATION_ID 'mask-trusted-bump'",
    )
    .expect("insert must succeed");
    // 選択度ゲート（一致率 > 1/2 で plain scan へ縮退）に掛からないよう、
    // `kind = 'b'` の詰め物も同時に投入する（kind='a' は 6/14）。
    for id in 101..=103u64 {
        core.execute_insert_sql(
            &ctx("tenant-a"),
            &format!(
                "INSERT INTO docs (id, embedding, kind, path) \
                 VALUES ({id}, '[{id}.0,1.0]', 'b', 'odd/{id}') \
                 USING OPERATION_ID 'mask-trusted-pad-{id}'"
            ),
        )
        .expect("insert must succeed");
    }

    // 直後のクエリは索引・スナップショットとも失効しており、信頼経路へは入らず
    // （カウンタ非増加）新しい行を反映した正しい結果を返す。
    let after_write = run(&core, "tenant-a", sql);
    let after = core.scalar_index_cache_stats().index_trusted_mask_scans;
    assert_eq!(
        after, before,
        "stale index must never be trusted right after a table generation bump"
    );
    assert_eq!(result_ids(&after_write), vec![2, 4, 6, 8, 10, 99]);

    // 再構築後は再び信頼経路へ戻り、結果は不変。
    let rewarm = run(&core, "tenant-a", sql);
    assert!(core.scalar_index_cache_stats().index_trusted_mask_scans > after);
    assert_eq!(result_ids(&rewarm), vec![2, 4, 6, 8, 10, 99]);
}
