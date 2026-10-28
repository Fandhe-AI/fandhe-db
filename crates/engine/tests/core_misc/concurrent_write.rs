//! `engine::storage::Storage` の並行書き込み正しさ回帰テスト（TASK-144、基盤・工程管理。
//! ポインタ: `docs/spec/05-tasks.md` TASK-144）。対象ビヘイビア ID は無し（基盤タスク）。
//!
//! `redb` の書き込みトランザクションは `begin_write` の排他ロックで直列化される
//! （`crates/engine/src/storage.rs` 冒頭の分離レベル doc コメント参照）。本ファイルは
//! その直列化のもとで複数スレッドが `Arc<Storage>` を共有して同時に書き込んでも、
//! 全行が欠損・破損・二重化なく永続化されることだけを検証する（正しさの回帰防止）。
//!
//! 待機時間・スループットの実測は本ファイルの対象外（時間依存アサーションを CI に
//! 混ぜてフレークにしないため。.claude/rules/coding-rust.md「テストの skip・ignore・
//! アサーション弱体化で CI を通さない」を踏まえ、本テストに `#[ignore]` は付けない）。
//! 実測ハーネスは `crates/engine/examples/concurrent_write_bench.rs`（手動実行専用、
//! `cargo test` の対象外）を参照。結果は `docs/design/concurrent-write-verification.md`
//! に記載する。

use std::sync::Arc;
use std::thread;

use engine::storage::{RowInput, Storage, Visibility};

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 全行を `scan_page` のページングで読み切る（`scan()` は 1 回の呼び出しで確保する
/// 総バイト量に上限があり、本テストの行サイズ・件数次第では超過し得るため使わない。
/// `crates/engine/src/storage.rs` の `scan_page_caps_page_by_byte_budget_...` テストと
/// 同じ読み切りループ）。
fn read_all_ids(storage: &Storage) -> Vec<u64> {
    let mut ids = Vec::new();
    let mut cursor: Option<engine::storage::RowCursor> = None;
    loop {
        let cursor_ref = cursor.as_ref().map(|(t, id)| (t.as_str(), *id));
        let (page, next_cursor) = storage
            .scan_page(cursor_ref, 1_000)
            .expect("scan_page should not fail against a healthy database");
        ids.extend(page.iter().map(|r| r.id));
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    ids
}

/// スレッド数・行データサイズはテスト実行時間を数百ミリ秒以内に収めるための定数上限
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」の考え方をテストにも適用し、
/// 無制限に大きくしない）。
const THREAD_COUNT: u64 = 8;
const ROWS_PER_THREAD: u64 = 20;
const EMBEDDING_DIM: usize = 16;

#[test]
fn concurrent_put_from_multiple_threads_persists_every_row_without_loss_or_duplication() {
    let path = unique_db_path("put");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Arc::new(Storage::open(&path).expect("open storage"));

    // 各スレッドは重複しない id 範囲を担当する（[thread_idx * ROWS_PER_THREAD, ...)）。
    // 直列化された書き込みの下で全件が過不足なく永続化されることを検証する。
    let handles: Vec<_> = (0..THREAD_COUNT)
        .map(|thread_idx| {
            let storage = Arc::clone(&storage);
            thread::spawn(move || {
                let base_id = thread_idx * ROWS_PER_THREAD;
                for offset in 0..ROWS_PER_THREAD {
                    let id = base_id + offset;
                    let embedding: Vec<f32> = (0..EMBEDDING_DIM).map(|i| (id + i as u64) as f32).collect();
                    let metadata = format!("thread={thread_idx}").into_bytes();
                    let tenant_id = format!("tenant-{thread_idx}");
                    storage
                        .put(
                            id,
                            &RowInput {
                                tenant_id: &tenant_id,
                                visibility: Visibility::Public,
                                embedding: &embedding,
                                metadata: &metadata,
                            },
                        )
                        .expect("concurrent put should not fail under redb's serialized write transactions");
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("writer thread should not panic");
    }

    let mut ids = read_all_ids(&storage);
    ids.sort_unstable();
    let expected: Vec<u64> = (0..THREAD_COUNT * ROWS_PER_THREAD).collect();
    assert_eq!(
        ids, expected,
        "every row written by every thread must be present exactly once"
    );

    // 内容の一致も抜き取りで確認する（embedding が id 由来の期待値と一致すること）。
    // 物理キーは (tenant_id, id) の複合キー（TABLE-12）のため、id からその行を
    // 書き込んだスレッド（= tenant_id）を逆算する。
    for id in [0u64, THREAD_COUNT * ROWS_PER_THREAD - 1] {
        let thread_idx = id / ROWS_PER_THREAD;
        let tenant_id = format!("tenant-{thread_idx}");
        let row = storage.get(&tenant_id, id).expect("row must exist");
        let expected_embedding: Vec<f32> =
            (0..EMBEDDING_DIM).map(|i| (id + i as u64) as f32).collect();
        assert_eq!(row.embedding, expected_embedding);
    }
}

#[test]
fn concurrent_put_batch_from_multiple_threads_persists_every_row_without_loss_or_duplication() {
    let path = unique_db_path("put-batch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Arc::new(Storage::open(&path).expect("open storage"));

    let handles: Vec<_> = (0..THREAD_COUNT)
        .map(|thread_idx| {
            let storage = Arc::clone(&storage);
            thread::spawn(move || {
                let base_id = thread_idx * ROWS_PER_THREAD;
                let embeddings: Vec<Vec<f32>> = (0..ROWS_PER_THREAD)
                    .map(|offset| {
                        let id = base_id + offset;
                        (0..EMBEDDING_DIM).map(|i| (id + i as u64) as f32).collect()
                    })
                    .collect();
                let metadata = format!("thread={thread_idx}").into_bytes();
                let tenant_id = format!("tenant-{thread_idx}");
                let rows: Vec<(u64, RowInput<'_>)> = (0..ROWS_PER_THREAD)
                    .map(|offset| {
                        let id = base_id + offset;
                        (
                            id,
                            RowInput {
                                tenant_id: &tenant_id,
                                visibility: Visibility::Public,
                                embedding: &embeddings[offset as usize],
                                metadata: &metadata,
                            },
                        )
                    })
                    .collect();
                storage
                    .put_batch(&rows)
                    .expect("concurrent put_batch should not fail under redb's serialized write transactions");
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("writer thread should not panic");
    }

    let mut ids = read_all_ids(&storage);
    ids.sort_unstable();
    let expected: Vec<u64> = (0..THREAD_COUNT * ROWS_PER_THREAD).collect();
    assert_eq!(
        ids, expected,
        "every row written by every thread's batch must be present exactly once"
    );
}
