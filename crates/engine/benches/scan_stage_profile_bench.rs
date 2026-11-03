//! `docs/design/crossdb-bench.md` で self が最劣後する 3 フェーズ（`agg_count`・
//! `rls_isolation`・`vector_knn_where`）の段別内訳プロファイル（Issue #464。
//! 親 Issue #456・ルート Issue #455）。
//!
//! production コード（`crates/engine/src/`）は無変更のテスト・ベンチ・docs 専任
//! タスク。段の定義・測定設計・fail-closed 整合性検証は
//! `benches/harness/scan_stage_profile.rs` 冒頭コメント・
//! `docs/design/scan-stage-profile.md` を正本とする。
//!
//! # コーパス（crossdb モデルの再現）
//!
//! tenant-a `TENANT_A_ROWS`（既定 23,000）行 `Visibility::Public`・tenant-b
//! `TENANT_B_ROWS`（既定 2,000）行 `Visibility::Private`（`BENCH_SCAN_PROFILE_SCALE`
//! で両方とも倍率適用）。`PolicyContext::new(tenant)`（`Visibility::Public` のみ
//! 許可）で `agg_count`（ctx=tenant-a）・`rls_isolation`（ctx=tenant-b）を測ると、
//! いずれも可視集合はテナント横断の Public 行（tenant-a の全行）のみになり、
//! 同一走査・同一 `COUNT(*)` を返す（`docs/design/crossdb-bench.md` の実測
//! 3,547µs/3,552µs の近さと整合）。`lang` 列の割当（`harness::scan_stage_profile::
//! lang_for_id`）は `BENCH_SCAN_PROFILE_SELECTIVITY=1/<N>`（既定 `1/5`・`ja` ≒ 20%）
//! で選択率を変えられる（Issue #653。crossdb fixture の実選択率 33% で計測する
//! 場合は `1/3` を指定する）。既定値は導入前の `LANGS[(id) % 5]` 輪番とビット同一。
//!
//! # SCALAR 事前フィルタ付き DISTANCE の索引経路内訳（Issue #653）
//!
//! `sql/exec.rs::execute_statement_with_cache` は `WHERE lang = '<value>'` を
//! `ScalarIndex::resolve_candidates`（Issue #474）による候補削減へ結線する。
//! 本ベンチは同一クエリを 2 アーム（[`ProfileArm::Index`]／[`ProfileArm::Plain`]。
//! 後者は `AND vec_norm(embedding) > 0` を付け加え `classify_scalar_plan` を `PlainScan` へ意図的に
//! 縮退させた、Issue #474 以前相当の形状）で e2e 計測し、`scalar_index_cache_stats()`
//! の増分で索引経路の発火有無を非 vacuous に確認する（索引アームは
//! `index_scans` が正に増分し `plain_scan_fallbacks` は不変、plain アームは
//! `classify_scalar_plan` が `PlainScan` を返す形状のため索引を消費する分岐
//! そのものへ入らず両カウンタとも不変のまま——`FallbackNoIndex`／
//! `FallbackSelectivity`〔索引対応述語だが lookup/resolve に失敗〕とは異なる
//! ことに注意）。さらに索引経路の内訳を I1〜I3（[`ProfileArm::Index`]
//! のみ）として計測する:
//!
//! | 段 | 内容 |
//! | --- | --- |
//! | I1 `index_candidate_resolve` | 値→候補スロット辞書からの `\"ja\"` lookup ＋ 複製（`ScalarIndex::candidates_for` 相当の pub API 再実装） |
//! | I2a `candidate_predicate` | I1 の候補のみへ `scan_scalar_columns` ＋ `lang = 'ja'` 判定（`build_from_cached_rls_rows_subset` の再適用契約に対応） |
//! | I2b `candidate_mask_build` | I2a と同じ述語適用で「一致した元スロット番号」のマスク（`Vec<u32>`）だけを構築する（`filter_cached_rls_rows_subset` 相当。#654 適用後の index アームは複製を経由しないため embedding／id／tenant_id／visibility のいずれも複製しない） |
//! | I3 `provider_search` | I2b のマスクを複製せず借用したままの `arena.vectors()` へ渡す `SearchProvider::search_subset`（距離計算＋Top-k。#654 相当） |
//!
//! SQL 表層固定コストは `e2e(index) − (I1 + I2b + I3)` の残差として算出する
//! （逆転時は測定ノイズとして `n/a` 表示）。I2b・I3 は #654 適用後の index
//! アームが実際に通る `filter_cached_rls_rows_subset` → `search_subset`
//! （複製なし・借用のみ）の処理を再実装したものであり、#654 以前相当の
//! 複製経路（`build_from_cached_rls_rows_subset`）は測定しない（codex-review
//! P1 指摘・PR #663。旧版は e2e(index) が実際には経由しない複製コストを
//! I2b/I3 へ混入させており、残差の算出契約と実装経路が不整合だった）。
//! 詳細・実測結果は `docs/design/filtered-distance-stage-profile.md` を参照。
//!
//! # 出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線
//! しない。`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する。
//! `make bench-scan-stage-profile`（Makefile）から実行する。

#[allow(dead_code)]
mod harness;

use harness::bench_engine;
use harness::env_report::EnvReport;
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::scan_stage_profile::{
    assert_scan_row_counts_match, bucket_share_pct, build_lang_filter, classify_against_bands,
    decode_dim_and_metadata_reimpl, expected_visible_hits, is_visible, lang_for_id,
    matches_lang_filter, median_of, min_of, ns_per_row, parse_rounds, parse_scale,
    parse_selectivity, reference_band, refuse_under_github_actions, render_arm_stats_line,
    render_bucket_line, render_bucket_share_line, render_diff_line, render_stage_line,
    scan_scalar_columns, stage_diff_ns_per_row, step_ratio_pct, verify_row_key_tenant_reimpl,
    where_clause_for_arm, ProfileArm,
};
use harness::stats;

use std::hint::black_box;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::declarative_filter::MetadataFilter;
use engine::hnsw::{HnswParams, Ratio, ValidatedHnswParams};
use engine::kernel::{SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine::{self, SearchEngineKind};
use engine::sql::exec::Cell;
use engine::storage::{RowInput, Storage, Visibility};
use engine::{arena::VectorArena, tenant};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 128;
const TENANT_A: &str = "tenant-a";
const TENANT_A_ROWS_BASE: u64 = 23_000;
const TENANT_B: &str = "tenant-b";
const TENANT_B_ROWS_BASE: u64 = 2_000;
const TOP_K: usize = 10;
const TABLE: &str = "docs";
const SEED_BATCH_ROWS: u64 = 5_000;
/// SQL `WHERE lang = '<TARGET_LANG>'` の対象値（[`harness::scan_stage_profile::
/// lang_for_id`] の `TARGET_LANG` と同じ値。名前の重複を避けるため本ファイル側は
/// 独立した定数として保持する）。
const TARGET_LANG: &str = "ja";

/// 生 `redb::Database` 再オープン時に走査する行テーブル（`knn_profile_bench.rs::
/// ROW_TABLE` と同一の再実装。ドリフト検出は `tests/scan_stage_profile_accept.rs`
/// が `Storage::scan()` との突き合わせで行う）。
const ROW_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("user_rows/docs");

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("scan_stage_profile_bench: {msg}");
    std::process::exit(1);
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// A1〜A5 の 1 ラウンド分測定（戻り値: 中央値 5 本・可視行数）。
struct RoundResultA {
    a1: Duration,
    a2: Duration,
    a3: Duration,
    a4: Duration,
    a5: Duration,
    visible_rows: usize,
}

/// A1（生 `redb` per-entry 走査のみ）の計測対象本体。`#[inline(never)]` により
/// 呼び出し元（`measure_a_series` のラウンドループ）へのインライン化を止め、
/// ループ本体のコード配置がラウンドループ側の変更（例: 前後の A0c ブロック追加）
/// に引きずられて動くことを防ぐ（Issue #635。配置アーティファクトを完全に
/// 無くす保証はなく、Step 2 の摂動テストで効果を確認したうえで採用している）。
#[inline(never)]
fn stage_a1_scan(db: &Database) -> (usize, u64) {
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROW_TABLE).expect("open row table");
    let mut rows = 0usize;
    let mut checksum: u64 = 0;
    for entry in table.iter().expect("iter row table") {
        let _entry = entry.expect("iterate row entry");
        checksum = checksum.wrapping_add(std::hint::black_box(1u64));
        rows += 1;
    }
    (rows, checksum)
}

/// A2（A1 ＋ ヘッダデコード）の計測対象本体。分離理由は `stage_a1_scan` 参照。
#[inline(never)]
fn stage_a2_header_decode(db: &Database) -> (usize, u64) {
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROW_TABLE).expect("open row table");
    let mut rows = 0usize;
    let mut checksum: u64 = 0;
    for entry in table.iter().expect("iter row table") {
        let (_k, v) = entry.expect("iterate row entry");
        let (tenant_id, is_public, _offset) = harness::knn_profile::decode_header_reimpl(v.value())
            .expect("header decode must succeed for well-formed synthetic rows");
        checksum = checksum.wrapping_add(std::hint::black_box(tenant_id.len() as u64));
        checksum = checksum.wrapping_add(std::hint::black_box(is_public as u64));
        rows += 1;
    }
    (rows, checksum)
}

/// A3（A2 ＋ RLS 判定 ＋ TABLE-12 キー/ヘッダ tenant 整合検査）の計測対象本体。
/// 不可視行は以降の追加処理を行わない（production と同じ「不可視行は
/// dim/metadata を一切デコードしない」順序）。分離理由は `stage_a1_scan` 参照。
#[inline(never)]
fn stage_a3_rls_visible(db: &Database, ctx: &PolicyContext) -> (usize, usize, u64) {
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROW_TABLE).expect("open row table");
    let mut rows = 0usize;
    let mut visible = 0usize;
    let mut checksum: u64 = 0;
    for entry in table.iter().expect("iter row table") {
        let (k, v) = entry.expect("iterate row entry");
        let (tenant_id, is_public, _offset) = harness::knn_profile::decode_header_reimpl(v.value())
            .expect("header decode must succeed for well-formed synthetic rows");
        let visibility = if is_public {
            Visibility::Public
        } else {
            Visibility::Private
        };
        if is_visible(ctx, tenant_id, visibility) {
            let (key_tenant, _id) = k.value();
            verify_row_key_tenant_reimpl(key_tenant, tenant_id)
                .expect("row key tenant must match header tenant for well-formed rows");
            checksum = checksum.wrapping_add(std::hint::black_box(1u64));
            visible += 1;
        }
        rows += 1;
    }
    (rows, visible, checksum)
}

/// A3 の可視行数のみを求める対照カウント（計測区間の外・`run()` を通さない）。
/// `measure_a_series` が `RoundResultA::visible_rows` を求めるために使う。
#[inline(never)]
fn stage_a3_visible_count(db: &Database, ctx: &PolicyContext) -> usize {
    let read_txn = db.begin_read().expect("begin read txn for visible count");
    let table = read_txn.open_table(ROW_TABLE).expect("open row table");
    let mut visible = 0usize;
    for entry in table.iter().expect("iter row table") {
        let (k, v) = entry.expect("iterate row entry");
        let (tenant_id, is_public, _offset) =
            harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
        let visibility = if is_public {
            Visibility::Public
        } else {
            Visibility::Private
        };
        if is_visible(ctx, tenant_id, visibility) {
            let (key_tenant, _id) = k.value();
            verify_row_key_tenant_reimpl(key_tenant, tenant_id).expect("tenant match");
            visible += 1;
        }
    }
    visible
}

/// A4（A3 ＋ dim・metadata 借用デコード。可視行のみ）の計測対象本体。
/// 累積段契約（A3 ⊆ A4 ⊆ A5）を保つため `verify_row_key_tenant_reimpl` は
/// ここでも省略しない（省略すると A4−A3 の差分がデコード追加コストではなく
/// 整合性検査コスト分だけ過小に出てしまい、後続の性能改善の帰属を誤る）。
/// 分離理由は `stage_a1_scan` 参照。
#[inline(never)]
fn stage_a4_dim_meta_decode(db: &Database, ctx: &PolicyContext) -> (usize, u64) {
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROW_TABLE).expect("open row table");
    let mut rows = 0usize;
    let mut checksum: u64 = 0;
    for entry in table.iter().expect("iter row table") {
        let (k, v) = entry.expect("iterate row entry");
        let (tenant_id, is_public, _offset) =
            harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
        let visibility = if is_public {
            Visibility::Public
        } else {
            Visibility::Private
        };
        if is_visible(ctx, tenant_id, visibility) {
            let (key_tenant, _id) = k.value();
            verify_row_key_tenant_reimpl(key_tenant, tenant_id)
                .expect("row key tenant must match header tenant for well-formed rows");
            let (dim, metadata) = decode_dim_and_metadata_reimpl(v.value())
                .expect("dim/metadata decode must succeed for well-formed synthetic rows");
            checksum = checksum.wrapping_add(std::hint::black_box(dim as u64));
            checksum = checksum.wrapping_add(std::hint::black_box(metadata.len() as u64));
        }
        rows += 1;
    }
    (rows, checksum)
}

/// A5（A4 ＋ `row_codec::validate_scalar_columns`。可視行のみ）の計測対象本体。
/// A4 と同じ理由で `verify_row_key_tenant_reimpl` を省略しない。分離理由は
/// `stage_a1_scan` 参照。
#[inline(never)]
fn stage_a5_scalar_validate(
    db: &Database,
    ctx: &PolicyContext,
    schema: &TableSchema,
) -> (usize, u64) {
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROW_TABLE).expect("open row table");
    let mut rows = 0usize;
    let mut checksum: u64 = 0;
    for entry in table.iter().expect("iter row table") {
        let (k, v) = entry.expect("iterate row entry");
        let (tenant_id, is_public, _offset) =
            harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
        let visibility = if is_public {
            Visibility::Public
        } else {
            Visibility::Private
        };
        if is_visible(ctx, tenant_id, visibility) {
            let (key_tenant, _id) = k.value();
            verify_row_key_tenant_reimpl(key_tenant, tenant_id)
                .expect("row key tenant must match header tenant for well-formed rows");
            let (dim, metadata) =
                decode_dim_and_metadata_reimpl(v.value()).expect("dim/metadata decode");
            engine::row_codec::validate_scalar_columns(schema, metadata)
                .expect("scalar column structure must be valid for well-formed synthetic rows");
            checksum = checksum.wrapping_add(std::hint::black_box(dim as u64));
        }
        rows += 1;
    }
    (rows, checksum)
}

/// W1（可視行 metadata への `scan_scalar_columns`）の計測対象本体。
/// W 系列は `773a835..997cf00` 区間でベンチ `main()` が大幅増量したにも
/// かかわらず `scan_scalar_columns`／`declarative_filter::matches_all`
/// 自体はバイト単位無変更のまま W2 が +17% 悪化して見えた
/// （バイナリ配置アーティファクト疑い）ため、A 系列（`stage_a1_scan` 参照）と
/// 同型の `#[inline(never)]` 独立関数へ抽出した（Issue #682）。
#[inline(never)]
fn stage_w1_scalar_scan(schema: &TableSchema, captured_metadata: &[Vec<u8>]) -> u64 {
    let mut checksum = 0u64;
    for metadata in captured_metadata {
        let scanned = scan_scalar_columns(schema, metadata).expect("scan_scalar_columns");
        checksum = checksum.wrapping_add(std::hint::black_box(scanned.len() as u64));
    }
    checksum
}

/// W2（W1 ＋ `lang = 'ja'` 判定〔`matches_lang_filter`〕）の計測対象本体。
/// 戻り値は一致件数（ラウンドごとの整合性検証にも同じ値を再利用する）。
/// 分離理由は `stage_w1_scalar_scan` 参照。
#[inline(never)]
fn stage_w2_predicate(
    schema: &TableSchema,
    captured_metadata: &[Vec<u8>],
    filters: &[MetadataFilter],
) -> u64 {
    let mut matched = 0u64;
    for metadata in captured_metadata {
        let scanned = scan_scalar_columns(schema, metadata).expect("scan_scalar_columns");
        if matches_lang_filter(filters, &scanned) {
            matched = matched.wrapping_add(std::hint::black_box(1u64));
        }
    }
    matched
}

fn measure_a_series(
    db: &Database,
    config: &MeasurementConfig,
    ctx: &PolicyContext,
    schema: &TableSchema,
) -> RoundResultA {
    // A1: per-entry 走査のみ（ヘッダを読まない）。計測対象本体は
    // `stage_a1_scan`（`#[inline(never)]`。Issue #635）。
    let a1 = run(config, || stage_a1_scan(db)).expect("measurement must satisfy protocol minimums");

    // A2: A1 ＋ ヘッダデコード（tenant_id・visibility）。計測対象本体は
    // `stage_a2_header_decode`。
    let a2 = run(config, || stage_a2_header_decode(db))
        .expect("measurement must satisfy protocol minimums");

    // A3: A2 ＋ RLS 判定（`PolicyContext::is_visible`）＋ TABLE-12 キー/ヘッダ
    // tenant 整合検査。不可視行は以降の追加処理を行わない（production と同じ
    // 「不可視行は dim/metadata を一切デコードしない」順序）。計測対象本体は
    // `stage_a3_rls_visible`。
    let a3 = run(config, || stage_a3_rls_visible(db, ctx))
        .expect("measurement must satisfy protocol minimums");
    // 可視行数は計測区間の外（`stage_a3_visible_count`）で別途求める。
    let a3_visible = stage_a3_visible_count(db, ctx);

    // A4: A3（RLS 判定＋キー/ヘッダ tenant 整合検査）＋ dim・metadata 借用デコード
    // （可視行のみ）。累積段契約（A3 ⊆ A4 ⊆ A5）を保つため、A3 が行う
    // `verify_row_key_tenant_reimpl` はここでも省略せず実行する（省略すると
    // A4−A3 の差分がデコード追加コストではなく整合性検査コスト分だけ過小に
    // 出てしまい、後続の性能改善の帰属を誤る）。計測対象本体は
    // `stage_a4_dim_meta_decode`。
    let a4 = run(config, || stage_a4_dim_meta_decode(db, ctx))
        .expect("measurement must satisfy protocol minimums");

    // A5: A4（キー/ヘッダ tenant 整合検査を含む）＋
    // `row_codec::validate_scalar_columns`（可視行のみ）。A4 と同じ理由で
    // `verify_row_key_tenant_reimpl` を省略しない。計測対象本体は
    // `stage_a5_scalar_validate`。
    let a5 = run(config, || stage_a5_scalar_validate(db, ctx, schema))
        .expect("measurement must satisfy protocol minimums");

    RoundResultA {
        a1: a1.summary.median,
        a2: a2.summary.median,
        a3: a3.summary.median,
        a4: a4.summary.median,
        a5: a5.summary.median,
        visible_rows: a3_visible,
    }
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    let rounds_raw = match bench_engine::read_env_var("BENCH_SCAN_PROFILE_ROUNDS") {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_ROUNDS: {e}")),
    };
    let rounds = match parse_rounds(rounds_raw.as_deref()) {
        Ok(r) => r,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_ROUNDS: {e}")),
    };
    let scale_raw = match bench_engine::read_env_var("BENCH_SCAN_PROFILE_SCALE") {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_SCALE: {e}")),
    };
    let scale = match parse_scale(scale_raw.as_deref()) {
        Ok(s) => s,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_SCALE: {e}")),
    };
    let selectivity_raw = match bench_engine::read_env_var("BENCH_SCAN_PROFILE_SELECTIVITY") {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_SELECTIVITY: {e}")),
    };
    let selectivity_denominator = match parse_selectivity(selectivity_raw.as_deref()) {
        Ok(d) => d,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_SELECTIVITY: {e}")),
    };
    // HNSW opt-in（Issue #677）: 既定 `BruteForce`（未設定）のときは本節以下の
    // 新規コードパスを一切通らず、出力・処理は #677 導入前とビット同一のまま
    // 不変（`bench_engine::parse_engine`／`parse_full_scan_ratio` は
    // `knn_profile_bench.rs` と同じ fail-closed 語彙・判定方針を共有する）。
    let scan_engine_raw = match bench_engine::read_env_var("BENCH_SCAN_PROFILE_ENGINE") {
        Ok(v) => v,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_ENGINE: {e}")),
    };
    let scan_engine = match bench_engine::parse_engine(scan_engine_raw.as_deref()) {
        Ok(e) => e,
        Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_ENGINE: {e}")),
    };
    let scan_full_scan_ratio_raw =
        match bench_engine::read_env_var("BENCH_SCAN_PROFILE_FULL_SCAN_RATIO") {
            Ok(v) => v,
            Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_FULL_SCAN_RATIO: {e}")),
        };
    let scan_full_scan_ratio_override =
        match bench_engine::parse_full_scan_ratio(scan_full_scan_ratio_raw.as_deref()) {
            Ok(r) => r,
            Err(e) => fail_closed(format!("BENCH_SCAN_PROFILE_FULL_SCAN_RATIO: {e}")),
        };
    if scan_full_scan_ratio_override.is_some()
        && scan_engine == harness::bench_engine::BenchEngine::BruteForce
    {
        fail_closed(
            "BENCH_SCAN_PROFILE_FULL_SCAN_RATIO requires BENCH_SCAN_PROFILE_ENGINE=hnsw|hnsw_f16|hnsw_i8 (got brute_force)",
        );
    }

    let tenant_a_rows = TENANT_A_ROWS_BASE * scale;
    let tenant_b_rows = TENANT_B_ROWS_BASE * scale;
    let total_physical_rows = (tenant_a_rows + tenant_b_rows) as usize;

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "scan_stage_profile_bench: rows={total_physical_rows} dim={DIM} rounds={rounds} scale={scale} selectivity=1/{selectivity_denominator} (tenant_a={tenant_a_rows} public, tenant_b={tenant_b_rows} private)"
    );
    // Issue #677 追記行（既存の summarizer 正規表現は上記行のみを見るため
    // 後方互換）。`BENCH_SCAN_PROFILE_ENGINE` 未設定（既定 `brute_force`）時は
    // この行自体を出力しないことで、既定出力（本関数の println 列全体）を
    // #677 導入前とビット同一のまま維持する（codex-review P2 指摘・PR #688:
    // 無条件 println! だと `scan_default_brute.log` のように既定実行でも
    // 新規行が増え、「既定出力はビット同一」という互換性説明と矛盾していた）。
    if scan_engine != harness::bench_engine::BenchEngine::BruteForce {
        println!(
            "scan_stage_profile_bench: engine={} full_scan_ratio={}",
            scan_engine.token(),
            match scan_full_scan_ratio_override {
                Some((n, d)) => format!("{n}/{d}"),
                None => "default".to_string(),
            }
        );
    }

    let schema = schema();
    let path = unique_db_path("issue464-scan-stage-profile");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    storage
        .create_table(&schema)
        .expect("create table for bench seeding");

    let mut rng = DeterministicRng::new(1);
    let mut next_id: u64 = 0;
    for (tenant_id, count, visibility) in [
        (TENANT_A, tenant_a_rows, Visibility::Public),
        (TENANT_B, tenant_b_rows, Visibility::Private),
    ] {
        let ctx =
            PolicyContext::with_visibilities(tenant_id, [Visibility::Public, Visibility::Private])
                .expect("valid tenant id");
        let mut remaining = count;
        while remaining > 0 {
            let batch_len = SEED_BATCH_ROWS.min(remaining) as usize;
            let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
            let mut batch_metadata: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
            for i in 0..batch_len {
                let id = next_id + i as u64;
                let v = rng.next_vector(DIM);
                let lang = lang_for_id(id, selectivity_denominator);
                let metadata = encode_scalar_columns(
                    &schema,
                    &[Value::Vector(v.clone()), Value::Text(lang.to_string())],
                )
                .expect("encode_scalar_columns for bench seeding");
                batch_vectors.push(v);
                batch_metadata.push(metadata);
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
                            metadata: &batch_metadata[i],
                        },
                    )
                })
                .collect();
            let op_id = OperationId::parse(&format!("seed-{tenant_id}-{next_id}"))
                .expect("valid operation_id");
            tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id).expect("seed batch insert");
            next_id += batch_len as u64;
            remaining -= batch_len as u64;
        }
    }
    if next_id as usize != total_physical_rows {
        fail_closed(format!(
            "seeded row count mismatch: expected {total_physical_rows}, got {next_id}"
        ));
    }

    let ctx_a = PolicyContext::new(TENANT_A).expect("valid tenant id");
    let ctx_b = PolicyContext::new(TENANT_B).expect("valid tenant id");
    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let query = rng.next_vector(DIM);

    // --- W 系列の事前準備: 可視行（ctx_a）の metadata・embedding を計測外で捕捉する。
    // production の `on_visible_row` フックと同じ順序（RLS 段を通過した行だけへ到達）
    // を pub API（`VectorArena::build_filtered_with_rows`）で再現する。
    let mut captured_metadata: Vec<Vec<u8>> = Vec::new();
    let mut captured_ids: Vec<u64> = Vec::new();
    let arena = VectorArena::build_filtered_with_rows(
        &storage,
        TABLE,
        |tenant, visibility| ctx_a.is_visible(tenant, visibility),
        |_slot, id, _embedding, metadata| {
            captured_ids.push(id);
            captured_metadata.push(metadata.to_vec());
            Ok(true)
        },
    )
    .expect("arena build must succeed for well-formed synthetic corpus");
    if arena.len() != tenant_a_rows as usize {
        fail_closed(format!(
            "visible arena row count mismatch: expected {tenant_a_rows}, got {}",
            arena.len()
        ));
    }
    if captured_metadata.len() != arena.len() {
        fail_closed("captured metadata count does not match arena row count");
    }

    // 計測外で `lang = 'ja'` の期待一致件数を求める（W2 の整合性検証に使う）。
    // `expected_match_ids` は W1/W2 で実際に測定するコード経路（`scan_scalar_columns`
    // ＋`matches_lang_filter`）を一切通さず、シード時に確定している「id → lang」の
    // 対応規則（`LANGS[(id as usize) % LANGS.len()]`。上記シードループ参照）から
    // 直接導出する。`scan_scalar_columns`／`matches_lang_filter` 自体に退行が
    // 混入した場合でも、同じロジックで期待値を作ってしまうと W2 の整合性検証が
    // 常に一致してしまい検出できない（P2 指摘・codex-review）ため、検証対象の
    // ロジックから独立した根拠が必要。
    let filters = vec![build_lang_filter(&schema, "lang", TARGET_LANG)
        .expect("build_lang_filter must succeed for well-formed schema")];
    let expected_match_ids: Vec<u64> = captured_ids
        .iter()
        .copied()
        .filter(|id| lang_for_id(*id, selectivity_denominator) == TARGET_LANG)
        .collect();
    if expected_match_ids.is_empty() {
        fail_closed("no rows matched lang = 'ja' in the synthetic corpus (fixture misconfigured)");
    }
    // `expected_visible_hits`（harness の独立導出関数）と件数が一致することを
    // 固定する（`parse_selectivity`／`lang_for_id` の呼び出し規約が本ファイルと
    // harness 側で食い違っていないかのドリフト検出）。
    if expected_visible_hits(&captured_ids, selectivity_denominator) != expected_match_ids.len() {
        fail_closed(
            "expected_visible_hits(harness) diverged from expected_match_ids(bench-local derivation)",
        );
    }

    // --- ラウンド輪番: A1〜A5・W1〜W4・R_dot を rounds 回ずつ測定する。--------------
    let mut a1_rounds = Vec::with_capacity(rounds as usize);
    let mut a2_rounds = Vec::with_capacity(rounds as usize);
    let mut a3_rounds = Vec::with_capacity(rounds as usize);
    let mut a4_rounds = Vec::with_capacity(rounds as usize);
    let mut a5_rounds = Vec::with_capacity(rounds as usize);
    let mut w1_rounds = Vec::with_capacity(rounds as usize);
    let mut w2_rounds = Vec::with_capacity(rounds as usize);
    let mut w3_rounds = Vec::with_capacity(rounds as usize);
    let mut w4_rounds = Vec::with_capacity(rounds as usize);
    let mut r_dot_rounds = Vec::with_capacity(rounds as usize);
    let mut a3_visible_counts: Vec<usize> = Vec::new();

    let db = {
        // `storage`（既存の書き込みハンドル）を保持したまま生 redb を同時に開くと
        // `DatabaseAlreadyOpen` になるため（`knn_profile_bench.rs` と同じ理由）、
        // ここでは drop せず読み取り専用の別ハンドルとして再利用可能な形にする
        // ため、A 系列測定の直前で明示的に `drop(storage)` してから開く。
        drop(storage);
        Database::open(&path).expect("reopen raw database for A series")
    };

    let parallel_provider = ParallelSearchProvider;

    for round in 0..rounds {
        let a = measure_a_series(&db, &config, &ctx_a, &schema);
        if a.visible_rows != tenant_a_rows as usize {
            fail_closed(format!(
                "round {round}: A3 visible row count mismatch: expected {tenant_a_rows}, got {}",
                a.visible_rows
            ));
        }
        a3_visible_counts.push(a.visible_rows);
        a1_rounds.push(a.a1);
        a2_rounds.push(a.a2);
        a3_rounds.push(a.a3);
        a4_rounds.push(a.a4);
        a5_rounds.push(a.a5);

        // rls_isolation（ctx_b）が agg_count（ctx_a）と同一走査であることを、
        // 毎ラウンド A3 の可視行数一致で確認する（COUNT(*) 値そのものは別途
        // e2e 検証する）。
        let b_visible = {
            let read_txn = db.begin_read().expect("begin read txn");
            let table = read_txn.open_table(ROW_TABLE).expect("open row table");
            let mut visible = 0usize;
            for entry in table.iter().expect("iter row table") {
                let (_k, v) = entry.expect("iterate row entry");
                let (tenant_id, is_public, _offset) =
                    harness::knn_profile::decode_header_reimpl(v.value()).expect("header decode");
                let visibility = if is_public {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                if is_visible(&ctx_b, tenant_id, visibility) {
                    visible += 1;
                }
            }
            visible
        };
        if b_visible != a.visible_rows {
            fail_closed(format!(
                "round {round}: rls_isolation(ctx_b) visible row count {b_visible} does not match agg_count(ctx_a) {}",
                a.visible_rows
            ));
        }

        // W1: 可視行 metadata へ `scan_scalar_columns`（`#[inline(never)]`。Issue #682）。
        let w1 = run(&config, || {
            stage_w1_scalar_scan(&schema, &captured_metadata)
        })
        .expect("measurement must satisfy protocol minimums");
        w1_rounds.push(w1.summary.median);

        // W2: W1 ＋ `lang = 'ja'` 判定（`#[inline(never)]`。Issue #682）。
        let w2 = run(&config, || {
            stage_w2_predicate(&schema, &captured_metadata, &filters)
        })
        .expect("measurement must satisfy protocol minimums");
        w2_rounds.push(w2.summary.median);
        // 整合性検証は計測対象本体（`stage_w2_predicate`）の戻り値をそのまま
        // 再利用する（同一ロジックの重複走査を排除。検証の弱体化ではない——
        // `expected_match_ids` 側は `lang_for_id` 由来の独立導出のまま不変）。
        let w2_match_count = stage_w2_predicate(&schema, &captured_metadata, &filters) as usize;
        if w2_match_count != expected_match_ids.len() {
            fail_closed(format!(
                "round {round}: W2 lang='ja' match count mismatch: expected {}, got {w2_match_count}",
                expected_match_ids.len()
            ));
        }

        // W3: W2 一致行 embedding を連続 `Vec<f32>` へ複製する。
        let dim = arena.dim() as usize;
        let w3 = run(&config, || {
            let mut copied: Vec<f32> = Vec::with_capacity(expected_match_ids.len() * dim);
            for (idx, metadata) in captured_metadata.iter().enumerate() {
                let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
                if matches_lang_filter(&filters, &scanned) {
                    let vector = arena.vector(idx).expect("arena vector for captured index");
                    copied.extend_from_slice(vector);
                }
            }
            copied
        })
        .expect("measurement must satisfy protocol minimums");
        w3_rounds.push(w3.summary.median);

        // W4: 一致行のみへ Top-k 探索。
        let mut matched_ids: Vec<u64> = Vec::with_capacity(expected_match_ids.len());
        let mut matched_vectors: Vec<f32> = Vec::with_capacity(expected_match_ids.len() * dim);
        for (idx, metadata) in captured_metadata.iter().enumerate() {
            let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
            if matches_lang_filter(&filters, &scanned) {
                matched_ids.push(captured_ids[idx]);
                matched_vectors.extend_from_slice(arena.vector(idx).expect("arena vector"));
            }
        }
        if matched_ids != expected_match_ids {
            fail_closed(format!(
                "round {round}: W3/W4 matched id set diverged from independently derived expected_match_ids (predicate regression suspected)"
            ));
        }
        let w4 = run(&config, || {
            parallel_provider
                .search(SearchInput {
                    ids: &matched_ids,
                    vectors: &matched_vectors,
                    dim: dim as u32,
                    query: &query,
                    k: TOP_K,
                })
                .expect("search must succeed for well-formed synthetic input")
        })
        .expect("measurement must satisfy protocol minimums");
        w4_rounds.push(w4.summary.median);

        // R_dot: 参照区間（変更を含まない区間）。全可視行への逐次内積総和。
        let r_dot = run(&config, || {
            let mut acc = 0.0f32;
            for chunk in arena.vectors().chunks_exact(dim) {
                acc += engine::isa::current().dot(chunk, &query);
            }
            acc
        })
        .expect("measurement must satisfy protocol minimums");
        r_dot_rounds.push(r_dot.summary.median);
    }

    if let Err(e) = assert_scan_row_counts_match(&[
        ("A3_visible_round0", a3_visible_counts[0]),
        (
            "A3_visible_last",
            *a3_visible_counts.last().expect("at least one round"),
        ),
    ]) {
        fail_closed(e);
    }

    // `db`（生 `redb::Database` ハンドル・A 系列専用）を drop してファイルロックを
    // 解放する（以降の e2e 測定は `Storage::open`／`EngineCore` 経由の pub API の
    // みで DB へアクセスする。`knn_profile_bench.rs` と同じ理由で、生ハンドルと
    // 書き込み可能ハンドルの同時生存は redb の `DatabaseAlreadyOpen` を招く）。
    drop(db);

    // --- e2e（EngineCore::execute_sql）: W0c/W0h/W0n・COUNT(*)（A0a/A0b）。--------
    let sql_where = format!(
        "SELECT id FROM {TABLE} {} ORDER BY embedding <=> '{}' LIMIT {TOP_K}",
        where_clause_for_arm(ProfileArm::Index, "lang", TARGET_LANG),
        vector_literal(&query)
    );
    // plain アーム（Issue #653）: `AND vec_norm(embedding) > 0` により `classify_scalar_plan` を
    // `PlainScan` へ縮退させ、Issue #474 以前相当の全可視行走査経路を測る。
    let sql_where_plain = format!(
        "SELECT id FROM {TABLE} {} ORDER BY embedding <=> '{}' LIMIT {TOP_K}",
        where_clause_for_arm(ProfileArm::Plain, "lang", TARGET_LANG),
        vector_literal(&query)
    );
    let sql_nowhere = format!(
        "SELECT id FROM {TABLE} ORDER BY embedding <=> '{}' LIMIT {TOP_K}",
        vector_literal(&query)
    );
    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");

    // W0c: 毎サンプル新規 `EngineCore` から測る（`SqlArenaCache` を空の状態から）。
    for _ in 0..config.warmup_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for W0-cold warmup");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        black_box(
            cold_core
                .execute_sql(&ctx_a, &sql_where)
                .expect("execute_sql must succeed for well-formed synthetic WHERE query"),
        );
    }
    let mut w0_cold_samples: Vec<Duration> =
        Vec::with_capacity(config.measured_iterations() as usize);
    let mut w0_cold_last_ids: Option<Vec<u64>> = None;
    for _ in 0..config.measured_iterations() {
        // 計測区間は `execute_sql` のみ（`Storage::open`／`EngineCore` 構築は
        // 含まない）。`docs/design/scan-stage-profile.md`「W0-cold − W0-hot で
        // `SqlArenaCache` の寄与を示す」という既存の解釈は、W0-hot（同一
        // `EngineCore` を使い回す `execute_sql` のみの区間）との差分が
        // `SqlArenaCache` のヒット/ミスにのみ帰属することを前提にしている。
        // ここに `Storage::open`／`EngineCore` 構築コストを含めると DB 起動
        // コストが混入し前提が崩れるため、この区間には含めない（PR #586
        // codex-review・Cursor Bugbot 指摘。Issue #479 で新設した `A0c-cold`
        // は `VisibleBitmapCache` のミス経路を測る別目的の測定点であり、
        // `Storage::open` を含めて測る設計は `A0c-cold` 側だけの事情。
        // W0-cold をそれに合わせて変更する必要はない）。
        let cold_storage = Storage::open(&path).expect("reopen storage for W0-cold measurement");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        let start = Instant::now();
        let result = black_box(
            cold_core
                .execute_sql(&ctx_a, &sql_where)
                .expect("execute_sql must succeed for well-formed synthetic WHERE query"),
        );
        w0_cold_samples.push(start.elapsed());
        w0_cold_last_ids = Some(
            result
                .rows
                .iter()
                .map(|row| match row.cells.first() {
                    Some(Cell::Integer(v)) => *v,
                    other => fail_closed(format!("W0-cold id cell type mismatch: got {other:?}")),
                })
                .collect(),
        );
    }
    if w0_cold_samples.is_empty() {
        fail_closed("W0-cold measurement produced no samples");
    }
    let w0_cold_summary = stats::summarize(&w0_cold_samples).expect("W0-cold summarize");
    let w0_cold_ids = w0_cold_last_ids.expect("W0-cold produced at least one sample");
    if w0_cold_ids.len() != TOP_K {
        fail_closed(format!(
            "W0-cold result row count mismatch: expected {TOP_K}, got {}",
            w0_cold_ids.len()
        ));
    }
    if w0_cold_ids
        .iter()
        .any(|id| !expected_match_ids.contains(id))
    {
        fail_closed("W0-cold returned an id outside the lang='ja' visible set (tenant/filter leak suspected)");
    }

    // A0c（Issue #479）: 毎サンプル新規 `Storage::open` ＋ `EngineCore`
    // （空の `VisibleBitmapCache`〔Issue #478〕）から `COUNT(*)` を測る。
    // 後段の A0a／A0b は同一 `EngineCore` を使い回すため 2 回目以降は必ず
    // 本キャッシュのヒット経路を測る（`sql/visible_cache.rs::execute_aggregate_with_cache`
    // が `user_rows/{table}` を一切開かない経路）。A0c はそのミス経路（走査に
    // 相乗りしたスナップショット構築を含む）を、before（キャッシュ非搭載）と
    // after（本キャッシュ搭載）の交互実測で比較できるようにするための対照値
    // （before では常に全行走査、after では構築コストを含むミス経路）。
    // `Storage::open`／`EngineCore` 構築コストを計測区間に含めるのは A0c
    // 自身の設計判断であり、W0-cold（`execute_sql` のみを計測。上記コメント
    // 参照）とは意図的に異なる区間を採る（PR #586 codex-review・Cursor
    // Bugbot 指摘。`docs/design/visible-bitmap-cache-verification.md` 参照）。
    // W0c の生 DB ハンドルは既に drop 済みだが、W0-hot/A0a/A0b 用の
    // `core`（同一 DB を開いたまま保持する）はまだ開いていないため、ここで
    // `Storage::open` の二重オープン（`DatabaseAlreadyOpen`）を避けられる。
    for _ in 0..config.warmup_iterations() {
        let cold_storage = Storage::open(&path).expect("reopen storage for A0-cold warmup");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        black_box(
            cold_core
                .execute_sql(&ctx_a, &count_sql)
                .expect("execute_sql must succeed for COUNT(*) query"),
        );
    }
    let mut a0c_samples: Vec<Duration> = Vec::with_capacity(config.measured_iterations() as usize);
    let mut a0c_last_value: Option<u64> = None;
    for _ in 0..config.measured_iterations() {
        // 計測区間は `Storage::open` を含む（本節冒頭のコメント・
        // `docs/design/visible-bitmap-cache-verification.md` の前提と一致させる）。
        let start = Instant::now();
        let cold_storage = Storage::open(&path).expect("reopen storage for A0-cold measurement");
        let cold_core = EngineCore::from_storage(cold_storage, search_engine::default_engine());
        let result = black_box(
            cold_core
                .execute_sql(&ctx_a, &count_sql)
                .expect("execute_sql must succeed for COUNT(*) query"),
        );
        a0c_samples.push(start.elapsed());
        if result.rows.len() != 1 {
            fail_closed(format!(
                "A0-cold COUNT(*) row count mismatch: expected 1, got {}",
                result.rows.len()
            ));
        }
        a0c_last_value = Some(match result.rows[0].cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => fail_closed(format!(
                "A0-cold COUNT(*) cell type mismatch: got {other:?}"
            )),
        });
    }
    if a0c_samples.is_empty() {
        fail_closed("A0-cold measurement produced no samples");
    }
    let a0c_summary = stats::summarize(&a0c_samples).expect("A0-cold summarize");
    match a0c_last_value {
        Some(value) if value == tenant_a_rows => {}
        Some(value) => fail_closed(format!(
            "A0-cold COUNT(*) value mismatch: expected {tenant_a_rows}, got {value}"
        )),
        None => fail_closed("A0-cold measurement produced no COUNT(*) value"),
    }

    let storage = Storage::open(&path).expect("reopen storage for W0-hot/W0-nowhere/A0");
    let core = EngineCore::from_storage(storage, search_engine::default_engine());

    // --- 索引アーム／plain アーム／I 系列（Issue #653）: ラウンド輪番。--------------
    // A/W 系列と同じプロトコル（ラウンドごとに独立した `run()` 呼び出しを行い、
    // ラウンド横断で min-of-R／median-of-R・per-round 生データを得る）へ揃える
    // （codex-review P1 指摘・PR #663。旧実装はここを一度だけ計測しており、
    // per-round 生データ・min-of-R が得られず `benchmark-judgement-policy.md`
    // §3 の交互 N≥5 ペア・生データ必須の規約に反していた）。
    let lang_col_index = schema
        .columns
        .iter()
        .position(|c| c.name == "lang")
        .expect("lang column exists in schema");
    let dim = arena.dim() as usize;

    // 値→候補スロット辞書（`ScalarIndex::build` 相当の pub API 再実装）は
    // `captured_metadata`／`arena` がラウンド間で不変のため、ループ外で 1 回だけ
    // 構築する（I1 の計測対象はここからの lookup ＋ 複製＋整列のみ）。
    let mut lang_candidates: std::collections::HashMap<&str, Vec<u32>> =
        std::collections::HashMap::new();
    for (idx, metadata) in captured_metadata.iter().enumerate() {
        let scanned = scan_scalar_columns(&schema, metadata).expect("scan_scalar_columns");
        if let Some(Some(value)) = scanned
            .get(lang_col_index)
            .map(|v| v.and_then(|v| v.as_text()))
        {
            let slot = u32::try_from(idx).expect("captured row index fits in u32");
            lang_candidates.entry(value).or_default().push(slot);
        }
    }
    let expected_candidate_count = lang_candidates
        .get(TARGET_LANG)
        .map(|v| v.len())
        .unwrap_or(0);
    if expected_candidate_count != expected_match_ids.len() {
        fail_closed(format!(
            "I1 candidate dictionary diverged from expected_match_ids: expected {}, got {}",
            expected_match_ids.len(),
            expected_candidate_count
        ));
    }

    let mut index_rounds: Vec<Duration> = Vec::with_capacity(rounds as usize);
    let mut plain_rounds: Vec<Duration> = Vec::with_capacity(rounds as usize);
    let mut i1_rounds: Vec<Duration> = Vec::with_capacity(rounds as usize);
    let mut i2a_rounds: Vec<Duration> = Vec::with_capacity(rounds as usize);
    let mut i2b_rounds: Vec<Duration> = Vec::with_capacity(rounds as usize);
    let mut i3_rounds: Vec<Duration> = Vec::with_capacity(rounds as usize);
    // 索引アーム・plain アームの `scalar_index_cache_stats()` 増分は非 vacuous
    // 性の確認を毎ラウンド fail-closed に行った上で、最終出力にはラウンド 0 の
    // 値を代表値として使う（カウンタはラウンドをまたいで単調増加するため、
    // 後続ラウンドの delta も同じ符号・非ゼロ性を持つ。実測値そのものは
    // ラウンドごとに変わりうるが、非 vacuous 性の結論には影響しない）。
    let mut index_scans_delta = 0u64;
    let mut index_plain_fallbacks_delta = 0u64;
    let mut index_builds_delta = 0u64;
    let mut index_arena_hits_delta = 0u64;
    let mut plain_scans_delta = 0u64;
    let mut plain_fallbacks_delta = 0u64;
    // `index_mask_scans`（`sql/exec.rs` の `filter_cached_rls_rows_subset` →
    // `search_subset` 経路。#654）の増分を非 vacuous に確認する（codex-review
    // P1 指摘・PR #663。I2b/I3 が再実装するのはこの経路であり、e2e(index) が
    // 実際にここを通っていることを本カウンタで裏取りする）。
    let mut index_mask_scans_delta = 0u64;

    for round in 0..rounds {
        // --- 索引アーム（Issue #653）: `scalar_index_cache_stats()` の増分で
        // `ScalarIndex::resolve_candidates`（Issue #474）による候補削減が実際に
        // 発火していることを非 vacuous に確認する。
        let index_stats_before = core.scalar_index_cache_stats();
        let index_arena_before = core.sql_arena_cache_stats();
        let w0_hot = run(&config, || {
            core.execute_sql(&ctx_a, &sql_where)
                .expect("execute_sql must succeed for well-formed synthetic WHERE query")
        })
        .expect("measurement must satisfy protocol minimums");
        let index_stats_after = core.scalar_index_cache_stats();
        let index_arena_after = core.sql_arena_cache_stats();
        index_rounds.push(w0_hot.summary.median);
        let w0_hot_result = core
            .execute_sql(&ctx_a, &sql_where)
            .expect("execute_sql must succeed for well-formed synthetic WHERE query");
        if w0_hot_result.rows.len() != TOP_K {
            fail_closed(format!(
                "round {round}: W0-hot result row count mismatch: expected {TOP_K}, got {}",
                w0_hot_result.rows.len()
            ));
        }
        // cold 側と同じ「返却 id が lang='ja' の可視集合に含まれる」検証を hot 側にも
        // 課す。cache ヒット経路（`SqlArenaCache`／`sql/hnsw_cache.rs` 等）はここでしか
        // 通過せず、キャッシュヒット時にフィルタが未適用のまま別の行が返っても件数
        // だけの確認では検出できない（P2 指摘）。
        let w0_hot_ids: Vec<u64> = w0_hot_result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Integer(v)) => *v,
                other => fail_closed(format!(
                    "round {round}: W0-hot id cell type mismatch: got {other:?}"
                )),
            })
            .collect();
        if w0_hot_ids.iter().any(|id| !expected_match_ids.contains(id)) {
            fail_closed(format!(
                "round {round}: W0-hot returned an id outside the lang='ja' visible set (tenant/filter leak suspected, possibly via a stale cache hit)"
            ));
        }
        let round_index_scans_delta = index_stats_after
            .index_scans
            .saturating_sub(index_stats_before.index_scans);
        let round_index_plain_fallbacks_delta = index_stats_after
            .plain_scan_fallbacks
            .saturating_sub(index_stats_before.plain_scan_fallbacks);
        let round_index_builds_delta = index_stats_after
            .builds
            .saturating_sub(index_stats_before.builds);
        let round_index_arena_hits_delta = index_arena_after
            .hits
            .saturating_sub(index_arena_before.hits);
        let round_index_mask_scans_delta = index_stats_after
            .index_mask_scans
            .saturating_sub(index_stats_before.index_mask_scans);
        // 索引アームは `index_scans` が実際に増え（非 vacuous）、`plain_scan_fallbacks`
        // は増えない（=常に候補削減経路を消費する）ことを毎ラウンド fail-closed に確認する。
        if round_index_scans_delta == 0 {
            fail_closed(format!(
                "round {round}: index arm did not consume ScalarIndex candidate resolution (index_scans delta=0; selectivity=1/{selectivity_denominator} may exceed the resolve_candidates threshold and fall back to plain scan)"
            ));
        }
        if round_index_plain_fallbacks_delta != 0 {
            fail_closed(format!(
                "round {round}: index arm unexpectedly fell back to plain scan (plain_scan_fallbacks delta={round_index_plain_fallbacks_delta})"
            ));
        }
        // `index_mask_scans` は「候補削減が発火し、かつ複製を経ずマスクだけを
        // 構築した」（#654）ことの直接証跡。本ベンチの `Ranking::Distance`・
        // 非 hybrid・HNSW opt-in 無し構成では、候補削減が発火した
        // （`round_index_scans_delta > 0`）ラウンドは必ずこの分岐へ入るため、
        // 増分 0 は I2b/I3 が再実装している実装経路（#654 相当）を e2e(index)
        // が実際には通っていないことを意味する（codex-review P1 指摘・PR #663）。
        if round_index_mask_scans_delta == 0 {
            fail_closed(format!(
                "round {round}: index arm did not take the #654 mask path (index_mask_scans delta=0; I2b/I3 reimplement filter_cached_rls_rows_subset -> search_subset, but e2e(index) did not exercise it this round)"
            ));
        }
        if round == 0 {
            index_scans_delta = round_index_scans_delta;
            index_plain_fallbacks_delta = round_index_plain_fallbacks_delta;
            index_builds_delta = round_index_builds_delta;
            index_arena_hits_delta = round_index_arena_hits_delta;
            index_mask_scans_delta = round_index_mask_scans_delta;
        }

        // --- plain アーム（Issue #653）: `AND vec_norm(embedding) > 0` により `PlainScan` へ縮退させ、
        // 索引経路を一切消費しないことを確認する（`index_scans`／`plain_scan_fallbacks`
        // がいずれも不変のまま、直前の索引アーム側の正の増分と対照させることで
        // 非 vacuous に検証する。理由は下記アサーション直前のコメント参照）。
        let plain_stats_before = core.scalar_index_cache_stats();
        let w0_plain = run(&config, || {
            core.execute_sql(&ctx_a, &sql_where_plain).expect(
                "execute_sql must succeed for well-formed synthetic WHERE query (plain arm)",
            )
        })
        .expect("measurement must satisfy protocol minimums");
        let plain_stats_after = core.scalar_index_cache_stats();
        plain_rounds.push(w0_plain.summary.median);
        let w0_plain_result = core
            .execute_sql(&ctx_a, &sql_where_plain)
            .expect("execute_sql must succeed for well-formed synthetic WHERE query (plain arm)");
        let w0_plain_ids: Vec<u64> = w0_plain_result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Integer(v)) => *v,
                other => fail_closed(format!(
                    "round {round}: W0-plain id cell type mismatch: got {other:?}"
                )),
            })
            .collect();
        if w0_plain_ids
            .iter()
            .any(|id| !expected_match_ids.contains(id))
        {
            fail_closed(format!(
                "round {round}: W0-plain returned an id outside the lang='ja' visible set (tenant/filter leak suspected)"
            ));
        }
        // 両アームは論理的に同一の `WHERE` 述語（`lang = 'ja'` かどうか）を持つため、
        // 索引経路・plain scan 経路のいずれで実行しても同一の Top-k id 集合を返す
        // ことを毎ラウンド固定する（索引経路が誤って別の行を返す退行を検出する）。
        let mut w0_hot_ids_sorted = w0_hot_ids.clone();
        w0_hot_ids_sorted.sort_unstable();
        let mut w0_plain_ids_sorted = w0_plain_ids.clone();
        w0_plain_ids_sorted.sort_unstable();
        if w0_hot_ids_sorted != w0_plain_ids_sorted {
            fail_closed(format!(
                "round {round}: index arm and plain arm returned different id sets for the logically identical WHERE predicate"
            ));
        }
        let round_plain_scans_delta = plain_stats_after
            .index_scans
            .saturating_sub(plain_stats_before.index_scans);
        let round_plain_fallbacks_delta = plain_stats_after
            .plain_scan_fallbacks
            .saturating_sub(plain_stats_before.plain_scan_fallbacks);
        // `classify_scalar_plan` が `PlainScan` を返す形状（`sql/exec.rs`
        // 参照）は「索引対応述語のみだが lookup/resolve に失敗した」
        // （`FallbackNoIndex`／`FallbackSelectivity`。`plain_scan_fallbacks` を
        // 記録する）とは異なり、索引を消費する分岐そのものへ一切入らないため
        // `index_scans`／`plain_scan_fallbacks` のいずれも増えない。これは
        // production の実際の挙動（`sql/exec.rs` の `if scalar_plan_kind !=
        // ScalarPlan::PlainScan` ゲート）であり、本ベンチの計測手法上の欠落では
        // ない——直前の索引アーム側の非 vacuous 確認（`index_scans_delta > 0`）が
        // 同じ計測経路で正の増分を捉えていることの対照として、plain アームが
        // 両カウンタとも不変であることを毎ラウンド積極的に固定する。
        if round_plain_scans_delta != 0 {
            fail_closed(format!(
                "round {round}: plain arm unexpectedly consumed ScalarIndex candidate resolution (index_scans delta={round_plain_scans_delta})"
            ));
        }
        if round_plain_fallbacks_delta != 0 {
            fail_closed(format!(
                "round {round}: plain arm unexpectedly recorded a FallbackNoIndex/FallbackSelectivity plain scan fallback (plain_scan_fallbacks delta={round_plain_fallbacks_delta}; expected 0 for a pure PlainScan-classified query, which never enters the index-consuming branch)"
            ));
        }
        if round == 0 {
            plain_scans_delta = round_plain_scans_delta;
            plain_fallbacks_delta = round_plain_fallbacks_delta;
        }

        // --- I 系列（索引経路の内訳。Issue #653）: `ScalarIndex`（pub(crate)）の
        // 候補削減を pub API（`scan_scalar_columns`／`declarative_filter::matches_all`／
        // `ParallelSearchProvider::search`）で再実装し、I1〜I3 を計測する。
        let i1 = run(&config, || {
            // `ScalarIndex::candidates_for` は lookup ＋ 複製ののち
            // `sort_unstable` でスロット昇順契約を満たす（`sql/scalar_index.rs`
            // 参照）。本ベンチの辞書は挿入順が既に昇順のため通常は no-op ソート
            // だが、production と同じ型（`Vec<u32>`）・同じコードパス（lookup →
            // 複製 → 整列）を踏むことを優先する（P2 指摘。旧実装は `Vec<usize>`
            // かつ整列を欠いていた）。
            let mut v: Vec<u32> = lang_candidates
                .get(TARGET_LANG)
                .cloned()
                .unwrap_or_default();
            v.sort_unstable();
            v
        })
        .expect("measurement must satisfy protocol minimums");
        i1_rounds.push(i1.summary.median);
        let i1_candidates: Vec<usize> = lang_candidates
            .get(TARGET_LANG)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|&slot| slot as usize)
            .collect();

        let i2a = run(&config, || {
            let mut matched = 0u64;
            for &idx in &i1_candidates {
                let scanned = scan_scalar_columns(&schema, &captured_metadata[idx])
                    .expect("scan_scalar_columns");
                if matches_lang_filter(&filters, &scanned) {
                    matched = matched.wrapping_add(std::hint::black_box(1u64));
                }
            }
            matched
        })
        .expect("measurement must satisfy protocol minimums");
        i2a_rounds.push(i2a.summary.median);

        // I2b: `VectorArena::filter_cached_rls_rows_subset`（`pub(crate)` のため
        // ベンチから直接呼べない）相当の再実装（codex-review P1 指摘・PR #663）。
        // 現行 HEAD は #654 適用済みで、index アームの温まった経路
        // （`sql/exec.rs::execute_statement_with_cache` の `!is_hybrid &&
        // !hnsw_subset_eligible` 分岐）は `VectorArena::build_from_cached_rls_rows_subset`
        // による embedding／id／tenant_id／visibility の複製ではなく、
        // `filter_cached_rls_rows_subset` によるキャッシュ済みスナップショットの
        // `VectorArena` 借用のまま候補述語を再適用し「一致した元スロット番号」の
        // マスク（`Vec<u32>`。狭義昇順）だけを構築する経路を通る（本ベンチの
        // `Ranking::Distance`・非 hybrid・HNSW opt-in 無し構成では常にこの分岐。
        // §モジュールドキュメント「SCALAR 事前フィルタ付き DISTANCE の索引経路
        // 内訳」参照）。P1 修正前の実装は旧経路（#654 以前相当）の複製コストを
        // 計測しており、e2e(index) が実際には経由しない処理量を I2b/I3 へ
        // 混入させ、残差（SQL 表層固定コスト）の算出契約と実装経路が不整合
        // だった。以降は複製を一切行わず、マスク（`Vec<u32>`）構築のみを測る。
        let i2b = run(&config, || {
            let mut kept: Vec<u32> = Vec::new();
            for &idx in &i1_candidates {
                let scanned = scan_scalar_columns(&schema, &captured_metadata[idx])
                    .expect("scan_scalar_columns");
                if matches_lang_filter(&filters, &scanned) {
                    kept.push(idx as u32);
                }
            }
            kept
        })
        .expect("measurement must satisfy protocol minimums");
        i2b_rounds.push(i2b.summary.median);

        // I3: `filter_cached_rls_rows_subset` が返すマスクを、複製せず借用したまま
        // の `arena.vectors()` へそのまま渡す `SearchProvider::search_subset`
        // （Issue #654）相当の再実装。`i_matched_slots` は I2b と同じ述語適用で
        // 独立に導出し（I2b のクロージャ内部値をタイミング計測外へ持ち出さない
        // ため）、id 側は検証（`expected_match_ids` との突合）専用に別途復元する。
        let i_matched_slots: Vec<u32> = i1_candidates
            .iter()
            .filter(|&&idx| {
                let scanned = scan_scalar_columns(&schema, &captured_metadata[idx])
                    .expect("scan_scalar_columns");
                matches_lang_filter(&filters, &scanned)
            })
            .map(|&idx| idx as u32)
            .collect();
        let i_matched_ids: Vec<u64> = i_matched_slots
            .iter()
            .map(|&slot| captured_ids[slot as usize])
            .collect();
        if i_matched_ids != expected_match_ids {
            fail_closed(format!(
                "round {round}: I2a/I2b matched id set diverged from independently derived expected_match_ids (index path regression suspected)"
            ));
        }
        let i3 = run(&config, || {
            parallel_provider
                .search_subset(engine::kernel::SubsetSearchInput {
                    slots: &i_matched_slots,
                    vectors: arena.vectors(),
                    dim: dim as u32,
                    query: &query,
                    k: TOP_K,
                })
                .expect("search must succeed for well-formed synthetic input")
        })
        .expect("measurement must satisfy protocol minimums");
        i3_rounds.push(i3.summary.median);
    }

    let index_min = min_of(&index_rounds).expect("rounds >= 1");
    let index_med = median_of(&index_rounds).expect("rounds >= 1");
    let plain_min = min_of(&plain_rounds).expect("rounds >= 1");
    let plain_med = median_of(&plain_rounds).expect("rounds >= 1");
    let i1_min = min_of(&i1_rounds).expect("rounds >= 1");
    let i1_med = median_of(&i1_rounds).expect("rounds >= 1");
    let i2a_min = min_of(&i2a_rounds).expect("rounds >= 1");
    let i2a_med = median_of(&i2a_rounds).expect("rounds >= 1");
    let i2b_min = min_of(&i2b_rounds).expect("rounds >= 1");
    let i2b_med = median_of(&i2b_rounds).expect("rounds >= 1");
    let i3_min = min_of(&i3_rounds).expect("rounds >= 1");
    let i3_med = median_of(&i3_rounds).expect("rounds >= 1");

    let w0_nowhere = run(&config, || {
        core.execute_sql(&ctx_a, &sql_nowhere)
            .expect("execute_sql must succeed for well-formed synthetic KNN query")
    })
    .expect("measurement must satisfy protocol minimums");

    let a0a = run(&config, || {
        core.execute_sql(&ctx_a, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");
    let a0b = run(&config, || {
        core.execute_sql(&ctx_b, &count_sql)
            .expect("execute_sql must succeed for COUNT(*) query")
    })
    .expect("measurement must satisfy protocol minimums");

    let count_a = core
        .execute_sql(&ctx_a, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    let count_b = core
        .execute_sql(&ctx_b, &count_sql)
        .expect("execute_sql must succeed for COUNT(*) query");
    for (label, result) in [("ctx_a", &count_a), ("ctx_b", &count_b)] {
        if result.rows.len() != 1 {
            fail_closed(format!(
                "COUNT(*) row count mismatch ({label}): expected 1, got {}",
                result.rows.len()
            ));
        }
        let value = match result.rows[0].cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => fail_closed(format!(
                "COUNT(*) cell type mismatch ({label}): got {other:?}"
            )),
        };
        if value != tenant_a_rows {
            fail_closed(format!(
                "COUNT(*) value mismatch ({label}): expected {tenant_a_rows}, got {value}"
            ));
        }
    }

    // --- 出力: 全測定・全整合性検証を完了したここまでの間、測定値は一切
    // println! していない（fail-closed 契約。`knn_profile_bench.rs` と同方針）。
    let a1_min = min_of(&a1_rounds).expect("rounds >= 1");
    let a1_med = median_of(&a1_rounds).expect("rounds >= 1");
    let a2_min = min_of(&a2_rounds).expect("rounds >= 1");
    let a2_med = median_of(&a2_rounds).expect("rounds >= 1");
    let a3_min = min_of(&a3_rounds).expect("rounds >= 1");
    let a3_med = median_of(&a3_rounds).expect("rounds >= 1");
    let a4_min = min_of(&a4_rounds).expect("rounds >= 1");
    let a4_med = median_of(&a4_rounds).expect("rounds >= 1");
    let a5_min = min_of(&a5_rounds).expect("rounds >= 1");
    let a5_med = median_of(&a5_rounds).expect("rounds >= 1");
    let w1_min = min_of(&w1_rounds).expect("rounds >= 1");
    let w1_med = median_of(&w1_rounds).expect("rounds >= 1");
    let w2_min = min_of(&w2_rounds).expect("rounds >= 1");
    let w2_med = median_of(&w2_rounds).expect("rounds >= 1");
    let w3_min = min_of(&w3_rounds).expect("rounds >= 1");
    let w3_med = median_of(&w3_rounds).expect("rounds >= 1");
    let w4_min = min_of(&w4_rounds).expect("rounds >= 1");
    let w4_med = median_of(&w4_rounds).expect("rounds >= 1");
    let r_dot_min = min_of(&r_dot_rounds).expect("rounds >= 1");
    let r_dot_med = median_of(&r_dot_rounds).expect("rounds >= 1");
    let ref_band_pct = reference_band(&r_dot_rounds).unwrap_or(0.0) * 100.0;

    println!("--- per-round raw medians (ms) ---");
    for (round, (((((((((a1, a2), a3), a4), a5), w1), w2), w3), w4), r_dot)) in a1_rounds
        .iter()
        .zip(&a2_rounds)
        .zip(&a3_rounds)
        .zip(&a4_rounds)
        .zip(&a5_rounds)
        .zip(&w1_rounds)
        .zip(&w2_rounds)
        .zip(&w3_rounds)
        .zip(&w4_rounds)
        .zip(&r_dot_rounds)
        .enumerate()
    {
        println!(
            "round[{round}]: A1={:.3} A2={:.3} A3={:.3} A4={:.3} A5={:.3} W1={:.3} W2={:.3} W3={:.3} W4={:.3} R_dot={:.3}",
            a1.as_secs_f64() * 1e3,
            a2.as_secs_f64() * 1e3,
            a3.as_secs_f64() * 1e3,
            a4.as_secs_f64() * 1e3,
            a5.as_secs_f64() * 1e3,
            w1.as_secs_f64() * 1e3,
            w2.as_secs_f64() * 1e3,
            w3.as_secs_f64() * 1e3,
            w4.as_secs_f64() * 1e3,
            r_dot.as_secs_f64() * 1e3,
        );
    }

    println!("--- A series (median-of-R / min-of-R, ns/row over total physical rows) ---");
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A1_redb_scan",
            total_physical_rows,
            a1_med,
            ns_per_row(a1_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a1_min.as_secs_f64() * 1e3
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A2_header_decode",
            total_physical_rows,
            a2_med,
            ns_per_row(a2_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a2_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A1",
        "A2",
        a1_med,
        a2_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A3_rls_visible",
            total_physical_rows,
            a3_med,
            ns_per_row(a3_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a3_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A2",
        "A3",
        a2_med,
        a3_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A4_dim_meta_decode",
            total_physical_rows,
            a4_med,
            ns_per_row(a4_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a4_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A3",
        "A4",
        a3_med,
        a4_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "A5_scalar_validate",
            total_physical_rows,
            a5_med,
            ns_per_row(a5_med, total_physical_rows).expect("total_physical_rows > 0"),
        ),
        a5_min.as_secs_f64() * 1e3
    );
    report_diff(
        "A4",
        "A5",
        a4_med,
        a5_med,
        total_physical_rows,
        ref_band_pct,
    );
    println!(
        "e2e(agg_count/A0a, ctx=tenant-a): median={:.3}ms",
        a0a.summary.median.as_secs_f64() * 1e3
    );
    println!(
        "e2e(rls_isolation/A0b, ctx=tenant-b): median={:.3}ms",
        a0b.summary.median.as_secs_f64() * 1e3
    );
    // A0c-cold は `rounds`（A1〜A5・W1〜W4・R_dot が使う「ラウンド」概念）の
    // 系列ではなく、`config.measured_iterations()`（本ベンチでは 20）個の
    // 生サンプルを直接集計している（`MeasurementConfig::new` 呼び出し・上の
    // `for _ in 0..config.measured_iterations()` ループ参照）。他系列の
    // 「min-of-R」（R = ラウンド数。既定 `BENCH_SCAN_PROFILE_ROUNDS`）と表記を
    // 揃えると集計対象が異なるにもかかわらず同じ略記になり誤解を招くため
    // （PR #586 codex-review 指摘）、ここでは実際のサンプル数を明記した
    // 「sample minimum (N=<count>)」表記を用いる。
    println!(
        "e2e(agg_count/A0c-cold, ctx=tenant-a, includes Storage::open): median={:.3}ms (sample minimum, N={}: {:.3}ms)",
        a0c_summary.median.as_secs_f64() * 1e3,
        a0c_samples.len(),
        a0c_samples
            .iter()
            .min()
            .expect("A0-cold has at least one sample")
            .as_secs_f64()
            * 1e3
    );

    let visible_rows = tenant_a_rows as usize;
    println!(
        "--- W series (median-of-R / min-of-R, ns/row over visible (tenant-a Public) rows) ---"
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "W1_scalar_scan",
            visible_rows,
            w1_med,
            ns_per_row(w1_med, visible_rows).expect("visible_rows > 0"),
        ),
        w1_min.as_secs_f64() * 1e3
    );
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "W2_predicate",
            visible_rows,
            w2_med,
            ns_per_row(w2_med, visible_rows).expect("visible_rows > 0"),
        ),
        w2_min.as_secs_f64() * 1e3
    );
    report_diff("W1", "W2", w1_med, w2_med, visible_rows, ref_band_pct);
    println!(
        "{} (min-of-R={:.3}ms)",
        render_stage_line(
            "W3_arena_copy",
            visible_rows,
            w3_med,
            ns_per_row(w3_med, visible_rows).expect("visible_rows > 0"),
        ),
        w3_min.as_secs_f64() * 1e3
    );
    report_diff("W2", "W3", w2_med, w3_med, visible_rows, ref_band_pct);
    println!(
        "W4_provider_search: matched_rows={} median={:.3}ms (min-of-R={:.3}ms)",
        expected_match_ids.len(),
        w4_med.as_secs_f64() * 1e3,
        w4_min.as_secs_f64() * 1e3
    );
    println!(
        "R_dot_kernel_distance_only (reference band): median={:.3}ms min-of-R={:.3}ms reference_band={:.2}%",
        r_dot_med.as_secs_f64() * 1e3,
        r_dot_min.as_secs_f64() * 1e3,
        ref_band_pct
    );
    println!(
        "e2e(vector_knn_where/W0-cold): median={:.3}ms",
        w0_cold_summary.median.as_secs_f64() * 1e3
    );
    println!(
        "e2e(vector_knn_where/W0-hot): median={:.3}ms (min-of-R={:.3}ms)",
        index_med.as_secs_f64() * 1e3,
        index_min.as_secs_f64() * 1e3
    );
    println!(
        "e2e(vector_knn/W0-nowhere, cache fast path): median={:.3}ms",
        w0_nowhere.summary.median.as_secs_f64() * 1e3
    );
    // 注意: `W0-nowhere` は可視行全体（tenant_a_rows 件）を候補集合として
    // dense 探索するのに対し、`W0-hot` は `lang='ja'` 一致行（約 20%）のみを
    // 候補集合とする。したがってこの raw diff には「SQL 表層内の WHERE 上乗せ」
    // だけでなく、dense 探索（距離計算・Top-k 選出）の候補集合サイズが異なる
    // ことによる処理量差も混入する（W3/W4 が示すとおり候補集合が小さいほど
    // 距離計算・Top-k コストは減る側に働くため、この raw diff を「WHERE 上乗せ」
    // として単純に W3（W1・W2 を含む累積値。W1+W2+W3 のような加算は W1 分の
    // scalar_scan コストを二重計上するため行わない）との比較・残差の帰属に
    // 使うことはできない。候補集合を揃えた比較は W 系列〔`W1`〜`W4`、いずれも一致行のみを対象〕・`R_dot`
    // （全可視行対象の参照区間）側で行う。P2 指摘・詳細は
    // `docs/design/scan-stage-profile.md`「W0-hot と W0-nowhere の候補集合差」
    // 節を参照）。
    let where_overhead_diff = index_med.checked_sub(w0_nowhere.summary.median);
    match where_overhead_diff {
        Some(d) => println!(
            "diff(W0-nowhere->W0-hot, raw diff; conflates WHERE overhead with reduced dense candidate set size, see docs): {:.3}ms",
            d.as_secs_f64() * 1e3
        ),
        None => println!(
            "diff(W0-nowhere->W0-hot): n/a (独立計測どうしの中央値比較のため測定ノイズにより逆転・未確定)"
        ),
    }

    // --- 選択率アーム（index/plain）・索引経路内訳（I 系列。Issue #653）。--------
    println!(
        "selectivity=1/{selectivity_denominator} expected_visible_hits={} ({:.2}%)",
        expected_match_ids.len(),
        expected_match_ids.len() as f64 / tenant_a_rows as f64 * 100.0
    );
    println!(
        "{}",
        render_arm_stats_line(
            ProfileArm::Index,
            TOP_K,
            index_scans_delta,
            index_plain_fallbacks_delta,
            index_builds_delta,
            index_arena_hits_delta,
        )
    );
    // `index_mask_scans` 増分（#654 の複製なしマスク経路。codex-review P1
    // 指摘・PR #663）。I2b/I3 が再実装している実装経路を e2e(index) が
    // 実際に消費していることの非 vacuous な裏取り。
    println!("index_mask_scans_delta={index_mask_scans_delta}");
    println!(
        "{}",
        render_arm_stats_line(
            ProfileArm::Plain,
            TOP_K,
            plain_scans_delta,
            plain_fallbacks_delta,
            0,
            0,
        )
    );
    println!(
        "e2e(index,k={TOP_K}): median={:.3}ms (min-of-R={:.3}ms)",
        index_med.as_secs_f64() * 1e3,
        index_min.as_secs_f64() * 1e3
    );
    println!(
        "e2e(plain,k={TOP_K}): median={:.3}ms (min-of-R={:.3}ms)",
        plain_med.as_secs_f64() * 1e3,
        plain_min.as_secs_f64() * 1e3
    );
    // 注意（P1 指摘・PR #663）: `plain` アームの `AND vec_norm(embedding) > 0` は
    // `sql/expr_program.rs`（Issue #353）の定数畳み込み対象ではない——
    // `embedding` は行ごとに異なる値を持つ列参照のため、`vec_norm` の評価は
    // 「行に依存しないスカラー算術・比較部分式」という定数畳み込みの適用条件を
    // 満たさず、`matches_all` 評価のたびに候補行数 × dim 相当の実ベクトル演算
    // （L2 ノルム計算）が発生する（`docs/design/filtered-distance-stage-profile.md`
    // の旧記述「定数畳み込み済みステップ 1 個/行」は誤りだった）。したがって
    // 以下の `arm_ratio` は「`ScalarIndex` 候補削減の効果」と「plain アームのみに
    // 追加された `vec_norm` 実計算コスト」の 2 効果を混同した値であり、
    // `ScalarIndex` 単体の寄与を切り出した数値としては使えない（`plain` アーム
    // 側に一方的な上乗せがあるぶん、この比率は索引効果を過大に見せる方向へ
    // 偏る）。索引経路の内訳自体（I 系列）は plain アームを経由しないため
    // この混同の影響を受けない。
    match stage_diff_ns_per_row(index_med, plain_med, visible_rows, "index", "plain") {
        Ok(_) => {
            let ratio_pct = step_ratio_pct(index_med, plain_med).unwrap_or(0.0);
            println!(
                "arm_ratio(index->plain,k={TOP_K}): ratio={ratio_pct:.2}% (conflates ScalarIndex candidate reduction with plain-arm-only vec_norm cost; not an isolated index-effect measurement, see comment above)"
            );
        }
        Err(_) => {
            let ratio_pct = step_ratio_pct(plain_med, index_med).unwrap_or(0.0);
            println!(
                "arm_ratio(plain->index,k={TOP_K}): ratio={ratio_pct:.2}% (index arm is slower than the plain arm this round; plain arm is the Issue #474-以前-shaped baseline. conflates the same two effects noted above)"
            );
        }
    }

    println!("--- index/plain/I-series per-round raw medians (ms) ---");
    for round in 0..index_rounds.len() {
        println!(
            "round[{round}]: index={:.3} plain={:.3} I1={:.3} I2a={:.3} I2b={:.3} I3={:.3}",
            index_rounds[round].as_secs_f64() * 1e3,
            plain_rounds[round].as_secs_f64() * 1e3,
            i1_rounds[round].as_secs_f64() * 1e3,
            i2a_rounds[round].as_secs_f64() * 1e3,
            i2b_rounds[round].as_secs_f64() * 1e3,
            i3_rounds[round].as_secs_f64() * 1e3,
        );
    }

    println!("--- index path buckets (I series; us and % of e2e index arm; median-of-R) ---");
    let e2e_index_total = index_med;
    for (label, dur, min_dur) in [
        ("I1_index_candidate_resolve", i1_med, i1_min),
        ("I2a_candidate_predicate", i2a_med, i2a_min),
        ("I2b_candidate_mask_build", i2b_med, i2b_min),
        ("I3_provider_search", i3_med, i3_min),
    ] {
        let us = dur.as_secs_f64() * 1e6;
        let min_us = min_dur.as_secs_f64() * 1e6;
        match bucket_share_pct(dur, e2e_index_total) {
            Ok(pct) => println!(
                "{} (min-of-R={min_us:.1}us)",
                render_bucket_share_line(label, us, pct)
            ),
            Err(e) => println!(
                "bucket_share({label}): us={us:.1} pct_of_e2e=n/a ({e}) (min-of-R={min_us:.1}us)"
            ),
        }
    }
    // SQL 表層固定コスト（残差）= e2e(index) − (I1 + I2b + I3)。I2b（マスク
    // 構築。#654 相当）は I2a（述語判定のみ）と同じ候補集合を独立に再走査する
    // ため I2a を含む累積値ではない（I2a はマスク非構築の対照値として別出しの
    // まま維持し、加算対象には含めない。codex-review P1 指摘・PR #663で
    // I2b/I3 を #654 適用後の実装経路〔複製なし〕へ揃えたことに伴う整理）。
    let index_path_sum = i1_med
        .checked_add(i2b_med)
        .and_then(|d| d.checked_add(i3_med));
    match index_path_sum.and_then(|sum| e2e_index_total.checked_sub(sum)) {
        Some(residual) => {
            let us = residual.as_secs_f64() * 1e6;
            match bucket_share_pct(residual, e2e_index_total) {
                Ok(pct) => println!(
                    "{}",
                    render_bucket_share_line("sql_surface_fixed_cost_residual", us, pct)
                ),
                Err(e) => println!(
                    "bucket_share(sql_surface_fixed_cost_residual): us={us:.1} pct_of_e2e=n/a ({e})"
                ),
            }
        }
        None => println!(
            "bucket_share(sql_surface_fixed_cost_residual): n/a (I1+I2b+I3 の合計が e2e(index) を超過。測定ノイズにより逆転・未確定)"
        ),
    }

    // --- HNSW opt-in（Issue #677。親 Issue #676 の Subset 縮退時委譲の観測）。
    // `scan_engine == BruteForce`（既定・未設定）のときは本節を一切実行せず、
    // ここまでの出力・検証は #677 導入前とビット同一のまま不変。
    //
    // `HnswIndexCacheStats`（`sql::hnsw_cache`。`pub(crate)` モジュール配下の
    // ため型名は書けない）のうち本節が参照するのは `hits`／`builds`／
    // `subset_searches`／`plain_scans`／`mask_splits_graph`／`masked_short`
    // （いずれも Issue #676 より前から存在するフィールド）のみに限定し、
    // #676 が新設した `subset_mask_scans`／`subset_arena_copies` は直接
    // 参照しない——本ファイルを #676 適用前の独立ソースツリー（`git archive`
    // 展開）へ overlay してもコンパイルできる状態を保つための意図的な制約
    // （`docs/design/hnsw-rls-cardinality-switch.md`「Issue #677」節参照）。
    // 採否そのものの直接観測（`subset_mask_scans`／`subset_arena_copies`）は
    // #676 適用後（HEAD）限定の別セッションで行う。
    if scan_engine != harness::bench_engine::BenchEngine::BruteForce {
        // 直前まで使っていた既定エンジンの `core`（brute_force・`Storage::open`
        // 保持）を明示的に破棄してから同じ redb ファイルを再オープンする
        // （`W0-cold`／`A0-cold` 系列が同型の理由で `Storage::open` を都度
        // 再オープンしているのと同じ制約。redb は 1 プロセス内で同一ファイルの
        // 二重オープンを許さない）。
        drop(core);
        let hnsw_storage =
            Storage::open(&path).expect("reopen storage for HNSW opt-in (Issue #677)");
        let hnsw_kind = {
            let mut validated = ValidatedHnswParams::new(HnswParams::default())
                .expect("valid HnswParams::default()");
            if let Some((numerator, denominator)) = scan_full_scan_ratio_override {
                validated = validated
                    .with_full_scan_ratio(Ratio { numerator, denominator })
                    .expect("BENCH_SCAN_PROFILE_FULL_SCAN_RATIO already validated by harness::bench_engine::parse_full_scan_ratio");
            }
            match scan_engine {
                harness::bench_engine::BenchEngine::Hnsw => SearchEngineKind::Hnsw(validated),
                harness::bench_engine::BenchEngine::HnswF16 => SearchEngineKind::Hnsw(
                    validated.with_resident_precision(engine::hnsw::ResidentPrecision::F16),
                ),
                harness::bench_engine::BenchEngine::HnswI8 => SearchEngineKind::Hnsw(
                    validated.with_resident_precision(engine::hnsw::ResidentPrecision::I8),
                ),
                harness::bench_engine::BenchEngine::BruteForce => {
                    unreachable!("guarded by the outer `scan_engine != BruteForce` check above")
                }
            }
        };
        let hnsw_core = EngineCore::from_storage_with_engine(hnsw_storage, hnsw_kind);

        // warm: `Subset` 形状は自身では索引を構築せず既存索引の base を再利用
        // するのみのため（`sql/hnsw_cache.rs::prepare_subset` 冒頭コメント。
        // `knn_profile_bench.rs::run_visible_ratio_sweep` と同じ理由）、
        // フィルタなしクエリを 1 回発行して `FullVisible` 形状の索引を先に
        // 構築してから WHERE クエリを計測する。
        let warm_stats_before = hnsw_core.hnsw_index_cache_stats();
        hnsw_core
            .execute_sql(&ctx_a, &sql_nowhere)
            .expect("execute_sql must succeed for HNSW warm-up KNN query");
        let warm_stats_after = hnsw_core.hnsw_index_cache_stats();
        if warm_stats_after.builds <= warm_stats_before.builds {
            fail_closed(
                "HNSW warm-up did not build an index (builds delta<=0; Subset 経路が index base を再利用できず vacuous な計測になる)",
            );
        }

        let mut hnsw_rounds: Vec<Duration> = Vec::with_capacity(rounds as usize);
        let mut hnsw_hits_delta = 0u64;
        let mut hnsw_subset_searches_delta = 0u64;
        let mut hnsw_plain_scans_delta = 0u64;
        let mut hnsw_mask_splits_graph_delta = 0u64;
        let mut hnsw_masked_short_delta = 0u64;
        for round in 0..rounds {
            let stats_before = hnsw_core.hnsw_index_cache_stats();
            let hnsw_hot = run(&config, || {
                hnsw_core.execute_sql(&ctx_a, &sql_where).expect(
                    "execute_sql must succeed for well-formed synthetic WHERE query (hnsw arm)",
                )
            })
            .expect("measurement must satisfy protocol minimums");
            let stats_after = hnsw_core.hnsw_index_cache_stats();
            hnsw_rounds.push(hnsw_hot.summary.median);

            let hnsw_result = hnsw_core.execute_sql(&ctx_a, &sql_where).expect(
                "execute_sql must succeed for well-formed synthetic WHERE query (hnsw arm)",
            );
            let hnsw_ids: Vec<u64> = hnsw_result
                .rows
                .iter()
                .map(|row| match row.cells.first() {
                    Some(Cell::Integer(v)) => *v,
                    other => fail_closed(format!(
                        "round {round}: hnsw arm id cell type mismatch: got {other:?}"
                    )),
                })
                .collect();
            if hnsw_ids.iter().any(|id| !expected_match_ids.contains(id)) {
                fail_closed(format!(
                    "round {round}: hnsw arm returned an id outside the lang='ja' visible set (tenant/filter leak suspected)"
                ));
            }

            let round_hits_delta = stats_after.hits.saturating_sub(stats_before.hits);
            let round_subset_searches_delta = stats_after
                .subset_searches
                .saturating_sub(stats_before.subset_searches);
            let round_plain_scans_delta = stats_after
                .plain_scans
                .saturating_sub(stats_before.plain_scans);
            let round_mask_splits_graph_delta = stats_after
                .mask_splits_graph
                .saturating_sub(stats_before.mask_splits_graph);
            let round_masked_short_delta = stats_after
                .masked_short
                .saturating_sub(stats_before.masked_short);
            // いずれか 1 経路が非 vacuous に発火していることを毎ラウンド確認する
            // （`hits` は `FullVisible` 形状の warm-up 由来では増えない
            // ——`Subset` 形状専用に `subset_searches` を持つため、`Subset`
            // 形状が ANN 完走した場合は `subset_searches` が増える）。
            if round_hits_delta == 0
                && round_subset_searches_delta == 0
                && round_plain_scans_delta == 0
                && round_mask_splits_graph_delta == 0
                && round_masked_short_delta == 0
            {
                fail_closed(format!(
                    "round {round}: hnsw arm did not exercise any observable Subset regime (all counters delta=0; vacuous measurement)"
                ));
            }
            if round == 0 {
                hnsw_hits_delta = round_hits_delta;
                hnsw_subset_searches_delta = round_subset_searches_delta;
                hnsw_plain_scans_delta = round_plain_scans_delta;
                hnsw_mask_splits_graph_delta = round_mask_splits_graph_delta;
                hnsw_masked_short_delta = round_masked_short_delta;
            }
        }

        let hnsw_min = min_of(&hnsw_rounds).expect("rounds >= 1");
        let hnsw_med = median_of(&hnsw_rounds).expect("rounds >= 1");
        println!("--- hnsw arm per-round raw medians (ms) ---");
        for (round, dur) in hnsw_rounds.iter().enumerate() {
            println!("round[{round}]: hnsw={:.3}", dur.as_secs_f64() * 1e3);
        }
        println!(
            "e2e(vector_knn_where/hnsw-hot,k={TOP_K}): median={:.3}ms (min-of-R={:.3}ms)",
            hnsw_med.as_secs_f64() * 1e3,
            hnsw_min.as_secs_f64() * 1e3
        );
        println!(
            "hnsw_regime={} hits_delta={hnsw_hits_delta} subset_searches_delta={hnsw_subset_searches_delta} plain_scans_delta={hnsw_plain_scans_delta} mask_splits_graph_delta={hnsw_mask_splits_graph_delta} masked_short_delta={hnsw_masked_short_delta}",
            harness::scan_stage_profile::classify_hnsw_subset_regime(
                hnsw_hits_delta,
                hnsw_subset_searches_delta,
                hnsw_plain_scans_delta,
                hnsw_mask_splits_graph_delta,
                hnsw_masked_short_delta,
            )
        );
        match stage_diff_ns_per_row(
            index_med,
            hnsw_med,
            expected_match_ids.len(),
            "index",
            "hnsw",
        ) {
            Ok(_) => {
                // stage_diff_ns_per_row(index_med, hnsw_med, ...) が Ok ⇒
                // hnsw_med >= index_med（hnsw アームが同等以上に遅い）。
                // ラベル「index->hnsw」と揃うよう from=index_med/to=hnsw_med で
                // 比率を計算する（codex-review・Cursor Bugbot 指摘: 引数順が
                // ラベルと逆で符号・表示が反転していた）。
                let ratio_pct = step_ratio_pct(index_med, hnsw_med).unwrap_or(0.0);
                println!(
                    "arm_ratio(index->hnsw,k={TOP_K}): ratio={ratio_pct:.2}% (brute_force ScalarIndex-mask arm〔Issue #654〕vs hnsw arm〔Issue #676〕on the same WHERE+DISTANCE query; hnsw arm is slower or equal this round)"
                );
            }
            Err(_) => {
                // Err ⇒ hnsw_med < index_med（hnsw アームがこのラウンドは速い）。
                // ラベル「hnsw->index」と揃うよう from=hnsw_med/to=index_med で
                // 比率を計算する。
                let ratio_pct = step_ratio_pct(hnsw_med, index_med).unwrap_or(0.0);
                println!(
                    "arm_ratio(hnsw->index,k={TOP_K}): ratio={ratio_pct:.2}% (hnsw arm is faster than the brute_force ScalarIndex-mask arm this round)"
                );
            }
        }
    }

    println!("scan_stage_profile_bench: consistency checks passed (A3 visible row count == tenant_a_rows for both ctx_a/ctx_b every round, W2 match count == expected, W0-cold/W0-hot result ids within lang='ja' visible set, COUNT(*) == tenant_a_rows for both contexts, index arm and plain arm returned identical id sets, ScalarIndex candidate resolution confirmed non-vacuous via scalar_index_cache_stats() deltas)");
}

fn report_diff(
    from_label: &str,
    to_label: &str,
    from: Duration,
    to: Duration,
    rows: usize,
    reference_band_pct: f64,
) {
    match stage_diff_ns_per_row(from, to, rows, "from", "to") {
        Ok(diff_ns_per_row) => {
            println!("{}", render_diff_line(from_label, to_label, diff_ns_per_row));
            if let Ok(step_pct) = step_ratio_pct(from, to) {
                let band = classify_against_bands(step_pct, reference_band_pct);
                println!(
                    "{}",
                    render_bucket_line(
                        &format!("{from_label}->{to_label}"),
                        diff_ns_per_row,
                        step_pct,
                        band
                    )
                );
            }
        }
        Err(e) => println!(
            "diff({from_label}->{to_label}): n/a (独立計測どうしの中央値比較のため測定ノイズにより逆転・未確定として継続: {e})"
        ),
    }
}

/// `f32` のベクトルを SQL のベクトルリテラル形式へ整形する
/// （`harness::sql_c1::vector_literal` はエラー型を返す `VectorLiteral` を返す
/// ラッパー付き型のため、本ベンチでは `WHERE` 節を含む独自 SQL 文字列組み立てに
/// あわせて薄い直接整形を用いる。値は決定的 RNG 由来の有限値のみのため、
/// 非有限値検証は不要）。
fn vector_literal(values: &[f32]) -> String {
    let parts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
    format!("[{}]", parts.join(","))
}
