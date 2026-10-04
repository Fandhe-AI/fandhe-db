//! engine クレート: vector-db のコアロジック層。
//!
//! 責務境界: データロード・検索カーネル・認証・RLS 相当のテナント境界・redb ベースの
//! 永続化を担う（クエリの受付・応答整形など wire プロトコルの詳細は持たない）。
//! `wire-server`（バイナリクレート）から呼び出されるライブラリで、
//! ワークスペース内での相互参照は path 依存に限る
//! （.claude/rules/coding-rust.md: workspace 構成の責務境界を跨ぐ依存を作らない）。
//!
//! 対応: TASK-66（基盤・工程管理）・TASK-140/TASK-141（`redb` 永続化層。RLS フィールドの
//! スキーマ同居まで含む。ポインタ: `docs/spec/05-tasks.md`）・TASK-88（宣言済み
//! トランザクション分離レベル。対象ビヘイビア: TABLE-3）・TASK-85（スキーマカタログ・
//! `CREATE TABLE`／`ALTER TABLE ADD COLUMN`。対象ビヘイビア: TABLE-1, TABLE-4,
//! TABLE-5, TABLE-6）・TASK-86（カタログスキーマ駆動の行エンコーダー。対象ビヘイビア:
//! TABLE-7）・TASK-146（テーブル粒度次元固定・複数テーブル共存の拡張機能。対象ビヘイビア:
//! EXT-1, EXT-2）・TASK-90（2 テーブル横断トランザクション・クラッシュ耐性回帰テスト。対象
//! ビヘイビア: TABLE-10）・TASK-87（コールドスタート・ベクトルアリーナ。対象ビヘイビア:
//! TABLE-8）・TASK-102（検索カーネルの疎検索構成要素。BM25 Okapi。対象ビヘイビア:
//! SEARCH-1, SEARCH-3。密検索との RRF 融合は TASK-103、評価ハーネス回帰テストは
//! TASK-104 の管轄）・TASK-124（`VectorCore` trait・`PolicyContext`・検索カーネル
//! provider 層の製品コア。対象ビヘイビア: CORE-1, CORE-2, CORE-13）・TASK-103
//! （密検索 provider（`kernel.rs`/`parallel_search.rs`）と疎検索（`sparse.rs`）を
//! RRF で融合するハイブリッド検索。対象ビヘイビア: SEARCH-1, SEARCH-3。
//! `VectorCore` trait への統合・SQL 表層統合は後続タスクの管轄）・TASK-133（`rls.rs`。
//! `PolicyContext`（TASK-124）と接続する事前フィルタ方式のテナント境界コア実装。対象
//! ビヘイビア: RLS-1, RLS-2, RLS-3, RLS-4。`core::EngineCore` は TASK-169
//! （`core::PrefilterCache`）経由でこのインデックスをキャッシュし世代整合を保った上で
//! 再利用する）・TASK-134（`rls.rs::SearchTimeFilter`。動的ポリシー用の
//! 検索時フィルタ方式によるテナント境界フォールバック。対象ビヘイビア: RLS-1, RLS-3。
//! `PrefilterIndex`（TASK-133）との使い分けは呼び出し元の責務）・TASK-107
//! （`hybrid.rs` の候補プールを再順位付けするリランキング層。`Reranker` trait による
//! 方式差し替えと、依存追加なしの参照実装 2 種。対象ビヘイビア: SEARCH-6, SEARCH-7,
//! SEARCH-8。方式（クロスエンコーダ等）の最終選定・効果測定回帰は後続タスクの管轄）・
//! TASK-128（バッチクエリ・一括インデクシング専用の検索エンジン `batch_search`。対象
//! ビヘイビア: CORE-6, CORE-7, CORE-16。単発クエリ経路 `core::EngineCore` へは
//! 構造的に接続しない）・TASK-129（`batch_search` の GPU バックエンド初期化失敗・
//! 実行時エラーからの CPU-SIMD 縮退機構 `batch_fallback`。対象ビヘイビア: CORE-8。
//! 縮退経路は `batch_search::run_batch_search` を GPU 参照実装と共有する）・
//! TASK-131（`search_engine.rs`。CORE-9 の差し替え点を `kernel.rs::SearchProvider`
//! （CORE-13）へ一本化する検索エンジン選択・構築レイヤ。`core::EngineCore::open`
//! の既定 provider 構築経路）・TASK-89（`tenant.rs`。行単位テナント境界の行ストア
//! 統合層と `policy.rs::PolicyContext::is_visible`（CORE-2）との統合。対象
//! ビヘイビア: TABLE-9, TABLE-11。詳細は `tenant.rs`・`policy.rs` の
//! モジュールドキュメント参照）・TASK-136（`rls.rs::RlsSafetyNet`。SQL 表層の実行結果
//! に対する RLS 実行時安全網を `sql::plan` から再配置・強化。対象ビヘイビア: RLS-5）。
//! プロトコル層
//! （`wire-server`）は `core::VectorCore` のみに依存し、認証・SQL 表層・実行バックエンド
//! 差し替え等は後続タスクで拡張する。
//!
//! 性能系タスク（TASK-127・TASK-130・TASK-83 等）向けの計測プロトコル基盤は
//! `benches/harness/`（TASK-158。lib 本体外・`cargo bench`／`tests/bench_harness.rs`
//! から利用）を参照。
//!
//! TASK-155（対象ビヘイビア: CORE-11, CORE-12）: `kernel.rs`（CORE-13）・
//! `search_engine.rs`（CORE-9）・`batch_search.rs`（CORE-6, CORE-7）・
//! `batch_fallback.rs`（CORE-8）に分散しうる「入力 → 実行経路」の判断を、
//! `dispatch.rs::select_execution_path` という副作用なしの決定表 1 箇所へ集約する。
//! 経路を外部から上書きする機構は設けない（CORE-12。詳細は `dispatch.rs` の
//! モジュールドキュメント参照）。`core.rs::EngineCore::search`（単発クエリ経路）・
//! `batch_fallback.rs::FallbackBatchEngine::batch_search`（バッチ経路）が実際に
//! `select_execution_path` の戻り値を見て実行を分岐する（配線済み。詳細は
//! `dispatch.rs` モジュールドキュメント参照）。
//!
//! TASK-157（対象ビヘイビア: CORE-15）: `buffer_pool.rs` がバッチ経路の中間バッファを
//! サイズクラス別・総量上限・グローバル LRU で管理するプールを提供し、
//! `batch_search.rs::run_batch_search` の行デコード用スクラッチバッファがこれを
//! 経由する（詳細は `buffer_pool.rs` モジュールドキュメント参照）。
//!
//! TASK-74（対象ビヘイビア: SQL-8）: `sql.rs` が SQL 表層の入口。受信 SQL テキストに
//! 対する AST 許可リスト検証（`sql::allowlist::validate_statement`）を提供する
//! （詳細は `sql.rs` モジュールドキュメント参照）。
//!
//! TASK-128〜130（対象ビヘイビア: CORE-6, CORE-8, CORE-16。ポインタ: Issue #178）:
//! `gpu_batch.rs` が `wgpu`（=30.0.1。依存追加はオーナー承認済み〔2026-08-26〕。
//! `crates/engine/Cargo.toml` コメント参照）を用いた実 GPU バックエンドを提供し、
//! `batch_fallback.rs::FallbackBatchEngine::build_with_gpu` から primary として
//! 差し込める（詳細は `gpu_batch.rs` モジュールドキュメント参照）。Issue #542
//! では `gpu_batch::packed_i8::GpuI8BatchBackend` として i8 パック常駐・
//! `dot4I8Packed` シェーダによる opt-in 候補生成経路（最終スコアは f32 再計算。
//! primary へは未接続）を追加した（詳細は `gpu-batch-i8-packed.md` 参照）。
//!
//! TASK-161（対象ビヘイビア: SQL-12）: `sql::mode` が取得モード（`recall`／
//! `precision`）の構文（`USING MODE` 句・`SET search_mode`）・優先順位解決
//! （クエリ句 > セッション変数 > 既定）・接続単位の `SessionState` を提供する。
//! `core::EngineCore::execute_sql_in_session` がこれを束ねる新しい公開 API（詳細は
//! `sql.rs`・`sql/mode.rs` モジュールドキュメント参照）。
//!
//! TASK-137（対象ビヘイビア: RLS-6, RLS-7）: `rls.rs::ImplicitRlsHook` が候補集合構築へ
//! 可視性フィルタを適用する単一注入点（詳細は `rls.rs` モジュールドキュメント参照）。
//!
//! TASK-138（対象ビヘイビア: RLS-8）: TASK-137 の暗黙適用契約が MVP クエリカタログ
//! （C1〜C4）以外の全読み取り経路（複数の任意スキーマテーブル・任意形状 SELECT・
//! 宣言的 UDF・`VectorCore::search`／`get_row`・`tenant::visible_rows`）へも
//! 一般化されて働くことを `tests/rls_generalized.rs` で機械検証する（経路インベントリ
//! ・検証マトリクスは `docs/design/rls-generalized-read-paths.md` 参照）。
//! `USING PLAN` 展開後クエリ（TASK-77・SQL-5。`sql::using_plan::bind_expansion` →
//! 既存 C4 ハイブリッド実行形への束縛）の RLS 暗黙適用は単一テーブルに限定して
//! `tests/rls_generalized.rs::using_plan_dispatch_implicitly_applies_rls` で検証済み。
//! 全テーブル軸への一般化は TASK-117 の管轄。
//!
//! TASK-95（対象ビヘイビア: RECOVER-4）: `tenant.rs` にテナント境界付き書き込みガード
//! （`insert_row`/`update_row`/`delete_row`）を追加し、`policy.rs::PolicyContext::is_owner`
//! の単一照合パスで書き込み認可を判定する（読み取りの可視性判定 `is_visible` とは独立）。
//! `core::EngineCore` は同名の薄い委譲メソッドのみを持ち、`VectorCore` trait へは
//! 昇格しない。機械検証は `tests/tenant_breach.rs`（詳細は `tenant.rs` モジュール
//! ドキュメント参照）。
//!
//! Issue #865（対象ビヘイビア: SQL-17・TASK-191）: SQL 表層 `UPDATE <table> SET
//! <col> = <lit>[, ...] WHERE id = <n> USING OPERATION_ID '<id>'`（部分更新・
//! 単一行）を `tenant::update_row_columns_unchecked`（`update_row_unchecked` と
//! 同一の書き込みプリミティブを共有する列指定の書き込み入口。read-merge-write を
//! 単一 write トランザクション内で行う）へ結線した。0 行更新でも台帳記録・
//! テーブル世代進行・commit を必ず行う非対称設計を含め、詳細は
//! `docs/design/update-single-row.md` 参照。
//!
//! TASK-89/TASK-95（対象ビヘイビア: TABLE-12, RLS-9）: 行 `id` の一意性スコープは
//! テナント内であり、行ストア（`catalog.rs` の `user_rows/{table}`）の物理キーを
//! `(tenant_id, id)` で名前空間化する（`catalog.rs::user_rows_table_def`）。
//! `tenant.rs::insert_row` の重複検出はサーバー側導出テナントの名前空間内だけを見るため、
//! 他テナント行の存在有無が `23505` の有無として観測される経路を構造的に持たない
//! （codex-review P0 指摘・PR #194 対応）。旧フォーマット（キーが `id` のみ）の DB は
//! `catalog.rs::map_row_table_error` が fail-closed に拒否する。読み取り側で同一 `id` の
//! 可視行が複数現れうる点の扱いは `core.rs::provider_result_is_valid` を参照。
//!
//! TASK-89/TASK-95（対象ビヘイビア: TABLE-12, RLS-9・codex-review P1 対応）: 公開の
//! 検索結果型 `kernel.rs::SearchHit` は `(tenant_id, id)` で行を一意に解決できる
//! テナント修飾済みヒットとし、`core::VectorCore::get_row` も `(tenant_id, id)` を
//! キーに取る（行 `id` の一意性スコープがテナント内のため `id` 単独では行を指せない）。
//! `SearchProvider` の戻り値は候補ヒット `kernel.rs::CandidateHit`（識別子は呼び出し元
//! 定義。`core.rs`・`sql/exec.rs` は候補アリーナのスロット番号を渡す）で、テナントの
//! 解決は provider の外側で行う（ホットパスへヒープ確保を持ち込まないため）。
//!
//! TASK-156（対象ビヘイビア: CORE-14）: `isa.rs` が CPU 命令セット（AVX2+FMA・
//! AVX-512・NEON）の実行時検出を提供し、`dispatch.rs::detect_current_isa`
//! （決定表の ISA 入力）・`kernel.rs::dot`（`CpuScalarProvider` 等が共有する内積
//! カーネル）双方の実体がこれへ委譲する。SIMD カーネル（`unsafe` を含む）は検出
//! 成功時のみ構築される sealed トークン経由でしか呼び出せない（詳細は `isa.rs`
//! モジュールドキュメント参照）。
//!
//! TASK-162（対象ビヘイビア: SEARCH-9）: `precision.rs` が `precision` モードの
//! 実行契約（確信度判定・空集合 fail-closed 応答）を提供する。適用位置は
//! `sql::exec::execute_statement` の DISTANCE 段（＋事後 SCALAR フィルタ）の後・
//! `RlsSafetyNet::apply` の前（詳細は `precision.rs`・`sql/exec.rs` モジュール
//! ドキュメント参照）。設定値は `core::EngineCore` が保持する `PrecisionPolicy`
//! で、クエリ・セッション変数から到達できないサーバー側専有の値とする。TASK-161
//! （`sql::mode`）が解決する `SearchMode` はこの契約の実行可否のみを分岐する。
//!
//! TASK-119（対象ビヘイビア: INDEX-3）: `chunking.rs` が `INSERT` 経由で届く
//! ファイル内容（パス＋本文）をファイル種別ごとの方針でチャンク列へ分割する
//! 純関数的な API を提供する（詳細は `chunking.rs` モジュールドキュメント参照）。
//!
//! TASK-120（対象ビヘイビア: INDEX-1, INDEX-2）: `embedding.rs` の `Embedder` trait・
//! 決定的参照実装 `HashingEmbedder` と、`incremental.rs::index_file` がこれを
//! 束ねてファイル形 `INSERT`（`chunking.rs` によるチャンク化 → 注入型 `Embedder` に
//! よるベクトル化 → `tenant.rs::replace_typed_rows_by_text_key` による同一パス
//! 置換書き込み）を実行する。行形 `INSERT`（`sql::parser::bind_insert`）とは
//! `sql::parser::bind_insert_form` の束縛段階の形判別で分岐し、既存の許可リスト
//! 構文（`sql::allowlist`）は変更しない。`core::EngineCore::execute_insert_sql` が
//! 分岐の入口（詳細は `embedding.rs`・`incremental.rs`・`sql/parser.rs` モジュール
//! ドキュメント参照）。`operation_id` の台帳記録（TASK-93・RECOVER-2）は行形と同じ
//! 契約で、置換書き込みと同一 write トランザクション内に結線済み。一括投入 4 上限
//! （`batch_limits.rs`・TASK-122、対象ビヘイビア: INDEX-4。`incremental::index_file_batch`・
//! `core::EngineCore::execute_insert_sql_batch` が結線）は実装済み。外部埋め込み
//! サービス実クライアントの接続は後続タスクの管轄。
//!
//! TASK-147（対象ビヘイビア: EXT-3）: `declarative_filter.rs` が、メタデータ列
//! （`TEXT` 列）に対する等価・前方一致フィルタを任意の列名へ宣言的に指定できる
//! 汎用 API（`DeclarativeFilter`／`MetadataFilter`）を提供する。`sql::parser` の
//! 旧 `ScalarEq`（等価専用・SQL-2 の実装例）はこの汎用 API へ統合済み
//! （BREAKING CHANGE。詳細は `declarative_filter.rs`・`sql/parser.rs` モジュール
//! ドキュメント参照）。
//!
//! TASK-92（対象ビヘイビア: RECOVER-1）: `recovery::required_op_id::LedgerMode` が
//! `operation_id` 必須化ガードをサーバー構成のみで決定する（詳細は `recovery`
//! モジュールドキュメント参照）。
//!
//! TASK-152（対象ビヘイビア: ERR-2）: `error_format.rs` が `wire_code` 写像の
//! 単一真実源（`ErrorClass`・`ClassifiedError` trait・`WireError`）を提供する。
//! `sql::allowlist::SqlSurfaceError`・`tenant::TenantWriteError` はこれへ委譲し、
//! 既存の `wire_code()` 返値は変更しない（詳細は `error_format.rs` モジュール
//! ドキュメント参照）。
//!
//! TASK-93（対象ビヘイビア: RECOVER-2）: `recovery::ledger` が、検証済み
//! `operation_id` をテナント内・テーブル単位で永続化する台帳を提供する。台帳への
//! 追記は行の書き込み・更新・削除と同一の `redb::WriteTransaction` 内で原子的に
//! 行う（詳細は `recovery::ledger` モジュールドキュメント参照）。
//!
//! TASK-166（対象ビヘイビア: SQL-13）: `sql::aggregate` が集計関数（`COUNT`/`SUM`/
//! `AVG`/`MIN`/`MAX`）のみを結果列とする `GROUP BY` なし単一行 SELECT を実行する。
//! RLS 適用順序は既存の検索 SELECT 実行経路（`arena.rs`）と同一の規約（デコード前の
//! ヘッダ判定 → 可視行のみ完全デコード）に揃え、集計値から他テナント行の存在・件数を
//! 推測できないことを維持する（詳細は `sql.rs`・`sql/aggregate.rs` モジュール
//! ドキュメント参照）。
//!
//! TASK-167（対象ビヘイビア: SQL-14）: `sql::group_by` が `GROUP BY <列>`（Issue #1185 で `TEXT` 限定を外した）を
//! 追加し、複数行の集計結果を返す（任意で `HAVING`・`ORDER BY`・`LIMIT` を伴う）。
//! グループ数・グループキーの累計バイト数は上限で有界化し、超過は fail-closed に
//! 拒否する。RLS 適用順序は TASK-166 の単一行経路と同一の規約を独立して踏襲し、
//! 他テナントにしか存在しないグループ値が結果に現れないことを維持する（詳細は
//! `sql/group_by.rs` モジュールドキュメント参照）。
//!
//! TASK-110（対象ビヘイビア: PLAN-1）: `query_planner.rs` が常駐 LLM プロセス
//! （Ollama 等）に対するクエリ展開クライアント（`LlmClient` trait・実装
//! `OllamaClient`・プロンプト組み立て・応答 JSON の厳格パース）を、依存追加なしの
//! 自作 HTTP/1.1／JSON で提供する。`dictionary.rs`（TASK-109）の辞書スナップショットを
//! 固定接頭辞コンテキストとして束ねる注入点は `core::EngineCore::with_query_planner` /
//! `core::EngineCore::plan_query`（詳細は `query_planner.rs` モジュールドキュメント
//! 参照）。展開結果はソフトヒントに限り、SQL・プラン文字列への未検証連結は行わない。
//!
//! TASK-111（対象ビヘイビア: PLAN-1。関連: EXT-4）: `hybrid.rs` に、TASK-110 の
//! `QueryExpansion::path_hint`/`kind_hint` を融合済みスコアへの加点として反映する
//! ソフトブースト機構（`BoostRule`・`apply_soft_boost`・`hybrid_search_boosted`）を
//! 追加する。ヒント一致は候補の除外・絞り込みには使わず、`truncate(k)` 前のプールへ
//! 小さな加点（既定 `SOFT_BOOST_PER_MATCH`）を行い再順位付けするのみで、ハードフィルタ
//! 化しない（詳細は `hybrid.rs` モジュールドキュメント参照）。
//!
//! TASK-148（対象ビヘイビア: EXT-4。ポインタ: `docs/spec/05-tasks.md` TASK-148・
//! `docs/spec/04-behavior/extensions.md` EXT-4）: `scoring_boost.rs` が TASK-111 の
//! ソフトブースト機構を一般化する（詳細は `scoring_boost.rs` モジュールドキュメント
//! 参照）。SQL 表層への構文露出は対象外。

// RECOVER-8（fail_fast）の例外は `recovery::stderr_log::write_line`／
// `log_stderr!` に一本化する（Issue #1081）。通常ビルド・bin では `eprintln!`
// の再混入を lint で防ぎ、`#[cfg(test)]` のテストコードと test_util は対象外
// とする（`make lint` は `--all-targets` で動くため、テストの通常アサーション
// 出力は妨げない）。
#![cfg_attr(not(test), deny(clippy::print_stderr))]

pub mod arena;
pub mod batch_fallback;
pub mod batch_limits;
pub mod batch_search;
pub mod buffer_pool;
pub mod bytea;
pub mod catalog;
pub mod chunking;
pub(crate) mod column_rewrite;
pub(crate) mod constraint;
pub mod core;
pub mod crypto;
pub mod datetime;
pub mod declarative_filter;
pub mod dictionary;
pub mod dispatch;
pub mod embedding;
pub mod error_format;
pub(crate) mod f16;
pub mod gpu_batch;
pub mod hnsw;
pub mod hybrid;
pub mod incremental;
pub mod isa;
pub mod json;
pub mod kernel;
pub(crate) mod key_index;
pub mod numeric;
pub mod parallel_search;
pub mod policy;
pub mod precision;
pub mod query_planner;
pub mod recovery;
pub mod rerank;
pub mod rls;
pub mod row_codec;
pub mod scalar_float;
pub mod scoring_boost;
pub mod search_engine;
pub mod sparse;
pub(crate) mod sq8;
pub mod sql;
pub mod storage;
pub mod tenant;
#[cfg(feature = "test-sync-points")]
pub mod test_sync;
pub mod tiering;
pub mod txn;
pub mod uuid;
pub mod wasm_udf;

/// テスト専用の共通ヘルパ群（Issue #173）。`#[cfg(test)]` 限定・非公開のため
/// `pub mod` を含まず `scripts/check_core_api.sh` の到達性スナップショットに影響しない。
///
/// `temp_db` 自身の自己テスト（`temp_db_tests`）は本モジュール配下でのみ
/// コンパイル・実行される（Issue #201 レビュー対応）。`tests/*.rs` 側は
/// `#[path = "../src/test_util/temp_db.rs"] mod temp_db;` で `temp_db.rs` 単体のみを
/// 取り込むため、結合テストバイナリごとの重複実行は発生しない。
#[cfg(test)]
mod test_util {
    pub mod temp_db;
    mod temp_db_tests;
}

/// engine クレートの識別子。
///
/// wire-server がリンク時にこのクレートへ到達可能であることを確認するための
/// プレースホルダ API（TASK-66 時点の雛形）。後続タスクで実際の公開 API に置き換わる。
pub const ENGINE_NAME: &str = "engine";

#[cfg(test)]
mod tests {
    use super::*;

    // workspace の雛形が成立していること（クレートがビルド・リンクできること）を
    // 確認する smoke テスト。対象ビヘイビア ID なし（基盤タスクのため）。
    #[test]
    fn engine_name_is_stable() {
        assert_eq!(ENGINE_NAME, "engine");
    }
}
