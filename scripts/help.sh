#!/usr/bin/env bash
# Make を介さない操作一覧。`make help` は Makefile の `## ` コメントから機械的に生成されるのに
# 対し、本ファイルは手動で保守する一覧である。Makefile のターゲットを追加・改名・削除したら
# 同じ変更で本ファイルも更新すること（scripts/tests/test_help_sync.py が一致を検査する）。
# 変更手順は .claude/rules/makefile.md を参照。
set -euo pipefail

if [ "$#" -ne 0 ]; then
    echo "usage: help.sh (no arguments)" >&2
    exit 2
fi

cat <<'EOF'
このリポジトリの操作と、Make を使わずに直接実行する方法
（`make <target>` は下記の直接コマンドを呼ぶだけの薄い入口。同名の環境変数・引数はそのまま渡る。
  `$` 行が複数ある項目は上から順に実行する）:

  setup
      開発環境を一括構築する
      $ scripts/setup.sh
  rustup
      rustup（cargo）を未導入の場合のみ導入する
      $ scripts/install_rustup.sh
  submodule
      docs/spec サブモジュールを初期化・更新する（取得失敗は警告のみ）
      $ scripts/init_submodule.sh
  hooks
      lefthook の git hooks を導入する
      $ scripts/install_hooks.sh
  lint-md
      markdownlint を実行する
      $ scripts/lint_docs.sh md
  lint-yaml
      yamllint を実行する
      $ scripts/lint_docs.sh yaml
  lint-editorconfig
      editorconfig-checker を実行する
      $ scripts/lint_docs.sh editorconfig
  lint-commits
      commitlint（origin/main からの分岐点以降のコミット）を実行する
      $ scripts/lint_docs.sh commits
  lint-docs
      ドキュメント／設定ファイル系 lint を一括実行する
      $ scripts/lint_docs.sh md
      $ scripts/lint_docs.sh yaml
      $ scripts/lint_docs.sh editorconfig
      $ scripts/lint_docs.sh commits
  fmt
      cargo fmt --all で整形する
      $ cargo fmt --all
  fmt-check
      cargo fmt --check（整形差分の検出）
      $ cargo fmt --all --check
  lint
      cargo clippy -D warnings
      $ cargo clippy --workspace --all-targets --all-features -- -D warnings
  test
      cargo test（workspace 全体）
      $ cargo test --workspace --all-features
  test-default-build
      既定ビルド（fault-injection 無効）専用の回帰テスト
      $ cargo test -p fandhe-db-wire-server --test wire_fault_injection_cli
  crash-test
      クラッシュ耐性回帰テスト（PERSIST-1）
      $ scripts/crash_test.sh
  crash-test-interrupt
      crash_test.sh の中断パス（SIGTERM・プロセスグループ）のセルフテスト
      $ scripts/crash_test_interrupt.sh
  crash-test-cross-table
      2 テーブル横断トランザクション・クラッシュ耐性回帰テスト
      $ scripts/crash_test_cross_table.sh
  crash-test-unique-index
      永続一意索引・クラッシュ耐性回帰テスト
      $ scripts/crash_test_unique_index.sh
  crash-test-partitioned-dml
      分割実行 DML の crash 耐性回帰テスト
      $ scripts/crash_test_partitioned_dml.sh
  tmp-leak-check
      テスト一時領域の残置検出ゲート
      $ scripts/check_tmp_leak.sh
  core-api-check
      コア API（VectorCore/SearchProvider）シグネチャ差分検知
      $ scripts/core_api_check.sh
  sort-determinism-check
      RRF 融合等のソート非決定性 API 再混入検知
      $ scripts/sort_determinism_check.sh
  check-cross
      aarch64 クロスコンパイル確認（cargo check のみ）
      $ cargo check -p fandhe-db-engine --all-targets --target aarch64-unknown-linux-gnu
  simd-codegen-check
      SIMD カーネルの生成コード検査（要素ごと挿入命令の不在）
      $ scripts/simd_codegen_check.sh
  simd-codegen-check-cross
      simd-codegen-check の aarch64 版
      $ scripts/simd_codegen_check.sh cross
  e2e-three-client
      psql/psycopg/pg 実クライアント統合テスト（opt-in・ci 対象外）
      $ scripts/e2e_three_client.sh
  e2e-three-client-tls
      TLS 1.3 越しの実クライアント統合テスト（opt-in・ci 対象外）
      $ cargo test -p fandhe-db-wire-server --test wire9_tls -- --ignored --nocapture --test-threads=1
  e2e-three-client-http
      NoSQL 表層の HTTP クライアント統合テスト（opt-in・ci 対象外）
      $ cargo test -p fandhe-db-wire-server --test three_client_http_e2e -- --ignored --nocapture --test-threads=1
  deny
      cargo deny check（依存監査。未導入なら自動導入）
      $ scripts/deny.sh
  scripts-test
      scripts/ 配下の Python 補助スクリプトの単体テスト
      $ python3 -m unittest discover scripts/tests
  worktree-lag-check
      メイン worktree の origin/main からの遅れを読み取り専用で評価する
      $ scripts/check_worktree_lag.sh
  worktree-lag-check-selftest
      worktree-lag-check のセルフテスト
      $ scripts/tests/check_worktree_lag_test.sh
  ci
      CI（ci.yml）と同等のチェックを一括実行する
      $ scripts/lint_docs.sh md
      $ scripts/lint_docs.sh yaml
      $ scripts/lint_docs.sh editorconfig
      $ scripts/lint_docs.sh commits
      $ cargo fmt --all --check
      $ cargo clippy --workspace --all-targets --all-features -- -D warnings
      $ cargo test --workspace --all-features
      $ cargo test -p fandhe-db-wire-server --test wire_fault_injection_cli
      $ scripts/crash_test.sh
      $ scripts/crash_test_interrupt.sh
      $ scripts/crash_test_cross_table.sh
      $ scripts/crash_test_unique_index.sh
      $ scripts/crash_test_partitioned_dml.sh
      $ scripts/check_tmp_leak.sh
      $ scripts/core_api_check.sh
      $ scripts/sort_determinism_check.sh
      $ python3 -m unittest discover scripts/tests
      $ scripts/simd_codegen_check.sh
      $ scripts/deny.sh
  bench-simd
      性能・Recall 受け入れ基準の回帰ベンチ（TASK-127）
      $ cargo bench --bench simd_bench -p fandhe-db-engine
  bench-contrast
      対照エンジン接続の回帰ベンチ（TASK-127 CORE-5・要 C++17）
      $ cargo bench --bench contrast_bench -p fandhe-db-engine --features contrast-bench
  bench-batch
      バッチ高速化受け入れ基準の回帰ベンチ（TASK-130）
      $ cargo bench --bench batch_bench -p fandhe-db-engine
  bench-c1
      SQL 表層 C1 p95 再測定ベンチ（TASK-83）
      $ cargo bench --bench sql_c1_bench -p fandhe-db-engine
  bench-tier
      ティア別レイテンシ受け入れ基準ベンチ（TASK-116・要 Ollama）
      $ cargo bench --bench tier_latency_bench -p fandhe-db-engine
  bench-hybrid
      境界同点グループ再取得ループのレイテンシ影響計測（Issue #324・#506）
      $ cargo bench --bench hybrid_latency_bench -p fandhe-db-engine
  bench-hybrid-ab
      再開型探索の前後比較（Issue #506。BEFORE_BIN/AFTER_BIN 等が必要）
      $ scripts/bench_hybrid_latency_ab.sh
  bench-parse-bind
      SQL パース・束縛コスト比率の実測ベンチ（Issue #360）
      $ cargo bench --bench sql_parse_bind_bench -p fandhe-db-engine
  bench-relational-p95
      述語 OR/IN・ORDER BY・結合の p95 計測ベンチ（Issue #1204）
      $ cargo bench --bench relational_p95_bench -p fandhe-db-engine
  bench-hybrid-profile
      hybrid_rrf クエリの段別内訳プロファイル（Issue #356 ほか）
      $ cargo bench --bench hybrid_profile_bench -p fandhe-db-engine --features bench-internals
  bench-hybrid-profile-ab
      スコアアキュムレータ再利用の前後比較（Issue #547）
      $ scripts/bench_hybrid_profile_ab.sh
  bench-hybrid-wire-profile
      hybrid_rrf の wire／SQL 表層／engine 内訳プロファイル（Issue #465）
      $ cargo bench --bench hybrid_wire_profile_bench -p fandhe-db-wire-server
  bench-knn-profile
      KNN 経路の段別内訳プロファイル（Issue #362）
      $ cargo bench --bench knn_profile_bench -p fandhe-db-engine
  bench-knn-visible-ratio
      可視比率別の hnsw_subset と plain scan の損益分岐点計測（Issue #487）
      $ scripts/bench_knn_visible_ratio_sweep.sh
  bench-knn-f16-resident
      f16 常駐と f32 常駐の前後比較（Issue #516）
      $ scripts/bench_knn_f16_resident_ab.sh
  bench-knn-i8-resident
      I8（SQ8）常駐と f32 常駐の前後比較（Issue #523）
      $ AB_CANDIDATE_ENGINE=hnsw_i8 scripts/bench_knn_f16_resident_ab.sh
  bench-knn-precision-resident
      i8／f16／f32 経路の前後比較（Issue #526）
      $ AB_CANDIDATE_ENGINES="hnsw_f16 hnsw_i8" scripts/bench_knn_f16_resident_ab.sh
  bench-knn-wire-profile
      vector_knn の wire／SQL 表層／距離カーネル内訳（Issue #463）
      $ cargo bench --bench knn_wire_profile_bench -p fandhe-db-wire-server
  bench-dot-kernel
      dot カーネルのマイクロベンチ（Issue #365）
      $ cargo bench --bench dot_kernel_bench -p fandhe-db-engine
  detect-features
      macOS 上の aarch64 feature 検出結果表を出力する（Issue #468）
      $ cargo run -p fandhe-db-engine --release --example detect_features
  gpu-adapter-info
      wgpu adapter の features／limits 表を出力する（Issue #535）
      $ cargo run -p fandhe-db-engine --release --example gpu_adapter_info
  bench-scan-stage-profile
      redb 全行走査の段別プロファイル（Issue #464）
      $ cargo bench --bench scan_stage_profile_bench -p fandhe-db-engine
  bench-chip
      チップ別手動計測（Issue #469）
      $ scripts/bench_chip.sh
  bench-chip-ab
      チップ別前後比較（Issue #530。BEFORE_DIR/AFTER_DIR 必須）
      $ scripts/bench_chip_ab.sh
  bench-scalar-index-crossdb-ab
      ScalarIndex の crossdb 前後比較（Issue #633。BEFORE/AFTER_COMMIT ほか必須）
      $ scripts/bench_scalar_index_crossdb_ab.sh
  bench-filtered-distance-ab
      候補 id マスク経路の前後比較（Issue #655。BEFORE/AFTER_DIR・COMMIT 必須）
      $ scripts/bench_filtered_distance_ab.sh
  bench-crossdb-self-hnsw-ab
      crossdb self の exact/hnsw 前後比較（Issue #658。CROSSDB_DIR/PYTHON 必須）
      $ scripts/bench_crossdb_self_hnsw_ab.sh
  bench-ingest-profile
      ingest 経路の段別内訳プロファイル（Issue #396）
      $ cargo bench --bench ingest_profile_bench -p fandhe-db-engine
  bench-ingest-durability-ab
      ingest_single_stmt の durability 別実測（Issue #851）
      $ scripts/bench_ingest_durability_ab.sh
  bench-ingest-wire-profile
      単文 INSERT の wire 往復内訳（Issue #484）
      $ cargo bench --bench ingest_wire_profile_bench -p fandhe-db-wire-server
  bench-hnsw-build
      HNSW 構築計算量の簡易ベンチ（TASK-132）
      $ cargo bench --bench hnsw_build_bench -p fandhe-db-engine
  bench-hnsw-parallel-build
      HNSW 並列構築のスレッド数別実測（Issue #406）
      $ cargo bench --bench hnsw_parallel_build_bench -p fandhe-db-engine
  bench-gpu-scaling
      GPU バッチ検索のスケーリング実測（要 GPU）
      $ cargo bench --bench gpu_scaling_bench -p fandhe-db-engine
  bench-crossdb
      self と対照 DB の機能別比較（CROSSDB_DIR/PYTHON 必須）
      $ scripts/bench_crossdb.sh
  bench-crossdb-ab
      crossdb run_all の輪番前後比較（Issue #848。CROSSDB_DIR/PYTHON 必須）
      $ scripts/bench_crossdb_ab.sh
  bench-hnsw-compare
      自作 HNSW と usearch の比較（要 C++17）
      $ cargo bench --bench hnsw_compare_bench -p fandhe-db-engine --features contrast-bench
  bench-hnsw-search
      HNSW 探索の 1 規模点計測（Issue #491）
      $ cargo bench --bench hnsw_search_bench -p fandhe-db-engine
  bench-hnsw-search-visited
      visited 集合切替閾値の前後比較（Issue #498）
      $ scripts/bench_hnsw_search_visited_ab.sh
  hnsw-search-recall
      HNSW 探索の Recall@10 実測（TASK-132）
      $ cargo test --release -p fandhe-db-engine --test hnsw_search -- --ignored --nocapture
  hnsw-i8-recall
      I8 常駐の Recall@10 を ef 掃引で実測（Issue #523）
      $ cargo test --release -p fandhe-db-engine --test hnsw_i8_recall -- --ignored --nocapture
  hnsw-acorn-recall
      ACORN-1 の可視比率別 Recall 回帰 層 B（Issue #502）
      $ cargo test --release -p fandhe-db-engine --test hnsw_acorn_recall -- --ignored --nocapture
  hnsw-acorn-twohop-runs
      ACORN TwoHop の Recall を RUNS 回連続測定（Issue #679）
      $ HNSW_ACORN_RECALL_RUNS="${RUNS:-5}" cargo test --release -p fandhe-db-engine --test hnsw_acorn_recall -- --ignored --nocapture --exact layer_b_25k_dim128_cluster_mask_hop_mode_report
  hnsw-crossdb-selectivity
      選択率 33% の到達分類・Recall 比較（Issue #659。CROSSDB_DIR 必須）
      $ scripts/hnsw_crossdb_selectivity.sh
  sparse-cache-recall-large
      疎索引キャッシュの大規模段等価性テスト（Issue #358）
      $ cargo test -p fandhe-db-engine --test sparse_cache_recall -- --ignored
  rerank-cross-encoder-eval
      実 ONNX クロスエンコーダの fixture 実測（Issue #333）
      $ cargo test --release -p fandhe-db-engine --features cross-encoder --test rerank_cross_encoder_recall -- --ignored --nocapture
  recall-regression
      ハイブリッド検索 Recall 閾値ゲート 層 B（TASK-104）
      $ cargo test --release -p fandhe-db-engine --test hybrid_recall -- --ignored --nocapture
  rerank-regression
      リランキング効果測定 Recall 閾値ゲート 層 B（TASK-108）
      $ cargo test --release -p fandhe-db-engine --test rerank_recall -- --ignored --nocapture
  query-planning-regression
      クエリ展開受け入れ基準の閾値ゲート 層 B（TASK-112・113）
      $ cargo test --release -p fandhe-db-engine --test query_planning_recall -- --ignored --nocapture
  precision-regression
      precision モード評価基準の閾値ゲート（TASK-163）
      $ cargo test --release -p fandhe-db-engine --test precision_eval -- --ignored --nocapture --exact precision_eval_threshold_gate
  precision-report
      precision 判断材料レポートとパラメータ感度スイープ（TASK-163）
      $ scripts/precision_report.sh
  bench-wire-concurrency
      同時接続数別スループット実測（Issue #482。WIRE_CONCURRENCY_N 必須）
      $ cargo test --release -p fandhe-db-wire-server --test wire_concurrency_throughput -- --ignored --nocapture
  wire-tenant-latency
      他テナント id と未存在 id の INSERT レイテンシ同一性検証（Issue #738）
      $ scripts/wire_tenant_latency.sh
  test-eval
      scripts/eval のユニットテストを実行する
      $ scripts/test_eval.sh
  docker-build
      開発コンテナイメージをビルドする
      $ docker compose build
  docker-shell
      開発コンテナのシェルに入る
      $ docker compose run --rm dev
  docker-ci
      コンテナ内で make ci を実行する
      $ docker compose run --rm dev make ci
EOF
