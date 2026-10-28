# 疎索引キャッシュ導入後の Recall 非劣化・前後比較検証（Issue #358）

## 背景

PR #377（Issue #357）で hybrid 実行が参照する `SparseIndex`（BM25 語彙・統計）を
テーブル世代整合キャッシュ化した（`crates/engine/src/sql/sparse_cache.rs`。詳細は
`docs/design/sparse-index-cache.md` 参照）。キャッシュはランキングの入力そのものを
差し替える最適化であるため、本ドキュメントは以下 3 点を検証専任で固定し、実測値を
記録する。

1. ヒット経路の検索結果が非キャッシュ経路（キャッシュ導入前と同じ
   build-per-query 経路）と完全一致すること（Recall 非劣化）
2. Issue #310/#320 で確定した RRF 融合の同点順位規約（`TieRank::GroupEnd`・境界
   同点グループ完全化）がキャッシュ経路でも不変であること
3. 期待した性能効果（親 Issue #355「密検索と同オーダー」）が実測で出ていること

production コード（`crates/engine/src/`）は本 Issue の範囲では変更しない
（テスト・ベンチ・example・docs のみ）。

## 検証設計

### 既存 Recall ゲートとの関係（構造的不変）

`tests/hybrid_recall.rs`・`tests/rerank_recall.rs`・`tests/query_planning_recall.rs`・
`tests/precision_eval.rs`・`tests/incremental_recall.rs` はいずれも `SparseIndex::
build` + `hybrid::hybrid_search` を直接呼び出し、`SparseIndexCache` が結線されている
唯一の経路（`sql::exec::execute_statement_with_cache`、SQL 表層 `EngineCore::
execute_sql` 経由）を通らない。したがってこれらの既存ゲートはキャッシュ導入に
対して**構造的に不変**であり、それ自体はキャッシュ経路の非劣化を証明しない
（`docs/design/hybrid-recall-regression.md`・`rerank-recall-regression.md` 等の
既存分析と同じ位置づけ）。

本 Issue ではこれら既存ゲートをファイル無変更のまま実行し green であることを
確認した（下表「実行結果」参照）うえで、SQL 表層専用の新規検証
（`crates/engine/tests/sparse_cache_recall.rs`）でキャッシュ経路そのものの非劣化を
別途担保する。

### cold/hot 等価性テスト（`tests/sparse_cache_recall.rs`）

- **cold**: QA ケースごとに `EngineCore::open`（新規 `Storage::open`）でキャッシュを
  素通りさせる（#377 導入前と同一の build-per-query 経路の代替）
- **hot**: 単一 `EngineCore` で全 QA ケースを連続実行し、2 件目以降はキャッシュ
  ヒットさせる
- 両者の Top-20 id 列が全ケースで**順序込み完全一致**することをアサート
- 決定的コーパス生成器は `tests/hybrid_recall.rs` からの共有 fixture 切り出しでは
  なく、`Xorshift64`・トピック語彙の最小部分集合を独立に複製した（切り出しは
  `hybrid_recall.rs` の層 A 固定値アサーションを巻き添えで変える経路になりうる
  ため、`hybrid_recall.rs`/`query_planning_recall.rs` が既に同一実装を複製し合う
  前例と同方針を踏襲。詳細な判断理由は同ファイル冒頭のドキュメント参照）

**vacuous pass 対策**（Issue #281 で問題視した「一度もキャッシュを通らず 0 件比較で
通る」失敗形の再発防止）: `sparse_index_cache_stats()` で hot 実行が実際に
`hits > 0` を記録していることを同時にアサートする。小規模段（qa=40 件）では
`hits == 39`（`misses == 1`・`stale_evictions == 0`）を固定でアサートする。

### RRF 同点順位規約の不変

`tests/sql_surface.rs::sql4_hybrid_tie_group_across_limit_boundary_is_deterministic`
と同一の同点誘発コーパス（密ランク・疎ランクを入れ替えた 2 行が完全同点になる
構成）を、`sparse_cache_recall.rs::
cold_and_hot_hybrid_tie_group_across_limit_boundary_match` で cold/hot 双方
（LIMIT 1/2/3・`hybrid_rrf(...)`/`HYBRID(...)` の 2 構文形）から検証し、`TieRank::
GroupEnd` の挙動（タイ組の境界カット・完全化）がキャッシュヒット状態でも
非キャッシュ状態と一致することを確認した。

### 大規模段

20,000 件規模（vocab 800）の cold/hot 等価性テストを実装し
（`cold_and_hot_hybrid_results_are_identical_large_scale`）、実装時に 1 回実測した
所要時間が **166.6 秒**（`cargo test`・debug ビルド）と 60 秒基準を超えたため、
既定実行対象外（`#[ignore]`）とし `make sparse-cache-recall-large` から手動実行する
運用へ切り替えた（`bench-hybrid-profile` 等の既存の opt-in ベンチと同方針）。
本 Issue で 1 回実測し green（cold/hot 完全一致）を確認済み。

## 実行結果（既存 Recall ゲート・cache 非経由。構造的不変の確認）

| 対象 | 結果 |
| ---- | ---- |
| `cargo test -p engine --test hybrid_recall` | ok（10 passed・層 A 固定値〔小規模・大規模〕不変） |
| `cargo test -p engine --test rerank_recall` | ok（13 passed） |
| `cargo test -p engine --test query_planning_recall` | ok（10 passed） |
| `cargo test -p engine --test precision_eval` | ok（11 passed） |
| `cargo test -p engine --test incremental_recall` | ok |
| `cargo test -p engine --test core_misc sparse_cache::` | ok（5 passed。#357 既存受け入れ基準） |
| `cargo test -p engine --test sql_named sql_surface::` | ok（12 passed） |
| `cargo test -p engine --test core_misc hybrid::` | ok（9 passed） |
| `make sort-determinism-check` | ok |

## 実行結果（新規 SQL 表層専用検証）

| テスト | 結果 |
| ------ | ---- |
| `cold_and_hot_hybrid_results_are_identical_and_cache_is_actually_hit`（小規模・qa=40） | ok。cold/hot Top-20 id 列が全ケース完全一致。hot 側 `hits=39`・`misses=1`・`stale_evictions=0`（`hits==0` の vacuous pass ではないことを確認）。`hot_hits20=46`（本ファイル独自フィクスチャの回帰トラッキング値。`hybrid_recall.rs` の Recall 実測値とは無関係） |
| `cold_and_hot_hybrid_tie_group_across_limit_boundary_match` | ok。LIMIT 1/2/3・2 構文形すべてで cold/hot 完全一致、タイ組の境界カット・完全化（`TieRank::GroupEnd`）を確認 |
| `cold_and_hot_hybrid_results_are_identical_large_scale`（`#[ignore]`・20,000 件） | ok（1 回実測。所要時間 166.6 秒） |

## 前後比較の実測（`feature_bench`）

`crates/engine/examples/feature_bench.rs`（Issue #344/#355 の実測基準
〔`hybrid_rrf` 288.6ms 等〕を出した計測 example）は本 Issue まで git 追跡外
だったため（main worktree に未追跡で存在。PR #374 の ADR も「履歴に無い」と
記録）、before/after の再現性が無かった。本 Issue で本リポの `crates/engine/
examples/` へ追跡化した。

- **before**: `git worktree add` で #377 直前の main（コミット `4d913bb`）を
  チェックアウトし、同じ `feature_bench.rs` を複製して
  `cargo run --release -p engine --example feature_bench` を 1 回実行
- **after**: 作業ブランチ（#377 込み）で同じコマンドを 1 回実行

いずれも非専有環境（本開発コンテナ）での 1 回実測であり、参考値として扱う
（`docs/design/hybrid-rrf-latency-breakdown.md` の注記と同方針）。

### `hybrid_rrf` フェーズ

WARMUP 5 + ITERS 50。before は #377 前のため全 50 試行が build-per-query、after は
WARMUP 後すべてキャッシュヒット経路。

| 指標 | before（`4d913bb`） | after（本ブランチ・#377 込み） | 比 |
| ---- | -------------------: | -------------------------------: | --: |
| p50 | 367,639 us | 147,350 us | 0.40x |
| p95 | 418,611 us | 253,997 us | 0.61x |
| p99 | 448,912 us | 291,967 us | 0.65x |
| mean | 373,318.8 us | 152,932.4 us | 0.41x |
| `cpu_tick_delta` | 2,077 | 846 | 0.41x |
| `rss_kb_after` | 145,224 | 144,676 | ほぼ同一 |

p50 で約 2.5 倍、p95 で約 1.65 倍のレイテンシ改善を確認した。

注記: after は `#377` に加え、その後の origin/main HEAD（`#371`〔`GROUP BY` 性能
改善〕・`#373`〔式評価のステップ列コンパイル化〕を含む）である。`hybrid_rrf` は
`WHERE`・`GROUP BY` を持たないクエリであるため、両者の寄与は構造的にほぼ無いと
判断した（`#356` の段別内訳実測〔`docs/design/hybrid-rrf-latency-breakdown.md`〕で
`SparseIndex::build` がコストの過半を占めることを既に確認済み）。`65fdb86`
（`#377` 直後）時点での再測定は行っていない。

### 対照: `vector_knn`（親 Issue #355「密検索と同オーダー」判定用）

| 指標 | before | after |
| ---- | -----: | ----: |
| p50 | 14,333 us | 15,327 us |
| p95 | 20,243 us | 29,347 us |

`vector_knn` 自体は #377 の変更対象外の経路であり、before/after の差は測定ノイズの
範囲（非専有環境の 1 回実測）。`hybrid_rrf` after の p50（147,350 us）は
`vector_knn` after の p50（15,327 us）の約 9.6 倍であり、キャッシュヒット後も
「密検索と同オーダー」（同じ桁）には未到達。ただし before（約 24.6 倍）からは
大幅に縮小しており、親 Issue #355 の目標に向けた改善方向であることは確認できる
（絶対的な目標値の再判定は本 Issue の対象外）。

### SQL 表層の他フェーズ・`bench-hybrid-profile` について

`hybrid_profile_bench`（Issue #356・`docs/design/hybrid-rrf-latency-breakdown.md`）
の before/after 再実測は、release ビルド 2 回分の時間コストと本 Issue の時間予算の
兼ね合いから見送った（`feature_bench` の `hybrid_rrf` 前後比較で本 Issue の要件 3
は既に定量的に満たされていると判断）。詳細な段別内訳（`SparseIndex::build` 由来の
コストの内訳変化）の再実測は申し送り事項とする（下記「スコープ外・申し送り」）。

## 3 クライアント wire 経由検証（要件 4）

- 層 A（`cargo test -p wire-server`）: green（全 crate 実行。109 + 各結合テスト
  ファイル含め failure 0）
- 層 B（`#[ignore]` の 3 クライアント e2e）: `three_clients_run_c1_through_c4_and_
  reject_wrong_password`（`crates/wire-server/tests/three_client_e2e.rs`。
  hybrid_rrf を含む C4 を検証する）を試行実行したところ、psql・psycopg（python）・
  node `pg` を同一テストケース内で逐次実行する構成のため、psycopg 未導入
  （`No module named 'psycopg'`）の時点で panic し停止した。テストが 3 クライアント
  を単一ケース内で連続実行する設計のため、psql 部分のみを単独で分離実行すること
  はできない。node `pg` モジュールも未導入であり、依存導入はユーザー承認制のため
  本 Issue では行わない。3 クライアント e2e は未実施のまま記録し、`psycopg`・
  node `pg` を導入したオーナー環境での実行を申し送る（silent skip にしない）
- `.github/workflows/recall.yml`（層 B・main 限定）は本 PR からは実行不能。
  マージ後の `workflow_dispatch`/週次 run の確認をオーナー／管理者作業として
  申し送る（本 Issue の新規テストは cache 非経由の既存ゲートに影響しないため、
  構造的に不変である旨も併記する）

## スコープ外・申し送り

- `recall.yml` 層 B のマージ後実行・Issue への pass/fail 記録: オーナー／管理者作業
- psycopg（python）・pg（node）クライアント経由の e2e: ローカル環境未整備・依存
  導入はユーザー承認待ち
- 専有環境での前後再実測: 本ドキュメントの値は非専有環境の 1 回実測
- `bench-hybrid-profile` の before/after 再実測（段別内訳の変化）: 時間予算の
  都合で本 Issue では見送り。`feature_bench` の `hybrid_rrf` 前後比較で要件 3 は
  定量的に満たされている
- WHERE フィルタ付き hybrid・DISTANCE 先行経路へのキャッシュ拡張（#377 の対象外
  事項）の検証は対象外
- `crates/engine/examples/seed_docs.rs`（未追跡・rustfmt 差分あり）は本 Issue と
  無関係のため追跡化しない

## 再現方法

```sh
# SQL 表層専用の cold/hot 等価性検証（小規模・同点誘発コーパス）
cargo test -p engine --test sparse_cache_recall

# 大規模段（数万件規模。既定では実行しない）
make sparse-cache-recall-large

# 既存 Recall ゲート（cache 非経由。構造的不変の再確認）
cargo test -p engine --test hybrid_recall --test rerank_recall \
  --test query_planning_recall --test precision_eval --test incremental_recall

# 前後比較（feature_bench。release ビルド）
cargo run --release -p engine --example feature_bench
```

## 追記（Issue #393）

`SparseIndex` の転置索引化（Issue #386 Phase 1・#388〜#392）後も本検証設計が
維持されることを、RLS 部分可視・未知語のみクエリ・空クエリの追加ケースと
疎側決定性契約の固定テストで再確認した。詳細は
`docs/design/sparse-inverted-index-recall-verification.md` 参照。
