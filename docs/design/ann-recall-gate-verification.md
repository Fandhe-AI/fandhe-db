# ANN opt-in 時の Recall ゲート同一閾値検証と TASK-121 系増分回帰の拡張（Issue #412）

## 背景

ADR `docs/design/ann-index-adoption.md`（Issue #403 で Accepted）の B 案
（条件付き opt-in 採用・自作 HNSW）の受け入れ基準のうち、以下 2 点が本 Issue
（#412）の担当:

- SEARCH 系 Recall ゲート（`crates/engine/tests/hybrid_recall.rs`・
  `rerank_recall.rs`・`query_planning_recall.rs` の層 B）を ANN 有効経路でも
  同一閾値で通過すること
- TASK-121 系増分回帰（`tests/incremental_recall.rs`）の ANN 対応

前提: `SearchEngineKind::Hnsw` opt-in（#407）・世代整合キャッシュと未索引分
brute-force 併用（#408。`docs/design/hnsw-generation-cache.md`）・RLS 事前
フィルタ統合（#409。`docs/design/hnsw-rls-cardinality-switch.md`）・hybrid 密側
境界再取得（#410。`docs/design/hnsw-hybrid-iterative-scan.md`）・`EXPLAIN` 露出
（#411。`docs/design/explain-search-engine-exposure.md`）はいずれも実装済み。

## 測定経路の設計判断

3 つの Recall ハーネスは従来 `engine::hybrid::hybrid_search` を in-memory 配列に
対して直接呼んでおり、ANN 経路（`sql::hnsw_cache`／`sql::hnsw_hybrid::
HnswDenseProvider`）を構造的に通らない。`hnsw::provider::HnswSearchProvider::
search` は常に brute-force へ委譲する契約であり、`SearchProvider` の差し替え
だけでは ANN は発火しない。ANN の実 seam はいずれも `pub(crate)` のため、結合
テストから ANN 経路へ到達できる唯一の production API は **SQL 表層**
（`EngineCore::from_storage_with_engine(storage, hnsw_kind(..))` ＋
`execute_sql` の `ORDER BY HYBRID(...)`）である。

そのため、3 ハーネス共通の fixture
`crates/engine/tests/fixtures/recall_engine.rs` を新設した:

- `RecallEngine`: `RECALL_ENGINE` 環境変数（未設定・空文字列・`brute_force` は
  既定〔既存 in-memory 経路をそのまま通す〕、`hnsw` は ANN opt-in。それ以外は
  fail-closed で panic）
- `SqlHybridFixture`: `docs(embedding VECTOR(dim), body TEXT)` へ行を投入し、
  `hybrid_top(query_vec, query_text, k)` で `ORDER BY HYBRID(...)` の
  `(id, score)` 列を返す。`score` は `sql/exec.rs` の hybrid 分岐が書き込む
  `ResultRow::score`（RRF 融合スコア）で、in-memory 版の `hybrid_search` が
  返すスコアと同じ意味
- `AnnStats`: `EngineCore::hnsw_index_cache_stats()` のフィールド値の複製
  （`HnswIndexCacheStats` 自体は `pub(crate)` モジュール配下のため型名を
  結合テストから綴れない。値は機微情報を含まないため出力可）
- `assert_ann_non_vacuous(expect_indexed)`: `expect_indexed` が真なら
  `builds >= 1 && build_failures == 0 && hybrid_dense_searches > 0` を、偽なら
  `builds == 0` を固定する（構築失敗→負のキャッシュ→黙って brute-force で
  「ANN pass」を誤報告する経路、および `MIN_INDEXED_ROWS` 未満の段を誤って
  ANN 通過と数える経路の両方を防ぐ）

既定経路（`RecallEngine::BruteForce`）は各ハーネスの既存 in-memory 測定コード
を一切変更せず素通しする——層 A・層 B とも既存の実測値・固定値アサーションは
無変更のまま green（`cargo test -p engine --test hybrid_recall` 等で確認済み）。

## 測定妥当性ガード（`tests/recall_engine_fixture.rs`）

fixture 自体の層 A テストとして以下を固定した:

1. `RecallEngine::from_env` の受理／拒否契約（純関数 `parse`。厳密一致のみ
   受理・trim 許容）
2. **「SQL 表層 + 既定エンジン」の hybrid クエリ結果（id 順序）が「in-memory
   `hybrid_search`」の結果と完全一致すること**（`sql_default_engine_hybrid_
   top_matches_in_memory_hybrid_search`）。ANN 有効時の Recall 差分が SQL 表層
   自体の違いではなく検索エンジンの違いにのみ起因することの前提を担保する
3. 非 vacuous ガードの正負両方（`MIN_INDEXED_ROWS`〔1,024〕以上で
   `builds >= 1`、未満で `builds == 0`）

## 3 ゲートへの結線

`hybrid_recall.rs`・`rerank_recall.rs`・`query_planning_recall.rs` の層 B
（`#[ignore]`）に `RecallEngine::from_env()` による分岐を追加した
（brute-force 側は既存コードを一切変更しない分岐構造）。hnsw 側は
`SqlHybridFixture` 経由で候補プール／Top-k を取得し、既存の集計ロジック
（`RecallResult`／`RerankRecallResult`／`CategoryRecallResult`）へそのまま
渡す。ゲート行には `engine=<brute_force|hnsw>` トークンを追加し、hnsw 実行時
は `AnnStats` の非機密カウンタ（`builds`／`build_failures`／`rebuilds`／
`hybrid_dense_searches`／`hybrid_queries`／`ef_cap_fallbacks`）を出力する。

各コーパス規模と `MIN_INDEXED_ROWS`（1,024）との関係:

| ゲート | 小規模段 | 大規模段 |
| ---- | ---- | ---- |
| hybrid | 400 docs（**`MIN_INDEXED_ROWS` 未満。構造的に brute-force のまま**） | 20,000 docs |
| rerank | （小規模段ゲートなし） | 20,000 docs |
| query-planning | 4,000 docs | 40,000 docs |

hybrid 小規模段以外はすべて `MIN_INDEXED_ROWS` を上回り、`RecallEngine::Hnsw`
指定時に実際に HNSW 索引を構築する。hybrid 小規模段は `RecallEngine::Hnsw`
を指定しても `assert_ann_non_vacuous(false)` により `builds == 0` を固定し、
ANN 通過とは数えない（測定経路自体は SQL 表層を経由するため無意味ではないが、
索引構築の検証にはならない）。

`recall.yml` は `recall-regression` job に `strategy.matrix.recall_engine:
[brute_force, hnsw]` を追加し、3 gate step の `env:` へ `RECALL_ENGINE` として
`matrix.recall_engine` を渡す（`run:` へ直接展開せず `env:` 経由。式インジェク
ション回避の定石）。当初は `workflow_dispatch.inputs.recall_engine`（`type:
choice`。既定 `brute_force`）の単一選択式で、`schedule` トリガでは `inputs`
が存在せず式が空文字列に解決 → `RecallEngine::from_env` の契約で既定
`brute_force` のまま評価される実装だった。これは週次 `schedule` 実行で HNSW
経路が一度も測定されないことを意味し、ADR の受け入れ条件「Recall ゲートを
ANN 有効経路でも同一閾値で通過」を継続的には保証できていなかった
（codex-review P1 指摘・Issue #412 PR #438）。`matrix` 化により
`workflow_dispatch`・`schedule` いずれのトリガでも `brute_force`/`hnsw` の
2 系列が独立 job として毎回両方ゲートされる（`fail-fast: false` で片方の
fail がもう片方の評価を止めない）。`RECALL_ENGINE` 自体は非機密の opt-in
フラグ（`BENCH_CORE6`等と同じ扱い）であり secrets 化しない
（`docs/design/ci-gate-variables.md` 参照）。

## 実測結果（ローカル `--release`。閾値は private spec から環境変数へ注入し値は本 doc に転記しない。ここに記載する Recall 実測値・統計カウンタはオーナー判断〔2026-08-29〕により公開可）

`RECALL_VERBOSE=1` opt-in で `brute_force` と `hnsw` を交互に 1 回ずつ実行し、
測定値を比較した。

### hybrid（`hybrid_recall.rs`）

| 段 | 指標 | brute_force | hnsw | 差分 |
| ---- | ---- | ---- | ---- | ---- |
| 小規模（400 docs） | recall@20 | 0.9010 | 0.9010 | 0（`builds=0`。構造的に brute-force のまま） |
| 大規模（20,000 docs） | recall@20 | 0.9145 | 0.9145 | 0 |
| 大規模（20,000 docs） | recall@100 | 0.9165 | 0.9165 | 0 |

大規模段の hnsw 実測での統計カウンタ（1 run）: `builds=1 build_failures=0
rebuilds=0 hybrid_dense_searches=420 hybrid_queries=100 ef_cap_fallbacks=80`
（`ef_cap_fallbacks` は `hybrid.rs::hybrid_search_boosted` の密側再取得ループが
`fetch_k` を `MAX_EF` 超まで倍増したラウンドで縮退した回数。production 契約
どおり brute-force 縮退により空集合誤返却を防いでおり、Recall には影響
していない）。

### rerank（`rerank_recall.rs`。大規模段のみ）

| 指標 | brute_force | hnsw | 差分 |
| ---- | ---- | ---- | ---- |
| after_recall@20 | 0.9488 | 0.9488 | 0 |
| non_degraded（after_hits20 >= baseline_hits20） | true | true | — |
| improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0 |

baseline_hits20=387・after_hits20=389・pool_ceiling_hits20=396 はいずれも
brute_force・hnsw で完全一致。hnsw 実測の統計（1 run）: `builds=1
build_failures=0 rebuilds=0 hybrid_dense_searches=492 hybrid_queries=100
ef_cap_fallbacks=106`。

### query-planning（`query_planning_recall.rs`）

| 段 | 指標 | brute_force | hnsw | 差分 |
| ---- | ---- | ---- | ---- | ---- |
| 小規模（4,000 docs） | intent_improvement | 0.9245 | 0.9245 | 0 |
| 小規模（4,000 docs） | direct_after_recall20 | 0.9321 | 0.9321 | 0 |
| 小規模（4,000 docs） | intent_improvement_degraded（NoisyLlmClient） | 0.3547 | 0.3547 | 0 |
| 大規模（40,000 docs） | direct_after_recall20 | 0.8852 | 0.8852 | 0 |

小規模段 hnsw 実測の統計: direct `builds=1 hybrid_dense_searches=662
hybrid_queries=160 ef_cap_fallbacks=0`、intent `builds=1
hybrid_dense_searches=731 hybrid_queries=160 ef_cap_fallbacks=0`、
intent_degraded `builds=1 hybrid_dense_searches=758 hybrid_queries=160
ef_cap_fallbacks=0`（いずれも `build_failures=0 rebuilds=0`）。大規模段:
`builds=1 build_failures=0 rebuilds=0 hybrid_dense_searches=456
hybrid_queries=100 ef_cap_fallbacks=120`。

### 判断

**全 6 測定点（hybrid 大規模 2 指標・rerank 大規模 1 指標＋非劣化＋
improvement_ratio・query-planning 小規模 3 指標・大規模 1 指標）で
brute_force と hnsw の実測値が完全一致した。** 合成コーパス・本リポの
決定的フィクスチャ範囲では、ANN opt-in 経路は既定エンジンと同一の Recall
挙動を示す。閾値は既定エンジンで公開済みの値（例: hybrid 0.9010/0.9145/
0.9165）と同一のまま運用でき、ANN 有効化のために閾値を緩める必要はない。

未達・原因分析の記録は不要（全指標が非劣化どころか完全一致のため）。
production コード（`crates/engine/src/`）は本 Issue の範囲では無変更。

## TASK-121 系（`crates/engine/tests/incremental_index_hnsw.rs`）

`tests/incremental_recall.rs`（TASK-121 の性能回帰）とは別ファイルとして、
ANN opt-in 時の増分反映の状態遷移を検証する結合テストを追加した。
`documents(embedding VECTOR(64), path, body)` へ `HashingEmbedder`
（`lines_per_chunk=2`・本文ちょうど 2 行＝ 1 ファイル = 1 チャンク）で
1,100 ファイル（`[MIN_INDEXED_ROWS（1,024）, MIN_ROWS_PER_THREAD*2（2,048）)`
に収め、`HnswIndex::build_parallel` が逐次構築に留まり決定的であることを
保証する）を投入し、以下 3 状態を検証する:

1. **初回構築**: `execute_insert_sql_batch`（`BatchLimits::default()` の
   ファイル数上限で分割）で 1,100 ファイルを投入後、代表サンプル（約 30 件）
   の自己検索到達率（本文中の marker をクエリにしてその `path` 自身が
   `HYBRID(...)` の top-10 に現れる率）が ANN opt-in・既定エンジンの双方で
   0.9 以上であることを固定。`stats.builds == 1`。
2. **overlay**（1〜3 ファイルを同一パス置換。stale+delta 比 ≈ 0.27%）:
   置換後の新チャンク（新 marker）が `WHERE path = ..` の直接照会で確認
   でき、旧 marker が本文から消えていること（`body_for_path` によるランキング
   非依存の決定的検証）。`stats.builds == 1 && stats.rebuilds == 0`
   （再構築は起きない）かつ `delta_searches + plain_scans + fallbacks > 0`
   （overlay／縮退経路のいずれかが実際に発火したことの非 vacuous 検証。
   具体的にどのカウンタが増えるかは可視カーディナリティ・マスク連結性等の
   実装内部の判定順序に依存するため 3 カウンタの和で判定する）。未置換の
   既存チャンクの自己検索到達率は overlay 前後で非劣化。
3. **再構築**（約 12.5%＝137 ファイルをさらに置換。stale+delta 比が 1/10 を
   超える）: `stats.builds >= 2 && stats.rebuilds >= 1`。再構築後の新チャンク
   の自己検索到達率が 0.9 以上。overlay 済みチャンク（状態 2）は再構築後も
   引き続き到達可能。

あわせて `ann_replace_does_not_touch_other_tenants_same_path_rows` で、
ANN opt-in core でも tenant-a の同一パス置換が tenant-b の同一パス行を
変更しないこと（`tests/incremental_index.rs::
resend_does_not_touch_other_tenants_same_path_rows` と同方針）を固定した。

いずれも `cargo test -p engine --test core_misc incremental_index_hnsw::` で green
（`--release` で約 0.5 秒）。production コード（`crates/engine/src/`）は
無変更・テスト専任。

## スコープ外・申し送り

- `precision` モード hybrid の ANN 化・`SearchTimeFilter` 経路・Rust API
  `hybrid` 相当 API の結線（#410 申し送りどおり継続）
- `full_scan_ratio`／`MAX_EF`／`ef_search` 既定値の再調整と前後比較（#413）
- `tests/rls_generalized.rs`／`tests/plan_rls_boost.rs` への HNSW variant
  追加（#409 申し送り）
- `recall.yml` の `hnsw` matrix job（`strategy.matrix.recall_engine: [brute_force, hnsw]`。PR #438 で `workflow_dispatch.inputs.recall_engine` から変更）の実 `workflow_dispatch`／`schedule` 疎通確認（マージ後の管理者作業）

## Issue #515 追記: f16 常駐（`hnsw_f16`）での同一閾値検証

### 背景

Issue #514 で HNSW 索引ノードの f16 常駐表現（`hnsw::ResidentPrecision::F16`。
`ValidatedHnswParams::with_resident_precision` の opt-in・既定 F32。
`docs/design/hnsw-f16-resident.md`）が入った。候補生成段のスコアが
f16→f32 昇格 dot（`isa::F16Kernel`）になるため探索順序が変わり得るが、
最終スコアは `kernel::dot` による f32 アリーナ再計算という #408 契約は不変
（本 doc 上部の測定妥当性ガードと同じ理由づけ）。本節は次の 2 点を固定する:

- 3 つの Recall ゲート層 B を f16 常駐でも **同一閾値** のまま通過できること
  （brute_force 対照の実測。#412 と同じ方式）
- `tests/hnsw_cache.rs` 系の「既定エンジン対照 Recall@10 ≥ 0.9・可視外
  非混入」を f16 常駐でも固定すること

### 測定経路（`RecallEngine::HnswF16`）

`crates/engine/tests/fixtures/recall_engine.rs::RecallEngine` へ
`HnswF16`（環境変数トークン `hnsw_f16`）を追加した。`SqlHybridFixture::new`
の `HnswF16` 分岐は、`ValidatedHnswParams::new(HnswParams::default())`
（`search_engine::hnsw_kind` が守るのと同じ「untrusted な `HnswParams` は
ここでのみ検証する」経路）に `with_resident_precision(ResidentPrecision::
F16)` を適用するだけで、fixture 内であっても production の「未検証入力の
唯一の入口」契約を迂回しない。

`AnnStats` へ `f16_residency_fallbacks`（`HnswIndexCacheStats` の同名
フィールドの複製。F16 要求時に範囲外成分〔`|x| > 65504.0`〕で F32 常駐へ
自動縮退した回数。D6・Issue #514）を追加し、`assert_ann_non_vacuous(true)`
は engine が `Hnsw`/`HnswF16` のいずれかに応じて `search_engine_kind()` の
Display に `resident=f32`/`resident=f16` が含まれること、F16 の場合は
さらに `f16_residency_fallbacks == 0`（このコーパスでは縮退が起きていない
こと）を追加で固定する——`f16_residency_fallbacks == 0` 単独では F32 を
要求した場合も 0 になり vacuous なため、Display 側の確認と組み合わせる。

3 ハーネスの `measure_*_via_hnsw` 系関数へ `engine: RecallEngine` 引数を
追加し、`match engine { .. RecallEngine::Hnsw | RecallEngine::HnswF16 => .. }`
の共有 arm で両エンジンを同じ経路から測定する（brute_force 側・在来の
`Hnsw` 単独の挙動は無変更）。`print_ann_stats` の出力へ
`f16_residency_fallbacks=` と `f16_kernel=`（`engine::isa::current_f16()`
の Debug 表現。非機密——スコアを含まない）を追記した。

### 実測結果（ローカル `--release`。本開発環境: x86_64・F16C あり・AVX-512 なし。閾値は private spec から環境変数へ注入し値は本 doc に転記しない。ここに記載する Recall 実測値・統計カウンタはオーナー判断〔2026-08-29〕により公開可）

`RECALL_VERBOSE=1` opt-in で `brute_force`／`hnsw`／`hnsw_f16` を各ゲート
1 回ずつ実行し比較した。

#### hybrid（`hybrid_recall.rs`）

| 段 | 指標 | brute_force | hnsw | hnsw_f16 | 差分 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| 小規模（400 docs） | recall@20 | 0.9010 | 0.9010 | 0.9010 | 0（3 系列とも `builds=0`。構造的に brute-force のまま） |
| 大規模（20,000 docs） | recall@20 | 0.9145 | 0.9145 | 0.9145 | 0 |
| 大規模（20,000 docs） | recall@100 | 0.9165 | 0.9165 | 0.9165 | 0 |

大規模段 `hnsw_f16` の統計（1 run）: `builds=1 build_failures=0 rebuilds=0
hybrid_dense_searches=420 hybrid_queries=100 ef_cap_fallbacks=80
f16_residency_fallbacks=0 f16_kernel=F16c(..)`（`hnsw`（F32）と統計値も
完全一致）。

#### rerank（`rerank_recall.rs`。大規模段のみ）

| 指標 | brute_force | hnsw | hnsw_f16 | 差分 |
| ---- | ---- | ---- | ---- | ---- |
| after_recall@20 | 0.9488 | 0.9488 | 0.9488 | 0 |
| non_degraded | true | true | true | — |
| improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0.2222 | 0 |

`hnsw_f16` の統計: `builds=1 build_failures=0 rebuilds=0
hybrid_dense_searches=492 hybrid_queries=100 ef_cap_fallbacks=106
f16_residency_fallbacks=0`。

#### query-planning（`query_planning_recall.rs`）

| 段 | 指標 | brute_force | hnsw | hnsw_f16 | 差分 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| 小規模（4,000 docs） | intent_improvement | 0.9245 | 0.9245 | 0.9245 | 0 |
| 小規模（4,000 docs） | direct_after_recall20 | 0.9321 | 0.9321 | 0.9321 | 0 |
| 小規模（4,000 docs） | intent_improvement_degraded | 0.3547 | 0.3547 | 0.3547 | 0 |
| 大規模（40,000 docs） | direct_after_recall20 | 0.8852 | 0.8852 | 0.8852 | 0 |

`hnsw_f16` 小規模段の統計: direct `builds=1 hybrid_dense_searches=662
hybrid_queries=160 ef_cap_fallbacks=0 f16_residency_fallbacks=0`、intent
`builds=1 hybrid_dense_searches=731 hybrid_queries=160 ef_cap_fallbacks=0
f16_residency_fallbacks=0`、intent_degraded `builds=1
hybrid_dense_searches=758 hybrid_queries=160 ef_cap_fallbacks=0
f16_residency_fallbacks=0`。いずれも `hnsw`（F32）と統計値が完全一致。

### 判断

**全 8 測定点（hybrid 小規模 1・大規模 2、rerank 大規模 1＋非劣化＋
improvement_ratio、query-planning 小規模 3・大規模 1）で
brute_force／hnsw／hnsw_f16 の実測 Recall 値が完全一致し、`hnsw_f16` の
`f16_residency_fallbacks` はいずれも 0（このコーパス範囲では D6 縮退は
発生しない）だった。** F32/F16 いずれも同一閾値のまま運用でき、f16 常駐
opt-in のために閾値を緩める必要はない（S8 決定規則: 全指標で
`hnsw_f16 >= brute_force` の公開済み基準値を満たした）。

未達・原因分析の記録は不要。production コード（`crates/engine/src/`）は
本 Issue の範囲では無変更。

### 可視外非混入テスト（`tests/hnsw_cache.rs`）

R4（テナント境界）・hybrid 密側再取得ループ・Rust API 検索・`Subset` 形状
（SCALAR 事前フィルタ付き DISTANCE）・`full_scan_ratio` ANN 側の 5 テストを
`run_*(precision: ResidentPrecision)` 共有本体へ切り出し、既存名を F32
ラッパー、`f16_*` を F16 ラッパーとして追加した（既存名のテストはビット
同一の挙動のまま green）。F16 ラッパーは追加で `search_engine_kind()` の
Display に `resident=f16` を含むこと・`f16_residency_fallbacks == 0` を
固定する。

さらに D6（範囲外成分による自動縮退）が SQL 表層経由でも fail-closed に
働くことを新規テスト
`f16_out_of_range_component_falls_back_to_f32_residency_without_leaking_or_losing_recall`
で固定した: tenant-a のコーパスに 1 成分 `70000.0`（> 65504.0）を持つ行を
1 件だけ混ぜ、`f16_residency_fallbacks == 1`（D6 縮退が実際に 1 回発生）・
`resident=f16`（静的な opt-in 設定自体は取り消されない。索引ノードの実効
表現のみが F32 へ切り替わる）・tenant-b（Private）の可視外非混入・既定
エンジン対照 Recall@10 ≥ 0.9 を固定する。

`tests/hnsw_hybrid_refetch.rs` の同点誘発コーパス停止性・決定性テストも
`run_*(precision)` 化し、`f16_tie_inducing_corpus_hybrid_search_
terminates_and_is_deterministic` として F16 常駐でも
（候補生成の f16→f32 昇格 dot で探索順序が変わり得ても）停止性・ビット
同一の決定性契約が不変であることを固定した。

いずれも `cargo test -p engine --test core_misc hnsw_cache:: hnsw_hybrid_refetch::`
で green。production コード（`crates/engine/src/`）は無変更・テスト専任。

### `recall.yml` の 3 系列化

`strategy.matrix.recall_engine` を `[brute_force, hnsw]` から
`[brute_force, hnsw, hnsw_f16]` へ拡張した（#412 と同じ「trigger を問わず
毎回全系列をゲートする」原則。`workflow_dispatch.inputs` は使わない）。

### スコープ外・申し送り（本節限定）

- 規模別（25k／100k／500k × dim 128／768）の常駐メモリ・レイテンシ前後
  比較・既定常駐精度を F16 へ反転するかの判断: Issue #516
- `recall.yml` 3 系列 matrix の実 `workflow_dispatch`／`schedule` 疎通・
  閾値ゲート最終判定: マージ後の管理者作業（#412 と同じ）
- Apple Silicon（NEON fp16）実機での層 B 実測: 手元環境が x86_64（F16C）
  のため未実施

## Issue #523 追記: I8（SQ8）常駐（`hnsw_i8`）での同一閾値検証

### 背景

Issue #521 で HNSW 索引ノードの対称 SQ8（i8）常駐表現
（`hnsw::ResidentPrecision::I8`。`docs/design/hnsw-sq8-resident.md`）、
Issue #522 でクエリ側二重量子化＋整数 i8×i8 dot カーネル
（`isa::I8Kernel`）が入った。候補生成が f16 よりさらに粗い 8-bit 量子化を
経由するため探索順序への影響が大きくなり得るが、最終スコアは常に
`kernel::dot` の f32 再計算という #408 契約は不変（Issue #515 と同じ
理由づけ）。本節は Issue #515 と同型に次の 2 点を固定する:

- 3 つの Recall ゲート層 B を I8 常駐でも **同一閾値** のまま通過できること
- `tests/hnsw_cache.rs` 系の「既定エンジン対照 Recall@10・可視外非混入」を
  I8 常駐でも固定すること（下限は精度別に異なり、F32／F16 は 0.9、I8 は
  `min_recall_for` により 0.8。詳細は後述の可視外非混入テスト節参照）

加えて、`tests/hnsw_i8_recall.rs`（brute-force 対照 `ef` 掃引）の実測で
i8 の候補生成が f16 より明確に大きい探索順序ノイズを持つことが判明した
ため、その事実と「本節の Recall ゲートには実害が及ばない」という結論を
あわせて記録する（詳細は後述）。

### 測定経路（`RecallEngine::HnswI8`）

`RecallEngine` へ `HnswI8`（環境変数トークン `hnsw_i8`）を追加し、
`SqlHybridFixture::new` の `HnswI8` 分岐は F16 分岐と同型に
`ValidatedHnswParams::new(HnswParams::default())` へ
`with_resident_precision(ResidentPrecision::I8)` を適用するだけで構築する
（untrusted な `HnswParams` の唯一の検証入口はここでも迂回しない）。

`AnnStats` へ `i8_residency_fallbacks`（次元ごとスケールの f32
アンダーフローで F32 常駐へ自動縮退した回数。D6 と同型・Issue #521）を
追加し、`assert_ann_non_vacuous(true)` は `HnswI8` のとき
`search_engine_kind()` の Display に `resident=i8` が含まれること・
`i8_residency_fallbacks == 0` を追加で固定する。3 ハーネスの
`match engine { .. RecallEngine::Hnsw | RecallEngine::HnswF16 |
RecallEngine::HnswI8 => .. }` へ共有 arm を拡張し、`print_ann_stats` の
出力へ `i8_residency_fallbacks=` と `i8_kernel=`（`engine::isa::
current_i8()` の Debug 表現）を追記した。

### 実測結果（ローカル `--release`。本開発環境: x86_64・AVX2＋FMA あり・VNNI なし〔`I8Kernel::Avx2Widen` 経路〕。閾値は private spec から環境変数へ注入し値は本 doc に転記しない。ここに記載する Recall 実測値・統計カウンタはオーナー判断〔2026-08-29〕により公開可）

`RECALL_VERBOSE=1` opt-in で `brute_force`／`hnsw`／`hnsw_f16`／`hnsw_i8`
を各ゲート 1 回ずつ実行し比較した。

#### hybrid（`hybrid_recall.rs`）

| 段 | 指標 | brute_force | hnsw | hnsw_f16 | hnsw_i8 | 差分 |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| 小規模（400 docs） | recall@20 | 0.9010 | 0.9010 | 0.9010 | 0.9010 | 0（4 系列とも `builds=0`。構造的に brute-force のまま） |
| 大規模（20,000 docs） | recall@20 | 0.9145 | 0.9145 | 0.9145 | 0.9145 | 0 |
| 大規模（20,000 docs） | recall@100 | 0.9165 | 0.9165 | 0.9165 | 0.9165 | 0 |

大規模段 `hnsw_i8` の統計（1 run）: `builds=1 build_failures=0 rebuilds=0
hybrid_dense_searches=420 hybrid_queries=100 ef_cap_fallbacks=80
i8_residency_fallbacks=0 i8_kernel=Avx2Widen(..)`（`hnsw`（F32）・
`hnsw_f16` と統計値も完全一致）。

#### rerank（`rerank_recall.rs`。大規模段のみ）

| 指標 | brute_force | hnsw | hnsw_f16 | hnsw_i8 | 差分 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| after_recall@20 | 0.9488 | 0.9488 | 0.9488 | 0.9488 | 0 |
| non_degraded | true | true | true | true | — |
| improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0.2222 | 0.2222 | 0 |

`hnsw_i8` の統計: `builds=1 build_failures=0 rebuilds=0
hybrid_dense_searches=492 hybrid_queries=100 ef_cap_fallbacks=106
i8_residency_fallbacks=0 i8_kernel=Avx2Widen(..)`（他 3 系列と統計値も
完全一致）。

#### query-planning（`query_planning_recall.rs`）

| 段 | 指標 | brute_force | hnsw | hnsw_f16 | hnsw_i8 | 差分 |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| 大規模（direct のみ） | direct_after_recall20 | 0.8852 | 0.8852 | 0.8852 | 0.8852 | 0 |
| 小規模（direct） | direct_after_recall20 | 0.9321 | 0.9321 | 0.9321 | 0.9321 | 0 |
| 小規模（intent） | intent_improvement | 0.9245 | 0.9245 | 0.9245 | 0.9245 | 0 |
| 小規模（intent_degraded） | intent_improvement_degraded | 0.3547 | 0.3547 | 0.3547 | 0.3547 | 0 |

`hnsw_i8` の統計（各段）: direct `builds=1 hybrid_dense_searches=662
hybrid_queries=160 i8_residency_fallbacks=0`、intent `builds=1
hybrid_dense_searches=731 hybrid_queries=160 i8_residency_fallbacks=0`、
intent_degraded `builds=1 hybrid_dense_searches=758 hybrid_queries=160
i8_residency_fallbacks=0`。いずれも他 3 系列と統計値が完全一致。

### 判断

**全 8 測定点（hybrid 小規模 1・大規模 2、rerank 大規模 1＋非劣化＋
improvement_ratio、query-planning 大規模 1・小規模 3）で
brute_force／hnsw／hnsw_f16／hnsw_i8 の実測 Recall 値が完全一致し、
`hnsw_i8` の `i8_residency_fallbacks` はいずれも 0（このコーパス範囲では
D6 縮退は発生しない）だった。** I8 常駐 opt-in は同一閾値のまま運用でき、
Recall ゲートの閾値を緩める必要はない（S8 決定規則: 全指標で
`hnsw_i8 >= brute_force` の公開済み基準値を満たした）。

未達・原因分析の記録は不要。production コード（`crates/engine/src/`）は
本 Issue の範囲では無変更。

**oversampling（R5）との関係**: `tests/hnsw_i8_recall.rs`（brute-force
対照・クラスタ構造ありコーパス・N=10,000・dim=128）の実測では、探索幅
（`ef`）だけを `64/128/256` と広げても返却件数 `k=10` 固定の Recall@10
は F32 対比の差分（約 0.095）から一切改善しない一方、`ef=64` のまま
候補数（`oversample_k`）を 10→20 へ増やし**元の f32 ベクトルで再採点**
すると Recall@10 は 1.0000（F32 と同水準）まで完全に回復することを
確認した（詳細は `docs/design/hnsw-sq8-resident.md`「Issue #523 追記」節
参照）。つまりこの条件では「探索幅（`ef`）拡大では補えない」が
「候補数を広げ f32 で再採点する oversampling」は有効であり、両者を
区別しない場合の「oversampling 一般が効かない」という結論は誤り
だった（codex-review 指摘・PR #621 で是正）。本節の Recall ゲート測定
（実コーパス規模・hybrid 密側再取得ループ経由）で専用 knob なしに
brute_force と完全一致したのは、hybrid 密側の再取得ループ
（`dense_fetch_k` 倍増。#410）が事実上この oversampling＋再採点と同型の
効果（候補を広く取ってから `kernel::dot` の f32 再計算でスコアを引き
直す）を担っているためと考えられ、上記の直接測定はその仮説と整合する
（本 Issue の範囲では構造的論拠までは検証していない。`hnsw_i8_recall.rs`
の直接 `HnswIndex::search` 呼び出しはこの再取得ループを経由しない）。

### 可視外非混入テスト（`tests/hnsw_cache.rs`）

R4（テナント境界）・hybrid 密側再取得ループ・Rust API 検索・`Subset` 形状
（SCALAR 事前フィルタ付き DISTANCE）・`full_scan_ratio` ANN 側の 5 テストに
`i8_*` ラッパー（`run_*(ResidentPrecision::I8)`）を追加した（f16 版と同型。
既存テストの挙動は無変更のまま green）。I8 ラッパーは追加で
`search_engine_kind()` の Display に `resident=i8` を含むこと・
`i8_residency_fallbacks == 0` を固定する。

さらに D6（次元ごとスケールの f32 アンダーフローによる自動縮退）が SQL
表層経由でも fail-closed に働くことを新規テスト
`i8_scale_underflow_dimension_falls_back_to_f32_residency_without_leaking_or_losing_recall`
で固定した: tenant-a の全行の次元 0 を `1e-44`（f32 subnormal。
`sq8.rs::fit_dim_params_rejects_scale_that_underflows_to_zero` と同じ値）
へ揃え、`i8_residency_fallbacks == 1`（D6 縮退が実際に 1 回発生）・
`resident=i8`（静的な opt-in 設定自体は取り消されない）・tenant-b
（Private）の可視外非混入・既定エンジン対照 Recall@10 ≥ 0.8（I8 の
`min_recall_for` 下限。`crates/engine/tests/hnsw_cache.rs`）を固定する
（f16 版は 1 成分だけを範囲外にするのに対し、I8 は次元ごとスケールが
全行にわたる列全体の統計のため、次元 1 本を丸ごとアンダーフローさせる
必要がある点が f16 版と異なる）。

`tests/hnsw_hybrid_refetch.rs` の同点誘発コーパス停止性・決定性テストも
`i8_tie_inducing_corpus_hybrid_search_terminates_and_is_deterministic`
として I8 常駐でも停止性・ビット同一の決定性契約が不変であることを固定
した。

いずれも `cargo test -p engine --test core_misc hnsw_cache:: hnsw_hybrid_refetch::`
で green。production コード（`crates/engine/src/`）は無変更・テスト専任。

### `recall.yml` の 4 系列化

`strategy.matrix.recall_engine` を `[brute_force, hnsw, hnsw_f16]` から
`[brute_force, hnsw, hnsw_f16, hnsw_i8]` へ拡張した（#412・#515 と同じ
「trigger を問わず毎回全系列をゲートする」原則）。

### スコープ外・申し送り（本節限定）

- 規模別の常駐メモリ・レイテンシ前後比較・`ef` 掃引の詳細表: Issue #523
  実施節（`docs/design/hnsw-sq8-resident.md`「Issue #523 追記」節）
- `recall.yml` 4 系列 matrix の実 `workflow_dispatch`／`schedule` 疎通・
  閾値ゲート最終判定: マージ後の管理者作業（#412・#515 と同じ）
- VNNI（AVX-512 VNNI／AVX-VNNI）実機・Apple Silicon（NEON dotprod）
  での層 B 実測: 手元環境が AVX2＋FMA（`Avx2Widen` 経路）のため未実施。
  `I8_DOT_MAX_DIM` 以下では全 ISA でビット同一という契約（#522）により
  Recall 値自体は ISA 非依存
- per-query スケール準備失敗（`prepare_query` 失敗）時の復号 dot 縮退を
  数えるカウンタ: `HnswIndexCacheStats` に存在しないため未計測（構造的に
  本節の fixture では到達不能。Issue #522 の設計）

## Issue #450 追記: repair 逐次段削減（#447〜#449）後の同一閾値検証

### 背景

Issue #447〜#449 で `HnswIndex::build_with_threads` の
`repair_reachability`（凍結後・単一スレッドの後始末）を大幅に削減した
（詳細は `docs/design/hnsw-parallel-build.md`「Issue #450 追記」節）。
このうち #449 の保証は `repair_reachability_inner` 単体について、同一
入力グラフに対する `threads` の値によらないビット同一性のみである
（上記 doc「Issue #449 追記」節「決定性の検証」参照）。一方 #448 は
`publish_links` から `ensure_reverse_link` を呼び並列構築で失われうる
逆辺を追加する変更であり、構築グラフ全体が変更前後でビット同一である
ことまでは保証しない。本節では #447〜#449 を含む変更全体を対象に、
Recall ゲート層 B・可視外非混入テストが `#412`／`#515` 時点の実測値と
変わらず通ることを実測で再確認する（グラフのビット同一性ではなく
Recall 値の一致を根拠とする）。

### 実測結果（ローカル `--release`。本開発環境: x86_64・AVX2+FMA あり・AVX-512 なし。閾値は private spec から環境変数へ注入するが、本節の実測は測定を発火させるためだけの permissive なプレースホルダ（recall 系 `(0.0,1.0]` は `0.0001`・improvement 系 `[0.0,1.0]` は `0.0`）を注入し、pass/fail 自体は判定材料としていない。実測値はオーナー判断〔2026-08-29〕により公開可）

`RECALL_VERBOSE=1` opt-in で `brute_force`／`hnsw` を各ゲート 1 回ずつ
実行し比較した（`RECALL_ENGINE` 未実装の変更は無いため `hnsw_f16`／
`hnsw_i8` の再検証は #515／#523 の既存実測から不変のまま対象外とした）。

#### hybrid（`hybrid_recall.rs`）

| 段 | 指標 | brute_force | hnsw | #412／#515 実測値 | 差分 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| 小規模（400 docs） | recall@20 | 0.9010 | 0.9010 | 0.9010 | 0 |
| 大規模（20,000 docs） | recall@20 | 0.9145 | 0.9145 | 0.9145 | 0 |
| 大規模（20,000 docs） | recall@100 | 0.9165 | 0.9165 | 0.9165 | 0 |

大規模段 `hnsw` の統計（1 run）: `builds=1 build_failures=0 rebuilds=0
hybrid_dense_searches=420 hybrid_queries=100 ef_cap_fallbacks=80
f16_residency_fallbacks=0 hybrid_resumed_rounds=240`（`hybrid_resumed_
rounds` は Issue #505/#619 で追加された再開型探索の統計。`ef_cap_
fallbacks` と同水準で非 vacuous に発火しており停止性・非退行に問題なし）。

#### rerank（`rerank_recall.rs`。大規模段のみ）

| 指標 | brute_force | hnsw | #412／#515 実測値 | 差分 |
| ---- | ---- | ---- | ---- | ---- |
| after_recall@20 | 0.9488 | 0.9488 | 0.9488 | 0 |
| non_degraded | true | true | true | — |
| improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0.2222 | 0 |

#### query-planning（`query_planning_recall.rs`）

| 段 | 指標 | brute_force | hnsw | #412／#515 実測値 | 差分 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| 小規模（4,000 docs） | intent_improvement | 0.9245 | 0.9245 | 0.9245 | 0 |
| 小規模（4,000 docs） | direct_after_recall20 | 0.9321 | 0.9321 | 0.9321 | 0 |
| 小規模（4,000 docs） | intent_improvement_degraded | 0.3547 | 0.3547 | 0.3547 | 0 |
| 大規模（40,000 docs） | direct_after_recall20 | 0.8852 | 0.8852 | 0.8852 | 0 |

### 判断

**全 8 測定点で brute_force／hnsw の実測 Recall 値が完全一致し、かつ
`#412`／`#515` 時点の実測値（repair 逐次段削減前）とも完全一致した。**
このうち `#449` 単体（`repair_reachability_inner`）は同一入力グラフに対する
`threads` 不変性がビット同一という設計保証だが、#448 を含む変更全体
（#447〜#449）については構築グラフそのもののビット同一性を保証しない
（上記「背景」節参照）。本節はその変更全体を対象に、Recall 値が
repair 逐次段削減の前後で一致することを実測で確認したものであり、
グラフのビット同一性ではなく Recall ゲートへの影響が無いことを根拠と
する。全指標で `hnsw >= brute_force` かつ `repair 削減後 == repair
削減前` の S8 決定規則を満たすため、閾値を緩める必要はない。

未達・原因分析の記録は不要。production コード（`crates/engine/src/`）は
本 Issue の範囲では無変更（テスト・docs 専任）。

### 可視外非混入・既存 ANN テストの無変更 green 確認

`cargo test --release -p engine --test core_misc incremental_index_hnsw::
hnsw_cache:: hnsw_hybrid_refetch:: hnsw::` と `cargo test --release -p engine --test
hnsw_parallel_profile_accept --test hnsw_search`（層 A。`hnsw_search` の `#[ignore]` 3 本
除く）・`cargo test --release -p engine --lib hnsw::`・`make
hnsw-search-recall`（層 B・`#[ignore]`）をいずれも実行し、全て green
であることを確認した（`hnsw_search_recall: ef=64/256 Recall@10=1.0000`
は `docs/design/hnsw-search.md` の既存記録と完全一致）。`tests/hnsw_
cache.rs`・`tests/hnsw_hybrid_refetch.rs` の可視外非混入・RLS 統合テスト
は本 Issue の変更対象（`hnsw.rs::repair_reachability_inner` 等）に触れる
経路を含むが無変更のまま green であり、#447〜#449 が RLS 境界・可視外
非混入契約に影響していないことを再確認した。

### スコープ外・申し送り（本節限定）

- `hnsw_f16`／`hnsw_i8` での再検証（Recall 経路自体は #447〜#449 の
  変更対象外のため実施していない。必要になった場合は次回 Recall 関連
  Issue で合わせて確認する）

## Issue #677 追記

Issue #676（`sql::hnsw_cache::prepare_subset_from_slots`。DISTANCE 段の
`Subset` 形状 plain scan 縮退時委譲）は 3 つの Recall ハーネス（`hybrid_
recall.rs`・`rerank_recall.rs`・`query_planning_recall.rs`）が使う `ORDER BY
HYBRID(...)` 経路（`sql::hnsw_hybrid::HnswDenseProvider`）を通らない
（`sql/exec.rs` の DISTANCE 段のみを変更対象とし、hybrid 密側は対象外のまま
——本 doc「Issue #676」相当節・`docs/design/hnsw-rls-cardinality-switch.md`
「Issue #676」節参照）。したがって #412・#515・#523 が記録した
brute_force／hnsw／hnsw_f16／hnsw_i8 の実測値（全 8〜12 測定点で完全一致）
は #676 適用前後で構造的に不変であり、Issue #506 と同じ理由（変更対象の
分岐自体を通らない経路であること）により本 Issue では実測での再確認を
行っていない。docs/spec submodule 未チェックアウトのため閾値注入自体も
できない環境だった（プレースホルダ閾値での実行にとどめる #450／#506 の
運用も、本経路が構造的に不変である以上は追加の情報を持たないと判断した）。

専有環境での再確認が必要と判断された場合は `make recall-regression
RECALL_ENGINE=hnsw RECALL_VERBOSE=1`（プレースホルダ閾値）等をオーナー・
後続 Issue へ申し送る。
