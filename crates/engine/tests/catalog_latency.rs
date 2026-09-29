//! `engine::catalog` の時間判定テスト（TASK-85、対象ビヘイビア: TABLE-4。
//! ポインタ: `docs/spec/04-behavior/data-model.md`）。
//!
//! `create_table` の所要時間が既存行数に依存しないことを、行数の異なる 2 つの DB の
//! 比率で判定する。旧来は `tests/catalog.rs` に同居していたが、同バイナリ内の fsync の
//! 多い兄弟テストと並列スレッドで走って計測が乱れ、負荷下で間欠失敗した（Issue #1164）。
//! cargo はテストバイナリを逐次実行するため、独立バイナリへ移設して並走を構造的に断ち、
//! さらに他の計測系テストと共通のタイミングロックを取得する。
//!
//! 計測設計: ラウンドごとに small/large を交互に計測し、加算的ノイズに強い最小値で
//! 比較する。判定閾値（`ratio < 5.0`）は据え置きでアサーションの弱体化ではない。

use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

#[path = "../src/test_util/timing_lock.rs"]
mod timing_lock;
use timing_lock::acquire_timing_lock;

fn embedding_schema(name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// 指定行数のダミー行を書き込んだ DB を用意する（性能検証の前提データ）。
fn seed_rows(storage: &Storage, count: u64) {
    let embedding = vec![0.5_f32; 8];
    let metadata = b"row".to_vec();
    let rows: Vec<(u64, RowInput<'_>)> = (0..count)
        .map(|id| {
            (
                id,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &embedding,
                    metadata: &metadata,
                },
            )
        })
        .collect();
    storage.put_batch(&rows).expect("seed put_batch");
}

#[test]
fn table4_create_table_latency_is_independent_of_row_count() {
    let _timing_guard = acquire_timing_lock();

    // 行数の異なる 2 つの DB で create_table の所要時間を比較し、行数に応じて
    // 増加しないことを検証する（絶対値ではなく比率で判定。実測値・spec 本文は転記しない）。
    let small_path = unique_db_path("table4-latency-small");
    let _small_guard = CleanupGuard(small_path.clone());
    let small = Storage::open(&small_path).expect("open small storage");
    seed_rows(&small, 1_000);

    let large_path = unique_db_path("table4-latency-large");
    let _large_guard = CleanupGuard(large_path.clone());
    let large = Storage::open(&large_path).expect("open large storage");
    seed_rows(&large, 10_000);

    // ウォームアップ（初回の DB ファイルアクセスの追加コストを計測の外側で吸収する）。
    small
        .create_table(&embedding_schema("docs_small_warmup", 8))
        .expect("warmup create_table small");
    large
        .create_table(&embedding_schema("docs_large_warmup", 8))
        .expect("warmup create_table large");

    const ROUNDS: usize = 9;
    let mut small_durations = Vec::with_capacity(ROUNDS);
    let mut large_durations = Vec::with_capacity(ROUNDS);

    for i in 0..ROUNDS {
        let start = Instant::now();
        small
            .create_table(&embedding_schema(&format!("docs_small_{i}"), 8))
            .expect("create_table small");
        small_durations.push(start.elapsed());

        let start = Instant::now();
        large
            .create_table(&embedding_schema(&format!("docs_large_{i}"), 8))
            .expect("create_table large");
        large_durations.push(start.elapsed());
    }

    // 負荷ノイズは加算的なため、代表値は最小値（本来のコストの推定量）とする。
    let best_small = small_durations.into_iter().min().unwrap_or(Duration::ZERO);
    let best_large = large_durations.into_iter().min().unwrap_or(Duration::ZERO);

    // 行数が 10 倍でも create_table の時間が極端に増加しないこと（TABLE-4）。
    let ratio = best_large.as_secs_f64().max(1e-9) / best_small.as_secs_f64().max(1e-9);
    assert!(
        ratio < 5.0,
        "create_table best latency scaled with row count too much: small={best_small:?}, large={best_large:?}, ratio={ratio}"
    );
}
