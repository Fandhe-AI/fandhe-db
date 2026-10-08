//! `engine::storage::Storage` の並行書き込み実測ハーネス（TASK-144、基盤・工程管理。
//! ポインタ: `docs/spec/05-tasks.md` TASK-144）。
//!
//! `cargo test` の対象には含めない手動実行専用ツール（`cargo run -p fandhe-db-engine --release
//! --example concurrent_write_bench` で実行する）。時間依存の測定値を CI のアサーション
//! に混ぜない方針（.claude/rules/coding-rust.md）のため、`tests/concurrent_write.rs`
//! （正しさの回帰テスト・CI 常時実行）とはファイルを分離している。
//!
//! 出力（待機時間 p50/p95/max・スループット）は
//! `docs/design/concurrent-write-verification.md` の実測環境・結果表に転記する。
//! スレッド数・行データサイズ・総行数はすべて定数で上限固定し、無制限確保をしない
//! （security.md「不安全な設計｜無制限リソース確保（DoS）」）。

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use engine::storage::{RowInput, Storage, Visibility};

/// 768 次元 f32 埋め込み（現実的なベクトルサイズの想定値）。
const EMBEDDING_DIM: usize = 768;
/// 小メタデータ列（id 等の付随情報を想定した固定長ダミー）。
const METADATA_LEN: usize = 64;
/// 1 設定（スレッド数 × 書き込み方式）あたりの総書き込み行数。
/// 手動実行が数十秒以内に終わるよう、行サイズ・スレッド数に対して控えめに固定する。
const ROWS_PER_CONFIG: u64 = 800;
/// `put_batch` 方式での 1 回のバッチ呼び出しあたりの行数。
const BATCH_SIZE: u64 = 20;

const THREAD_COUNTS: [u64; 4] = [1, 2, 4, 8];

#[derive(Clone, Copy)]
enum WriteMode {
    SinglePut,
    PutBatch,
}

impl WriteMode {
    fn label(self) -> &'static str {
        match self {
            WriteMode::SinglePut => "put(single)",
            WriteMode::PutBatch => "put_batch",
        }
    }
}

struct RunResult {
    thread_count: u64,
    mode: &'static str,
    op_count: usize,
    p50: Duration,
    p95: Duration,
    max: Duration,
    total: Duration,
    rows_per_sec: f64,
}

fn unique_db_path(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fandhe-db-engine-bench-{label}-{}.redb",
        std::process::id()
    ));
    path
}

fn embedding_for(id: u64) -> Vec<f32> {
    (0..EMBEDDING_DIM).map(|i| (id + i as u64) as f32).collect()
}

fn metadata_for(id: u64) -> Vec<u8> {
    let mut buf = vec![0u8; METADATA_LEN];
    let id_bytes = id.to_le_bytes();
    buf[..id_bytes.len()].copy_from_slice(&id_bytes);
    buf
}

/// 各操作（`put` 1 回、または `put_batch` 1 回）の所要時間を計測し、スレッドごとに
/// 集める。全スレッド join 後にまとめて集計するため、スレッド内では
/// `Vec::with_capacity` を操作回数（事前に定数から算出できる既知の上限）で確保する。
fn run_single_put(storage: Arc<Storage>, thread_count: u64) -> RunResult {
    let rows_per_thread = ROWS_PER_CONFIG / thread_count;
    let start = Instant::now();
    let handles: Vec<_> = (0..thread_count)
        .map(|thread_idx| {
            let storage = Arc::clone(&storage);
            thread::spawn(move || {
                let base_id = thread_idx * rows_per_thread * 1000;
                let tenant_id = format!("tenant-{thread_idx}");
                let mut latencies = Vec::with_capacity(rows_per_thread as usize);
                for offset in 0..rows_per_thread {
                    let id = base_id + offset;
                    let embedding = embedding_for(id);
                    let metadata = metadata_for(id);
                    let op_start = Instant::now();
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
                        .expect("put should succeed against a healthy database");
                    latencies.push(op_start.elapsed());
                }
                latencies
            })
        })
        .collect();

    let mut all_latencies = Vec::new();
    for handle in handles {
        all_latencies.extend(handle.join().expect("writer thread should not panic"));
    }
    let total = start.elapsed();
    summarize(thread_count, WriteMode::SinglePut, all_latencies, total)
}

fn run_put_batch(storage: Arc<Storage>, thread_count: u64) -> RunResult {
    let rows_per_thread = ROWS_PER_CONFIG / thread_count;
    let batches_per_thread = rows_per_thread / BATCH_SIZE;
    let start = Instant::now();
    let handles: Vec<_> = (0..thread_count)
        .map(|thread_idx| {
            let storage = Arc::clone(&storage);
            thread::spawn(move || {
                let base_id = thread_idx * rows_per_thread * 1000;
                let tenant_id = format!("tenant-{thread_idx}");
                let mut latencies = Vec::with_capacity(batches_per_thread as usize);
                for batch_idx in 0..batches_per_thread {
                    let batch_base = base_id + batch_idx * BATCH_SIZE;
                    let embeddings: Vec<Vec<f32>> = (0..BATCH_SIZE)
                        .map(|offset| embedding_for(batch_base + offset))
                        .collect();
                    let metadatas: Vec<Vec<u8>> = (0..BATCH_SIZE)
                        .map(|offset| metadata_for(batch_base + offset))
                        .collect();
                    let rows: Vec<(u64, RowInput<'_>)> = (0..BATCH_SIZE as usize)
                        .map(|i| {
                            (
                                batch_base + i as u64,
                                RowInput {
                                    tenant_id: &tenant_id,
                                    visibility: Visibility::Public,
                                    embedding: &embeddings[i],
                                    metadata: &metadatas[i],
                                },
                            )
                        })
                        .collect();
                    let op_start = Instant::now();
                    storage
                        .put_batch(&rows)
                        .expect("put_batch should succeed against a healthy database");
                    latencies.push(op_start.elapsed());
                }
                latencies
            })
        })
        .collect();

    let mut all_latencies = Vec::new();
    for handle in handles {
        all_latencies.extend(handle.join().expect("writer thread should not panic"));
    }
    let total = start.elapsed();
    summarize(thread_count, WriteMode::PutBatch, all_latencies, total)
}

fn summarize(
    thread_count: u64,
    mode: WriteMode,
    mut latencies: Vec<Duration>,
    total: Duration,
) -> RunResult {
    latencies.sort_unstable();
    let op_count = latencies.len();
    let percentile = |p: f64| -> Duration {
        if op_count == 0 {
            return Duration::ZERO;
        }
        let idx = ((op_count as f64) * p).floor() as usize;
        latencies[idx.min(op_count - 1)]
    };
    let rows_per_sec = if total.as_secs_f64() > 0.0 {
        ROWS_PER_CONFIG as f64 / total.as_secs_f64()
    } else {
        0.0
    };
    RunResult {
        thread_count,
        mode: mode.label(),
        op_count,
        p50: percentile(0.50),
        p95: percentile(0.95),
        max: latencies.last().copied().unwrap_or(Duration::ZERO),
        total,
        rows_per_sec,
    }
}

fn print_row(r: &RunResult) {
    println!(
        "{:>7} | {:<12} | {:>9} | {:>10.3?} | {:>10.3?} | {:>10.3?} | {:>10.3?} | {:>12.1}",
        r.thread_count, r.mode, r.op_count, r.p50, r.p95, r.max, r.total, r.rows_per_sec
    );
}

fn main() {
    println!(
        "concurrent_write_bench: EMBEDDING_DIM={EMBEDDING_DIM}, METADATA_LEN={METADATA_LEN}, \
         ROWS_PER_CONFIG={ROWS_PER_CONFIG}, BATCH_SIZE={BATCH_SIZE}"
    );
    println!(
        "{:>7} | {:<12} | {:>9} | {:>10} | {:>10} | {:>10} | {:>10} | {:>12}",
        "threads", "mode", "op_count", "p50", "p95", "max", "total", "rows/sec"
    );

    for &thread_count in &THREAD_COUNTS {
        let path = unique_db_path(&format!("single-{thread_count}"));
        let storage = Arc::new(Storage::open(&path).expect("open storage"));
        let result = run_single_put(storage, thread_count);
        print_row(&result);
        let _ = std::fs::remove_file(&path);
    }

    for &thread_count in &THREAD_COUNTS {
        let path = unique_db_path(&format!("batch-{thread_count}"));
        let storage = Arc::new(Storage::open(&path).expect("open storage"));
        let result = run_put_batch(storage, thread_count);
        print_row(&result);
        let _ = std::fs::remove_file(&path);
    }
}
