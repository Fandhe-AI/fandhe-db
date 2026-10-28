//! コールドスタート・ベクトルアリーナの統合テスト（TASK-87、対象ビヘイビア: TABLE-8。
//! ポインタ: `docs/spec/05-tasks.md` TASK-87・`docs/spec/04-behavior/data-model.md`
//! TABLE-8）。
//!
//! `VectorArena::build`（`crates/engine/src/arena.rs`）は `pub` な公開エントリポイントで
//! あり、後続の検索カーネル（TASK-133 以降）が呼び出す想定の API である。本ファイルは
//! クレート外部（`tests/` ディレクトリ＝ライブラリ利用者と同じ視点）から
//! `engine::arena::VectorArena` を実際に構築・参照できることを検証する（codex レビュー
//! 指摘対応: 構築手段が `pub(crate)` かつ呼び出し元がテストのみだと、本番経路から
//! 到達不能な dead code になる懸念があったため）。
//!
//! テーブルスコープ行 API（TASK-146・EXT-1/EXT-2）を使った基本構築・複数テーブル分離・
//! 次元不一致拒否（挿入時）・空テーブル・対象テーブル不存在の 5 シナリオを、
//! `crates/engine/src/arena.rs` 内の `#[cfg(test)]` ユニットテストから複製する。境界値検証
//! （`check_capacity` の上限ちょうど・`try_reserve_exact` の OOM 変換）・`build` 自体が
//! `Err(DimMismatch)` を返す分岐（手書きバイト列で検証済み次元一致を迂回する必要がある）・
//! TOCTOU 回帰テストは `catalog::get_table_schema_in_txn` 等の `pub(crate)` ヘルパーに
//! 依存するため、引き続き `src/arena.rs` 内のユニットテストの責務とする
//! （クレート外からは呼べないため）。

use engine::arena::{ArenaError, VectorArena};
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::storage::{RowInput, Storage, Visibility};

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した（旧: 結合テストごとの複製。
// `tests/power_loss.rs`・`tests/bench_accept.rs` と同じ `#[path]` 取り込み方式）。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT_ID: &str = "tenant-a";

/// `table_name` の schema を組み立てる（`embedding` 列 1 本のみを持つ最小構成）。
fn schema_for(table_name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        table_name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

// 対象ビヘイビア: TABLE-8。クレート外部から `Storage::create_table` →
// `Storage::insert_rows_into_table` → `VectorArena::build` という公開 API のみを使い、
// アリーナを構築・参照できること（コールドスタート・アリーナの基本契約）を検証する。
#[test]
fn table8_build_produces_arena_matching_inserted_rows() {
    let path = unique_db_path("basic");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let dim: usize = 8;
    storage
        .create_table(&schema_for("docs", dim as u32))
        .expect("create_table");

    // 埋め込みは `rows` より先に確保して所有権を保持し、`RowInput` からは参照のみを渡す
    // （`RowInput<'_>` は借用のため、一時変数を `map` クロージャ内で使い捨てにできない）。
    let embeddings: Vec<Vec<f32>> = (0..10u64)
        .map(|i| (0..dim).map(|d| (i as f32) + d as f32 * 0.1).collect())
        .collect();
    let rows: Vec<(u64, RowInput<'_>)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, embedding)| {
            (
                i as u64,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: if i % 2 == 0 {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    },
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
    .expect("seed rows");

    let arena = VectorArena::build(&storage, "docs").expect("build arena via public API");
    assert_eq!(arena.table_name(), "docs");
    assert_eq!(arena.dim(), dim as u32);
    assert_eq!(arena.len(), 10);
    assert!(!arena.is_empty());
    assert_eq!(arena.vectors().len(), 10 * dim);
    assert_eq!(arena.ids(), &(0u64..10).collect::<Vec<_>>()[..]);

    for i in 0..10usize {
        let expected_row = storage
            .get_row_from_table("docs", TENANT_ID, i as u64)
            .expect("read row back via storage");
        assert_eq!(arena.vector(i), Some(expected_row.embedding.as_slice()));
        assert_eq!(arena.tenant_id(i), Some(expected_row.tenant_id.as_str()));
        assert_eq!(arena.visibility(i), Some(expected_row.visibility));
    }

    // 範囲外は panic せず None を返す。
    assert_eq!(arena.vector(10), None);
}

// 対象ビヘイビア: TABLE-8（codex P1 対応の核心シナリオ）。複数テーブルが共存する状態で、
// 公開 API のみを使って構築したアリーナが対象テーブルの行だけを保持し、他テーブルの
// 行（次元が一致する行を含む）が混入しないことを検証する。
#[test]
fn table8_build_scopes_arena_to_the_requested_table_only() {
    let path = unique_db_path("multi-table");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&schema_for("docs_a", 4))
        .expect("create_table docs_a");
    storage
        .create_table(&schema_for("docs_b", 4))
        .expect("create_table docs_b");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_row(
        &storage,
        "docs_a",
        &ctx,
        0,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0, 3.0, 4.0],
            metadata: b"table=docs_a",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed docs_a row");
    engine::tenant::insert_row(
        &storage,
        "docs_b",
        &ctx,
        0,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &[9.0, 9.0, 9.0, 9.0],
            metadata: b"table=docs_b",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed docs_b row");

    let arena_a = VectorArena::build(&storage, "docs_a").expect("build arena for docs_a");
    assert_eq!(arena_a.len(), 1);
    assert_eq!(arena_a.ids(), &[0u64]);
    assert_eq!(arena_a.vector(0), Some([1.0, 2.0, 3.0, 4.0].as_slice()));

    let arena_b = VectorArena::build(&storage, "docs_b").expect("build arena for docs_b");
    assert_eq!(arena_b.len(), 1);
    assert_eq!(arena_b.ids(), &[0u64]);
    assert_eq!(arena_b.vector(0), Some([9.0, 9.0, 9.0, 9.0].as_slice()));
}

// 対象ビヘイビア: TABLE-8。挿入経路（`insert_row_into_table`）が次元不一致行を
// fail-closed に拒否し、`build` が対象テーブルへ帰属する行のみを見ることを検証する。
// `build` 自体が `Err(ArenaError::DimMismatch)` を返す分岐（挿入検証をすり抜けて
// 手書きバイト列で書き込まれた行がある場合の防御）は `pub(crate)` ヘルパーが必要な
// ため `src/arena.rs` 内のユニットテストの責務とする。
#[test]
fn table8_insert_rejects_dimension_mismatch_and_build_only_sees_valid_rows() {
    let path = unique_db_path("dim-mismatch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_for("docs", 4))
        .expect("create_table");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        0,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0, 3.0, 4.0],
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed matching-dim row");

    // 別次元のテーブルを作成し検索対象から外す（`insert_row_into_table` は次元検証する
    // ため、公開 API だけでは同一テーブル内に次元不一致行を作れない。ここでは
    // `Storage` 単体としての次元検証が有効であることの確認に留め、`build` 側の
    // `Err(DimMismatch)` 分岐は `src/arena.rs` の内部テストで手書きバイト列を使って
    // 再現している）。
    let err = engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        1,
        &RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0],
            metadata: b"m",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("dimension-mismatched insert must be rejected by the public API");
    assert!(matches!(
        err,
        engine::tenant::TenantWriteError::Catalog(engine::catalog::CatalogError::Invalid(_))
    ));

    // 次元検証を通過した行のみが残っており、公開 API から見た `build` は成功する。
    let arena = VectorArena::build(&storage, "docs").expect("build arena");
    assert_eq!(arena.len(), 1);
    assert_eq!(arena.ids(), &[0u64]);
}

// 対象ビヘイビア: TABLE-8。カタログに登録済みだが 1 行も書き込んでいないテーブルは
// 空アリーナとして成功すること。
#[test]
fn table8_build_on_empty_table_returns_empty_arena() {
    let path = unique_db_path("empty");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_for("docs", 16))
        .expect("create_table");

    let arena = VectorArena::build(&storage, "docs").expect("build arena on empty table");
    assert_eq!(arena.dim(), 16);
    assert_eq!(arena.len(), 0);
    assert!(arena.is_empty());
    assert!(arena.vectors().is_empty());
    assert!(arena.ids().is_empty());
}

// 対象ビヘイビア: TABLE-8。対象テーブルがカタログに存在しない場合は `Err` で拒否すること
// （`ArenaError` が公開 API 越しに一貫して `Err` を返すことの確認）。
#[test]
fn table8_build_rejects_missing_table() {
    let path = unique_db_path("missing-table");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let err = VectorArena::build(&storage, "not_registered")
        .expect_err("must reject a table that was never created");
    assert!(matches!(err, ArenaError::Catalog(_)));
}
