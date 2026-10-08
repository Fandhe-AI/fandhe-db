//! 2000 次元検索の実測ハーネス（TASK-151、対象ビヘイビア: EXT-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-151・`docs/spec/04-behavior/extensions.md`
//! EXT-2・`docs/spec/06-roadmap.md` MS-6）。
//!
//! `cargo test` の対象には含めない手動実行専用ツール（`cargo run -p fandhe-db-engine --release
//! --example high_dim_bench` で実行する）。時間依存の測定値を CI のアサーションに
//! 混ぜない方針（.claude/rules/coding-rust.md）のため、`tests/extensions.rs` の
//! `ext2_2000_dim_*`（正しさの回帰テスト・CI 常時実行）とはファイルを分離している
//! （`crates/engine/examples/multi_dim_bench.rs`（TASK-91）と同じ構成方針）。
//!
//! 比較設計:
//! - **A: 単発経路の次元スケーリング** — `kernel.rs::SearchProvider` を直接叩き、
//!   768 次元・2000 次元それぞれの単発クエリ p50/p95/max と、その比率（理論値は
//!   演算量ベースで ≒ 2000/768 ≒ 2.6 倍）を計測する。既定 provider
//!   （`search_engine::default_engine()`。現時点はスレッド並列のみで SIMD 化はしない。
//!   `parallel_search.rs` 冒頭コメント参照）に加え、参照実装 `CpuScalarProvider`
//!   （単一スレッド・スカラー）でも同様に計測し、並列化による短縮幅も併記する。
//! - **B: 共存状態の end-to-end** — `EngineCore::search`（製品の実検索経路）で、
//!   (a) 2000 次元テーブル単独 DB と (b) 768 + 2000 次元テーブル共存 DB のそれぞれで
//!   2000 次元テーブルへの検索 p50/p95 を計測し、共存による劣化の有無を比較する。
//!
//! 出力は `docs/design/high-dim-2000-reverification.md` の実測結果表に転記する。
//! 行数・次元・反復回数はすべて定数で上限固定し、無制限確保をしない
//! （security.md「不安全な設計｜無制限リソース確保（DoS）」）。外部クレートには
//! 依存しない（dependency-policy 準拠）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

const TENANT_ID: &str = "bench-tenant";

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 実行終了時（panic 時含む）に一時 DB ファイルを確実に削除するガード
/// （`tests/extensions.rs::CleanupGuard` と同じ方針。手動実行専用ツールのため
/// ヘルパ共通化はせず複製する）。
struct CleanupGuard(PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-task151-highdim-bench-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

/// 決定論的な擬似乱数生成器（外部クレート非依存の xorshift32。`tests/extensions.rs`
/// と同型）。
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

fn make_embedding(dim: u32, seed: u32) -> Vec<f32> {
    // seed 0 は xorshift の不動点になるため非ゼロへ置換する。`seed | 1` だと
    // 隣接する偶奇シード（0/1・2/3 等）が同じ値へ潰れ、実質半数の異なる
    // ベクトルしか得られなくなる問題があった。置換先を実際に使われる seed=1
    // と衝突しない u32::MAX にすることで、0 と 1 を含むすべての隣接シードが
    // 異なるベクトルを生成する（`tests/extensions.rs::make_embedding` と同型修正）。
    let seeded = if seed == 0 { u32::MAX } else { seed };
    let mut rng = Xorshift32(seeded);
    (0..dim).map(|_| rng.next_f32()).collect()
}

// --- A: 単発経路の次元スケーリング（provider 直接） -------------------------------

/// パート A で使う行数（`MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES` の範囲内。
/// 2000 次元 × 20,000 行 × 4 バイトは約 160 MiB。手動実行が現実的な時間で終わるよう
/// 控えめに固定する）。
const PROVIDER_ROW_COUNT: usize = 20_000;
const PROVIDER_TOP_K: usize = 10;
const PROVIDER_WARMUP_ITERS: usize = 3;
const PROVIDER_MEASURED_ITERS: usize = 20;

struct LatencyStats {
    p50: Duration,
    p95: Duration,
    max: Duration,
}

fn summarize_latencies(mut latencies: Vec<Duration>) -> LatencyStats {
    latencies.sort_unstable();
    let n = latencies.len();
    let percentile = |p: f64| -> Duration {
        if n == 0 {
            return Duration::ZERO;
        }
        let rank = ((n as f64) * p).ceil() as usize;
        let idx = rank.saturating_sub(1).min(n - 1);
        latencies[idx]
    };
    LatencyStats {
        p50: percentile(0.50),
        p95: percentile(0.95),
        max: latencies.last().copied().unwrap_or(Duration::ZERO),
    }
}

/// `dim` 次元・`PROVIDER_ROW_COUNT` 行のフラット化済みベクトルとクエリを用意し、
/// `provider` へ単発検索を `PROVIDER_MEASURED_ITERS` 回投げて p50/p95/max を返す
/// （事前 `PROVIDER_WARMUP_ITERS` 回はウォームアップとして計測対象外）。
fn measure_provider(provider: &dyn SearchProvider, dim: u32) -> LatencyStats {
    let ids: Vec<u64> = (0..PROVIDER_ROW_COUNT as u64).collect();
    let vectors: Vec<f32> = (0..PROVIDER_ROW_COUNT as u32)
        .flat_map(|i| make_embedding(dim, i + 1))
        .collect();
    let query = make_embedding(dim, PROVIDER_ROW_COUNT as u32 + 1);

    let run_once = || {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim,
            query: &query,
            k: PROVIDER_TOP_K,
        };
        let started = Instant::now();
        let hits = provider
            .search(input)
            .expect("provider search should succeed");
        let elapsed = started.elapsed();
        assert!(!hits.is_empty(), "provider returned no hits");
        elapsed
    };

    for _ in 0..PROVIDER_WARMUP_ITERS {
        run_once();
    }
    let latencies: Vec<Duration> = (0..PROVIDER_MEASURED_ITERS).map(|_| run_once()).collect();
    summarize_latencies(latencies)
}

fn print_latency_row(label: &str, s: &LatencyStats) {
    println!(
        "{:<38} | {:>10.3?} | {:>10.3?} | {:>10.3?}",
        label, s.p50, s.p95, s.max
    );
}

/// パート B 専用の行出力。ヘッダーの 4 列目は `max` ではなく `db_size` であり、
/// `print_latency_row`（4 列目に `s.max` を出す）を使うと db_size ラベルに反して
/// 常に max レイテンシが出力され、再実行時に latency と DB size を取り違える恐れが
/// あった（Cursor Bugbot 指摘）。db_size は明示的な数値列として出す。
fn print_engine_row(label: &str, s: &LatencyStats, db_size_bytes: u64) {
    println!(
        "{:<38} | {:>10.3?} | {:>10.3?} | {:>10} bytes",
        label, s.p50, s.p95, db_size_bytes
    );
}

fn run_part_a() {
    println!(
        "\n=== Part A: single-shot query dimension scaling (row_count={PROVIDER_ROW_COUNT}, k={PROVIDER_TOP_K}) ==="
    );
    println!(
        "{:<38} | {:>10} | {:>10} | {:>10}",
        "provider/dim", "p50", "p95", "max"
    );

    let parallel = search_engine::default_engine();
    let scalar: Box<dyn SearchProvider> = Box::new(CpuScalarProvider);

    let stats_parallel_768 = measure_provider(parallel.as_ref(), 768);
    let stats_parallel_2000 = measure_provider(parallel.as_ref(), 2000);
    let stats_scalar_768 = measure_provider(scalar.as_ref(), 768);
    let stats_scalar_2000 = measure_provider(scalar.as_ref(), 2000);

    print_latency_row("default(parallel) dim=768", &stats_parallel_768);
    print_latency_row("default(parallel) dim=2000", &stats_parallel_2000);
    print_latency_row("CpuScalarProvider dim=768", &stats_scalar_768);
    print_latency_row("CpuScalarProvider dim=2000", &stats_scalar_2000);

    let ratio = |a: Duration, b: Duration| -> f64 {
        if b.as_secs_f64() > 0.0 {
            a.as_secs_f64() / b.as_secs_f64()
        } else {
            f64::NAN
        }
    };
    println!(
        "ratio (2000/768, p50): default(parallel)={:.3}, CpuScalarProvider={:.3} (理論値の目安 ≒ 2.6)",
        ratio(stats_parallel_2000.p50, stats_parallel_768.p50),
        ratio(stats_scalar_2000.p50, stats_scalar_768.p50),
    );
}

// --- B: 共存状態の end-to-end（`EngineCore::search`） ------------------------------

/// パート B で使う 1 テーブルあたりの行数（`MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES`
/// の範囲内。共存 DB は 768 次元テーブルも同じ行数を持つため、DB ファイルサイズは
/// 単独条件よりおおむね (768+2000)/2000 倍になる）。
const ENGINE_ROW_COUNT: u32 = 10_000;
const ENGINE_TOP_K: usize = 10;
const ENGINE_WARMUP_ITERS: usize = 3;
const ENGINE_MEASURED_ITERS: usize = 20;

fn vector_table_schema(name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

fn seed_table(storage: &Storage, name: &str, dim: u32, row_count: u32) {
    storage
        .create_table(&vector_table_schema(name, dim))
        .unwrap_or_else(|e| panic!("create_table({name}) failed: {e}"));
    let embeddings: Vec<Vec<f32>> = (0..row_count).map(|i| make_embedding(dim, i + 1)).collect();
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
    // テナント境界付きバッチ API 経由で投入する（生の `Storage::insert_rows_into_table`
    // は codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した）。
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

/// DB ファイルのバイト長を取得する。この値は
/// `docs/design/high-dim-2000-reverification.md` の実測結果表へ転記する測定契約の
/// 一部であるため、パス消失・権限・I/O エラーを 0 バイトとして握り潰さず
/// `Err` として呼び出し側（`run_part_b`）へ伝播し、測定を打ち切る（fail-closed）。
fn file_size(path: &PathBuf) -> std::io::Result<u64> {
    std::fs::metadata(path).map(|m| m.len())
}

/// `core.search(ctx, "emb2000", query, ENGINE_TOP_K)` を `ENGINE_MEASURED_ITERS` 回
/// 計測する（事前 `ENGINE_WARMUP_ITERS` 回はウォームアップ）。
fn measure_engine_search(core: &EngineCore, ctx: &PolicyContext, query: &[f32]) -> LatencyStats {
    let run_once = || {
        let started = Instant::now();
        let hits = core
            .search(ctx, "emb2000", query, ENGINE_TOP_K)
            .expect("EngineCore::search should succeed");
        let elapsed = started.elapsed();
        assert!(!hits.is_empty(), "search returned no hits");
        elapsed
    };
    for _ in 0..ENGINE_WARMUP_ITERS {
        run_once();
    }
    let latencies: Vec<Duration> = (0..ENGINE_MEASURED_ITERS).map(|_| run_once()).collect();
    summarize_latencies(latencies)
}

fn run_part_b() -> std::io::Result<()> {
    println!(
        "\n=== Part B: end-to-end coexistence (row_count/table={ENGINE_ROW_COUNT}, k={ENGINE_TOP_K}) ==="
    );
    println!(
        "{:<38} | {:>10} | {:>10} | {:>12}",
        "config", "p50", "p95", "db_size"
    );

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    let query = make_embedding(2000, ENGINE_ROW_COUNT + 1);

    // (a) 2000 次元テーブル単独 DB。
    // `file_size` はブロックを抜けて `core`（と内部の `Storage`）が drop された後に
    // 呼ぶ（redb はプロセス生存中ファイルを成長させるため、`Storage` が生きたまま
    // 読むと確定前のファイル長を拾い得る。close 後に読むことで両条件のファイル
    // サイズを公平に比較する）。
    let path_solo = unique_db_path("solo-2000");
    let _guard_solo = CleanupGuard(path_solo.clone());
    let stats_solo = {
        let storage = Storage::open(&path_solo).expect("open solo storage");
        seed_table(&storage, "emb2000", 2000, ENGINE_ROW_COUNT);
        let core = EngineCore::from_storage(storage, search_engine::default_engine());
        let stats = measure_engine_search(&core, &ctx, &query);
        drop(core);
        stats
    };
    let size_solo = file_size(&path_solo)?;
    print_engine_row("solo(2000 only)", &stats_solo, size_solo);

    // (b) 768 + 2000 次元テーブル共存 DB。
    let path_coexist = unique_db_path("coexist-768-2000");
    let _guard_coexist = CleanupGuard(path_coexist.clone());
    let stats_coexist = {
        let storage = Storage::open(&path_coexist).expect("open coexist storage");
        seed_table(&storage, "emb768", 768, ENGINE_ROW_COUNT);
        seed_table(&storage, "emb2000", 2000, ENGINE_ROW_COUNT);
        let core = EngineCore::from_storage(storage, search_engine::default_engine());
        let stats = measure_engine_search(&core, &ctx, &query);
        drop(core);
        stats
    };
    let size_coexist = file_size(&path_coexist)?;
    print_engine_row("coexist(768+2000)", &stats_coexist, size_coexist);

    let delta_pct = |a: Duration, b: Duration| -> f64 {
        if b.as_secs_f64() > 0.0 {
            (a.as_secs_f64() - b.as_secs_f64()) / b.as_secs_f64() * 100.0
        } else {
            f64::NAN
        }
    };
    println!(
        "coexistence overhead (p95, coexist vs solo): {:+.1}%",
        delta_pct(stats_coexist.p95, stats_solo.p95)
    );
    Ok(())
}

fn main() -> std::io::Result<()> {
    run_part_a();
    run_part_b()
}
