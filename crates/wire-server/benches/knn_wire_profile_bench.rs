//! `vector_knn` 786µs（`docs/design/crossdb-bench.md`。25,000 行・dim 128・
//! wire 経由・psycopg・p50）の内訳を、wire／SQL 表層／距離カーネル／Top-k の
//! 4 区分へ切り分ける実測入口（Issue #463。ポインタ: TASK-83・TASK-127・
//! TASK-158・TASK-73/WIRE-1・SQL-1〜4・RLS-6/RLS-7）。
//!
//! # 計測段（tier）
//!
//! 同一プロセス内で、同一コーパス（tenant-a 23,000 行 Public ＋ tenant-b 2,000
//! 行 Private・dim 128・crossdb の可視性モデルと同一）・同一 200 クエリベクトル
//! （決定的 RNG）・同一 `Arc<EngineCore>`（既定エンジン
//! `search_engine::default_engine()` = `ParallelSearchProvider`）を共有して
//! 交互計測する。
//!
//! | 段 | 内容 | 対応する区分 |
//! | --- | --- | --- |
//! | T1′ `kernel_distance_only` | 事前構築 `VectorArena` の全可視行へ `isa::current().dot` を適用し総和（Top-k なし） | 距離カーネル |
//! | T1s `provider_scalar` | `CpuScalarProvider::search`（単線・逐次） | 距離＋Top-k（単線条件） |
//! | T1p `provider_parallel` | `ParallelSearchProvider::search`（production 既定） | 検索カーネル段の実効値 |
//! | T2 `sql_surface_hot` | `EngineCore::execute_sql_in_session`（wire と同じ入口。`SqlArenaCache` ウォーム） | SQL 表層 |
//! | T3e `wire_encode_only` | `wire_server::result_encoder` による応答エンコードのみ（計測外で得た `QueryResult` を対象） | wire の部分区間（informational） |
//! | T3 `wire_roundtrip` | in-process ループバックサーバーへの簡易クエリ 1 往復 | wire e2e |
//!
//! 4 区分への帰属（min-of-R の中央値を用いる。`harness::knn_wire` 参照）:
//! - 距離カーネル = T1′
//! - Top-k（単線条件） = T1s − T1′（負なら n/a）
//! - SQL 表層 = T2 − T1p
//! - wire = T3 − T2
//!
//! Top-k は並列 provider 下では分離できない（`crates/engine/benches/
//! knn_profile_bench.rs` と同じ理由）ため、単線条件（T1s − T1′）に限定し、
//! production 実効値は T1p として別に示す。
//!
//! # 計測器の差異
//!
//! crossdb（`scripts/crossdb_bench/self_db.py`）は別プロセスの release
//! `wire-server` バイナリへ psycopg（Python・簡易クエリ・`prepare_threshold=None`）
//! で接続する。本ベンチは in-process ループバックの生 TCP クライアントで、
//! プロセス間・言語間のオーバーヘッドを含まない。T3 の実測値は 786µs を下回る
//! 見込みで、その残差は「クライアント側（psycopg・プロセス間スケジューリング）」
//! として本ベンチの対象外（`docs/design/knn-wire-stage-profile.md` 参照）。
//!
//! # fail-closed 検証
//!
//! 出力前に以下をすべて検証する（`.claude/rules/security.md`「テナント境界」・
//! 「fail-closed を維持する」）: arena 行数が可視集合と一致すること、T1p・T2・T3
//! の返却 id 集合が同一クエリに対し完全一致すること、返却 id がすべて可視
//! テナント（tenant-a・id < `TENANT_A_ROWS`）の範囲であること、T3 が
//! `ErrorResponse`（'E'）を返さないこと。すべて通過するまで測定値を一切
//! `println!` しない（`knn_profile_bench.rs`・`sql_c1_bench.rs` と同一契約）。
//!
//! # CI・出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線
//! しない。`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する
//! （[`harness::knn_wire::refuse_under_github_actions`]）。`make
//! bench-knn-wire-profile`（Makefile）から実行する。判定ロジック自体
//! （時間非依存）は `harness::knn_wire` にあり、`tests/knn_wire_profile_accept.rs`
//! で `make ci` 側から回帰検証する。production コード
//! （`crates/engine/src/`・`crates/wire-server/src/`）は無変更。

#[allow(dead_code)]
mod harness;

#[path = "../tests/common/mod.rs"]
mod common;

use harness::env_report::EnvReport;
use harness::knn_wire::{
    bucket_diff, classify_against_bands, diff_ratio_pct, median_of, min_of, parse_rounds,
    reference_band, refuse_under_github_actions, render_bucket_line, render_reference_band_line,
    render_tier_round_line, render_tier_summary_line, step_ratio_pct,
};
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::{c1_statement, vector_literal};

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::search_engine;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use engine::{arena::VectorArena, tenant};

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 128;
const TENANT_A: &str = "tenant-a";
const TENANT_A_ROWS: usize = 23_000;
const TENANT_B: &str = "tenant-b";
const TENANT_B_ROWS: usize = 2_000;
const TOTAL_ROWS: usize = TENANT_A_ROWS + TENANT_B_ROWS;
const TOP_K: usize = 10;
const TABLE: &str = "docs";
const COLUMN: &str = "embedding";
const SEED_BATCH_ROWS: usize = 5_000;
const QUERY_POOL: usize = 200;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("knn_wire_profile_bench: {msg}");
    std::process::exit(1);
}

/// T1p（`ParallelSearchProvider`）・T2（SQL 表層）・T3（wire）が同一クエリに対し
/// 完全一致した Top-k id 列を返すことを突き合わせるための、id 抽出結果の
/// 正規化表現。
fn sorted_ids(mut ids: Vec<u64>) -> Vec<u64> {
    ids.sort_unstable();
    ids
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }
    let rounds = match parse_rounds(std::env::var("BENCH_KNN_WIRE_ROUNDS").ok().as_deref()) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let dedicated_env = std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "knn_wire_profile_bench: rows={TOTAL_ROWS} dim={DIM} top_k={TOP_K} rounds={rounds} \
         (tenant_a={TENANT_A_ROWS} tenant_b={TENANT_B_ROWS}) dedicated_env={dedicated_env}"
    );
    if !dedicated_env {
        println!(
            "knn_wire_profile_bench: BENCH_DEDICATED_ENV 未設定のため共有環境の参考値として扱う \
             （docs/design/benchmark-judgement-policy.md §5）。採否根拠にしない。"
        );
    }

    // --- データ投入 -----------------------------------------------------------
    let path = unique_db_path("issue463-knn-wire-profile");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new(
                COLUMN,
                ColumnType::Vector(DIM as u32),
                false,
            )],
        ))
        .expect("create table for bench seeding");

    let mut rng = DeterministicRng::new(1);
    let mut next_id: u64 = 0;
    for (tenant_id, count, visibility) in [
        (TENANT_A, TENANT_A_ROWS, Visibility::Public),
        (TENANT_B, TENANT_B_ROWS, Visibility::Private),
    ] {
        let ctx = PolicyContext::new(tenant_id).expect("valid tenant id");
        let mut remaining = count;
        while remaining > 0 {
            let batch_len = SEED_BATCH_ROWS.min(remaining);
            let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
            for _ in 0..batch_len {
                batch_vectors.push(rng.next_vector(DIM));
            }
            let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
                .map(|i| {
                    let id = next_id + i as u64;
                    (
                        id,
                        RowInput {
                            tenant_id,
                            visibility,
                            embedding: &batch_vectors[i],
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
    if next_id as usize != TOTAL_ROWS {
        fail_closed(format!(
            "seeded row count mismatch: expected {TOTAL_ROWS}, got {next_id}"
        ));
    }

    // ctx は wire 認証の既定（`auth::verify`）と同じく Public のみ可視。
    let policy_ctx = PolicyContext::new(TENANT_A).expect("valid tenant id");

    // T1 系専用の arena（storage を move する前に構築する）。
    let arena = VectorArena::build_filtered(&storage, TABLE, |tenant, visibility| {
        policy_ctx.is_visible(tenant, visibility)
    })
    .expect("arena build must succeed for well-formed synthetic corpus");
    if arena.len() != TENANT_A_ROWS {
        fail_closed(format!(
            "arena row count mismatch: expected {TENANT_A_ROWS} visible rows, got {}",
            arena.len()
        ));
    }

    // 200 クエリベクトルを事前構築する（輪番。T1〜T3 で同一系列を共有する）。
    let queries: Vec<Vec<f32>> = (0..QUERY_POOL).map(|_| rng.next_vector(DIM)).collect();
    let sqls: Vec<String> = queries
        .iter()
        .map(|q| {
            let literal = vector_literal(q).expect("finite query vector");
            c1_statement(TABLE, COLUMN, &literal, TOP_K)
                .expect("well-formed C1 statement from validated identifiers")
        })
        .collect();

    let core = Arc::new(EngineCore::from_storage(
        storage,
        search_engine::default_engine(),
    ));

    let users_path = common::write_user_store_file(&[("bench", TENANT_A, "bench")]);
    let addr = common::spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut wire_stream = common::authenticate_to_ready_for_query(addr, "bench", "bench");
    wire_stream
        .set_nodelay(true)
        .expect("set TCP_NODELAY on client socket");

    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    let parallel_provider = ParallelSearchProvider;
    let scalar_provider = CpuScalarProvider;

    // --- ウォームアップ（SqlArenaCache をヒット状態にする）---------------------
    let mut warm_session = SessionState::default();
    for i in 0..config.warmup_iterations() as usize {
        let sql = &sqls[i % QUERY_POOL];
        match core
            .execute_sql_in_session(&policy_ctx, &mut warm_session, sql)
            .expect("warmup sql_surface query must succeed")
        {
            SqlOutcome::Query(_) => {}
            other => fail_closed(format!("unexpected warmup outcome: {other:?}")),
        }
    }
    for i in 0..config.warmup_iterations() as usize {
        let sql = &sqls[i % QUERY_POOL];
        common::send_simple_query(&mut wire_stream, sql);
        let _ = common::read_row_description(&mut wire_stream);
        for _ in 0..TOP_K {
            let _ = common::read_data_row(&mut wire_stream);
        }
        let _ = common::read_command_complete(&mut wire_stream);
        common::read_ready_for_query(&mut wire_stream);
    }

    // 各ラウンドが保持する統計。
    let mut t1_prime_medians = Vec::with_capacity(rounds as usize);
    let mut t1s_medians = Vec::with_capacity(rounds as usize);
    let mut t1p_medians = Vec::with_capacity(rounds as usize);
    let mut t2_medians = Vec::with_capacity(rounds as usize);
    let mut t3e_medians = Vec::with_capacity(rounds as usize);
    let mut t3_medians = Vec::with_capacity(rounds as usize);
    let mut round_lines: Vec<String> = Vec::new();

    // 段ごとの計測反復回数（warmup + measured）。各ラウンドで各段がこの回数
    // だけクエリプールを輪番するため、最終反復のインデックスは段をまたいで
    // 一致する（下記ループ内コメント参照）。
    let iterations_per_stage =
        config.warmup_iterations() as usize + config.measured_iterations() as usize;
    let mut cursor: usize = 0;

    for round in 1..=rounds {
        // 本ラウンドの各段（T1′/T1s/T1p/T2/T3e/T3）は、いずれも同じ開始点
        // `round_start` から `iterations_per_stage` 回だけ独立にクエリプールを
        // 輪番する（codex-review 指摘: ラウンド境界でのみ 1 クエリを進める
        // 構成では、各段の `run()` が warmup・計測の全反復で同一クエリを
        // 繰り返し測定クエリ数が rounds 件（既定 5・上限 50）に留まり、
        // ラウンド間の入力差がノイズに混ざっていた。各段の全反復でクエリ集合を
        // 一巡させることで、1 ラウンドあたり最大 `iterations_per_stage` 件・
        // 全体で最大 `rounds * iterations_per_stage` 件のクエリを反映する）。
        // 各段の反復回数・開始点が同一であるため、最終反復のインデックス
        // （`round_last_idx`）は段をまたいで一致し、T1p・T2・T3 の id 完全一致
        // 検証（本ループ末尾）はこの最終反復のクエリに対して行う。
        let round_start = cursor;
        let round_last_idx = (round_start + iterations_per_stage - 1) % QUERY_POOL;
        cursor = (cursor + iterations_per_stage) % QUERY_POOL;

        // T1′: 距離カーネルのみ（Top-k なし）。
        let mut t1_prime_cursor = round_start;
        let t1_prime = run(&config, || {
            let idx = t1_prime_cursor % QUERY_POOL;
            t1_prime_cursor += 1;
            let query = &queries[idx];
            let mut acc = 0.0f32;
            let dim = arena.dim() as usize;
            for chunk in arena.vectors().chunks_exact(dim) {
                acc += engine::isa::current().dot(chunk, query);
            }
            black_box(acc)
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "kernel_distance_only",
            round,
            t1_prime.summary.median,
            harness::accept::p95_from_samples(&t1_prime.samples).expect("non-empty sample set"),
        ));
        t1_prime_medians.push(t1_prime.summary.median);

        // T1s: 単線 provider（距離＋Top-k）。round_start から輪番。
        let mut t1s_cursor = round_start;
        let mut t1s_ids: Option<Vec<u64>> = None;
        let t1s = run(&config, || {
            let idx = t1s_cursor % QUERY_POOL;
            t1s_cursor += 1;
            let query = &queries[idx];
            let hits = scalar_provider
                .search(SearchInput {
                    ids: arena.ids(),
                    vectors: arena.vectors(),
                    dim: arena.dim(),
                    query,
                    k: TOP_K,
                })
                .expect("scalar search must succeed for well-formed synthetic input");
            t1s_ids = Some(sorted_ids(hits.iter().map(|h| h.id).collect()));
            black_box(hits)
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "provider_scalar",
            round,
            t1s.summary.median,
            harness::accept::p95_from_samples(&t1s.samples).expect("non-empty sample set"),
        ));
        t1s_medians.push(t1s.summary.median);

        // T1p: 並列 provider（production 既定）。round_start から輪番（他段と
        // 同一の開始点・反復回数のため、各反復のクエリは T2・T3 の同一反復目と
        // 一致する）。返却 id は反復ごとに `t1p_ids_all` へ追記し、計測区間の
        // 最後の反復だけでなく全反復（warmup 含む）を検証対象にする
        // （codex-review 指摘: 最終反復のみの検証では途中反復の不一致・可視外
        // 混入を見逃す）。
        let mut t1p_cursor = round_start;
        let mut t1p_ids_all: Vec<Vec<u64>> = Vec::with_capacity(iterations_per_stage);
        let t1p = run(&config, || {
            let idx = t1p_cursor % QUERY_POOL;
            t1p_cursor += 1;
            let query = &queries[idx];
            let hits = parallel_provider
                .search(SearchInput {
                    ids: arena.ids(),
                    vectors: arena.vectors(),
                    dim: arena.dim(),
                    query,
                    k: TOP_K,
                })
                .expect("parallel search must succeed for well-formed synthetic input");
            t1p_ids_all.push(sorted_ids(hits.iter().map(|h| h.id).collect()));
            black_box(hits)
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "provider_parallel",
            round,
            t1p.summary.median,
            harness::accept::p95_from_samples(&t1p.samples).expect("non-empty sample set"),
        ));
        t1p_medians.push(t1p.summary.median);

        // 可視外テナント（tenant-b・id >= TENANT_A_ROWS）が混入しないことを、
        // 本ラウンドの全反復（warmup 含む）について検査する。
        for ids in &t1p_ids_all {
            for id in ids {
                if *id as usize >= TENANT_A_ROWS {
                    fail_closed(format!(
                        "tenant boundary violation: provider_parallel returned invisible id {id}"
                    ));
                }
            }
        }

        // T2: SQL 表層（wire と同じ入口。SqlArenaCache ウォーム済み）。
        // round_start から輪番（他段と同一の開始点・反復回数）。応答エンコード
        // 用サンプル（T3e が使う `QueryResult`）はこの測定区間の外で別途 1 回
        // だけ取得する（codex-review 指摘: 計測クロージャ内で毎反復
        // `result.clone()` すると SQL 表層区分〔T2〕にエンコード用複製コストが
        // 混入し過大評価・wire 側が過小評価されうる。測定クロージャは
        // `execute_sql_in_session` の戻り値をそのまま `black_box` へ渡すだけに
        // する）。
        let mut t2_cursor = round_start;
        let mut t2_ids_all: Vec<Vec<u64>> = Vec::with_capacity(iterations_per_stage);
        let mut hot_session = SessionState::default();
        let t2 = run(&config, || {
            let idx = t2_cursor % QUERY_POOL;
            t2_cursor += 1;
            let sql = &sqls[idx];
            let outcome = core
                .execute_sql_in_session(&policy_ctx, &mut hot_session, sql)
                .expect("sql_surface_hot query must succeed for well-formed synthetic input");
            match outcome {
                SqlOutcome::Query(result) => {
                    t2_ids_all.push(sorted_ids(result.rows.iter().map(|r| r.id).collect()));
                    black_box(result)
                }
                other => fail_closed(format!("unexpected sql_surface_hot outcome: {other:?}")),
            }
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "sql_surface_hot",
            round,
            t2.summary.median,
            harness::accept::p95_from_samples(&t2.samples).expect("non-empty sample set"),
        ));
        t2_medians.push(t2.summary.median);
        // 可視外テナントが混入しないことを、本ラウンドの全反復について検査する。
        for ids in &t2_ids_all {
            for id in ids {
                if *id as usize >= TENANT_A_ROWS {
                    fail_closed(format!(
                        "tenant boundary violation: sql_surface_hot returned invisible id {id}"
                    ));
                }
            }
        }

        // T3e: 応答エンコードのみ（計測外で得た QueryResult に対して測る）。
        // T2 の計測区間には含めず、ここで round_last_idx の SQL を 1 回だけ
        // 計測外に実行してサンプルを取得する（上記 T2 修正のコメント参照。
        // round_last_idx を使うのは、下記 T3 の最終反復・id 完全一致検証と
        // 対象クエリを揃えるため）。
        let sample_result = match core
            .execute_sql_in_session(&policy_ctx, &mut hot_session, &sqls[round_last_idx])
            .expect("sample sql_surface_hot query must succeed for well-formed synthetic input")
        {
            SqlOutcome::Query(result) => result,
            other => fail_closed(format!(
                "unexpected sample sql_surface_hot outcome: {other:?}"
            )),
        };
        let tag = format!("SELECT {}", sample_result.rows.len());
        let t3e = run(&config, || {
            let mut total = 0usize;
            let row_desc =
                wire_server::result_encoder::encode_row_description(&sample_result.columns)
                    .expect("encode row description for sample result");
            total += row_desc.len();
            for row in &sample_result.rows {
                let data_row =
                    wire_server::result_encoder::encode_data_row(&sample_result.columns, row)
                        .expect("encode data row for sample result");
                total += data_row.len();
            }
            let command_complete = wire_server::result_encoder::encode_command_complete(&tag)
                .expect("encode command complete for sample result");
            total += command_complete.len();
            black_box(total)
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "wire_encode_only",
            round,
            t3e.summary.median,
            harness::accept::p95_from_samples(&t3e.samples).expect("non-empty sample set"),
        ));
        t3e_medians.push(t3e.summary.median);

        // T3: wire e2e（in-process ループバックサーバーへの簡易クエリ往復）。
        // round_start から輪番（他段と同一の開始点・反復回数）。
        let mut t3_cursor = round_start;
        let mut t3_ids_all: Vec<Vec<u64>> = Vec::with_capacity(iterations_per_stage);
        let t3 = run(&config, || {
            let idx = t3_cursor % QUERY_POOL;
            t3_cursor += 1;
            let sql = &sqls[idx];
            let start = Instant::now();
            common::send_simple_query(&mut wire_stream, sql);
            let cols = common::read_row_description(&mut wire_stream);
            if cols.is_empty() {
                fail_closed("wire_roundtrip: RowDescription reported zero columns");
            }
            let mut ids = Vec::with_capacity(TOP_K);
            for _ in 0..TOP_K {
                let row = common::read_data_row(&mut wire_stream);
                let id_text = row
                    .first()
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| fail_closed("wire_roundtrip: id cell was NULL"));
                let id: u64 = id_text.parse().unwrap_or_else(|_| {
                    fail_closed(format!(
                        "wire_roundtrip: id cell not a valid u64: {id_text:?}"
                    ))
                });
                ids.push(id);
            }
            let _ = common::read_command_complete(&mut wire_stream);
            common::read_ready_for_query(&mut wire_stream);
            t3_ids_all.push(sorted_ids(ids));
            // `Instant::elapsed()` は `black_box` に渡す戻り値とは無関係に
            // ここで確定させる（`protocol::run` はクロージャの戻り値のみを
            // 計測対象とする契約のため、往復そのものの経過時間を戻り値として
            // 返す。engine 側 `dot_wrapper` パターンとは異なりネットワーク I/O
            // を計測するため、クロージャ内で計測することはできない——
            // `run` は `Instant::now()`/`elapsed()` を自前で取る契約であり、
            // ここでの `start` はワークロード内部の整合性確認用に過ぎない）。
            let _ = start.elapsed();
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "wire_roundtrip",
            round,
            t3.summary.median,
            harness::accept::p95_from_samples(&t3.samples).expect("non-empty sample set"),
        ));
        t3_medians.push(t3.summary.median);
        // 可視外テナントが混入しないことを、本ラウンドの全反復について検査する。
        for ids in &t3_ids_all {
            for id in ids {
                if *id as usize >= TENANT_A_ROWS {
                    fail_closed(format!(
                        "tenant boundary violation: wire_roundtrip returned invisible id {id}"
                    ));
                }
            }
        }

        // T1p・T2・T3 は本ラウンドで同一の開始点・反復回数からクエリプールを
        // 輪番したため、反復インデックスが同じであれば同一クエリに対する
        // 呼び出しであり、返却 id 集合は完全一致するはずである（モジュール
        // 冒頭コメント「fail-closed 検証」の宣言どおり）。codex-review 指摘
        // （PRRT_kwDOUAKASM6fph58）: 以前は最終反復のみを検証しており、途中
        // 反復の不一致・可視外混入を見逃していた。ここでは各段が収集した
        // 全反復（warmup 含む・`iterations_per_stage` 件）を突き合わせる。
        if t1p_ids_all.len() != iterations_per_stage
            || t2_ids_all.len() != iterations_per_stage
            || t3_ids_all.len() != iterations_per_stage
        {
            fail_closed(format!(
                "iteration count mismatch: expected {iterations_per_stage} per stage, got \
                 provider_parallel={} sql_surface_hot={} wire_roundtrip={}",
                t1p_ids_all.len(),
                t2_ids_all.len(),
                t3_ids_all.len()
            ));
        }
        for iter_idx in 0..iterations_per_stage {
            let query_idx = (round_start + iter_idx) % QUERY_POOL;
            let t1p_ids = &t1p_ids_all[iter_idx];
            let t2_ids = &t2_ids_all[iter_idx];
            let t3_ids = &t3_ids_all[iter_idx];
            for (label, ids) in [
                ("provider_parallel", t1p_ids),
                ("sql_surface_hot", t2_ids),
                ("wire_roundtrip", t3_ids),
            ] {
                if ids.len() != TOP_K {
                    fail_closed(format!(
                        "{label}: expected {TOP_K} result rows, got {} (idx={query_idx})",
                        ids.len()
                    ));
                }
            }
            if t1p_ids != t2_ids {
                fail_closed(format!(
                    "id set mismatch for identical query (idx={query_idx}): provider_parallel={t1p_ids:?} sql_surface_hot={t2_ids:?}"
                ));
            }
            if t2_ids != t3_ids {
                fail_closed(format!(
                    "id set mismatch for identical query (idx={query_idx}): sql_surface_hot={t2_ids:?} wire_roundtrip={t3_ids:?}"
                ));
            }
        }
    }

    // --- 整合性検証 -------------------------------------------------------------
    // 上記ループ内でテナント境界・行数はラウンドごとに検証済み。ここでは
    // 集約統計（min-of-R/median-of-R・参照区間帯）の算出自体が空データで
    // 失敗しないことを確認する（`?` ではなく明示的な `unwrap_or_else` で
    // fail_closed に倒す。`main` は `Result` を返さないため）。
    let summarize = |label: &str, medians: &[Duration]| -> (Duration, Duration) {
        let min = min_of(medians).unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
        let median = median_of(medians).unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
        (min, median)
    };

    let (t1p_min, t1p_med) = summarize("kernel_distance_only", &t1_prime_medians);
    let (t1s_min, t1s_med) = summarize("provider_scalar", &t1s_medians);
    let (parallel_min, parallel_med) = summarize("provider_parallel", &t1p_medians);
    let (t2_min, t2_med) = summarize("sql_surface_hot", &t2_medians);
    let (t3e_min, t3e_med) = summarize("wire_encode_only", &t3e_medians);
    let (t3_min, t3_med) = summarize("wire_roundtrip", &t3_medians);

    let reference_band_pct = reference_band(&t1_prime_medians)
        .map(|b| b * 100.0)
        .unwrap_or_else(|e| fail_closed(format!("reference_band: {e}")));

    // --- ここまで全検証通過。以降で初めて測定値を出力する（fail-closed 契約）。---
    for line in &round_lines {
        println!("{line}");
    }
    println!(
        "{}",
        render_tier_summary_line("kernel_distance_only", t1p_min, t1p_med)
    );
    println!(
        "{}",
        render_tier_summary_line("provider_scalar", t1s_min, t1s_med)
    );
    println!(
        "{}",
        render_tier_summary_line("provider_parallel", parallel_min, parallel_med)
    );
    println!(
        "{}",
        render_tier_summary_line("sql_surface_hot", t2_min, t2_med)
    );
    println!(
        "{}",
        render_tier_summary_line("wire_encode_only", t3e_min, t3e_med)
    );
    println!(
        "{}",
        render_tier_summary_line("wire_roundtrip", t3_min, t3_med)
    );
    println!("{}", render_reference_band_line(reference_band_pct));

    // 4 区分内訳（分子・分母とも min-of-R で統一。distance/topk は単線条件・
    // SQL/wire は production 実効値〔parallel〕基準。codex-review 指摘対応:
    // 分子〔diff〕は min-of-R どうしの差分のため、分母の「全体構成比」にも
    // min-of-R の `t3_min` を使う——旧実装は median-of-R の `t3_med` を分母に
    // 使っており統計量が不統一だった）。
    //
    // `ratio`（全体構成比・`t3_min` に対する比率）と `band`（ノイズ判定）は
    // 別の分母から算出する独立した値であり意図的に分離している
    // （codex-review 指摘対応: `benchmark-judgement-policy.md` §4 が要求する
    // ノイズ判定は「対象区間自身の相対増分 `|to/from-1|`」であって、全体への
    // 構成比ではないため `ratio` をノイズ判定へ流用しない。`from` がゼロの
    // 距離カーネル区分は増分率が定義できないため band を `None` とする）。
    let render_bucket = |label: &str, from: Duration, to: Duration| match bucket_diff(from, to) {
        Some(diff) => {
            let ratio = diff_ratio_pct(diff, t3_min)
                .unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
            let band = if from.is_zero() {
                None
            } else {
                let step_ratio = step_ratio_pct(from, to)
                    .unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
                Some(classify_against_bands(step_ratio, reference_band_pct))
            };
            println!(
                "{}",
                render_bucket_line(label, Some(diff), Some(ratio), band)
            );
        }
        None => println!("{}", render_bucket_line(label, None, None, None)),
    };

    println!("--- vector_knn 786us breakdown (min-of-{rounds}, median) ---");
    render_bucket("kernel_distance_only", Duration::ZERO, t1p_min);
    render_bucket("topk_single_threaded", t1p_min, t1s_min);
    render_bucket("sql_surface", parallel_min, t2_min);
    render_bucket("wire", t2_min, t3_min);
    println!(
        "diagnostic(wire_encode_only): min={t3e_min:?} median={t3e_med:?} (t3 の部分区間・informational)"
    );

    // クライアントに Terminate を送って終了する（サーバースレッドは join しない。
    // `tests/wire_nodelay_latency.rs` と同一パターン）。
    let _ = std::io::Write::write_all(&mut wire_stream, &[b'X', 0, 0, 0, 4]);
}
