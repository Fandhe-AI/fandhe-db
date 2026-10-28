//! `VectorCore` / `PolicyContext` / provider 注入の結合テスト（TASK-124）。
//! 対象ビヘイビア: CORE-1（プロトコル非依存のコア API）・CORE-2（テナント境界の
//! 単一照合パス）・CORE-13（実行バックエンド provider 注入）。境界系（次元・k 検証）も含む。
//!
//! シード手法（codex P0-2・Issue #137 対応）: `EngineCore` はテナント境界を迂回する
//! 生ハンドルへの経路（旧 `storage()` アクセサ・`test-support` feature）を公開しない
//! （`crates/engine/src/core.rs` のドキュメント参照）。そのため本ファイルのテストは、
//! `engine::storage::Storage::open` で直接開いた `Storage` にテーブル作成・行投入を
//! 済ませてから、その所有権ごと [`EngineCore::from_storage`] へ渡して `EngineCore` を
//! 構築する（構築後は `EngineCore` から生 `Storage` へ戻る経路がない）。

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use engine::catalog::CatalogError;
use engine::core::{CoreError, EngineCore, VectorCore};
use engine::kernel::{
    CandidateHit, CpuScalarProvider, KernelError, SearchHit, SearchInput, SearchProvider,
};
use engine::policy::PolicyContext;
use engine::storage::{RowInput, Storage, Visibility};

// 一時ディレクトリ（`TempDir`）は Issue #173 で `crates/engine/src/test_util/temp_db.rs`
// へ一本化した。
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

fn seed_row(
    storage: &Storage,
    table: &str,
    id: u64,
    tenant: &str,
    visibility: Visibility,
    embedding: &[f32],
) {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容ハッシュを
    // 持つため、同一テナント・同一テーブル内で内容の異なる複数行へ同一
    // operation_id を使い回すと 2 件目以降が OperationIdContentMismatch で拒否される。
    // 行ごとに一意の operation_id を使う。
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
            visibility,
            embedding,
            metadata: &[],
        },
        &op_id,
    )
    .expect("seed row");
}

// 対象ビヘイビア: CORE-1。`&dyn VectorCore` のみを介する 2 種類の「模擬プロトコル
// アダプタ」が同一結果を得ることを検証する（コア API がプロトコル非依存であること）。
fn _assert_object_safe(_: &dyn VectorCore) {}

/// 組み込み呼び出し風アダプタ（プロトコルフレーミングなしで直接 trait を呼ぶ想定）。
fn embedded_adapter_search(
    core: &dyn VectorCore,
    ctx: &PolicyContext,
    table: &str,
    query: &[f32],
    k: usize,
) -> Vec<SearchHit> {
    core.search(ctx, table, query, k)
        .expect("embedded adapter search")
}

/// リクエスト/レスポンス変換風アダプタ（wire プロトコル層を模し、ID のみのバイト列を
/// 経由させてから復元する体で `VectorCore` を呼ぶ）。
fn wire_like_adapter_search(
    core: &dyn VectorCore,
    ctx: &PolicyContext,
    table: &str,
    query: &[f32],
    k: usize,
) -> Vec<u64> {
    let hits = core
        .search(ctx, table, query, k)
        .expect("wire-like adapter search");
    hits.into_iter().map(|h| h.id).collect()
}

#[test]
fn core1_two_protocol_adapters_agree_via_object_safe_trait() {
    let dir = TempDir::new("core1");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    seed_row(
        &storage,
        "docs",
        2,
        "tenant-a",
        Visibility::Public,
        &[0.0, 1.0],
    );
    seed_row(
        &storage,
        "docs",
        3,
        "tenant-a",
        Visibility::Public,
        &[2.0, 0.0],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let core_ref: &dyn VectorCore = &core;

    let embedded = embedded_adapter_search(core_ref, &ctx, "docs", &[1.0, 0.0], 2);
    let wire_ids = wire_like_adapter_search(core_ref, &ctx, "docs", &[1.0, 0.0], 2);

    assert_eq!(embedded.iter().map(|h| h.id).collect::<Vec<_>>(), wire_ids);
    assert_eq!(wire_ids, vec![3, 1]);
}

/// 実装から独立した検査器: `EngineCore`（≒ `core.rs` の判定ロジック）を経由せず、
/// 検索完了後に再度開いた `Storage` から直接行を読み直してテナントを再計算する
/// （CORE-2）。`redb` はプロセス内で同一ファイルへの多重 `Database` を許容しないため、
/// 呼び出し元は本関数を呼ぶ前に検証対象の `EngineCore`（`Storage` を内部所有する）を
/// drop 済みにしておくこと（codex P0-2・Issue #137 対応: `EngineCore` から生 `Storage`
/// を取り出す経路は公開しないため、独立検証はパスを再オープンする形にする）。
fn assert_no_cross_tenant_leak(
    path: &Path,
    table: &str,
    expected_tenant: &str,
    hits: &[SearchHit],
) {
    let storage = Storage::open(path).expect("reopen storage for independent verification");
    for hit in hits {
        // 行ストアの物理キーは `(tenant_id, id)`（対象ビヘイビア: TABLE-12）。
        // 期待テナントの名前空間で引けること自体が「返された hit が当該テナントの行で
        // ある」ことの独立検証になり、名前空間外の行なら取得自体が失敗する。
        let row = storage
            .get_row_from_table(table, expected_tenant, hit.id)
            .unwrap_or_else(|_| {
                panic!(
                    "cross-tenant leak detected: hit id={} is not a row of tenant={expected_tenant}",
                    hit.id
                )
            });
        assert_eq!(
            row.tenant_id, expected_tenant,
            "cross-tenant leak detected: hit id={} belongs to tenant={}",
            hit.id, row.tenant_id
        );
    }
}

// 越境しないことの検証は `Private` 行で行う（ポインタ: TASK-89 / TABLE-9）。
#[test]
fn core2_multi_tenant_search_has_zero_cross_tenant_leakage() {
    let dir = TempDir::new("core2-leak");
    let path = dir.db_path();

    let hits = {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // tenant-a と tenant-b を混在させ、tenant-b 側に最高スコアの行を置く
        // （マスクが効いていなければ tenant-a の検索結果に混入する）。
        seed_row(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        seed_row(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        seed_row(
            &storage,
            "docs",
            3,
            "tenant-b",
            Visibility::Private,
            &[100.0, 0.0],
        );
        seed_row(
            &storage,
            "docs",
            4,
            "tenant-b",
            Visibility::Private,
            &[50.0, 0.0],
        );
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        core.search(&ctx_a, "docs", &[1.0, 0.0], 10)
            .expect("search ok")
        // `core`（内部の `Storage` を含む）はここで drop され、db ファイルが解放される。
    };

    assert!(!hits.is_empty());
    assert_no_cross_tenant_leak(&path, "docs", "tenant-a", &hits);
    assert!(hits.iter().all(|h| h.id == 1 || h.id == 2));
}

// 不可視であることを前提にしたこの検証は `Private` 行で行う
// （ポインタ: TASK-89 / TABLE-9）。
#[test]
fn core2_get_row_does_not_distinguish_invisible_from_missing() {
    let dir = TempDir::new("core2-getrow");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-b",
        Visibility::Private,
        &[1.0, 0.0],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    // 点取得のキーは `(tenant_id, id)`（対象ビヘイビア: TABLE-12）。他テナントの
    // `Private` 行（存在するが不可視）と、存在しない行キーの双方が `NotFound` に
    // 統一されること＝応答から他テナント行の存在を判別できないことを確認する（RLS-9）。
    let other_tenant_row = core.get_row(&ctx_a, "docs", "tenant-b", 1);
    let missing_row = core.get_row(&ctx_a, "docs", "tenant-a", 999);
    let missing_foreign_row = core.get_row(&ctx_a, "docs", "tenant-b", 999);
    assert_eq!(
        format!("{:?}", other_tenant_row.as_ref().err()),
        format!("{:?}", missing_foreign_row.as_ref().err()),
        "an invisible foreign row and a nonexistent foreign row must be indistinguishable"
    );
    assert!(matches!(missing_foreign_row, Err(CoreError::NotFound)));

    assert!(matches!(other_tenant_row, Err(CoreError::NotFound)));
    assert!(matches!(missing_row, Err(CoreError::NotFound)));
}

// レビュー指摘対応（Medium・Issue #32）: `get_row` はテーブル不存在・行不存在のみを
// `CoreError::NotFound` に丸め込み、それ以外（デコード不正等のデータ破損）は
// `CoreError::Catalog` としてそのまま伝播しなければならない。`Storage`/`EngineCore` の
// 公開 API では意図的な破損データを作れないため、`tests/persistence.rs` と同じ手法で
// テーブル固有の行テーブルへ `redb` を直接操作して不正なバイト列を書き込む。
#[test]
fn get_row_surfaces_data_corruption_distinctly_from_not_found() {
    let dir = TempDir::new("core-corrupt");
    let path = dir.db_path();
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // `Storage` を閉じてから raw `redb::Database` で同一ファイルを再度開く
        // （redb はプロセス内で同一ファイルへの多重 `Database` を許容しない）。
    }
    {
        let db = redb::Database::create(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            // `catalog.rs::user_rows_table_name` と同一の命名規則（`user_rows/<table>`）。
            // 物理キーは `(tenant_id, id)`（対象ビヘイビア: TABLE-12。`catalog.rs::
            // user_rows_table_def` と同一のキー型。旧 `u64` 単独キーで書くと、
            // engine 側が `IncompatibleRowKeyFormat` を返して本テストが検証したい
            // 行デコード経路へ到達しなくなる＝テストが空振りする）。
            let row_table_def: redb::TableDefinition<(&str, u64), &[u8]> =
                redb::TableDefinition::new("user_rows/docs");
            let mut table = write_txn.open_table(row_table_def).expect("open row table");
            // version バイトのみで後続フィールドが一切ない、意図的な破損バイト列。
            table
                .insert(("tenant-a", 1u64), &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let core = EngineCore::open(&path).expect("reopen engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .get_row(&ctx, "docs", "tenant-a", 1)
        .expect_err("corrupted row must not be silently reported as NotFound");
    // 行データのデコード失敗（`CatalogError::Invalid`）であることまで確認する。
    // 単に `Catalog(_)` で受けると、行テーブルのキー型不一致
    // （`CatalogError::IncompatibleRowKeyFormat`。TABLE-12 の物理キー変更に伴う
    // 旧フォーマット拒否）でも通過してしまい、本テストが空振りする。
    assert!(
        matches!(err, CoreError::Catalog(CatalogError::Invalid(_))),
        "expected CoreError::Catalog(Invalid) (row decode failure surfaced distinctly), got: {err}"
    );
}

/// P0-1 検証用の計装 provider（Issue #137）: `SearchInput` に渡された `ids`・
/// `vectors` の長さをそのまま記録してから `CpuScalarProvider` へ委譲する。かつて
/// `MaskIgnoringProvider`（`is_visible` クロージャを無視して全行を候補にする provider）
/// が実証していた「provider がマスクを無視すれば他テナント行を読める」という経路は、
/// `SearchInput` 自体に不可視行のデータを含めない設計（`kernel.rs` のドキュメント参照）
/// へ変更したことで構造的に塞がれた。本 provider は「コアが実際に可視行だけへ絞り込んだ
/// 縮約ビューを渡していること」を provider 側の観測で裏付ける。
struct CapturingProvider {
    captured_ids: Arc<Mutex<Vec<u64>>>,
    captured_vector_len: Arc<Mutex<usize>>,
}

impl SearchProvider for CapturingProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        *self.captured_ids.lock().expect("lock captured_ids") = input.ids.to_vec();
        *self
            .captured_vector_len
            .lock()
            .expect("lock captured_vector_len") = input.vectors.len();
        CpuScalarProvider.search(input)
    }
}

// レビュー指摘対応（codex P0-1・Issue #137）: `EngineCore::search` が provider へ
// 渡す `SearchInput` に、他テナントの不可視行の id・ベクトルが一切含まれないこと
// （＝可視行だけへ絞り込んだ縮約ビューであること）を、provider 側の観測で検証する。
// dim=2・tenant-a の可視行が 1 件のみのデータセットに対し、`ids` が他テナント
// （id=2, tenant-b）を含まず、`vectors` の長さが可視行数 × dim（1 * 2 = 2）に
// 一致することを確認する。id=2 を不可視のまま保つには `Private` を使う
// （ポインタ: TASK-89 / TABLE-9）。
#[test]
fn search_projects_input_to_visible_rows_only_before_calling_provider() {
    let dir = TempDir::new("p0-1-projection");
    let captured_ids = Arc::new(Mutex::new(Vec::new()));
    let captured_vector_len = Arc::new(Mutex::new(0usize));
    let provider = CapturingProvider {
        captured_ids: Arc::clone(&captured_ids),
        captured_vector_len: Arc::clone(&captured_vector_len),
    };

    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    seed_row(
        &storage,
        "docs",
        2,
        "tenant-b",
        Visibility::Private,
        &[100.0, 0.0],
    );
    let core = EngineCore::from_storage(storage, Box::new(provider));

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let hits = core
        .search(&ctx_a, "docs", &[1.0, 0.0], 10)
        .expect("search ok");

    let ids = captured_ids.lock().expect("lock captured_ids");
    // provider が観測する識別子は候補アリーナのスロット番号（0 起点）であり、行 `id`
    // ではない（対象ビヘイビア: TABLE-12。行 `id` の一意性スコープがテナント内に閉じた
    // ため、コアはスロット番号を識別子として渡し、テナント修飾済みの行解決は provider の
    // 外側で行う。`kernel.rs::CandidateHit` のドキュメント参照）。可視行は 1 件だけなので
    // 候補も 1 件であり、他テナントの `Private` 行は provider のアドレス空間へ渡らない。
    assert_eq!(
        ids.as_slice(),
        &[0u64],
        "provider must observe exactly one candidate slot (the caller's own visible row)"
    );
    // 行の同定（テナント・行 id）はコア側が行い、呼び出し元へは `(tenant_id, id)` で返る。
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 1);
    assert_eq!(hits[0].tenant_id, "tenant-a");
    let vector_len = *captured_vector_len
        .lock()
        .expect("lock captured_vector_len");
    assert_eq!(
        vector_len, 2,
        "provider must only observe vectors for the visible rows (1 row * dim 2)"
    );
}

/// negative test 用のダミー provider（codex P0/P1・Issue #137）: モードごとに Top-k
/// 契約（可視集合所属・件数 <= k・id 一意性・スコア有限性・スコア降順＋同点 id 昇順）へ
/// 個別に違反する `SearchHit` 列を返す。`EngineCore::search` のコア側検証が各違反を
/// 個別に検出して拒否できることを確認する。
enum MalformedHitMode {
    /// データセットに存在しない id を捏造する（可視集合所属チェックの違反）。
    Fabricated,
    /// 要求 `k` を超える件数を返す（件数チェックの違反。他は正しい形式にする）。
    TooMany,
    /// 同じ id を複数回返す（一意性チェックの違反。スコア自体は正しい降順にする）。
    Duplicate,
    /// スコアに NaN を含める（有限性チェックの違反）。
    NonFiniteScore,
    /// スコア降順・同点 id 昇順の契約に違反する順序で返す（順序チェックの違反）。
    Misordered,
}

struct MalformedHitProvider(MalformedHitMode);

impl SearchProvider for MalformedHitProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        match self.0 {
            MalformedHitMode::Fabricated => Ok(vec![CandidateHit {
                id: 9_999,
                score: 1.0,
            }]),
            MalformedHitMode::TooMany => {
                // 可視行 2 件（呼び出し側は k=1 で呼ぶ）を両方返し、他の契約は満たす
                // 正しい降順で件数チェックのみを単独で違反させる。
                let mut ids = input.ids.to_vec();
                ids.sort_unstable();
                Ok(ids
                    .into_iter()
                    .rev()
                    .enumerate()
                    .map(|(rank, id)| CandidateHit {
                        id,
                        score: -(rank as f32),
                    })
                    .collect())
            }
            MalformedHitMode::Duplicate => {
                let id = *input
                    .ids
                    .first()
                    .expect("dataset must have at least one visible row");
                Ok(vec![
                    CandidateHit { id, score: 2.0 },
                    CandidateHit { id, score: 1.0 },
                ])
            }
            MalformedHitMode::NonFiniteScore => {
                let id = *input
                    .ids
                    .first()
                    .expect("dataset must have at least one visible row");
                Ok(vec![CandidateHit {
                    id,
                    score: f32::NAN,
                }])
            }
            MalformedHitMode::Misordered => {
                // 可視行 2 件を id 昇順のまま返す。スコアも昇順になるため、
                // 「スコア降順」の契約に違反する。
                let mut ids = input.ids.to_vec();
                ids.sort_unstable();
                Ok(ids
                    .into_iter()
                    .enumerate()
                    .map(|(rank, id)| CandidateHit {
                        id,
                        score: rank as f32,
                    })
                    .collect())
            }
        }
    }
}

fn seed_two_visible_rows(storage: &Storage) {
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    seed_row(
        storage,
        "docs",
        2,
        "tenant-a",
        Visibility::Public,
        &[0.5, 0.0],
    );
}

#[test]
fn provider_returning_a_hit_absent_from_the_dataset_is_rejected() {
    let dir = TempDir::new("provider-fabricated-hit");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    let core = EngineCore::from_storage(
        storage,
        Box::new(MalformedHitProvider(MalformedHitMode::Fabricated)),
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 10)
        .expect_err("provider returning an id absent from the dataset must be rejected");

    assert!(
        matches!(err, CoreError::ProviderResultRejected),
        "expected ProviderResultRejected, got: {err}"
    );
}

// レビュー指摘対応（codex P1・Issue #137）: provider が要求件数 k を超える hit を返した
// 場合、他の契約（id・スコア・順序）が正しくても拒否されること。
#[test]
fn provider_returning_more_hits_than_k_is_rejected() {
    let dir = TempDir::new("provider-too-many-hits");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    seed_two_visible_rows(&storage);
    let core = EngineCore::from_storage(
        storage,
        Box::new(MalformedHitProvider(MalformedHitMode::TooMany)),
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 1)
        .expect_err("provider returning more hits than k must be rejected");

    assert!(
        matches!(err, CoreError::ProviderResultRejected),
        "expected ProviderResultRejected, got: {err}"
    );
}

// レビュー指摘対応（codex P1・Issue #137）: provider が同一 id を複数件返した場合、
// 拒否されること（id の一意性契約）。
#[test]
fn provider_returning_a_duplicate_id_is_rejected() {
    let dir = TempDir::new("provider-duplicate-id");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    seed_two_visible_rows(&storage);
    let core = EngineCore::from_storage(
        storage,
        Box::new(MalformedHitProvider(MalformedHitMode::Duplicate)),
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 10)
        .expect_err("provider returning a duplicate id must be rejected");

    assert!(
        matches!(err, CoreError::ProviderResultRejected),
        "expected ProviderResultRejected, got: {err}"
    );
}

// レビュー指摘対応（codex P1・Issue #137）: provider が非有限（NaN）スコアを返した
// 場合、拒否されること（スコア有限性契約）。
#[test]
fn provider_returning_a_non_finite_score_is_rejected() {
    let dir = TempDir::new("provider-non-finite-score");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    seed_two_visible_rows(&storage);
    let core = EngineCore::from_storage(
        storage,
        Box::new(MalformedHitProvider(MalformedHitMode::NonFiniteScore)),
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 10)
        .expect_err("provider returning a non-finite score must be rejected");

    assert!(
        matches!(err, CoreError::ProviderResultRejected),
        "expected ProviderResultRejected, got: {err}"
    );
}

// レビュー指摘対応（codex P1・Issue #137）: provider がスコア降順・同点 id 昇順の
// 契約に違反する順序で返した場合、拒否されること。
#[test]
fn provider_returning_hits_out_of_score_order_is_rejected() {
    let dir = TempDir::new("provider-misordered-hits");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    seed_two_visible_rows(&storage);
    let core = EngineCore::from_storage(
        storage,
        Box::new(MalformedHitProvider(MalformedHitMode::Misordered)),
    );

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 10)
        .expect_err("provider returning hits out of score order must be rejected");

    assert!(
        matches!(err, CoreError::ProviderResultRejected),
        "expected ProviderResultRejected, got: {err}"
    );
}

/// CORE-13: 呼び出しがカスタム provider を通ることを記録する計装 provider。
struct RecordingProvider {
    calls: Arc<AtomicUsize>,
}

impl SearchProvider for RecordingProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        CpuScalarProvider.search(input)
    }
}

#[test]
fn core13_custom_provider_injection_is_actually_called() {
    let dir = TempDir::new("core13-inject");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = RecordingProvider {
        calls: Arc::clone(&calls),
    };

    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    let core = EngineCore::from_storage(storage, Box::new(provider));

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let _ = core
        .search(&ctx, "docs", &[1.0, 0.0], 1)
        .expect("search ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let _ = core
        .search(&ctx, "docs", &[1.0, 0.0], 1)
        .expect("search ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// 対象ビヘイビア: CORE-13。既定コンストラクタ（`EngineCore::open`）が CPU-only
// provider だけで全機能を成立させることを検証する。他のテストと異なり構築対象の
// 制約（`open` が自らパスを開いて `Storage` を所有する）を保つため、シード用の
// `Storage` は別スコープで開いて閉じてから、検証対象の `EngineCore::open` で
// 同じパスを再度開く。
#[test]
fn core13_default_cpu_only_constructor_supports_full_functionality() {
    let dir = TempDir::new("core13-default");
    let path = dir.db_path();
    {
        let storage = Storage::open(&path).expect("open storage to seed");
        storage
            .create_table(&schema_for("docs", 3))
            .expect("create table");
        seed_row(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0, 0.0],
        );
    }

    let core = EngineCore::open(&path).expect("open engine core (default CPU provider)");

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let hits = core
        .search(&ctx, "docs", &[1.0, 0.0, 0.0], 1)
        .expect("search ok");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 1);

    let row = core
        .get_row(&ctx, "docs", "tenant-a", 1)
        .expect("get_row ok");
    assert_eq!(row.tenant_id, "tenant-a");
}

#[test]
fn boundary_query_dimension_mismatch_is_rejected() {
    let dir = TempDir::new("boundary-dim");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 3))
        .expect("create table");
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0, 0.0],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core.search(&ctx, "docs", &[1.0, 0.0], 1).unwrap_err();
    assert!(matches!(
        err,
        CoreError::Kernel(KernelError::DimMismatch { .. })
    ));
}

#[test]
fn boundary_k_zero_and_k_over_limit_are_rejected() {
    let dir = TempDir::new("boundary-k");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Public,
        &[1.0, 0.0],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    assert!(matches!(
        core.search(&ctx, "docs", &[1.0, 0.0], 0),
        Err(CoreError::InvalidK { k: 0 })
    ));
    assert!(matches!(
        core.search(&ctx, "docs", &[1.0, 0.0], usize::MAX),
        Err(CoreError::InvalidK { .. })
    ));
}

// レビュー指摘対応（Medium 1・Issue #32）: query の次元不一致は
// `VectorArena::build`（対象テーブル全行のデコード・確保）へ進む前に、カタログ照会
// だけで早期拒否されなければならない。これを直接計測せずに検証するため、対象テーブルへ
// `redb` を直接操作してデコード不能な破損行を仕込んでおく。もし早期拒否が働かず
// `VectorArena::build` が実際に全行を走査してしまえば、この破損行のデコードで
// 別種のエラー（`CoreError::Arena`）になるはずで、期待どおり
// `CoreError::Kernel(KernelError::DimMismatch)` が返ることは走査が発生しなかった証拠になる。
#[test]
fn dim_mismatch_is_rejected_before_scanning_table_rows() {
    let dir = TempDir::new("dim-mismatch-early-reject");
    let path = dir.db_path();
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 3))
            .expect("create table");
    }
    {
        let db = redb::Database::create(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            // 物理キーは `(tenant_id, id)`（対象ビヘイビア: TABLE-12。`catalog.rs::
            // user_rows_table_def` と同一のキー型。旧 `u64` 単独キーで書くと、
            // engine 側が `IncompatibleRowKeyFormat` を返して本テストが検証したい
            // 行デコード経路へ到達しなくなる＝テストが空振りする）。
            let row_table_def: redb::TableDefinition<(&str, u64), &[u8]> =
                redb::TableDefinition::new("user_rows/docs");
            let mut table = write_txn.open_table(row_table_def).expect("open row table");
            // version バイトのみで後続フィールドが一切ない、意図的な破損バイト列
            // （`get_row_surfaces_data_corruption_distinctly_from_not_found` と同手法）。
            table
                .insert(("tenant-a", 1u64), &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let core = EngineCore::open(&path).expect("reopen engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // schema の次元（3）と異なる次元（2）のクエリを渡す。
    let err = core
        .search(&ctx, "docs", &[1.0, 0.0], 1)
        .expect_err("dim mismatch must be rejected");
    assert!(
        matches!(
            err,
            CoreError::Kernel(KernelError::DimMismatch {
                expected: 3,
                found: 2
            })
        ),
        "expected early DimMismatch rejection (proving the malformed row was never scanned), got: {err}"
    );
}

// レビュー指摘対応（Cursor Bugbot Medium・Issue #32 #137）: 正しい次元だが非有限
// （NaN・Inf）の要素を含む query も、`dim_mismatch_is_rejected_before_scanning_table_rows`
// と同じ手法（破損行を仕込んだテーブル）で `VectorArena::build` へ進む前に早期拒否
// されなければならない。早期拒否が働かず全行走査してしまえば、この破損行のデコードで
// 別種のエラー（`CoreError::Arena`）になるはずで、期待どおり
// `CoreError::Kernel(KernelError::NonFiniteQuery)` が返ることは走査が発生しなかった証拠になる。
#[test]
fn non_finite_query_is_rejected_before_scanning_table_rows() {
    let dir = TempDir::new("non-finite-early-reject");
    let path = dir.db_path();
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 3))
            .expect("create table");
    }
    {
        let db = redb::Database::create(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            // 物理キーは `(tenant_id, id)`（対象ビヘイビア: TABLE-12。`catalog.rs::
            // user_rows_table_def` と同一のキー型。旧 `u64` 単独キーで書くと、
            // engine 側が `IncompatibleRowKeyFormat` を返して本テストが検証したい
            // 行デコード経路へ到達しなくなる＝テストが空振りする）。
            let row_table_def: redb::TableDefinition<(&str, u64), &[u8]> =
                redb::TableDefinition::new("user_rows/docs");
            let mut table = write_txn.open_table(row_table_def).expect("open row table");
            // version バイトのみで後続フィールドが一切ない、意図的な破損バイト列
            // （`get_row_surfaces_data_corruption_distinctly_from_not_found` と同手法）。
            table
                .insert(("tenant-a", 1u64), &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let core = EngineCore::open(&path).expect("reopen engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // schema の次元（3）とは一致するが NaN を含むクエリを渡す。
    let err = core
        .search(&ctx, "docs", &[1.0, f32::NAN, 0.0], 1)
        .expect_err("non-finite query must be rejected");
    assert!(
        matches!(err, CoreError::Kernel(KernelError::NonFiniteQuery)),
        "expected early NonFiniteQuery rejection (proving the malformed row was never scanned), got: {err}"
    );
}

// レビュー指摘対応（Medium 2・Issue #32）: `search`・`get_row` はいずれもテーブル不存在を
// `CoreError::NotFound` へ丸め込み、他テナントの存在情報を漏らさない契約で統一する
// （security.md「アクセス制御の不備」）。
#[test]
fn search_and_get_row_agree_on_not_found_for_missing_table() {
    let dir = TempDir::new("missing-table-symmetry");
    let core = EngineCore::open(dir.db_path()).expect("open engine core");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let search_err = core
        .search(&ctx, "not_registered", &[1.0, 0.0], 1)
        .expect_err("search on missing table must fail");
    let get_row_err = core
        .get_row(&ctx, "not_registered", "tenant-a", 1)
        .expect_err("get_row on missing table must fail");

    assert!(matches!(search_err, CoreError::NotFound));
    assert!(matches!(get_row_err, CoreError::NotFound));
}

// レビュー指摘対応（Low・Issue #32）。`ctx` が `Public` のみ許可（既定コンストラクタ）の
// 場合、同一テナントの `Visibility::Private` 行は `search` 結果から除外されること
// （CORE-2: 可視性ラベル評価が正しく `search` 経路を通っていることの統合テスト）。
#[test]
fn search_excludes_private_rows_of_the_same_tenant_when_ctx_disallows_private() {
    let dir = TempDir::new("private-visibility-excluded");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    // Private 行がクエリに最も近い（マスクが効いていなければ検索結果の先頭に来る）。
    seed_row(
        &storage,
        "docs",
        1,
        "tenant-a",
        Visibility::Private,
        &[1.0, 0.0],
    );
    seed_row(
        &storage,
        "docs",
        2,
        "tenant-a",
        Visibility::Public,
        &[0.5, 0.0],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    // 既定コンストラクタは Public のみ許可（`PolicyContext::new` のドキュメント参照）。
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let hits = core
        .search(&ctx, "docs", &[1.0, 0.0], 10)
        .expect("search ok");

    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);

    // `Visibility::Private` を明示許可すれば結果に含まれるようになることも併せて確認する
    // （マスク自体が機能していることの対照実験）。
    let ctx_with_private =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let hits_with_private = core
        .search(&ctx_with_private, "docs", &[1.0, 0.0], 10)
        .expect("search ok");
    assert_eq!(
        hits_with_private.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn boundary_empty_table_search_returns_empty_results() {
    let dir = TempDir::new("boundary-empty");
    let storage = Storage::open(dir.db_path()).expect("open storage");
    storage
        .create_table(&schema_for("docs", 2))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let hits = core
        .search(&ctx, "docs", &[1.0, 0.0], 5)
        .expect("search ok");
    assert!(hits.is_empty());
}
