//! TASK-146 拡張機能（EXT-1, EXT-2）の統合テスト（対象ビヘイビア: EXT-1, EXT-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-146・`docs/spec/04-behavior/extensions.md`）。
//!
//! `crates/engine/src/catalog.rs` へ追加したテーブルスコープ行 API
//! （`insert_row_into_table` / `insert_rows_into_table` / `get_row_from_table` /
//! `scan_table_page`）を検証する。`tests/multi_dim_tables.rs`（TASK-91）は
//! production コードに行とテーブルを関連付ける機構がまだ無かった時点で、
//! id レンジ分離によりテーブル帰属をテスト側で模擬していたが、本ファイルは
//! その機構そのもの（本タスクの成果物）を直接検証する。
//!
//! スコープ境界: 検索カーネル本体・類似度計算はデータ層の責務外（別タスク管轄）。
//! EXT-1 の「検索」に対応する範囲として、本ファイルではテーブルスコープ scan で
//! 読み出した embedding に対しテスト側でコサイン類似度の brute-force top-k を計算し、
//! データ層の挙動を確認するに留める。

use std::path::PathBuf;

use engine::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore, VectorCore};
use engine::kernel::{CpuScalarProvider, KernelError};
use engine::policy::PolicyContext;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::TenantWriteError;

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT_ID: &str = "tenant-a";

/// 外部クレート非依存の決定的擬似乱数生成器（xorshift32）。テストデータ生成にのみ使う。
struct Xorshift32(u32);

impl Xorshift32 {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u32() as f64 / u32::MAX as f64) as f32
    }
}

/// `dim` 次元・`seed` に基づく決定論的な埋め込みを生成する。
fn make_embedding(dim: u32, seed: u32) -> Vec<f32> {
    // seed 0 は xorshift の不動点になるため非ゼロへ置換する。`seed | 1` だと
    // 隣接する偶奇シード（0/1・2/3 等）が同じ値へ潰れ、実質半数の異なる
    // ベクトルしか得られなくなる問題があった。置換先を実際に使われる seed=1
    // と衝突しない u32::MAX にすることで、0 と 1 を含むすべての隣接シードが
    // 異なるベクトルを生成する。
    let seeded = if seed == 0 { u32::MAX } else { seed };
    let mut rng = Xorshift32(seeded);
    (0..dim).map(|_| rng.next_f32()).collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

fn vector_table_schema(name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("body", ColumnType::Text, true),
        ],
    )
}

/// 隣接シード（0/1・2/3 等）が同一ベクトルへ潰れないことを確認する回帰テスト。
/// `seed | 1` による奇数化では偶奇の隣接シードが同値化し、データセットが実質
/// 半数の異なるベクトルしか持たなくなる不具合があったため、0 のみを非ゼロへ
/// 置換する現行実装（`make_embedding`）で隣接シードが異なるベクトルを
/// 生成することを保証する。
#[test]
fn make_embedding_adjacent_seeds_produce_distinct_vectors() {
    for seed in 0..8u32 {
        let a = make_embedding(16, seed);
        let b = make_embedding(16, seed + 1);
        assert_ne!(
            a,
            b,
            "adjacent seeds {seed} and {} must not collapse to the same embedding",
            seed + 1
        );
    }
}

// --- EXT-1: 既定 768 次元での挿入・読み出し・検索動作 ------------------------------

#[test]
fn ext1_insert_and_read_back_768_dim_rows() {
    let path = unique_db_path("ext1-insert-read");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 768))
        .expect("create_table");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    let embeddings: Vec<Vec<f32>> = (0..20u32).map(|i| make_embedding(768, i + 1)).collect();
    for (i, embedding) in embeddings.iter().enumerate() {
        // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
        // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
        // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{i}"))
            .expect("valid operation_id");
        engine::tenant::insert_row(
            &storage,
            "docs",
            &ctx,
            i as u64,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding,
                metadata: b"m",
            },
            &op_id,
        )
        .unwrap_or_else(|e| panic!("tenant::insert_row failed for id={i}: {e}"));
    }

    for (i, embedding) in embeddings.iter().enumerate() {
        let row = storage
            .get_row_from_table("docs", TENANT_ID, i as u64)
            .unwrap_or_else(|e| panic!("get_row_from_table failed for id={i}: {e}"));
        assert_eq!(row.id, i as u64);
        assert_eq!(row.embedding.len(), 768);
        assert_eq!(&row.embedding, embedding);
    }

    let (page, cursor) = storage
        .scan_table_page("docs", None, 100)
        .expect("scan_table_page");
    assert_eq!(page.len(), 20);
    assert_eq!(cursor, None);
    for row in &page {
        assert_eq!(row.embedding.len(), 768);
    }
}

#[test]
fn ext1_brute_force_top_k_ranks_self_match_first() {
    // 検索カーネル本体（類似度計算・ランキング）は別タスク管轄（TASK-124〜）。
    // ここではデータ層から読み出した embedding に対しテスト側で brute-force に
    // コサイン類似度を計算し、自己一致がランク 1 になることでデータ層の
    // 挿入・読み出し動作を確認する（EXT-1）。
    let path = unique_db_path("ext1-topk");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 768))
        .expect("create_table");

    let embeddings: Vec<Vec<f32>> = (0..10u32).map(|i| make_embedding(768, i + 1)).collect();
    let rows: Vec<(u64, RowInput<'_>)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, embedding)| {
            (
                i as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"m",
                },
            )
        })
        .collect();
    // テナント境界付きバッチ API 経由（生の `Storage::insert_rows_into_table` は
    // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した）。
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_rows(
        &storage,
        "docs",
        &ctx,
        &rows,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert_rows");

    let (all_rows, _cursor) = storage
        .scan_table_page("docs", None, 100)
        .expect("scan_table_page");

    // クエリ = 挿入済み embedding のうち 1 件（自己一致が最上位になるべき）。
    let query_idx = 3usize;
    let query = &embeddings[query_idx];

    let mut ranked: Vec<(u64, f32)> = all_rows
        .iter()
        .map(|row| (row.id, cosine_similarity(query, &row.embedding)))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).expect("similarity is not NaN"));

    assert_eq!(
        ranked.first().map(|(id, _)| *id),
        Some(query_idx as u64),
        "self-match must rank first: {ranked:?}"
    );
}

// --- EXT-2: テーブル粒度次元固定・複数テーブル共存 --------------------------------

#[test]
fn ext2_multiple_tables_with_distinct_dims_coexist() {
    let path = unique_db_path("ext2-coexist");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let dims: [(&str, u32); 3] = [("small", 384), ("mid", 768), ("large", 1536)];
    for (name, dim) in dims {
        storage
            .create_table(&vector_table_schema(name, dim))
            .unwrap_or_else(|e| panic!("create_table({name}) failed: {e}"));
    }

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    for (name, dim) in dims {
        let embedding = make_embedding(dim, 7);
        engine::tenant::insert_row(
            &storage,
            name,
            &ctx,
            1,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &embedding,
                metadata: b"m",
            },
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .unwrap_or_else(|e| panic!("insert into {name} (dim {dim}) failed: {e}"));
    }

    for (name, dim) in dims {
        let row = storage
            .get_row_from_table(name, TENANT_ID, 1)
            .unwrap_or_else(|e| panic!("get_row_from_table({name}) failed: {e}"));
        assert_eq!(row.embedding.len(), dim as usize);
    }
}

#[test]
fn ext2_rejects_dimension_mismatch_per_table_fail_closed() {
    let path = unique_db_path("ext2-dim-mismatch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("small", 384))
        .expect("create_table(small)");
    storage
        .create_table(&vector_table_schema("mid", 768))
        .expect("create_table(mid)");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");

    // 他テーブルの次元（768）を small（384）へ挿入しようとすると拒否される。
    let wrong_dim_embedding = make_embedding(768, 1);
    let err = engine::tenant::insert_row(
        &storage,
        "small",
        &ctx,
        1,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &wrong_dim_embedding,
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("mismatched dim must be rejected");
    assert!(matches!(
        err,
        TenantWriteError::Catalog(CatalogError::Invalid(_))
    ));

    // 次元 0 の埋め込みも拒否される。
    let err = engine::tenant::insert_row(
        &storage,
        "small",
        &ctx,
        2,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &[],
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("zero-length embedding must be rejected");
    assert!(matches!(
        err,
        TenantWriteError::Catalog(CatalogError::Invalid(_))
    ));

    // small テーブルには何も挿入されていないはず（拒否されたので）。
    let (rows, _cursor) = storage
        .scan_table_page("small", None, 100)
        .expect("scan_table_page(small)");
    assert!(rows.is_empty());
}

#[test]
fn ext2_same_id_coexists_independently_across_tables() {
    // TASK-91 の id レンジ模擬（`multi_dim_tables.rs`）では検証不能だった点。
    // テーブル帰属した独立ストア（本タスクの production 実装）を直接証明する。
    let path = unique_db_path("ext2-same-id");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("small", 384))
        .expect("create_table(small)");
    storage
        .create_table(&vector_table_schema("mid", 768))
        .expect("create_table(mid)");

    let small_embedding = make_embedding(384, 11);
    let mid_embedding = make_embedding(768, 22);

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_row(
        &storage,
        "small",
        &ctx,
        42,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &small_embedding,
            metadata: b"small-42",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert into small id=42");
    engine::tenant::insert_row(
        &storage,
        "mid",
        &ctx,
        42,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &mid_embedding,
            metadata: b"mid-42",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert into mid id=42");

    let small_row = storage
        .get_row_from_table("small", TENANT_ID, 42)
        .expect("get small id=42");
    let mid_row = storage
        .get_row_from_table("mid", TENANT_ID, 42)
        .expect("get mid id=42");

    assert_eq!(small_row.embedding, small_embedding);
    assert_eq!(small_row.metadata, b"small-42");
    assert_eq!(mid_row.embedding, mid_embedding);
    assert_eq!(mid_row.metadata, b"mid-42");
}

#[test]
fn ext2_scan_table_page_returns_only_own_table_rows() {
    let path = unique_db_path("ext2-scan-isolation");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("small", 384))
        .expect("create_table(small)");
    storage
        .create_table(&vector_table_schema("mid", 768))
        .expect("create_table(mid)");

    // `RowInput` は埋め込みバッファを借用するため、先にすべての埋め込みを実体として
    // 保持してから `RowInput` を組み立てる（借用元の生存期間を `insert_rows_into_table`
    // 呼び出しまで確実に確保するため）。
    let small_embeddings: Vec<Vec<f32>> = (0..5u64)
        .map(|i| make_embedding(384, i as u32 + 1))
        .collect();
    let small_rows: Vec<(u64, RowInput<'_>)> = small_embeddings
        .iter()
        .enumerate()
        .map(|(i, embedding)| {
            (
                i as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"s",
                },
            )
        })
        .collect();
    // テナント境界付きバッチ API 経由（生の `Storage::insert_rows_into_table` は
    // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した）。
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_rows(
        &storage,
        "small",
        &ctx,
        &small_rows,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed small");

    let mid_embeddings: Vec<Vec<f32>> = (100..103u64)
        .map(|i| make_embedding(768, i as u32 + 1))
        .collect();
    let mid_rows: Vec<(u64, RowInput<'_>)> = mid_embeddings
        .iter()
        .enumerate()
        .map(|(idx, embedding)| {
            (
                100u64 + idx as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"m",
                },
            )
        })
        .collect();
    engine::tenant::insert_rows(
        &storage,
        "mid",
        &ctx,
        &mid_rows,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed mid");

    let (small_page, _) = storage
        .scan_table_page("small", None, 100)
        .expect("scan small");
    assert_eq!(small_page.len(), 5);
    assert!(small_page.iter().all(|r| r.embedding.len() == 384));
    assert!(small_page.iter().all(|r| r.id < 100));

    let (mid_page, _) = storage.scan_table_page("mid", None, 100).expect("scan mid");
    assert_eq!(mid_page.len(), 3);
    assert!(mid_page.iter().all(|r| r.embedding.len() == 768));
    assert!(mid_page.iter().all(|r| r.id >= 100));
}

#[test]
fn ext2_state_survives_close_and_reopen() {
    let path = unique_db_path("ext2-persist");
    let _cleanup = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage (first)");
        storage
            .create_table(&vector_table_schema("small", 384))
            .expect("create_table(small)");
        storage
            .create_table(&vector_table_schema("mid", 768))
            .expect("create_table(mid)");

        let small_embedding = make_embedding(384, 3);
        let mid_embedding = make_embedding(768, 5);
        let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
        engine::tenant::insert_row(
            &storage,
            "small",
            &ctx,
            1,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &small_embedding,
                metadata: b"s1",
            },
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert small id=1");
        engine::tenant::insert_row(
            &storage,
            "mid",
            &ctx,
            1,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &mid_embedding,
                metadata: b"m1",
            },
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert mid id=1");
        // `storage` はここでスコープを抜けて drop される（close 相当）。
    }

    {
        let storage = Storage::open(&path).expect("open storage (second)");
        let small_row = storage
            .get_row_from_table("small", TENANT_ID, 1)
            .expect("get small id=1 after reopen");
        let mid_row = storage
            .get_row_from_table("mid", TENANT_ID, 1)
            .expect("get mid id=1 after reopen");
        assert_eq!(small_row.embedding.len(), 384);
        assert_eq!(mid_row.embedding.len(), 768);
        assert_eq!(small_row.metadata, b"s1");
        assert_eq!(mid_row.metadata, b"m1");

        // 誤次元挿入の拒否・同一 id の独立共存も再 open 後に不変であることを確認する。
        let wrong_dim = make_embedding(768, 9);
        let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
        let err = engine::tenant::insert_row(
            &storage,
            "small",
            &ctx,
            2,
            &RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &wrong_dim,
                metadata: b"s2",
            },
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect_err("mismatched dim must still be rejected after reopen");
        assert!(matches!(
            err,
            TenantWriteError::Catalog(CatalogError::Invalid(_))
        ));
    }
}

#[test]
fn ext2_insert_rows_into_table_discards_whole_transaction_on_mid_batch_failure() {
    // `storage.rs` の `persist2_put_batch_discards_whole_transaction_on_mid_batch_encode_failure`
    // に対応するテーブルスコープ版。`insert_rows_into_table` は write トランザクションを
    // commit する前にバッチ内の各行を検証するため（本ファイル冒頭コメント・catalog.rs の
    // `insert_rows_into_table` ドキュメンテーションコメント参照）、3 件目の次元不一致で
    // 失敗した場合、先に検証を通過した 1・2 件目も含めて全体が未反映のまま拒否される
    // ことを確認する。
    let path = unique_db_path("ext2-batch-mid-failure");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("small", 384))
        .expect("create_table(small)");

    let ok_embedding_1 = make_embedding(384, 1);
    let ok_embedding_2 = make_embedding(384, 2);
    let wrong_dim_embedding = make_embedding(768, 3); // small は 384 次元固定のため不一致。
    let batch = vec![
        (
            1u64,
            RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &ok_embedding_1,
                metadata: b"ok-1",
            },
        ),
        (
            2u64,
            RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &ok_embedding_2,
                metadata: b"ok-2",
            },
        ),
        (
            3u64,
            RowInput {
                tenant_id: TENANT_ID,
                visibility: Visibility::Public,
                embedding: &wrong_dim_embedding,
                metadata: b"bad-3",
            },
        ),
    ];

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    let err = engine::tenant::insert_rows(
        &storage,
        "small",
        &ctx,
        &batch,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("batch containing a dimension mismatch must fail");
    assert!(matches!(
        err,
        TenantWriteError::Catalog(CatalogError::Invalid(_))
    ));

    // トランザクション全体が破棄され、先に検証を通過していた 1・2 件目も
    // 一切反映されていないこと（`RowNotFound` まで確認し、別の理由での
    // 読み取り失敗と区別する）。
    for id in [1u64, 2] {
        let err = storage
            .get_row_from_table("small", TENANT_ID, id)
            .expect_err(&format!("row {id} must not be visible after discard"));
        assert!(matches!(err, CatalogError::RowNotFound(_)));
    }
    let (rows, _cursor) = storage
        .scan_table_page("small", None, 100)
        .expect("scan_table_page(small) after aborted batch");
    assert!(
        rows.is_empty(),
        "no rows should be committed when the batch transaction is discarded, got: {rows:?}"
    );
}

// --- scan_table_page ページング契約（TASK-146 レビュー指摘対応） ------------------
// `catalog.rs::scan_table_page` は `storage.rs::scan_page`（TASK-140/141）と同じ
// カーソル・打ち切り契約を独自実装で再現している。`storage.rs` 側の同名テストと
// 対になる形でテーブルスコープ版を検証する。

#[test]
fn ext2_scan_table_page_resumes_via_after_cursor_across_pages() {
    // `after: Some(cursor)` からの継続読み出し（`cursor.checked_add(1)` 起点計算）を
    // 3 ページに分けて検証する。
    let path = unique_db_path("scan-page-resume");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 1))
        .expect("create_table(docs)");

    let embeddings: Vec<[f32; 1]> = (0..25u64).map(|i| [i as f32]).collect();
    let rows: Vec<(u64, RowInput<'_>)> = (0..25u64)
        .map(|i| {
            (
                i,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &embeddings[i as usize],
                    metadata: b"m",
                },
            )
        })
        .collect();
    // テナント境界付きバッチ API 経由（生の `Storage::insert_rows_into_table` は
    // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した）。
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_rows(
        &storage,
        "docs",
        &ctx,
        &rows,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed 25 rows");

    let (page1, cursor1) = storage
        .scan_table_page("docs", None, 10)
        .expect("first page");
    assert_eq!(page1.len(), 10);
    assert_eq!(page1.first().map(|r| r.id), Some(0));
    assert_eq!(page1.last().map(|r| r.id), Some(9));
    // カーソルは行ストアの物理キーと同形の `(tenant_id, id)`（対象ビヘイビア: TABLE-12）。
    assert_eq!(cursor1, Some((TENANT_ID.to_string(), 9)));

    let (page2, cursor2) = storage
        .scan_table_page(
            "docs",
            cursor1.as_ref().map(|(t, id)| (t.as_str(), *id)),
            10,
        )
        .expect("second page");
    assert_eq!(page2.len(), 10);
    assert_eq!(page2.first().map(|r| r.id), Some(10));
    assert_eq!(page2.last().map(|r| r.id), Some(19));
    assert_eq!(cursor2, Some((TENANT_ID.to_string(), 19)));

    let (page3, cursor3) = storage
        .scan_table_page(
            "docs",
            cursor2.as_ref().map(|(t, id)| (t.as_str(), *id)),
            10,
        )
        .expect("third (partial) page");
    assert_eq!(page3.len(), 5);
    assert_eq!(page3.first().map(|r| r.id), Some(20));
    assert_eq!(page3.last().map(|r| r.id), Some(24));
    // 末尾未満の件数しか返らなかったので、続きがないことを示す `None` を返す。
    assert_eq!(cursor3, None);
}

#[test]
fn ext2_scan_table_page_byte_budget_caps_page_and_resume_covers_all_rows_without_gaps_or_duplicates(
) {
    // `MAX_SCAN_PAGE_BYTES` によるバイト上限打ち切り分岐（行数上限 limit=100 には
    // 遠く及ばない件数でも、メタデータのバイト量合計が上限を超えると打ち切られる）。
    // カーソル再開で全ページを結合し、欠落・重複がないこと（＝全行一致）まで確認する。
    let path = unique_db_path("scan-page-bytes");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 1))
        .expect("create_table(docs)");

    // 最大メタデータ長相当（4 MiB）の行を 5 件用意する。エンコード済みバイト量の合計は
    // すぐに `MAX_SCAN_PAGE_BYTES`（16 MiB）を超えるため、バイト上限で打ち切られるはずである。
    let big_metadata = vec![0u8; 4 * 1024 * 1024];
    let embedding = [1.0f32];
    let rows: Vec<(u64, RowInput<'_>)> = (0..5u64)
        .map(|i| {
            (
                i,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &embedding,
                    metadata: &big_metadata,
                },
            )
        })
        .collect();
    // テナント境界付きバッチ API 経由（生の `Storage::insert_rows_into_table` は
    // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した）。
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_rows(
        &storage,
        "docs",
        &ctx,
        &rows,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed large rows");

    let (page1, cursor1) = storage
        .scan_table_page("docs", None, 100)
        .expect("first page capped by byte budget");
    assert!(
        page1.len() < 5,
        "byte budget must cap the page before the row limit (100) is reached, got {} rows",
        page1.len()
    );
    assert!(
        !page1.is_empty(),
        "at least one row must be returned even under a tight byte budget"
    );
    assert!(
        cursor1.is_some(),
        "a capped page must report a continuation cursor"
    );

    let mut all_ids: Vec<u64> = page1.iter().map(|r| r.id).collect();
    let mut cursor = cursor1;
    loop {
        let (page, next_cursor) = storage
            .scan_table_page(
                "docs",
                cursor.as_ref().map(|(t, id)| (t.as_str(), *id)),
                100,
            )
            .expect("subsequent page after byte-budget cap");
        all_ids.extend(page.iter().map(|r| r.id));
        if next_cursor.is_none() {
            break;
        }
        cursor = next_cursor;
    }
    all_ids.sort_unstable();
    assert_eq!(
        all_ids,
        (0..5u64).collect::<Vec<_>>(),
        "resuming via cursors after byte-budget caps must yield all rows exactly once"
    );
}

#[test]
fn ext2_scan_table_page_limit_zero_returns_empty_page() {
    let path = unique_db_path("scan-page-limit-zero");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 1))
        .expect("create_table(docs)");
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        1,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &[1.0],
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed row");

    let (page, cursor) = storage
        .scan_table_page("docs", None, 0)
        .expect("limit=0 must not error");
    assert!(page.is_empty());
    assert_eq!(cursor, None);
}

#[test]
fn ext2_scan_table_page_after_u64_max_returns_empty_page() {
    // `after == Some(u64::MAX)` は `checked_add(1)` がオーバーフローする境界値。
    // 「これ以上続きがない」ことを示す空ページを返す（`storage.rs::scan_page` と同じ契約）。
    let path = unique_db_path("scan-page-after-max");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&vector_table_schema("docs", 1))
        .expect("create_table(docs)");
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        u64::MAX,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &[1.0],
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed row at id=u64::MAX");

    let (page, cursor) = storage
        .scan_table_page("docs", Some((TENANT_ID, u64::MAX)), 10)
        .expect("after=(tenant, u64::MAX) must not error");
    assert!(page.is_empty());
    assert_eq!(cursor, None);
}

#[test]
fn ext2_insert_rows_into_table_rejects_empty_batch_against_nonexistent_table() {
    // codex レビュー指摘（PR #130・P1）対応の回帰テスト。`rows.is_empty()` の早期
    // return をカタログ存在確認より先に置くと、存在しないテーブルへの空バッチ挿入が
    // `Ok(())` になり「テーブル不存在は fail-closed に `Err`」という契約を空バッチ
    // という入力値で迂回できてしまう。
    let path = unique_db_path("ext2-empty-batch-notfound");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    let err = engine::tenant::insert_rows(
        &storage,
        "ghost",
        &ctx,
        &[],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("empty batch against a nonexistent table must still be rejected");
    assert!(matches!(
        err,
        TenantWriteError::Catalog(CatalogError::TableNotFound(_))
    ));
}

#[test]
fn ext2_scan_table_page_rejects_limit_zero_against_nonexistent_table() {
    // codex レビュー指摘（PR #130・P1／cursor Low）対応の回帰テスト。`limit == 0` の
    // 早期 return をカタログ存在確認より先に置くと、存在しないテーブルへの limit=0
    // 走査が空ページで成功してしまい、同じくテーブル不存在の fail-closed 契約を
    // 迂回できてしまう。
    let path = unique_db_path("ext2-limit-zero-notfound");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let err = storage
        .scan_table_page("ghost", None, 0)
        .expect_err("limit=0 scan against a nonexistent table must still be rejected");
    assert!(matches!(err, CatalogError::TableNotFound(_)));
}

#[test]
fn ext2_rejects_operations_on_nonexistent_or_vectorless_table() {
    let path = unique_db_path("ext2-notfound");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // 不存在テーブルへの挿入・取得・走査は TableNotFound。
    let embedding = make_embedding(8, 1);
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    let err = engine::tenant::insert_row(
        &storage,
        "ghost",
        &ctx,
        1,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &embedding,
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("insert into nonexistent table must fail");
    assert!(matches!(
        err,
        TenantWriteError::Catalog(CatalogError::TableNotFound(_))
    ));

    let err = storage
        .get_row_from_table("ghost", TENANT_ID, 1)
        .expect_err("get from nonexistent table must fail");
    assert!(matches!(err, CatalogError::TableNotFound(_)));

    let err = storage
        .scan_table_page("ghost", None, 10)
        .expect_err("scan of nonexistent table must fail");
    assert!(matches!(err, CatalogError::TableNotFound(_)));

    // VECTOR 列を持たないテーブルへの embedding 付き挿入は拒否される。
    let text_only = TableSchema::new(
        "notes",
        vec![ColumnDef::new("body", ColumnType::Text, false)],
    );
    storage
        .create_table(&text_only)
        .expect("create_table(notes)");
    let err = engine::tenant::insert_row(
        &storage,
        "notes",
        &ctx,
        1,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &embedding,
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("insert with embedding into VECTOR-less table must fail");
    assert!(matches!(
        err,
        TenantWriteError::Catalog(CatalogError::Invalid(_))
    ));
}

// --- EXT-2: 2000 次元テーブルの共存・検索経路（TASK-151。対象ビヘイビア: EXT-2。
// ポインタ: `docs/spec/05-tasks.md` TASK-151・`docs/spec/04-behavior/extensions.md`
// EXT-2・`docs/spec/06-roadmap.md` MS-6）--------------------------------------------
//
// ここまでの `ext2_*` 群（TASK-146）はカタログ・行ストア層（`Storage` API）を直接
// 検証する。本節は `EngineCore::search`（`core.rs`）→ 既定検索 provider
// （`search_engine::default_engine()`）という製品の実検索経路まで通し、2000 次元
// テーブルが 768 次元テーブルと同一 `Storage` に共存した状態でも正しく・
// テーブル境界を越えずに検索できることを確認する（`tests/multi_dim_tables.rs`
// （TASK-91）・`tests/search_engine.rs`（TASK-131・CORE-9）の手法を踏襲）。
//
// production コード（`crates/engine/src/`）は変更しない（本 Issue のスコープ外）。
// 実測（性能・SIMD/GPU 相対比較）は `examples/high_dim_bench.rs` と
// `docs/design/high-dim-2000-reverification.md` が担い、本ファイルは正しさのみを扱う。

/// 768 / 2000 次元それぞれで検証する行数（`MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES`
/// の範囲内。2000 次元 × 200 行 × 4 バイトは約 1.6 MiB で十分に小さい）。
const HIGH_DIM_ROW_COUNT: u32 = 200;

/// `emb768` / `emb2000` へ割り当てる行 ID のオフセット。両テーブルの ID 範囲を
/// 重ならないよう分離し、検索結果の hit ID がどちらのテーブル由来かをテストで
/// 判別できるようにする（テーブル境界を越えた混入を検出するための前提）。
const EMB768_ID_OFFSET: u64 = 0;
const EMB2000_ID_OFFSET: u64 = 10_000;

/// `dim` 次元のテーブル `name` を作成し、決定論的な埋め込み `HIGH_DIM_ROW_COUNT` 行を
/// `id_offset` を起点とする ID で挿入した `Storage` を返す準備ヘルパー。呼び出し元が
/// 複数テーブルを同一 `Storage` に積み上げるために使う（`insert_rows_into_table` は
/// テーブル単位のバッチ API のため）。`id_offset` はテーブルごとに重ならない値を渡し、
/// 検索結果の hit ID からテーブル帰属を検証できるようにする。
fn seed_high_dim_table(storage: &Storage, name: &str, dim: u32, id_offset: u64) {
    storage
        .create_table(&vector_table_schema(name, dim))
        .unwrap_or_else(|e| panic!("create_table({name}) failed: {e}"));

    let embeddings: Vec<Vec<f32>> = (0..HIGH_DIM_ROW_COUNT)
        .map(|i| make_embedding(dim, i + 1))
        .collect();
    let rows: Vec<(u64, RowInput<'_>)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, embedding)| {
            (
                id_offset + i as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding,
                    metadata: b"m",
                },
            )
        })
        .collect();
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_rows(
        storage,
        name,
        &ctx,
        &rows,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .unwrap_or_else(|e| panic!("tenant::insert_rows({name}) failed: {e}"));
}

/// `dim` 次元の embedding 群（`seed_high_dim_table` と同一の決定論的生成規則）を
/// 単独で再構築する（テスト側で query ベクトルを取り出すために使う。`Storage` は
/// 所有権が `EngineCore::from_storage` へ移るため、挿入した embedding をテスト側で
/// 再度読み出す代わりに同じ生成規則で再現する）。
fn high_dim_embeddings(dim: u32) -> Vec<Vec<f32>> {
    (0..HIGH_DIM_ROW_COUNT)
        .map(|i| make_embedding(dim, i + 1))
        .collect()
}

/// 768 次元テーブル `emb768` と 2000 次元テーブル `emb2000` の両方を持つ `Storage` を
/// `path` に構築する（新規作成のみ。close・reopen はテスト側で行う）。
fn build_coexisting_high_dim_storage(path: &PathBuf) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    seed_high_dim_table(&storage, "emb768", 768, EMB768_ID_OFFSET);
    seed_high_dim_table(&storage, "emb2000", 2000, EMB2000_ID_OFFSET);
    storage
}

#[test]
fn ext2_2000_dim_table_coexists_with_768_dim_table_and_search_is_exact() {
    // `tests/search_engine.rs::core9_default_engine_matches_cpu_scalar_reference` と
    // 同じ手法（既定 provider と参照実装 `CpuScalarProvider` を別 `Storage` へ同一データで
    // 構築し、結果が完全一致することを確認する）。`parallel_search.rs` 冒頭コメントの
    // 通り、内積計算は共有ヘルパー `kernel.rs::dot` を経由し加算順序をスカラー参照実装と
    // 揃えているため、2000 次元でもスコアまで含めた完全一致を期待できる
    // （近似判定に倒すとテスト側の許容誤差設計という別の複雑さを持ち込むため避ける）。
    let path_default = unique_db_path("ext2-hd-coexist-default");
    let _cleanup_default = CleanupGuard(path_default.clone());
    let storage_default = build_coexisting_high_dim_storage(&path_default);
    let core_default = EngineCore::from_storage(storage_default, search_engine::default_engine());

    let path_reference = unique_db_path("ext2-hd-coexist-reference");
    let _cleanup_reference = CleanupGuard(path_reference.clone());
    let storage_reference = build_coexisting_high_dim_storage(&path_reference);
    let core_reference = EngineCore::from_storage(storage_reference, Box::new(CpuScalarProvider));

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    const TOP_K: usize = 10;

    for (table, dim, id_offset) in [
        ("emb768", 768u32, EMB768_ID_OFFSET),
        ("emb2000", 2000u32, EMB2000_ID_OFFSET),
    ] {
        let embeddings = high_dim_embeddings(dim);
        // クエリは挿入済み embedding の 1 件（テーブル境界・provider 差し替え等価性の
        // 確認が目的であり、自己一致が Top-1 になることまでは主張しない。raw dot
        // product は非正規化ベクトルでは自己一致が最大内積になるとは限らないため）。
        let query = &embeddings[3];

        let hits_default = core_default
            .search(&ctx, table, query, TOP_K)
            .unwrap_or_else(|e| panic!("default engine search({table}) failed: {e}"));
        let hits_reference = core_reference
            .search(&ctx, table, query, TOP_K)
            .unwrap_or_else(|e| panic!("reference engine search({table}) failed: {e}"));

        assert_eq!(
            hits_default, hits_reference,
            "default provider と CpuScalarProvider の Top-k が {table}（dim={dim}）で不一致"
        );
        assert!(
            hits_default.len() <= TOP_K,
            "{table}: hit 件数が k を超過: {}",
            hits_default.len()
        );
        // `emb768`・`emb2000` は重ならない ID 範囲（`EMB768_ID_OFFSET` /
        // `EMB2000_ID_OFFSET`）へ挿入しているため、hit の ID が検索対象テーブルの
        // 範囲内に収まっていることを確認すれば、他テーブルの行が誤って混入して
        // いないことを検出できる。
        let id_range = id_offset..(id_offset + HIGH_DIM_ROW_COUNT as u64);
        assert!(
            hits_default.iter().all(|hit| id_range.contains(&hit.id)),
            "{table}: 自テーブルの id 範囲 {id_range:?} 外の hit が混入: {hits_default:?}"
        );
    }
}

#[test]
fn ext2_2000_dim_search_query_dim_mismatch_is_rejected_per_table_fail_closed() {
    // 曖昧な次元不一致は拒否側に倒す（coding-rust.md「エラー契約は fail-closed」）。
    // 2000 次元クエリを 768 次元テーブルへ、768 次元クエリを 2000 次元テーブルへ
    // 投げてもテナント・他テーブルの行が漏れずに拒否されることを確認する。
    let path = unique_db_path("ext2-hd-dim-mismatch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = build_coexisting_high_dim_storage(&path);
    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");

    let query_2000 = make_embedding(2000, 999);
    let err = core
        .search(&ctx, "emb768", &query_2000, 10)
        .expect_err("2000-dim query against a 768-dim table must be rejected");
    assert!(matches!(
        err,
        CoreError::Kernel(KernelError::DimMismatch {
            expected: 768,
            found: 2000,
        })
    ));

    let query_768 = make_embedding(768, 998);
    let err = core
        .search(&ctx, "emb2000", &query_768, 10)
        .expect_err("768-dim query against a 2000-dim table must be rejected");
    assert!(matches!(
        err,
        CoreError::Kernel(KernelError::DimMismatch {
            expected: 2000,
            found: 768,
        })
    ));
}

#[test]
fn ext2_2000_dim_search_results_survive_close_and_reopen() {
    // `ext2_state_survives_close_and_reopen`（データ層版）の検索経路版。2000 次元
    // テーブルが 768 次元テーブルと共存した状態で永続化され、再オープン後も
    // `EngineCore::search` から同一の Top-k が得られることを確認する。
    let path = unique_db_path("ext2-hd-persist");
    let _cleanup = CleanupGuard(path.clone());

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    const TOP_K: usize = 10;
    let query = high_dim_embeddings(2000)[5].clone();

    // close 前（初回構築した `Storage`・`EngineCore`）の検索結果を、drop（close 相当）
    // する前にこのスコープ内で確定させる。ここで `hits_before` を取らずに再オープンで
    // 代用すると、比較が「2 回の post-close 読み出し」同士になり、close/reopen を
    // 挟んだ永続化の検証にならない。
    let hits_before = {
        let storage = build_coexisting_high_dim_storage(&path);
        let core = EngineCore::from_storage(storage, search_engine::default_engine());
        let hits = core
            .search(&ctx, "emb2000", &query, TOP_K)
            .expect("search before close");
        // ここでスコープを抜けて `core`（と内部の `storage`）が drop される
        // （close 相当。`ext2_state_survives_close_and_reopen` と同じ手法）。
        hits
    };

    let hits_after = {
        let storage = Storage::open(&path).expect("reopen storage after close");
        let core = EngineCore::from_storage(storage, search_engine::default_engine());
        core.search(&ctx, "emb2000", &query, TOP_K)
            .expect("search after reopen")
    };

    assert_eq!(
        hits_before, hits_after,
        "close/reopen の前後で 2000 次元テーブルの Top-k が変化した"
    );
    assert!(!hits_before.is_empty(), "hit が 0 件");
}
