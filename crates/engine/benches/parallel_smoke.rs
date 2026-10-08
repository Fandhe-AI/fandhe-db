//! マルチスレッド並列検索カーネル（TASK-126）の p95 手動計測スモーク。対象ビヘイビア:
//! CORE-3・SEARCH-4（数値基準は spec 参照。本ベンチは手動実行専用の計測入口を
//! 提供するのみで、数値基準の回帰テスト化・CI 定期実行は TASK-127 の範囲）。
//! CORE-5（対照エンジン比較）は `contrast_bench.rs`（TASK-127・Issue #176）で判定する。
//!
//! `cargo bench --bench parallel_smoke -p fandhe-db-engine` で手動実行する（`make ci` の対象外。
//! `Cargo.toml` 側 `harness = false` / `test = false` は `benches/measurement.rs` と
//! 同一方針。時間依存の測定値を CI アサーションへ混ぜない）。
//!
//! 10 万本×768 次元のテーブルを決定的シードで合成し、`ParallelSearchProvider`
//! （`core.rs::EngineCore::open` の既定 provider。ベクトル化は行わず並列化のみ）へ
//! 単発クエリを繰り返し投げて p95 を計測する。`kernel.rs::SearchProvider` を直接呼び出し、
//! `redb`/アリーナは経由しない（provider 単体の計測入口。ストレージ層を含む end-to-end
//! 計測は対象外）。

// `harness` は `benches/measurement.rs` と 2 つの独立したコンパイル単位（cargo bench
// バイナリ）から取り込まれる共有ソースで、本ファイルは `protocol`・`rng` のみ使う
// （`ab`・`stats` は `protocol` 経由の間接利用に留まる）。バイナリターゲットでは
// 未到達の `pub` 項目が `dead_code` として警告されうるため、モジュール全体を
// 対象に許容する（`harness/mod.rs` 自体は変更しない）。
#[allow(dead_code)]
mod harness;

use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;

use engine::kernel::{SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;

const ROW_COUNT: usize = 100_000;
const DIM: usize = 768;
const TOP_K: usize = 20;

fn main() {
    let mut rng = DeterministicRng::new(1);
    let ids: Vec<u64> = (0..ROW_COUNT as u64).collect();
    let mut vectors = Vec::with_capacity(ROW_COUNT * DIM);
    for _ in 0..ROW_COUNT {
        vectors.extend(rng.next_vector(DIM));
    }
    let query = rng.next_vector(DIM);

    let provider = ParallelSearchProvider;
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");

    let measurement = run(&config, || {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &query,
            k: TOP_K,
        };
        provider
            .search(input)
            .expect("search must succeed for well-formed synthetic input")
    })
    .expect("measurement must satisfy protocol minimums");

    // `stats::Summary` は p95 を持たないため（median/q1/q3 のみ）、生サンプルから
    // 本ベンチ側で算出する（`harness/mod.rs` 側の契約は変更しない）。
    let mut sorted = measurement.samples.clone();
    sorted.sort();
    let p95_rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    let p95_idx = p95_rank
        .saturating_sub(1)
        .min(sorted.len().saturating_sub(1));
    let p95 = sorted.get(p95_idx).copied().unwrap_or_default();

    println!(
        "parallel_search: rows={ROW_COUNT} dim={DIM} k={TOP_K} median={:?} q1={:?} q3={:?} p95={p95:?} n={}",
        measurement.summary.median,
        measurement.summary.q1,
        measurement.summary.q3,
        measurement.samples.len()
    );
}
