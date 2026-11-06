//! hybrid_rrf クエリの段別内訳プロファイルベンチ（Issue #356。親 Issue #355・
//! ポインタ: `docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3）。
//!
//! Issue #355 は「疎索引（`SparseIndex`）がクエリ毎に再構築され、これが
//! `hybrid_rrf` の主要コストと推定される」という調査結果を記録しているが、
//! 定量的な内訳切り分けは未実施だった。本ベンチは以下の段を実測し、後続の
//! Issue #357（`SparseIndex` のテーブル世代整合キャッシュ設計）が「どの段を
//! キャッシュ対象にすべきか」を判断できる分解能を提供する:
//!
//! 1. `sql_hybrid` / `sql_dense_knn`: `EngineCore::execute_sql` 経由の hybrid_rrf
//!    クエリと密 KNN のみのクエリの対照（SQL パース・束縛・テーブル走査を含む
//!    エンドツーエンドの差分の上限）
//! 2. `collect_body_strings`: 本文 String 収集（`sql/exec.rs:370-509` の
//!    `sparse_docs: Vec<(u64, String)>` 蓄積）の近似下限
//! 3. `sparse_build_total`: `SparseIndex::build` 単体
//! 4. `tokenize_only` / `tokenize_term_freq` / `tokenize_term_doc_freq`: build 内部の
//!    tokenize / term_freq 構築 / doc_freq マージの累積 3 段（複製実装。
//!    `harness::hybrid_profile` モジュールドキュメント「複製近似の限界」参照）
//!
//! Issue #387 は、キャッシュヒット後（Issue #357）になお残る `search_within`
//! 単体コストと疎側再取得ループの寄与を切り分けるため、以下を追加する:
//!
//! 5. `hybrid_search_cached_index`: 事前構築済み `SparseIndex`（キャッシュヒット
//!    相当）を使った `hybrid::hybrid_search` の直接呼び出し。密・疎双方の再取得
//!    発火回数（`sparse_refetch`／`provider_calls_max`）を併記する
//! 6. `search_within_fetch_k=<k>`: `SparseIndex::search_within` 単体を、疎側再取得
//!    スケジュール上で実際に呼ばれる `fetch_k` ごとに実測する
//! 7. `search_within_subset_only` / `search_within_subset_df` /
//!    `search_within_replica_full`: `search_within` 内部の可視 subset 構築／df 再
//!    計算パス／スコアリングパスの累積 3 区間（複製実装。起動時に実 API の出力と
//!    数値一致するかを fail-closed 検証してから使う）
//!
//! Issue #389（転置索引・doc_len／doc_ids 配列の追加）は受け入れ条件として
//! メモリ増分の実測・記録を求めるため、以下を追加する:
//!
//! 8. `memory stage=sparse_index_resident`: `SparseIndex` を保持したままの
//!    VmRSS 増分（`/proc/self/status`。読めない環境では `unavailable`）と
//!    `approx_heap_bytes()` の実測値
//!
//! Issue #392 は疎側再取得ループの再スコアリング回避（`SparseIndex::
//! score_within`／`SparseScored::top`）の効果を実測するため、以下を追加する:
//!
//! 9. `score_within_once`: `SparseIndex::score_within` 単体（クエリ 1 回分の
//!    スコア計算のみ。Top-k 選出を含まない）
//! 10. `sparse_refetch_loop`: `engine::hybrid::sparse_refetch_observed`
//!     （`hybrid_search_boosted` の疎側再取得ループ本体そのもの。Issue #387
//!     PR #416 で production 経路と共有化済み）を round-robin クエリで
//!     直接計測した、疎側再取得ループの実測累積コスト。既存の `search_within_
//!     fetch_k=<k>`＋`sparse_refetch_summary`（複数ラウンドの medianを合算
//!     した推定値）を実測で補完する
//!
//! # 実測値の比較可能性についての重要な注意
//!
//! `harness::hybrid_profile` モジュールドキュメント参照: Issue #355 が言及する
//! `feature_bench.rs` はこのリポジトリの履歴に存在しない。本ベンチのコーパスは
//! 新規に組み立てたものであり、**実測 ms は Issue #355 の 288ms と直接比較可能
//! ではない**。本ベンチが答える問いは「どの段が支配的か」という相対的な内訳の
//! 分解能であり、絶対値の再現ではない。
//!
//! # 出力規約
//!
//! spec 由来の pass/fail 閾値を持たない情報提供専用ベンチ（`hybrid_latency_bench.rs`
//! と同方針）。実測値は常に標準出力する（実測値の公開はオーナー判断 2026-08-29 で
//! 許可済み。spec 閾値の注入・表示は行わないため逆算リスクなし）。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make
//! bench-hybrid-profile` からの手動実行専用）。`GITHUB_ACTIONS` 環境変数が
//! 設定されていれば起動直後に fail-closed で拒否する
//! （`harness::hybrid_profile::refuse_under_github_actions`）。
//!
//! 判定ロジック自体（時間非依存）は `harness::hybrid_profile` にあり
//! `tests/hybrid_profile_accept.rs` で `make ci` 側から回帰検証する。

#[allow(dead_code)]
mod harness;

use std::collections::BTreeSet;
use std::collections::HashMap;

use harness::env_report::EnvReport;
use harness::hybrid_latency::RefetchTrackingProvider;
use harness::hybrid_profile::{
    bodyclone_replica, bucket_diff, collect_body_strings, dense_refetch_schedule,
    expected_visible_count, fetch_cap, generate_corpus, generate_queries, initial_fetch_k,
    refetch_schedule_matches_observed_calls, refuse_under_github_actions,
    render_baseline_bucket_line, render_cache_stats_delta_line, render_dense_refetch_line,
    render_sparse_refetch_line, render_sparse_refetch_summary_line, render_sql_surface_bucket_line,
    render_sql_surface_round_raw_line, render_sql_surface_summary_line, render_stage_line,
    replica_matches_real, resolve_rows_from_env, resolve_visible_ratio_denominator_from_env,
    rowcopy_replica, scan_replica, select_visible_ids, slotmap_replica, sparse_refetch_schedule,
    sql_dense_statement, sql_dense_statement_with_projection, sql_hybrid_statement,
    sql_hybrid_statement_with_projection, summarize_sparse_refetch, tokenize_only,
    tokenize_term_doc_freq, tokenize_term_freq, HybridProjection, ProfileSparseIndex,
    SQL_DEFAULT_HYBRID_POOL_DEPTH,
};
use harness::protocol::{run, run_bounded_retain, MeasurementConfig};
use harness::scan_stage_profile::{
    classify_against_bands, median_of, min_of, parse_rounds, reference_band, step_ratio_pct,
    ScanStageError,
};

use engine::arena::VectorArena;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hybrid::{hybrid_search, rrf_fuse, sparse_refetch_observed, RrfConfig};
use engine::kernel::{CandidateHit, CpuScalarProvider, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::rls::RlsSafetyNet;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine;
use engine::sparse::{ScoredDoc, SparseIndex};
use engine::sql::allowlist::{validate_sql, Statement as SqlStatement};
use engine::sql::mode::SessionState;
use engine::sql::parser::bind_in_session;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// コーパス規模の既定値（Issue #356 本文が言及する feature_bench の行数感に
/// 合わせた、本ベンチ独自の値。spec 由来の値ではない）。Issue #547 で
/// `BENCH_HYBRID_PROFILE_ROWS`（既定 25,000・後方互換）による opt-in 可変化。
const DIM: usize = 128;
const TOP_K: usize = 10;
const NUM_QUERIES: usize = 5;
const SEED: u64 = 0x3562_3562_3562_3562;

const TABLE: &str = "docs";
const VECTOR_COLUMN: &str = "embedding";
const TEXT_COLUMN: &str = "body";
const TENANT_ID: &str = "hybrid-profile-tenant";
/// 単一 write トランザクションの確保量を有界化するための投入チャンクサイズ
/// （`sql_c1_bench.rs::SEED_BATCH_ROWS` と同一方針）。
const SEED_BATCH_ROWS: usize = 5_000;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("hybrid_profile_bench: {msg}");
    std::process::exit(1);
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "hybrid_profile_bench: measures the stage-by-stage breakdown of hybrid_rrf query \
         latency (Issue #356; corpus is freshly authored, NOT a reproduction of Issue #355's \
         feature_bench figures — see docs/design/hybrid-rrf-latency-breakdown.md). Not a \
         pass/fail gate."
    );

    // --- 行数・可視率の opt-in（Issue #547。#546〔PR #565〕のスコアアキュムレータ
    // 再利用が「索引 N ≫ 可視集合」条件で効くかを検証するための注入点。
    // `harness::hybrid_profile` モジュールドキュメント「可視率の意味」参照） ---
    let num_docs = resolve_rows_from_env().unwrap_or_else(|e| fail_closed(e.to_string()));
    let visible_ratio_denominator =
        resolve_visible_ratio_denominator_from_env().unwrap_or_else(|e| fail_closed(e.to_string()));
    let expected_visible = expected_visible_count(num_docs, visible_ratio_denominator)
        .unwrap_or_else(|e| fail_closed(e.to_string()));

    // --- コーパス生成（密ベクトル・疎本文とも決定的） ---
    let corpus = generate_corpus(SEED, num_docs, DIM)
        .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));
    let queries = generate_queries(SEED, NUM_QUERIES, DIM);

    let doc_refs = corpus.sparse_docs();

    // 可視行 id 集合（SQL 段は RLS の正規経路で、直接 API 段は明示的な部分集合
    // として、同じ規則〔`doc_id % denominator == 0`〕をどちらも使う。索引は
    // 常に全件〔`doc_refs`／`SparseIndex::build`〕から構築し、可視率が縮小
    // させるのは「クエリが渡す候補集合」のみである点が Issue #547 の測定条件）。
    let visible_row_ids: BTreeSet<u64> = select_visible_ids(num_docs, visible_ratio_denominator);
    // 直接 API 段（B0s/B0/B4/B5/B8）が `SearchInput`／`search_within` へ渡す
    // 可視部分集合。`corpus.ids`（0 始まり連番）と同じ並び順を保つため、
    // フィルタだけで id・vector・body が引き続き位置対応する。
    let visible_ids: Vec<u64> = corpus
        .ids
        .iter()
        .copied()
        .filter(|id| visible_row_ids.contains(id))
        .collect();
    let visible_vectors: Vec<f32> = corpus
        .ids
        .iter()
        .enumerate()
        .filter(|(_, id)| visible_row_ids.contains(id))
        .flat_map(|(i, _)| corpus.vectors[i * DIM..(i + 1) * DIM].iter().copied())
        .collect();
    let visible_bodies: Vec<String> = corpus
        .ids
        .iter()
        .enumerate()
        .filter(|(_, id)| visible_row_ids.contains(id))
        .map(|(i, _)| corpus.bodies[i].clone())
        .collect();
    if visible_ids.len() != expected_visible {
        fail_closed(format!(
            "select_visible_ids produced {} ids, expected {expected_visible} \
             (rows={num_docs} denominator={visible_ratio_denominator})",
            visible_ids.len()
        ));
    }

    // --- Issue #389: SparseIndex 常駐時の常駐メモリ（RSS）増分 -----------------
    // プロセス内でこれが最初かつ唯一の `SparseIndex::build` 呼び出しになるよう、
    // 複製実装（tokenize/term_freq/doc_freq の累積 3 段）の構造的整合性チェック
    // （`harness::hybrid_profile::build_actually_succeeds` ドキュメント参照）を
    // この 1 回の構築で兼務させる（codex-review 指摘・Cursor Bugbot 指摘・PR #424:
    // 整合性チェック用に別途 `SparseIndex::build` を呼んで破棄すると、その確保・
    // 解放でアロケータ／ページがウォームになり、続く RSS 計測が「未ウォーム状態
    // からの増分」にならず過小評価しうる。整合性チェックの目的〔複製実装の転記
    // ミス検出〕は「同一入力で `SparseIndex::build` 自体が成功するか」の確認に
    // 尽きるため、ここで構築したインデックスの `is_ok()` をそのままその判定に
    // 使い、計測対象としても保持し続ければ二重構築を避けられる）。また
    // `core.execute_sql` も未呼び出し、つまり `sql/sparse_cache.rs::
    // SparseIndexCache`（Issue #357）経由の構築機会もまだ無い時点で計測する
    // （そのため本計測は SQL 実行（テーブル作成・投入・COUNT(*)・hybrid/dense
    // いずれのクエリも含む）より前、コーパス生成直後に置く）。
    // `SparseIndex` を保持したまま前後の VmRSS を比較する（保持しなければ
    // drop されて増分を観測できない）。`approx_heap_bytes()` はテスト・
    // ベンチ以外の一般利用側（`sql/sparse_cache.rs`）が実際に参照する
    // 概算値であり、RSS 実測と並記することで概算の妥当性を突き合わせられる。
    let vm_rss_kb_before = harness::proc_stats::read_vm_rss_kb();
    let resident_index = SparseIndex::build(&doc_refs).unwrap_or_else(|e| {
        fail_closed(format!(
            "SparseIndex::build failed for the generated corpus \
             (replication integrity check via memory-measurement build): {e}"
        ))
    });
    let approx_heap_bytes = resident_index.approx_heap_bytes();
    let vm_rss_kb_after = harness::proc_stats::read_vm_rss_kb();
    let vm_hwm_kb = harness::proc_stats::read_vm_hwm_kb();
    println!(
        "{}",
        harness::hybrid_profile::render_memory_line(
            approx_heap_bytes,
            vm_rss_kb_before,
            vm_rss_kb_after,
            vm_hwm_kb,
        )
    );
    // 位置つき posting（Issue #2114）による追加ぶんの内訳。docs
    // `sparse-inverted-index.md` の実測記録の再現用で、既存の memory 行の形式は変えない。
    let position_heap_bytes = resident_index.approx_position_heap_bytes();
    println!(
        "hybrid_profile: memory stage=sparse_index_positions position_heap_bytes={} \
         approx_heap_bytes_without_positions={}",
        position_heap_bytes,
        approx_heap_bytes.saturating_sub(position_heap_bytes),
    );
    // `resident_index` は RSS 差分計測の対象そのものであり、以降の段では参照
    // しないため、計測直後に明示的に drop してよい（メモリ計測意図の明確化）。
    drop(resident_index);

    // --- SQL 段用の一時 DB へ投入 ---
    let path = unique_db_path("issue356-hybrid-profile");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new(VECTOR_COLUMN, ColumnType::Vector(DIM as u32), false),
            ColumnDef::new(TEXT_COLUMN, ColumnType::Text, false),
        ],
    );
    storage
        .create_table(&schema)
        .expect("create table for bench seeding");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant id");
    let mut next_id: usize = 0;
    while next_id < num_docs {
        let batch_len = SEED_BATCH_ROWS.min(num_docs - next_id);
        let mut metadata_batch: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
        for i in next_id..next_id + batch_len {
            let encoded = encode_scalar_columns(
                &schema,
                &[Value::Null, Value::Text(corpus.bodies[i].clone())],
            )
            .expect("encode scalar columns for bench seeding");
            metadata_batch.push(encoded);
        }
        let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
            .map(|i| {
                let global = next_id + i;
                let start = global * DIM;
                let id = corpus.ids[global];
                // 可視率 100%（既定・`visible_ratio_denominator == 1`）では従来
                // どおり全行 Public。< 100% の条件は、RLS の正規経路（テナント内
                // `Visibility::Private` 行は `PolicyContext::new` の Public-only
                // ctx から不可視）で可視率を実現する（別テナント方式より
                // 「可視率」の意味に忠実。Issue #547 計画参照）。
                let visibility = if visible_row_ids.contains(&id) {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                (
                    id,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility,
                        embedding: &corpus.vectors[start..start + DIM],
                        metadata: &metadata_batch[i],
                    },
                )
            })
            .collect();
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
            "hybrid-profile-seed-batch-{next_id}"
        ))
        .expect("valid operation_id");
        engine::tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id)
            .expect("seed batch insert");
        next_id += batch_len;
    }

    // Issue #660 レビュー指摘（PR #668 codex-review P2）対応: S8 に id 投影相当の
    // 処理を含めるため、`storage`（この直後 `EngineCore::from_storage` へ move
    // する）から一度だけ実 `VectorArena` を構築して退避する。production の
    // `VectorArena::build`（TABLE-8）と同一の構築経路であり、以降 S8 計測の
    // `engine::sql::exec::project_id_only_rows`（`bench-internals` feature 限定）
    // へ渡す。
    let issue660_s8_arena = VectorArena::build(&storage, TABLE)
        .unwrap_or_else(|e| fail_closed(format!("Issue #660 S8 arena build failed: {e}")));

    // --- Issue #660: 双子 DB（同一スキーマ・0 行）の準備 ------------------------
    // S1(parse)/S2(schema)/S3(bind) はパース・束縛コストが行数に依存しない
    // （`validate_sql` はテーブル存在確認のみ、`bind_in_session` は列解決のみで
    // 行データを見ない）ことを利用し、可視全行を持つ `storage`（この直後
    // `EngineCore::from_storage` へ move する）とは別の、同一スキーマ・0 行の
    // 「双子 DB」で計測する。move 前に実 DB で 1 回だけ束縛して `BoundStatement`
    // を退避し、双子 DB での束縛結果と構造的に一致することを fail-closed に
    // 確認する（§3.3: 忠実性検証。`BoundStatement` は `PartialEq` を導出済み）。
    let issue660_fidelity_query = &queries[0];
    let issue660_fidelity_sql = sql_hybrid_statement_with_projection(
        TABLE,
        VECTOR_COLUMN,
        TEXT_COLUMN,
        &issue660_fidelity_query.vector,
        &issue660_fidelity_query.text,
        TOP_K,
        HybridProjection::Id,
    )
    .unwrap_or_else(|e| fail_closed(format!("Issue #660 fidelity statement build failed: {e}")));
    let issue660_session = SessionState::default();
    let issue660_real_validated = match validate_sql(&issue660_fidelity_sql, &storage)
        .unwrap_or_else(|e| fail_closed(format!("Issue #660 real DB validate_sql failed: {e}")))
    {
        SqlStatement::Select(validated) => validated,
        other => fail_closed(format!(
            "Issue #660 fidelity statement is not a SELECT (unexpected variant): {other:?}"
        )),
    };
    let issue660_real_bound = bind_in_session(
        &issue660_real_validated,
        &schema,
        issue660_session.search_mode(),
        issue660_session.udfs(),
    )
    .unwrap_or_else(|e| fail_closed(format!("Issue #660 real DB bind_in_session failed: {e}")));

    let issue660_twin_path = unique_db_path("issue660-hybrid-profile-twin");
    let _issue660_twin_guard = CleanupGuard(issue660_twin_path.clone());
    let issue660_twin_storage =
        Storage::open(&issue660_twin_path).expect("open twin storage for Issue #660");
    issue660_twin_storage
        .create_table(&schema)
        .expect("create twin table for Issue #660");
    let issue660_twin_validated = match validate_sql(&issue660_fidelity_sql, &issue660_twin_storage)
        .unwrap_or_else(|e| fail_closed(format!("Issue #660 twin DB validate_sql failed: {e}")))
    {
        SqlStatement::Select(validated) => validated,
        other => fail_closed(format!(
            "Issue #660 twin fidelity statement is not a SELECT (unexpected variant): {other:?}"
        )),
    };
    let issue660_twin_bound = bind_in_session(
        &issue660_twin_validated,
        &schema,
        issue660_session.search_mode(),
        issue660_session.udfs(),
    )
    .unwrap_or_else(|e| fail_closed(format!("Issue #660 twin DB bind_in_session failed: {e}")));
    if issue660_real_bound != issue660_twin_bound {
        fail_closed(
            "Issue #660 fidelity check failed: twin DB BoundStatement does not match real DB \
             (schema drift between measurement DB and twin DB) — S1/S2/S3 would not be \
             representative of the real bind path",
        );
    }

    let core = EngineCore::from_storage(storage, search_engine::default_engine());

    // 可視件数の突き合わせ（RLS 経由の `COUNT(*)` は可視行のみを数えるため、
    // 可視率 100% では投入行数と一致し、< 100% では `expected_visible_count`
    // と一致するはずである。不一致は構成ミス〔テーブル定義・投入経路・
    // 可視率規則の不整合〕を示すため fail-closed に打ち切る）。
    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let count_result = core
        .execute_sql(&ctx, &count_sql)
        .unwrap_or_else(|e| fail_closed(format!("COUNT(*) query failed: {e}")));
    let visible_count = match count_result.rows.first().and_then(|row| row.cells.first()) {
        Some(engine::sql::exec::Cell::Integer(n)) => *n,
        other => fail_closed(format!("unexpected COUNT(*) result shape: {other:?}")),
    };
    if visible_count != expected_visible as u64 {
        fail_closed(format!(
            "visible row count mismatch: expected {expected_visible}, got {visible_count} \
             (rows={num_docs} denominator={visible_ratio_denominator})"
        ));
    }
    println!(
        "hybrid_profile: rows={num_docs} visible_ratio=1/{visible_ratio_denominator} \
         visible_count={visible_count} dim={DIM}"
    );

    let config = MeasurementConfig::new(20, 30, SEED).expect("protocol minimums satisfied");

    // --- SQL レベル: sql_hybrid / sql_dense_knn ---
    let mut query_idx = 0usize;
    let sql_hybrid_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        let sql =
            sql_hybrid_statement(TABLE, VECTOR_COLUMN, TEXT_COLUMN, &q.vector, &q.text, TOP_K)
                .unwrap_or_else(|e| fail_closed(format!("sql_hybrid_statement failed: {e}")));
        core.execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| fail_closed(format!("sql_hybrid execute_sql failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sql_hybrid measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&sql_hybrid_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "sql_hybrid",
            sql_hybrid_measurement.summary.median.as_micros(),
            p95.as_micros(),
            num_docs,
        )
    );

    let mut query_idx = 0usize;
    let sql_dense_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        let sql = sql_dense_statement(TABLE, VECTOR_COLUMN, &q.vector, TOP_K)
            .unwrap_or_else(|e| fail_closed(format!("sql_dense_statement failed: {e}")));
        core.execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| fail_closed(format!("sql_dense_knn execute_sql failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sql_dense_knn measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&sql_dense_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "sql_dense_knn",
            sql_dense_measurement.summary.median.as_micros(),
            p95.as_micros(),
            num_docs,
        )
    );

    // --- コンポーネントレベル（直接 API。SQL パース・テーブル走査を含まない） ---

    // `sql/exec.rs::on_visible_row` は可視行の本文のみ蓄積するため、本複製も
    // 可視部分集合（`visible_ids`/`visible_bodies`）だけを対象にする（Issue #547。
    // `harness::hybrid_profile` モジュールドキュメント「可視率の意味」参照）。
    let collect_measurement = run(&config, || {
        collect_body_strings(&visible_ids, &visible_bodies)
    })
    .unwrap_or_else(|e| fail_closed(format!("collect_body_strings measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&collect_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "collect_body_strings",
            collect_measurement.summary.median.as_micros(),
            p95.as_micros(),
            visible_ids.len(),
        )
    );

    let build_measurement = run(&config, || {
        SparseIndex::build(&doc_refs)
            .unwrap_or_else(|e| fail_closed(format!("SparseIndex::build failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sparse_build_total measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&build_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "sparse_build_total",
            build_measurement.summary.median.as_micros(),
            p95.as_micros(),
            num_docs,
        )
    );

    let tokenize_measurement = run(&config, || tokenize_only(&corpus.bodies))
        .unwrap_or_else(|e| fail_closed(format!("tokenize_only measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&tokenize_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let tokenize_check = tokenize_only(&corpus.bodies);
    println!(
        "{}",
        render_stage_line(
            "tokenize_only",
            tokenize_measurement.summary.median.as_micros(),
            p95.as_micros(),
            tokenize_check,
        )
    );

    let term_freq_measurement = run(&config, || tokenize_term_freq(&corpus.bodies))
        .unwrap_or_else(|e| fail_closed(format!("tokenize_term_freq measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&term_freq_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let term_freq_check = tokenize_term_freq(&corpus.bodies);
    println!(
        "{}",
        render_stage_line(
            "tokenize_term_freq",
            term_freq_measurement.summary.median.as_micros(),
            p95.as_micros(),
            term_freq_check,
        )
    );

    let term_doc_freq_measurement = run(&config, || tokenize_term_doc_freq(&corpus.bodies))
        .unwrap_or_else(|e| fail_closed(format!("tokenize_term_doc_freq measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&term_doc_freq_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let term_doc_freq_check = tokenize_term_doc_freq(&corpus.bodies);
    println!(
        "{}",
        render_stage_line(
            "tokenize_term_doc_freq",
            term_doc_freq_measurement.summary.median.as_micros(),
            p95.as_micros(),
            term_doc_freq_check,
        )
    );

    println!(
        "hybrid_profile: check_values tokenize_total_tokens={tokenize_check} \
         tokenize_term_freq_total_unique_terms={term_freq_check} \
         tokenize_term_doc_freq_vocab_size={term_doc_freq_check}"
    );

    // --- Issue #387: キャッシュヒット後（Issue #357）になお残る search_within ---
    // --- 単体コスト・疎側再取得ループの寄与 -----------------------------------

    let sparse_index = SparseIndex::build(&doc_refs)
        .unwrap_or_else(|e| fail_closed(format!("SparseIndex::build (Issue #387) failed: {e}")));
    let replica = ProfileSparseIndex::build(&doc_refs)
        .unwrap_or_else(|e| fail_closed(format!("ProfileSparseIndex::build failed: {e}")));
    let visible: BTreeSet<u64> = visible_row_ids.clone();
    let pool_depth = SQL_DEFAULT_HYBRID_POOL_DEPTH;
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, pool_depth)
        .unwrap_or_else(|e| fail_closed(format!("RrfConfig::new failed: {e:?}")));

    // 疎側再取得スケジュールの収集（Issue #387 PR #416 codex-review P1 指摘
    // 対応）: `sparse_refetch_schedule` は production の疎側再取得ループ実装
    // （`hybrid.rs::sparse_refetch_loop`）をテスト・ベンチ向け公開フック
    // `engine::hybrid::sparse_refetch_observed` 経由でそのまま呼び出すため、
    // `schedule.fetch_ks` は予測・複製ではなく実際に発火した `fetch_k` の列
    // そのものである（`hybrid_search_boosted` が内部で呼ぶのと同一の
    // `sparse_index`・`query_text`・`visible_ids`・`cfg.pool_depth()` を渡して
    // 同じ決定的コードパスを実行するため、別途の呼び出し回数突き合わせ・終端
    // 安定性検証は不要。以前はここで境界同点判定〔`boundary_tie_decision`〕を
    // ベンチ側で複製・予測しており、複製の追随漏れリスクを終端固定点検証で
    // 間接的に補っていたが、実観測へ切り替えたことでその補償自体が不要になった）。
    //
    // 忠実性検証（fail-closed）: `search_within` 内部の subset/df/score 3 区間
    // 分解（後段の `search_within_subset_only`/`_subset_df`/`_replica_full`）に
    // 使う複製 `ProfileSparseIndex` の出力が、上記スケジュール上で実際に呼ばれる
    // 各 `fetch_k` について実 `SparseIndex::search_within` の出力と数値一致する
    // ことを起動時に確認する（`harness::hybrid_profile` モジュールドキュメント
    // 「複製固有の限界」参照）。
    let mut sparse_schedules = Vec::with_capacity(queries.len());
    for q in &queries {
        let schedule = sparse_refetch_schedule(&sparse_index, &q.text, &visible, pool_depth)
            .unwrap_or_else(|e| fail_closed(format!("sparse_refetch_schedule failed: {e}")));
        for &fetch_k in &schedule.fetch_ks {
            replica_matches_real(&sparse_index, &replica, &q.text, fetch_k, &visible)
                .unwrap_or_else(|e| fail_closed(format!("replica fidelity check failed: {e}")));
        }
        sparse_schedules.push(schedule);
    }

    // 密側の忠実性検証: 複製予測（dense_refetch_schedule）の呼び出し回数と、
    // 実 hybrid_search 呼び出し時に RefetchTrackingProvider が観測した回数を
    // 突き合わせる。
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    for (idx, q) in queries.iter().enumerate() {
        let predicted = dense_refetch_schedule(
            &provider,
            &visible_ids,
            &visible_vectors,
            corpus.dim,
            &q.vector,
            pool_depth,
        )
        .unwrap_or_else(|e| fail_closed(format!("dense_refetch_schedule failed: {e}")));
        provider.reset();
        let input = SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(&provider, input, &sparse_index, &q.text, TOP_K, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search (fidelity pass) failed: {e}")));
        refetch_schedule_matches_observed_calls(idx, &predicted, provider.calls())
            .unwrap_or_else(|e| fail_closed(format!("dense refetch fidelity check failed: {e}")));
    }
    println!(
        "hybrid_profile: fidelity checks passed (sparse_refetch_schedule now calls production's \
         shared sparse_refetch_loop via engine::hybrid::sparse_refetch_observed, so its \
         fetch_ks are an actual observation of hybrid_search_boosted's sparse refetch calls, \
         not a prediction — no separate call-count cross-check is needed for it; replica \
         search_within matches real API for every fetch_k on that schedule (used by the \
         subset/df/score breakdown below); dense refetch schedule predictions still match \
         observed hybrid_search calls via RefetchTrackingProvider)"
    );

    // --- hybrid_search_cached_index: 事前構築済み SparseIndex（キャッシュヒット
    // 相当）を使った直接 API 呼び出し。計測区間（timed pass）は素の
    // `ParallelSearchProvider` を使う（`RefetchTrackingProvider` は呼び出しの
    // たびに atomic な呼び出し回数・最大 k 更新を行うため、計測区間へ混ぜると
    // p95/median にその分のオーバーヘッドが混入する。codex-review 指摘・
    // Issue #387 PR #416）。呼び出し回数・最大 k の統計は別パス（stats pass。
    // `hybrid_latency_bench.rs::measure_stage` と同じ「計測区間内では統計蓄積を
    // 行わない」方針）で `RefetchTrackingProvider` を使って集計する。
    let timed_provider = ParallelSearchProvider;
    let mut query_idx = 0usize;
    let hybrid_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        let input = SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(&timed_provider, input, &sparse_index, &q.text, TOP_K, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search (timed) failed: {e}")))
    })
    .unwrap_or_else(|e| {
        fail_closed(format!(
            "hybrid_search_cached_index measurement failed: {e}"
        ))
    });
    let p95 = harness::accept::p95_from_samples(&hybrid_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));

    let mut dense_stats = Vec::with_capacity(queries.len());
    for q in &queries {
        provider.reset();
        let input = SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(&provider, input, &sparse_index, &q.text, TOP_K, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search (stats pass) failed: {e}")));
        dense_stats.push(harness::hybrid_latency::aggregate_refetch_stats(
            provider.calls(),
            provider.max_k_seen(),
            visible_ids.len(),
        ));
    }
    let dense_summary = harness::hybrid_latency::summarize_refetch_stats(&dense_stats);
    println!(
        "{}",
        render_dense_refetch_line(
            "hybrid_search_cached_index",
            hybrid_measurement.summary.median.as_micros(),
            p95.as_micros(),
            &dense_summary,
        )
    );

    // --- score_within_once: `SparseIndex::score_within` 単体（Issue #392）。
    // クエリ 1 回分のスコア計算のみを round-robin クエリで実測する
    // （Top-k 選出〔`SparseScored::top`〕を含まない）。
    let mut query_idx = 0usize;
    let score_within_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        sparse_index
            .score_within(&q.text, &visible)
            .unwrap_or_else(|e| fail_closed(format!("score_within failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("score_within_once measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&score_within_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let check = sparse_index
        .score_within(&queries[0].text, &visible)
        .map(|scored| scored.len())
        .unwrap_or(0);
    println!(
        "{}",
        render_stage_line(
            "score_within_once",
            score_within_measurement.summary.median.as_micros(),
            p95.as_micros(),
            check,
        )
    );

    // --- sparse_refetch_loop: 疎側再取得ループ本体（`hybrid_search_boosted` が
    // 実際に呼ぶコードパスそのもの。Issue #392）の実測累積コスト。Issue #387
    // PR #416 で production 経路とテスト・ベンチ向け公開フック
    // `sparse_refetch_observed` が同一実装を共有する構成になったため、下記の
    // `search_within_fetch_k=<k>`＋`sparse_refetch_summary`（複数ラウンドの
    // median 合算による推定値）とは異なり本区間は実測値そのものである。
    let mut query_idx = 0usize;
    let sparse_refetch_loop_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        sparse_refetch_observed(&sparse_index, &q.text, &visible, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("sparse_refetch_observed failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sparse_refetch_loop measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&sparse_refetch_loop_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let check = sparse_refetch_observed(&sparse_index, &queries[0].text, &visible, &cfg)
        .map(|(hits, _limit, _fetch_ks)| hits.len())
        .unwrap_or(0);
    println!(
        "{}",
        render_stage_line(
            "sparse_refetch_loop",
            sparse_refetch_loop_measurement.summary.median.as_micros(),
            p95.as_micros(),
            check,
        )
    );

    // --- 疎側再取得スケジュールの出力 ---
    for (idx, schedule) in sparse_schedules.iter().enumerate() {
        println!("{}", render_sparse_refetch_line(idx, schedule));
    }
    let sparse_summary = summarize_sparse_refetch(&sparse_schedules);

    // --- search_within_fetch_k=<k>: 疎側再取得スケジュール上で実際に呼ばれる
    // 各 fetch_k について、実 search_within 単体を round-robin クエリで実測する。
    let mut union_fetch_ks: Vec<usize> = sparse_schedules
        .iter()
        .flat_map(|s| s.fetch_ks.iter().copied())
        .collect();
    union_fetch_ks.sort_unstable();
    union_fetch_ks.dedup();

    let mut median_by_fetch_k: std::collections::BTreeMap<usize, u128> =
        std::collections::BTreeMap::new();
    for &fetch_k in &union_fetch_ks {
        let mut query_idx = 0usize;
        let measurement = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            sparse_index
                .search_within(&q.text, fetch_k, &visible)
                .unwrap_or_else(|e| {
                    fail_closed(format!("search_within(fetch_k={fetch_k}) failed: {e}"))
                })
        })
        .unwrap_or_else(|e| {
            fail_closed(format!(
                "search_within_fetch_k={fetch_k} measurement failed: {e}"
            ))
        });
        let p95 = harness::accept::p95_from_samples(&measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
        let check = sparse_index
            .search_within(&queries[0].text, fetch_k, &visible)
            .map(|hits| hits.len())
            .unwrap_or(0);
        median_by_fetch_k.insert(fetch_k, measurement.summary.median.as_micros());
        println!(
            "{}",
            render_stage_line(
                &format!("search_within_fetch_k={fetch_k}"),
                measurement.summary.median.as_micros(),
                p95.as_micros(),
                check,
            )
        );
    }

    // 推定累積時間（codex-review P1 指摘対応。PR #416）: `median_by_fetch_k[k]`
    // は各 fetch_k を全クエリで round-robin 測定した**全クエリ混合集団**の実測
    // 中央値であり、特定クエリの実測値ではない。以下はその混合集団中央値を、
    // 最も再取得回数が多いクエリの実スケジュールに沿って合算した**推定値**
    // （クエリ別の真の累積コストの実測ではない。以前は「最悪ケース」＝
    // クエリ別実測であるかのように扱っていたが、実体は全クエリ混合中央値に
    // よる推定である。詳細は `render_sparse_refetch_summary_line` ドキュメント
    // 参照）。
    let estimated_worst_cumulative_mixed_median_us: u128 = sparse_schedules
        .iter()
        .map(|s| {
            s.fetch_ks
                .iter()
                .map(|k| median_by_fetch_k.get(k).copied().unwrap_or(0))
                .sum::<u128>()
        })
        .max()
        .unwrap_or(0);
    println!(
        "{}",
        render_sparse_refetch_summary_line(
            &sparse_summary,
            estimated_worst_cumulative_mixed_median_us
        )
    );

    // --- search_within 内部 3 区間（複製実装）---
    // codex-review P2 指摘対応（PR #416）: `subset_only`／`subset_df` は
    // `fetch_k` を受け取らず、可視集合サイズのみに依存するため k に対して
    // 不変である。以前は initial_k/final_k の k ループ内で再測定しラベルに
    // `k=<k>` を付けていたため、反復測定の揺らぎを fetch_k による差である
    // かのように誤読させていた。ここでは k ループの外で 1 回だけ測定し、
    // ラベルからも `k=` を外して k 非依存であることを明示する。k が実際に
    // 効く `search_within_replica_full` のみ initial_k/final_k の 2 点で
    // 計測するループに残す。
    let mut query_idx = 0usize;
    let subset_only_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        replica.subset_only(&q.text, &visible)
    })
    .unwrap_or_else(|e| fail_closed(format!("search_within_subset_only measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&subset_only_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "search_within_subset_only (k-independent)",
            subset_only_measurement.summary.median.as_micros(),
            p95.as_micros(),
            visible.len(),
        )
    );

    let mut query_idx = 0usize;
    let subset_df_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        replica.subset_df(&q.text, &visible)
    })
    .unwrap_or_else(|e| fail_closed(format!("search_within_subset_df measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&subset_df_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "search_within_subset_df (k-independent)",
            subset_df_measurement.summary.median.as_micros(),
            p95.as_micros(),
            visible.len(),
        )
    );

    let cap = fetch_cap(visible.len());
    let initial_k = initial_fetch_k(pool_depth, cap);
    let final_k = union_fetch_ks.last().copied().unwrap_or(initial_k);
    for &fetch_k in &[initial_k, final_k] {
        let mut query_idx = 0usize;
        let (replica_full_measurement, _retained) = run_bounded_retain(&config, 0, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            replica.search_within_replica(&q.text, fetch_k, &visible)
        })
        .unwrap_or_else(|e| {
            fail_closed(format!(
                "search_within_replica_full measurement failed: {e}"
            ))
        });
        let p95 = harness::accept::p95_from_samples(&replica_full_measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
        let check = replica
            .search_within_replica(&queries[0].text, fetch_k, &visible)
            .len();
        println!(
            "{}",
            render_stage_line(
                &format!("search_within_replica_full k={fetch_k}"),
                replica_full_measurement.summary.median.as_micros(),
                p95.as_micros(),
                check,
            )
        );
    }

    // =========================================================================
    // Issue #465: hybrid_rrf 最新基線のラウンド計測（Issue #392 適用後）
    // =========================================================================
    //
    // 上記の単一パス段（Issue #356〜#392）はいずれも 1 回分の warmup+計測のみで
    // あり、共有計測環境のノイズを考慮した交互複数ラウンド計測になっていない。
    // 本節は `docs/design/benchmark-judgement-policy.md` の計測規約（交互
    // N≥5・min-of-R と median-of-R の併記・参照区間帯併記）に沿って B0s〜B8 の
    // 段を `BENCH_HYBRID_PROFILE_ROUNDS` ラウンド交互計測し、帰属表（SQL 表層・
    // 投影・密・疎・残差の内訳）を出力する。既存の単一パス段は無変更のまま
    // 残す（Issue #356〜#392 節との比較可能性維持）。
    //
    // 参照区間には `CpuScalarProvider::search`（単線・決定的）を使う。
    // `ParallelSearchProvider`（B0）はスレッドスケジューリングに依存し
    // ラウンド間で振れやすいため参照区間には使わない
    // （`docs/design/knn-wire-stage-profile.md` T1′ と同じ理由）。
    let rounds_env = std::env::var("BENCH_HYBRID_PROFILE_ROUNDS").ok();
    let rounds = match parse_rounds(rounds_env.as_deref()) {
        Ok(r) => r,
        Err(ScanStageError::InvalidRounds(reason)) => {
            fail_closed(format!("invalid BENCH_HYBRID_PROFILE_ROUNDS: {reason}"))
        }
        Err(e) => fail_closed(format!(
            "BENCH_HYBRID_PROFILE_ROUNDS validation failed: {e}"
        )),
    };
    println!(
        "hybrid_profile: baseline round measurement (Issue #465) starting — rounds={rounds} \
         (set BENCH_HYBRID_PROFILE_ROUNDS=5..50 to override; set BENCH_DEDICATED_ENV=1 to \
         self-report a dedicated measurement environment)"
    );
    // 値の中身を確認せず存在のみで判定すると "0" や空文字でも専有環境扱いになる
    // ため、他ベンチ（sql_c1_bench.rs・*_wire_profile_bench.rs）と同じ厳密一致
    // 契約（trim 後 "1" のときのみ true）に揃える（codex-review P2 指摘・PR #556）。
    let dedicated_env = std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    if !dedicated_env {
        println!(
            "hybrid_profile: BENCH_DEDICATED_ENV not set — the following baseline numbers are \
             shared-environment reference values only, not a basis for accept/reject decisions \
             (docs/design/benchmark-judgement-policy.md)"
        );
    }

    let dense_fetch_k = initial_fetch_k(pool_depth, fetch_cap(visible.len()));
    let mut b0s_round_medians = Vec::with_capacity(rounds as usize);
    let mut b0_round_medians = Vec::with_capacity(rounds as usize);
    let mut b1_round_medians = Vec::with_capacity(rounds as usize);
    let mut b2_round_medians = Vec::with_capacity(rounds as usize);
    let mut b3_round_medians = Vec::with_capacity(rounds as usize);
    let mut b4_round_medians = Vec::with_capacity(rounds as usize);
    let mut b5_round_medians = Vec::with_capacity(rounds as usize);
    let mut b7_round_medians = Vec::with_capacity(rounds as usize);
    let mut b8_round_medians = Vec::with_capacity(rounds as usize);

    // Issue #549 参考値: B7_fuse_lower_bound（`rrf_fuse` 単体の下限近似）。密・疎
    // それぞれの Top-`pool_depth` 候補を計測外（ラウンドループの前）で事前に捕捉し、
    // 計測区間には `hybrid::rrf_fuse` の呼び出しのみを含める。境界同点グループ完全化
    // （Issue #310・#320）の再取得コストは含まない「融合コアだけの処理時間」の
    // 下限近似であり、B4（`hybrid_search`。再取得込みの実効値）と対にして参照する
    // （申し送り: 前後比較・採否は #550／#547 の担当。本ベンチは参考値の記録のみ）。
    let b7_precomputed: Vec<(Vec<CandidateHit>, Vec<ScoredDoc>)> = queries
        .iter()
        .map(|q| {
            // 可視部分集合（`visible_ids`/`visible_vectors`）を使う。B4
            // （`hybrid_search_cached_index`）と同じ候補集合を密側へ渡さないと、
            // B7 が可視率縮小時に B4 より広い母集団から Top-`pool_depth` を
            // 拾ってしまい、対比対象として不整合になる（codex-review 指摘）。
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: pool_depth,
            };
            let dense = ParallelSearchProvider
                .search(input)
                .unwrap_or_else(|e| fail_closed(format!("B7 precompute dense search failed: {e}")));
            let mut sparse = sparse_refetch_observed(&sparse_index, &q.text, &visible, &cfg)
                .unwrap_or_else(|e| {
                    fail_closed(format!("B7 precompute sparse_refetch_observed failed: {e}"))
                })
                .0;
            sparse.truncate(pool_depth);
            (dense, sparse)
        })
        .collect();

    // fail-closed 整合性検証: B1（SQL hybrid・SELECT id）が返す id 集合が
    // B4（直接 API・hybrid_search）と一致することを、ラウンドループの前に
    // 1 回（queries[0]）だけ確認する（計測区間には混ぜない）。SQL 表層と
    // 直接 API 呼び出しが同じ融合結果を返すことの構造的な裏付けであり、
    // 不一致は投影・束縛経路の不整合を示すため fail-closed に打ち切る。
    {
        let q = &queries[0];
        let sql = sql_hybrid_statement_with_projection(
            TABLE,
            VECTOR_COLUMN,
            TEXT_COLUMN,
            &q.vector,
            &q.text,
            TOP_K,
            HybridProjection::Id,
        )
        .unwrap_or_else(|e| fail_closed(format!("B1 fidelity statement build failed: {e}")));
        let result = core
            .execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| fail_closed(format!("B1 fidelity execute_sql failed: {e}")));
        let mut b1_ids: Vec<u64> = result.rows.iter().map(|row| row.id).collect();
        b1_ids.sort_unstable();
        let input = SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        let hits = hybrid_search(
            &ParallelSearchProvider,
            input,
            &sparse_index,
            &q.text,
            TOP_K,
            &cfg,
        )
        .unwrap_or_else(|e| fail_closed(format!("B4 fidelity hybrid_search failed: {e}")));
        let mut b4_ids: Vec<u64> = hits.iter().map(|hit| hit.id).collect();
        b4_ids.sort_unstable();
        if b1_ids != b4_ids {
            fail_closed(format!(
                "B1(sql_hybrid_select_id)/B4(hybrid_search_cached_index) id set mismatch: \
                 sql={b1_ids:?} direct={b4_ids:?}"
            ));
        }
        // 可視件数が TOP_K 未満の設定（`BENCH_HYBRID_PROFILE_VISIBLE_RATIO` で
        // 小規模コーパス×高い分母を指定した場合。公開されている入力範囲内）では
        // 返る行数が可視件数で頭打ちになるのが正当であり、常に TOP_K ちょうどを
        // 要求すると到達可能な入力で必ず fail-closed してしまう（codex-review
        // 指摘）。期待値は `TOP_K` と可視件数の小さい方とする。
        let expected_hits = TOP_K.min(visible.len());
        if b1_ids.len() != expected_hits {
            fail_closed(format!(
                "B1/B4 fidelity: expected {expected_hits} ids, got {}",
                b1_ids.len()
            ));
        }
    }

    // Issue #660 §3.3: 各ラウンドの B1（SQL hybrid・`SparseIndexCache`／
    // `SqlArenaCache`／`ScalarIndexCache`／`HnswIndexCache` の照会経路）計測
    // 直前・直後でのみ各キャッシュの統計を採取し、増分を B1 の実行回数
    // （warmup + measured。B1 は 1 回の `execute_sql` ごとに各キャッシュを
    // 高々 1 回照会する）と突き合わせる fail-closed 判定へ供する（codex-review
    // 指摘: B1〜B3 全体を跨ぐ前後比較では B1 単独の命中を証明できないため、
    // 採取区間を B1 の `run(...)` 呼び出しの直前・直後のみへ縮小した）。
    let issue660_b1_iterations_per_round =
        u64::from(config.warmup_iterations()) + u64::from(config.measured_iterations());
    let mut issue660_sql_arena_delta_sum = 0u64;
    let mut issue660_sparse_delta_sum = 0u64;
    let mut issue660_scalar_delta_sum = 0u64;
    let mut issue660_hnsw_delta_sum = 0u64;

    for round in 0..rounds {
        println!("hybrid_profile: baseline round {}/{rounds}", round + 1);

        // B0s_dense_scalar_ref: 参照区間（単線・決定的）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: dense_fetch_k,
            };
            CpuScalarProvider
                .search(input)
                .unwrap_or_else(|e| fail_closed(format!("B0s dense_scalar_ref failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B0s measurement failed: {e}")));
        b0s_round_medians.push(m.summary.median);

        // B0_dense_provider_pool: 密側の実効値（production 既定 provider）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: dense_fetch_k,
            };
            ParallelSearchProvider
                .search(input)
                .unwrap_or_else(|e| fail_closed(format!("B0 dense_provider_pool failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B0 measurement failed: {e}")));
        b0_round_medians.push(m.summary.median);

        // B1_sql_hybrid_select_id: crossdb 規範形（SQL 表層 e2e の上限）。
        // キャッシュ統計は本計測の直前・直後でのみ採取する（B1 単独の命中を
        // 証明するため。上記コメント参照）。
        let issue660_sql_arena_before = core.sql_arena_cache_stats();
        let issue660_sparse_before = core.sparse_index_cache_stats();
        let issue660_scalar_before = core.scalar_index_cache_stats();
        let issue660_hnsw_before = core.hnsw_index_cache_stats();
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let sql = sql_hybrid_statement_with_projection(
                TABLE,
                VECTOR_COLUMN,
                TEXT_COLUMN,
                &q.vector,
                &q.text,
                TOP_K,
                HybridProjection::Id,
            )
            .unwrap_or_else(|e| fail_closed(format!("B1 statement build failed: {e}")));
            core.execute_sql(&ctx, &sql)
                .unwrap_or_else(|e| fail_closed(format!("B1 execute_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B1 measurement failed: {e}")));
        b1_round_medians.push(m.summary.median);
        let issue660_sql_arena_after = core.sql_arena_cache_stats();
        let issue660_sparse_after = core.sparse_index_cache_stats();
        let issue660_scalar_after = core.scalar_index_cache_stats();
        let issue660_hnsw_after = core.hnsw_index_cache_stats();
        issue660_sql_arena_delta_sum += issue660_sql_arena_after
            .hits
            .saturating_sub(issue660_sql_arena_before.hits);
        issue660_sparse_delta_sum += issue660_sparse_after
            .hits
            .saturating_sub(issue660_sparse_before.hits);
        issue660_scalar_delta_sum += issue660_scalar_after
            .hits
            .saturating_sub(issue660_scalar_before.hits);
        issue660_hnsw_delta_sum += issue660_hnsw_after
            .hits
            .saturating_sub(issue660_hnsw_before.hits);

        // B2_sql_hybrid_select_star: 既存段と同じ投影（本文複製あり）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let sql =
                sql_hybrid_statement(TABLE, VECTOR_COLUMN, TEXT_COLUMN, &q.vector, &q.text, TOP_K)
                    .unwrap_or_else(|e| fail_closed(format!("B2 statement build failed: {e}")));
            core.execute_sql(&ctx, &sql)
                .unwrap_or_else(|e| fail_closed(format!("B2 execute_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B2 measurement failed: {e}")));
        b2_round_medians.push(m.summary.median);

        // B3_sql_dense_select_id: fast path 対照（informational）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let sql = sql_dense_statement_with_projection(
                TABLE,
                VECTOR_COLUMN,
                &q.vector,
                TOP_K,
                HybridProjection::Id,
            )
            .unwrap_or_else(|e| fail_closed(format!("B3 statement build failed: {e}")));
            core.execute_sql(&ctx, &sql)
                .unwrap_or_else(|e| fail_closed(format!("B3 execute_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B3 measurement failed: {e}")));
        b3_round_medians.push(m.summary.median);

        // B4_hybrid_search_cached_index: engine 内 hybrid 経路（直接 API）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: TOP_K,
            };
            hybrid_search(
                &ParallelSearchProvider,
                input,
                &sparse_index,
                &q.text,
                TOP_K,
                &cfg,
            )
            .unwrap_or_else(|e| fail_closed(format!("B4 hybrid_search failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B4 measurement failed: {e}")));
        b4_round_medians.push(m.summary.median);

        // B5_sparse_refetch_loop: 疎側再取得ループ本体（既存フック共用）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            sparse_refetch_observed(&sparse_index, &q.text, &visible, &cfg)
                .unwrap_or_else(|e| fail_closed(format!("B5 sparse_refetch_observed failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B5 measurement failed: {e}")));
        b5_round_medians.push(m.summary.median);

        // B7_fuse_lower_bound: `rrf_fuse` 単体（参考値。密・疎の取得コストは含まない
        // 下限近似。Issue #549）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let (dense, sparse) = &b7_precomputed[query_idx % b7_precomputed.len()];
            query_idx += 1;
            rrf_fuse(dense, sparse, &cfg)
                .unwrap_or_else(|e| fail_closed(format!("B7 rrf_fuse failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B7 measurement failed: {e}")));
        b7_round_medians.push(m.summary.median);

        // B8_visible_set_build: 残差内訳（可視集合 BTreeSet 構築。std 操作のみ）。
        let m = run(&config, || {
            visible_ids.iter().copied().collect::<BTreeSet<u64>>()
        })
        .unwrap_or_else(|e| fail_closed(format!("B8 measurement failed: {e}")));
        b8_round_medians.push(m.summary.median);
    }

    // --- per-round 生データ（計測規約 §3: per-run 生データ必須） ---
    for (round, (((((((b0s, b0), b1), b2), b3), b4), b5), b8)) in b0s_round_medians
        .iter()
        .zip(&b0_round_medians)
        .zip(&b1_round_medians)
        .zip(&b2_round_medians)
        .zip(&b3_round_medians)
        .zip(&b4_round_medians)
        .zip(&b5_round_medians)
        .zip(&b8_round_medians)
        .enumerate()
    {
        println!(
            "hybrid_profile: baseline_round_raw round={} B0s={}us B0={}us B1={}us B2={}us \
             B3={}us B4={}us B5={}us B8={}us",
            round + 1,
            b0s.as_micros(),
            b0.as_micros(),
            b1.as_micros(),
            b2.as_micros(),
            b3.as_micros(),
            b4.as_micros(),
            b5.as_micros(),
            b8.as_micros(),
        );
    }
    // B7 は Issue #549 で追加した参考値のため、上記の 8 段タプルとは別に単独ループで
    // per-round 生データを出力する（既存タプル連結を組み替えて破壊するリスクを避ける）。
    for (round, b7) in b7_round_medians.iter().enumerate() {
        println!(
            "hybrid_profile: baseline_round_raw_b7 round={} B7={}us",
            round + 1,
            b7.as_micros(),
        );
    }

    let ref_band_ratio = reference_band(&b0s_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("reference_band computation failed: {e}")));
    let ref_band_pct = ref_band_ratio * 100.0;
    println!(
        "hybrid_profile: baseline reference_band(B0s)={ref_band_pct:.2}% \
         (dedicated_env={dedicated_env})"
    );

    let min_b0s = min_of(&b0s_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B0s) failed: {e}")));
    let med_b0s = median_of(&b0s_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B0s) failed: {e}")));
    let min_b0 = min_of(&b0_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B0) failed: {e}")));
    let med_b0 = median_of(&b0_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B0) failed: {e}")));
    let min_b1 = min_of(&b1_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B1) failed: {e}")));
    let med_b1 = median_of(&b1_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B1) failed: {e}")));
    let min_b2 = min_of(&b2_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B2) failed: {e}")));
    let med_b2 = median_of(&b2_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B2) failed: {e}")));
    let min_b3 = min_of(&b3_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B3) failed: {e}")));
    let med_b3 = median_of(&b3_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B3) failed: {e}")));
    let min_b4 = min_of(&b4_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B4) failed: {e}")));
    let med_b4 = median_of(&b4_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B4) failed: {e}")));
    let min_b5 = min_of(&b5_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B5) failed: {e}")));
    let med_b5 = median_of(&b5_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B5) failed: {e}")));
    let min_b7 = min_of(&b7_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B7) failed: {e}")));
    let med_b7 = median_of(&b7_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B7) failed: {e}")));
    let min_b8 = min_of(&b8_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B8) failed: {e}")));
    let med_b8 = median_of(&b8_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B8) failed: {e}")));

    println!(
        "hybrid_profile: baseline_summary_b7 B7(min={}us,median={}us)",
        min_b7.as_micros(),
        med_b7.as_micros(),
    );

    println!(
        "hybrid_profile: baseline_summary B0s(min={}us,median={}us) B0(min={}us,median={}us) \
         B1(min={}us,median={}us) B2(min={}us,median={}us) B3(min={}us,median={}us) \
         B4(min={}us,median={}us) B5(min={}us,median={}us) B8(min={}us,median={}us)",
        min_b0s.as_micros(),
        med_b0s.as_micros(),
        min_b0.as_micros(),
        med_b0.as_micros(),
        min_b1.as_micros(),
        med_b1.as_micros(),
        min_b2.as_micros(),
        med_b2.as_micros(),
        min_b3.as_micros(),
        med_b3.as_micros(),
        min_b4.as_micros(),
        med_b4.as_micros(),
        min_b5.as_micros(),
        med_b5.as_micros(),
        min_b8.as_micros(),
        med_b8.as_micros(),
    );

    // --- 帰属表（min-of-R 基準。ratio_of_b1 は表示専用で B1 min に対する構成比を
    // 示すが、band 判定はこれと分離し docs/design/benchmark-judgement-policy.md
    // §4 の「before を分母とする」規約どおり、当該区間自身の before/after
    // （`step_ratio_pct(before, after)`）を用いる。比較元となる before/after の
    // 対を持たない単独区分（dense/sparse/residual 系・wire 側等）は band を
    // n/a とする ---
    let sql_surface_diff = bucket_diff(min_b4, min_b1); // B1 - B4
    let projection_diff = bucket_diff(min_b1, min_b2); // B2 - B1
    let residual_diff = {
        // B4 - B0 - B5（飽和差分の連鎖。どこかで逆転したら以降は None）
        bucket_diff(min_b0, min_b4).and_then(|d| bucket_diff(min_b5, d))
    };
    let residual_minus_visible_diff = residual_diff.and_then(|d| bucket_diff(min_b8, d));

    let b1_us = min_b1.as_micros().max(1) as f64;
    let render_bucket =
        |label: &str,
         diff: Option<std::time::Duration>,
         band_basis: Option<(std::time::Duration, std::time::Duration)>| {
            let diff_us = diff.map(|d| d.as_micros());
            let ratio_pct = diff_us.map(|us| (us as f64 / b1_us) * 100.0);
            let band = match band_basis {
                Some((before, after)) => match step_ratio_pct(before, after) {
                    Ok(pct) => classify_against_bands(pct, ref_band_pct).to_string(),
                    Err(_) => "n/a".to_string(),
                },
                None => "n/a".to_string(),
            };
            println!(
                "{}",
                render_baseline_bucket_line(label, diff_us, ratio_pct, &band)
            );
        };
    render_bucket(
        "sql_surface(B1-B4)",
        sql_surface_diff,
        Some((min_b4, min_b1)),
    );
    render_bucket("projection(B2-B1)", projection_diff, Some((min_b1, min_b2)));
    render_bucket("dense(B0)", Some(min_b0), None);
    render_bucket("sparse(B5)", Some(min_b5), None);
    render_bucket(
        "fuse_lower_bound(B7, informational, Issue #549)",
        Some(min_b7),
        None,
    );
    render_bucket("residual(B4-B0-B5)", residual_diff, None);
    render_bucket(
        "residual_minus_visible_set_build(B4-B0-B5-B8)",
        residual_minus_visible_diff,
        None,
    );
    render_bucket("visible_set_build(B8)", Some(min_b8), None);
    render_bucket(
        "dense_fast_path_contrast(B3, informational)",
        Some(min_b3),
        None,
    );

    println!(
        "hybrid_profile: baseline round measurement (Issue #465) done — see \
         docs/design/hybrid-rrf-latency-breakdown.md \"最新基線\" section for the transcribed \
         attribution table and top-2-stage identification handed to Issue #548"
    );

    // =========================================================================
    // Issue #660: SQL 表層固定コスト（B1-B4）の S0〜S8 再分解
    // =========================================================================
    //
    // 親 Issue #652・ルート #649。上記 B1（`sql_hybrid_select_id`）は hybrid
    // 経路が `cache_fast_path_eligible` の対象外（`sql/exec.rs`）であるため、
    // 疎索引キャッシュ（`SparseIndexCache`）がヒットしていても可視全行の行ループ
    // （`VectorArena::build_from_cached_rls_rows`）を再実行する。以下は
    // その内訳をパース・束縛・行ループ構成要素（複製近似）・末尾処理へ分解する。

    println!(
        "hybrid_profile: sql_surface_breakdown (Issue #660) starting — rounds={rounds} \
         (see docs/design/hybrid-rrf-latency-breakdown.md \"Issue #660\" section for the \
         attribution table transcribed from this run's output)"
    );

    // S1/S3 用のクエリ round-robin は B1 と同じ `query_idx % queries.len()` 規則。
    let issue660_sql_texts: Vec<String> = queries
        .iter()
        .map(|q| {
            sql_hybrid_statement_with_projection(
                TABLE,
                VECTOR_COLUMN,
                TEXT_COLUMN,
                &q.vector,
                &q.text,
                TOP_K,
                HybridProjection::Id,
            )
            .unwrap_or_else(|e| fail_closed(format!("Issue #660 S1 statement build failed: {e}")))
        })
        .collect();
    // S3(bind) 計測区間からパース自体のコストを除くため、`ValidatedStatement` は
    // 計測区間外で 1 回だけ構築して保持する（双子 DB に対して行う。パースは
    // テーブル存在確認のみで行数に依存しないため実 DB と等価）。
    let issue660_validated: Vec<_> = issue660_sql_texts
        .iter()
        .map(|sql| {
            match validate_sql(sql, &issue660_twin_storage).unwrap_or_else(|e| {
                fail_closed(format!("Issue #660 S3 precompute validate_sql failed: {e}"))
            }) {
                SqlStatement::Select(validated) => validated,
                other => fail_closed(format!(
                    "Issue #660 S3 precompute statement is not a SELECT (unexpected variant): {other:?}"
                )),
            }
        })
        .collect();

    // S1d/S3d 用（Issue #660 レビュー指摘対応）: `common_fixed(B3-S0)` は
    // dense SQL（B3 と同一文）の round trip から dense-topk 対照区間を引いた
    // 値であり、そこから引く parse・bind も dense 側で揃える必要がある。
    // 双子 DB 方式・round-robin 規則は S1/S3（hybrid）と同一で、文だけを
    // `sql_dense_statement_with_projection`（B3 と同じ構築関数・投影）に
    // 差し替える。S2（`get_table_schema`）は SQL 文の形状に依存しないため
    // dense 専用の複製は行わず、hybrid 側と同一の測定値を双方で共有する。
    let issue660_dense_sql_texts: Vec<String> = queries
        .iter()
        .map(|q| {
            sql_dense_statement_with_projection(
                TABLE,
                VECTOR_COLUMN,
                &q.vector,
                TOP_K,
                HybridProjection::Id,
            )
            .unwrap_or_else(|e| fail_closed(format!("Issue #660 S1d statement build failed: {e}")))
        })
        .collect();
    let issue660_dense_validated: Vec<_> = issue660_dense_sql_texts
        .iter()
        .map(|sql| {
            match validate_sql(sql, &issue660_twin_storage).unwrap_or_else(|e| {
                fail_closed(format!("Issue #660 S3d precompute validate_sql failed: {e}"))
            }) {
                SqlStatement::Select(validated) => validated,
                other => fail_closed(format!(
                    "Issue #660 S3d precompute statement is not a SELECT (unexpected variant): {other:?}"
                )),
            }
        })
        .collect();

    // S4/S5 用: 可視全行の事前エンコード済み metadata（`encode_scalar_columns` は
    // 一時 DB への投入時と同じ関数。redb からの読み出し自体は含まない）。
    let issue660_encoded_visible: Vec<Vec<u8>> = visible_bodies
        .iter()
        .map(|body| {
            encode_scalar_columns(&schema, &[Value::Null, Value::Text(body.clone())])
                .unwrap_or_else(|e| fail_closed(format!("Issue #660 S4 pre-encode failed: {e}")))
        })
        .collect();

    // S8 用: `hybrid_search`（B4 と同一経路）の Top-`TOP_K` hits を 1 回だけ
    // 事前計算し、`RlsSafetyNet::apply`（Top-k のみ）＋ id 投影相当の末尾処理
    // コストを密・疎の再取得コストと混ぜずに計測する（Issue #660 レビュー
    // 指摘・PR #668 対応）。`hybrid_search` が返す `HybridHit::id` は実際の行
    // `id`（`SearchInput::ids` に渡した `visible_ids` 由来）だが、production の
    // `RlsSafetyNet::apply`／`project_rows` が受け取る hits の第 1 要素は
    // アリーナのスロット番号（`sql/exec.rs` のモジュールドキュメント参照）
    // であり、両者は一致しない。ここで一度だけ `issue660_s8_arena` 上の
    // id→スロット写像を作り、hits をスロット番号へ変換してから測定ラウンドへ
    // 渡す（変換自体は計測区間の外）。
    let issue660_id_to_slot: HashMap<u64, u32> = issue660_s8_arena
        .ids()
        .iter()
        .enumerate()
        .map(|(slot, id)| (*id, slot as u32))
        .collect();
    let issue660_s8_hits: Vec<(u64, f64)> = {
        let q = &queries[0];
        let input = SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(
            &ParallelSearchProvider,
            input,
            &sparse_index,
            &q.text,
            TOP_K,
            &cfg,
        )
        .unwrap_or_else(|e| {
            fail_closed(format!(
                "Issue #660 S8 precompute hybrid_search failed: {e}"
            ))
        })
        .into_iter()
        .map(|hit| {
            let slot = *issue660_id_to_slot.get(&hit.id).unwrap_or_else(|| {
                fail_closed(format!(
                    "Issue #660 S8 precompute: hit id {} missing from arena slot map",
                    hit.id
                ))
            });
            (slot as u64, hit.score)
        })
        .collect()
    };

    let mut s0_round_medians = Vec::with_capacity(rounds as usize);
    let mut s1_round_medians = Vec::with_capacity(rounds as usize);
    let mut s2_round_medians = Vec::with_capacity(rounds as usize);
    let mut s3_round_medians = Vec::with_capacity(rounds as usize);
    // S1d/S3d（Issue #660 レビュー指摘対応）: dense SQL（B3 と同一文）の
    // parse・bind。`common_fixed(B3-S0)` の減算対象を hybrid ではなく dense
    // 側の parse・bind へ揃えるために測定する。
    let mut s1d_round_medians = Vec::with_capacity(rounds as usize);
    let mut s3d_round_medians = Vec::with_capacity(rounds as usize);
    let mut s4_round_medians = Vec::with_capacity(rounds as usize);
    let mut s5_round_medians = Vec::with_capacity(rounds as usize);
    let mut s6_round_medians = Vec::with_capacity(rounds as usize);
    let mut s7_round_medians = Vec::with_capacity(rounds as usize);
    let mut s8_round_medians = Vec::with_capacity(rounds as usize);

    for round in 0..rounds {
        println!(
            "hybrid_profile: sql_surface_breakdown round {}/{rounds} (Issue #660)",
            round + 1
        );

        // S0_dense_topk_ref: B3（fast path 対照）の減算対象。B0 は k=dense_fetch_k
        // だが B3 は LIMIT TOP_K のため、k を揃えた対照区間を別途持つ。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: TOP_K,
            };
            ParallelSearchProvider
                .search(input)
                .unwrap_or_else(|e| fail_closed(format!("S0 dense_topk_ref failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("S0 measurement failed: {e}")));
        s0_round_medians.push(m.summary.median);

        // S1_parse: 字句解析＋許可リスト構文解析＋テーブル存在確認（双子 DB）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let sql = &issue660_sql_texts[query_idx % issue660_sql_texts.len()];
            query_idx += 1;
            validate_sql(sql, &issue660_twin_storage)
                .unwrap_or_else(|e| fail_closed(format!("S1 validate_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("S1 measurement failed: {e}")));
        s1_round_medians.push(m.summary.median);

        // S2_schema: テーブル定義読み出し（`core::read_txn_with_schema` の
        // `pub(crate)` 非公開分を `Storage::get_table_schema` で近似）。
        let m = run(&config, || {
            issue660_twin_storage
                .get_table_schema(TABLE)
                .unwrap_or_else(|e| fail_closed(format!("S2 get_table_schema failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("S2 measurement failed: {e}")));
        s2_round_medians.push(m.summary.median);

        // S3_bind: 列解決・投影構築（`ValidatedStatement` は計測区間外で構築済み）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let validated = &issue660_validated[query_idx % issue660_validated.len()];
            query_idx += 1;
            bind_in_session(
                validated,
                &schema,
                issue660_session.search_mode(),
                issue660_session.udfs(),
            )
            .unwrap_or_else(|e| fail_closed(format!("S3 bind_in_session failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("S3 measurement failed: {e}")));
        s3_round_medians.push(m.summary.median);

        // S1d_parse_dense: S1 と同じ双子 DB 方式・round-robin 規則で、文だけを
        // dense SQL（B3 と同一文）へ差し替えたパース計測（Issue #660）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let sql = &issue660_dense_sql_texts[query_idx % issue660_dense_sql_texts.len()];
            query_idx += 1;
            validate_sql(sql, &issue660_twin_storage)
                .unwrap_or_else(|e| fail_closed(format!("S1d validate_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("S1d measurement failed: {e}")));
        s1d_round_medians.push(m.summary.median);

        // S3d_bind_dense: S3 と同じ規則で dense SQL の束縛を計測する
        // （`ValidatedStatement` は計測区間外で構築済み。Issue #660）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let validated = &issue660_dense_validated[query_idx % issue660_dense_validated.len()];
            query_idx += 1;
            bind_in_session(
                validated,
                &schema,
                issue660_session.search_mode(),
                issue660_session.udfs(),
            )
            .unwrap_or_else(|e| fail_closed(format!("S3d bind_in_session failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("S3d measurement failed: {e}")));
        s3d_round_medians.push(m.summary.median);

        // S4_scan_replica: `on_visible_row` の構造検証部の複製。
        let m = run(&config, || scan_replica(&schema, &issue660_encoded_visible))
            .unwrap_or_else(|e| fail_closed(format!("S4 measurement failed: {e}")));
        s4_round_medians.push(m.summary.median);

        // S5_rowcopy_replica: `push_visible_row`＋`on_visible_row` 末尾の複製。
        let m = run(&config, || {
            rowcopy_replica(&visible_ids, &visible_vectors, DIM, TENANT_ID)
        })
        .unwrap_or_else(|e| fail_closed(format!("S5 measurement failed: {e}")));
        s5_round_medians.push(m.summary.median);

        // S6_slotmap_replica: `slot_ids`／`visible_id_counts` 再構築の複製。
        let m = run(&config, || slotmap_replica(&visible_ids))
            .unwrap_or_else(|e| fail_closed(format!("S6 measurement failed: {e}")));
        s6_round_medians.push(m.summary.median);

        // S7_bodyclone_replica: `SELECT *` 投影が行う本文列複製の複製
        // （既存 `projection(B2-B1)` の実体と対にして読む）。
        let m = run(&config, || bodyclone_replica(&visible_bodies))
            .unwrap_or_else(|e| fail_closed(format!("S7 measurement failed: {e}")));
        s7_round_medians.push(m.summary.median);

        // S8_tail: `RlsSafetyNet::apply`（Top-k のみ）＋ id 投影の複製
        // （Issue #660 レビュー指摘・PR #668 対応。production `project_rows` の
        // `ProjectedColumn::Id` 経路と同一のスロット→行 id 解決・`ResultRow`／
        // `Cell` 構築を `project_id_only_rows` で実施する）。`label_of` は
        // `project_rows` 呼び出し前の `sql/exec.rs::execute_statement` と同じく
        // `issue660_s8_arena` から `tenant_id`／`visibility` を引く。
        let m = run(&config, || {
            let hits = issue660_s8_hits.clone();
            let verified = RlsSafetyNet::new(&ctx).apply(hits, |slot_id| {
                let slot = usize::try_from(slot_id).ok()?;
                let tenant = issue660_s8_arena.tenant_id(slot)?;
                let visibility = issue660_s8_arena.visibility(slot)?;
                Some((tenant, visibility))
            });
            let rows = engine::sql::exec::project_id_only_rows(verified, &issue660_s8_arena)
                .unwrap_or_else(|e| fail_closed(format!("S8 projection failed: {e}")));
            rows.len()
        })
        .unwrap_or_else(|e| fail_closed(format!("S8 measurement failed: {e}")));
        s8_round_medians.push(m.summary.median);
    }

    // --- per-round 生データ ---
    for round in 0..rounds as usize {
        let values: [(&str, u128); 11] = [
            ("S0", s0_round_medians[round].as_micros()),
            ("S1", s1_round_medians[round].as_micros()),
            ("S2", s2_round_medians[round].as_micros()),
            ("S3", s3_round_medians[round].as_micros()),
            ("S1d", s1d_round_medians[round].as_micros()),
            ("S3d", s3d_round_medians[round].as_micros()),
            ("S4", s4_round_medians[round].as_micros()),
            ("S5", s5_round_medians[round].as_micros()),
            ("S6", s6_round_medians[round].as_micros()),
            ("S7", s7_round_medians[round].as_micros()),
            ("S8", s8_round_medians[round].as_micros()),
        ];
        println!("{}", render_sql_surface_round_raw_line(round + 1, &values));
    }

    let issue660_min_s0 = min_of(&s0_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S0) failed: {e}")));
    let issue660_med_s0 = median_of(&s0_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S0) failed: {e}")));
    let issue660_min_s1 = min_of(&s1_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S1) failed: {e}")));
    let issue660_med_s1 = median_of(&s1_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S1) failed: {e}")));
    let issue660_min_s2 = min_of(&s2_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S2) failed: {e}")));
    let issue660_med_s2 = median_of(&s2_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S2) failed: {e}")));
    let issue660_min_s3 = min_of(&s3_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S3) failed: {e}")));
    let issue660_med_s3 = median_of(&s3_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S3) failed: {e}")));
    let issue660_min_s1d = min_of(&s1d_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S1d) failed: {e}")));
    let issue660_med_s1d = median_of(&s1d_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S1d) failed: {e}")));
    let issue660_min_s3d = min_of(&s3d_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S3d) failed: {e}")));
    let issue660_med_s3d = median_of(&s3d_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S3d) failed: {e}")));
    let issue660_min_s4 = min_of(&s4_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S4) failed: {e}")));
    let issue660_med_s4 = median_of(&s4_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S4) failed: {e}")));
    let issue660_min_s5 = min_of(&s5_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S5) failed: {e}")));
    let issue660_med_s5 = median_of(&s5_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S5) failed: {e}")));
    let issue660_min_s6 = min_of(&s6_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S6) failed: {e}")));
    let issue660_med_s6 = median_of(&s6_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S6) failed: {e}")));
    let issue660_min_s7 = min_of(&s7_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S7) failed: {e}")));
    let issue660_med_s7 = median_of(&s7_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S7) failed: {e}")));
    let issue660_min_s8 = min_of(&s8_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(S8) failed: {e}")));
    let issue660_med_s8 = median_of(&s8_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(S8) failed: {e}")));

    for (name, min_d, med_d) in [
        ("S0_dense_topk_ref", issue660_min_s0, issue660_med_s0),
        ("S1_parse", issue660_min_s1, issue660_med_s1),
        ("S2_schema", issue660_min_s2, issue660_med_s2),
        ("S3_bind", issue660_min_s3, issue660_med_s3),
        ("S1d_parse_dense", issue660_min_s1d, issue660_med_s1d),
        ("S3d_bind_dense", issue660_min_s3d, issue660_med_s3d),
        ("S4_scan_replica", issue660_min_s4, issue660_med_s4),
        ("S5_rowcopy_replica", issue660_min_s5, issue660_med_s5),
        ("S6_slotmap_replica", issue660_min_s6, issue660_med_s6),
        ("S7_bodyclone_replica", issue660_min_s7, issue660_med_s7),
        ("S8_tail", issue660_min_s8, issue660_med_s8),
    ] {
        println!(
            "{}",
            render_sql_surface_summary_line(name, min_d.as_micros(), med_d.as_micros())
        );
    }

    // --- §3.2 差分区分（min-of-R 基準。飽和差分・逆転は n/a） -------------------
    // 指摘（Issue #660 レビュー・PRRT_kwDOUAKASM6gV23s）: `common_fixed` は
    // B3（dense SQL）と S0（dense-topk 対照）の差であり dense 側の値である。
    // ここから引く parse・bind は同じく dense 側（S1d+S2+S3d）で揃える必要が
    // あり、hybrid 側（S1+S2+S3）を引くと hybrid 固有の構文・列解決コストの
    // 分だけ `unexplained_common` に混入してしまう。
    let issue660_common_fixed = bucket_diff(issue660_min_s0, min_b3); // B3 - S0
    let issue660_hybrid_only =
        bucket_diff(min_b4, min_b1) // B1 - B4
            .and_then(|d| issue660_common_fixed.and_then(|c| bucket_diff(c, d)));
    let issue660_parse_bind_sum = issue660_min_s1 + issue660_min_s2 + issue660_min_s3; // hybrid SQL・逆転しない加算のみ
    let issue660_parse_bind_dense_sum = issue660_min_s1d + issue660_min_s2 + issue660_min_s3d; // dense SQL（B3 と同一文）
    let issue660_parse_bind_hybrid_delta =
        bucket_diff(issue660_parse_bind_dense_sum, issue660_parse_bind_sum); // hybrid - dense
    let issue660_scalar_stage_replica = issue660_min_s4 + issue660_min_s5;
    let issue660_unexplained_common = issue660_common_fixed.and_then(|c| {
        bucket_diff(
            issue660_min_s1d
                + issue660_min_s2
                + issue660_min_s3d
                + issue660_min_s6
                + issue660_min_s8,
            c,
        )
    });
    let issue660_unexplained_hybrid =
        issue660_hybrid_only.and_then(|h| bucket_diff(issue660_scalar_stage_replica, h));

    let issue660_render_bucket = |label: &str, diff: Option<std::time::Duration>| {
        let diff_us = diff.map(|d| d.as_micros());
        let ratio_pct = diff_us.map(|us| (us as f64 / b1_us) * 100.0);
        println!(
            "{}",
            render_sql_surface_bucket_line(label, diff_us, ratio_pct)
        );
    };
    issue660_render_bucket("common_fixed(B3-S0)", issue660_common_fixed);
    issue660_render_bucket("hybrid_only((B1-B4)-common_fixed)", issue660_hybrid_only);
    issue660_render_bucket("parse_bind(S1+S2+S3)", Some(issue660_parse_bind_sum));
    issue660_render_bucket(
        "parse_bind_dense(S1d+S2+S3d)",
        Some(issue660_parse_bind_dense_sum),
    );
    // hybrid_only の内訳の一部として読む情報用の区分（hybrid_only の計算式自体は
    // 変更しない）。hybrid SQL が dense SQL（B3）比でどれだけ余分に
    // parse・bind コストを負うかを切り出す。
    issue660_render_bucket(
        "parse_bind_hybrid_delta(parse_bind-parse_bind_dense)",
        issue660_parse_bind_hybrid_delta,
    );
    issue660_render_bucket(
        "scalar_stage_replica(S4+S5)",
        Some(issue660_scalar_stage_replica),
    );
    issue660_render_bucket(
        "unexplained_common(common_fixed-(S1d+S2+S3d+S6+S8))",
        issue660_unexplained_common,
    );
    issue660_render_bucket(
        "unexplained_hybrid(hybrid_only-scalar_stage_replica)",
        issue660_unexplained_hybrid,
    );
    issue660_render_bucket("projection_replica(S7)", Some(issue660_min_s7));

    // --- キャッシュ照会の非 vacuous 検証（§3.3。B1 の `run(...)` 直前・直後
    // でのみ採取した各ラウンドの増分を合算し、B1 の総実行回数（warmup+measured
    // を rounds 回）と突き合わせる。B1 は 1 回の `execute_sql` ごとに sql_arena・
    // sparse を高々 1 回ずつ照会するため、cache-hot 経路が成立していれば増分は
    // 総実行回数と厳密に一致するはずで、それ未満なら B1 の一部が cache miss
    // 経路（あるいは無関係な照会が混入）していることを意味する。hnsw は既定
    // エンジンでは構造的に 0 回のはずであり、0 以外なら fail-closed。scalar は
    // 本クエリ形状では照会経路自体が対象外のため informational として増分のみ
    // 報告する） ---
    let issue660_b1_expected_total = u64::from(rounds) * issue660_b1_iterations_per_round;
    // `render_cache_stats_delta_line` の before/after 引数には、B1 ブラケット
    // 区間の増分合計のみを渡す（B1 の直前直後以外〔B2 等〕での照会増分を含む
    // 生の before/after を渡すと合計値と食い違うため。before=0・after=delta_sum
    // と読み替える）。
    println!(
        "{}",
        render_cache_stats_delta_line(
            "sql_arena_hits",
            0,
            issue660_sql_arena_delta_sum,
            issue660_b1_expected_total,
            issue660_sql_arena_delta_sum == issue660_b1_expected_total,
        )
    );
    println!(
        "{}",
        render_cache_stats_delta_line(
            "sparse_index_hits",
            0,
            issue660_sparse_delta_sum,
            issue660_b1_expected_total,
            issue660_sparse_delta_sum == issue660_b1_expected_total,
        )
    );
    println!(
        "{}",
        render_cache_stats_delta_line("scalar_index_hits", 0, issue660_scalar_delta_sum, 0, true)
    );
    println!(
        "{}",
        render_cache_stats_delta_line(
            "hnsw_index_hits",
            0,
            issue660_hnsw_delta_sum,
            0,
            issue660_hnsw_delta_sum == 0,
        )
    );
    if issue660_sql_arena_delta_sum != issue660_b1_expected_total
        || issue660_sparse_delta_sum != issue660_b1_expected_total
    {
        fail_closed(format!(
            "Issue #660 fail-closed: sql_arena/sparse cache hit counters (delta \
             sql_arena={issue660_sql_arena_delta_sum} sparse={issue660_sparse_delta_sum}) did \
             not match B1's total execution count ({issue660_b1_expected_total} = rounds * \
             (warmup+measured)) measured directly around each round's B1 run() call — the SQL \
             surface breakdown above would not be representative of the cache-hot path"
        ));
    }
    if issue660_hnsw_delta_sum != 0 {
        fail_closed(format!(
            "Issue #660 fail-closed: hnsw_index_cache hits grew by {issue660_hnsw_delta_sum} on \
             the default (non-HNSW) search engine — expected structurally 0"
        ));
    }

    println!(
        "hybrid_profile: sql_surface_breakdown (Issue #660) done — see \
         docs/design/hybrid-rrf-latency-breakdown.md \"Issue #660\" section for the \
         attribution table transcribed from this run's output"
    );

    println!(
        "hybrid_profile: done (see docs/design/hybrid-rrf-latency-breakdown.md for the \
         attribution table transcribed from this run's output)"
    );
}
