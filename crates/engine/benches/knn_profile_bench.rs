//! KNN 経路の段別内訳プロファイル（Issue #362。親 Issue #361「ベクトル検索・
//! ストレージレイアウト最適化」Phase 5 の起点タスク）。
//!
//! SQL 表層経由の KNN（`SELECT id FROM docs ORDER BY embedding <=> '<vec>' LIMIT 10`。
//! 25,000 行・dim128）のレイテンシについて、どの段が支配的かを段別に実測する。
//! `sql_c1_bench.rs`（TASK-83）は SQL 表層全体の p95 を単一の数値として測るのに対し、
//! 本ベンチは redb 走査・ヘッダデコード・f32 デコード・arena 構築・距離計算・Top-k
//! 選出の内訳を分離する（`docs/design/knn-stage-profile.md` 参照）。
//!
//! # 計測条件（受け入れ条件 2）
//!
//! - SQL 表層経路（S0/S0'）は `PrefilterCache`（`core.rs`。`EngineCore::search`
//!   専用・Rust API 直呼び用）を経由しない。ただし `sql::exec::execute_statement`
//!   の `VectorArena` はテーブル単位世代整合キャッシュ（`sql/arena_cache.rs::
//!   SqlArenaCache`。Issue #363）を経由するため、「クエリ毎に候補行を redb から
//!   再デコードする e2e コスト」は S0-cold（毎サンプル新規 `EngineCore` で
//!   `SqlArenaCache` を空の状態から測る）でのみ表れる。S0-hot は同一
//!   `EngineCore` を使い回し `SqlArenaCache` ヒット時のオーバーヘッドを測る
//!   （下記「`SqlArenaCache` と S0 の cold/hot 分離」節参照）。
//! - 既定 provider は `ParallelSearchProvider`（`search_engine::default_engine()`）。
//!   対照として単線 `CpuScalarProvider` も測定する。
//!
//! # 段別分解の測定設計
//!
//! DB ファイルを一度構築したのち、フェーズごとに開き直して測定する
//! （`crates/engine/tests/persistence.rs` と同じ「一度 drop してから生 `redb::Database`
//! を再オープンする」手順）。
//!
//! | 段 | 内容 |
//! | --- | --- |
//! | S0-cold | `EngineCore::execute_sql` 経由の SQL 表層 KNN（e2e。サンプルごとに
//! 新規 `Storage::open` + `EngineCore::from_storage` で `SqlArenaCache`〔Issue #363〕
//! を空の状態から測る。下記「`SqlArenaCache` と S0 の cold/hot 分離」節参照） |
//! | S0-hot | S0-cold と同じクエリを単一 `EngineCore` へ 40 回以上繰り返し、初回
//! 以降は `SqlArenaCache` がヒットする状態（キャッシュヒット時のオーバーヘッド） |
//! | S0' | `SELECT COUNT(*) FROM docs`（走査＋ヘッダ＋RLS の SQL 経由クロスチェック。
//! 集計クエリのため `SqlArenaCache`〔`VectorArena` 専用〕の対象外） |
//! | S1 | 生 `redb::Database` 再オープンでの per-entry 走査のみ |
//! | S2 | S1 ＋ ヘッダデコード（`harness::knn_profile::decode_header_reimpl`） |
//! | S3 | S2 ＋ f32 デコード（`harness::knn_profile::decode_row_reimpl`） |
//! | S4 | `VectorArena::build_filtered`（pub API。クエリ毎再構築の実コスト） |
//! | S5 | 距離計算＋Top-k（`ParallelSearchProvider`・`CpuScalarProvider`） |
//! | S5' | 距離計算のみ（`isa::current().dot` を全行へ適用して総和。Top-k なし） |
//!
//! ## `SqlArenaCache` と S0 の cold/hot 分離
//!
//! Issue #363 で SQL 表層 `SELECT`（`USING PLAN` 展開経由を含む）の `VectorArena` が
//! テーブル単位世代整合キャッシュ（`sql/arena_cache.rs::SqlArenaCache`）を経由する
//! ようになったため、同一 `EngineCore` へ同一クエリを繰り返すだけでは初回以降
//! ほぼ全呼び出しがキャッシュヒットになり、「クエリ毎に候補行を redb から
//! 再デコードする e2e コスト」を S0 が表さなくなる（codex-review P1-1 指摘・
//! PR #378）。`SqlArenaCache`／`EngineCore` に既存の invalidate/clear 相当の
//! 公開 API は無く、世代を進めるダミー DML はコーパスのデータを変えてしまうため
//! 使わない。代わりに S0-cold は、warmup・計測フェーズの各サンプルごとに計測
//! 区間の外側（`Instant::now()` の前）で新規 `Storage::open` + `EngineCore::
//! from_storage` を構築し、常に空の `SqlArenaCache` から `execute_sql` を計測する
//! （`harness::protocol::run`/`run_bounded_retain` は setup を計測区間の外側に
//! 置けない〔クロージャ 1 本に setup と計測を束ねる契約〕ため、本段のみ独自の
//! warmup/計測ループを持ち `harness::stats::summarize` で統計手順を揃える）。
//! S0-hot は従来どおり単一 `EngineCore` を使い回し、キャッシュヒット時の
//! オーバーヘッドを別段として測る。
//!
//! 整合性検証（fail-closed）: S1〜S3 の走査行数が一致すること、S3 のデコード結果を
//! `Storage::scan()`（pub API・完全デコード）の結果と突き合わせて一致すること
//! （`tests/knn_profile_accept.rs` が同じ突き合わせを時間非依存の回帰として持つ。
//! 本ベンチは実データ規模での 1 回限りのクロスチェックを行う）、S0-cold・S0-hot
//! それぞれの結果行数が `TOP_K` に一致すること、S0' の `COUNT(*)` が計測外の
//! 再実行で `TOTAL_ROWS` と一致することを実行時にアサートする。不一致ならベンチは
//! エラー終了し測定値を出力しない。
//!
//! # `dot_lanes` の実アセンブリ確認（受け入れ条件 3）
//!
//! [`dot_wrapper`] は `engine::isa::current().dot(a, b)` を呼ぶだけの
//! `#[inline(never)]` の薄いラッパーで、production コードは無変更のまま
//! `cargo bench --bench knn_profile_bench -p fandhe-db-engine --no-run` でビルドしたバイナリを
//! `objdump -d` で逆アセンブルする手動手順の入口にする（`docs/design/
//! knn-stage-profile.md`「`dot_lanes` の実アセンブリ確認」節に手順・結果を記録する）。
//!
//! # CI・出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線しない
//! （`bench-hybrid` と同方針）。`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で
//! 拒否する（[`harness::knn_profile::refuse_under_github_actions`]）。`make
//! bench-knn-profile`（Makefile）から実行する。判定ロジック自体（時間非依存）は
//! `harness::knn_profile` にあり `tests/knn_profile_accept.rs` で `make ci` 側から
//! 回帰検証する。
//!
//! 既定 dim128（`harness::bench_engine::DEFAULT_BENCH_DIM`）。`BENCH_KNN_PROFILE_DIM`
//! （正整数・上限 `harness::bench_engine::MAX_BENCH_DIM`。Issue #466）で上書きできる。
//! dim=768／1536 が Issue #365 で採否の判別変数と判明したため、S0〜S5' すべての
//! 段が同じベクトル次元数で測れるようにする。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::knn_profile::{
    assert_scan_row_counts_match, decode_header_reimpl, decode_row_reimpl, ns_per_row,
    refuse_under_github_actions, render_diff_line, render_index_memory_line,
    render_kernel_isa_line, render_stage_line, requires_hnsw_stats_check, resident_label_for_token,
    scaled_rows, stage_diff_ns_per_row, KnnProfileError,
};
use harness::proc_stats::{read_vm_hwm_kb, read_vm_rss_kb};
use harness::protocol::{run, run_bounded_retain, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::{c1_statement, c1_where_statement, vector_literal};
use harness::stats;

use std::hint::black_box;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hnsw::{HnswIndex, HnswParams, Ratio, ResidentPrecision, ValidatedHnswParams};
use engine::isa;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value as RowValue};
use engine::search_engine::{self, SearchEngineKind};
use engine::sql::exec::Cell;
use engine::storage::{RowInput, Storage, Visibility};
use engine::{arena::VectorArena, tenant};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// feature_bench の `vector_knn` フェーズ実測条件を再現する規模（Issue #362。
/// テナント A 20,000 行 ＋ テナント B 5,000 行 ＝ 計 25,000 行）。
const TENANT_A: &str = "tenant-a";
const TENANT_A_ROWS: usize = 20_000;
const TENANT_B: &str = "tenant-b";
const TENANT_B_ROWS: usize = 5_000;
const TOTAL_ROWS: usize = TENANT_A_ROWS + TENANT_B_ROWS;
const TOP_K: usize = 10;

/// `BENCH_KNN_PROFILE_VISIBLE_RATIO` が受理する分母の上限（Issue #487）。行数
/// スケール上限（[`MAX_SWEEP_SCALE`]）× [`TOTAL_ROWS`] でも可視行数が [`TOP_K`]
/// 未満に潰れない範囲を確保しつつ、無制限な `bucket` 列挙の生成を防ぐ。
const MAX_VISIBLE_RATIO_DENOMINATOR: u32 = 1_000;

/// `BENCH_KNN_PROFILE_SCALE`（Issue #487。スイープ専用。既定経路の行数は不変）の
/// 上限倍率。`TOTAL_ROWS * MAX_SWEEP_SCALE` が `hnsw::MAX_HNSW_NODES`
/// （1,000,000）ちょうどになる値とし、`harness::bench_engine::parse_scale` と
/// 同じ「呼び出し元が上限を計算して渡す」契約に従う。
const MAX_SWEEP_SCALE: u64 = 40;
const TABLE: &str = "docs";
const COLUMN: &str = "embedding";
const SEED_BATCH_ROWS: usize = 5_000;

/// 生 `redb::Database` 再オープン時に走査する行テーブル。物理キー型・テーブル名は
/// `crates/engine/src/catalog.rs::user_rows_table_name`（`"user_rows/{table}"`）・
/// `crates/engine/src/storage.rs::RowStoreTableDef` の契約をベンチ内で複製した
/// ものであり、ドリフト検出は `tests/knn_profile_accept.rs` が
/// `Storage::scan()`（pub API）との突き合わせで行う（モジュール冒頭コメント参照）。
const ROW_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("user_rows/docs");

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("knn_profile_bench: {msg}");
    std::process::exit(1);
}

/// S0-cold／S0-hot が使う `EngineCore` を `knn_engine`（`BENCH_KNN_PROFILE_ENGINE`。
/// Issue #413）に応じて構築する。S1〜S5' は生 redb 走査・provider 直呼び等の
/// エンジン非依存経路のため本関数を使わない。`sparse_visited_max_override`
/// （`BENCH_KNN_PROFILE_SPARSE_VISITED_MAX`。Issue #497）は `knn_engine ==
/// Hnsw` のときのみ意味を持ち、`None`（未設定）なら既定値（常に dense）の
/// まま構築する——未計測の性能変更を既定にしない方針（`docs/design/
/// benchmark-judgement-policy.md`）に沿い、この knob 自体が出力・処理を
/// 変えるのは明示的に指定した場合に限る。
fn build_core_for(
    knn_engine: harness::bench_engine::BenchEngine,
    storage: Storage,
    sparse_visited_max_override: Option<usize>,
) -> EngineCore {
    match knn_engine {
        harness::bench_engine::BenchEngine::BruteForce => {
            EngineCore::from_storage(storage, search_engine::default_engine())
        }
        harness::bench_engine::BenchEngine::Hnsw => {
            let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid HnswParams::default()");
            let kind = match (kind, sparse_visited_max_override) {
                (SearchEngineKind::Hnsw(validated), Some(max)) => {
                    SearchEngineKind::Hnsw(validated.with_sparse_visited_max(max))
                }
                (kind, _) => kind,
            };
            EngineCore::from_storage_with_engine(storage, kind)
        }
        harness::bench_engine::BenchEngine::HnswF16 => {
            // f16 常駐 opt-in（Issue #514・#516。`recall_engine.rs::RecallEngine::
            // HnswF16` と同一構築経路）。
            let validated = ValidatedHnswParams::new(HnswParams::default())
                .expect("valid HnswParams::default()")
                .with_resident_precision(ResidentPrecision::F16);
            EngineCore::from_storage_with_engine(storage, SearchEngineKind::Hnsw(validated))
        }
        harness::bench_engine::BenchEngine::HnswI8 => {
            // I8（SQ8）常駐 opt-in（Issue #521・#523。`recall_engine.rs::
            // RecallEngine::HnswI8` と同一構築経路）。
            let validated = ValidatedHnswParams::new(HnswParams::default())
                .expect("valid HnswParams::default()")
                .with_resident_precision(ResidentPrecision::I8);
            EngineCore::from_storage_with_engine(storage, SearchEngineKind::Hnsw(validated))
        }
    }
}

/// 段間差分（[`stage_diff_ns_per_row`]）の結果を出力する。S1〜S4・S5prime-S5_scalar
/// はいずれも段ごとに独立した `run` 呼び出し（別トランザクション・別 warmup/
/// サンプル列）による中央値どうしの比較であり、測定ノイズにより逆転しうる
/// （`Err(KnnProfileError::NonMonotonicStages)`）。この逆転は行数不一致・レイアウト
/// ドリフト等の整合性検証（`fail_closed` で維持）とは性質が異なるため、ベンチを
/// 失敗させず「未確定（n/a）」として出力を継続する（codex-review P2-2 指摘・
/// PR #378）。
fn print_stage_diff_or_unconfirmed(from: &str, to: &str, diff: Result<f64, KnnProfileError>) {
    match diff {
        Ok(value) => println!("{}", render_diff_line(from, to, value)),
        Err(e) => println!(
            "diff({from}->{to}): n/a (独立計測どうしの中央値比較のため測定ノイズにより \
             逆転・未確定として継続: {e})"
        ),
    }
}

fn main() {
    // メモリ計測の子プロセス経路（Issue #516。`INDEX_MEMORY_CHILD_ENV` 設定時の
    // み）。設定されていれば 1 点分の索引単体メモリ計測だけを行いここで終了し、
    // 通常の計測経路（下記）には進まない（`hnsw_parallel_build_bench.rs::
    // run_memory_child_if_requested` と同型）。
    run_index_memory_child_if_requested();

    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    // ANN opt-in（Issue #413）。`BENCH_KNN_PROFILE_ENGINE` は S0-cold／S0-hot
    // （SQL 表層 e2e）の `EngineCore` 構築にのみ効く。S1〜S5' は構造的にエンジン
    // 非依存（生 redb 走査・`VectorArena::build_filtered`・provider 直呼び）の
    // ため変更しない（S5 を hnsw として提示しない。モジュール冒頭コメント参照）。
    let knn_engine = match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_ENGINE")
        .and_then(|raw| harness::bench_engine::parse_engine(raw.as_deref()))
    {
        Ok(e) => e,
        Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_ENGINE: {e}")),
    };

    // ベクトル次元数（Issue #466）。S1〜S5' も含め本ファイル全体で同じ値を使う。
    let dim: usize =
        match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_DIM").and_then(|raw| {
            harness::bench_engine::parse_dim(
                raw.as_deref(),
                harness::bench_engine::DEFAULT_BENCH_DIM,
                harness::bench_engine::MAX_BENCH_DIM,
            )
        }) {
            Ok(d) => d as usize,
            Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_DIM: {e}")),
        };

    // 可視比率 × 行数の損益分岐点スイープ（Issue #487）。`BENCH_KNN_PROFILE_
    // VISIBLE_RATIO` 未設定（既定）時は本節が一切分岐せず、以降の出力・処理は
    // 本 Issue 導入前と完全に同一のまま進む（既定経路の出力不変を保つ設計）。
    let full_scan_ratio_override =
        match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_FULL_SCAN_RATIO")
            .and_then(|raw| harness::bench_engine::parse_full_scan_ratio(raw.as_deref()))
        {
            Ok(v) => v,
            Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_FULL_SCAN_RATIO: {e}")),
        };
    if full_scan_ratio_override.is_some()
        && !matches!(
            knn_engine,
            harness::bench_engine::BenchEngine::Hnsw | harness::bench_engine::BenchEngine::HnswF16
        )
    {
        fail_closed(
            "BENCH_KNN_PROFILE_FULL_SCAN_RATIO requires BENCH_KNN_PROFILE_ENGINE=hnsw or hnsw_f16",
        );
    }

    // ACORN-1（2-hop 展開・Issue #501）の opt-in（Issue #502）。可視比率
    // スイープ（`run_visible_ratio_sweep`）専用の knob——`full_scan_ratio_override`
    // と同様、S0-cold／S0-hot（既定経路）・S1〜S5' には効かない。未設定
    // （既定）時は `ValidatedHnswParams::acorn_max_visible_ratio() == None`
    // のまま、本 Issue 導入前と出力・処理が完全に同一。
    let acorn_max_visible_ratio_override =
        match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO")
            .and_then(|raw| harness::bench_engine::parse_acorn_max_visible_ratio(raw.as_deref()))
        {
            Ok(v) => v,
            Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO: {e}")),
        };
    if acorn_max_visible_ratio_override.is_some()
        && !matches!(
            knn_engine,
            harness::bench_engine::BenchEngine::Hnsw | harness::bench_engine::BenchEngine::HnswF16
        )
    {
        fail_closed(
            "BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO requires BENCH_KNN_PROFILE_ENGINE=hnsw or hnsw_f16",
        );
    }

    // visited 集合の切替閾値（Issue #497）。S0-cold／S0-hot の `EngineCore`
    // 構築（`build_core_for`）と可視比率スイープ（`run_visible_ratio_sweep`→
    // `build_core_for_sweep`）の双方に効く。未設定時は既定値（常に dense）の
    // まま、本 knob 導入前と出力・処理が完全に同一。#498 の可視比率別
    // before/after 計測が主な使用先。
    let sparse_visited_max_override =
        match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_SPARSE_VISITED_MAX")
            .and_then(|raw| harness::bench_engine::parse_sparse_visited_max(raw.as_deref()))
        {
            Ok(v) => v,
            Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_SPARSE_VISITED_MAX: {e}")),
        };
    if sparse_visited_max_override.is_some()
        && !matches!(knn_engine, harness::bench_engine::BenchEngine::Hnsw)
    {
        fail_closed("BENCH_KNN_PROFILE_SPARSE_VISITED_MAX requires BENCH_KNN_PROFILE_ENGINE=hnsw");
    }
    let visible_ratio_denominator = match harness::bench_engine::read_env_var(
        "BENCH_KNN_PROFILE_VISIBLE_RATIO",
    )
    .and_then(|raw| {
        harness::bench_engine::parse_visible_ratio(raw.as_deref(), MAX_VISIBLE_RATIO_DENOMINATOR)
    }) {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_VISIBLE_RATIO: {e}")),
    };
    let sweep_scale: u64 = match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_SCALE")
        .and_then(|raw| harness::bench_engine::parse_scale(raw.as_deref(), MAX_SWEEP_SCALE))
    {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_SCALE: {e}")),
    };

    // f16 常駐の前後比較用モード（Issue #516）。既定経路（S0-cold〜residual の
    // 段別分解。本節の対象外）とは独立の投入・warm・計測フローを持つ。
    // `BENCH_KNN_PROFILE_INDEX_MEMORY` は索引単体の常駐バイト数計測（redb を
    // 経由しない・子プロセス隔離）、`BENCH_KNN_PROFILE_HOT_ONLY` は
    // 25k/100k/500k 規模点での SQL 表層 e2e レイテンシ前後比較（S0-hot のみ・
    // 索引 1 回構築）。互いに排他、`BENCH_KNN_PROFILE_VISIBLE_RATIO`
    // （上記スイープ）とも排他とする（計測条件が異なりすぎるため同時指定を
    // fail-closed で拒否する。この判定は `visible_ratio_denominator` の分岐
    // （下記）より前に置く——後ろに置くとスイープが先に `return` してしまい
    // 排他違反を検出できない。モジュール冒頭コメントの既定経路の出力不変契約は
    // 3 モードとも未設定〔既定〕時は分岐しないことで維持する）。
    let index_memory: bool =
        match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_INDEX_MEMORY")
            .and_then(|raw| harness::bench_engine::parse_flag(raw.as_deref()))
        {
            Ok(v) => v,
            Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_INDEX_MEMORY: {e}")),
        };
    let hot_only: bool = match harness::bench_engine::read_env_var("BENCH_KNN_PROFILE_HOT_ONLY")
        .and_then(|raw| harness::bench_engine::parse_flag(raw.as_deref()))
    {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_KNN_PROFILE_HOT_ONLY: {e}")),
    };
    if index_memory && hot_only {
        fail_closed(
            "BENCH_KNN_PROFILE_INDEX_MEMORY and BENCH_KNN_PROFILE_HOT_ONLY are mutually exclusive",
        );
    }
    if (index_memory || hot_only) && visible_ratio_denominator.is_some() {
        fail_closed(
            "BENCH_KNN_PROFILE_INDEX_MEMORY/BENCH_KNN_PROFILE_HOT_ONLY and \
             BENCH_KNN_PROFILE_VISIBLE_RATIO are mutually exclusive",
        );
    }
    // ACORN-1 knob（Issue #502）は可視比率スイープ専用。他モードで設定すると
    // 静かに無視される事故を防ぐため fail-closed で拒否する。
    if acorn_max_visible_ratio_override.is_some() && visible_ratio_denominator.is_none() {
        fail_closed(
            "BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO requires BENCH_KNN_PROFILE_VISIBLE_RATIO",
        );
    }
    if index_memory {
        run_index_memory_mode(knn_engine, dim, sweep_scale);
        return;
    }
    if hot_only {
        run_hot_only(knn_engine, dim, sweep_scale, full_scan_ratio_override);
        return;
    }

    if let Some(denominator) = visible_ratio_denominator {
        run_visible_ratio_sweep(
            knn_engine,
            dim,
            denominator,
            full_scan_ratio_override,
            sparse_visited_max_override,
            acorn_max_visible_ratio_override,
            sweep_scale,
        );
        return;
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!("knn_profile_bench: rows={TOTAL_ROWS} dim={dim} top_k={TOP_K} (tenant_a={TENANT_A_ROWS} tenant_b={TENANT_B_ROWS}) engine={}", knn_engine.token());
    println!("knn_profile_bench: SQL 表層は PrefilterCache を経由しない（core.rs は EngineCore::search 専用）が SqlArenaCache（Issue #363）は経由する。S0-cold は毎サンプル新規 EngineCore で SqlArenaCache を空の状態から測る。既定 provider は ParallelSearchProvider、対照は CpuScalarProvider。BENCH_KNN_PROFILE_ENGINE=hnsw のときは S0-cold/S0-hot の EngineCore を hnsw opt-in（Issue #403 B 案）で構築する（S1〜S5' は非対象）。");

    // --- データ投入: 一時 DB へテナント A・B の行を投入する ---
    let path = unique_db_path("issue362-knn-profile");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new(
                COLUMN,
                ColumnType::Vector(dim as u32),
                false,
            )],
        ))
        .expect("create table for bench seeding");

    let mut rng = DeterministicRng::new(1);
    let mut next_id: u64 = 0;
    for (tenant_id, count) in [(TENANT_A, TENANT_A_ROWS), (TENANT_B, TENANT_B_ROWS)] {
        let ctx = PolicyContext::new(tenant_id).expect("valid tenant id");
        let mut remaining = count;
        while remaining > 0 {
            let batch_len = SEED_BATCH_ROWS.min(remaining);
            let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
            for _ in 0..batch_len {
                batch_vectors.push(rng.next_vector(dim));
            }
            let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
                .map(|i| {
                    let id = next_id + i as u64;
                    (
                        id,
                        RowInput {
                            tenant_id,
                            visibility: Visibility::Public,
                            embedding: &batch_vectors[i],
                            // VECTOR 列のみのテーブルのため、スカラーペイロードは
                            // 空バイト列になる（`sql_c1_bench.rs` と同一理由）。
                            metadata: b"",
                        },
                    )
                })
                .collect();
            let op_id = OperationId::parse(&format!("seed-{tenant_id}-{next_id}"))
                .expect("valid operation_id");
            tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id).expect("seed batch insert");
            next_id += batch_len as u64;
            remaining -= batch_len;
        }
    }
    // 投入件数の検証は `debug_assert!` ではなく明示チェックにする。ベンチは
    // release ビルドで動かすため `debug_assert!` は既定でコンパイルから除去され、
    // 検査が事実上無効化される（codex-review 指摘・PR #378）。投入済みベクトルを
    // 別バッファへ複製して件数検証する必要はない（`next_id` の到達値で足りる）。
    if next_id as usize != TOTAL_ROWS {
        fail_closed(format!(
            "seeded row count mismatch: expected {TOTAL_ROWS}, got {next_id}"
        ));
    }

    let policy_ctx = PolicyContext::new(TENANT_A).expect("valid tenant id");
    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");

    // 単一クエリ（テナント A の可視ベクトルからの近傍探索。RLS は Public のため
    // テナント B の行も可視）。S0〜S5 を通じて同一クエリベクトルを使い、段間で
    // 比較可能にする。
    let query = rng.next_vector(dim);

    // --- S4/S5/S5': pub API（`&storage` 使用）。--------------------------------
    // S4 は「arena 構築の実コスト」のみを対象とする。`harness::protocol::run` は
    // 戻り値を `black_box` 通過後、計測区間の内側で drop する契約
    // （`protocol.rs` モジュールコメント・Issue #302）のため、クロージャが
    // `VectorArena`（ヒープ確保を伴う）をそのまま返すと解放コストが構築コストへ
    // 混入する（codex-review 指摘・PR #378）。`run_bounded_retain`
    // （`retain_capacity = 0`）を使い、`elapsed()` 取得後・計測区間の外側で
    // 毎回即座に drop することで、同時に生存する `VectorArena` を常に 0 件に
    // 保ちつつ解放コストも計測区間から除く（codex-review 指摘・PR #378 追記。
    // `mem::replace` によるリングバッファ入れ替えは入れ替え自体が計測区間の内側で
    // 発生していたため、解放コスト除外の要求を満たしていなかった）。
    let (s4, _retained) = run_bounded_retain(&config, 0, || {
        VectorArena::build_filtered(&storage, TABLE, |tenant, visibility| {
            policy_ctx.is_visible(tenant, visibility)
        })
        .expect("arena build must succeed for well-formed synthetic corpus")
    })
    .expect("measurement must satisfy protocol minimums");
    let s4_median = s4.summary.median;

    // S5/S5' は事前構築済みの単一アリーナに対して測る（クエリ毎の再構築コストは
    // S4 で分離済みのため、ここでは検索カーネル段のみを対象にする）。
    let arena = VectorArena::build_filtered(&storage, TABLE, |tenant, visibility| {
        policy_ctx.is_visible(tenant, visibility)
    })
    .expect("arena build must succeed for well-formed synthetic corpus");
    if arena.len() != TOTAL_ROWS {
        fail_closed(format!(
            "arena row count mismatch: expected {TOTAL_ROWS}, got {}",
            arena.len()
        ));
    }

    let parallel_provider = ParallelSearchProvider;
    let s5_parallel = run(&config, || {
        parallel_provider
            .search(SearchInput {
                ids: arena.ids(),
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &query,
                k: TOP_K,
            })
            .expect("parallel search must succeed for well-formed synthetic input")
    })
    .expect("measurement must satisfy protocol minimums");

    let scalar_provider = CpuScalarProvider;
    let s5_scalar = run(&config, || {
        scalar_provider
            .search(SearchInput {
                ids: arena.ids(),
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &query,
                k: TOP_K,
            })
            .expect("scalar search must succeed for well-formed synthetic input")
    })
    .expect("measurement must satisfy protocol minimums");

    let s5_prime = run(&config, || {
        let mut acc = 0.0f32;
        let dim = arena.dim() as usize;
        for chunk in arena.vectors().chunks_exact(dim) {
            acc += dot_wrapper(chunk, &query);
        }
        acc
    })
    .expect("measurement must satisfy protocol minimums");

    // 検索ループの追加コスト分離は S5_scalar − S5' を用いる（S5_parallel − S5' は
    // 使わない）。`ParallelSearchProvider::search` はワーカースレッド生成・行範囲
    // 分割・部分 Top-k・結果マージ・入力検証を含み、`s5_prime`（呼び出し元スレッド
    // で `dot_wrapper` を逐次実行するのみ）との差分には並列化コストが混在して
    // しまう（codex-review 指摘・PR #378）。`s5_scalar`（`CpuScalarProvider`）は
    // `s5_prime` と同じ単線・逐次走査条件のため、並列化コストは混在しない。ただし
    // `CpuScalarProvider::search` は Top-k 選出（`BinaryHeap` 部分ソート）のほかにも
    // 次元検証・クエリの有限値検査・行ごとの範囲取得（境界チェック付き slice）・
    // スコアの有限値検査・候補構築を行うため、この差分は「Top-k 選出のみ」の
    // コストではなく「Top-k を含む検索ループ全体の追加コスト」である
    // （codex-review 指摘・PR #378）。
    //
    // `s5_prime` と `s5_scalar` は入れ子ではなく独立に計測した別経路であるため、
    // 差分が理論上非負である保証はない（`s5_scalar` 側にのみ `#[inline(never)]`
    // 呼び出しオーバーヘッドが乗らない等の要因で、僅差は測定ノイズにより逆転し
    // 得る。cursor 指摘・PR #378）。よってこの差分のみ非致命扱いとし、負の
    // 差分が出ても `fail_closed` でベンチ全体を中断せず、以降の S0/S1〜S4 の
    // 計測を継続する（S1〜S4 等の入れ子な累積段の非単調性チェックは従来どおり
    // fail-closed を維持する）。出力自体も他の段と同様、全計測・整合性検証が
    // 完了してからまとめて行う（codex-review 指摘・PR #378。fail-closed 契約に
    // 反して整合性検証前に測定値を出力しないため、ここでは結果を変数へ保持する
    // のみで println! しない）。
    let s5prime_vs_scalar_diff = stage_diff_ns_per_row(
        s5_prime.summary.median,
        s5_scalar.summary.median,
        TOTAL_ROWS,
        "S5prime",
        "S5_scalar",
    );

    let literal = vector_literal(&query).expect("finite query vector");
    let sql = c1_statement(TABLE, COLUMN, &literal, TOP_K)
        .expect("well-formed C1 statement from validated identifiers");

    // `storage`（S4/S5 で使い終えた元ハンドル）を明示的に drop し、redb のファイル
    // ロックを解放する。redb は書き込み可能ハンドルを同時に複数開けない契約
    // （`redb::DatabaseError::DatabaseAlreadyOpen`）のため、直後の S0-cold 測定が
    // 毎回開く一時ハンドルと同時生存させられない（P1-1 是正・PR #378
    // codex-review 指摘）。
    drop(storage);

    // --- S0-cold: SqlArenaCache を毎回空の状態から測る e2e 計測。----------------
    // モジュール冒頭コメント「`SqlArenaCache` と S0 の cold/hot 分離」節参照。
    // `harness::protocol::run` 系は setup（DB オープン）と計測を単一クロージャに
    // 束ねる契約のため使えず、本段のみ独自の warmup/計測ループを持つ。
    for _ in 0..config.warmup_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for S0-cold warmup");
        let cold_core = build_core_for(knn_engine, cold_storage, sparse_visited_max_override);
        black_box(
            cold_core
                .execute_sql(&policy_ctx, &sql)
                .expect("execute_sql must succeed for well-formed synthetic KNN query"),
        );
    }
    let mut s0_cold_samples: Vec<Duration> =
        Vec::with_capacity(config.measured_iterations() as usize);
    let mut s0_cold_last_result_len: Option<usize> = None;
    for _ in 0..config.measured_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for S0-cold measurement");
        let cold_core = build_core_for(knn_engine, cold_storage, sparse_visited_max_override);
        let start = Instant::now();
        let result = black_box(
            cold_core
                .execute_sql(&policy_ctx, &sql)
                .expect("execute_sql must succeed for well-formed synthetic KNN query"),
        );
        let elapsed = start.elapsed();
        s0_cold_samples.push(elapsed);
        // 計測区間の外側でのみ結果行数を観測する（`run`/`run_bounded_retain` と
        // 同じく、計測区間そのものへは戻り値の検証を混ぜない）。
        s0_cold_last_result_len = Some(result.rows.len());
    }
    if s0_cold_samples.is_empty() {
        fail_closed("S0-cold measurement produced no samples");
    }
    if s0_cold_last_result_len != Some(TOP_K) {
        fail_closed(format!(
            "S0-cold result row count mismatch: expected {TOP_K}, got {s0_cold_last_result_len:?}"
        ));
    }
    let s0_cold_summary =
        stats::summarize(&s0_cold_samples).expect("S0-cold summarize must succeed");

    // --- S0-hot/S0': SQL 表層 e2e（単一 `EngineCore` を使い回すホットパス）。----
    let storage = Storage::open(&path).expect("reopen storage for S0-hot/S0'");
    let core = build_core_for(knn_engine, storage, sparse_visited_max_override);
    let s0_hot = run(&config, || {
        core.execute_sql(&policy_ctx, &sql)
            .expect("execute_sql must succeed for well-formed synthetic KNN query")
    })
    .expect("measurement must satisfy protocol minimums");
    if s0_hot.samples.is_empty() {
        fail_closed("S0-hot measurement produced no samples");
    }
    let s0_hot_result_len = core
        .execute_sql(&policy_ctx, &sql)
        .expect("execute_sql must succeed for well-formed synthetic KNN query")
        .rows
        .len();
    if s0_hot_result_len != TOP_K {
        fail_closed(format!(
            "S0-hot result row count mismatch: expected {TOP_K}, got {s0_hot_result_len}"
        ));
    }

    // 非 vacuous 確認（Issue #413。`feature_bench.rs` と同じ原則）。hnsw／
    // hnsw_f16 opt-in 時は S0-hot 測定後に索引が実際に構築・使用されたことを
    // 固定し、満たさなければ `fail_closed` する（Issue #516 codex P1 指摘対応。
    // hnsw_f16 でも `HnswIndexCache` は `hnsw` と同一の
    // `sql::hnsw_cache::HnswIndexCacheStats` を返す設計のため、検証ロジック
    // 自体は精度非依存で共有できる）。brute_force では統計を出力しない
    // （`hnsw_index_cache_stats()` は常に全欄 0）。
    if requires_hnsw_stats_check(knn_engine.token()) {
        let s = core.hnsw_index_cache_stats();
        println!(
            "knn_profile_bench: hnsw_stats builds={} build_failures={} hits={} misses={} fallbacks={} entries={}",
            s.builds, s.build_failures, s.hits, s.misses, s.fallbacks, s.entries
        );
        if s.builds == 0 || s.hits == 0 {
            fail_closed(format!(
                "ANN non-vacuous check failed: builds={} hits={}",
                s.builds, s.hits
            ));
        }
    }

    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let s0_prime = run(&config, || {
        core.execute_sql(&policy_ctx, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");
    // S0-hot の TOP_K 検証（上記）と同様、計測クロージャの戻り値は `black_box` へ
    // 渡すだけで中身を検証しない。SQL 経路が誤った件数を返しても計測は成功して
    // しまうため、計測外で COUNT(*) を再実行し値を TOTAL_ROWS と突き合わせる
    // （codex-review 指摘・PR #378。fail-closed）。
    let count_result = core
        .execute_sql(&policy_ctx, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    if count_result.rows.len() != 1 {
        fail_closed(format!(
            "S0' COUNT(*) row count mismatch: expected 1, got {}",
            count_result.rows.len()
        ));
    }
    let count_value = match count_result.rows[0].cells.first() {
        Some(Cell::Integer(v)) => *v,
        other => fail_closed(format!("S0' COUNT(*) cell type mismatch: got {other:?}")),
    };
    if count_value != TOTAL_ROWS as u64 {
        fail_closed(format!(
            "S0' COUNT(*) value mismatch: expected {TOTAL_ROWS}, got {count_value}"
        ));
    }

    // --- S1〜S3: 生 redb 再オープンでの走査・デコード。--------------------------
    // `core`（`EngineCore::from_storage` が所有する `Storage`）を drop してファイル
    // ロックを解放してから、生 `redb::Database` として再オープンする
    // （`tests/persistence.rs` と同一手順）。
    drop(core);

    let db = Database::open(&path).expect("reopen raw database for S1-S3");

    // S1: per-entry 走査のみ（`black_box` は harness::protocol::run が内包する）。
    // 段間差分（S2 - S1 等）を正しく「ヘッダデコード等の追加コストのみ」に
    // 保つため、S1 のループ本体は S2/S3 と同じ「行数を数えるだけ」に揃える
    // （バイト数集計等 S1 固有の処理を追加しない。S1 ⊆ S2 ⊆ S3 の入れ子を
    // 崩すと差分が歪む。codex-review 指摘・PR #378）。
    //
    // S1〜S3 は全段で「同じ型（u64）の累積器へ、行あたり同じ回数（2 回）だけ
    // `std::hint::black_box` 経由で `wrapping_add` する」という同形・同量の
    // 観測処理を持つ（S1 は行データを読まないため加算対象は定数、S2 はヘッダ
    // 由来値、S3 は f32 由来値のビット列。観測処理自体の演算コストを全段で
    // 揃えることで、S2−S1／S3−S2 の差分にヘッダデコード・f32 デコード以外の
    // コスト〔checksum 演算の非対称性〕が混入しないようにする。
    // codex-review P2-1 指摘・PR #378）。
    let s1 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let _entry = entry.expect("iterate row entry");
            // S2/S3 と同形の観測処理（モジュール冒頭コメント・上記コメント参照）。
            // S1 は行バイト列を読まないため加算対象は定数だが、S2/S3 と同じ
            // 回数・同じ型の演算を `black_box` 経由で行い、段間差分から
            // checksum 演算コスト自体を相殺する。
            checksum = checksum.wrapping_add(std::hint::black_box(1u64));
            checksum = checksum.wrapping_add(std::hint::black_box(1u64));
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");
    let s1_rows = {
        let read_txn = db.begin_read().expect("begin read txn for row count check");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        table.iter().expect("iter row table").count()
    };

    // S2: S1 ＋ ヘッダデコード。ヘッダデコード結果（`tenant_id`・`is_public`）は
    // 単に破棄せず、行数と組み合わせた値依存の checksum へ混入して返す（`tenant_id`
    // の長さ・`is_public` フラグはいずれもデコード結果に実際に依存する軽量な値の
    // ため、release 最適化でヘッダデコード自体が未観測として除去されるのを防ぐ。
    // codex-review P1-2 指摘・PR #378。S3 と同種の是正）。
    //
    // S1 と同形の観測処理（上記「S1〜S3 は全段で…」コメント参照）: 行あたり
    // 常に 2 回、`black_box` 経由で `u64` 累積器へ加算する。分岐（`if is_public`）
    // による加算回数の増減を避けるため、`is_public` は `bool as u64`（0/1）へ
    // 変換してから無条件に加算する（codex-review P2-1 指摘・PR #378）。
    let s2 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let (tenant_id, is_public, _offset) = decode_header_reimpl(v.value())
                .expect("header decode must succeed for well-formed synthetic rows");
            checksum = checksum.wrapping_add(std::hint::black_box(tenant_id.len() as u64));
            checksum = checksum.wrapping_add(std::hint::black_box(is_public as u64));
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");
    let s2_rows = {
        let read_txn = db.begin_read().expect("begin read txn for row count check");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let _ = decode_header_reimpl(v.value()).expect("header decode");
            rows += 1;
        }
        rows
    };
    let diff_s1_s2 =
        stage_diff_ns_per_row(s1.summary.median, s2.summary.median, TOTAL_ROWS, "S1", "S2");

    // S3: S2 ＋ f32 デコード。スクラッチバッファはループ間で使い回す
    // （`storage.rs::decode_row_embedding_and_metadata_into` と同じ設計を
    // 再実装側でも踏襲する。モジュール冒頭コメント参照）。デコード結果
    // （`scratch`）は行数のみのカウントへ捨てず、先頭・末尾要素を checksum へ
    // 加算して返す。`_decoded`（`ReimplDecodedRow`）自体は embedding を保持しない
    // 借用参照のみのため checksum の元にならず、`scratch`（f32 デコードの実出力）
    // を直接使う必要がある（release 最適化で f32 デコード自体が未観測として
    // 除去されるのを防ぐ。codex-review P1-2 指摘・PR #378）。
    //
    // S1/S2 と同形の観測処理（上記「S1〜S3 は全段で…」コメント参照）: 行あたり
    // 常に 2 回、`black_box` 経由で `u64` 累積器へ加算する。S1/S2 と型を揃える
    // ため、f32 由来値は `f32::to_bits`（ビット列の再解釈。丸め等の追加浮動小数点
    // 演算を持ち込まない）で `u32` へ変換したうえで `u64` へ拡張する
    // （codex-review P2-1 指摘・PR #378）。
    let mut scratch: Vec<f32> = Vec::with_capacity(dim);
    let s3 = run(&config, || {
        let read_txn = db.begin_read().expect("begin read txn");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        let mut rows = 0usize;
        let mut checksum: u64 = 0;
        for entry in table.iter().expect("iter row table") {
            let (_k, v) = entry.expect("iterate row entry");
            let _decoded = decode_row_reimpl(v.value(), &mut scratch)
                .expect("row decode must succeed for well-formed synthetic rows");
            let first_bits = scratch.first().copied().unwrap_or(0.0).to_bits() as u64;
            let last_bits = scratch.last().copied().unwrap_or(0.0).to_bits() as u64;
            checksum = checksum.wrapping_add(std::hint::black_box(first_bits));
            checksum = checksum.wrapping_add(std::hint::black_box(last_bits));
            rows += 1;
        }
        (rows, checksum)
    })
    .expect("measurement must satisfy protocol minimums");
    let mut s3_scratch: Vec<f32> = Vec::with_capacity(dim);
    let mut s3_rows = 0usize;
    // 突き合わせ対象の 1 件（行 id・ベンチ内デコード結果）。`Storage::scan()`
    // は `ROWS_TABLE`（"rows"）のみを走査し、本ベンチが `tenant::insert_rows`
    // 経由で書き込むカタログテーブル（`user_rows/docs`）は対象外のため使えない
    // （モジュール冒頭コメント「行バイト列レイアウトのベンチ内再実装」節参照）。
    // 代わりに、同じ id を持つ行を pub API 経由で構築済みの `arena`（S4 で
    // `VectorArena::build_filtered` により構築した「正本」）から embedding を
    // 引いて突き合わせる（ヘッダ・フルバイト列レイアウトの突き合わせは
    // `tests/knn_profile_accept.rs` が `Storage::put`/`Storage::scan()` を使う
    // 小規模フィクスチャで別途行う）。
    let mut cross_check_sample: Option<(u64, Vec<f32>)> = None;
    {
        let read_txn = db.begin_read().expect("begin read txn for row count check");
        let table = read_txn.open_table(ROW_TABLE).expect("open row table");
        for entry in table.iter().expect("iter row table") {
            let (k, v) = entry.expect("iterate row entry");
            let _decoded = decode_row_reimpl(v.value(), &mut s3_scratch).expect("row decode");
            if cross_check_sample.is_none() {
                let (_tenant, id) = k.value();
                // クロスチェック用の 1 件のみ計測外で所有化する（`decoded` は
                // embedding を持たないスクラッチ参照のみを返す設計に変更済み。
                // モジュール冒頭コメント参照。codex-review 指摘 #1・PR #378）。
                cross_check_sample = Some((id, s3_scratch.clone()));
            }
            s3_rows += 1;
        }
    }
    let diff_s2_s3 =
        stage_diff_ns_per_row(s2.summary.median, s3.summary.median, TOTAL_ROWS, "S2", "S3");
    let diff_s3_s4 = stage_diff_ns_per_row(s3.summary.median, s4_median, TOTAL_ROWS, "S3", "S4");

    // --- 残差: S0-cold -（S4 + S5） -----------------------------------------------
    // S0-hot はキャッシュヒット時のオーバーヘッドを測る別段のため、段別分解・残差の
    // 基準には使わない（S0-cold が「クエリ毎に候補行を再デコードする e2e コスト」を
    // 表す段。モジュール冒頭コメント「`SqlArenaCache` と S0 の cold/hot 分離」節
    // 参照。P1-1 是正・PR #378 codex-review 指摘）。
    let s4_plus_s5 = s4_median.saturating_add(s5_parallel.summary.median);
    let residual = s0_cold_summary.median.saturating_sub(s4_plus_s5);

    // --- 整合性検証（fail-closed） ---------------------------------------------
    // モジュール冒頭コメントの契約（「不一致ならベンチはエラー終了し測定値を
    // 出力しない」）どおり、S1〜S4・S0-cold/S0-hot/S0' の全測定・全差分計算を終えた
    // この時点で全チェックを完了させ、以降で初めて結果を出力する（codex-review 指摘・
    // PR #378: 従来は各段の println! を測定直後に行っていたため、後段の
    // 整合性チェック〔行数不一致・レイアウトドリフト〕で fail_closed する場合でも
    // 失敗前に出力済みの測定値が有効な結果として残ってしまっていた）。
    if let Err(e) =
        assert_scan_row_counts_match(&[("S1", s1_rows), ("S2", s2_rows), ("S3", s3_rows)])
    {
        fail_closed(e);
    }
    if s1_rows != TOTAL_ROWS {
        fail_closed(format!(
            "raw redb scan row count mismatch: expected {TOTAL_ROWS}, got {s1_rows}"
        ));
    }
    // `db`（生 `redb::Database` ハンドル）はこれ以降使わないため drop し、ファイル
    // ロックを解放する（後続の測定はすべて `EngineCore`/`&storage` 経由の pub API
    // を通じてのみ DB へアクセスする）。
    drop(db);

    // S3 のベンチ内デコードと `VectorArena::build_filtered`（pub API・S4 で構築済みの
    // 「正本」デコード結果）の 1 件突き合わせ（レイアウト再実装のドリフト検出。
    // モジュール冒頭コメント参照）。`Storage::scan()` は `ROWS_TABLE`（"rows"）のみを
    // 走査し、本ベンチが書き込むカタログテーブル（`user_rows/docs`）は対象外のため
    // 使えない。ヘッダ（`tenant_id`/`visibility`）を含むフルバイト列レイアウトの
    // 突き合わせは `tests/knn_profile_accept.rs` が `Storage::put`/`Storage::scan()`
    // を使う小規模フィクスチャで別途行う（同一の `encode_row`/`decode_row_header`
    // 実装を経るため、テーブル名の違いはレイアウトそのものに影響しない）。
    if let Some((id, embedding)) = cross_check_sample {
        let matched = arena
            .ids()
            .iter()
            .position(|&arena_id| arena_id == id)
            .map(|idx| {
                let dim = arena.dim() as usize;
                &arena.vectors()[idx * dim..(idx + 1) * dim] == embedding.as_slice()
            })
            .unwrap_or(false);
        if !matched {
            fail_closed(
                "S3 reimpl decode did not match the VectorArena::build_filtered row for the same id (layout drift suspected)",
            );
        }
    }
    // S1〜S4 の段間差分は、各段が入れ子（S1 ⊆ S2 ⊆ S3 ⊆ S4）の処理を行う設計では
    // あるものの、実測は段ごとに独立した `run` 呼び出し（別トランザクション・別
    // warmup/サンプル列）であり、中央値どうしの比較である以上は測定ノイズにより
    // 逆転しうる（S5prime-S5_scalar と同じ理由。上記コメント参照。codex-review
    // P2-2 指摘・PR #378: 独立計測である事実を見落として fail_closed していた
    // ため、以前は測定ノイズだけでベンチ全体が失敗しえた）。よってこれらの差分は
    // 整合性検証（行数不一致・レイアウトドリフト等。fail-closed を維持）とは
    // 区別し、非致命扱いとする。逆転した場合は該当差分のみ「未確定」として出力し、
    // 他の測定値はそのまま出力を継続する。

    // --- 出力: 全測定・全整合性検証を完了したここまでの間、測定値は一切
    // println! していない（fail-closed 契約。上記「整合性検証」節参照）。以降は
    // 検証済みの結果をまとめて出力するのみで、新たな fail_closed 分岐は持たない。
    println!(
        "{}",
        render_stage_line(
            "S4_arena_build",
            TOTAL_ROWS,
            s4_median,
            ns_per_row(s4_median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S5_search_parallel",
            TOTAL_ROWS,
            s5_parallel.summary.median,
            ns_per_row(s5_parallel.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S5_search_scalar",
            TOTAL_ROWS,
            s5_scalar.summary.median,
            ns_per_row(s5_scalar.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S5prime_distance_only",
            TOTAL_ROWS,
            s5_prime.summary.median,
            ns_per_row(s5_prime.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    print_stage_diff_or_unconfirmed("S5prime", "S5_scalar", s5prime_vs_scalar_diff);
    println!(
        "{}",
        render_stage_line(
            "S0_cold_sql_e2e",
            TOTAL_ROWS,
            s0_cold_summary.median,
            ns_per_row(s0_cold_summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S0_hot_sql_e2e",
            TOTAL_ROWS,
            s0_hot.summary.median,
            ns_per_row(s0_hot.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S0prime_count_star",
            TOTAL_ROWS,
            s0_prime.summary.median,
            ns_per_row(s0_prime.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S1_redb_scan",
            TOTAL_ROWS,
            s1.summary.median,
            ns_per_row(s1.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    println!(
        "{}",
        render_stage_line(
            "S2_header_decode",
            TOTAL_ROWS,
            s2.summary.median,
            ns_per_row(s2.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    print_stage_diff_or_unconfirmed("S1", "S2", diff_s1_s2);
    println!(
        "{}",
        render_stage_line(
            "S3_f32_decode",
            TOTAL_ROWS,
            s3.summary.median,
            ns_per_row(s3.summary.median, TOTAL_ROWS).expect("TOTAL_ROWS > 0"),
        )
    );
    print_stage_diff_or_unconfirmed("S2", "S3", diff_s2_s3);
    print_stage_diff_or_unconfirmed("S3", "S4", diff_s3_s4);
    println!(
        "residual(S0-(S4+S5)): median={:.3}ms (parse/bind/result-assembly 等。read_txn 境界差を含みうる保守的な残差)",
        residual.as_secs_f64() * 1e3
    );

    println!("knn_profile_bench: consistency checks passed (S1..S3 row counts, S0 result count, S0' COUNT(*) value, S3 vs VectorArena cross-check)");
}

/// `knn_engine`（`BENCH_KNN_PROFILE_FULL_SCAN_RATIO`／
/// `BENCH_KNN_PROFILE_SPARSE_VISITED_MAX`〔Issue #497〕／
/// `BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO`〔ACORN-1・Issue #501・#502〕
/// override 対応）で [`EngineCore`] を構築する（[`run_visible_ratio_sweep`]
/// 専用。既定経路の `build_core_for` は override を持たないため共有しない）。
fn build_core_for_sweep(
    knn_engine: harness::bench_engine::BenchEngine,
    storage: Storage,
    full_scan_ratio_override: Option<(u32, u32)>,
    sparse_visited_max_override: Option<usize>,
    acorn_max_visible_ratio_override: Option<(u32, u32)>,
) -> EngineCore {
    // `with_acorn_max_visible_ratio` は「`acorn_max_visible_ratio >=
    // full_scan_ratio`」を検証する（`hnsw.rs::ValidatedHnswParams::
    // with_acorn_max_visible_ratio` docコメント参照）ため、必ず
    // `full_scan_ratio_override` 適用後に呼ぶ（本関数の Hnsw／HnswF16 双方の
    // arm で共有する適用順）。
    let apply_overrides = |mut validated: ValidatedHnswParams| -> ValidatedHnswParams {
        if let Some((numerator, denominator)) = full_scan_ratio_override {
            validated = validated
                .with_full_scan_ratio(Ratio {
                    numerator,
                    denominator,
                })
                .expect("BENCH_KNN_PROFILE_FULL_SCAN_RATIO already validated by harness::bench_engine::parse_full_scan_ratio");
        }
        if let Some((numerator, denominator)) = acorn_max_visible_ratio_override {
            validated = validated
                .with_acorn_max_visible_ratio(Ratio {
                    numerator,
                    denominator,
                })
                .expect("BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO must not be less than the effective full_scan_ratio (BENCH_KNN_PROFILE_FULL_SCAN_RATIO or its default)");
        }
        validated
    };
    match knn_engine {
        harness::bench_engine::BenchEngine::BruteForce => {
            EngineCore::from_storage(storage, search_engine::default_engine())
        }
        harness::bench_engine::BenchEngine::Hnsw => {
            let mut validated = ValidatedHnswParams::new(HnswParams::default())
                .expect("valid HnswParams::default()");
            validated = apply_overrides(validated);
            if let Some(max) = sparse_visited_max_override {
                validated = validated.with_sparse_visited_max(max);
            }
            EngineCore::from_storage_with_engine(storage, SearchEngineKind::Hnsw(validated))
        }
        harness::bench_engine::BenchEngine::HnswF16 => {
            // Issue #487 スイープは元々 f16 常駐を対象としないが、Issue #516 で
            // `BenchEngine::HnswF16` を追加した以上、この match を非網羅にしない
            // ため f32 版と同じ override 適用ロジックに `with_resident_precision`
            // を重ねるだけの対応を用意する（本 Issue のスイープ計測はこの経路を
            // 使わない。`run_hot_only` が f16 常駐計測の本体）。
            let mut validated = ValidatedHnswParams::new(HnswParams::default())
                .expect("valid HnswParams::default()")
                .with_resident_precision(ResidentPrecision::F16);
            validated = apply_overrides(validated);
            EngineCore::from_storage_with_engine(storage, SearchEngineKind::Hnsw(validated))
        }
        harness::bench_engine::BenchEngine::HnswI8 => {
            // f16 の arm と同型（Issue #523）: このスイープ計測は I8 常駐を
            // 対象としないが、`BenchEngine::HnswI8` を追加した以上この match
            // を非網羅にしないため用意する。
            let mut validated = ValidatedHnswParams::new(HnswParams::default())
                .expect("valid HnswParams::default()")
                .with_resident_precision(ResidentPrecision::I8);
            validated = apply_overrides(validated);
            EngineCore::from_storage_with_engine(storage, SearchEngineKind::Hnsw(validated))
        }
    }
}

/// `sql::hnsw_cache::HnswIndexCacheStats` の Subset 系カウンタから、このクエリで
/// 実際に選ばれた経路を分類する（Issue #487。doc 表「観測 arm」列のラベル）。
/// 呼び出し前後の差分（delta）を渡す契約——累積値をそのまま渡すと、warm-up の
/// フィルタなしクエリ由来の `hits`／`builds` 増分と混ざる。
// `HnswIndexCacheStats`（`sql::hnsw_cache`）は `pub(crate)` モジュール配下のため
// bench（`engine` クレート外部）からは型名を書けない。カウンタを個別の `u64`
// として受け渡す（`core.hnsw_index_cache_stats()` 自体は `pub fn` のため呼び出し・
// フィールアクセスは可能——型を明示的に書けないだけ）。
fn observed_arm_label(
    subset_searches_delta: u64,
    plain_scans_delta: u64,
    mask_splits_graph_delta: u64,
    masked_short_delta: u64,
    acorn_searches_delta: u64,
) -> &'static str {
    // 互いに排他な 4 カウンタ（`hnsw_cache.rs` のドキュメンテーションコメント
    // 参照）のうち、非 0 のものを優先順位付きで採用する。全 0 は
    // brute_force エンジン（Subset 系カウンタを一切持たない）を表す。
    // `acorn_searches`（ACORN-1・Issue #501・#502）は `subset_searches` の
    // 部分集合（`TraversalRegime::TwoHop` レジームで縮退なしに完走した回数。
    // `sql/hnsw_cache.rs` docコメント参照）のため、`subset_searches_delta > 0`
    // の判定内で優先的に区別する（`subset_searches_delta` 自体はどちらの
    // レジームでも増分するため、先に判定してしまうと 2-hop 発火を見逃す）。
    if acorn_searches_delta > 0 {
        "ann_masked_two_hop"
    } else if subset_searches_delta > 0 {
        "ann_masked"
    } else if plain_scans_delta > 0 {
        "plain_scan_ratio"
    } else if mask_splits_graph_delta > 0 {
        "plain_scan_mask_split"
    } else if masked_short_delta > 0 {
        "plain_scan_masked_short"
    } else {
        "n/a (brute_force engine)"
    }
}

/// 可視比率 × 行数の損益分岐点スイープ（Issue #487。ADR
/// `docs/design/hnsw-rls-cardinality-switch.md`「スコープ外・申し送り」の
/// `full_scan_ratio` 再調整項目への実測入力）。
///
/// `main` から `BENCH_KNN_PROFILE_VISIBLE_RATIO` が設定されている場合にのみ
/// 呼ばれ、既定経路（S0-cold〜residual の段別プロファイル。本関数とは無関係）
/// とは完全に独立した投入・warm・計測フローを持つ。SCALAR 事前フィルタ付き
/// DISTANCE（`sql::hnsw_cache` の `Subset` 形状）を発火させ、ANN（マスク付き
/// 探索）と plain scan のどちらが選ばれたかを [`observed_arm_label`] で分類する。
///
/// `Subset` 形状は自身では索引を構築しない（`sql::hnsw_cache::prepare_subset`。
/// 既存の索引が `Ready`／`NeedOverlay` であればその base を再利用し per-query
/// オーバーレイを計算して `Indexed` を返す。`Lookup::Miss`／
/// `BuildFailedThisGeneration`——利用可能な索引が無い場合——のみ `FullScan` へ
/// 縮退する）ため、同一 `EngineCore` でまずフィルタなしクエリを 1 回発行して
/// `FullVisible` 形状の索引を warm し、`Subset` 経路が再利用できる base を
/// 用意したうえで WHERE クエリを計測する。
fn run_visible_ratio_sweep(
    knn_engine: harness::bench_engine::BenchEngine,
    dim: usize,
    denominator: u32,
    full_scan_ratio_override: Option<(u32, u32)>,
    sparse_visited_max_override: Option<usize>,
    acorn_max_visible_ratio_override: Option<(u32, u32)>,
    scale: u64,
) {
    const BUCKET_COLUMN: &str = "bucket";

    let tenant_a_rows = TENANT_A_ROWS as u64 * scale;
    let tenant_b_rows = TENANT_B_ROWS as u64 * scale;
    let total_rows = tenant_a_rows + tenant_b_rows;
    if !total_rows.is_multiple_of(denominator as u64) {
        fail_closed(format!(
            "BENCH_KNN_PROFILE_VISIBLE_RATIO=1/{denominator} does not evenly divide total_rows={total_rows} (scale={scale}); pick a denominator that divides {TOTAL_ROWS}*scale exactly"
        ));
    }
    let visible_rows = total_rows / denominator as u64;
    if visible_rows == 0 {
        fail_closed(format!(
            "BENCH_KNN_PROFILE_VISIBLE_RATIO=1/{denominator} yields 0 visible rows for total_rows={total_rows}"
        ));
    }

    let effective_full_scan_ratio = full_scan_ratio_override.unwrap_or_else(|| {
        let default = ValidatedHnswParams::default().full_scan_ratio();
        (default.numerator, default.denominator)
    });

    println!(
        "knn_profile_bench: visible_ratio_sweep total_rows={total_rows} dim={dim} top_k={TOP_K} \
         denominator={denominator} visible_rows={visible_rows} engine={} full_scan_ratio={}/{} \
         sparse_visited_max={} acorn_max_visible_ratio={} \
         (Issue #487・#497・#502。S0-cold・S1〜S5' は非対象。QEMU 共有開発環境での実測は参考値——\
         docs/design/hnsw-rls-cardinality-switch.md 参照)",
        knn_engine.token(),
        effective_full_scan_ratio.0,
        effective_full_scan_ratio.1,
        sparse_visited_max_override
            .map(|v| v.to_string())
            .unwrap_or_else(|| "default".to_string()),
        acorn_max_visible_ratio_override
            .map(|(n, d)| format!("{n}/{d}"))
            .unwrap_or_else(|| "none".to_string()),
    );

    let path = unique_db_path("issue487-knn-visible-ratio-sweep");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for sweep seeding");
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new(COLUMN, ColumnType::Vector(dim as u32), false),
            ColumnDef::new(BUCKET_COLUMN, ColumnType::Text, false),
        ],
    );
    storage
        .create_table(&schema)
        .expect("create table for sweep seeding");

    let mut rng = DeterministicRng::new(1);
    let mut next_id: u64 = 0;
    for (tenant_id, count) in [(TENANT_A, tenant_a_rows), (TENANT_B, tenant_b_rows)] {
        let ctx = PolicyContext::new(tenant_id).expect("valid tenant id");
        let mut remaining = count;
        while remaining > 0 {
            let batch_len = (SEED_BATCH_ROWS as u64).min(remaining);
            let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len as usize);
            let mut batch_metadata: Vec<Vec<u8>> = Vec::with_capacity(batch_len as usize);
            for i in 0..batch_len {
                batch_vectors.push(rng.next_vector(dim));
                let id = next_id + i;
                let bucket = format!("b{}", id % denominator as u64);
                let values = [RowValue::Null, RowValue::Text(bucket)];
                batch_metadata.push(
                    encode_scalar_columns(&schema, &values)
                        .expect("bucket value must encode within TEXT column limits"),
                );
            }
            let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len as usize)
                .map(|i| {
                    let id = next_id + i as u64;
                    (
                        id,
                        RowInput {
                            tenant_id,
                            visibility: Visibility::Public,
                            embedding: &batch_vectors[i],
                            metadata: &batch_metadata[i],
                        },
                    )
                })
                .collect();
            let op_id = OperationId::parse(&format!("sweep-{tenant_id}-{next_id}"))
                .expect("valid operation_id");
            tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id).expect("seed batch insert");
            next_id += batch_len;
            remaining -= batch_len;
        }
    }
    if next_id != total_rows {
        fail_closed(format!(
            "sweep seeded row count mismatch: expected {total_rows}, got {next_id}"
        ));
    }

    let policy_ctx = PolicyContext::new(TENANT_A).expect("valid tenant id");
    let query = rng.next_vector(dim);
    let literal = vector_literal(&query).expect("finite query vector");
    let filterless_sql = c1_statement(TABLE, COLUMN, &literal, TOP_K)
        .expect("well-formed C1 statement from validated identifiers");
    let where_sql = c1_where_statement(TABLE, COLUMN, BUCKET_COLUMN, "b0", &literal, TOP_K)
        .expect("well-formed WHERE statement from validated identifiers/tokens");

    let core = build_core_for_sweep(
        knn_engine,
        storage,
        full_scan_ratio_override,
        sparse_visited_max_override,
        acorn_max_visible_ratio_override,
    );

    // --- warm: `FullVisible` 形状の索引を 1 回構築する（`Subset` 形状は索引を
    // 構築しないため。モジュール冒頭コメント参照）。--------------------------
    let _ = core
        .execute_sql(&policy_ctx, &filterless_sql)
        .expect("warm-up query must succeed");
    if requires_hnsw_stats_check(knn_engine.token()) {
        let warm_stats = core.hnsw_index_cache_stats();
        if warm_stats.builds == 0 {
            fail_closed(format!(
                "warm-up did not build the FullVisible HNSW index (builds={})",
                warm_stats.builds
            ));
        }
    }

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");

    // --- 参照区間: フィルタなし SQL 表層 e2e（`S0prime_count_star` と並ぶ
    // run-to-run 実測ノイズ帯の基準。`docs/design/benchmark-judgement-policy.md`
    // §4）。---------------------------------------------------------------
    let reference = run(&config, || {
        core.execute_sql(&policy_ctx, &filterless_sql)
            .expect("execute_sql must succeed for filterless reference query")
    })
    .expect("measurement must satisfy protocol minimums");

    let stats_before_subset = core.hnsw_index_cache_stats();

    // --- 対象: SCALAR 事前フィルタ付き DISTANCE（`Subset` 形状）。-----------
    let subset = run(&config, || {
        core.execute_sql(&policy_ctx, &where_sql)
            .expect("execute_sql must succeed for WHERE subset query")
    })
    .expect("measurement must satisfy protocol minimums");

    let stats_after_subset = core.hnsw_index_cache_stats();

    // --- 参照区間: `COUNT(*)`（エンジン非依存の run-to-run ノイズ帯基準）。--
    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let count_reference = run(&config, || {
        core.execute_sql(&policy_ctx, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");

    // --- 計測外での結果検証（fail-closed）。---------------------------------
    let where_result = core
        .execute_sql(&policy_ctx, &where_sql)
        .expect("execute_sql must succeed for WHERE subset query");
    let expected_rows = (TOP_K as u64).min(visible_rows) as usize;
    if where_result.rows.len() != expected_rows {
        fail_closed(format!(
            "WHERE subset result row count mismatch: expected {expected_rows}, got {}",
            where_result.rows.len()
        ));
    }
    for row in &where_result.rows {
        let id = match row.cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => fail_closed(format!("WHERE subset id cell type mismatch: got {other:?}")),
        };
        if !id.is_multiple_of(denominator as u64) {
            fail_closed(format!(
                "WHERE subset returned a row outside the filtered bucket: id={id} denominator={denominator}"
            ));
        }
    }
    let count_result = core
        .execute_sql(&policy_ctx, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    let count_value = match count_result.rows.first().and_then(|r| r.cells.first()) {
        Some(Cell::Integer(v)) => *v,
        other => fail_closed(format!("COUNT(*) cell type mismatch: got {other:?}")),
    };
    if count_value != total_rows {
        fail_closed(format!(
            "COUNT(*) value mismatch: expected {total_rows}, got {count_value}"
        ));
    }
    // `builds_delta` 契約違反（Subset 形状が索引を再構築した）は、後続の
    // S0_hot_* median/raw 出力より前に検査する（Cursor Bugbot 指摘。契約
    // 違反時に測定値だけがログへ書かれ、失敗理由が読み取れなくなることを
    // 防ぐ。`observed_arm_label` 分類・他カウンタの出力は builds_delta が
    // 健全であることを前提にしてよいため、この検査だけを前倒しする）。
    if requires_hnsw_stats_check(knn_engine.token()) {
        let builds_delta = stats_after_subset
            .builds
            .saturating_sub(stats_before_subset.builds);
        if builds_delta > 0 {
            fail_closed(format!(
                "Subset 形状は索引を再構築しない契約のはずが builds={builds_delta} を観測した（モジュール冒頭コメント参照）"
            ));
        }
    }

    // --- 出力（fail-closed 検証をすべて終えたここまでの間、測定値は一切
    // println! していない。既定経路 main() と同じ契約）。---------------------
    println!(
        "{}",
        render_stage_line(
            "S0_hot_sql_e2e",
            total_rows as usize,
            reference.summary.median,
            ns_per_row(reference.summary.median, total_rows as usize).expect("total_rows > 0"),
        )
    );
    println!(
        "raw(S0_hot_sql_e2e): samples_ms={:?}",
        reference
            .samples
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .collect::<Vec<f64>>()
    );
    println!(
        "{}",
        render_stage_line(
            "S0_hot_where_subset",
            total_rows as usize,
            subset.summary.median,
            ns_per_row(subset.summary.median, total_rows as usize).expect("total_rows > 0"),
        )
    );
    println!(
        "raw(S0_hot_where_subset): samples_ms={:?}",
        subset
            .samples
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .collect::<Vec<f64>>()
    );
    println!(
        "{}",
        render_stage_line(
            "S0prime_count_star",
            total_rows as usize,
            count_reference.summary.median,
            ns_per_row(count_reference.summary.median, total_rows as usize)
                .expect("total_rows > 0"),
        )
    );
    println!(
        "raw(S0prime_count_star): samples_ms={:?}",
        count_reference
            .samples
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .collect::<Vec<f64>>()
    );

    let expected = harness::bench_engine::expected_arm_acorn(
        visible_rows,
        total_rows,
        effective_full_scan_ratio,
        acorn_max_visible_ratio_override,
    )
    .expect("visible_rows/total_rows/full_scan_ratio/acorn_max_visible_ratio must not overflow at this scale");
    let expected_label = match expected {
        harness::bench_engine::ExpectedArm::AnnMasked => "ann_masked",
        harness::bench_engine::ExpectedArm::PlainScanRatio => "plain_scan_ratio",
        harness::bench_engine::ExpectedArm::AnnMaskedTwoHop => "ann_masked_two_hop",
    };

    if requires_hnsw_stats_check(knn_engine.token()) {
        // `HnswIndexCacheStats`（`sql::hnsw_cache`）は `pub(crate)` モジュール
        // 配下のため型名を bench 側に書けない（`observed_arm_label` 上部の
        // コメント参照）。フィールドごとの差分を個別のローカル変数に留める。
        // `builds_delta` の契約違反検査は既に上（`計測外での結果検証`）で
        // 完了済み。
        let subset_searches_delta = stats_after_subset
            .subset_searches
            .saturating_sub(stats_before_subset.subset_searches);
        let plain_scans_delta = stats_after_subset
            .plain_scans
            .saturating_sub(stats_before_subset.plain_scans);
        let mask_splits_graph_delta = stats_after_subset
            .mask_splits_graph
            .saturating_sub(stats_before_subset.mask_splits_graph);
        let masked_short_delta = stats_after_subset
            .masked_short
            .saturating_sub(stats_before_subset.masked_short);
        let fallbacks_delta = stats_after_subset
            .fallbacks
            .saturating_sub(stats_before_subset.fallbacks);
        // ACORN-1（2-hop 展開・Issue #501）の発火状況（Issue #502）。
        // `acorn_searches` は `TraversalRegime::TwoHop` レジームで縮退なしに
        // 完走した回数（`subset_searches` の部分集合）、`acorn_expansions` は
        // `bridge_expand` が受理・2-hop 候補化した累計件数——ACORN opt-in
        // （`acorn_max_visible_ratio_override.is_some()`）でなければ常に 0。
        let acorn_searches_delta = stats_after_subset
            .acorn_searches
            .saturating_sub(stats_before_subset.acorn_searches);
        let acorn_expansions_delta = stats_after_subset
            .acorn_expansions
            .saturating_sub(stats_before_subset.acorn_expansions);
        // `BENCH_KNN_PROFILE_ENGINE=hnsw|hnsw_f16` では 4 カウンタ全 0 は
        // 「このクエリが Subset 系のいずれの経路も通らなかった」ことを意味し、
        // brute_force エンジンの `n/a` とは区別すべき vacuous な計測である
        // （Cursor Bugbot 指摘。ラベルだけ `n/a (brute_force engine)` と出力
        // されるとスイープが誤って green のまま通過してしまう。Issue #516
        // codex P1 指摘対応で hnsw_f16 にもこの検証を適用した）。
        if subset_searches_delta == 0
            && plain_scans_delta == 0
            && mask_splits_graph_delta == 0
            && masked_short_delta == 0
        {
            fail_closed(format!(
                "BENCH_KNN_PROFILE_ENGINE={} だが Subset 系カウンタ（subset_searches/plain_scans/mask_splits_graph/masked_short）が全て 0 だった（vacuous な計測。hnsw_cache の適用条件から外れている可能性）",
                knn_engine.token()
            ));
        }
        println!(
            "knn_profile_bench: hnsw_stats(subset_delta) subset_searches={} plain_scans={} \
             mask_splits_graph={} masked_short={} fallbacks={}",
            subset_searches_delta,
            plain_scans_delta,
            mask_splits_graph_delta,
            masked_short_delta,
            fallbacks_delta,
        );
        let observed_label = observed_arm_label(
            subset_searches_delta,
            plain_scans_delta,
            mask_splits_graph_delta,
            masked_short_delta,
            acorn_searches_delta,
        );
        println!("knn_profile_bench: arm expected={expected_label} observed={observed_label}");
        println!(
            "knn_profile_bench: acorn(delta) searches={acorn_searches_delta} expansions={acorn_expansions_delta}"
        );
        // 期待値が `AnnMaskedTwoHop`（ACORN opt-in・可視カーディナリティ比が
        // `full_scan_ratio <= r <= acorn_max_visible_ratio` の範囲）なのに
        // `acorn_searches_delta == 0` だった場合は vacuous な計測として拒否
        // する（`sparse_visited` の同型ガード・#498 の方針を踏襲）。逆に
        // `expected != AnnMaskedTwoHop` のときは `acorn_searches_delta > 0`
        // を要求しない——`traversal_regime_for` 自体が `expected_arm_acorn`
        // より広い縮退経路（`mask_splits_graph`／`masked_short`）を持つため。
        if expected == harness::bench_engine::ExpectedArm::AnnMaskedTwoHop
            && acorn_searches_delta == 0
        {
            fail_closed(format!(
                "BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO が可視カーディナリティ比 \
                 {visible_rows}/{total_rows} を範囲内に含むにもかかわらず acorn_searches の \
                 delta が 0 だった（TwoHop レジームが一度も縮退なしに完走しなかった vacuous な \
                 計測。observed={observed_label}）"
            ));
        }

        // visited 集合切替閾値の診断出力（Issue #498）。`sparse_visited_max`
        // 未設定（`sparse_visited_max_override == None`）は production の既定
        // （`DEFAULT_SPARSE_VISITED_MAX = 0`。常に dense）と同じ効果を持つため
        // `effective_sparse_visited_max` は 0 とみなす——`hnsw.rs::
        // search_masked_with` の述語（`mask.count_ones() < sparse_visited_max`）
        // と単一の情報源を共有し、期待値の計算をここだけで二重管理しない。
        let sparse_visited_searches_delta = stats_after_subset
            .sparse_visited_searches
            .saturating_sub(stats_before_subset.sparse_visited_searches);
        let effective_sparse_visited_max = sparse_visited_max_override.unwrap_or(0);
        let expected_sparse = visible_rows < effective_sparse_visited_max as u64;
        let expected_visited_label = if expected_sparse { "sparse" } else { "dense" };
        println!(
            "knn_profile_bench: sparse_visited(delta) searches={sparse_visited_searches_delta} \
             expected_visited={expected_visited_label}"
        );
        // ann_masked（マスク付き探索が縮退なしで完走した）かつ、可視候補数が
        // 閾値未満で sparse が選ばれるべき条件のときに限り、実際に 1 件も
        // `VisitedSparse` を選ばなかった（delta == 0）ことを vacuous な計測
        // として拒否する。`plain_scan_*`（既存 fixture では構造的に到達不能。
        // `docs/design/hnsw-rls-cardinality-switch.md`「可視比率 × 行数の
        // 損益分岐点実測（Issue #487）」節）や `dense` 期待（閾値 ≤ 可視行数。
        // sparse が発火しないのが正しい挙動）は fail させない——「既存
        // fixture では切替到達不能」という事実そのものを記録することが本
        // 診断の目的の一つのため。
        if observed_label == "ann_masked" && expected_sparse && sparse_visited_searches_delta == 0 {
            fail_closed(
                "BENCH_KNN_PROFILE_SPARSE_VISITED_MAX の閾値未満の可視候補数で ann_masked が \
                 観測されたにもかかわらず sparse_visited_searches の delta が 0 だった \
                 （VisitedSparse が一度も選ばれなかった vacuous な計測。Issue #498）"
                    .to_string(),
            );
        }
    } else {
        println!(
            "knn_profile_bench: arm expected={expected_label} observed=n/a (brute_force engine)"
        );
    }

    println!("knn_profile_bench: visible_ratio_sweep consistency checks passed (WHERE result count/bucket membership, COUNT(*) value)");
}

/// f16 常駐の前後比較（Issue #516・要件 A）: `BENCH_KNN_PROFILE_HOT_ONLY=1` で
/// 呼ばれる。既定経路（S0-cold〜residual の段別分解）とは独立した投入・warm・
/// 計測フローを持ち、SQL 表層 e2e のホットパス（S0-hot 相当。索引 1 回構築＋
/// キャッシュヒット）と参照区間（`COUNT(*)`）のみを測る——`BENCH_KNN_PROFILE_
/// SCALE` を 500k 行規模（scale=20）まで許すため、既定経路が行う S0-cold（毎
/// サンプル新規 `EngineCore` 構築。40 回以上）は本モードでは行わない
/// （非現実的な所要時間になるため。`docs/design/hnsw-f16-resident.md`
/// 「Issue #516 追記」節「hot-only モード」参照）。
fn run_hot_only(
    knn_engine: harness::bench_engine::BenchEngine,
    dim: usize,
    scale: u64,
    full_scan_ratio_override: Option<(u32, u32)>,
) {
    let tenant_a_rows = TENANT_A_ROWS as u64 * scale;
    let tenant_b_rows = TENANT_B_ROWS as u64 * scale;
    let total_rows = tenant_a_rows + tenant_b_rows;
    if total_rows == 0 {
        fail_closed("hot-only mode requires total_rows > 0");
    }

    let effective_full_scan_ratio = full_scan_ratio_override.unwrap_or_else(|| {
        let default = ValidatedHnswParams::default().full_scan_ratio();
        (default.numerator, default.denominator)
    });
    println!(
        "knn_profile_bench: hot_only total_rows={total_rows} dim={dim} top_k={TOP_K} \
         engine={} full_scan_ratio={}/{} (Issue #516。S0-cold・S1〜S5' は非対象。QEMU \
         共有開発環境での実測は参考値——docs/design/hnsw-f16-resident.md 参照)",
        knn_engine.token(),
        effective_full_scan_ratio.0,
        effective_full_scan_ratio.1,
    );
    // 非 vacuous 証跡（Issue #526）: ディスパッチされた 3 経路（f32／f16／i8）の
    // ISA を実行開始時点で 1 行出力する。Apple 実機での計測が実際に NEON 系
    // カーネル（`NeonFp16`／`NeonDotprod`）へ到達したことを、測定値そのものより
    // 前段で確認できるようにする（本環境〔x86_64 QEMU〕では `F16c`／
    // `Avx2Widen` が期待値）。
    println!(
        "{}",
        render_kernel_isa_line(
            &format!("{:?}", isa::current().isa()),
            &format!("{:?}", isa::current_f16().isa()),
            &format!("{:?}", isa::current_i8().isa()),
        )
    );

    let path = unique_db_path("issue516-knn-hot-only");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for hot-only seeding");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new(
                COLUMN,
                ColumnType::Vector(dim as u32),
                false,
            )],
        ))
        .expect("create table for hot-only seeding");

    let mut rng = DeterministicRng::new(1);
    let mut next_id: u64 = 0;
    for (tenant_id, count) in [(TENANT_A, tenant_a_rows), (TENANT_B, tenant_b_rows)] {
        let ctx = PolicyContext::new(tenant_id).expect("valid tenant id");
        let mut remaining = count;
        while remaining > 0 {
            let batch_len = (SEED_BATCH_ROWS as u64).min(remaining);
            let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len as usize);
            for _ in 0..batch_len {
                batch_vectors.push(rng.next_vector(dim));
            }
            let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len as usize)
                .map(|i| {
                    let id = next_id + i as u64;
                    (
                        id,
                        RowInput {
                            tenant_id,
                            visibility: Visibility::Public,
                            embedding: &batch_vectors[i],
                            metadata: b"",
                        },
                    )
                })
                .collect();
            let op_id = OperationId::parse(&format!("hotonly-{tenant_id}-{next_id}"))
                .expect("valid operation_id");
            tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id).expect("seed batch insert");
            next_id += batch_len;
            remaining -= batch_len;
        }
    }
    if next_id != total_rows {
        fail_closed(format!(
            "hot-only seeded row count mismatch: expected {total_rows}, got {next_id}"
        ));
    }

    let policy_ctx = PolicyContext::new(TENANT_A).expect("valid tenant id");
    let query = rng.next_vector(dim);
    let literal = vector_literal(&query).expect("finite query vector");
    let sql = c1_statement(TABLE, COLUMN, &literal, TOP_K)
        .expect("well-formed C1 statement from validated identifiers");

    // hot-only モード（Issue #516）は visited 集合切替閾値（Issue #497）・
    // ACORN-1（Issue #501・#502）いずれも計測対象外のため、既定値（常に
    // dense／ACORN 無効）のまま `None` を渡す。
    let core = build_core_for_sweep(knn_engine, storage, full_scan_ratio_override, None, None);

    // --- warm: 索引構築を含む 1 回目のクエリ（計測外）。--------------------
    let warm_start = Instant::now();
    let warm_result = core
        .execute_sql(&policy_ctx, &sql)
        .expect("warm-up query must succeed");
    let index_warm_ms = warm_start.elapsed().as_secs_f64() * 1e3;
    if warm_result.rows.len() != TOP_K.min(total_rows as usize) {
        fail_closed(format!(
            "hot-only warm-up result row count mismatch: expected {}, got {}",
            TOP_K.min(total_rows as usize),
            warm_result.rows.len()
        ));
    }
    println!("knn_profile_bench: index_warm_ms={index_warm_ms:.3}");

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");

    // --- S0-hot: 単一 `EngineCore` を使い回すホットパス。-----------------------
    let s0_hot = run(&config, || {
        core.execute_sql(&policy_ctx, &sql)
            .expect("execute_sql must succeed for well-formed synthetic KNN query")
    })
    .expect("measurement must satisfy protocol minimums");

    // --- 参照区間: `COUNT(*)`（run-to-run 実測ノイズ帯の基準。`docs/design/
    // benchmark-judgement-policy.md` §4）。--------------------------------------
    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let count_reference = run(&config, || {
        core.execute_sql(&policy_ctx, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");

    // --- 計測外での結果検証（fail-closed）。------------------------------------
    let hot_result_len = core
        .execute_sql(&policy_ctx, &sql)
        .expect("execute_sql must succeed for well-formed synthetic KNN query")
        .rows
        .len();
    let expected_rows = TOP_K.min(total_rows as usize);
    if hot_result_len != expected_rows {
        fail_closed(format!(
            "hot-only S0-hot result row count mismatch: expected {expected_rows}, got {hot_result_len}"
        ));
    }
    let count_result = core
        .execute_sql(&policy_ctx, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    let count_value = match count_result.rows.first().and_then(|r| r.cells.first()) {
        Some(Cell::Integer(v)) => *v,
        other => fail_closed(format!("COUNT(*) cell type mismatch: got {other:?}")),
    };
    if count_value as u64 != total_rows {
        fail_closed(format!(
            "COUNT(*) value mismatch: expected {total_rows}, got {count_value}"
        ));
    }

    // --- 非 vacuous 検証（fail-closed。hnsw／hnsw_f16 のみ）。-------------------
    if requires_hnsw_stats_check(knn_engine.token()) {
        let s = core.hnsw_index_cache_stats();
        println!(
            "knn_profile_bench: hnsw_stats builds={} build_failures={} hits={} misses={} \
             fallbacks={} entries={} f16_residency_fallbacks={} i8_residency_fallbacks={}",
            s.builds,
            s.build_failures,
            s.hits,
            s.misses,
            s.fallbacks,
            s.entries,
            s.f16_residency_fallbacks,
            s.i8_residency_fallbacks,
        );
        if s.builds == 0 || s.hits == 0 || s.build_failures > 0 {
            fail_closed(format!(
                "ANN non-vacuous check failed: builds={} hits={} build_failures={}",
                s.builds, s.hits, s.build_failures
            ));
        }
        let expected_resident = resident_label_for_token(knn_engine.token())
            .expect("hnsw/hnsw_f16/hnsw_i8 tokens must map to a resident label");
        if knn_engine == harness::bench_engine::BenchEngine::HnswF16
            && s.f16_residency_fallbacks != 0
        {
            fail_closed(format!(
                "hnsw_f16 requested but f16_residency_fallbacks={} (D6 auto-degrade to f32; \
                 corpus embeddings must stay within the f16 finite range for this measurement \
                 to be meaningful)",
                s.f16_residency_fallbacks
            ));
        }
        if knn_engine == harness::bench_engine::BenchEngine::HnswI8 && s.i8_residency_fallbacks != 0
        {
            fail_closed(format!(
                "hnsw_i8 requested but i8_residency_fallbacks={} (D6 auto-degrade to f32; \
                 corpus embeddings must be finite and avoid per-dimension scale underflow for \
                 this measurement to be meaningful)",
                s.i8_residency_fallbacks
            ));
        }
        // `EXPLAIN` は `USING PLAN(...)` 文にのみ対応する契約
        // （`sql/allowlist.rs`「EXPLAIN is only supported for SELECT ... USING
        // PLAN(...) statements」）で、本モードが使う `c1_statement`（`ORDER BY
        // embedding <=> '<vec>' LIMIT k`。`USING PLAN` を伴わない）には使えない。
        // 代わりに `EngineCore::search_engine_kind()`（pub API）の `Display` 出力
        // （`search_engine.rs`: `"hnsw(...,resident=<value>)"`）で要求精度が実際に
        // 構築へ到達したことを確認する（`tests/fixtures/recall_engine.rs::
        // assert_ann_non_vacuous` と同じ判定方法）。
        let kind_display = core
            .search_engine_kind()
            .map(|k| k.to_string())
            .unwrap_or_default();
        let resident_suffix = format!("resident={expected_resident}");
        println!(
            "knn_profile_bench: resident_precision requested={expected_resident} \
             search_engine_kind={kind_display}"
        );
        if !kind_display.contains(&resident_suffix) {
            fail_closed(format!(
                "search_engine_kind() display does not contain {resident_suffix:?}: got \
                 {kind_display:?}"
            ));
        }
    }

    // --- 出力（全 fail-closed 検証を終えたここまでの間、測定値は一切 println!
    // していない。既定経路 main() と同じ契約）。--------------------------------
    println!(
        "{}",
        render_stage_line(
            "S0_hot_sql_e2e",
            total_rows as usize,
            s0_hot.summary.median,
            ns_per_row(s0_hot.summary.median, total_rows as usize).expect("total_rows > 0"),
        )
    );
    println!(
        "raw(S0_hot_sql_e2e): samples_ms={:?}",
        s0_hot
            .samples
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .collect::<Vec<f64>>()
    );
    println!(
        "{}",
        render_stage_line(
            "S0prime_count_star",
            total_rows as usize,
            count_reference.summary.median,
            ns_per_row(count_reference.summary.median, total_rows as usize)
                .expect("total_rows > 0"),
        )
    );
    println!(
        "raw(S0prime_count_star): samples_ms={:?}",
        count_reference
            .samples
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .collect::<Vec<f64>>()
    );
    println!("knn_profile_bench: hot_only consistency checks passed (S0-hot result count, COUNT(*) value, resident precision)");
}

/// メモリ計測を新規子プロセスへ隔離するための起動用環境変数名（Issue #516。
/// `hnsw_parallel_build_bench.rs::MEMORY_CHILD_ENV` と同型）。値は
/// `"<scale>:<dim>:<engine_token>"`（例: `"20:768:hnsw_f16"`）。
const INDEX_MEMORY_CHILD_ENV: &str = "BENCH_KNN_PROFILE_INDEX_MEMORY_CHILD";

/// `INDEX_MEMORY_CHILD_ENV` が設定されている場合のみ実行される子プロセス経路
/// （Issue #516）。1 点分（`scale`・`dim`・エンジン）のコーパス生成・
/// `HnswIndex::build_parallel_with_precision` 呼び出しを行い、結果行を標準出力へ
/// 書いて終了する（`main` の通常経路には戻らない）。
fn run_index_memory_child_if_requested() {
    let Ok(raw) = std::env::var(INDEX_MEMORY_CHILD_ENV) else {
        return;
    };
    // GITHUB_ACTIONS 下拒否は親プロセス起動時点で既に検査済みだが、子プロセス
    // 単体で誤って呼ばれた場合の defense-in-depth（`hnsw_parallel_build_bench.rs`
    // と同一方針）。
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        eprintln!("knn_profile_bench: refusing to run under GITHUB_ACTIONS (manual-only bench)");
        std::process::exit(1);
    }
    let mut parts = raw.splitn(3, ':');
    let (scale_str, dim_str, engine_token) = match (parts.next(), parts.next(), parts.next()) {
        (Some(s), Some(d), Some(e)) => (s, d, e),
        _ => {
            eprintln!(
                "knn_profile_bench: invalid {INDEX_MEMORY_CHILD_ENV}={raw:?} (expected \
                 \"<scale>:<dim>:<engine_token>\")"
            );
            std::process::exit(1);
        }
    };
    let scale: u64 = match scale_str.parse() {
        Ok(v) if (1..=MAX_SWEEP_SCALE).contains(&v) => v,
        _ => {
            eprintln!(
                "knn_profile_bench: invalid scale {scale_str:?} in {INDEX_MEMORY_CHILD_ENV} \
                 (must be 1..={MAX_SWEEP_SCALE})"
            );
            std::process::exit(1);
        }
    };
    let dim: usize = match dim_str.parse::<u32>() {
        Ok(v) if (1..=harness::bench_engine::MAX_BENCH_DIM).contains(&v) => v as usize,
        _ => {
            eprintln!(
                "knn_profile_bench: invalid dim {dim_str:?} in {INDEX_MEMORY_CHILD_ENV} (must be \
                 1..={})",
                harness::bench_engine::MAX_BENCH_DIM
            );
            std::process::exit(1);
        }
    };
    let precision = match engine_token {
        "hnsw" => ResidentPrecision::F32,
        "hnsw_f16" => ResidentPrecision::F16,
        "hnsw_i8" => ResidentPrecision::I8,
        other => {
            eprintln!(
                "knn_profile_bench: invalid engine token {other:?} in \
                 {INDEX_MEMORY_CHILD_ENV} (must be \"hnsw\", \"hnsw_f16\", or \"hnsw_i8\")"
            );
            std::process::exit(1);
        }
    };
    let rows = match scaled_rows(scale, TOTAL_ROWS as u64) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("knn_profile_bench: {e}");
            std::process::exit(1);
        }
    };
    match measure_index_memory(rows, dim, precision) {
        Ok(line) => {
            println!("{line}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("knn_profile_bench: {e}");
            std::process::exit(1);
        }
    }
}

/// `rows` 件・`dim` 次元・`precision` 常駐精度で `HnswIndex` を 1 回構築し、
/// 構築前後の RSS・VmHWM・`approx_heap_bytes` を計測する（Issue #516。子プロセス
/// 内でのみ呼ばれる想定——アロケータ残留ページによる後続点の汚染を避けるため、
/// 呼び出し元 `run_index_memory_child_if_requested` が新規プロセスを 1 点ごとに
/// 起動する契約。`hnsw_parallel_build_bench.rs::measure_memory` と同型だが、
/// redb を経由せずメモリ上のコーパスから直接構築する点が異なる——SQL 表層
/// `VectorArena`〔`MAX_ARENA_TOTAL_BYTES` 1 GiB〕を経由しないため、500k×768 の
/// ような arena 構造的上限を超える点でも索引単体としては計測できる）。
fn measure_index_memory(
    rows: u64,
    dim: usize,
    precision: ResidentPrecision,
) -> Result<String, String> {
    let mut rng = DeterministicRng::new(0xF16_5EED ^ rows ^ (dim as u64));
    let mut corpus: Vec<f32> = Vec::with_capacity(rows as usize * dim);
    for _ in 0..rows {
        corpus.extend(rng.next_vector(dim));
    }

    let vm_rss_kb_before = read_vm_rss_kb();
    let index = HnswIndex::build_parallel_with_precision(
        HnswParams::default(),
        precision,
        dim as u32,
        &corpus,
        1,
    )
    .map_err(|e| format!("rows={rows} dim={dim}: HnswIndex build failed: {e}"))?;
    let vm_rss_kb_after = read_vm_rss_kb();
    let vm_hwm_kb = read_vm_hwm_kb();

    if index.len() != rows as usize {
        return Err(format!(
            "rows={rows} dim={dim}: built index len {} does not match corpus rows {rows}",
            index.len()
        ));
    }
    let requested = match precision {
        ResidentPrecision::F32 => "f32",
        ResidentPrecision::F16 => "f16",
        ResidentPrecision::I8 => "i8",
    };
    let effective = match index.resident_precision() {
        ResidentPrecision::F32 => "f32",
        ResidentPrecision::F16 => "f16",
        ResidentPrecision::I8 => "i8",
    };
    Ok(render_index_memory_line(
        rows,
        dim as u32,
        requested,
        effective,
        index.approx_heap_bytes(),
        vm_rss_kb_before,
        vm_rss_kb_after,
        vm_hwm_kb,
    ))
}

/// 索引単体メモリ計測モード（Issue #516・要件 B。`BENCH_KNN_PROFILE_INDEX_MEMORY=1`）。
/// `knn_engine` が `brute_force` の場合は索引を構築しないため拒否する
/// （fail-closed。typo で「メモリ計測のつもりが何も測っていない」事故を防ぐ）。
fn run_index_memory_mode(knn_engine: harness::bench_engine::BenchEngine, dim: usize, scale: u64) {
    if knn_engine == harness::bench_engine::BenchEngine::BruteForce {
        fail_closed(
            "BENCH_KNN_PROFILE_INDEX_MEMORY requires BENCH_KNN_PROFILE_ENGINE=hnsw or hnsw_f16 \
             (brute_force builds no index)",
        );
    }
    let rows = match scaled_rows(scale, TOTAL_ROWS as u64) {
        Ok(r) => r,
        Err(e) => fail_closed(e),
    };
    println!(
        "knn_profile_bench: index_memory_mode rows={rows} dim={dim} engine={} (Issue #516。\
         子プロセス隔離計測。QEMU 共有開発環境での実測は参考値)",
        knn_engine.token()
    );
    // 非 vacuous 証跡（Issue #526）: 親プロセス側（子プロセスは索引構築のみで
    // dot カーネルを実際にディスパッチしない）で、実行時に選ばれる 3 経路の
    // ISA を記録する。`run_hot_only` と同じ理由（`harness::knn_profile` モジュール
    // 冒頭コメント参照）。
    println!(
        "{}",
        render_kernel_isa_line(
            &format!("{:?}", isa::current().isa()),
            &format!("{:?}", isa::current_f16().isa()),
            &format!("{:?}", isa::current_i8().isa()),
        )
    );
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        fail_closed(format!("current_exe unavailable: {e}"));
    });
    let child_value = format!("{scale}:{dim}:{}", knn_engine.token());
    let output = std::process::Command::new(&exe)
        .env(INDEX_MEMORY_CHILD_ENV, &child_value)
        .output()
        .unwrap_or_else(|e| fail_closed(format!("memory child process spawn failed: {e}")));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        fail_closed(format!(
            "memory child process exited with {:?}: {stderr}",
            output.status.code()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with("knn_profile_bench: index_memory "))
        .unwrap_or_else(|| {
            fail_closed("memory child process produced no index_memory line".to_string())
        });
    println!("{line}");
}

/// [`engine::isa::current().dot`] を呼ぶだけの薄いラッパー。`#[inline(never)]` に
/// することで、`objdump -d` でのシンボル特定・逆アセンブル確認（受け入れ条件 3）を
/// 容易にする（モジュール冒頭コメント参照）。production コード（`engine::isa`）は
/// 無変更。
#[inline(never)]
fn dot_wrapper(a: &[f32], b: &[f32]) -> f32 {
    engine::isa::current().dot(a, b)
}
