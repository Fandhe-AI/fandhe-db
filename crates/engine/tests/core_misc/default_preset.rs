//! TASK-150 デフォルトプリセット確認テスト（EXT-7。対象ビヘイビア: EXT-7。
//! ポインタ: `docs/spec/05-tasks.md` TASK-150・`docs/spec/04-behavior/extensions.md`
//! EXT-7・マイルストーン MS-6）。
//!
//! 「初期導入時に追加設定を一切行わない状態」（`SET` なし・config 指定なし。等価性
//! 検証には決定的な `CpuScalarProvider` を使うが、既定 provider
//! （`search_engine::default_engine()`。`EngineCore::open` 経由）を実際に通す
//! スモークテストも 1 本含める）で、検索（SEARCH-1〜3）とクエリプランニング
//! （PLAN-1〜5）の既定構成が有効になっていることを確認する統合テスト。ここでいう
//! 「既定構成」は既に public な実装コードで公開済みの値・契約のみを指し、spec 本文は
//! 転記しない（[spec-confidentiality](../../../.claude/rules/spec-confidentiality.md)）:
//!
//! - 等重み RRF・プール深さ 200: `hybrid.rs::RrfConfig::default`
//! - シンボル辞書必須: `dictionary.rs::Dictionary::symbols`（無効化スイッチなし）
//! - 既定検索モード: `sql/mode.rs::SearchMode::Recall`（`SET search_mode` なしの既定）
//! - クエリプランニングの既定結線: `core.rs::EngineCore::dictionary_snapshot` が
//!   設定なしで辞書を自動構築し、`plan_query` が辞書スナップショット由来の固定接頭辞を
//!   自動的に束ねる（planner 未注入時は fail-closed 拒否）
//!
//! production コード（`crates/engine/src/`・`crates/wire-server/`）は変更しない
//! （本 Issue のスコープはテスト追加のみ）。

use std::sync::{Arc, Mutex};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::dictionary::{DictionaryBuilder, DictionaryConfig};
use engine::embedding::HashingEmbedder;
use engine::hybrid::{hybrid_search, RrfConfig};
use engine::incremental::IncrementalConfig;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::row_codec::Value;
use engine::sparse::SparseIndex;
use engine::storage::{Storage, Visibility};

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した（`tests/extensions.rs`・
// `tests/sql_surface.rs` と同じ流儀）。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT_ID: &str = "tenant-a";

fn tenant_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

// --- SEARCH-1〜3: 既定 RRF 構成の値確認 ---------------------------------------------

#[test]
fn default_rrf_config_matches_publicly_documented_defaults() {
    // `RrfConfig::default()`（追加設定を一切行わない状態）の公開 getter で
    // 等重み・プール深さ 200・k=60.0 を確認する。既定値のドリフトを検出する回帰の要。
    let cfg = RrfConfig::default();
    assert_eq!(
        cfg.dense_weight(),
        cfg.sparse_weight(),
        "dense/sparse weights must be equal (equal-weight RRF) by default"
    );
    assert_eq!(cfg.dense_weight(), 1.0);
    assert_eq!(cfg.sparse_weight(), 1.0);
    assert_eq!(cfg.pool_depth(), 200);
    assert_eq!(cfg.k_const(), 60.0);
    // Issue #310: 同点順位規約の既定は `TieRank::GroupEnd`（id 依存バイアス除去のため
    // `Positional` から切り替え）。
    assert_eq!(cfg.tie_rank(), engine::hybrid::TieRank::GroupEnd);
}

// --- SEARCH-1〜3: 追加設定なしの SQL 表層でハイブリッド検索が既定構成で動くこと -----

fn hybrid_table_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("body", ColumnType::Text, true),
        ],
    )
}

/// SEARCH-1・SEARCH-3 検証共通の合成コーパス（`tests/hybrid.rs::build_corpus`／
/// `build_corpus_with_noise` と同じ設計方針）。密のみ・疎のみに強い候補を両方含め、
/// 既定構成で両チャネルが融合へ寄与していることを確認できるようにする。
///
/// - id=1: キーワード「rust」を含み、かつ密ベクトルもクエリに近い（疎・密ともに当たる）
/// - id=2: キーワードのみ一致（密ベクトルはクエリから明確に遠い＝負の内積。
///   「疎のみが当てられる文書」）
/// - id=3: 密ベクトルのみクエリに近い（テキストに一致語なし。「密のみが当てられる文書」）
/// - id=4, 100..105: どちらにも当たらないノイズ文書。`LIMIT 4` に対しコーパス総数を
///   10 件へ増やすことで `truncate(k)` が実際に候補を絞り込む状況を作り、
///   「id=2・id=3 が Top-4 に残る」という主張に判別力を持たせる（`LIMIT` = コーパス
///   全件だと密のみ・疎のみのランキングでも同じ結果になり、両チャネル寄与の主張が
///   検証にならない。`tests/hybrid.rs::build_corpus_with_noise` と同じ理由）。
///   ノイズ文書の本文は非 NULL に保ち、`sql/exec.rs` の
///   `sparse_docs.is_empty()` フォールバック（密のみ経路）ではなく本来比較したい
///   `hybrid::hybrid_search` 経路を通す。
///
///   id=2 の密ベクトルはクエリ `[1.0, 0.0]` との内積が `-0.5`（ノイズ文書群の内積
///   `-0.1..=-0.6` の範囲内・かつノイズ上位 2 件より低い）になるよう選んである。
///   これにより「密のみ」で Top-4 を取ると id=1・id=3・ノイズ上位 2 件が並び、id=2 は
///   落選する（`dense_only_top_k` で実測して固定する。下記テストの
///   `assert!(!dense_only_top_k(..).contains(&2), ..)` 参照）。この保証がないと
///   `sql_ids.contains(&2)` は「密のみの Top-4 に元々含まれていた」ケースと区別が
///   つかず、hybrid fusion（sparse チャンネルの寄与）が壊れていても検出できない
///   （Bugbot 指摘・PR #264 review thread 対応）。
fn hybrid_corpus() -> Vec<(u64, [f32; 2], &'static str)> {
    let mut docs = vec![
        (1u64, [1.0f32, 0.0], "rust vector database search"),
        (2, [-0.5, 0.85], "rust programming language guide"),
        (3, [0.9, 0.1], "completely unrelated topic about gardening"),
        (4, [-1.0, 0.0], "another unrelated topic about cooking"),
    ];
    let noise_texts: [&str; 6] = [
        "noise document about weather",
        "noise document about sports",
        "noise document about travel",
        "noise document about music",
        "noise document about finance",
        "noise document about history",
    ];
    for (offset, text) in noise_texts.iter().enumerate() {
        docs.push((
            100 + offset as u64,
            [-0.1 * (offset as f32 + 1.0), 0.0],
            text,
        ));
    }
    docs
}

/// `hybrid_corpus()` に対する「密のみ」Top-k ランキングの id 列を、`CpuScalarProvider`
/// （`kernel.rs` の厳密な総当たり参照実装）で直接計算する。sparse 側を一切経由しない
/// ため、この結果に id=2 が含まれていれば「id=2 が Top-k に現れる」ことは sparse 融合の
/// 証拠にならない（Bugbot 指摘対応）。両テストで「id=2 の出現が sparse 融合の証拠として
/// 有効である」ことをハンドウェーブではなく実測で固定するために使う。
fn dense_only_top_k(corpus: &[(u64, [f32; 2], &str)], query: &[f32; 2], k: usize) -> Vec<u64> {
    let ids: Vec<u64> = corpus.iter().map(|(id, ..)| *id).collect();
    let vectors: Vec<f32> = corpus.iter().flat_map(|(_, emb, _)| *emb).collect();
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query,
        k,
    };
    let hits = CpuScalarProvider
        .search(input)
        .expect("dense-only baseline search should succeed");
    hits.iter().map(|h| h.id).collect()
}

fn seed_hybrid_corpus(storage: &Storage) {
    storage
        .create_table(&hybrid_table_schema())
        .expect("create_table(docs)");
    for (id, emb, body) in hybrid_corpus() {
        let ctx = tenant_ctx(TENANT_ID);
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(body.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .unwrap_or_else(|e| panic!("insert row id={id} failed: {e}"));
    }
}

#[test]
fn sql_hybrid_query_with_no_additional_config_matches_default_library_level_hybrid_search() {
    // SQL 表層側: `CREATE TABLE` → 決定的データ投入 → セッション設定なしで
    // `execute_sql` を呼ぶ（`SET`・config 指定は一切行わない）。等価性の判定には
    // 厳密な `CpuScalarProvider`（総当たり）を使い、既定 provider の近似特性を
    // 混入させず既定構成そのものが有効かどうかだけを問える形にする
    // （`tests/sql_surface.rs::new_core` と同じ方針。既定 provider を実際に通す
    // 経路は下の `default_engine_with_no_additional_config_returns_hits_from_both_channels`
    // が別途スモーク検証する）。
    let path_sql = unique_db_path("default-preset-sql-hybrid");
    let _cleanup_sql = CleanupGuard(path_sql.clone());
    let storage_sql = Storage::open(&path_sql).expect("open storage");
    seed_hybrid_corpus(&storage_sql);
    let core = EngineCore::from_storage(storage_sql, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx(TENANT_ID);

    let sql = "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'rust') LIMIT 4";
    let sql_result = core
        .execute_sql(&ctx, sql)
        .expect("hybrid SQL query with no additional config should succeed");
    let sql_ids: Vec<u64> = sql_result.rows.iter().map(|r| r.id).collect();

    // ライブラリレベル側: 同一データ・同一クエリを `hybrid::hybrid_search` に
    // `RrfConfig::default()`（追加設定なし）で直接投げる。SQL 既定経路 ≡ 公開既定
    // 構成、という等価性で「既定が有効」を行動レベルで固定する。
    let corpus = hybrid_corpus();
    let ids: Vec<u64> = corpus.iter().map(|(id, ..)| *id).collect();
    let vectors: Vec<f32> = corpus.iter().flat_map(|(_, emb, _)| *emb).collect();
    let doc_refs: Vec<(u64, &str)> = corpus.iter().map(|(id, _, body)| (*id, *body)).collect();
    let sparse_index = SparseIndex::build(&doc_refs).expect("sparse index build ok");
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &[1.0, 0.0],
        k: 4,
    };
    let cfg = RrfConfig::default();
    let lib_hits = hybrid_search(&CpuScalarProvider, input, &sparse_index, "rust", 4, &cfg)
        .expect("library-level hybrid_search with RrfConfig::default() should succeed");
    let lib_ids: Vec<u64> = lib_hits.iter().map(|h| h.id).collect();

    assert_eq!(
        sql_ids, lib_ids,
        "SQL default hybrid path must match library-level hybrid_search(RrfConfig::default())"
    );

    // 判別力の実測固定（Bugbot 指摘対応）: 「密のみ」の Top-4 に id=2 が含まれないことを
    // 先に確認する。これがないと下の `sql_ids.contains(&2)` は「密ランキングで元々
    // Top-4 だった」ケースと区別がつかず、sparse 融合が壊れていても検出できない。
    let dense_only_ids = dense_only_top_k(&hybrid_corpus(), &[1.0, 0.0], 4);
    assert!(
        !dense_only_ids.contains(&2),
        "test corpus invariant violated: id=2 must NOT be in the dense-only top-4, \
         otherwise its presence in the hybrid result below has no detection power \
         for sparse fusion: {dense_only_ids:?}"
    );

    // 密のみ（id=3）・疎のみ（id=2）に強い候補がいずれも Top-k に現れる（両チャネルが
    // 既定で寄与している = 融合が有効であることの確認）。id=2 は上記の通り密のみでは
    // Top-4 から落選するため、ここに現れることは sparse 融合の寄与の証拠になる。
    assert!(
        sql_ids.contains(&2),
        "sparse-leaning doc must be present: {sql_ids:?}"
    );
    assert!(
        sql_ids.contains(&3),
        "dense-leaning doc must be present: {sql_ids:?}"
    );
}

#[test]
fn default_engine_with_no_additional_config_returns_hits_from_both_channels() {
    // 既定 provider（`search_engine::default_engine()`。`EngineCore::open` 経由）を
    // 実際に通すスモークテスト。`CpuScalarProvider` との厳密一致は要求しない
    // （既定 provider `ParallelSearchProvider`（`search_engine.rs::SearchEngineKind::ParallelBruteForce`）
    // はマルチスレッド並列の**総当たり**実装であり近似探索ではないが、複数ワーカーの
    // 部分結果をマージする都合上スコア同点時の順序が `CpuScalarProvider`（単一スレッド
    // 総当たり）と一致する保証まではない。完全一致を期待するとテスト側が同点順序の
    // 詳細に依存する別の複雑さを持ち込み、かつ flaky になりうるため、ここでは
    // 「両チャネルの寄与が Top-k に現れる」ことのみを確認する）。テーブル作成・
    // データ投入は `EngineCore` が storage を
    // 外へ出さない一方向設計（`core.rs` モジュールドキュメント参照）のため、
    // 同一パスへ先に `Storage::open` で投入してから close し、`EngineCore::open`
    // で同じパスを再オープンする（`tests/extensions.rs` の close/reopen 手法と同じ）。
    let path = unique_db_path("default-preset-default-engine-smoke");
    let _cleanup = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage (seed)");
        seed_hybrid_corpus(&storage);
        // ここでスコープを抜けて `storage` が drop される（close 相当）。
    }

    // `with_embedder`・`with_query_planner`・`with_dictionary_config` 等、いずれの
    // ビルダーメソッドも呼ばない（追加設定を一切行わない状態）。
    let core = EngineCore::open(&path).expect("EngineCore::open with no additional config");
    let ctx = tenant_ctx(TENANT_ID);

    let sql = "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'rust') LIMIT 4";
    let result = core
        .execute_sql(&ctx, sql)
        .expect("hybrid SQL query via the default engine with no additional config should succeed");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();

    assert!(ids.len() <= 4, "hit count must not exceed LIMIT: {ids:?}");

    // 判別力の実測固定（Bugbot 指摘対応。上の SQL 表層テストと同じ理由）: 「密のみ」の
    // Top-4 に id=2 が含まれないことを先に確認する。これがないと下の
    // `ids.contains(&2)` は密ランキングでたまたま Top-4 だったケースと区別がつかず、
    // 既定エンジン経路で sparse 融合が壊れている／skip されていても検出できない。
    let dense_only_ids = dense_only_top_k(&hybrid_corpus(), &[1.0, 0.0], 4);
    assert!(
        !dense_only_ids.contains(&2),
        "test corpus invariant violated: id=2 must NOT be in the dense-only top-4, \
         otherwise its presence in the default-engine result below has no detection \
         power for sparse fusion: {dense_only_ids:?}"
    );

    assert!(
        ids.contains(&2),
        "sparse-leaning doc must be present via the default engine: {ids:?}"
    );
    assert!(
        ids.contains(&3),
        "dense-leaning doc must be present via the default engine: {ids:?}"
    );
}

// --- SEARCH-1〜3: 既定検索モードが recall であること --------------------------------

#[test]
fn sql_query_with_no_set_search_mode_resolves_to_default_recall_behavior() {
    // `SET search_mode` を発行しないセッション（`execute_sql` は `SessionState::default()`
    // を内部で使う後方互換エントリポイント）での実行が既定 `recall` で受理されることを
    // 確認する（既存 `tests/sql_search_mode.rs` の「既定値」観点と重複しない範囲として、
    // 本ファイルでは「追加設定なし」で普通の Top-k SELECT が拒否されず、独立オラクルと
    // 完全一致することのみを確認する）。
    let path = unique_db_path("default-preset-search-mode");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let corpus: Vec<(u64, [f32; 3])> = vec![
        (1, [1.0, 0.0, 0.0]),
        (2, [0.9, 0.1, 0.0]),
        (3, [0.0, 1.0, 0.0]),
    ];
    for (id, emb) in &corpus {
        let ctx = tenant_ctx(TENANT_ID);
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx(TENANT_ID);
    // `USING MODE`・`SET search_mode` のいずれも発行しない（追加設定なし）。
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 2",
        )
        .expect("plain SELECT with no mode configuration should succeed under default recall");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1, 2]);

    // 上の 2 件一致だけでは「既定が precision に化けても検出できない」（codex-review
    // P2 指摘・PR #264 対応）: precision モードは既定 `PrecisionPolicy::default()`
    // （`precision.rs::DEFAULT_MAX_RESULTS = 1`）により常に高々 1 件しか返さない。
    // 同一クエリを `USING MODE 'precision'` で明示実行し、件数が 1 件以下（＝上の
    // 2 件一致とは構造的に両立し得ない）であることを確認することで、上の結果が
    // 「たまたま precision でも 2 件返る」ケースではなく実際に recall 経路を
    // 通っていることの識別力を持たせる。
    let precision_result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 2 USING MODE 'precision'",
        )
        .expect("explicit precision mode should succeed");
    assert!(
        precision_result.rows.len() <= 1,
        "precision mode must return at most DEFAULT_MAX_RESULTS=1 row, proving it is \
         distinguishable from the 2-row default (recall) result above: {:?}",
        precision_result
            .rows
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
    );
}

// --- PLAN-5: シンボル辞書が既定で必須構築されること ----------------------------------

#[test]
fn dictionary_config_default_enables_both_auxiliary_sources() {
    let cfg = DictionaryConfig::default();
    assert!(cfg.enable_file_tree);
    assert!(cfg.enable_term_index);
}

#[test]
fn engine_core_with_no_additional_config_builds_nonempty_symbol_dictionary() {
    // 追加設定なしの `EngineCore`（`with_dictionary_config` を呼ばない）に対し
    // Rust コード片を含む行を投入し、`dictionary_snapshot` が `symbols` 非空の辞書を
    // 返すことを確認する。
    let path = unique_db_path("default-preset-dictionary");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(8), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = tenant_ctx(TENANT_ID);
    engine::tenant::insert_typed_row(
        &storage,
        "documents",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.1; 8]),
            Value::Text("src/lib.rs".to_string()),
            Value::Text("pub fn run_batch() {}\n".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op-1")
            .expect("valid operation_id"),
    )
    .expect("insert row");

    // `with_dictionary_config` を呼ばない（既定 `DictionaryConfig::default()`）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let dict = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("dictionary_snapshot with no additional config should succeed");
    assert!(
        !dict.symbols.is_empty(),
        "symbol dictionary must be non-empty with no additional config"
    );
}

#[test]
fn dictionary_builder_still_populates_symbols_when_auxiliary_sources_disabled() {
    // `DictionaryBuilder` レベルで任意情報源（file_tree・term_index）を無効化した
    // config でもシンボル辞書は構築される（「必須・無効化スイッチなし」の型面契約の
    // 行動確認。`dictionary.rs` 内の同種テストの型面契約を本 EXT-7 テストファイルの
    // 「既定構成」観点として独立に固定する）。
    let config = DictionaryConfig {
        enable_file_tree: false,
        enable_term_index: false,
        ..DictionaryConfig::default()
    };
    let mut builder = DictionaryBuilder::new(config);
    builder.ingest("src/x.rs", "fn only_one() {}\n");
    let dict = builder.finish();
    assert_eq!(dict.symbols.len(), 1);
    assert!(dict.file_tree.paths.is_empty());
    assert!(dict.term_index.is_empty());
}

// --- PLAN-1〜5: クエリプランニングの既定結線 -----------------------------------------

/// 固定 JSON を返すモック `LlmClient`（実 Ollama 非依存。`tests/query_planner.rs` と
/// 同じ流儀）。呼び出しごとに受け取ったプロンプトを記録し、テストから検証できる
/// ようにする。
struct MockLlmClient {
    response: String,
    seen_prompts: Arc<Mutex<Vec<String>>>,
}

impl LlmClient for MockLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.seen_prompts
            .lock()
            .expect("mock lock poisoned")
            .push(prompt.to_string());
        Ok(self.response.clone())
    }
}

const FIXED_EXPANSION_JSON: &str =
    r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null}"#;

fn documents_table_schema() -> TableSchema {
    TableSchema::new(
        "documents",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(16), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        path.replace('\'', "''"),
        body.replace('\'', "''")
    )
}

#[test]
fn plan_query_without_planner_configured_is_rejected_fail_closed_by_default() {
    // planner 未注入の既定状態（`with_query_planner` を呼ばない）では `plan_query` が
    // fail-closed に拒否されることを確認する（既定で外部送信経路が開いていないことの
    // 確認）。
    let path = unique_db_path("default-preset-plan-no-planner");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&documents_table_schema())
        .expect("create table");
    // `with_embedder`・`with_incremental_config` のみ設定し、`with_query_planner` は
    // 呼ばない（追加設定なし＝ planner 未注入）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(16).expect("valid dim")));
    let ctx = tenant_ctx(TENANT_ID);

    let err = core
        .plan_query(&ctx, "documents", "anything")
        .expect_err("plan_query without a configured planner must fail-closed by default");
    assert!(matches!(
        err,
        engine::core::CoreError::QueryPlannerUnavailable
    ));
}

#[test]
fn plan_query_prompt_automatically_bundles_dictionary_snapshot_symbols_with_no_additional_config() {
    // planner をスタブ注入した状態で、`plan_query` のプロンプトに辞書スナップショット
    // 由来のシンボル名が設定なしで含まれることを確認する（辞書コンテキストの自動
    // 束ね＝既定構成の確認。`with_dictionary_config` は呼ばない）。
    let path = unique_db_path("default-preset-plan-dictionary-bundle");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&documents_table_schema())
        .expect("create table");
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(16).expect("valid dim")))
        .with_incremental_config(IncrementalConfig {
            chunking: engine::chunking::ChunkingConfig {
                lines_per_chunk: 4,
                max_markdown_section_chars: None,
            },
            ..IncrementalConfig::default()
        })
        .with_query_planner(Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }));
    let ctx = tenant_ctx(TENANT_ID);

    let body = "//! module doc\npub fn run_batch() {}\nstruct Wrapper {}\n";
    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", body, "op-1"),
    )
    .expect("file insert should succeed");

    core.plan_query(&ctx, "documents", "how does batching work?")
        .expect("plan_query should succeed");

    let prompts = seen_prompts.lock().expect("mock lock poisoned");
    assert_eq!(prompts.len(), 1);
    // `run_batch`（辞書スナップショットのシンボル）が、呼び出し元がシンボル名を一切
    // 指定していないにもかかわらずプロンプトへ自動的に束ねられている。
    assert!(
        prompts[0].contains("run_batch"),
        "dictionary snapshot symbols must be auto-bundled into the prompt with no additional config: {:?}",
        prompts[0]
    );
}
