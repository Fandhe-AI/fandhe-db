//! 述語（`OR`／`IN`）・スカラー `ORDER BY`・2 テーブル結合の p95 計測ベンチ（Issue #1204。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・SQL-25・SQL-28、RLS-10）。
//!
//! これらの形は `EngineCore::execute_sql_in_session`（SQL 表層）の関係演算経路を通るが、
//! 回帰ベンチが無かったため p95 を測る入口として追加する。規模は SQL-24／25／28 の
//! 数値基準の計測規模に合わせる（値は `harness::relational_p95` の定数。spec 本文は転記しない）。
//!
//! # 出力規約
//!
//! 情報提供専用で CI（`.github/workflows/*`）へは配線しない（GitHub ホステッド runner では
//! 起動を拒否する）。共有環境の値は spec 閾値の確定判定に使えない（policy §5・§6）ため、
//! 閾値行は `BENCH_DEDICATED_ENV=1` の申告が無い限り「未評価」と出力する。
//! 縮小規模（`BENCH_RELATIONAL_P95_ROWS`）の run は `scale=reduced` と自己ラベルされ、記録には使わない。
//!
//! # 安全策
//!
//! fixture には必ず他テナントの Private 行を混ぜ、計測前に各 arm を 1 回実行して
//! 「受理される・非空・他テナント行が混入しない・順序／述語／結合キーが期待どおり」を
//! 検査する（RLS を外した状態や空振りを測る事態を fail-closed で防ぐ）。
//! `LIMIT` 後の結果だけでは越境を見逃すため、他テナント行は各 arm の上位に必ず並ぶ
//! sentinel（述語 arm は計測クエリと同一のベクトル・`lang=l0`、順序 arm は最小／最大 `qty`）として投入し、
//! 結合は一意キーの `ORDER BY` 付きで `MAX_SEARCH_K` 以内の `LIMIT`／`OFFSET` ページ分割により全件取得し、id 集合まで照合する。加えて他テナント文脈で同じ文が
//! 他テナント行を返すこと（＝越境が起きれば見える fixture であること）を確認する。
//!
//! # 述語経路の段別計測（Issue #1319。SQL-24・SQL-2 ポインタ）
//!
//! 述語グループは p95 行の後に、arm ごとの段別内訳（`stage_round`／`stage_summary`／`stage_diff` 行。
//! 全ラウンドのサンプルをプールした median・Q1・Q3）を出力する。`perf` が使えない環境でも、
//! プロセス内のタイマーと件数だけで PlainScan 反実仮想の「全件 SCALAR 評価の固定費 F」と
//! 「一致行の複製費 c」を分離できる。engine 本体は無変更で、段は pub API によるベンチ内の再実装
//! （`path=current` は現 HEAD の索引 trusted-mask 経路、`path=plain_scan_ref` は #1275 時点の
//! PlainScan 経路の反実仮想。後者は現 HEAD の述語 arm の実経路ではない）。出力は閾値判定に使わない。
//! 毎ラウンド、各段の結果を fixture 規則から独立に導いた期待と照合し、違反は非 0 終了する。
//!
//! 使い方は `make bench-relational-p95`。時間非依存の判定ロジックは
//! `harness::relational_p95` にあり `tests/relational_p95_accept.rs` が `make ci` で検証する。

#[allow(dead_code)]
mod harness;

use std::time::Duration;

use harness::env_report::EnvReport;
use harness::protocol::{run_bounded_retain, MeasurementConfig};
use harness::relational_p95::{
    arm_branch_values, arm_lang_values, arm_pred_form, author_id_for_doc, checked_residual,
    checked_stage_diff, expected_match_ids, expected_order_multi, expected_order_single,
    expected_path, inner_product_distance, interleave_with_reference, is_exact_id_set,
    join_scale_label, join_statement, lang_for_id, lang_in_first_n, lang_token,
    order_by_statements, other_tenant_rows, pair_ratio, parse_group, parse_rounds,
    parse_rows_scale, predicate_statements, qty_for_id, quantile_nearest_rank,
    refuse_under_github_actions, render_round_line, render_stage_diff_line,
    render_stage_round_line, render_stage_summary_line, render_summary_line, render_threshold_line,
    rotate_arms, round_p95, scale_label, sentinel_dominates, sentinel_qty, sentinel_scale,
    summarize_rounds, summarize_stage, topk_matches, visible_doc_rows, Group, PredForm, StagePath,
    StageSummary, JOIN_ROWS, PRED_LIMIT, REFERENCE_ARM, STAGE_E2E, STAGE_IDX_CANDIDATE_RESOLVE,
    STAGE_IDX_SEARCH_SUBSET, STAGE_PLAIN_COPY, STAGE_PLAIN_SCALAR_EVAL, STAGE_PLAIN_SEARCH,
    WIDE_LIMIT,
};
use harness::rng::DeterministicRng;
use harness::sql_c1::vector_literal;

use engine::arena::VectorArena;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::declarative_filter::{matches_all, DeclarativeFilter, MetadataFilter};
use engine::kernel::{CandidateHit, SearchInput, SearchProvider, SubsetSearchInput};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, scan_scalar_columns_masked, Value};
use engine::search_engine;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 768;
const JOIN_DIM: u32 = 2;
const SEED_BATCH_ROWS: usize = 10_000;
const TENANT_A: &str = "bench-tenant-a";
const TENANT_B: &str = "bench-tenant-b";
const DOCS: &str = "docs";
const DOCUMENTS: &str = "documents";
const AUTHORS: &str = "authors";
/// 結合右辺の越境検出用 probe 間隔。自テナント文書のうち `id % JOIN_PROBE_MOD == JOIN_PROBE_MOD - 1` の行は
/// 他テナントの作者行（右辺）を結合キーに持つ。右辺の RLS が効く限り内部結合から脱落し、右辺 RLS だけが
/// 外れると結果へ現れる（事前検査で検出できる fixture。RLS-10）。
const JOIN_PROBE_MOD: u64 = 100;
/// 結合の全件検査のページ幅・最大ページ数（`OFFSET` は `MAX_SEARCH_K` 以内）。
const JOIN_PAGE_ROWS: usize = 5_000;
const JOIN_PAGES: usize = 3;
/// ラウンドあたりの warmup／計測回数（p95 の分解能を確保するため計測 200 回）。
const WARMUP: u32 = 20;
const MEASURED: u32 = 200;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("relational_p95_bench: {msg}");
    std::process::exit(1);
}

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "n/a".to_string())
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")))
}

/// 1 テナント分の行を `SEED_BATCH_ROWS` 単位のチャンクで投入する。
/// `make_values(id)` が列値（先頭はベクトル列）を返す。
fn seed_rows(
    storage: &Storage,
    schema: &TableSchema,
    tenant_id: &str,
    visibility: Visibility,
    ids: std::ops::Range<u64>,
    make_values: &dyn Fn(u64) -> Vec<Value>,
) {
    let policy = ctx(tenant_id);
    let mut next = ids.start;
    while next < ids.end {
        let batch_len = SEED_BATCH_ROWS.min((ids.end - next) as usize);
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
        let mut metadata: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
        for i in 0..batch_len {
            let values = make_values(next + i as u64);
            let vector = match values.first() {
                Some(Value::Vector(v)) => v.clone(),
                _ => fail_closed("first column must be a vector"),
            };
            let encoded = encode_scalar_columns(schema, &values)
                .unwrap_or_else(|e| fail_closed(format!("encode row: {e}")));
            vectors.push(vector);
            metadata.push(encoded);
        }
        let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
            .map(|i| {
                (
                    next + i as u64,
                    RowInput {
                        tenant_id,
                        visibility,
                        embedding: &vectors[i],
                        metadata: &metadata[i],
                    },
                )
            })
            .collect();
        let op_id = OperationId::parse(&format!("seed-{}-{tenant_id}-{next}", schema.name))
            .unwrap_or_else(|e| fail_closed(format!("operation_id: {e}")));
        tenant::insert_rows(storage, &schema.name, &policy, &rows, &op_id)
            .unwrap_or_else(|e| fail_closed(format!("seed insert: {e}")));
        next += batch_len as u64;
    }
}

fn docs_schema() -> TableSchema {
    TableSchema::new(
        DOCS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::BigInt, false),
        ],
    )
}

fn documents_schema() -> TableSchema {
    TableSchema::new(
        DOCUMENTS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(JOIN_DIM), false),
            ColumnDef::new("title", ColumnType::Text, false),
            ColumnDef::new("author_id", ColumnType::BigInt, false),
        ],
    )
}

fn authors_schema() -> TableSchema {
    TableSchema::new(
        AUTHORS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(JOIN_DIM), false),
            ColumnDef::new("name", ColumnType::Text, false),
        ],
    )
}

fn open_storage(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(&format!("issue1204-relational-p95-{label}"));
    let guard = CleanupGuard(path.clone());
    let storage =
        Storage::open(&path).unwrap_or_else(|e| fail_closed(format!("open storage: {e}")));
    (storage, guard)
}

fn exec(core: &EngineCore, tenant_ctx: &PolicyContext, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(tenant_ctx, &mut session, sql) {
        Ok(SqlOutcome::Query(result)) => result,
        Ok(_) => fail_closed("statement did not return a query result"),
        Err(e) => fail_closed(format!("statement rejected: {e}")),
    }
}

fn cell_i64(cell: Option<&Cell>) -> i64 {
    match cell {
        Some(Cell::SignedInteger(v)) => *v,
        _ => fail_closed("expected a signed integer cell"),
    }
}

fn cell_text(cell: Option<&Cell>) -> &str {
    match cell {
        Some(Cell::Text(s)) => s.as_str(),
        _ => fail_closed("expected a text cell"),
    }
}

/// 述語 arm の期待結果（文脈から見える述語充足行の `(id, 距離)` の距離昇順）を fixture から求める。
/// 計測クエリと同じベクトル列を参照実装の負の内積距離（エンジンのスコアは内積）で並べ、エンジン出力とは独立に導出する。
/// `other_rows > 0`（他テナント文脈）のときは、計測クエリと同一ベクトルの sentinel（距離 0・`l0`）を加える。
fn expected_predicate_ranked(
    arm: &str,
    own_rows: u64,
    other_rows: u64,
    sentinel: &[f32],
    query: &[f32],
) -> Vec<(u64, f64)> {
    let n = predicate_lang_count(arm);
    let mut ranked: Vec<(u64, f64)> = (0..own_rows)
        .filter(|id| lang_in_first_n(lang_for_id(*id), n))
        .map(|id| (id, inner_product_distance(&rng_vector_for(id), query)))
        .collect();
    let sentinel_dist = inner_product_distance(sentinel, query);
    ranked.extend((own_rows..own_rows + other_rows).map(|id| (id, sentinel_dist)));
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
    ranked
}

/// arm が許可する `lang` 集合の大きさ（`l0`..`l{n-1}`）。
fn predicate_lang_count(arm: &str) -> u64 {
    match arm {
        "pred_eq" | "pred_or_same" => 1,
        "pred_or2" | "pred_in2" => 2,
        _ => 8,
    }
}

/// 述語 arm の事前検査: 非空・他テナント id 非混入・述語充足に加え、fixture から導出した
/// 期待上位 `PRED_LIMIT` 行（件数・id 集合・距離順。同値境界は許容）との照合。
/// 述語が誤って狭い集合だけを返す等の取り違えを検出する。
fn precheck_predicate(
    arm: &str,
    result: &QueryResult,
    (own_rows, visible_ids): (u64, u64),
    ranked: &[(u64, f64)],
) {
    if result.rows.is_empty() {
        fail_closed(format!("{arm}: empty result"));
    }
    let n = predicate_lang_count(arm);
    for row in &result.rows {
        if row.id >= visible_ids {
            fail_closed(format!("{arm}: row outside the visible id range"));
        }
        let lang = if row.id < own_rows {
            lang_for_id(row.id)
        } else {
            lang_token(0)
        };
        if !lang_in_first_n(lang, n) {
            fail_closed(format!("{arm}: row does not satisfy predicate"));
        }
    }
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    if !topk_matches(&ids, ranked, PRED_LIMIT) {
        fail_closed(format!("{arm}: result is not the expected top rows"));
    }
}

/// `EXPLAIN` の `scalar_plan:` 行から経路トークン（先頭語）を取り出す。
fn explain_scalar_plan_token(core: &EngineCore, tenant_ctx: &PolicyContext, sql: &str) -> String {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(tenant_ctx, &mut session, &format!("EXPLAIN {sql}")) {
        Ok(SqlOutcome::Explain(result)) => result
            .rows
            .iter()
            .filter_map(|r| match r.cells.first() {
                Some(Cell::Text(t)) => t.strip_prefix("scalar_plan: "),
                _ => None,
            })
            .find_map(|t| t.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| fail_closed("explain: no scalar_plan line")),
        _ => fail_closed("explain: unexpected outcome"),
    }
}

/// 述語 arm の経路自己検査: EXPLAIN トークンとカウンタ差分が `expected_path` と一致することを確かめ、
/// 出力は arm ラベル・トークン・カウンタ差分だけにする（テナント ID・行の値・SQL 全文は出さない）。
/// 呼び出し前に全 arm を 1 回ずつ実行してキャッシュを温めておくこと（初回は redb 走査経路を通るため）。
fn check_predicate_path(core: &EngineCore, tenant_ctx: &PolicyContext, label: &str, sql: &str) {
    let (token_expected, indexed) = expected_path(label);
    let token = explain_scalar_plan_token(core, tenant_ctx, sql);
    let (i0, c0) = (
        core.scalar_index_cache_stats(),
        core.sql_arena_cache_stats(),
    );
    let _ = exec(core, tenant_ctx, sql);
    let (i1, c1) = (
        core.scalar_index_cache_stats(),
        core.sql_arena_cache_stats(),
    );
    let d_trusted = i1
        .index_trusted_mask_scans
        .saturating_sub(i0.index_trusted_mask_scans);
    let d_scans = i1.index_scans.saturating_sub(i0.index_scans);
    let d_plain = i1
        .plain_scan_fallbacks
        .saturating_sub(i0.plain_scan_fallbacks);
    let d_copy = c1
        .full_rebuild_copies
        .saturating_sub(c0.full_rebuild_copies);
    println!(
        "path arm={label} scalar_plan={token} d_index_scans={d_scans} d_trusted_mask_scans={d_trusted} d_plain_scan_fallbacks={d_plain} d_full_rebuild_copies={d_copy}"
    );
    let ok = token == token_expected
        && if indexed {
            d_trusted == 1 && d_copy == 0
        } else {
            d_scans == 0 && d_copy == 1
        };
    if !ok {
        fail_closed(format!("{label}: unexpected execution path"));
    }
}

/// 順序 arm の事前検査: 非空・可視 id 範囲外の混入なし・id の重複なし・各行の値が fixture と一致・
/// 値列が可視行 `visible`（`visible_doc_rows`）から導出した期待上位 `WIDE_LIMIT` 行と一致。
/// 対象テナント文脈は自行のみ、他テナント文脈は自行と sentinel を `visible` に含めて同じ関数で検査する
/// （計測対象の文そのものの結果を照合する。同値境界の id 差は許すため値列で比べる）。
fn precheck_order(
    arm: &str,
    result: &QueryResult,
    visible: &[(u64, &'static str, i64)],
    sentinel_from: Option<u64>,
) {
    if result.rows.is_empty() {
        fail_closed(format!("{arm}: empty result"));
    }
    // 他テナント文脈（`sentinel_from = Some(own_rows)`）では、sentinel が上位へ実際に現れること
    // （越境すれば必ず検出できる fixture であること）を実測で確認する。
    if sentinel_from.is_some_and(|own| !result.rows.iter().any(|r| r.id >= own)) {
        fail_closed(format!("{arm}: no sentinel row in the top rows"));
    }
    let by_id: std::collections::HashMap<u64, (&str, i64)> =
        visible.iter().map(|r| (r.0, (r.1, r.2))).collect();
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        fail_closed(format!("{arm}: duplicate row ids"));
    }
    for r in &result.rows {
        let Some((lang, qty)) = by_id.get(&r.id).copied() else {
            fail_closed(format!("{arm}: row outside the visible id range"));
        };
        let (qty_col, lang_col) = if arm == "order_single" {
            (1, None)
        } else {
            (2, Some(1))
        };
        if cell_i64(r.cells.get(qty_col)) != qty
            || lang_col.is_some_and(|c| cell_text(r.cells.get(c)) != lang)
        {
            fail_closed(format!("{arm}: values do not match fixture for id"));
        }
    }
    if arm == "order_single" {
        let got: Vec<i64> = result
            .rows
            .iter()
            .map(|r| cell_i64(r.cells.get(1)))
            .collect();
        if got != expected_order_single(visible, WIDE_LIMIT) {
            fail_closed("order_single: result is not the expected top rows ascending");
        }
    } else {
        let got: Vec<(&str, i64)> = result
            .rows
            .iter()
            .map(|r| (cell_text(r.cells.get(1)), cell_i64(r.cells.get(2))))
            .collect();
        if got != expected_order_multi(visible, WIDE_LIMIT) {
            fail_closed("order_multi: result is not the expected top rows");
        }
    }
}

/// arm 群を輪番で N ラウンド計測し、要約行を出力する。
fn measure_group(
    group: &str,
    size: (usize, &str),
    rounds: u32,
    core: &EngineCore,
    tenant_ctx: &PolicyContext,
    arms: &[(&'static str, String)],
    with_ratio: bool,
) {
    let (rows, scale) = size;
    let config = MeasurementConfig::new(WARMUP, MEASURED, 1)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));
    let mut per_arm: Vec<Vec<Duration>> = vec![Vec::new(); arms.len()];
    let ref_idx = arms.iter().position(|(l, _)| *l == REFERENCE_ARM);
    // 比率あり群は参照 arm を各候補の直前に挟む（policy §3 の baseline/cand1/baseline/cand2 輪番）。
    // 各候補は直前に測った参照 arm の p95 と対にして保持し、時間方向の環境変動を比率へ混入させない。
    let ratio_ref = if with_ratio { ref_idx } else { None };
    let mut pairs: Vec<Vec<(Duration, Duration)>> = vec![Vec::new(); arms.len()];
    for round in 0..rounds as usize {
        let load = loadavg();
        let mut last_ref: Option<Duration> = None;
        for idx in interleave_with_reference(round, arms.len(), ratio_ref) {
            let Some((label, sql)) = arms.get(idx) else {
                continue;
            };
            // 戻り値（`QueryResult`）の解放を計測区間の外へ出す（`retain_capacity == 0` は計測直後に drop）。
            let (m, _) = run_bounded_retain(&config, 0, || {
                let mut session = SessionState::default();
                core.execute_sql_in_session(tenant_ctx, &mut session, sql)
                    .unwrap_or_else(|e| fail_closed(format!("{label}: execute: {e}")))
            })
            .unwrap_or_else(|e| fail_closed(format!("{label}: protocol violation: {e}")));
            let p95 = round_p95(&m.samples).unwrap_or_else(|e| fail_closed(e));
            println!(
                "{}",
                render_round_line(group, label, round + 1, p95, m.summary.median, &load)
            );
            // 参照 arm は 1 ラウンドに候補数だけ現れる。要約（min_of_n・中央値・ラン間の幅）を
            // N ラウンド統計に保つため、参照 arm はラウンド最初の 1 回だけ per_arm へ記録する
            // （比率用の対応付け p95 は pairs に別途保持し、全出現の round 行は出力する）。
            if ratio_ref != Some(idx) || last_ref.is_none() {
                if let Some(v) = per_arm.get_mut(idx) {
                    v.push(p95);
                }
            }
            if ratio_ref == Some(idx) {
                last_ref = Some(p95);
            } else if let (Some(r), Some(v)) = (last_ref, pairs.get_mut(idx)) {
                v.push((p95, r));
            }
        }
    }
    let summaries: Vec<_> = per_arm
        .iter()
        .map(|v| summarize_rounds(v).unwrap_or_else(|e| fail_closed(e)))
        .collect();
    for (i, ((label, _), summary)) in arms.iter().zip(&summaries).enumerate() {
        // 比率は候補 min ÷ 「その min を出したペアで直前に測った参照 arm の p95」。分子と分母を同一ペアに
        // 揃え、分母を `ref_paired` として併記するので、出力した値から比率を再現できる。
        let ratio = match (ratio_ref, pairs.get(i)) {
            (Some(r), Some(ps)) if i != r && !ps.is_empty() => {
                Some(pair_ratio(ps).unwrap_or_else(|e| fail_closed(e)))
            }
            _ => None,
        };
        println!(
            "{}",
            render_summary_line(group, label, rows, scale, summary, ratio)
        );
    }
}

// --- 述語経路の段別計測（Issue #1319。SQL-24・SQL-2 ポインタ） ---
//
// engine の `pub(crate)` 関数は直接呼ばず、pub API（`VectorArena::build_filtered_with_rows`・
// `row_codec::scan_scalar_columns_masked`・`declarative_filter::matches_all`・
// `ParallelSearchProvider::{search, search_subset}`）で段をベンチ内に再実装する
// （`scan_stage_profile_bench.rs` の I 系列・W 系列と同じ方式。production 無変更のため既定ビルドの
// 挙動・性能は構造的に不変）。段本体は `#[inline(never)]` に分離する（Issue #682 の前例）。
//
// 系列は 2 つ: `current`（現 HEAD が通る索引 trusted-mask 経路）と `plain_scan_ref`
// （#1275 時点の PlainScan 経路の反実仮想。F＝全件の SCALAR 評価・c＝一致行の複製の分離用。
// 現 HEAD の述語 arm はどれもこの経路を通らない）。

/// 段計測のラウンド内 warmup／計測回数（段は短いので p95 ではなくプール済み分位点で見る）。
const STAGE_WARMUP: u32 = 20;
const STAGE_MEASURED: u32 = 20;

/// 段別計測が共有する、計測外で構築した可視行スナップショットと索引相当の辞書。
struct StageFixture<'a> {
    schema: &'a TableSchema,
    /// 可視行（スロット順）の metadata バイト列と行 id。
    metadata: Vec<Vec<u8>>,
    ids: Vec<u64>,
    /// 借用して探索に使うスナップショット arena。
    arena: &'a VectorArena,
    /// `lang` 値 → 値索引（`ScalarIndex` の `column.equality` 相当）。
    lang_dict: std::collections::HashMap<String, usize>,
    /// 値索引 → 候補スロット（昇順。`slots_for_value_index` 相当）。
    lang_slots: Vec<Vec<u32>>,
    /// `lang` 列だけ true の列マスク（`scan_scalar_columns_masked` 用）。
    lang_mask: Vec<bool>,
    query: &'a [f32],
}

impl<'a> StageFixture<'a> {
    fn new(
        schema: &'a TableSchema,
        arena: &'a VectorArena,
        ids: Vec<u64>,
        metadata: Vec<Vec<u8>>,
        query: &'a [f32],
    ) -> Self {
        let lang_col = schema
            .columns
            .iter()
            .position(|c| c.name == "lang")
            .unwrap_or_else(|| fail_closed("stage fixture: lang column not found"));
        let lang_mask: Vec<bool> = (0..schema.columns.len()).map(|i| i == lang_col).collect();
        let mut lang_dict: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut lang_slots: Vec<Vec<u32>> = Vec::new();
        for (slot, meta) in metadata.iter().enumerate() {
            let scanned = scan_scalar_columns_masked(schema, meta, Some(&lang_mask))
                .unwrap_or_else(|e| fail_closed(format!("stage fixture: scan: {e}")));
            let Some(text) = scanned
                .get(lang_col)
                .and_then(|v| v.as_ref())
                .and_then(|v| v.as_text())
            else {
                fail_closed("stage fixture: lang cell is not text");
            };
            let slot = u32::try_from(slot)
                .unwrap_or_else(|_| fail_closed("stage fixture: slot exceeds u32"));
            let next = lang_slots.len();
            let vi = *lang_dict.entry(text.to_string()).or_insert(next);
            if vi == next {
                lang_slots.push(Vec::new());
            }
            lang_slots
                .get_mut(vi)
                .unwrap_or_else(|| fail_closed("stage fixture: value index out of range"))
                .push(slot);
        }
        Self {
            schema,
            metadata,
            ids,
            arena,
            lang_dict,
            lang_slots,
            lang_mask,
            query,
        }
    }
}

/// PlainScan 反実仮想が複製する、一致行の owned バッファ（embedding・id・tenant_id・visibility）。
struct CopiedRows {
    ids: Vec<u64>,
    embeddings: Vec<f32>,
    tenants: Vec<String>,
    visibilities: Vec<Visibility>,
}

impl CopiedRows {
    /// 複製した総バイト数（tenant 文字列は本体長のみ）。
    fn bytes(&self) -> usize {
        self.embeddings.len() * std::mem::size_of::<f32>()
            + self.ids.len() * std::mem::size_of::<u64>()
            + self.tenants.iter().map(String::len).sum::<usize>()
            + self.visibilities.len() * std::mem::size_of::<Visibility>()
    }
}

/// arm の述語を `lang` 列へ束縛したフィルタ列。評価は「いずれかが一致」（OR 分岐の any）。
/// IN 形は 1 本の `in_list`、等価は 1 本、OR 形は分岐ごとの等価。
fn plain_filters(arm: &str, schema: &TableSchema) -> Vec<MetadataFilter> {
    let values = arm_branch_values(arm).unwrap_or_else(|e| fail_closed(e));
    let form = arm_pred_form(arm).unwrap_or_else(|e| fail_closed(e));
    let declared: Vec<DeclarativeFilter> = match form {
        PredForm::InList => vec![DeclarativeFilter::in_list(
            "lang",
            values.iter().map(|v| v.to_string()).collect(),
        )],
        PredForm::Equality | PredForm::OrBranches => values
            .iter()
            .map(|v| DeclarativeFilter::equals("lang", *v))
            .collect(),
    };
    declared
        .iter()
        .map(|f| {
            f.bind(schema)
                .unwrap_or_else(|e| fail_closed(format!("bind stage filter: {e}")))
        })
        .collect()
}

#[inline(always)]
fn row_matches(fx: &StageFixture<'_>, meta: &[u8], filters: &[MetadataFilter]) -> bool {
    let scanned = scan_scalar_columns_masked(fx.schema, meta, Some(&fx.lang_mask))
        .unwrap_or_else(|e| fail_closed(format!("stage scan: {e}")));
    filters
        .iter()
        .any(|f| matches_all(std::slice::from_ref(f), &scanned))
}

/// 索引経路の候補解決。実経路（`ScalarIndex::candidates_for`）の手順に合わせ、値 → 値索引 →
/// 候補スロットの順で引く。単一値（`pred_eq`・重複除去後に 1 値の `pred_or_same`）は
/// スロット列の複製のみで整列・重複除去しない（`FilterOp::Equals` 腕と同じ）。複数値
/// （IN・OR 形）は値ごとのスロットを連結し、呼び出し元と同じ後処理（整列＋重複除去）を行う
/// （`FilterOp::InText` 腕＋`sql::exec` の後処理相当。OR 形の実経路は分岐ごとの候補和集合で
/// あり、ここでは IN 形と同じ連結＋整列で近似する）。
#[inline(never)]
fn stage_idx_candidate_resolve(fx: &StageFixture<'_>, values: &[&str]) -> Vec<u32> {
    let slots_of = |v: &str| {
        fx.lang_dict
            .get(v)
            .and_then(|&vi| fx.lang_slots.get(vi))
            .map(|s| s.as_slice())
    };
    if let [single] = values {
        return slots_of(single).map(|s| s.to_vec()).unwrap_or_default();
    }
    let mut out: Vec<u32> = Vec::new();
    for v in values {
        if let Some(slots) = slots_of(v) {
            out.extend_from_slice(slots);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// 索引経路の距離計算・順位付け（借用した arena 上の `search_subset`。複製なし）。
#[inline(never)]
fn stage_idx_search_subset(fx: &StageFixture<'_>, slots: &[u32]) -> Vec<CandidateHit> {
    ParallelSearchProvider
        .search_subset(SubsetSearchInput {
            slots,
            vectors: fx.arena.vectors(),
            dim: DIM as u32,
            query: fx.query,
            k: PRED_LIMIT,
        })
        .unwrap_or_else(|e| fail_closed(format!("search_subset: {e}")))
}

/// PlainScan 反実仮想の F: 可視行全件の SCALAR 評価（`lang` 列だけデコード）。戻り値は一致件数。
#[inline(never)]
fn stage_plain_scalar_eval(fx: &StageFixture<'_>, filters: &[MetadataFilter]) -> u64 {
    let mut matched = 0u64;
    for meta in &fx.metadata {
        if row_matches(fx, meta, filters) {
            matched += 1;
        }
    }
    matched
}

/// PlainScan 反実仮想の F＋c: F と同じ評価に加え、一致行を owned バッファへ複製する
/// （`Vec::new()` から amortized 成長。embedding だけでなく id・tenant_id・visibility も複製）。
#[inline(never)]
fn stage_plain_copy(fx: &StageFixture<'_>, filters: &[MetadataFilter]) -> CopiedRows {
    let mut out = CopiedRows {
        ids: Vec::new(),
        embeddings: Vec::new(),
        tenants: Vec::new(),
        visibilities: Vec::new(),
    };
    let vectors = fx.arena.vectors();
    for (slot, meta) in fx.metadata.iter().enumerate() {
        if !row_matches(fx, meta, filters) {
            continue;
        }
        let (Some(emb), Some(id)) = (vectors.get(slot * DIM..(slot + 1) * DIM), fx.ids.get(slot))
        else {
            fail_closed("stage copy: slot out of range");
        };
        out.embeddings.extend_from_slice(emb);
        out.ids.push(*id);
        out.tenants.push(TENANT_A.to_string());
        out.visibilities.push(Visibility::Public);
    }
    out
}

/// PlainScan 反実仮想の距離計算・順位付け（複製済みバッファへの `search`）。
#[inline(never)]
fn stage_plain_search(fx: &StageFixture<'_>, copied: &CopiedRows) -> Vec<CandidateHit> {
    ParallelSearchProvider
        .search(SearchInput {
            ids: &copied.ids,
            vectors: &copied.embeddings,
            dim: DIM as u32,
            query: fx.query,
            k: PRED_LIMIT,
        })
        .unwrap_or_else(|e| fail_closed(format!("search: {e}")))
}

/// 段のプール済みサンプル 1 件分（arm × 系列 × 段）。
struct StageAcc {
    arm: &'static str,
    path: StagePath,
    stage: &'static str,
    samples: Vec<Duration>,
    round_medians: Vec<Duration>,
    counts: String,
}

fn stage_acc<'a>(
    accs: &'a mut Vec<StageAcc>,
    arm: &'static str,
    path: StagePath,
    stage: &'static str,
) -> &'a mut StageAcc {
    let pos = accs
        .iter()
        .position(|a| a.arm == arm && a.path == path && a.stage == stage);
    let idx = pos.unwrap_or_else(|| {
        accs.push(StageAcc {
            arm,
            path,
            stage,
            samples: Vec::new(),
            round_medians: Vec::new(),
            counts: String::new(),
        });
        accs.len() - 1
    });
    match accs.get_mut(idx) {
        Some(a) => a,
        None => fail_closed("stage accumulator index out of range"),
    }
}

/// arm ごとの段計測の事前計算（計測外）。
struct ArmStagePlan {
    label: &'static str,
    sql: String,
    values: Vec<&'static str>,
    filters: Vec<MetadataFilter>,
    expected_ids: Vec<u64>,
    ranked: Vec<(u64, f64)>,
}

/// スロット列を行 id 列へ写す。範囲外は fail-closed。
fn slot_ids(fx: &StageFixture<'_>, slots: impl Iterator<Item = u64>) -> Vec<u64> {
    slots
        .map(|s| {
            usize::try_from(s)
                .ok()
                .and_then(|i| fx.ids.get(i).copied())
                .unwrap_or_else(|| fail_closed("stage verify: slot out of range"))
        })
        .collect()
}

/// 述語グループの段別内訳を測って出力する。毎ラウンド、計測前に各段の出力を fixture 規則から
/// 独立に導いた期待（一致 id 集合・Top-k）と照合し、違反は値を出さず非 0 終了する（fail-closed）。
/// 出力は全検査・全計測の完了後にまとめて行う。
fn measure_predicate_stages(
    rounds: u32,
    fx: &StageFixture<'_>,
    own: u64,
    sentinel: &[f32],
    arms: &[(&'static str, String)],
    core: &EngineCore,
    tenant_ctx: &PolicyContext,
) {
    let config = MeasurementConfig::new(STAGE_WARMUP, STAGE_MEASURED, 1)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));
    let plans: Vec<ArmStagePlan> = arms
        .iter()
        .map(|(label, sql)| ArmStagePlan {
            label,
            sql: sql.clone(),
            values: arm_lang_values(label).unwrap_or_else(|e| fail_closed(e)),
            filters: plain_filters(label, fx.schema),
            expected_ids: expected_match_ids(label, own).unwrap_or_else(|e| fail_closed(e)),
            ranked: expected_predicate_ranked(label, own, 0, sentinel, fx.query),
        })
        .collect();
    let mut accs: Vec<StageAcc> = Vec::new();
    for round in 0..rounds as usize {
        for idx in rotate_arms(round, plans.len()) {
            let Some(plan) = plans.get(idx) else { continue };
            let arm = plan.label;
            // 検証（計測前）: 索引系列。
            let slots = stage_idx_candidate_resolve(fx, &plan.values);
            let mut cand_ids = slot_ids(fx, slots.iter().map(|s| u64::from(*s)));
            cand_ids.sort_unstable();
            if cand_ids != plan.expected_ids {
                fail_closed(format!(
                    "{arm}: index candidates diverged from expected ids"
                ));
            }
            let hits = stage_idx_search_subset(fx, &slots);
            let hit_ids = slot_ids(fx, hits.iter().map(|h| h.id));
            if !topk_matches(&hit_ids, &plan.ranked, PRED_LIMIT) {
                fail_closed(format!(
                    "{arm}: search_subset result is not the expected top rows"
                ));
            }
            // 検証（計測前）: PlainScan 反実仮想系列。
            let matched = stage_plain_scalar_eval(fx, &plan.filters);
            let copied = stage_plain_copy(fx, &plan.filters);
            let mut copied_ids = copied.ids.clone();
            copied_ids.sort_unstable();
            if matched != plan.expected_ids.len() as u64 || copied_ids != plan.expected_ids {
                fail_closed(format!(
                    "{arm}: plain scan matches diverged from expected ids"
                ));
            }
            let plain_hits = stage_plain_search(fx, &copied);
            let plain_ids: Vec<u64> = plain_hits.iter().map(|h| h.id).collect();
            if !topk_matches(&plain_ids, &plan.ranked, PRED_LIMIT) {
                fail_closed(format!(
                    "{arm}: plain search result is not the expected top rows"
                ));
            }
            // 計測。戻り値の解放は計測区間の外へ出す（`run_bounded_retain` の retain 0）。
            let cand =
                run_bounded_retain(&config, 0, || stage_idx_candidate_resolve(fx, &plan.values))
                    .map(|r| r.0);
            let subset =
                run_bounded_retain(&config, 0, || stage_idx_search_subset(fx, &slots)).map(|r| r.0);
            // e2e は各反復の結果を全件保持（`STAGE_MEASURED` 件・Top-k 行のみで小さい）し、
            // 計測区間の外で期待 Top-k と照合する（誤った結果を返す経路の時間を採らない）。
            let e2e = run_bounded_retain(&config, STAGE_MEASURED as usize, || {
                let mut session = SessionState::default();
                match core.execute_sql_in_session(tenant_ctx, &mut session, &plan.sql) {
                    Ok(SqlOutcome::Query(result)) => result,
                    Ok(_) => fail_closed(format!("{arm}: execute: not a query result")),
                    Err(e) => fail_closed(format!("{arm}: execute: {e}")),
                }
            })
            .unwrap_or_else(|e| fail_closed(format!("{arm}: protocol violation: {e}")));
            if e2e.1.len() != STAGE_MEASURED as usize {
                fail_closed(format!("{arm}: e2e results were not retained"));
            }
            for result in &e2e.1 {
                let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
                if !topk_matches(&ids, &plan.ranked, PRED_LIMIT) {
                    fail_closed(format!("{arm}: e2e result is not the expected top rows"));
                }
            }
            let e2e = Ok(e2e.0);
            let eval =
                run_bounded_retain(&config, 0, || stage_plain_scalar_eval(fx, &plan.filters))
                    .map(|r| r.0);
            let copy =
                run_bounded_retain(&config, 0, || stage_plain_copy(fx, &plan.filters)).map(|r| r.0);
            let search =
                run_bounded_retain(&config, 0, || stage_plain_search(fx, &copied)).map(|r| r.0);
            let k = PRED_LIMIT;
            let rows_n = fx.metadata.len();
            let series: [(StagePath, &'static str, _, String); 6] = [
                (
                    StagePath::Current,
                    STAGE_IDX_CANDIDATE_RESOLVE,
                    cand,
                    format!("candidates={} values={}", slots.len(), plan.values.len()),
                ),
                (
                    StagePath::Current,
                    STAGE_IDX_SEARCH_SUBSET,
                    subset,
                    format!("scanned_rows={} k={k}", slots.len()),
                ),
                (StagePath::Current, STAGE_E2E, e2e, format!("k={k}")),
                (
                    StagePath::PlainScanRef,
                    STAGE_PLAIN_SCALAR_EVAL,
                    eval,
                    format!("evaluated_rows={rows_n} matched={matched}"),
                ),
                (
                    StagePath::PlainScanRef,
                    STAGE_PLAIN_COPY,
                    copy,
                    format!("matched={matched} copied_bytes={}", copied.bytes()),
                ),
                (
                    StagePath::PlainScanRef,
                    STAGE_PLAIN_SEARCH,
                    search,
                    format!("rows={} k={k}", copied.ids.len()),
                ),
            ];
            for (path, stage, result, counts) in series {
                let m = result
                    .unwrap_or_else(|e| fail_closed(format!("{arm}: protocol violation: {e}")));
                let acc = stage_acc(&mut accs, arm, path, stage);
                acc.samples.extend_from_slice(&m.samples);
                // 段サマリと同じ nearest-rank 定義の中央値（`m.summary.median` は線形補間のため使わない）。
                let round_median = quantile_nearest_rank(&m.samples, 0.5)
                    .unwrap_or_else(|e| fail_closed(format!("{arm}: round median: {e}")));
                acc.round_medians.push(round_median);
                acc.counts = counts;
            }
        }
    }
    // 出力（全検査完了後）。
    for acc in &accs {
        for (i, median) in acc.round_medians.iter().enumerate() {
            println!(
                "{}",
                render_stage_round_line(acc.arm, acc.path, acc.stage, i + 1, *median, &acc.counts)
            );
        }
    }
    let summaries: Vec<(&StageAcc, StageSummary)> = accs
        .iter()
        .map(|a| {
            (
                a,
                summarize_stage(&a.samples).unwrap_or_else(|e| fail_closed(e)),
            )
        })
        .collect();
    for (acc, summary) in &summaries {
        println!(
            "{}",
            render_stage_summary_line(acc.arm, acc.path, acc.stage, summary, &acc.counts)
        );
    }
    let median_of = |arm: &str, path: StagePath, stage: &str| -> Option<Duration> {
        summaries
            .iter()
            .find(|(a, _)| a.arm == arm && a.path == path && a.stage == stage)
            .map(|(_, s)| s.median)
    };
    for plan in &plans {
        let arm = plan.label;
        let cur = StagePath::Current;
        let plain = StagePath::PlainScanRef;
        let residual = match (
            median_of(arm, cur, STAGE_E2E),
            median_of(arm, cur, STAGE_IDX_CANDIDATE_RESOLVE),
            median_of(arm, cur, STAGE_IDX_SEARCH_SUBSET),
        ) {
            (Some(e), Some(a), Some(b)) => checked_residual(e, a, b),
            _ => None,
        };
        println!(
            "{}",
            render_stage_diff_line(arm, cur, "residual_median", residual)
        );
        let c = match (
            median_of(arm, plain, STAGE_PLAIN_COPY),
            median_of(arm, plain, STAGE_PLAIN_SCALAR_EVAL),
        ) {
            (Some(fc), Some(f)) => checked_stage_diff(fc, f),
            _ => None,
        };
        println!("{}", render_stage_diff_line(arm, plain, "c_median", c));
    }
}

fn run_docs_groups(group: Group, rows: usize, rounds: u32) {
    let (storage, _guard) = open_storage("docs");
    let schema = docs_schema();
    storage
        .create_table(&schema)
        .unwrap_or_else(|e| fail_closed(format!("create table: {e}")));
    let make = |id: u64| {
        vec![
            Value::Vector(rng_vector_for(id)),
            Value::Text(lang_for_id(id).to_string()),
            Value::BigInt(qty_for_id(id)),
        ]
    };
    let own = rows as u64;
    let other = other_tenant_rows(rows) as u64;
    let query = DeterministicRng::new(2).next_vector(DIM);
    // 他テナント行は各 arm の上位（`l0`・最小／最大 `qty`）に並ぶ sentinel とする。順位は正規化しない
    // 内積で決まるため、ベクトルは「クエリ方向 × 係数」とし、内積が自テナント fixture 全行の最大内積を
    // 必ず上回るようにする（生成時に assert）。RLS が外れれば計測クエリ自身の LIMIT 内へ越境行が
    // 必ず入る fixture になる。`qty` は fixture の範囲外（偶数 id が最小、奇数 id が最大）で、これも生成時に
    // assert する。他テナント文脈の事前検査は、sentinel が実際に上位へ現れることを実測で確認する。
    let max_fixture_dot = (0..own)
        .map(|id| -inner_product_distance(&rng_vector_for(id), &query))
        .fold(f64::NEG_INFINITY, f64::max);
    let query_norm_sq = -inner_product_distance(&query, &query);
    let sentinel_vector: Vec<f32> = {
        let factor = sentinel_scale(max_fixture_dot, query_norm_sq);
        query.iter().map(|x| x * factor).collect()
    };
    assert!(
        sentinel_dominates(
            -inner_product_distance(&sentinel_vector, &query),
            max_fixture_dot
        ),
        "sentinel inner product must exceed every fixture row"
    );
    assert!(
        (0..own).all(|id| qty_for_id(id) > sentinel_qty(0) && qty_for_id(id) < sentinel_qty(1)),
        "sentinel qty must lie outside the fixture qty range"
    );
    let make_other = |id: u64| {
        vec![
            Value::Vector(sentinel_vector.clone()),
            Value::Text(lang_token(0).to_string()),
            Value::BigInt(sentinel_qty(id)),
        ]
    };
    seed_rows(
        &storage,
        &schema,
        TENANT_A,
        Visibility::Public,
        0..own,
        &make,
    );
    seed_rows(
        &storage,
        &schema,
        TENANT_B,
        Visibility::Private,
        own..own + other,
        &make_other,
    );

    let ctx_a =
        PolicyContext::new(TENANT_A).unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")));
    // 述語グループの段別計測（Issue #1319）用に、対象テナントの可視行スナップショットを
    // `EngineCore::from_storage` が storage を消費する前に捕捉する（RLS を通る pub API 経由。
    // 他テナントの Private sentinel 行が含まれないことを件数で検査し、違反は fail-closed）。
    let mut stage_ids: Vec<u64> = Vec::new();
    let mut stage_metadata: Vec<Vec<u8>> = Vec::new();
    let stage_arena = if group.includes(Group::Predicate) {
        let arena = VectorArena::build_filtered_with_rows(
            &storage,
            DOCS,
            |t, v| ctx_a.is_visible(t, v),
            |_slot, id, _embedding, metadata| {
                stage_ids.push(id);
                stage_metadata.push(metadata.to_vec());
                Ok(true)
            },
        )
        .unwrap_or_else(|e| fail_closed(format!("stage snapshot: {e}")));
        if arena.len() as u64 != own || stage_ids.len() != arena.len() {
            fail_closed("stage snapshot: visible row count does not match the own-tenant rows");
        }
        Some(arena)
    } else {
        None
    };

    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let ctx_b = ctx(TENANT_B);
    let literal =
        vector_literal(&query).unwrap_or_else(|e| fail_closed(format!("vector literal: {e}")));

    if group.includes(Group::Predicate) {
        let arms = predicate_statements(DOCS, &literal).unwrap_or_else(|e| fail_closed(e));
        for (label, sql) in &arms {
            // 計測する文（`arms`）そのものを、対象テナント文脈と他テナント文脈の双方で fixture 由来の
            // 期待上位行と照合する（他テナント文脈は sentinel が上位に現れること＝越境が見える fixture）。
            let ranked = expected_predicate_ranked(label, own, 0, &sentinel_vector, &query);
            precheck_predicate(label, &exec(&core, &ctx_a, sql), (own, own), &ranked);
            let ranked_b = expected_predicate_ranked(label, own, other, &sentinel_vector, &query);
            let result_b = exec(&core, &ctx_b, sql);
            precheck_predicate(label, &result_b, (own, own + other), &ranked_b);
            // 越境すれば必ず検出できる fixture であることの実測: 他テナント文脈の上位 `PRED_LIMIT` 件は
            // 全件が sentinel（対象テナントの行は 1 件も入らない）。
            if result_b.rows.len() != PRED_LIMIT.min(other as usize)
                || result_b.rows.iter().any(|r| r.id < own)
            {
                fail_closed(format!(
                    "{label}: sentinel rows do not dominate the top rows"
                ));
            }
        }
        // 経路の自己検査（Issue #1275）。全 arm を 1 回ずつ実行してキャッシュを温めてから測る。
        for (_, sql) in &arms {
            let _ = exec(&core, &ctx_a, sql);
        }
        for (label, sql) in &arms {
            check_predicate_path(&core, &ctx_a, label, sql);
        }
        measure_group(
            "predicate",
            (rows, scale_label(rows)),
            rounds,
            &core,
            &ctx_a,
            &arms,
            true,
        );
        // 既存の p95 行の後に別ループで段別内訳を測る（p95 行の意味・値を変えない）。
        if let Some(arena) = stage_arena.as_ref() {
            let fx = StageFixture::new(&schema, arena, stage_ids, stage_metadata, &query);
            measure_predicate_stages(rounds, &fx, own, &sentinel_vector, &arms, &core, &ctx_a);
        }
    }
    if group.includes(Group::OrderBy) {
        let arms = order_by_statements(DOCS).unwrap_or_else(|e| fail_closed(e));
        let visible_a = visible_doc_rows(own, 0);
        let visible_b = visible_doc_rows(own, other);
        for (label, sql) in &arms {
            precheck_order(label, &exec(&core, &ctx_a, sql), &visible_a, None);
            precheck_order(label, &exec(&core, &ctx_b, sql), &visible_b, Some(own));
        }
        measure_group(
            "order_by",
            (rows, scale_label(rows)),
            rounds,
            &core,
            &ctx_a,
            &arms,
            false,
        );
    }
}

/// id から決定的にベクトルを作る（行ごとに `DeterministicRng` を seed する）。
fn rng_vector_for(id: u64) -> Vec<f32> {
    DeterministicRng::new(id.wrapping_add(1_000)).next_vector(DIM)
}

/// 結合文を `LIMIT`／`OFFSET` のページ（各 `JOIN_PAGE_ROWS` 行）で全件取得し、行 id を返す。
/// 行順が未規定だとページ間で重複・取りこぼしが起きうるため、一意キー（`title` は `t{id}` で行ごとに一意）の
/// `ORDER BY` を付けてページ境界を固定する（結合のスカラー `ORDER BY` は Issue #1190 で対応済み）。
/// `LIMIT`・`OFFSET` とも `MAX_SEARCH_K` 以内に収めるための分割で、末尾ページが満杯のまま
/// 取得上限に達した場合は全件取得できていないため fail-closed する。
fn exec_join_pages(core: &EngineCore, tenant_ctx: &PolicyContext, head: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for page in 0..JOIN_PAGES {
        let sql = format!(
            "{head}ORDER BY {DOCUMENTS}.title LIMIT {JOIN_PAGE_ROWS} OFFSET {}",
            page * JOIN_PAGE_ROWS
        );
        let result = exec(core, tenant_ctx, &sql);
        let len = result.rows.len();
        ids.extend(result.rows.iter().map(|r| r.id));
        if len < JOIN_PAGE_ROWS {
            return ids;
        }
    }
    fail_closed("join_inner: full join exceeds the paging capacity")
}

/// 結合 arm の事前検査（計測する `LIMIT` 付きの文そのものの結果）: 件数が `min(WIDE_LIMIT, 期待集合の大きさ)`・
/// id の重複なし・全 id が期待集合（`expected_ids`・昇順）に属する・各行のタイトルと作者名が結合キーどおり
/// （`cells_of(id)`）、をすべて満たすこと。`ORDER BY` が無く先頭 `LIMIT` 行の選択は未規定なので、
/// どの行が返るかではなく「返る行がすべて期待集合の正しい結合結果であること」を見る。
/// `ORDER BY` が無いため越境行が先頭 `LIMIT` 内に入る保証は作れない。越境の検出力は、同じ結合を
/// `LIMIT`／`OFFSET` ページで全件取得して期待 id 集合と過不足なく照合する検査（`run_join_group`）が担う。
fn precheck_join(
    result: &QueryResult,
    expected_ids: &[u64],
    cells_of: &dyn Fn(u64) -> (String, String),
) {
    let want = WIDE_LIMIT.min(expected_ids.len());
    if result.rows.len() != want || want == 0 {
        fail_closed("join_inner: unexpected row count for the measured statement");
    }
    let mut seen = std::collections::HashSet::new();
    for row in &result.rows {
        if expected_ids.binary_search(&row.id).is_err() {
            fail_closed("join_inner: row outside the expected join result");
        }
        if !seen.insert(row.id) {
            fail_closed("join_inner: duplicate row ids");
        }
        let (title, name) = cells_of(row.id);
        if cell_text(row.cells.first()) != title || cell_text(row.cells.get(1)) != name {
            fail_closed("join_inner: join key mismatch");
        }
    }
}

fn run_join_group(rows: usize, rounds: u32) {
    let join_rows = rows.min(JOIN_ROWS) as u64;
    let (storage, _guard) = open_storage("join");
    let documents = documents_schema();
    let authors = authors_schema();
    for schema in [&documents, &authors] {
        storage
            .create_table(schema)
            .unwrap_or_else(|e| fail_closed(format!("create table: {e}")));
    }
    let other = other_tenant_rows(join_rows as usize) as u64;
    let is_probe = |id: u64| id % JOIN_PROBE_MOD == JOIN_PROBE_MOD - 1;
    let make_doc = |id: u64| {
        // probe 行は他テナントの作者行（id `join_rows..`）を指す。右辺 RLS が効けば内部結合で脱落する。
        let author = if is_probe(id) {
            (join_rows + id % other.max(1)) as i64
        } else {
            author_id_for_doc(id, join_rows)
        };
        vec![
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(format!("t{id}")),
            Value::BigInt(author),
        ]
    };
    // 他テナント文書は他テナントの作者行（id `join_rows..`）へ結合させ、対照クエリで結合が成立するようにする。
    let make_other_doc = |id: u64| {
        vec![
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(format!("t{id}")),
            Value::BigInt((join_rows + (id - join_rows) % other.max(1)) as i64),
        ]
    };
    let make_author = |id: u64| {
        vec![
            Value::Vector(vec![id as f32, 1.0]),
            Value::Text(format!("a{id}")),
        ]
    };
    seed_rows(
        &storage,
        &documents,
        TENANT_A,
        Visibility::Public,
        0..join_rows,
        &make_doc,
    );
    seed_rows(
        &storage,
        &documents,
        TENANT_B,
        Visibility::Private,
        join_rows..join_rows + other,
        &make_other_doc,
    );
    seed_rows(
        &storage,
        &authors,
        TENANT_A,
        Visibility::Public,
        0..join_rows,
        &make_author,
    );
    seed_rows(
        &storage,
        &authors,
        TENANT_B,
        Visibility::Private,
        join_rows..join_rows + other,
        &make_author,
    );

    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let ctx_a =
        PolicyContext::new(TENANT_A).unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")));
    let arm = join_statement(DOCUMENTS, AUTHORS).unwrap_or_else(|e| fail_closed(e));
    // 計測する文（`arm.1`）そのものを、対象テナント文脈と他テナント文脈の双方で照合する。
    // 対象テナント: 結果は自テナントの結合可能文書のみ（probe 行は右辺 RLS で脱落）。
    // 他テナント（両 visibility 可視）: 自行・他テナント行とも結合可能で全 id が候補になる。
    let expected_a: Vec<u64> = (0..join_rows).filter(|id| !is_probe(*id)).collect();
    let expected_b: Vec<u64> = (0..join_rows + other).collect();
    let cells_of = |id: u64| -> (String, String) {
        let author = if id >= join_rows {
            (join_rows + (id - join_rows) % other.max(1)) as i64
        } else if is_probe(id) {
            (join_rows + id % other.max(1)) as i64
        } else {
            author_id_for_doc(id, join_rows)
        };
        (format!("t{id}"), format!("a{author}"))
    };
    let ctx_b = ctx(TENANT_B);
    precheck_join(&exec(&core, &ctx_a, &arm.1), &expected_a, &cells_of);
    precheck_join(&exec(&core, &ctx_b, &arm.1), &expected_b, &cells_of);
    // 対照検査: 結合結果を `MAX_SEARCH_K` 以内の `LIMIT`／`OFFSET` ページで全件取得し、対象テナントの
    // 結合結果が自テナント文書の全件（過不足なし）であることを確認する。RLS が外れれば他テナント
    // 文書が加わり件数・id 範囲が崩れる。他テナント文脈では自行と Public な他テナント行の双方が見える。
    let head = arm
        .1
        .strip_suffix(&format!("LIMIT {WIDE_LIMIT}"))
        .unwrap_or_else(|| fail_closed("join_inner: unexpected statement shape"));
    let full = exec_join_pages(&core, &ctx_a, head);
    let own_joinable = (0..join_rows).filter(|id| !is_probe(*id)).count() as u64;
    if own_joinable >= join_rows {
        fail_closed("join_inner: fixture has no cross-tenant probe rows (scale too small)");
    }
    // 件数だけでは重複と欠落が同時に起きても通過するため、期待する文書 id の集合と過不足なく照合する。
    if !is_exact_id_set(&full, &expected_a) {
        fail_closed("join_inner: full join is not exactly the own-tenant joinable documents");
    }
    let full_b = exec_join_pages(&core, &ctx_b, head);
    if !is_exact_id_set(&full_b, &expected_b) {
        fail_closed("join_inner: control query did not see both tenants' rows");
    }
    measure_group(
        "join",
        (join_rows as usize, join_scale_label(join_rows as usize)),
        rounds,
        &core,
        &ctx_a,
        &[arm],
        false,
    );
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }
    let rounds = parse_rounds(std::env::var("BENCH_RELATIONAL_P95_ROUNDS").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let group = parse_group(std::env::var("BENCH_RELATIONAL_P95_GROUP").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let rows = parse_rows_scale(std::env::var("BENCH_RELATIONAL_P95_ROWS").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let dedicated = std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "relational_p95_bench: group={} rounds={rounds} rows={rows} dedicated_env_attested={dedicated} \
         (informational only; see docs/design/relational-p95-bench.md)",
        group.label()
    );

    if group.includes(Group::Predicate) || group.includes(Group::OrderBy) {
        run_docs_groups(group, rows, rounds);
    }
    if group.includes(Group::Join) {
        run_join_group(rows, rounds);
    }
    // 実行した全グループが規定の行数のときだけ「閾値との比較」を案内する（縮小規模は専有環境でも対象外）。
    let docs_full = !(group.includes(Group::Predicate) || group.includes(Group::OrderBy))
        || scale_label(rows) == "full";
    let join_full = !group.includes(Group::Join) || join_scale_label(rows.min(JOIN_ROWS)) == "full";
    println!(
        "{}",
        render_threshold_line(dedicated, docs_full && join_full)
    );
}
