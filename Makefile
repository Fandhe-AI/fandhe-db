# fandhe-db の開発タスクランナー。
#
# Make は scripts/*.sh（または言語ネイティブの 1 行コマンド）を呼ぶだけの薄い入口であり、
# 複数行・条件分岐・環境依存の処理の定義は scripts/ 側にのみ置く（各レシピは 1 行の呼び出しに
# 限定し、ここで処理を二重定義しない）。変更時は .claude/rules/makefile.md に従うこと。
# Make を使わない場合の同等コマンドは scripts/help.sh を参照。
#
# `make setup` 一発で開発環境（サブモジュール・rustup・lefthook）を構築し、
# `make ci` で CI（.github/workflows/ci.yml）と同等のチェックをローカル実行する。
# Docker で環境非依存に開発・検証する場合は docker-* ターゲットを使う（compose.yaml 参照）。
# Fandhe-AI/rust-ai-library の Makefile と同一方針。
#
# GNU Make 3.81（macOS 標準）で動作する。3.82+ 専用機能（.ONESHELL・.RECIPEPREFIX 等）は使わない。
#
# 使い方: `make help`（既定ターゲット）でターゲット一覧を表示する。
# 一覧は下記の `## ` コメントから生成されるため、`## ` の無いターゲットは表示されない。
# 詳細な説明は各ターゲット直前の `# ` コメントに置く。
# ターゲットを追加・改名・削除したら scripts/help.sh も同じ変更で更新すること
# （scripts/tests/test_help_sync.py が両者の一致を検査する）。
# 中身の無いターゲット（`@true` 等）は呼び出し元が終了コードを信用してしまうため置かない。
#
# lint ツール・導入系ツールの固定バージョンは scripts/lib.sh に集約している。

.DEFAULT_GOAL := help
SHELL := /bin/bash

# hnsw-acorn-twohop-runs の既定連続測定回数（`make hnsw-acorn-twohop-runs RUNS=<N>` で上書き）
RUNS ?= 5

.PHONY: help
help: ## このヘルプを表示する（Make 非依存の一覧: scripts/help.sh）
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_-]+:.*## /{printf "  \033[36m%-30s\033[0m %s\n", $$1, $$2}' $(firstword $(MAKEFILE_LIST))

# 開発環境を一括構築する（サブモジュール → rustup → lefthook の順を保証）
.PHONY: setup
setup: ## 開発環境を一括構築する
	@scripts/setup.sh

.PHONY: rustup
rustup: ## rustup（cargo）を未導入の場合のみ導入する
	@scripts/install_rustup.sh

# docs/spec サブモジュールを初期化・更新する（private・アクセス権が無ければ警告のみ）
.PHONY: submodule
submodule: ## docs/spec サブモジュールを初期化・更新する（取得失敗は警告のみ）
	@scripts/init_submodule.sh

# lefthook の git hooks を導入する（未導入なら lefthook 本体も導入）
.PHONY: hooks
hooks: ## lefthook の git hooks を導入する
	@scripts/install_hooks.sh

# markdownlint（.markdownlint.jsonc / .markdownlintignore 参照）
.PHONY: lint-md
lint-md: ## markdownlint を実行する
	@scripts/lint_docs.sh md

# yamllint（.yamllint 参照）
.PHONY: lint-yaml
lint-yaml: ## yamllint を実行する
	@scripts/lint_docs.sh yaml

# editorconfig-checker（.editorconfig + .editorconfig-checker.json 参照）
.PHONY: lint-editorconfig
lint-editorconfig: ## editorconfig-checker を実行する
	@scripts/lint_docs.sh editorconfig

# commitlint（origin/main からの分岐点以降のコミットを検証）
.PHONY: lint-commits
lint-commits: ## commitlint（origin/main からの分岐点以降のコミット）を実行する
	@scripts/lint_docs.sh commits

.PHONY: lint-docs
lint-docs: lint-md lint-yaml lint-editorconfig lint-commits ## ドキュメント／設定ファイル系 lint を一括実行する

.PHONY: fmt
fmt: ## cargo fmt --all で整形する
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## cargo fmt --check（整形差分の検出）
	cargo fmt --all --check

# cargo clippy -D warnings（lint ゲート）
.PHONY: lint
lint: ## cargo clippy -D warnings
	cargo clippy --workspace --all-targets --all-features -- -D warnings

.PHONY: test
test: ## cargo test（workspace 全体）
	cargo test --workspace --all-features

# 既定ビルド（feature fault-injection 無効）専用の回帰テストを実行する（Issue #710・#715。make test は --all-features のため対象外のテストをピンポイント実行）
.PHONY: test-default-build
test-default-build: ## 既定ビルド（fault-injection 無効）専用の回帰テスト
	cargo test -p fandhe-db-wire-server --test wire_fault_injection_cli

# クラッシュ耐性回帰テスト（TASK-142・PERSIST-1。scripts/crash_test.sh を実行）
.PHONY: crash-test
crash-test: ## クラッシュ耐性回帰テスト（PERSIST-1）
	scripts/crash_test.sh

# crash_test.sh の中断パス（SIGTERM 単体・プロセスグループ）のセルフテスト（Issue #134・TASK-142。scripts/crash_test_interrupt.sh を実行）
.PHONY: crash-test-interrupt
crash-test-interrupt: ## crash_test.sh の中断パス（SIGTERM・プロセスグループ）のセルフテスト
	scripts/crash_test_interrupt.sh

# 2 テーブル横断トランザクション・クラッシュ耐性回帰テスト（TASK-90・TABLE-10。scripts/crash_test_cross_table.sh を実行）
.PHONY: crash-test-cross-table
crash-test-cross-table: ## 2 テーブル横断トランザクション・クラッシュ耐性回帰テスト
	scripts/crash_test_cross_table.sh

# 永続一意索引・クラッシュ耐性回帰テスト（Issue #1070・TABLE-16。scripts/crash_test_unique_index.sh を実行）
.PHONY: crash-test-unique-index
crash-test-unique-index: ## 永続一意索引・クラッシュ耐性回帰テスト
	scripts/crash_test_unique_index.sh

# 分割実行 DML の crash 耐性回帰テスト（Issue #1131・RECOVER-11/PERSIST-1 ポインタ。scripts/crash_test_partitioned_dml.sh を実行）
.PHONY: crash-test-partitioned-dml
crash-test-partitioned-dml: ## 分割実行 DML の crash 耐性回帰テスト
	scripts/crash_test_partitioned_dml.sh

# テスト一時領域の残置検出ゲート（Issue #1303。scripts/check_tmp_leak.sh を実行。FULL=1 で workspace 全テスト）
.PHONY: tmp-leak-check
tmp-leak-check: ## テスト一時領域の残置検出ゲート
	scripts/check_tmp_leak.sh

# コア API（VectorCore/SearchProvider）シグネチャ差分検知（TASK-125・CORE-1。cargo 不要のテキスト比較）
.PHONY: core-api-check
core-api-check: ## コア API（VectorCore/SearchProvider）シグネチャ差分検知
	@scripts/core_api_check.sh

# RRF 融合等のソート非決定性 API 再混入検知（TASK-84・Issue #61。cargo 不要のテキスト比較）
.PHONY: sort-determinism-check
sort-determinism-check: ## RRF 融合等のソート非決定性 API 再混入検知
	@scripts/sort_determinism_check.sh

# TASK-156（CORE-14）aarch64 クロスコンパイル確認。cargo check のみ（リンクしないためクロスリンカ不要）。手元に target 未導入でも make ci を壊さないよう独立ターゲットとする（bench-* と同方針）。`contrast-bench` feature を付けないため usearch（TASK-127 CORE-5・Issue #176。C++ ビルドを伴う）は本コマンドの対象に含まれない
.PHONY: check-cross
check-cross: ## aarch64 クロスコンパイル確認（cargo check のみ）
	cargo check -p fandhe-db-engine --all-targets --target aarch64-unknown-linux-gnu

# SIMD カーネル（isa.rs）の生成コード検査。要素ごと挿入命令の不在を --emit asm で機械検査（Issue #467・TASK-156 関連。engine の release ビルドを伴う）
.PHONY: simd-codegen-check
simd-codegen-check: ## SIMD カーネルの生成コード検査（要素ごと挿入命令の不在）
	@scripts/simd_codegen_check.sh

# simd-codegen-check の aarch64 版（cross-check ジョブから実行。要 aarch64-unknown-linux-gnu target。リンク不要）
.PHONY: simd-codegen-check-cross
simd-codegen-check-cross: ## simd-codegen-check の aarch64 版
	@scripts/simd_codegen_check.sh cross

# TASK-73（WIRE-1）/TASK-82（SQL-5〜7,9,10）/TASK-165（SQL-12・SEARCH-9）/TASK-168（SQL-13・SQL-14）/TASK-187（SQL-11）/Issue #454（広域取得）/Issue #705・#706（TASK-153・ERR-5。commit 後 panic 注入フラグ・3 クライアント detail 到達検証）/Issue #943（WIRE-19。3 クライアントのトランザクション状態遷移検証）/Issue #1177（WIRE-17・WIRE-18・SQL-16。SCRAM／psql \copy 往復／複数行 INSERT と NoSQL rows[] パリティ）/Issue #1176（WIRE-11・WIRE-13・WIRE-14・SQL-10。拡張クエリ／型復元／バイナリ受信）/Issue #1339（SQL-32。分割実行 DML）/Issue #1347（SQL-21。RETURNING の返却行・タグ受領）/Issue #1354（RECOVER-12。新しい BEGIN 内の再送による明示トランザクションの成否確定）psql/psycopg/pg 実クライアント統合テスト（opt-in・`ci` には含めない。要 psql・python3+psycopg・node+pg。PSQL_BIN/PYTHON_BIN/NODE_BIN で上書き可）
.PHONY: e2e-three-client
e2e-three-client: ## psql/psycopg/pg 実クライアント統合テスト（opt-in・ci 対象外）
	@scripts/e2e_three_client.sh

# TASK-228（WIRE-9・WIRE-1）Issue #969: TLS 1.3 越しの psql/psycopg/pg 実クライアント統合テスト（opt-in・`ci` には含めない。要 psql・python3+psycopg・node+pg。PSQL_BIN/PYTHON_BIN/NODE_BIN で上書き可。証明書・鍵は RFC 8032 TEST 1 の公開テストベクタから実行時生成）
.PHONY: e2e-three-client-tls
e2e-three-client-tls: ## TLS 1.3 越しの実クライアント統合テスト（opt-in・ci 対象外）
	cargo test -p fandhe-db-wire-server --test wire9_tls -- --ignored --nocapture --test-threads=1

# TASK-183（HTTP-13）NoSQL 表層（--surface nosql）の無改造 HTTP クライアント（curl／urllib／fetch）統合テスト（opt-in・`ci` には含めない。要 curl・python3・node（≥18）・psql（Issue #779 の SQL 経路パリティ・Issue #877 の UPDATE/DELETE DML パリティ・Issue #950 の Phase 7 パリティ総合検証〔範囲比較・IN・OR・sort・offset・複数列 group_by・insert・DDL 3 op〕・Issue #1148 の FK 参照アクション〔ON DELETE CASCADE／ON UPDATE SET NULL・宣言時検査 42830・語彙外 42601〕パリティ検証に使用。PSQL_BIN で上書き可）。CURL_BIN／PYTHON_BIN／NODE_BIN で上書き可。各テストが `[e2e-record]` 行を stderr へ出力（秘密情報非含有）。記録様式は docs/design/three-client-e2e-harness.md 参照。Op::ALL 網羅ガード（parity_matrix_covers_every_nosql_op）は非 ignore で `make ci` から常時実行）
.PHONY: e2e-three-client-http
e2e-three-client-http: ## NoSQL 表層の HTTP クライアント統合テスト（opt-in・ci 対象外）
	cargo test -p fandhe-db-wire-server --test three_client_http_e2e -- --ignored --nocapture --test-threads=1

# cargo deny check advisories bans licenses sources（依存監査。cargo-deny 未導入なら自動導入）
.PHONY: deny
deny: ## cargo deny check（依存監査。未導入なら自動導入）
	@scripts/deny.sh

# scripts/ 配下の Python 補助スクリプトの単体テスト（python3 -m unittest discover scripts/tests。標準ライブラリのみ・cargo 不要）
.PHONY: scripts-test
scripts-test: ## scripts/ 配下の Python 補助スクリプトの単体テスト
	python3 -m unittest discover scripts/tests

# メイン worktree の origin/main からの遅れと pull 前リスクを読み取り専用で評価する（Issue #1329。REPO=<worktree> BASE_REF=origin/main REMOTE_CHECK=1 STRICT=1 を任意指定。環境依存の運用ツールのため ci には含めない）
.PHONY: worktree-lag-check
worktree-lag-check: ## メイン worktree の origin/main からの遅れを読み取り専用で評価する
	@scripts/check_worktree_lag.sh

# worktree-lag-check のセルフテスト（一時 git リポジトリで判定と読み取り専用性を検証。Issue #1329）
.PHONY: worktree-lag-check-selftest
worktree-lag-check-selftest: ## worktree-lag-check のセルフテスト
	@scripts/tests/check_worktree_lag_test.sh

.PHONY: ci
ci: lint-docs fmt-check lint test test-default-build crash-test crash-test-interrupt crash-test-cross-table crash-test-unique-index crash-test-partitioned-dml tmp-leak-check core-api-check sort-determinism-check scripts-test simd-codegen-check deny ## CI（ci.yml）と同等のチェックを一括実行する

# TASK-127 の性能・Recall 受け入れ基準回帰ベンチを実行する（時間依存のため ci には含めない。.github/workflows/bench.yml から週次 schedule / workflow_dispatch で実行）
.PHONY: bench-simd
bench-simd: ## 性能・Recall 受け入れ基準の回帰ベンチ（TASK-127）
	cargo bench --bench simd_bench -p fandhe-db-engine

# TASK-127 CORE-5（対照エンジンに対する p95 レイテンシ比率）の回帰ベンチを実行する（`contrast-bench` feature 限定・C++17 コンパイラが必要。時間依存のため ci には含めない。.github/workflows/bench.yml から週次 schedule / workflow_dispatch で実行）
.PHONY: bench-contrast
bench-contrast: ## 対照エンジン接続の回帰ベンチ（TASK-127 CORE-5・要 C++17）
	cargo bench --bench contrast_bench -p fandhe-db-engine --features contrast-bench

# TASK-130 のバッチ高速化受け入れ基準回帰ベンチを実行する（時間依存のため ci には含めない。.github/workflows/bench.yml から週次 schedule / workflow_dispatch で実行）
.PHONY: bench-batch
bench-batch: ## バッチ高速化受け入れ基準の回帰ベンチ（TASK-130）
	cargo bench --bench batch_bench -p fandhe-db-engine

# TASK-83（Conditional Go 条件7）の SQL 表層 C1 p95 再測定ベンチを実行する（時間依存のため ci には含めない。.github/workflows/bench.yml から workflow_dispatch のみで実行。schedule 化はしない）
.PHONY: bench-c1
bench-c1: ## SQL 表層 C1 p95 再測定ベンチ（TASK-83）
	cargo bench --bench sql_c1_bench -p fandhe-db-engine

# TASK-116（PLAN-4/6/7）のティア別レイテンシ受け入れ基準ベンチを実行する（時間依存・常駐 Ollama 前提のため ci には含めない。CI 経路は存在せず README「ティア別レイテンシ受け入れ基準の実測手順」記載の Actions 外の承認済み計測環境で運用者が直接実行する）
.PHONY: bench-tier
bench-tier: ## ティア別レイテンシ受け入れ基準ベンチ（TASK-116・要 Ollama）
	cargo bench --bench tier_latency_bench -p fandhe-db-engine

# Issue #324（境界同点グループ再取得ループ〔Issue #320〕のレイテンシ影響計測。CORE-7・PLAN-4/6/7 関連ポインタ）＋ Issue #506（BENCH_HYBRID_LATENCY_ENGINE 設定時は SQL 表層〔hnsw opt-in〕計測モードへ切替。既定〔未設定〕モードの出力は不変）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。BENCH_HYBRID_LATENCY_ENGINE=brute_force|hnsw|hnsw_f16 で SQL 表層モードを起動（未設定時は既定の in-build 比較モード）。BENCH_HYBRID_LATENCY_SCALE=small|large|all（既定 all）・BENCH_HYBRID_LATENCY_CORPUS=no_refetch|tie_refetch|all（既定 all）・BENCH_HYBRID_LATENCY_NUM_DOCS／_DIM／_VOCAB_SIZE／_QUANTIZE_LEVELS（既定はスケール別定数）・BENCH_HYBRID_LATENCY_EXPECT_RESUMED=1（tie_refetch の after 側計測にのみ指定。hybrid_resumed_rounds が 0 なら非 0 終了）
.PHONY: bench-hybrid
bench-hybrid: ## 境界同点グループ再取得ループのレイテンシ影響計測（Issue #324・#506）
	cargo bench --bench hybrid_latency_bench -p fandhe-db-engine

# Issue #506: 再開型探索（Issue #505・sql::hnsw_hybrid::HnswDenseProvider）の前後比較を ref_bf_large_tie5・hnsw_large_uniform・hnsw_large_tie5・hnsw_410shape_tie2 の 4 条件で交互 min-of-N 計測する（BEFORE_BIN・AFTER_BIN に退避済みバイナリの絶対パス、BEFORE_COMMIT・AFTER_COMMIT にビルド元コミットの hash を指定。AB_PAIRS（既定 5・5 未満は拒否）で交互ペア数を指定可。手動実行専用・CI 非配線。scripts/bench_hybrid_latency_ab.sh 参照）
.PHONY: bench-hybrid-ab
bench-hybrid-ab: ## 再開型探索の前後比較（Issue #506。BEFORE_BIN/AFTER_BIN 等が必要）
	scripts/bench_hybrid_latency_ab.sh

# Issue #360（SQL パース・束縛結果のセッション内キャッシュ検討）のパース・束縛コスト比率実測ベンチを実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）
.PHONY: bench-parse-bind
bench-parse-bind: ## SQL パース・束縛コスト比率の実測ベンチ（Issue #360）
	cargo bench --bench sql_parse_bind_bench -p fandhe-db-engine

# Issue #1204（述語 OR/IN・スカラー ORDER BY・2 テーブル結合）の p95 計測ベンチを実行する（述語グループは段別内訳〔median・Q1/Q3〕も出力。env: BENCH_RELATIONAL_P95_ROUNDS / _GROUP / _ROWS。時間依存・spec 閾値の CI 判定なし・手動実行専用のため ci には含めない）
.PHONY: bench-relational-p95
bench-relational-p95: ## 述語 OR/IN・ORDER BY・結合の p95 計測ベンチ（Issue #1204）
	cargo bench --bench relational_p95_bench -p fandhe-db-engine

# Issue #356（親 Issue #355。hybrid_rrf クエリの段別内訳プロファイル切り分け。SEARCH-1・SEARCH-3 関連ポインタ）＋ Issue #387（search_within の段別・疎側再取得発火回数）＋ Issue #465（Issue #392 適用後の最新基線ラウンド計測）＋ Issue #547（行数・可視率 opt-in）＋ Issue #660（SQL 表層固定コスト B1-B4 の S0〜S8 内訳再分解）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。BENCH_HYBRID_PROFILE_ROUNDS=<5-50>（既定 5）でラウンド数、BENCH_DEDICATED_ENV=1 で専有環境自己申告、BENCH_HYBRID_PROFILE_ROWS=<1-100000>（既定 25000）で行数、BENCH_HYBRID_PROFILE_VISIBLE_RATIO=1/<1-1000>（既定 1/1）で可視率を指定できる（Issue #547）
.PHONY: bench-hybrid-profile
bench-hybrid-profile: ## hybrid_rrf クエリの段別内訳プロファイル（Issue #356 ほか）
	cargo bench --bench hybrid_profile_bench -p fandhe-db-engine --features bench-internals

# Issue #547: #546（スコアアキュムレータ再利用）の前後比較を N=25k/100k・可視率 1/1・1/10 の 4 条件で交互 min-of-N 計測する（BEFORE_BIN・AFTER_BIN に退避済みバイナリの絶対パス、BEFORE_COMMIT・AFTER_COMMIT にビルド元コミットの hash を指定。AB_PAIRS（既定 5・5 未満は拒否）・AB_ROUNDS（既定 5・5..=50）で交互ペア数・ラウンド数を指定可。手動実行専用・CI 非配線。scripts/bench_hybrid_profile_ab.sh 参照）
.PHONY: bench-hybrid-profile-ab
bench-hybrid-profile-ab: ## スコアアキュムレータ再利用の前後比較（Issue #547）
	scripts/bench_hybrid_profile_ab.sh

# Issue #465（`hybrid_rrf` 6,178µs〔docs/design/crossdb-bench.md〕の engine 内 hybrid 経路／SQL 表層／wire 内訳を切り分ける）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。BENCH_HYBRID_WIRE_ROUNDS=<5-50>（既定 5）でラウンド数、BENCH_DEDICATED_ENV=1 で専有環境自己申告を指定できる
.PHONY: bench-hybrid-wire-profile
bench-hybrid-wire-profile: ## hybrid_rrf の wire／SQL 表層／engine 内訳プロファイル（Issue #465）
	cargo bench --bench hybrid_wire_profile_bench -p fandhe-db-wire-server

# Issue #362（KNN 経路の段別内訳プロファイル。走査・デコード・arena 構築・距離計算の切り分け）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。BENCH_KNN_PROFILE_ENGINE=brute_force|hnsw|hnsw_f16|hnsw_i8（既定 brute_force・Issue #413・#516・#523）で S0-cold/S0-hot の検索エンジンを ANN opt-in（Issue #403 B 案・hnsw_f16 は Issue #514 f16 常駐 opt-in）へ切り替えられる（S1〜S5' は非対象）。BENCH_KNN_PROFILE_DIM=<1-4096>（既定 128・Issue #466）でベクトル次元数を上書きできる。BENCH_KNN_PROFILE_VISIBLE_RATIO=1/<N>（Issue #487）で可視比率スイープへ切り替わる（S1〜S5' 非対象。BENCH_KNN_PROFILE_FULL_SCAN_RATIO=<num>/<den>〔engine=hnsw|hnsw_f16 限定〕・BENCH_KNN_PROFILE_SCALE=<1-40> と併用可）。BENCH_KNN_PROFILE_SPARSE_VISITED_MAX=<非負整数>〔engine=hnsw 限定・Issue #497〕で HNSW visited 集合切替閾値（`ValidatedHnswParams::with_sparse_visited_max`。既定 0＝常に dense）を上書きできる（S0-cold/S0-hot・可視比率スイープの双方に効く。#498 の計測用 knob）。BENCH_KNN_PROFILE_HOT_ONLY=1（Issue #516。VISIBLE_RATIO と排他）で S0-cold を省いた SQL 表層 e2e ホットパスのみを大規模点（scale 最大 40）向けに計測する。BENCH_KNN_PROFILE_INDEX_MEMORY=1（Issue #516。HOT_ONLY と排他・engine=hnsw|hnsw_f16 限定）で索引単体の常駐メモリ（子プロセス隔離計測）を出す
.PHONY: bench-knn-profile
bench-knn-profile: ## KNN 経路の段別内訳プロファイル（Issue #362）
	cargo bench --bench knn_profile_bench -p fandhe-db-engine

# Issue #487（可視比率〔1/2・1/4・1/10・1/20・1/50〕× 行数〔25k・100k〕での hnsw_subset と plain scan の損益分岐点を交互 N≥5 ペアで計測する）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。SWEEP_PAIRS=<N>〔既定 5〕でペア数を上書きできる。SWEEP_CANDIDATES=default|visited|acorn〔既定 default。visited は Issue #498 の sparse_visited_max 診断用 candidate、acorn は Issue #501・#502 の ACORN-1（2-hop 展開）opt-in 前後比較用 candidate〕・SWEEP_RATIOS／SWEEP_SCALES で候補セット・可視率・規模点を上書きできる。ログは target/bench-knn-visible-ratio/<unix-ts>/ 配下）
.PHONY: bench-knn-visible-ratio
bench-knn-visible-ratio: ## 可視比率別の hnsw_subset と plain scan の損益分岐点計測（Issue #487）
	scripts/bench_knn_visible_ratio_sweep.sh

# Issue #516（f16 常駐〔hnsw_f16〕と f32 常駐〔hnsw〕の前後比較〔25k／100k／500k 行 × dim 128／768〕・常駐メモリを交互 N≥5 ペア＋索引単体メモリ計測で記録する）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。AB_PAIRS=<N>〔既定 5〕・AB_POINTS="scale:dim ..."〔既定 "1:128 4:128 20:128 1:768 4:768"〕・AB_MEMORY_POINTS="scale:dim ..."〔既定 AB_POINTS + "20:768"〕で上書きできる。ログは target/bench-knn-f16-resident/<UTC ts>/ 配下。scripts/bench_knn_f16_resident_ab.sh --summarize <dir> で TSV 集約）
.PHONY: bench-knn-f16-resident
bench-knn-f16-resident: ## f16 常駐と f32 常駐の前後比較（Issue #516）
	scripts/bench_knn_f16_resident_ab.sh

# Issue #523（I8〔SQ8〕常駐〔hnsw_i8〕と f32 常駐〔hnsw〕の前後比較・常駐メモリを交互 N≥5 ペア＋索引単体メモリ計測で記録する。scripts/bench_knn_f16_resident_ab.sh の AB_CANDIDATE_ENGINE=hnsw_i8 opt-in で実行する〔#516 と同一スクリプト・同一 knob〕）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。AB_PAIRS=<N>〔既定 5〕・AB_POINTS="scale:dim ..."〔既定 "1:128 4:128 20:128 1:768 4:768"〕・AB_MEMORY_POINTS="scale:dim ..."〔既定 AB_POINTS + "20:768"〕で上書きできる。ログは target/bench-knn-i8-resident/<UTC ts>/ 配下。scripts/bench_knn_f16_resident_ab.sh --summarize <dir> で TSV 集約）
.PHONY: bench-knn-i8-resident
bench-knn-i8-resident: ## I8（SQ8）常駐と f32 常駐の前後比較（Issue #523）
	AB_CANDIDATE_ENGINE=hnsw_i8 scripts/bench_knn_f16_resident_ab.sh

# Issue #526（Apple M 実機での i8／f16／f32 経路の前後比較）。scripts/bench_knn_f16_resident_ab.sh の AB_CANDIDATE_ENGINES="hnsw_f16 hnsw_i8" opt-in（複数候補輪番）で hnsw〔f32〕・hnsw_f16・hnsw_i8 の 3 精度を同一計測セッションで一括計測する（#516・#523 と同一スクリプト・同一 knob。手順・記録テンプレートは docs/design/chip-kernel-guidelines.md §7.7 参照）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。AB_PAIRS=<N>〔既定 5〕・AB_POINTS="scale:dim ..."〔既定 "1:128 4:128 20:128 1:768 4:768"〕・AB_MEMORY_POINTS="scale:dim ..."〔既定 AB_POINTS + "20:768"〕で上書きできる。ログは target/bench-knn-precision-resident/<UTC ts>/ 配下。scripts/bench_knn_f16_resident_ab.sh --summarize <dir> で TSV 集約）
.PHONY: bench-knn-precision-resident
bench-knn-precision-resident: ## i8／f16／f32 経路の前後比較（Issue #526）
	AB_CANDIDATE_ENGINES="hnsw_f16 hnsw_i8" scripts/bench_knn_f16_resident_ab.sh

# Issue #463（`vector_knn` 786µs〔docs/design/crossdb-bench.md〕の wire／SQL 表層／距離カーネル・Top-k 内訳を切り分ける）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。BENCH_KNN_WIRE_ROUNDS=<5-50>（既定 5）でラウンド数、BENCH_DEDICATED_ENV=1 で専有環境自己申告を指定できる
.PHONY: bench-knn-wire-profile
bench-knn-wire-profile: ## vector_knn の wire／SQL 表層／距離カーネル内訳（Issue #463）
	cargo bench --bench knn_wire_profile_bench -p fandhe-db-wire-server

# Issue #365（isa.rs dot カーネルの複数アキュムレータ化）のマイクロベンチを実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BENCH_DOT_KERNEL_BLOCK_AB=1 で Issue #512 の行ブロックカーネル block4 A/B〔既定 Off・fail-closed パース〕、BENCH_DOT_KERNEL_TAIL_AB=1 で Issue #529 の dim 100／129／768 分岐なし tail A/B〔fail-closed env・既定 Off〕をそれぞれ追加計測する）
.PHONY: bench-dot-kernel
bench-dot-kernel: ## dot カーネルのマイクロベンチ（Issue #365）
	cargo bench --bench dot_kernel_bench -p fandhe-db-engine

# Issue #468（macOS 上の is_aarch64_feature_detected! 実効性検証）の feature 検出結果表を出力する（時間非依存・spec 閾値なしの情報提供専用のため ci には含めない。手動実行専用。出力は docs/design/chip-kernel-guidelines.md へ転記する運用）
.PHONY: detect-features
detect-features: ## macOS 上の aarch64 feature 検出結果表を出力する（Issue #468）
	cargo run -p fandhe-db-engine --release --example detect_features

# Issue #535（wgpu SUBGROUP 可用性設計）向けの adapter features／limits 表を出力する（時間非依存・spec 閾値なしの情報提供専用のため ci には含めない。手動実行専用。出力は docs/design/gpu-batch-topk.md へ転記する運用）
.PHONY: gpu-adapter-info
gpu-adapter-info: ## wgpu adapter の features／limits 表を出力する（Issue #535）
	cargo run -p fandhe-db-engine --release --example gpu_adapter_info

# Issue #464（docs/design/crossdb-bench.md で self が最劣後する agg_count／rls_isolation／vector_knn_where の redb 全行走査・ヘッダデコード・RLS 判定・dim/metadata デコード・WHERE 述語評価の段別内訳を切り分ける）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。BENCH_SCAN_PROFILE_ROUNDS=<5-50>（既定 5）でラウンド数、BENCH_SCAN_PROFILE_SCALE=<1-4>（既定 1＝25,000 行・4＝100,000 行）で規模、BENCH_SCAN_PROFILE_SELECTIVITY=1/<2-100>（既定 1/5・crossdb fixture 相当の 1/3 で選択率 33%。Issue #653・docs/design/filtered-distance-stage-profile.md）で lang='ja' 選択率、BENCH_DEDICATED_ENV=1 で専有環境自己申告を指定できる（1 プロセス = 1 規模点）。BENCH_SCAN_PROFILE_ENGINE=brute_force|hnsw|hnsw_f16|hnsw_i8（既定 brute_force・未設定時は出力・処理が導入前とビット同一のまま不変）・BENCH_SCAN_PROFILE_FULL_SCAN_RATIO=<n>/<d>（hnsw 系エンジン限定の opt-in override）で HNSW opt-in 時の Subset 縮退時委譲（Issue #676）の到達経路・レイテンシを追加計測できる（Issue #677。docs/design/hnsw-rls-cardinality-switch.md「Issue #677」節参照）
.PHONY: bench-scan-stage-profile
bench-scan-stage-profile: ## redb 全行走査の段別プロファイル（Issue #464）
	cargo bench --bench scan_stage_profile_bench -p fandhe-db-engine

# Issue #469（チップ別手動計測。bench-dot-kernel・bench-knn-profile・feature_bench〔dim 128／768〕を 1 ワークロード = 1 プロセスでラウンドロビン交互計測し CPU 情報付き summary.json を出力する）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BENCH_CHIP_ROUNDS=<1-50>〔既定 5。5 未満は参考値として自己ラベル〕・BENCH_CHIP_WORKLOADS=<dot_kernel,knn_profile,feature_128,feature_768 の部分集合。既定は全 4 種〕・BENCH_CHIP_OUT_DIR〔既定 target/bench-chip/<unix-ts>〕・BENCH_DEDICATED_ENV=1 で専有環境自己申告を指定できる。実測手順は README「チップ別カーネルの実測手順」参照）
.PHONY: bench-chip
bench-chip: ## チップ別手動計測（Issue #469）
	@scripts/bench_chip.sh

# Issue #530（Phase 4 通しのチップ別前後比較。親 #459・ルート #455）。`chip_bench`（Issue #469）を before/after 2 状態ディレクトリ（`git archive` で書き出した独立ワークツリー）で交互 min-of-N 実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BEFORE_DIR・AFTER_DIR 必須〔各ディレクトリで事前に `cargo bench --bench chip_bench -p fandhe-db-engine --no-run` 等のビルドを済ませておくこと〕。AB_PAIRS（既定 5・5 未満は拒否）で交互ペア数を指定可。BENCH_CHIP_WORKLOADS で計測対象ワークロードを絞り込み可。ログは _/bench/chip-ab/<UTC ts>/ 配下。scripts/bench_chip_ab.sh --summarize <dir> で TSV 集約）を実行する
.PHONY: bench-chip-ab
bench-chip-ab: ## チップ別前後比較（Issue #530。BEFORE_DIR/AFTER_DIR 必須）
	@scripts/bench_chip_ab.sh

# Issue #633（ScalarIndex 索引対象限定〔Issue #632〕の前後比較。Issue #645 でも同一手順で使用〔閾値見直し #644 の効果確認〕。Issue #655（候補 id マスク経路 #654 の crossdb 前後比較）でも同一手順で使用）。crossdb self 5 フェーズ〔hybrid_rrf・bulk_hybrid_k200・vector_knn_where・bulk_knn_where_k200・where_compound_count〕と WHERE 実行後 hybrid_rrf 単独ループ・wire-server RSS を before/after（既定で ref も）3 コミットの独立ソースツリー（`git archive` 展開）で交互 N≥5 ペア実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BEFORE_COMMIT・AFTER_COMMIT・CROSSDB_DIR〔docs25k.redb／docs25k.jsonl／queries200.jsonl を含むディレクトリ〕・CROSSDB_PYTHON〔psycopg 入り python3〕必須。REF_COMMIT（既定 ee99db3・空文字で無効）・AB_PAIRS（既定 5・5 未満は拒否）・HYBRID_ITERS（既定 200）・WARM_WHERE（既定 50）・CROSSDB_SELF_PORT_BASE（既定 15437）で上書きできる。ログは docs/design/bench-data/scalar-index-crossdb-ab/ 配下。scripts/bench_scalar_index_crossdb_ab.sh --summarize <dir> で TSV 集約）を実行する
.PHONY: bench-scalar-index-crossdb-ab
bench-scalar-index-crossdb-ab: ## ScalarIndex の crossdb 前後比較（Issue #633。BEFORE/AFTER_COMMIT ほか必須）
	@scripts/bench_scalar_index_crossdb_ab.sh

# Issue #655（候補 id マスク経路〔Issue #654〕前後比較。親 #650・ルート #649）。SCALAR 事前フィルタ付き DISTANCE の段別プロファイル（`scan_stage_profile_bench`。Issue #653）を before（#654 未適用）/after（#654 適用後）2 コミットの独立ソースツリー（`git archive` 展開）で交互 N≥5 ペア実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BEFORE_DIR・AFTER_DIR・BEFORE_COMMIT・AFTER_COMMIT 必須。AB_PAIRS（既定 5・5 未満は拒否）・AB_ROUNDS（既定 5）・AB_AFTER_ONLY_SELECTIVITY（既定 1/3・空文字で after-only 段を無効化）・OUT_DIR で上書きできる。ログは docs/design/bench-data/filtered-distance-mask-ab/ 配下。scripts/bench_filtered_distance_ab.sh --summarize <dir> で TSV 集約。crossdb 側の前後比較は bench-scalar-index-crossdb-ab を使う）を実行する
.PHONY: bench-filtered-distance-ab
bench-filtered-distance-ab: ## 候補 id マスク経路の前後比較（Issue #655。BEFORE/AFTER_DIR・COMMIT 必須）
	@scripts/bench_filtered_distance_ab.sh

# Issue #658（crossdb self の exact/hnsw 構成を同一バイナリで交互 N≥5 ペア実行し前後比較する）。事前に `cargo build --release -p fandhe-db-wire-server` と `cargo build --release -p fandhe-db-engine --example crossdb_plan_probe` が必要（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。CROSSDB_DIR〔docs25k.redb／queries200.jsonl を含むディレクトリ〕・CROSSDB_PYTHON〔psycopg 入り python3〕必須。AB_PAIRS（既定 5・5 未満は拒否）・CROSSDB_SELF_PORT（既定 15438）・CROSSDB_SELF_HNSW_ARGS（hnsw arm にのみ適用される `--hnsw-*` opt-in）で上書きできる。ログは docs/design/bench-data/crossdb-self-hnsw-ab/ 配下。scripts/bench_crossdb_self_hnsw_ab.sh --summarize <dir> で TSV 集約）を実行する
.PHONY: bench-crossdb-self-hnsw-ab
bench-crossdb-self-hnsw-ab: ## crossdb self の exact/hnsw 前後比較（Issue #658。CROSSDB_DIR/PYTHON 必須）
	@scripts/bench_crossdb_self_hnsw_ab.sh

# Issue #396（ingest 経路の段別内訳プロファイル。所有権検査・content_hash・台帳記録・encode・redb insert・世代更新・commit の切り分け）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BENCH_INGEST_PROFILE_MODE=batch|single〔既定 batch。single は Issue #484: 単文 INSERT 経路の P0/E0/S0/I1〜I8 内訳〕。batch モード: BENCH_INGEST_PROFILE_ROWS／BENCH_INGEST_PROFILE_DIM で規模を上書き可能。BENCH_INGEST_PROFILE_INSERT_MODE=insert|reserve で I6 段の redb insert_reserve A/B 計測モードを切替可能〔Issue #400・既定 insert・single モードは insert のみ対応〕。single モード: BENCH_INGEST_PROFILE_STATEMENTS（既定 25,000・2,000〜100,000）で単文数を上書き可能。BENCH_INGEST_PROFILE_DURABILITY=immediate|none〔Issue #851・既定 immediate・single モードのみ対応〕で durability A/B 計測モードを切替可能）
.PHONY: bench-ingest-profile
bench-ingest-profile: ## ingest 経路の段別内訳プロファイル（Issue #396）
	cargo bench --bench ingest_profile_bench -p fandhe-db-engine

# Issue #851（`ingest_single_stmt` の durability 別実測。既定 immediate と opt-in none を同一バイナリ・同一 fixture で交互 N≥5 ペア実行し、I8〔redb commit〕・E0/S0 tier・fsync 系原始操作の参考値を記録する）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。AB_PAIRS=<N>〔既定 5〕・STATEMENTS=<N>〔既定 5,000・2,000〜100,000〕・FSYNC_PROBE_DIR=<dir>〔既定 ${TMPDIR:-/tmp}＝DB 実配置先ボリューム。Issue #857〕で上書きできる。ログは target/bench-ingest-durability-ab/<UTC ts>/ 配下。python3 scripts/bench_ingest_durability_ab_summarize.py <dir> で TSV 集約）
.PHONY: bench-ingest-durability-ab
bench-ingest-durability-ab: ## ingest_single_stmt の durability 別実測（Issue #851）
	scripts/bench_ingest_durability_ab.sh

# Issue #484（単文 INSERT の wire 往復内訳。`bench-ingest-profile MODE=single` が計測する engine 内部段を補い wire プロトコル層自体の寄与を切り分ける）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。BENCH_INGEST_WIRE_ROWS（既定 25,000・5,000〜100,000。BENCH_INGEST_WIRE_ROUNDS で割り切れる値のみ）・BENCH_INGEST_WIRE_ROUNDS=<5-50>（既定 5）でラウンド数、BENCH_DEDICATED_ENV=1 で専有環境自己申告を指定できる
.PHONY: bench-ingest-wire-profile
bench-ingest-wire-profile: ## 単文 INSERT の wire 往復内訳（Issue #484）
	cargo bench --bench ingest_wire_profile_bench -p fandhe-db-wire-server

# TASK-132（Issue #404。HNSW グラフ構築の受け入れ条件 (b): 構築計算量が規模に対してほぼ N log N であることの簡易ベンチ確認）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）
.PHONY: bench-hnsw-build
bench-hnsw-build: ## HNSW 構築計算量の簡易ベンチ（TASK-132）
	cargo bench --bench hnsw_build_bench -p fandhe-db-engine

# Issue #406（HNSW 構築の並列化の受け入れ条件 (b): 100k 点で構築時間がスレッド数に応じて短縮することの実測記録）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BENCH_HNSW_PARALLEL_ROWS／BENCH_HNSW_PARALLEL_THREADS で規模・スレッド数ラダーを上書き可。Issue #495 追記: CSR 平坦化段 `flatten=`（逐次縮退経路は 0ms・並列経路は 0 超）・各 threads 点の常駐メモリ実測行〔`approx_heap_bytes`／VmRSS 前後差／VmHWM〕を出力する）
.PHONY: bench-hnsw-parallel-build
bench-hnsw-parallel-build: ## HNSW 並列構築のスレッド数別実測（Issue #406）
	cargo bench --bench hnsw_parallel_build_bench -p fandhe-db-engine

# GPU バッチ検索（engine::gpu_batch）が CPU-SIMD バッチ経路に対してどの規模・バッチサイズから優位になるかを実測する（時間依存・GPU 実機必須・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BENCH_GPU_SCALING_ROWS／BENCH_GPU_SCALING_DIMS／BENCH_GPU_SCALING_BATCH／BENCH_GPU_SCALING_TOPK／BENCH_GPU_SCALING_ITERS で規模・バッチ・k・反復回数を上書き可）
.PHONY: bench-gpu-scaling
bench-gpu-scaling: ## GPU バッチ検索のスケーリング実測（要 GPU）
	cargo bench --bench gpu_scaling_bench -p fandhe-db-engine

# self（wire-server 経由）と pgvector / sqlite-vec / Qdrant / LanceDB / MySQL を機能別に比較する（Docker・Python venv・`cargo build --release -p fandhe-db-wire-server`・seed_docs 生成 fixture が必要。CROSSDB_DIR〔fixture ディレクトリ〕と CROSSDB_PYTHON〔venv の python〕を必須指定。任意 CROSSDB_DIM（十進数字のみ・例 768。Issue #466）で dim 別 fixture 名（docs25k-d<dim>.*／queries200-d<dim>.jsonl）・results/logs サブディレクトリへ切替。self は exact 構成に加え hnsw 構成（`--config hnsw`。Issue #658）も自動で回す。事前に `cargo build --release -p fandhe-db-engine --example crossdb_plan_probe` が必要——未ビルドだと self/hnsw 行のみ FAILED として記録され本ターゲット全体が非 0 終了する。時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）
.PHONY: bench-crossdb
bench-crossdb: ## self と対照 DB の機能別比較（CROSSDB_DIR/PYTHON 必須）
	@scripts/bench_crossdb.sh

# Issue #848（`run_all.sh` フル一括〔self exact/hnsw/nosql・対照 DB 全系統。Elasticsearch を含む〕を N≥5 ラウンド輪番実行し `docs/design/benchmark-judgement-policy.md` 規約〔per-run 生データ・min-of-N＋median・run-to-run 幅〕で self との勝敗を集計する。事前に `cargo build --release -p fandhe-db-wire-server` と `cargo build --release -p fandhe-db-engine --example crossdb_plan_probe` が必要。時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。CROSSDB_DIR・CROSSDB_PYTHON 必須。AB_PAIRS（既定 5・5 未満は拒否）で上書き可能。生データは `$(CROSSDB_DIR)/results/<ts>-round<N>/`（セッション起動時刻 `<ts>` を含む。複数セッションの残留混在を防ぐ）・環境記録は `docs/design/bench-data/crossdb-<ts>-ab/env.txt`。`scripts/bench_crossdb_ab.sh --summarize <dir> <rounds> [<round_dir_prefix>]` で Markdown 集約表を出力（`round_dir_prefix` は本コマンドの標準出力最終行が示す値を使う。省略時は既定 `round`）を実行する
.PHONY: bench-crossdb-ab
bench-crossdb-ab: ## crossdb run_all の輪番前後比較（Issue #848。CROSSDB_DIR/PYTHON 必須）
	@scripts/bench_crossdb_ab.sh

# 自作 HNSW と usearch の構築時間（スレッド数ラダー）・Recall@10・探索レイテンシを同一条件で比較する（`contrast-bench` feature 限定・C++17 コンパイラが必要。self・usearch の 2 エンジンとも同一の L2 正規化済みコーパス・クエリで評価するため Recall@10 を単純比較できる。時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。BENCH_HNSW_COMPARE_ROWS／BENCH_HNSW_COMPARE_DIM／BENCH_HNSW_COMPARE_THREADS／BENCH_HNSW_COMPARE_QUERIES で条件を上書き可）
.PHONY: bench-hnsw-compare
bench-hnsw-compare: ## 自作 HNSW と usearch の比較（要 C++17）
	cargo bench --bench hnsw_compare_bench -p fandhe-db-engine --features contrast-bench

# Issue #491（受理判定後 prefetch〔Issue #490・PR #574〕の前後比較実測）の 1 規模点計測を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。before/after バイナリを交互起動する前後比較・8 点〔10k／100k・dim 128／768・マスク有無〕の判定は運用者が行う。BENCH_HNSW_SEARCH_ROWS〔既定 10000・1..=200000〕・BENCH_HNSW_SEARCH_DIM〔既定 128・1..=4096〕・BENCH_HNSW_SEARCH_MASK〔既定 none・1..=99 の可視率%〕・BENCH_HNSW_SEARCH_QUERIES〔既定 200〕・BENCH_HNSW_SEARCH_EF〔既定 64〕・BENCH_HNSW_SEARCH_K〔既定 10〕・BENCH_DEDICATED_ENV=1 で専有環境自己申告・BENCH_HNSW_SEARCH_COMMIT〔ビルド時指定。git archive 再現手順で before/after バイナリへ計測対象コミットを焼き込むため必須。詳細は docs/design/hnsw-search.md「再現方法」節参照〕・BENCH_HNSW_SEARCH_SPARSE_VISITED_MAX〔Issue #498。dense=0／sparse=18446744073709551615 の 2 arm のみ。`--features bench-internals` が必須——`make bench-hnsw-search-visited` を使う〕を指定できる）
.PHONY: bench-hnsw-search
bench-hnsw-search: ## HNSW 探索の 1 規模点計測（Issue #491）
	cargo bench --bench hnsw_search_bench -p fandhe-db-engine

# Issue #498（visited 集合切替閾値 sparse_visited_max の可視比率別 dense/sparse 前後比較。層 1）を実行する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用。`--features bench-internals` で hnsw_search_bench をビルドし、規模点〔既定 10000／100000〕× 可視率〔既定 50/25/10/5/2%〕ごとに dense/sparse を交互 N≥5 ペアで計測する。AB_PAIRS=<N>〔既定 5〕・AB_ROWS="<rows...>"・AB_MASKS="<percent...>" で上書きできる。ログは target/bench-hnsw-search-visited/<unix-ts>/ 配下。scripts/bench_hnsw_search_visited_ab.sh --summarize <dir> で一覧化できる）
.PHONY: bench-hnsw-search-visited
bench-hnsw-search-visited: ## visited 集合切替閾値の前後比較（Issue #498）
	scripts/bench_hnsw_search_visited_ab.sh

# TASK-132（Issue #405。HNSW 探索の受け入れ条件 (a): 10k×dim128 の決定的フィクスチャで Recall@10（ef=64/256）が brute-force 対照で閾値以上であることを実測する）を実行する（debug では構築に約 110s かかるため #[ignore]・release 実行専用。ci には含めない。実測値は標準出力へ出す）
.PHONY: hnsw-search-recall
hnsw-search-recall: ## HNSW 探索の Recall@10 実測（TASK-132）
	cargo test --release -p fandhe-db-engine --test hnsw_search -- --ignored --nocapture

# Issue #523（R5。I8（SQ8）常駐 opt-in の brute-force 対照 Recall@10 を F32 常駐対比で ef ∈ {64, 128, 256} 掃引し、oversampling（ef 引き上げ）で補えるかの判断材料を標準出力へ出す。crates/engine/tests/hnsw_i8_recall.rs。層 A は make ci 対象・層 B は #[ignore]・release 実行専用）
.PHONY: hnsw-i8-recall
hnsw-i8-recall: ## I8 常駐の Recall@10 を ef 掃引で実測（Issue #523）
	cargo test --release -p fandhe-db-engine --test hnsw_i8_recall -- --ignored --nocapture

# Issue #502（ACORN-1〔2-hop 展開〕の可視比率別 Recall 回帰の層 B: 25,000 行・dim128 で可視比率 1/2・1/4・1/5・1/10 を横断し Recall@10・レジーム分類を標準出力へ記録する）を実行する（層 A は make ci 対象・crates/engine/tests/hnsw_acorn_recall.rs。層 B は #[ignore]・release 実行専用。同ファイルの Issue #679 決定的フィクスチャ層 B レポートも一緒に走る。単独実行は make hnsw-acorn-twohop-runs 参照）
.PHONY: hnsw-acorn-recall
hnsw-acorn-recall: ## ACORN-1 の可視比率別 Recall 回帰 層 B（Issue #502）
	cargo test --release -p fandhe-db-engine --test hnsw_acorn_recall -- --ignored --nocapture

# Issue #679（ACORN TwoHop へ確実に到達する決定的フィクスチャ〔クラスタ丸ごと可視マスク〕で 25,000 行・dim128 の hop モード別 Recall・acorn_expansions を RUNS 回連続測定し標準出力へ記録する。既定 RUNS=5。crates/engine/tests/hnsw_acorn_recall.rs::layer_b_25k_dim128_cluster_mask_hop_mode_report。層 A は make ci 対象・層 B は #[ignore]・release 実行専用）
.PHONY: hnsw-acorn-twohop-runs
hnsw-acorn-twohop-runs: ## ACORN TwoHop の Recall を RUNS 回連続測定（Issue #679）
	HNSW_ACORN_RECALL_RUNS=$(RUNS) cargo test --release -p fandhe-db-engine --test hnsw_acorn_recall -- --ignored --nocapture --exact layer_b_25k_dim128_cluster_mask_hop_mode_report

# Issue #659（選択率 33%〔crossdb fixture の lang='ja'〕での ann_masked／mask_splits_graph／plain scan 到達分類・ACORN-1 比較・既定エンジン対照 Recall を arm 表として標準出力へ記録する）を実行する（層 A は make ci 対象・crates/engine/tests/hnsw_crossdb_selectivity.rs。層 B は #[ignore]・release 実行専用。CROSSDB_DIR〔docs25k.redb・queries200.jsonl を含むディレクトリ〕必須）
.PHONY: hnsw-crossdb-selectivity
hnsw-crossdb-selectivity: ## 選択率 33% の到達分類・Recall 比較（Issue #659。CROSSDB_DIR 必須）
	@scripts/hnsw_crossdb_selectivity.sh

# Issue #358（疎索引キャッシュ導入後の Recall 非劣化検証。SEARCH-1・SEARCH-3 関連ポインタ）の大規模段 cold/hot 等価性テストを実行する（数万件規模で cargo test の既定実行時間を超えるため #[ignore]・手動実行専用。ci には含めない）
.PHONY: sparse-cache-recall-large
sparse-cache-recall-large: ## 疎索引キャッシュの大規模段等価性テスト（Issue #358）
	cargo test -p fandhe-db-engine --test sparse_cache_recall -- --ignored

# Issue #333（SEARCH-7 方式変更）の実 ONNX クロスエンコーダによる自然言語 fixture 実測を実行する（手動・ローカル専用。ci には含めない。CROSS_ENCODER_MODEL_PATH・CROSS_ENCODER_TOKENIZER_PATH・ORT_DYLIB_PATH の環境変数指定が必要。実測値・再現手順は docs/design/rerank-recall-regression.md「Issue #333」節参照）
.PHONY: rerank-cross-encoder-eval
rerank-cross-encoder-eval: ## 実 ONNX クロスエンコーダの fixture 実測（Issue #333）
	cargo test --release -p fandhe-db-engine --features cross-encoder --test rerank_cross_encoder_recall -- --ignored --nocapture

# TASK-104 のハイブリッド検索 Recall 閾値ゲート（層 B）を実行する（spec 閾値の環境変数注入が必要。ci には含めない。.github/workflows/recall.yml から実行。標準出力は対象名と pass/fail のみ。実測値は RECALL_VERBOSE=1〔GitHub Actions 外限定〕。Issue #303。RECALL_ENGINE=hnsw で ANN opt-in 経路、RECALL_ENGINE=hnsw_f16 で f16 常駐 opt-in 経路、RECALL_ENGINE=hnsw_i8 で I8（SQ8）常駐 opt-in 経路を測定〔既定 brute_force。Issue #412・#515・#523〕）
.PHONY: recall-regression
recall-regression: ## ハイブリッド検索 Recall 閾値ゲート 層 B（TASK-104）
	cargo test --release -p fandhe-db-engine --test hybrid_recall -- --ignored --nocapture

# TASK-108 のリランキング効果測定 Recall 閾値ゲート（層 B）を実行する（spec 閾値の環境変数注入が必要。ci には含めない。.github/workflows/recall.yml から実行。標準出力は対象名と pass/fail のみ。実測値は RECALL_VERBOSE=1〔GitHub Actions 外限定〕。Issue #303。RECALL_ENGINE=hnsw で ANN opt-in 経路、RECALL_ENGINE=hnsw_f16 で f16 常駐 opt-in 経路、RECALL_ENGINE=hnsw_i8 で I8（SQ8）常駐 opt-in 経路を測定〔既定 brute_force。Issue #412・#515・#523〕）
.PHONY: rerank-regression
rerank-regression: ## リランキング効果測定 Recall 閾値ゲート 層 B（TASK-108）
	cargo test --release -p fandhe-db-engine --test rerank_recall -- --ignored --nocapture

# TASK-112・TASK-113 のクエリ展開受け入れ基準（intent 改善幅・direct 維持・劣化展開時の intent 改善幅・大規模段 direct 絶対下限）Recall 閾値ゲート（層 B。--ignored 一括実行のため大規模段ゲートも対象に含む）を実行する（spec 閾値の環境変数注入が必要。ci には含めない。.github/workflows/recall.yml から実行。標準出力は対象名と pass/fail のみ。実測値は RECALL_VERBOSE=1〔GitHub Actions 外限定〕。Issue #303。RECALL_ENGINE=hnsw で ANN opt-in 経路、RECALL_ENGINE=hnsw_f16 で f16 常駐 opt-in 経路、RECALL_ENGINE=hnsw_i8 で I8（SQ8）常駐 opt-in 経路を測定〔既定 brute_force。Issue #412・#515・#523〕）
.PHONY: query-planning-regression
query-planning-regression: ## クエリ展開受け入れ基準の閾値ゲート 層 B（TASK-112・113）
	cargo test --release -p fandhe-db-engine --test query_planning_recall -- --ignored --nocapture

# TASK-163 の precision モード評価基準（Top-1 Accuracy・MRR@10・誤返却率）閾値ゲートのみを実行する（pass/fail のみ出力・実測値は出さない。RECALL_VERBOSE=1〔GitHub Actions 外限定〕opt-in 時のみ実測値を追加出力。目標値未確定のため .github/workflows/recall.yml には未接続。ci には含めない。Issue #303）
.PHONY: precision-regression
precision-regression: ## precision モード評価基準の閾値ゲート（TASK-163）
	cargo test --release -p fandhe-db-engine --test precision_eval -- --ignored --nocapture --exact precision_eval_threshold_gate

# TASK-163 の判断材料レポート（hybrid/dense の指標）とパラメータ感度スイープを実行する（実測値を標準出力へ出すためローカル専用。CI・GitHub Actions からは実行しない。GITHUB_ACTIONS 下ではテスト側が fail-closed で拒否する。Issue #303）
.PHONY: precision-report
precision-report: ## precision 判断材料レポートとパラメータ感度スイープ（TASK-163）
	@scripts/precision_report.sh

# Issue #482（接続処理モデルの判断記録。1 接続 1 スレッド ＋ 接続数上限）の同時接続数 N 別スループットを実測する（時間依存・spec 閾値を持たない情報提供専用のため ci には含めない。CI ワークフローにも配線しない。手動実行専用）。WIRE_CONCURRENCY_N（必須。1〜64）で同時接続数を指定する。1 プロセス = 1 規模点（docs/design/benchmark-judgement-policy.md §5 準拠）。WIRE_CONCURRENCY_ROWS／WIRE_CONCURRENCY_DIM／WIRE_CONCURRENCY_ROUNDS で規模を上書きできる（既定 25,000 行・dim 128・200 往復）
.PHONY: bench-wire-concurrency
bench-wire-concurrency: ## 同時接続数別スループット実測（Issue #482。WIRE_CONCURRENCY_N 必須）
	cargo test --release -p fandhe-db-wire-server --test wire_concurrency_throughput -- --ignored --nocapture

# Issue #738（TASK-95・TABLE-12・RLS-9）(b) 他テナント保持 id・(c) 未存在 id への自テナント名義 INSERT の wire 往復レイテンシ分布が統計的に区別できないことを実測する（`#[ignore]`・release 専用・spec 閾値を持たない同一性検証のため ci 非配線・CI ワークフローにも配線しない。手動実行専用）。TENANT_LATENCY_ROUNDS（既定 200・200 未満は拒否）／TENANT_LATENCY_WARMUP（既定 20）で規模を上書きできる。1 プロセス = 1 計測（docs/design/benchmark-judgement-policy.md §5 準拠）。実行記録（日付・SHA・rounds・両腕 median/p95・A/A 帯・判定）は PR 本文へ転記する
.PHONY: wire-tenant-latency
wire-tenant-latency: ## 他テナント id と未存在 id の INSERT レイテンシ同一性検証（Issue #738）
	@scripts/wire_tenant_latency.sh

# scripts/eval のユニットテストを実行する（TASK-118。python3 が必要）
.PHONY: test-eval
test-eval: ## scripts/eval のユニットテストを実行する
	@scripts/test_eval.sh

.PHONY: docker-build
docker-build: ## 開発コンテナイメージをビルドする
	docker compose build

.PHONY: docker-shell
docker-shell: ## 開発コンテナのシェルに入る
	docker compose run --rm dev

# コンテナ内で make ci を実行する（環境非依存の検証）
.PHONY: docker-ci
docker-ci: ## コンテナ内で make ci を実行する
	docker compose run --rm dev make ci
