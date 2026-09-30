//! `CREATE INDEX` の所要時間が既存行数に依存しないことの時間判定テスト
//! （Issue #1201・INDEX-7・TASK-206。ポインタ: `docs/spec/04-behavior/` の
//! INDEX-7 と、同一基準の TABLE-4 → `catalog_latency.rs`）。
//!
//! `CREATE INDEX` は宣言（カタログ書き込み）だけを行い、索引本体の構築は次の
//! クエリ時まで遅延される。その帰結として所要時間が行数に比例しないことを、行数の
//! 異なる 2 つの DB の比率で判定する（構築が遅延されること自体は
//! `index_declaration_targets.rs` がカウンタで決定的に固定する）。
//!
//! 計測設計は `catalog_latency.rs`（Issue #1164）と同じ: fsync の多い兄弟テストとの
//! 並走を断つため独立バイナリとし、共通のタイミングロックを取得する。ラウンドごとに
//! small/large を交互（ABAB）に計測し、加算的ノイズに強い最小値で比較する。判定閾値
//! （`ratio < 5.0`）は TABLE-4 と同じで、弱体化ではない。実測値は記載しない。

use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

#[path = "../src/test_util/timing_lock.rs"]
mod timing_lock;
use timing_lock::acquire_timing_lock;

const DIM: u32 = 8;
const ROUNDS: usize = 9;

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `rows` 行を持つ DB を用意して `EngineCore` を返す（`EngineCore` 構築前に
/// `tenant::insert_rows` で一括投入する。計測の前提データ）。
fn core_with_rows(label: &str, rows: usize) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let metadata = engine::row_codec::encode_scalar_columns(
        &schema(),
        &[Value::Null, Value::Text("ja".to_string())],
    )
    .expect("encode metadata");
    let embedding = vec![0.5_f32; DIM as usize];
    let batch: Vec<(u64, RowInput<'_>)> = (0..rows as u64)
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
    let op_id = OperationId::parse(&format!("seed-{label}")).expect("valid operation id");
    engine::tenant::insert_rows(&storage, "docs", &ctx(), &batch, &op_id).expect("seed rows");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

/// `create_sql(name)` の `CREATE INDEX` を small/large で ABAB 交互に計測し、
/// 最小値の比率が閾値未満であることを確認する。計測外で `DROP INDEX` して
/// ラウンド間の状態を揃える。
fn assert_create_index_latency_independent_of_rows(
    label: &str,
    create_sql: impl Fn(&str) -> String,
) {
    let _timing_guard = acquire_timing_lock();
    let (small, _small_guard) = core_with_rows(&format!("index7-{label}-small"), 1_000);
    let (large, _large_guard) = core_with_rows(&format!("index7-{label}-large"), 10_000);

    // ウォームアップ（初回のファイルアクセスの追加コストを計測の外側で吸収する）。
    for core in [&small, &large] {
        ddl(core, &create_sql("idx_warmup"));
        ddl(core, "DROP INDEX idx_warmup");
    }

    let mut small_durations = Vec::with_capacity(ROUNDS);
    let mut large_durations = Vec::with_capacity(ROUNDS);
    for i in 0..ROUNDS {
        let name = format!("idx_{i}");
        let sql = create_sql(&name);
        let start = Instant::now();
        ddl(&small, &sql);
        small_durations.push(start.elapsed());
        ddl(&small, &format!("DROP INDEX {name}"));

        let start = Instant::now();
        ddl(&large, &sql);
        large_durations.push(start.elapsed());
        ddl(&large, &format!("DROP INDEX {name}"));
    }

    // 負荷ノイズは加算的なため、代表値は最小値（本来のコストの推定量）とする。
    let best_small = small_durations.into_iter().min().unwrap_or(Duration::ZERO);
    let best_large = large_durations.into_iter().min().unwrap_or(Duration::ZERO);
    let ratio = best_large.as_secs_f64().max(1e-9) / best_small.as_secs_f64().max(1e-9);
    assert!(
        ratio < 5.0,
        "{label}: CREATE INDEX best latency scaled with row count too much: small={best_small:?}, large={best_large:?}, ratio={ratio}"
    );
}

/// スカラー索引宣言（`CREATE INDEX ... (lang)`）の所要時間は行数に依存しない。
#[test]
fn index7_create_scalar_index_latency_is_independent_of_row_count() {
    assert_create_index_latency_independent_of_rows("scalar", |name| {
        format!("CREATE INDEX {name} ON docs (lang)")
    });
}

/// HNSW 索引宣言（`CREATE INDEX ... USING hnsw (embedding)`）の所要時間も行数に
/// 依存しない（構築は次のクエリ時まで遅延される）。
#[test]
fn index7_create_hnsw_index_latency_is_independent_of_row_count() {
    assert_create_index_latency_independent_of_rows("hnsw", |name| {
        format!("CREATE INDEX {name} ON docs USING hnsw (embedding)")
    });
}
