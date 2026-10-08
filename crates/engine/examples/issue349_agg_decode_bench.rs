//! Issue #349（集計・`GROUP BY` の行デコードをスクラッチ再利用方式へ統一）の
//! 前後比較専用の手動実行ハーネス（`multi_dim_bench.rs`・`concurrent_write_bench.rs`
//! と同じく `cargo test` の対象には含めない。`cargo run -p fandhe-db-engine --release --example
//! issue349_agg_decode_bench` で実行する）。時間依存の測定値を CI のアサーションに
//! 混ぜない方針（`.claude/rules/coding-rust.md`）のため、`tests/sql_aggregate.rs`・
//! `tests/sql_group_by.rs`（正しさの回帰テスト・CI 常時実行）とはファイルを
//! 分離している。
//!
//! `COUNT(*)`・5 集計・`GROUP BY … HAVING` の 3 フェーズについて、25,000 行の
//! テーブルへ `EngineCore::execute_sql` を複数回発行し p50/p95 を記録する
//! （行数・繰り返し回数は定数で上限固定。security.md「不安全な設計｜無制限
//! リソース確保」対応）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::using_operation_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

const TENANT_ID: &str = "bench-tenant";
const ROWS: u64 = 25_000;
const DIM: u32 = 8;
const REPEATS: usize = 21;

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

struct CleanupGuard(PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn unique_db_path() -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fandhe-db-engine-issue349-bench-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

fn embedding_for(id: u64) -> Vec<f32> {
    (0..DIM).map(|i| (id + i as u64) as f32).collect()
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// `EngineCore::insert_row`（`catalog::user_rows_table_def` が管理する SQL 表層の
/// per-table 行ストア）経由で書き込む。`storage::Storage::put_batch` は SQL 表層とは
/// 別の生 `ROWS_TABLE` へ書くため（`aggregate.rs`/`group_by.rs` が走査するのは
/// per-table 行ストアの方）、集計クエリの計測にはこちらを使う。テーブル自体は
/// 呼び出し元が別途 `Storage::create_table` で作成済みであることを前提とする。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    let schema = schema();
    let langs = ["ja", "en", "fr"];
    for id in 0..ROWS {
        let lang = langs[(id % 3) as usize];
        let metadata = engine::row_codec::encode_scalar_columns(
            &schema,
            &[
                engine::row_codec::Value::Null,
                engine::row_codec::Value::Text(lang.to_string()),
            ],
        )
        .expect("encode metadata");
        let embedding = embedding_for(id);
        let row = RowInput {
            tenant_id: TENANT_ID,
            visibility: Visibility::Public,
            embedding: &embedding,
            metadata: &metadata,
        };
        let op_id = OperationId::parse(&format!("issue349-seed-{id}")).expect("op id");
        core.insert_row(ctx, "docs", id, &row, Some(&op_id))
            .expect("insert row");
    }
}

fn percentiles(mut samples: Vec<Duration>) -> (Duration, Duration) {
    samples.sort();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    (p50, p95)
}

fn run_phase(core: &EngineCore, ctx: &PolicyContext, label: &str, sql: &str) {
    let mut samples = Vec::with_capacity(REPEATS);
    let mut last_result = None;
    for _ in 0..REPEATS {
        let start = Instant::now();
        let result = core.execute_sql(ctx, sql).expect("query should succeed");
        samples.push(start.elapsed());
        last_result = Some(result);
    }
    let (p50, p95) = percentiles(samples);
    println!(
        "{{\"phase\":\"{label}\",\"rows\":{ROWS},\"p50_ms\":{:.3},\"p95_ms\":{:.3},\"result\":{:?}}}",
        p50.as_secs_f64() * 1000.0,
        p95.as_secs_f64() * 1000.0,
        last_result.unwrap().rows,
    );
}

fn main() {
    let path = unique_db_path();
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
    }

    let core = EngineCore::open(&path).expect("open engine core");
    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant");
    seed(&core, &ctx);

    run_phase(&core, &ctx, "agg_count", "SELECT COUNT(*) FROM docs");
    run_phase(
        &core,
        &ctx,
        "agg_multi",
        "SELECT COUNT(*), COUNT(embedding), MIN(id), MAX(id), SUM(id) FROM docs",
    );
    run_phase(
        &core,
        &ctx,
        "group_by_having",
        "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n > 0",
    );
}
