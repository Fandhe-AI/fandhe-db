//! engine の一通りの機能（SQL 表層・ベクトル検索・RLS）を通しで実行し、
//! レイテンシとリソース利用状況を計測して JSON を stdout へ出力するベンチマーク。
//!
//! `cargo run --release -p fandhe-db-engine --example feature_bench` で実行する。依存追加は
//! せず std のみで計測する（`/proc/self/status`・`/proc/self/stat` を読む簡易実装。
//! Linux 以外では該当欄が 0 のまま出力される）。
//!
//! 計測対象は `docs/spec` の個別ビヘイビア ID には対応づけない（横断的な性能観測用の
//! 補助ツールであり、`tests/*.rs` の受け入れ基準テストとは独立）。SQL 構文・API の
//! 呼び出し方は各結合テスト（`tests/sql_aggregate.rs`・`tests/sql_group_by.rs`・
//! `tests/sql_surface.rs`・`tests/sql_search_mode.rs`・`tests/sql_udf_call.rs`）を
//! 参照して揃えてある。UDF 呼び出し（TASK-79）フェーズも含む。
//!
//! Issue #344/#355 の Recall/性能受け入れ基準の実測（`hybrid_rrf` 288.6ms 等）を
//! 出した計測 example。以前は git 追跡外だったため before/after の再現性が無く
//! （PR #374 の ADR 参照）、Issue #358 で `crates/engine/examples/` へ追跡化した。
//!
//! # ANN opt-in・規模スケール（Issue #413）
//!
//! `BENCH_FEATURE_ENGINE`（`unset`／`""`／`brute_force` → 既定エンジン、`hnsw` →
//! `search_engine::hnsw_kind(HnswParams::default())` opt-in・Issue #403 B 案）・
//! `BENCH_FEATURE_SCALE`（正整数倍率。既定 1 = `ROWS_A`＋`ROWS_B` 合計 25,000 行、
//! 4 = 100,000 行。`hnsw::MAX_HNSW_NODES` を超えない範囲で bound）で
//! `docs/design/hnsw-index.md` の前後比較を測定する。パースは
//! `benches/harness/bench_engine.rs`（`#[path]` で本ファイルへ単一ファイル取り込み。
//! `harness::` ツリー全体は取り込まない）を fail-closed に用いる。JSON の 13
//! フェーズの名前・順序・フィールドは engine・scale を問わず不変（追加情報は
//! `meta` にのみ追加する）。engine=hnsw のときは全フェーズ計測後に
//! `EngineCore::hnsw_index_cache_stats()` が `builds>=1 && hits>0` を満たすことを
//! 検証し（非 vacuous 確認）、満たさなければ `fail_bench` で終了する。
//!
//! `BENCH_FEATURE_DIM`（正整数・既定 128・上限 `bench_engine::MAX_BENCH_DIM`。
//! Issue #466）でベクトル次元数を上書きできる。dim=768／1536 が Issue #365
//! （dot カーネル多アキュムレータ化検討）で採否の判別変数と判明したため、
//! 横断 SQL ベンチ側にも dim を可変にする規模点を用意する。パースは
//! `bench_engine::parse_dim` を fail-closed に用い、`meta.dim` は既存キーの
//! まま値のみが変わる（JSON の形は不変）。

// `parse_visible_ratio`／`parse_full_scan_ratio`／`ExpectedArm`／`expected_arm`
// （Issue #487）は `knn_profile_bench.rs` 専用のスイープ opt-in で、本ファイルは
// 使わない。`#[path]` 取り込みは 1 ファイルを 2 バイナリで共有する構成のため、
// 本ファイル内だけを見ると未使用になり `dead_code` lint に抵触する
// （`bench_engine.rs` 自体を分割しない理由はモジュール冒頭コメント参照）。
#[allow(dead_code)]
#[path = "../benches/harness/bench_engine.rs"]
mod bench_engine;
use bench_engine::BenchEngine;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{Embedder, HashingEmbedder};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use std::hint::black_box;
use std::time::Instant;

/// scale=1（既定）時の tenant-a・tenant-b 投入行数。[`main`] が
/// `BENCH_FEATURE_SCALE` 倍率をこの基準値へ掛けて実行時の行数を決める
/// （Issue #413。`ROWS_A`・`ROWS_B` の定数名は既存 doc コメント・実測記録との
/// 対応づけのため残す）。
const ROWS_A_BASE: u64 = 20_000;
const ROWS_B_BASE: u64 = 5_000;
/// tenant-a 投入時に Private 可視性を割り当てる周期（[`run_ingest`] の
/// `make_batch` 呼び出しに使う）。RLS 境界確認（[`run_rls_isolation_phase`]）が
/// tenant-a の期待 Public/Private 件数をこの定数から導出するため、投入側と
/// 検証側で値がずれないよう単一の定数として共有する。
const TENANT_A_PRIVATE_EVERY_N: u64 = 10;
const BATCH_SIZE: u64 = 1_000;
const WARMUP: usize = 5;
const ITERS: usize = 50;

const LANGS: &[&str] = &["ja", "en", "fr", "de", "es"];
const TOPICS: &[&str] = &[
    "topic-00", "topic-01", "topic-02", "topic-03", "topic-04", "topic-05", "topic-06", "topic-07",
    "topic-08", "topic-09", "topic-10", "topic-11", "topic-12", "topic-13", "topic-14", "topic-15",
    "topic-16", "topic-17", "topic-18", "topic-19",
];
const WORDS: &[&str] = &[
    "vector",
    "index",
    "search",
    "query",
    "engine",
    "tenant",
    "policy",
    "cache",
    "kernel",
    "embedding",
    "storage",
    "recall",
    "precision",
    "hybrid",
    "rerank",
    "fusion",
    "latency",
    "throughput",
    "cluster",
    "shard",
    "replica",
    "commit",
    "transaction",
    "ledger",
    "operation",
    "dictionary",
    "planner",
    "boost",
    "score",
    "rank",
];
const QUERY_TEXTS: &[&str] = &[
    "vector search recall precision",
    "tenant policy cache kernel",
    "hybrid rerank fusion latency",
    "storage commit transaction ledger",
    "dictionary planner boost score",
];

/// `/proc/self/status`・`/proc/self/stat` から読んだリソース使用量のスナップショット。
struct ProcStats {
    vm_rss_kb: u64,
    vm_hwm_kb: u64,
    cpu_ticks: u64,
}

/// 1 フェーズ分の計測結果（レイテンシ分布＋計測後のリソーススナップショット＋
/// フェーズ固有の付帯情報。`extra` は呼び出し元が組み立て済みの JSON オブジェクト
/// 本体の断片で、安全な既知キー・数値・固定文字列のみで構成する）。
struct PhaseStat {
    name: &'static str,
    iterations: usize,
    min_us: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    mean_us: f64,
    rss_kb_after: u64,
    cpu_tick_delta: u64,
    extra: String,
}

/// `rest`（`"VmRSS:    1234 kB"` の `"VmRSS:"` を取り除いた残り）から kB 数値を読む。
/// 値が読めない場合は untrusted 環境差として 0 を返す（本ツールは診断目的であり、
/// 計測失敗でベンチ全体を止めない）。
fn parse_kb(rest: &str) -> u64 {
    rest.split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// `/proc/self/stat` から utime（14 番目）＋stime（15 番目）フィールドの合計 tick 数を
/// 読む。`comm`（2 番目のフィールド）は括弧で囲まれ空白を含みうるため、最後の `)` の
/// 後ろから空白区切りで数える（`man proc` の慣行どおり）。
fn parse_cpu_ticks(stat: &str) -> u64 {
    let Some(paren_end) = stat.rfind(')') else {
        return 0;
    };
    let fields: Vec<&str> = stat[paren_end + 1..].split_whitespace().collect();
    // `paren_end` 直後は 3 番目のフィールド（state）から始まる。utime は 14 番目
    // （このスライスでの index 11）、stime は 15 番目（index 12）。
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    utime.saturating_add(stime)
}

/// 現プロセスのメモリ・CPU 使用量を読む（Linux 専用。読めない環境では全欄 0）。
fn read_proc_stats() -> ProcStats {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut vm_rss_kb = 0u64;
    let mut vm_hwm_kb = 0u64;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            vm_rss_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            vm_hwm_kb = parse_kb(rest);
        }
    }
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let cpu_ticks = parse_cpu_ticks(&stat);
    ProcStats {
        vm_rss_kb,
        vm_hwm_kb,
        cpu_ticks,
    }
}

/// マイクロ秒サンプル列から (min, p50, p95, p99, max, mean) を計算する。
fn percentiles(mut samples: Vec<u64>) -> (u64, u64, u64, u64, u64, f64) {
    if samples.is_empty() {
        return (0, 0, 0, 0, 0, 0.0);
    }
    samples.sort_unstable();
    let n = samples.len();
    let min = samples[0];
    let max = samples[n - 1];
    let sum: u64 = samples.iter().sum();
    let mean = sum as f64 / n as f64;
    let pick = |q: f64| -> u64 {
        let idx = ((q * (n as f64 - 1.0)).round() as usize).min(n - 1);
        samples[idx]
    };
    (min, pick(0.50), pick(0.95), pick(0.99), max, mean)
}

/// ベンチの前提条件（クエリ成功・RLS 境界の期待値一致）が破れた場合に、
/// エラー内容を stderr へ出力してベンチを異常終了させる（fail-closed）。
/// `std::process::exit` は unwind をスキップし `main` の `CleanupGuard`
/// （一時 redb ファイル削除）が走らないため、`panic!` で呼び出し元へ
/// 伝播させ通常の unwind 経路（`Cargo.toml` に `panic = "abort"` の指定は
/// なく既定の unwind）でクリーンアップを効かせる。構文・実行契約の退行や
/// テナント境界違反を「エラー生成時間の計測」「`no_leak:false` の記録のみ」
/// として握りつぶしてベンチが正常終了しないようにする
/// （PR #380 codex-review P1 指摘・AGENTS.md のテナント境界・fail-closed 基準）。
fn fail_bench(context: &str, detail: &str) -> ! {
    panic!("feature_bench: {context}: {detail}");
}

/// `warmup` 回のウォームアップ後、`iters` 回のレイテンシ（マイクロ秒）を計測する。
/// `f` の戻り値は `black_box` で消費し、コンパイラによる呼び出し省略を避ける。
fn measure_us<F: FnMut() -> R, R>(warmup: usize, iters: usize, mut f: F) -> Vec<u64> {
    for _ in 0..warmup {
        black_box(f());
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        let r = f();
        let elapsed = start.elapsed();
        black_box(r);
        samples.push(elapsed.as_micros() as u64);
    }
    samples
}

/// レイテンシサンプル列とリソーススナップショット差分から [`PhaseStat`] を組み立てる。
fn build_phase(
    name: &'static str,
    samples: Vec<u64>,
    rss_kb_after: u64,
    cpu_tick_delta: u64,
    extra: String,
) -> PhaseStat {
    let iterations = samples.len();
    let (min_us, p50_us, p95_us, p99_us, max_us, mean_us) = percentiles(samples);
    PhaseStat {
        name,
        iterations,
        min_us,
        p50_us,
        p95_us,
        p99_us,
        max_us,
        mean_us,
        rss_kb_after,
        cpu_tick_delta,
        extra,
    }
}

fn phase_to_json(p: &PhaseStat) -> String {
    format!(
        "{{\"name\":\"{}\",\"iterations\":{},\"min_us\":{},\"p50_us\":{},\"p95_us\":{},\
         \"p99_us\":{},\"max_us\":{},\"mean_us\":{:.1},\"rss_kb_after\":{},\
         \"cpu_tick_delta\":{},\"extra\":{{{}}}}}",
        p.name,
        p.iterations,
        p.min_us,
        p.p50_us,
        p.p95_us,
        p.p99_us,
        p.max_us,
        p.mean_us,
        p.rss_kb_after,
        p.cpu_tick_delta,
        p.extra,
    )
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:.6}")).collect();
    format!("[{}]", parts.join(","))
}

/// 決定的な合成本文を生成する（外部 RNG 依存なし。`id`・`lang_idx`・`topic_idx` から
/// `WORDS` を周回選択して数十語のダミーテキストを組み立てる）。
fn make_body(id: u64, lang_idx: usize, topic_idx: usize) -> String {
    let mut s = String::new();
    for w in 0..40usize {
        let idx = (id as usize)
            .wrapping_mul(31)
            .wrapping_add(w * 7)
            .wrapping_add(lang_idx)
            .wrapping_add(topic_idx)
            % WORDS.len();
        s.push_str(WORDS[idx]);
        s.push(' ');
    }
    s.push_str(&format!("doc-{id}"));
    s
}

/// `[start_id, end_id]` 範囲の 1 バッチ分の投入データを構築する。`private_every_n`
/// が `Some(n)` の場合、`id % n == 0` の行を `Visibility::Private` にする
/// （ロング説明の「tenant-a の 10% は Private」を `n = 10` で表現する）。
///
/// 戻り値は (ids, visibilities, embeddings, encoded scalar columns) の 4 本の
/// 並行配列。呼び出し元（[`insert_batch`]）が同じ index で zip して `RowInput` を
/// 組み立てる。
type BatchData = (Vec<u64>, Vec<Visibility>, Vec<Vec<f32>>, Vec<Vec<u8>>);

fn make_batch(
    schema: &TableSchema,
    embedder: &HashingEmbedder,
    start_id: u64,
    end_id: u64,
    private_every_n: Option<u64>,
) -> BatchData {
    let mut ids = Vec::new();
    let mut viss = Vec::new();
    let mut bodies = Vec::new();
    let mut langs = Vec::new();
    let mut topics = Vec::new();
    for id in start_id..=end_id {
        let lang_idx = (id as usize) % LANGS.len();
        let topic_idx = (id as usize) % TOPICS.len();
        let visibility = match private_every_n {
            Some(n) if n > 0 && id % n == 0 => Visibility::Private,
            _ => Visibility::Public,
        };
        ids.push(id);
        viss.push(visibility);
        langs.push(LANGS[lang_idx].to_string());
        topics.push(TOPICS[topic_idx].to_string());
        bodies.push(make_body(id, lang_idx, topic_idx));
    }
    let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let embs = embedder
        .embed_batch(&refs)
        .expect("embed_batch for ingest batch");
    let encoded: Vec<Vec<u8>> = langs
        .iter()
        .zip(topics.iter())
        .zip(bodies.iter())
        .zip(embs.iter())
        .map(|(((lang, topic), body), emb)| {
            encode_scalar_columns(
                schema,
                &[
                    Value::Vector(emb.clone()),
                    Value::Text(lang.clone()),
                    Value::Text(topic.clone()),
                    Value::Text(body.clone()),
                ],
            )
            .expect("encode_scalar_columns for ingest batch")
        })
        .collect();
    (ids, viss, embs, encoded)
}

/// 1 バッチを `insert_rows` で投入し、そのレイテンシ（マイクロ秒）を返す。
fn insert_batch(
    storage: &Storage,
    ctx: &PolicyContext,
    ids: &[u64],
    viss: &[Visibility],
    embs: &[Vec<f32>],
    encoded: &[Vec<u8>],
    op_label: &str,
) -> u64 {
    let inputs: Vec<(u64, RowInput<'_>)> = ids
        .iter()
        .zip(viss.iter())
        .zip(embs.iter())
        .zip(encoded.iter())
        .map(|(((id, vis), emb), meta)| {
            (
                *id,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: *vis,
                    embedding: emb,
                    metadata: meta,
                },
            )
        })
        .collect();
    let op = OperationId::parse(op_label).expect("valid operation id label");
    let start = Instant::now();
    engine::tenant::insert_rows(storage, "docs", ctx, &inputs, &op).expect("insert_rows");
    start.elapsed().as_micros() as u64
}

/// フェーズ 1: `insert_rows` によるバッチ投入（`rows_a` 行を tenant-a・10%
/// Private、`rows_b` 行を tenant-b・全 Public）。バッチ毎レイテンシと
/// スループットを計測する。`rows_a`／`rows_b` は [`main`] が `BENCH_FEATURE_SCALE`
/// 倍率を `ROWS_A_BASE`／`ROWS_B_BASE` へ掛けて決める実行時行数（Issue #413）。
fn run_ingest(
    storage: &Storage,
    schema: &TableSchema,
    embedder: &HashingEmbedder,
    ctx_a: &PolicyContext,
    ctx_b: &PolicyContext,
    rows_a: u64,
    rows_b: u64,
) -> PhaseStat {
    let cpu_before = read_proc_stats().cpu_ticks;
    let mut samples = Vec::new();
    let mut total_rows: u64 = 0;
    let mut batch_no: u64 = 0;

    let mut id = 1u64;
    while id <= rows_a {
        let end = (id + BATCH_SIZE - 1).min(rows_a);
        let (ids, viss, embs, encoded) =
            make_batch(schema, embedder, id, end, Some(TENANT_A_PRIVATE_EVERY_N));
        let us = insert_batch(
            storage,
            ctx_a,
            &ids,
            &viss,
            &embs,
            &encoded,
            &format!("feature-bench-a-{batch_no}"),
        );
        samples.push(us);
        total_rows += ids.len() as u64;
        batch_no += 1;
        id = end + 1;
    }

    let mut id = rows_a + 1;
    let end_b = rows_a + rows_b;
    while id <= end_b {
        let end = (id + BATCH_SIZE - 1).min(end_b);
        let (ids, viss, embs, encoded) = make_batch(schema, embedder, id, end, None);
        let us = insert_batch(
            storage,
            ctx_b,
            &ids,
            &viss,
            &embs,
            &encoded,
            &format!("feature-bench-b-{batch_no}"),
        );
        samples.push(us);
        total_rows += ids.len() as u64;
        batch_no += 1;
        id = end + 1;
    }

    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    let total_us: u64 = samples.iter().sum();
    let rows_per_sec = if total_us > 0 {
        total_rows as f64 / (total_us as f64 / 1_000_000.0)
    } else {
        0.0
    };
    let extra = format!(
        "\"rows_total\":{total_rows},\"batches\":{},\"rows_per_sec\":{rows_per_sec:.1}",
        samples.len()
    );
    build_phase("ingest", samples, after.vm_rss_kb, cpu_delta, extra)
}

/// `execute_sql`（セッション不要の SELECT/集計）を warmup+計測ループで実行する
/// 汎用フェーズヘルパー。事前の 1 回の「プローブ」呼び出しで正当性・行数を確認し、
/// `extra` へ記録する。プローブ・各反復いずれも `Err` を返した場合は構文・実行
/// 契約の退行を示すため、エラー生成時間を p50/p95 として報告せず [`fail_bench`]
/// でベンチを止める（PR #380 codex-review P1 指摘）。
fn run_select_phase(
    name: &'static str,
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> PhaseStat {
    let probe = core.execute_sql(ctx, sql);
    let extra = match &probe {
        Ok(r) => format!("\"ok\":true,\"rows\":{}", r.rows.len()),
        Err(e) => fail_bench(&format!("{name} probe query failed"), &format!("{e:?}")),
    };
    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || match core.execute_sql(ctx, sql) {
        Ok(r) => r,
        Err(e) => fail_bench(&format!("{name} iteration query failed"), &format!("{e:?}")),
    });
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase(name, samples, after.vm_rss_kb, cpu_delta, extra)
}

/// `USING MODE '<mode>'` 付きクエリのフェーズ。`precision` は確信度ゲートにより
/// 空集合（`SqlOutcome::Query` で 0 行）を返しうる契約（エラーではない）ため
/// それ自体はベンチを止めない。一方 `Err`（構文・実行契約の退行）や
/// `SqlOutcome::Query` 以外の想定外の outcome は正常な計測対象ではないため、
/// エラー生成時間を p50/p95 として報告せず [`fail_bench`] でベンチを止める
/// （PR #380 codex-review P1 指摘）。
fn run_mode_phase(
    name: &'static str,
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> PhaseStat {
    let mut probe_session = SessionState::default();
    let probe = core.execute_sql_in_session(ctx, &mut probe_session, sql);
    let extra = match &probe {
        Ok(SqlOutcome::Query(r)) => format!("\"ok\":true,\"rows\":{}", r.rows.len()),
        Ok(_) => fail_bench(
            &format!("{name} probe query"),
            "unexpected non-query outcome",
        ),
        Err(e) => fail_bench(&format!("{name} probe query failed"), &format!("{e:?}")),
    };
    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || {
        let mut session = SessionState::default();
        match core.execute_sql_in_session(ctx, &mut session, sql) {
            Ok(SqlOutcome::Query(r)) => r,
            Ok(_) => fail_bench(&format!("{name} iteration"), "unexpected non-query outcome"),
            Err(e) => fail_bench(&format!("{name} iteration query failed"), &format!("{e:?}")),
        }
    });
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase(name, samples, after.vm_rss_kb, cpu_delta, extra)
}

/// UDF 呼び出しフェーズ（TASK-79・SQL-9）。`CREATE FUNCTION` はセッションへの
/// 登録のため計測対象外（1 回だけセットアップし、以降はそのセッションを使い回して
/// `SELECT ... norm_scale(embedding, 2.0) ...` を計測する）。プローブ・各反復が
/// `Err` あるいは `SqlOutcome::Query` 以外を返した場合は構文・実行契約の退行を
/// 示すため、エラー生成時間を p50/p95 として報告せず [`fail_bench`] でベンチを
/// 止める（PR #380 codex-review P1 指摘）。
fn run_udf_phase(core: &EngineCore, ctx: &PolicyContext, query_vec_literal: &str) -> PhaseStat {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        ctx,
        &mut session,
        "CREATE FUNCTION norm_scale(v, s) AS s * vec_sum(vec_div(v, vec_norm(v)))",
    )
    .expect("CREATE FUNCTION should succeed");
    let sql = format!(
        "SELECT id, norm_scale(embedding, 2.0) AS score FROM docs \
         ORDER BY embedding <=> '{query_vec_literal}' LIMIT 10"
    );
    let probe = core.execute_sql_in_session(ctx, &mut session, &sql);
    let extra = match &probe {
        Ok(SqlOutcome::Query(r)) => format!("\"ok\":true,\"rows\":{}", r.rows.len()),
        Ok(_) => fail_bench("udf_call probe query", "unexpected non-query outcome"),
        Err(e) => fail_bench("udf_call probe query failed", &format!("{e:?}")),
    };
    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || {
        match core.execute_sql_in_session(ctx, &mut session, &sql) {
            Ok(SqlOutcome::Query(r)) => r,
            Ok(_) => fail_bench("udf_call iteration", "unexpected non-query outcome"),
            Err(e) => fail_bench("udf_call iteration query failed", &format!("{e:?}")),
        }
    });
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase("udf_call", samples, after.vm_rss_kb, cpu_delta, extra)
}

fn count_of(result: &QueryResult) -> Option<u64> {
    let row = result.rows.first()?;
    match row.cells.first()? {
        engine::sql::exec::Cell::Integer(n) => Some(*n),
        _ => None,
    }
}

/// `ctx` の `COUNT(*)` を実行し、成功かつ整数セルが得られた件数を返す。クエリ
/// エラー・想定外の結果形状はいずれも RLS 境界の検査不能を意味し、実際の漏えいと
/// 区別できないまま計測を続けてはならないため、[`fail_bench`] で即座にベンチを
/// 止める（PR #380 codex-review P1 指摘。`.ok()` で件数 0 へ畳み込まない）。
fn count_visible_rows_or_fail(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
    label: &str,
) -> u64 {
    match core.execute_sql(ctx, sql) {
        Ok(r) => count_of(&r).unwrap_or_else(|| {
            fail_bench(
                "RLS isolation check",
                &format!("{label}: COUNT(*) の結果が整数セルを持たない"),
            )
        }),
        Err(e) => fail_bench(
            "RLS isolation check",
            &format!("{label}: COUNT(*) query failed: {e:?}"),
        ),
    }
}

/// フェーズ 11: RLS テナント境界の確認。tenant-a（Public のみ／Public+Private）・
/// tenant-b（Public のみ）それぞれから見える件数を、[`run_ingest`] が投入した
/// fixture の期待値と照合し、他テナントの行が混入していない（=期待値どおりの
/// 件数しか見えない）ことを確認する。`PolicyContext::is_visible`（`policy.rs`）は
/// 許可可視性集合の判定後、可視性ラベルが `Public` であれば行テナントの一致判定
/// より先に可視と短絡させる（テナント一致判定を経ない）ため、`Public` 行は
/// テナントを問わず「Public を許可する ctx すべて」から見える（グローバルな
/// 可視性プール）。したがって各 ctx の期待値は「自テナントの Public 件数」では
/// なく「全テナントの Public 件数の合計」を基準にする必要がある（各テナント自身
/// の fixture サイズとのみ比較すると、正しい分離時にも他テナントの Public 行分
/// だけ実測が期待値を上回り false になる。PR #380 Bugbot 指摘）。件数不一致・
/// クエリエラーはいずれも [`count_visible_rows_or_fail`] が fail-closed で
/// ベンチを止めるため、以降の tenant-b 計測へ検査不能なまま進むことはない
/// （PR #380 codex-review P1 指摘・AGENTS.md のテナント境界・fail-closed 基準）。
fn run_rls_isolation_phase(
    core: &EngineCore,
    ctx_a_public: &PolicyContext,
    ctx_a_full: &PolicyContext,
    ctx_b: &PolicyContext,
    rows_a: u64,
    rows_b: u64,
) -> PhaseStat {
    let sql = "SELECT COUNT(*) FROM docs";
    let count_a_public = count_visible_rows_or_fail(core, ctx_a_public, sql, "tenant-a public");
    let count_a_full = count_visible_rows_or_fail(core, ctx_a_full, sql, "tenant-a full");
    let count_b = count_visible_rows_or_fail(core, ctx_b, sql, "tenant-b public");

    // fixture（run_ingest）の期待件数。tenant-a は rows_a 行中
    // id % TENANT_A_PRIVATE_EVERY_N == 0 のみ Private（make_batch 参照）、
    // tenant-b は rows_b 行すべて Public。
    let expected_private_a = rows_a / TENANT_A_PRIVATE_EVERY_N;
    let expected_public_a = rows_a - expected_private_a;
    // Public は全テナントから見えるグローバルプール（上記ドキュメンテーション
    // コメント参照）のため、Public を許可する ctx（tenant-a Public のみ・
    // tenant-b）はいずれも tenant-a と tenant-b の Public 行合計を見る。
    let expected_public_global = expected_public_a + rows_b;
    let expected_full_a = expected_public_global + expected_private_a;

    // fail-closed: 件数が 1 つでも期待値とずれたら、それを `no_leak:false` として
    // 記録したまま計測を続けず、[`fail_bench`] で即座にベンチを止める
    // （検査不能と実際のテナント漏えいを区別できないまま成功終了させない）。
    if count_a_public != expected_public_global {
        fail_bench(
            "RLS isolation check",
            &format!(
                "tenant-a public COUNT(*) mismatch: expected {expected_public_global}, got {count_a_public}"
            ),
        );
    }
    if count_a_full != expected_full_a {
        fail_bench(
            "RLS isolation check",
            &format!(
                "tenant-a full COUNT(*) mismatch: expected {expected_full_a}, got {count_a_full}"
            ),
        );
    }
    if count_b != expected_public_global {
        fail_bench(
            "RLS isolation check",
            &format!("tenant-b public COUNT(*) mismatch: expected {expected_public_global}, got {count_b}"),
        );
    }

    let private_rows_a = count_a_full.saturating_sub(count_a_public);
    let extra = format!(
        "\"count_tenant_a_public\":{count_a_public},\"count_tenant_a_full\":{count_a_full},\
         \"count_tenant_b_public\":{count_b},\"private_rows_tenant_a\":{private_rows_a},\
         \"no_cross_tenant_leak\":true"
    );

    let cpu_before = read_proc_stats().cpu_ticks;
    let samples = measure_us(WARMUP, ITERS, || match core.execute_sql(ctx_b, sql) {
        Ok(r) => r,
        Err(e) => fail_bench("RLS isolation measurement query failed", &format!("{e:?}")),
    });
    let after = read_proc_stats();
    let cpu_delta = after.cpu_ticks.saturating_sub(cpu_before);
    build_phase("rls_isolation", samples, after.vm_rss_kb, cpu_delta, extra)
}

fn db_file_size_bytes(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `hnsw::MAX_HNSW_NODES`（1,000,000）を `ROWS_A_BASE + ROWS_B_BASE`（25,000）で
/// 割った最大倍率。`BENCH_FEATURE_SCALE` の上限として `bench_engine::parse_scale`
/// へ渡す（Issue #413）。
const MAX_FEATURE_SCALE: u64 = engine::hnsw::MAX_HNSW_NODES as u64 / (ROWS_A_BASE + ROWS_B_BASE);

fn main() {
    let engine_choice = match bench_engine::read_env_var("BENCH_FEATURE_ENGINE")
        .and_then(|raw| bench_engine::parse_engine(raw.as_deref()))
    {
        Ok(e) => e,
        Err(e) => fail_bench("BENCH_FEATURE_ENGINE", &e.to_string()),
    };
    let scale = match bench_engine::read_env_var("BENCH_FEATURE_SCALE")
        .and_then(|raw| bench_engine::parse_scale(raw.as_deref(), MAX_FEATURE_SCALE))
    {
        Ok(s) => s,
        Err(e) => fail_bench("BENCH_FEATURE_SCALE", &e.to_string()),
    };
    let rows_a = ROWS_A_BASE * scale;
    let rows_b = ROWS_B_BASE * scale;
    let dim: u32 = match bench_engine::read_env_var("BENCH_FEATURE_DIM").and_then(|raw| {
        bench_engine::parse_dim(
            raw.as_deref(),
            bench_engine::DEFAULT_BENCH_DIM,
            bench_engine::MAX_BENCH_DIM,
        )
    }) {
        Ok(d) => d,
        Err(e) => fail_bench("BENCH_FEATURE_DIM", &e.to_string()),
    };

    let mut db_path = std::env::temp_dir();
    db_path.push(format!(
        "vector-db-feature-bench-{}-{:x}.redb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    ));
    // プロセス終了時に一時 DB ファイルを削除する RAII ガード。
    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = CleanupGuard(db_path.clone());

    let embedder = HashingEmbedder::new(dim).expect("valid embedder dim");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("topic", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );

    let storage = Storage::open(&db_path).expect("open storage");
    storage.create_table(&schema).expect("create table");

    let ctx_a_public = PolicyContext::new("tenant-a").expect("valid tenant ctx");
    let ctx_a_full =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant ctx");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant ctx");

    let mut phases: Vec<PhaseStat> = vec![run_ingest(
        &storage,
        &schema,
        &embedder,
        &ctx_a_public,
        &ctx_b,
        rows_a,
        rows_b,
    )];
    let rows_after_ingest_bytes = db_file_size_bytes(&db_path);

    // クエリベクトル・クエリ文の周回プール（`core` の構築に先立って用意する。
    // 索引構築時間プローブ〔下記〕が `qv0` を必要とするため）。
    let query_vecs: Vec<Vec<f32>> = QUERY_TEXTS
        .iter()
        .map(|t| {
            embedder
                .embed_batch(&[t])
                .expect("embed query text")
                .remove(0)
        })
        .collect();
    // 埋め込み器の出力次元と schema の `dim` が食い違ったまま計測を続けると、
    // ベクトルカーネルへ形の合わない入力を渡すことになるため即座に止める
    // （Issue #466。`BENCH_FEATURE_DIM` 上書き経路の fail-closed 検査）。
    if query_vecs[0].len() != dim as usize {
        fail_bench(
            "BENCH_FEATURE_DIM",
            &format!(
                "query embedding dim mismatch: expected {dim}, got {}",
                query_vecs[0].len()
            ),
        );
    }
    let qv0 = vec_literal(&query_vecs[0]);
    let qv1 = vec_literal(&query_vecs[1 % query_vecs.len()]);
    let qv2 = vec_literal(&query_vecs[2 % query_vecs.len()]);

    // 索引構築時間の一発計測（Issue #413 設計判断 4。Issue #439 codex-review
    // 指摘への対応で計測対象インスタンスを分離）。`vector_knn` と同形のクエリを
    // 実行し、hnsw では arena デコード＋HNSW 構築、brute_force では
    // `SqlArenaCache`（Issue #363）の cold 構築に相当する経過時間を記録する。
    //
    // 計測は ingest 済み db ファイルを複製した**別ファイル・別 `EngineCore`
    // インスタンス**（`probe_core`）に対して行い、13 フェーズの実行に使う
    // `core` のキャッシュ・索引状態には一切触れない。これにより after/既定・
    // after/hnsw いずれも 13 フェーズ開始時点で cold のまま揃い、事前ウォーム
    // アップを持たない before バイナリ（`0803a8c`）と 13 フェーズの測定条件が
    // 一致する（比較対象は 13 フェーズの p50/p95 のみであることを
    // `docs/design/hnsw-index.md`「測定条件」に明記する）。
    let probe_db_path = {
        let mut p = db_path.clone();
        let mut file_name = p.file_name().expect("db_path has file name").to_os_string();
        file_name.push("-probe");
        p.set_file_name(file_name);
        p
    };
    std::fs::copy(&db_path, &probe_db_path)
        .unwrap_or_else(|e| fail_bench("probe db copy failed", &format!("{e}")));
    let _cleanup_probe = CleanupGuard(probe_db_path.clone());
    let probe_storage = Storage::open(&probe_db_path).expect("open probe storage");
    let probe_core = match engine_choice {
        BenchEngine::BruteForce => {
            EngineCore::from_storage(probe_storage, engine::search_engine::default_engine())
        }
        BenchEngine::Hnsw => {
            let kind = engine::search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid HnswParams::default()");
            EngineCore::from_storage_with_engine(probe_storage, kind)
        }
        BenchEngine::HnswF16 => {
            // f16 常駐 opt-in（Issue #514・#516）。`ValidatedHnswParams::new` を
            // 経由した検証済み値へ `with_resident_precision` を適用する構築経路は
            // `tests/fixtures/recall_engine.rs::RecallEngine::HnswF16` と同一
            // （untrusted な `HnswParams` の唯一の検証入口はここでも変わらない）。
            let validated =
                engine::hnsw::ValidatedHnswParams::new(engine::hnsw::HnswParams::default())
                    .expect("valid HnswParams::default()")
                    .with_resident_precision(engine::hnsw::ResidentPrecision::F16);
            let kind = engine::search_engine::SearchEngineKind::Hnsw(validated);
            EngineCore::from_storage_with_engine(probe_storage, kind)
        }
        BenchEngine::HnswI8 => {
            // I8（SQ8）常駐 opt-in（Issue #521・#523）。f16 arm と同一の構築経路。
            let validated =
                engine::hnsw::ValidatedHnswParams::new(engine::hnsw::HnswParams::default())
                    .expect("valid HnswParams::default()")
                    .with_resident_precision(engine::hnsw::ResidentPrecision::I8);
            let kind = engine::search_engine::SearchEngineKind::Hnsw(validated);
            EngineCore::from_storage_with_engine(probe_storage, kind)
        }
    };
    let index_warm_start = Instant::now();
    let index_warm_probe = probe_core.execute_sql(
        &ctx_a_full,
        &format!("SELECT * FROM docs ORDER BY embedding <=> '{qv0}' LIMIT 10"),
    );
    let index_warm_us = index_warm_start.elapsed().as_micros() as u64;
    if let Err(e) = index_warm_probe {
        fail_bench("index warm-up probe query failed", &format!("{e:?}"));
    }
    drop(probe_core);
    let _ = std::fs::remove_file(&probe_db_path);

    // `insert_rows` で書き込み済みの `Storage` を `EngineCore` へ引き渡す
    // （`EngineCore::open` による同一ファイルへの二重オープンを避ける。
    // `from_storage` は所有権移動のみでテナント境界の迂回経路を新設しない）。
    // 上記プローブとは独立のインスタンスであり、13 フェーズはこの `core` が
    // cold な状態から開始する。
    let core = match engine_choice {
        BenchEngine::BruteForce => {
            EngineCore::from_storage(storage, engine::search_engine::default_engine())
        }
        BenchEngine::Hnsw => {
            let kind = engine::search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid HnswParams::default()");
            EngineCore::from_storage_with_engine(storage, kind)
        }
        BenchEngine::HnswF16 => {
            // 上の probe_core 構築と同一経路（Issue #514・#516）。
            let validated =
                engine::hnsw::ValidatedHnswParams::new(engine::hnsw::HnswParams::default())
                    .expect("valid HnswParams::default()")
                    .with_resident_precision(engine::hnsw::ResidentPrecision::F16);
            let kind = engine::search_engine::SearchEngineKind::Hnsw(validated);
            EngineCore::from_storage_with_engine(storage, kind)
        }
        BenchEngine::HnswI8 => {
            // 上の probe_core 構築と同一経路（Issue #521・#523）。
            let validated =
                engine::hnsw::ValidatedHnswParams::new(engine::hnsw::HnswParams::default())
                    .expect("valid HnswParams::default()")
                    .with_resident_precision(engine::hnsw::ResidentPrecision::I8);
            let kind = engine::search_engine::SearchEngineKind::Hnsw(validated);
            EngineCore::from_storage_with_engine(storage, kind)
        }
    };

    // 許可リスト構文（`sql::allowlist`）は非集計 `SELECT` に `ORDER BY` を必須と
    // するため（`LIMIT` 単体では受理されない）、`WHERE` フィルタの効果を見るために
    // `ORDER BY embedding <=> '<vec>'` を併記する（`tests/sql_surface.rs` の
    // `WHERE lang = 'ja' ORDER BY ... LIMIT ...` と同じ形）。
    phases.push(run_select_phase(
        "point_where",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{qv0}' LIMIT 10"),
    ));
    phases.push(run_select_phase(
        "where_compound",
        &core,
        &ctx_a_full,
        "SELECT COUNT(*) FROM docs WHERE visible() AND id > 100 AND lang = 'ja'",
    ));
    phases.push(run_select_phase(
        "agg_count",
        &core,
        &ctx_a_full,
        "SELECT COUNT(*) FROM docs",
    ));
    phases.push(run_select_phase(
        "agg_multi",
        &core,
        &ctx_a_full,
        "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs",
    ));
    phases.push(run_select_phase(
        "group_by_having",
        &core,
        &ctx_a_full,
        "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n > 1 ORDER BY n DESC LIMIT 5",
    ));
    phases.push(run_select_phase(
        "vector_knn",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs ORDER BY embedding <=> '{qv0}' LIMIT 10"),
    ));
    phases.push(run_select_phase(
        "vector_knn_where",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{qv1}' LIMIT 10"),
    ));
    phases.push(run_select_phase(
        "hybrid_rrf",
        &core,
        &ctx_a_full,
        &format!(
            "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{qv2}', body, '{}') LIMIT 10",
            QUERY_TEXTS[2 % QUERY_TEXTS.len()]
        ),
    ));
    phases.push(run_mode_phase(
        "mode_recall",
        &core,
        &ctx_a_full,
        &format!("SELECT * FROM docs ORDER BY embedding <=> '{qv0}' LIMIT 10 USING MODE 'recall'"),
    ));
    phases.push(run_mode_phase(
        "mode_precision",
        &core,
        &ctx_a_full,
        &format!(
            "SELECT * FROM docs ORDER BY embedding <=> '{qv0}' LIMIT 10 USING MODE 'precision'"
        ),
    ));
    phases.push(run_rls_isolation_phase(
        &core,
        &ctx_a_public,
        &ctx_a_full,
        &ctx_b,
        rows_a,
        rows_b,
    ));
    phases.push(run_udf_phase(&core, &ctx_a_full, &qv0));

    // 非 vacuous 確認（Issue #413 設計判断 5）。hnsw opt-in 時、全フェーズ計測後に
    // 索引が実際に構築・使用されたことを固定する（構築失敗→負のキャッシュ→
    // 黙って brute-force で「ANN 計測」を誤報告する経路を防ぐ。
    // `ann-recall-gate-verification.md` の `builds=1` 確認と同じ原則）。
    // brute_force では `hnsw_index_cache_stats()` は常に全欄 0（索引を一切
    // 構築しない構造）のため統計出力・アサートの対象外とする。
    let hnsw_stats_json = match engine_choice {
        BenchEngine::Hnsw | BenchEngine::HnswF16 | BenchEngine::HnswI8 => {
            let s = core.hnsw_index_cache_stats();
            if s.builds == 0 || s.hits == 0 {
                fail_bench(
                    "ANN non-vacuous check",
                    &format!(
                        "hnsw engine did not build/hit an index: builds={} hits={}",
                        s.builds, s.hits
                    ),
                );
            }
            // f16 常駐 opt-in（Issue #516）では、embedding が f16 有限範囲外
            // （D6）で自動縮退した回数もあわせて出力する。f32 常駐（`BenchEngine::
            // Hnsw`）はこのカウンタが常に 0 のままだが、区別なく出力して doc
            // 側の前後比較表から両エンジンの JSON を同一に扱えるようにする。
            format!(
                "\"builds\":{},\"build_failures\":{},\"rebuilds\":{},\"hits\":{},\
                 \"misses\":{},\"fallbacks\":{},\"plain_scans\":{},\
                 \"subset_searches\":{},\"hybrid_dense_searches\":{},\
                 \"hybrid_queries\":{},\"ef_cap_fallbacks\":{},\"entries\":{},\
                 \"f16_residency_fallbacks\":{},\"i8_residency_fallbacks\":{}",
                s.builds,
                s.build_failures,
                s.rebuilds,
                s.hits,
                s.misses,
                s.fallbacks,
                s.plain_scans,
                s.subset_searches,
                s.hybrid_dense_searches,
                s.hybrid_queries,
                s.ef_cap_fallbacks,
                s.entries,
                s.f16_residency_fallbacks,
                s.i8_residency_fallbacks,
            )
        }
        BenchEngine::BruteForce => String::new(),
    };

    let final_db_bytes = db_file_size_bytes(&db_path);
    let final_proc = read_proc_stats();

    let mut out = String::new();
    out.push_str("{\"meta\":{");
    out.push_str(&format!(
        "\"rows_tenant_a\":{rows_a},\"rows_tenant_b\":{rows_b},\"dim\":{dim},\
         \"batch_size\":{BATCH_SIZE},\"warmup\":{WARMUP},\"iters\":{ITERS},\
         \"db_bytes_after_ingest\":{rows_after_ingest_bytes},\
         \"db_bytes_final\":{final_db_bytes},\
         \"vm_rss_kb_final\":{},\"vm_hwm_kb_final\":{},\
         \"label\":\"feature_bench\",\
         \"engine\":\"{}\",\"scale\":{scale},\"rows_total\":{},\
         \"index_warm_us\":{index_warm_us}",
        final_proc.vm_rss_kb,
        final_proc.vm_hwm_kb,
        engine_choice.token(),
        rows_a + rows_b,
    ));
    if !hnsw_stats_json.is_empty() {
        out.push_str(",\"hnsw_stats\":{");
        out.push_str(&hnsw_stats_json);
        out.push('}');
    }
    out.push_str("},\"phases\":[");
    for (i, p) in phases.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&phase_to_json(p));
    }
    out.push_str("]}");
    println!("{out}");
}
