//! TASK-121 系（`tests/incremental_recall.rs`）の ANN opt-in 対応（Issue #412・R3。
//! ADR `docs/design/ann-index-adoption.md` B 案の受け入れ条件「TASK-121 系増分
//! 回帰の ANN 対応」）。
//!
//! 増分インデックス反映（TASK-120。ファイル形 `INSERT` → チャンク化 →
//! `Embedder` によるベクトル化 → 同一パス置換書き込み）が積み重なる各状態
//! （初回構築 → 未索引分 brute-force 併用〔overlay〕→ 差分比率超過による
//! 再構築）で、ANN opt-in（`RecallEngine::Hnsw` 相当。`EngineCore::
//! from_storage_with_engine` ＋ `sql::hnsw_cache::HnswIndexCache`）でも
//! 既定エンジン（twin core 対照）と同水準で新規チャンクへ到達できることを固定
//! する。判定の主軸は「置換後の新チャンクが検索結果に到達し旧チャンクが
//! 現れない」というランキング非依存の正解到達（決定的）と、既定エンジン対照
//! 自己検索 Recall@10（同一クエリでの結果一致率）。
//!
//! `HnswIndex::build_parallel` は `MIN_ROWS_PER_THREAD * 2`（2,048。
//! `docs/design/hnsw-parallel-build.md`）以上で並列構築へ切り替わり構築グラフが
//! run-to-run で変わりうるため、本テストの行数は
//! `[MIN_INDEXED_ROWS（1,024）, 2,048)` に収め逐次構築（決定的）を維持する
//! （`tests/hnsw_cache.rs`・`tests/fixtures/recall_engine.rs` と同じ fixture
//! 方針）。

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{Embedder, HashingEmbedder};
use engine::incremental::IncrementalConfig;
use engine::policy::PolicyContext;
use engine::search_engine;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 64;
/// `MIN_INDEXED_ROWS`（`sql::hnsw_cache.rs` の非公開定数）の値の複製
/// （`tests/hybrid_recall.rs::MIN_INDEXED_ROWS` 等と同じ値）。
const MIN_INDEXED_ROWS: usize = 1_024;
/// 1 ファイル = 1 チャンク（`lines_per_chunk = 2`・本文ちょうど 2 行）になるよう
/// 揃え、`FILE_COUNT` をそのまま索引行数として扱えるようにする。
/// `[MIN_INDEXED_ROWS, 2_048)` に収める。
const FILE_COUNT: usize = 1_100;

fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 2,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn documents_schema() -> TableSchema {
    TableSchema::new(
        "documents",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn file_path(i: usize) -> String {
    format!("docs/file_{i:04}.md")
}

/// `marker` を含む 2 行本文（`lines_per_chunk = 2` によりちょうど 1 チャンクに
/// なる）。`marker` はクエリ側の一致対象トークン。
fn file_body(i: usize, marker: &str) -> String {
    format!("content{i} filler line one\n{marker} filler line two")
}

fn insert_file_sql(path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO documents (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
}

/// [`EngineCore::execute_insert_sql_batch`]（TASK-122・INDEX-4）で `BatchLimits::
/// default().max_files_per_batch` 件ずつに分割して投入する（`FILE_COUNT` が
/// 単一バッチの上限を超えるため）。
fn seed_files(core: &EngineCore, ctx: &PolicyContext, marker: &str, op_prefix: &str) {
    let limits = BatchLimits::default();
    let sqls: Vec<String> = (0..FILE_COUNT)
        .map(|i| {
            insert_file_sql(
                &file_path(i),
                &file_body(i, marker),
                &format!("{op_prefix}-{i}"),
            )
        })
        .collect();
    for chunk in sqls.chunks(limits.max_files_per_batch) {
        let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
        core.execute_insert_sql_batch(ctx, &refs)
            .unwrap_or_else(|e| panic!("seed batch insert failed: {e}"));
    }
}

/// 同一パスの置換書き込み（TASK-120 の同一パス置換セマンティクス）。単一ファイル形
/// `execute_insert_sql` を使う（`execute_insert_sql_batch` の全文ファイル形要求とは
/// 独立に、置換 1 件ごとの意味論を明示するため単発 API を使う）。
fn replace_file(core: &EngineCore, ctx: &PolicyContext, i: usize, marker: &str, op_id: &str) {
    core.execute_insert_sql(
        ctx,
        &insert_file_sql(&file_path(i), &file_body(i, marker), op_id),
    )
    .unwrap_or_else(|e| panic!("replace file id={i} failed: {e}"));
}

fn query_vector_for(text: &str) -> Vec<f32> {
    let embedder = HashingEmbedder::new(DIM).expect("valid dim");
    embedder
        .embed_batch(&[text])
        .expect("embed ok")
        .into_iter()
        .next()
        .expect("one vector")
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

/// `marker` を含む上位 `k` 件の `path` を hybrid クエリ（`ORDER BY HYBRID(...)`）
/// で取得する。
fn hybrid_top_paths(core: &EngineCore, ctx: &PolicyContext, marker: &str, k: usize) -> Vec<String> {
    let query_vector = query_vector_for(marker);
    let sql = format!(
        "SELECT path FROM documents ORDER BY HYBRID(embedding, {}, body, '{}') LIMIT {k}",
        vec_literal(&query_vector),
        sql_escape(marker),
    );
    let result = core
        .execute_sql(ctx, &sql)
        .expect("hybrid query should succeed");
    result
        .rows
        .into_iter()
        .filter_map(|r| {
            r.cells.into_iter().find_map(|c| match c {
                engine::sql::exec::Cell::Text(s) => Some(s),
                _ => None,
            })
        })
        .collect()
}

/// `path` の現在の本文を `WHERE path = ..` で直接取得する（ランキングに依存
/// しない決定的な検証。同一パス置換のセマンティクス〔TASK-120〕は「旧チャンクの
/// 本文がもう存在しない」ことを意味するのであって、BM25/hybrid の順位圏外に
/// 押し出されることまでは保証しない——`content{i}` のような他の共通トークンが
/// 残っていれば依然として上位に現れうる。そのため負の主張〔旧チャンクが
/// 見えない〕は `tests/incremental_index.rs::
/// resend_does_not_touch_other_tenants_same_path_rows` と同じ直接照会方式で行う）。
fn body_for_path(core: &EngineCore, ctx: &PolicyContext, path: &str) -> String {
    let zero_vec = vec_literal(&vec![0.0f32; DIM as usize]);
    let sql = format!(
        "SELECT body FROM documents WHERE path = '{}' ORDER BY embedding <=> {zero_vec} LIMIT 1",
        sql_escape(path)
    );
    let result = core
        .execute_sql(ctx, &sql)
        .expect("path lookup should succeed");
    result
        .rows
        .into_iter()
        .next()
        .and_then(|r| {
            r.cells.into_iter().find_map(|c| match c {
                engine::sql::exec::Cell::Text(s) => Some(s),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// `sample` 件のファイル（`marker` で置換済み）について、自己検索（本文中の
/// `marker` をクエリにしてその `path` 自身が top-10 に現れるか）の到達率を返す。
fn self_retrieval_rate(
    core: &EngineCore,
    ctx: &PolicyContext,
    file_indices: &[usize],
    marker: &str,
) -> f64 {
    let mut hit = 0usize;
    for &i in file_indices {
        let want = file_path(i);
        let query_marker = format!("{marker} content{i}");
        let top = hybrid_top_paths(core, ctx, &query_marker, 10);
        if top.contains(&want) {
            hit += 1;
        }
    }
    hit as f64 / file_indices.len() as f64
}

/// ANN opt-in core（`RecallEngine::Hnsw` 相当）を新規 DB に構築する。
fn new_ann_core(dir: &std::path::Path) -> EngineCore {
    let storage = Storage::open(dir).expect("open storage");
    storage
        .create_table(&documents_schema())
        .expect("create table");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    EngineCore::from_storage_with_engine(storage, kind)
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

/// 既定エンジンの twin core（ANN 非経由の対照）を新規 DB に構築する。
fn new_default_core(dir: &std::path::Path) -> EngineCore {
    let storage = Storage::open(dir).expect("open storage");
    storage
        .create_table(&documents_schema())
        .expect("create table");
    EngineCore::from_storage(storage, search_engine::default_engine())
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

/// ANN opt-in core の `HnswIndexCache` 統計サマリ（`tests/fixtures/recall_engine.rs
/// ::AnnStats` と同型の複製。`sql::hnsw_cache::HnswIndexCacheStats` は
/// `pub(crate)` モジュール配下のため型名を綴れない）。
struct AnnStats {
    builds: u64,
    build_failures: u64,
    rebuilds: u64,
    delta_searches: u64,
    fallbacks: u64,
    plain_scans: u64,
}

fn ann_stats(core: &EngineCore) -> AnnStats {
    let s = core.hnsw_index_cache_stats();
    AnnStats {
        builds: s.builds,
        build_failures: s.build_failures,
        rebuilds: s.rebuilds,
        delta_searches: s.delta_searches,
        fallbacks: s.fallbacks,
        plain_scans: s.plain_scans,
    }
}

// fixture 方針の健全性チェック（本ファイル冒頭ドキュメント参照。コンパイル時
// 定数のため実行時アサートではなく const アサートで固定する）: 逐次構築
// （決定的）を維持するため `MIN_ROWS_PER_THREAD * 2`（2,048）未満に収めつつ、
// 実際に HNSW 索引が構築される規模（`MIN_INDEXED_ROWS` 以上）であることを固定する。
const _: () = assert!(FILE_COUNT >= MIN_INDEXED_ROWS && FILE_COUNT < 2_048);

/// 状態遷移（初回構築 → overlay → 再構築）を通した ANN opt-in と既定エンジンの
/// 同水準到達を固定する。
#[test]
fn ann_incremental_states_match_default_engine_self_retrieval() {
    let ann_dir = unique_db_path("incremental-hnsw-ann");
    let _ann_guard = CleanupGuard(ann_dir.clone());
    let default_dir = unique_db_path("incremental-hnsw-default");
    let _default_guard = CleanupGuard(default_dir.clone());

    let ann_core = new_ann_core(&ann_dir);
    let default_core = new_default_core(&default_dir);
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    // ---------- 状態 1: 初回構築（FILE_COUNT 件・marker="orig"） ----------
    seed_files(&ann_core, &ctx, "orig", "seed-ann");
    seed_files(&default_core, &ctx, "orig", "seed-default");

    let sample: Vec<usize> = (0..FILE_COUNT).step_by(37).collect(); // 約 30 件の代表サンプル
    let ann_rate = self_retrieval_rate(&ann_core, &ctx, &sample, "orig");
    let default_rate = self_retrieval_rate(&default_core, &ctx, &sample, "orig");
    assert!(
        ann_rate >= 0.9,
        "initial-build self-retrieval rate below floor: ann={ann_rate}"
    );
    assert!(
        default_rate >= 0.9,
        "initial-build self-retrieval rate below floor (default engine): default={default_rate}"
    );

    let stats1 = ann_stats(&ann_core);
    assert_eq!(stats1.builds, 1, "expected exactly one initial HNSW build");
    assert_eq!(stats1.build_failures, 0);
    let initial_rate = ann_rate;

    // ---------- 状態 2: overlay（1〜3 ファイルを同一パス置換。stale+delta 比は
    // 極小 <1% のため再構築は起きないはず） ----------
    let overlay_targets = [0usize, 500, 999];
    for &i in &overlay_targets {
        replace_file(&ann_core, &ctx, i, "delta", &format!("overlay-ann-{i}"));
        replace_file(
            &default_core,
            &ctx,
            i,
            "delta",
            &format!("overlay-default-{i}"),
        );
    }

    for &i in &overlay_targets {
        let want = file_path(i);
        let query_marker = format!("delta content{i}");
        let ann_top = hybrid_top_paths(&ann_core, &ctx, &query_marker, 10);
        assert!(
            ann_top.contains(&want),
            "overlay: ANN core must retrieve the replaced chunk for {want}"
        );
        let default_top = hybrid_top_paths(&default_core, &ctx, &query_marker, 10);
        assert!(
            default_top.contains(&want),
            "overlay: default engine must retrieve the replaced chunk for {want}"
        );

        // 旧チャンク（marker="orig"）の本文はもう存在しない（同一パス置換の
        // セマンティクス。TASK-120）。ランキング非依存の直接照会で固定する
        // （`body_for_path` のドキュメント参照。BM25/hybrid の順位圏外へ押し出
        // されることまでは保証しないため、ランキングベースの負の主張は使わない）。
        let body = body_for_path(&ann_core, &ctx, &want);
        assert!(
            body.contains("delta"),
            "overlay: {want} body must contain the new marker after replacement"
        );
        assert!(
            !body.contains("orig"),
            "overlay: {want} body must no longer contain the stale marker after replacement"
        );
    }

    let stats2 = ann_stats(&ann_core);
    assert_eq!(
        stats2.builds, 1,
        "overlay must not trigger a rebuild (stale+delta ratio well below threshold)"
    );
    assert_eq!(stats2.rebuilds, 0);
    // overlay（未索引分 brute-force 併用）が実際に発火したことの非 vacuous 検証。
    // 具体的にどの縮退経路（`delta_searches`〔未索引分マージ〕・`plain_scans`
    // 〔可視カーディナリティ判定〕・`fallbacks`〔その他の縮退〕のいずれか）を通るかは
    // 実装内部の判定順序（`docs/design/hnsw-rls-cardinality-switch.md` 参照）に
    // 依存するため、「索引のみで完結せず何らかの overlay 関連経路を経由した」ことを
    // 3 カウンタの和で固定する（黙って `builds`／`rebuilds` の非変化だけを見ると、
    // 索引が実質的に無視されて毎回全件 brute-force に縮退していても検出できない
    // ため、非ゼロを要求する）。`stats1`（状態 1 終了時点。初回構築の
    // self-retrieval 検証クエリを含む）はこの時点で既に非ゼロになりうるため、
    // 絶対値ではなく `stats1` からの増分で判定する（さもないと overlay 経路が
    // 一度も発火しなくても状態 1 の累積値だけで通ってしまう vacuous な検証になる。
    // codex-review P2・Cursor Bugbot 指摘・Issue #412 PR #438）。
    let overlay_engaged = (stats2.delta_searches - stats1.delta_searches)
        + (stats2.plain_scans - stats1.plain_scans)
        + (stats2.fallbacks - stats1.fallbacks);
    assert!(
        overlay_engaged > 0,
        "overlay queries must exercise some overlay/fallback path beyond the state-1 baseline \
         (delta_searches {}->{} plain_scans {}->{} fallbacks {}->{})",
        stats1.delta_searches,
        stats2.delta_searches,
        stats1.plain_scans,
        stats2.plain_scans,
        stats1.fallbacks,
        stats2.fallbacks
    );

    // 既存（未置換）チャンクの自己検索到達率は overlay 後も非劣化。overlay
    // 対象ファイル（`overlay_targets`）はもう marker="orig" を含まないため
    // `sample` から除外する（含めたままだと「未変更チャンクの取得」検証で
    // なくなり、約 5% の許容予算を「置換済みファイルの orig 不在」という別の
    // 理由で圧迫しうる。Cursor Bugbot 指摘・Issue #412 PR #438）。
    let untouched_sample: Vec<usize> = sample
        .iter()
        .copied()
        .filter(|i| !overlay_targets.contains(i))
        .collect();
    let ann_rate_after_overlay = self_retrieval_rate(&ann_core, &ctx, &untouched_sample, "orig");
    assert!(
        ann_rate_after_overlay >= initial_rate - 0.05,
        "overlay must not degrade self-retrieval on untouched chunks: before={initial_rate} after={ann_rate_after_overlay}"
    );

    // ---------- 状態 3: 再構築（さらに多数のファイルを置換。stale+delta 比が
    // 1/10 を超え再構築が起こるはず） ----------
    // 開始位置は既存 ID 範囲（`0..FILE_COUNT`）に収まるよう固定する。以前は
    // `1000..1000 + (FILE_COUNT / 8)` で `FILE_COUNT`（1,100）を超えて
    // `1137` まで走査しており、`FILE_COUNT` を超える 37 件（1100〜1136）は
    // 既存ファイルの置換ではなく新規挿入になっていたため、「137 ファイルの
    // 置換で stale+delta 比が閾値を超え再構築が起こる」という意図した
    // fixture になっていなかった（codex-review P2・Cursor Bugbot 指摘・
    // Issue #412 PR #438）。`REBUILD_START` は overlay 済みの
    // `overlay_targets`（0, 500, 999）と重複しない範囲に固定する。
    const REBUILD_START: usize = 200;
    const REBUILD_COUNT: usize = FILE_COUNT / 8; // 約 12.5%
    const _: () = assert!(REBUILD_START + REBUILD_COUNT <= FILE_COUNT);
    let rebuild_targets: Vec<usize> = (REBUILD_START..REBUILD_START + REBUILD_COUNT).collect();
    for &i in &rebuild_targets {
        replace_file(&ann_core, &ctx, i, "rebuilt", &format!("rebuild-ann-{i}"));
        replace_file(
            &default_core,
            &ctx,
            i,
            "rebuilt",
            &format!("rebuild-default-{i}"),
        );
    }

    let rebuild_sample: Vec<usize> = rebuild_targets.iter().copied().step_by(11).collect();
    let ann_rebuild_rate = self_retrieval_rate(&ann_core, &ctx, &rebuild_sample, "rebuilt");
    assert!(
        ann_rebuild_rate >= 0.9,
        "post-rebuild self-retrieval rate below floor: ann={ann_rebuild_rate}"
    );

    let stats3 = ann_stats(&ann_core);
    assert!(
        stats3.builds >= 2,
        "expected at least one rebuild beyond the initial build (builds={})",
        stats3.builds
    );
    assert!(
        stats3.rebuilds >= 1,
        "expected the stale+delta ratio to trigger a rebuild"
    );
    assert_eq!(stats3.build_failures, 0);

    // overlay 済みチャンク（状態 2）は再構築後も引き続き到達可能。
    for &i in &overlay_targets {
        let want = file_path(i);
        let query_marker = format!("delta content{i}");
        let ann_top = hybrid_top_paths(&ann_core, &ctx, &query_marker, 10);
        assert!(
            ann_top.contains(&want),
            "post-rebuild: overlay chunk for {want} must remain retrievable"
        );
    }
}

/// テナント非干渉（Issue #412・R3 の派生検証）: tenant-a のファイル置換が
/// tenant-b の同一パス行を変更しない（`tests/incremental_index.rs::
/// resend_does_not_touch_other_tenants_same_path_rows` と同方針。ANN opt-in
/// core での再確認）。
#[test]
fn ann_replace_does_not_touch_other_tenants_same_path_rows() {
    let dir = unique_db_path("incremental-hnsw-tenant-isolation");
    let _guard = CleanupGuard(dir.clone());
    let core = new_ann_core(&dir);

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

    let body_b = "tenant b content line one\ntenant b marker line two";
    core.execute_insert_sql(
        &ctx_b,
        &insert_file_sql("shared/path.md", body_b, "tenant-b-op-1"),
    )
    .expect("tenant-b insert should succeed");

    let body_a = "tenant a content line one\ntenant a marker line two";
    core.execute_insert_sql(
        &ctx_a,
        &insert_file_sql("shared/path.md", body_a, "tenant-a-op-1"),
    )
    .expect("tenant-a insert should succeed");

    // tenant-a が同じパスを再送しても tenant-b の行は変更されない。
    let body_a2 = "tenant a updated line one\ntenant a updated marker line two";
    core.execute_insert_sql(
        &ctx_a,
        &insert_file_sql("shared/path.md", body_a2, "tenant-a-op-2"),
    )
    .expect("tenant-a resend should succeed");

    let read_ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vec_literal(&vec![0.0f32; DIM as usize]);
    let rows_b = core
        .execute_sql(
            &read_ctx_b,
            &format!(
                "SELECT body FROM documents WHERE path = 'shared/path.md' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("tenant-b select should succeed");
    let bodies_b: Vec<String> = rows_b
        .rows
        .into_iter()
        .filter_map(|r| {
            r.cells.into_iter().find_map(|c| match c {
                engine::sql::exec::Cell::Text(s) => Some(s),
                _ => None,
            })
        })
        .collect();
    assert!(
        bodies_b.iter().any(|b| b.contains("tenant b")),
        "tenant-b's row for the shared path must remain intact after tenant-a's replacement"
    );
    assert!(
        bodies_b.iter().all(|b| !b.contains("tenant a")),
        "tenant-a's content must never leak into tenant-b's visible rows"
    );
}
