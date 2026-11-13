# hybrid_rrf クエリ内訳プロファイル切り分け

- ステータス: Accepted（本コミットで計測ベンチ・ADR を追加。実測は本開発環境
  ——非専有・並行エージェントあり——での 1 回実測。専有環境での再実測は運用者判断）
- 対応: Issue #356（`test(engine): hybrid_rrf 288ms の内訳プロファイル切り分け`）・
  Issue #387（`search_within` の段別プロファイル・疎側再取得発火回数の追加）。
  親: Issue #355（`SparseIndex` のクエリ毎再構築の排除）
- 前提: `docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3（判定内容・数値基準は
  spec 側が SSOT。本 ADR は spec 由来の pass/fail 閾値を持たない情報提供専用の
  実測記録）

## 背景

Issue #355 は「hybrid 検索（`crates/engine/src/sql/exec.rs` の `Ranking::Hybrid`
分岐）はクエリ毎に可視行の本文を String 複製で収集し、`SparseIndex::build`
（`crates/engine/src/sparse.rs`）をゼロから実行している」という調査結果と、
「feature_bench（25,000 行）実測で hybrid_rrf p50 288.6ms、密 KNN 単体 11.7ms の
約 25 倍」という実測値を記録している。しかし `SparseIndex::build` が実際に
支配的なのか、本文 String 収集や境界同点グループ再取得ループ（Issue #310/#320）
の寄与がどの程度なのかは定量的に切り分けられていなかった。本 Issue はその内訳を
実測し、後続の Issue #357（`SparseIndex` のテーブル世代整合キャッシュ設計）が
「どの段をキャッシュ対象にすべきか」を判断できる分解能を提供する。

## 実測値の比較可能性についての重要な注意

Issue #355 が言及する `crates/engine/examples/feature_bench.rs` は、本ブランチの
分岐元コミット（`f749af6`）時点のリポジトリ履歴に**一度も存在しない**
（`git log --all --diff-filter=A -- 'crates/engine/examples/*'` で確認済み。全
ブランチ・全リモート追跡ブランチを対象にしても該当コミットはなく、GitHub の
コード検索でも `feature_bench` を含むファイルはヒットしなかった）。おそらく
Issue #355 の実測は一時的なローカルスクリプトで行われ、コミットされなかった。

そのため本 Issue のベンチ（`crates/engine/benches/hybrid_profile_bench.rs`・
`crates/engine/benches/harness/hybrid_profile.rs`）は feature_bench の複製では
なく、Issue #356 本文の記述（行数・次元の規模感）にのみ合わせて新規に組み立てた
コーパスを使う。単純化点は以下のとおり:

- 単一テナント・全行 `Visibility::Public`（Issue #355 の tenant-a/tenant-b 2 テナント
  構成は再現しない。本 Issue が切り分けたいのは RLS 境界ではなく hybrid_rrf 内部の
  段別コストのため）
- 疎チャネル本文は 40 語の合成語彙を 1 文書あたり 30 語回転させて生成（feature_bench
  の語彙生成方式そのものは不明なため、非自明な tokenize/BM25 統計コストを持つ
  合成文書として設計）

**したがって本 ADR の実測 ms は Issue #355 の 288ms と直接比較可能ではない**
（コーパス内容・実行環境のいずれも異なる）。本 Issue が実際に必要とするのは
絶対値の再現ではなく「どの段が支配的か」という相対的な内訳の分解能であり、
その目的には十分に応えられる。なお偶然にも本実測の `sql_hybrid` 中央値
（286.1ms、下記「実測結果」参照）は Issue #355 の記録値（288.6ms）に近い値と
なったが、これはコーパス規模（25,000 行）を Issue #356 の記述に合わせたことに
よる規模感の一致であり、意図した再現ではないことに注意する。

## スコープ（計画レビューによる絞り込み）

境界同点グループ再取得ループの寄与は Issue #324（`crates/engine/benches/harness/
hybrid_latency.rs`・`docs/design/hybrid-refetch-latency.md`）で既に測定済みで、
CORE-7 は `hybrid_search` を通らない測定経路のため構造的に不変であることが
確認されている。本 Issue では再取得ループの統計収集を再実装せず、以下 4 点の
分解に絞る:

1. `sql_hybrid` 対 `sql_dense_knn`（SQL 表層エンドツーエンドの対照）
2. 本文 String 収集（`collect_body_strings`）
3. `SparseIndex::build` 全体（`sparse_build_total`）
4. build 内部の tokenize / term_freq 構築 / doc_freq マージの累積 3 段

## 測定設計

`crates/engine/benches/hybrid_profile_bench.rs`（実測本体）・`crates/engine/
benches/harness/hybrid_profile.rs`（時間非依存ヘルパ。決定的コーパス生成・build
内部 3 段の複製・SQL 文字列組み立て）・`crates/engine/tests/hybrid_profile_accept.rs`
（`harness::hybrid_profile` の回帰テスト。`make ci` 対象）で構成する。

- コーパス: 25,000 文書・128 次元・単一テナント・全行 Public（上記「実測値の
  比較可能性」参照）
- 計測プロトコル: `harness::protocol::run`（warmup 20 回・計測 30 回）。SQL 段
  （`sql_hybrid`/`sql_dense_knn`）はクエリ 5 種を round-robin
- `sql_dense_knn` は「密チャネルのみの SQL 対照」であり、`sql/exec.rs` の
  `on_visible_row`（370-509 行）を読むと、`is_hybrid == false` の間は本文
  String 収集（`sparse_docs.push`）自体が実行されない（`scan_scalar_columns`
  による構造検証は hybrid・dense 双方で共通に走るが、`Text` 列の `String` 確保は
  hybrid 側のみ）。したがって `sql_hybrid − sql_dense_knn` の差は「本文 String
  収集＋`SparseIndex::build`＋RRF 融合＋（該当すれば）再取得ループ」の合算に
  ほぼ等しく、`collect_body_strings`・`sparse_build_total` を差し引いた残差を
  「融合＋再取得＋帰属できない差分」として扱う（厳密な対称比較ではない近似）
- build 内部 3 段（`tokenize_only`・`tokenize_term_freq`・`tokenize_term_doc_freq`）
  は `sparse.rs::with_params` のロジックを計測用に手動転記した複製実装で、
  上限検証（`MAX_DOC_BYTES` 等）は行わない。複製の妥当性は「同一コーパスに対し
  実際の `SparseIndex::build` が成功するか」という構造的整合性チェック
  （`build_actually_succeeds`）のみで担保する——`SparseIndex` の内部フィールド
  （`doc_freq`・`docs`）が private のため、複製実装の語彙数と実際の内部状態を
  数値比較する手段が公開 API には存在しない（下記「複製近似の限界」参照）

## 実測結果

本開発環境（Linux/x86_64、論理コア数 12、Avx2Fma。**非専有**——他エージェントが
並行実行中——のため下記数値は参考値として扱う）での 1 回実測
（`make bench-hybrid-profile`。中央値・p95、単位 ms）:

| stage | median | p95 | 備考 |
| ----- | -----: | --: | ---- |
| `sql_hybrid` | 286.1 | 303.0 | SQL 表層 hybrid_rrf エンドツーエンド |
| `sql_dense_knn` | 9.0 | 9.6 | SQL 表層・密 KNN のみ（対照） |
| `collect_body_strings` | 0.84 | 0.86 | 本文 25,000 件の `String` clone 収集 |
| `sparse_build_total` | 176.4 | 181.2 | `SparseIndex::build` 単体 |
| `tokenize_only` | 20.3 | 20.7 | tokenize 段（累積） |
| `tokenize_term_freq` | 85.2 | 86.2 | tokenize + term_freq 構築（累積） |
| `tokenize_term_doc_freq` | 150.9 | 153.7 | tokenize + term_freq + doc_freq マージ（累積） |

検算値（決定的コーパスに対する固定値）: `tokenize_only` の総トークン数
800,000（25,000 文書 × 30 語 + 各文書の `doc-{id}` トークン 1 個）、
`tokenize_term_doc_freq` のコーパス全体語彙数 25,041。

### 帰属分析

`sql_hybrid − sql_dense_knn` = 286.1 − 9.0 = **277.1ms**（hybrid 経路が dense 経路
に対して上乗せするコストの上限）。この差分に対する内訳:

| 内訳 | ms | 対 277.1ms 比率 |
| ---- | -: | --------------: |
| 本文 String 収集（`collect_body_strings`） | 0.8 | 0.3% |
| `SparseIndex::build`（`sparse_build_total`） | 176.4 | **63.7%** |
| 残差（融合・再取得ループ・帰属できない差分） | 99.9 | 36.0% |

`SparseIndex::build` が単独で差分の 6 割以上を占め、Issue #355 のコードリーディング
上の推定（build がクエリ毎再構築のコストの主要因）を定量的に裏付ける。残差
36% は融合（`hybrid::rrf_fuse`）・（本コーパスでは連続値ベクトルのため通常は
発生しない）再取得ループ・測定誤差の合算だが、単独の内訳としては
`SparseIndex::build` を下回る。

### build 内部 3 段の内訳（累積差分から算出）

| 段 | 累積 median | 段単独の限界寄与 |
| -- | ----------: | ---------------: |
| tokenize | 20.3ms | 20.3ms |
| term_freq 構築 | 85.2ms | 85.2 − 20.3 = 64.9ms |
| doc_freq マージ | 150.9ms | 150.9 − 85.2 = 65.7ms |
| （`DocEntry`/`id_index` 構築＋上限検証） | 176.4ms（`sparse_build_total`） | 176.4 − 150.9 = 25.5ms |

tokenize・term_freq 構築・doc_freq マージがほぼ均等に build 全体の 3 割前後
（tokenize 11.5%・term_freq 36.8%・doc_freq 37.2%・残り 14.5% が `DocEntry`/
`id_index` 構築と上限検証）を占め、単一のホットスポットではなく `BTreeMap<String,
_>` を用いた文字列キー処理全体（tokenize が生成する `String` の clone を term_freq・
doc_freq の双方が繰り返す構造）がコストの主要因であることが読み取れる。

### 複製近似の限界

`tokenize_term_doc_freq`（複製実装の doc_freq マージ後、`DocEntry`/`id_index`
構築前）は 150.9ms、`sparse_build_total`（実際の `SparseIndex::build`）は
176.4ms で、比率は 150.9 / 176.4 ≈ **0.855**。残り 14.5%（25.5ms）は
`DocEntry`/`id_index` の構築コスト・4 段の上限検証（`MAX_DOC_BYTES` 等）の
オーバーヘッドとして妥当な範囲に収まっており、複製実装が build の主要コスト
（tokenize・term_freq・doc_freq）を過小評価も過大評価もしていないことを示す
（計画時に設定した許容乖離の目安 15% 以内）。

## Issue #357（キャッシュ設計）への示唆

- `SparseIndex::build` がクエリ毎再構築コストの支配項（対 hybrid 上乗せ分の
  64%）であることが定量的に裏付けられた。Issue #357 が「テーブル世代整合の
  `SparseIndex` キャッシュ」を設計する際、キャッシュ対象は `SparseIndex::build`
  全体（本文 String 収集は build の入力であり、キャッシュヒット時は入力生成
  自体が不要になるため合わせて省略される）で妥当
- build 内部の tokenize / term_freq / doc_freq はいずれも同程度の寄与（各 3 割
  前後）であり、内部の特定段だけを個別最適化するより、build 全体をテーブル
  世代（`catalog.rs::bump_table_generation_in_txn` 等が進める世代カウンタ）に
  紐付けてキャッシュし、書き込みが無い限り再構築を省略する設計（本リポ既存の
  `PrefilterCache`/`DictionaryCache` と同じ世代整合・fail-closed 契約。Issue
  #280/#285 参照）が最も効果が大きいと考えられる
- 残差 36%（融合・再取得ループ等）はキャッシュでは削減できないため、
  `SparseIndex` キャッシュ導入後の hybrid_rrf レイテンシは dense KNN 対照
  （本実測で 9.0ms）まで下がるのではなく、本文収集＋build を除いた残差
  （本実測で概算 100ms 程度）に漸近すると見込まれる。この残差の縮小は
  Issue #357 のスコープ外（別途 Issue #320/#324 の枠組みで扱う）

## 再現方法

```
make bench-hybrid-profile
```

`.github/workflows/*` へは配線しない（手動実行専用）。`GITHUB_ACTIONS` 環境変数
が設定された実行環境下では起動直後に fail-closed で拒否する
（`harness::hybrid_profile::refuse_under_github_actions`）。判定ロジック自体
（時間非依存）は `crates/engine/tests/hybrid_profile_accept.rs` で `make ci`
側から回帰検証する。`engine::hybrid::sparse_refetch_observed`（ベンチ・診断専用
公開フック）は非既定 feature `bench-internals` の背後にあるため、これに依存する
関数（`harness::hybrid_profile::sparse_refetch_schedule`）とその import・依存
テスト 2 件（`sparse_refetch_schedule_*`）のみ同 feature の背後に置く。コーパス
生成・SQL 文組み立て・tokenize 複製・`refuse_under_github_actions` 等、大半の
時間非依存テストは feature 無指定の通常の `cargo test -p engine` でも実行され
（42 件）、`bench-internals` を含む `--all-features` では上記 2 件を加えた 44 件が
検証される（Issue #387 PR #416 codex-review P2 指摘対応・2 巡目。1 巡目の対応では
ファイル全体を `#![cfg(feature = "bench-internals")]` で覆っており、依存しない
既存テストまで既定 feature で 0 件になっていた）。

## 申し送り

- 本実測は非専有環境（並行エージェントあり）での 1 回実測であり、専有環境での
  再実測はオーナー／実装担当の判断で別途実施する
- Issue #357（疎索引キャッシュ）実装後の `feature_bench` `hybrid_rrf` フェーズの
  before/after 比較は Issue #358 で実施済み（`docs/design/
  sparse-index-cache-verification.md` 参照。`hybrid_profile_bench` 自体の
  before/after 再実測は同 Issue で時間予算の都合により見送り、申し送り事項）
- production コード（`crates/engine/src/`）は本 Issue で無変更（テスト・ベンチ
  専任）

## Issue #387: `search_within` 段別・再取得発火回数の基線

Issue #357（`SparseIndex` のテーブル世代整合キャッシュ）導入後、キャッシュ
ヒット時になお残るコストが `crates/engine/src/sparse.rs::search_within`
単体（可視 subset 構築／df 再計算パス／スコアリングパスの 3 区間）と、それを
繰り返し呼ぶ `hybrid.rs::hybrid_search_boosted` の疎側再取得ループ
（Issue #320）のどちらにどれだけ帰属するかは未計測だった。本節はその内訳と、疎側の
再取得発火回数・密側の発火回数を実測した基線を記録する（Issue #358 の
`feature_bench` `hybrid_rrf` hot p50 約 147ms・本 ADR 前節の残差 36%
〔約 100ms〕の主な内訳仮説を検証する位置づけ）。

### 測定設計

`crates/engine/src/sparse.rs::SparseIndex`（`docs`・`id_index`・`k1`・`b` は
private フィールド）・`crates/engine/src/hybrid.rs` の境界同点判定
（`resolve_boundary_tie_group`/`complete_boundary_tie_group_by`。いずれも
`pub(crate)`/private）はいずれもベンチ・統合テストから直接呼べないため、
`harness/hybrid_profile.rs` に以下を追加した:

- `ProfileSparseIndex`: `search_within` の 3 区間（`subset_only`／`subset_df`／
  `search_within_replica`）を個別に呼び分けられる複製索引。`replica_matches_real`
  が実 `SparseIndex::search_within` の出力（`doc_id` 列・スコア）と数値一致する
  ことをベンチ起動時に fail-closed で検証してから使う（Issue #356 の build
  複製と異なり、本複製は公開 API 経由で出力を直接比較できる）
- `boundary_tie_decision`: 境界同点判定の**判定結果のみ**（列の切り詰めは
  行わない）を複製し、`dense_refetch_schedule`（鏡像定数
  `MAX_POOL_DEPTH_MIRROR`/`MAX_FETCH_K_MIRROR` を使い、初期 `fetch_k` から
  倍増しつつ実 `provider.search` を呼ぶ）が密側の再取得スケジュールを予測する。
  この密側予測はベンチ起動時に `RefetchTrackingProvider`（既存 Issue #324
  ハーネス）が観測する実際の呼び出し回数と突き合わせて fail-closed 検証する。
  疎側の `sparse_refetch_schedule` は当初 `boundary_tie_decision` による予測
  だったが、codex-review P1 指摘（PR #416）対応で production の疎側再取得
  ループ実装（`hybrid.rs::sparse_refetch_loop`）をテスト・ベンチ向け公開フック
  `engine::hybrid::sparse_refetch_observed` 経由で直接呼ぶ方式へ変更した
  （「限界・申し送り」節参照）。以降の予測固有の限界の記述はこの変更前の
  設計時点のものであり、密側（`dense_refetch_schedule`）のみに適用される

複製固有の限界: `MAX_POOL_DEPTH_MIRROR`/`MAX_FETCH_K_MIRROR` は `hybrid.rs`
側の値をこのファイルへ手動転記したものであり、コード上の同期は強制されない
（ドリフトは `max_pool_depth_mirror_matches_rrf_config_bounds` アクセプトテストが
`RrfConfig::new` の受理境界との突き合わせで検知する）。境界同点判定の複製は
判定結果（`Resolved`/`Undetermined`）のみを再現し、`hybrid_search_boosted` が
行う「境界の同点グループ全体を対称に除外する」列操作（`exclude_undetermined_
boundary_group`）自体は複製しない（本測定が必要とするのはスケジュール
——何回・どの `fetch_k` で呼ばれるか——であり、最終的にどの候補が採用されるかの
複製は不要なため）。

計測条件は Issue #356 と同一コーパス（25,000 行・dim 128・単一テナント・全行
`Visibility::Public`）・同一クエリ集合（5 件）・同一プロトコル
（`MeasurementConfig::new(20, 30, SEED)`）。`pool_depth` は SQL 表層の既定
（`sql/exec.rs::DEFAULT_HYBRID_POOL_DEPTH` の鏡像 `SQL_DEFAULT_HYBRID_POOL_DEPTH`
= 200）を使い、`SparseIndex`・`ProfileSparseIndex` は事前に 1 回だけ構築して
以降の全段で使い回す（`sql/sparse_cache.rs::SparseIndexCache` のキャッシュ
ヒット経路と同型の前提）。

### 実測結果（本開発環境・非専有・並行エージェントあり・1 回実測）

忠実性検証（`search_within` 3 区間複製 ↔ 実 API・密側スケジュール予測
〔`dense_refetch_schedule`〕↔ `RefetchTrackingProvider` 実測呼び出し回数）は
いずれも通過（`hybrid_profile: fidelity checks passed ...`）。

| 段 | median | p95 | 備考 |
| -- | -----: | --: | ---- |
| `hybrid_search_cached_index`（キャッシュヒット相当・`hybrid_search` 直接呼び出し） | 115.7ms | 136.1ms | `provider_calls_max=1`（密側は 1 回で確定） |
| `search_within_fetch_k=400`〜`25000`（各 fetch_k 単体） | 17.2〜18.8ms | 18.3〜21.7ms | `fetch_k` によらずほぼ一定 |
| `search_within_subset_only`（区間 1・k 非依存） | 0.78ms | 0.81ms | `fetch_k` を受け取らず可視集合サイズのみに依存するため 1 回のみ測定 |
| `search_within_subset_df`（区間 1+2・k 非依存） | 9.66ms | 10.76ms | `fetch_k` を受け取らず可視集合サイズのみに依存するため 1 回のみ測定。区間 2 単独の寄与 ≈ 8.9ms |
| `search_within_replica_full`（区間 1+2+3） | 15.25〜16.76ms | 17.09〜17.89ms | 区間 3 単独の寄与 ≈ 5.6〜6.8ms |

疎側再取得ループの実測（5 クエリ。codex-review P1 指摘対応〔PR #416〕後の
`engine::hybrid::sparse_refetch_observed` 経由の再実行——production の
`sparse_refetch_loop` を直接呼ぶため、以下の `fetch_ks`／`calls`／
`reached_cap` は予測ではなく実際に発火した呼び出し列そのもの）:

```
sparse_refetch query=0 calls=7 fetch_ks=400,800,1600,3200,6400,12800,25000 final_hits=17501 reached_cap=true
sparse_refetch query=1 calls=7 fetch_ks=400,800,1600,3200,6400,12800,25000 final_hits=13125 reached_cap=true
sparse_refetch query=2 calls=7 fetch_ks=400,800,1600,3200,6400,12800,25000 final_hits=18125 reached_cap=true
sparse_refetch query=3 calls=6 fetch_ks=400,800,1600,3200,6400,12800       final_hits=12500 reached_cap=false
sparse_refetch query=4 calls=6 fetch_ks=400,800,1600,3200,6400,12800       final_hits=12500 reached_cap=false
sparse_refetch_summary queries=5 calls_max=7 calls_total=33 reached_cap_count=3 max_fetch_k=25000
```

（発火回数・`fetch_ks` は修正前の複製予測による実行結果と一致した——
`sparse_refetch_loop` の抽出はロジックを変えない純粋なリファクタリングであり、
この一致はその不変性を裏付ける。）

密側は `provider_calls_max=1`（初回 `fetch_k=400` で境界確定）で再取得が
発生していない。疎側は 5 クエリ全件で 6〜7 回発火し、うち 3 クエリは可視集合
全体（25,000 件）まで倍増し続ける exhaustive 到達（本ベンチの合成コーパスは
文書ごとに語彙を等間隔で回転させるだけの構成のため BM25 スコアが大量に同点に
なりやすく、Issue #356 前提調査で想定した「最悪ケースに近い同点誘発」が
実際に生じている）。

### 帰属分析

- `search_within` 単体は `fetch_k` によらずほぼ一定（17〜19ms）であり、これは
  区間 1（subset 構築・0.8ms・全体の 4〜5%）・区間 2（df 再計算パス・8.9〜
  9.2ms・全体の約 50%）・区間 3（スコアリング・5.6〜6.8ms・全体の 35〜40%）の
  合算で説明できる。**df 再計算パス（区間 2）が単独最大の寄与**であり、
  「クエリ語ごとに可視 subset 全件を線形走査して df を数え直す」処理
  （`sparse.rs::search_within` 832〜842 行の複製）が `fetch_k` に依存しない
  固定コスト（可視集合サイズにのみ依存）として支配的である
- 疎側再取得ループの累積コストは `sparse_refetch_summary` の
  `estimated_cumulative_mixed_median_us=124551`（`search_within_fetch_k=<k>`
  段——各 `fetch_k` を全クエリで round-robin 測定した**全クエリ混合集団**の
  実測中央値であり、クエリ別の実測値ではない——を、最も再取得回数が多い
  クエリの実スケジュールに沿って合算した**推定値**。以前は
  `cumulative_median_us`・「実測値」「最悪ケース」と表記していたが、実体は
  全クエリ混合中央値による推定である。codex-review P1 指摘対応・PR #416）で、
  `hybrid_search_cached_index` の実測 median（115.7ms）と近い値になった
  （`search_within` 自体が `fetch_k` に依存せずほぼ一定のため、再取得回数の
  多寡がほぼ線形にコストへ跳ね返る。ただし上記のとおり全クエリ混合中央値に
  よる推定であり、クエリ別の真の累積コストとの乖離は未検証）
- 本 ADR 前節（Issue #356）が「残差 36%（約 100ms）」としていた帰属不能分は、
  本実測により**大部分が疎側再取得ループ（`search_within` を 6〜7 回繰り返す
  こと）で説明できる**ことが確認された。`search_within` 1 回あたりのコストの
  半分（区間 2・df 再計算パス）は `fetch_k` を増やしても変わらないため、
  再取得回数そのものを減らす（同点誘発を弱める・境界判定を改善する）よりも、
  df 再計算パスを転置索引（`term -> doc_id` の逆引き構造）で置き換え、
  クエリ語ごとに可視 subset を全走査せず該当 posting のみを辿る方式へ変える
  方が、再取得 1 回あたりのコストを直接削減でき効果が大きいと考えられる

### 転置索引化（Phase 1 後続）への示唆

親 Issue 系譜（#355 → #356 → #357 → #358 → 本 Issue）が指す「転置索引化」を
実施する場合、本実測が示す優先順位は以下のとおり:

1. **df 再計算パス（区間 2）**: 転置索引があれば「クエリ語ごとに `term -> df`
   を可視 subset に限定して再計算する」処理は、posting リストの長さを
   可視集合でフィルタしてカウントするだけになり、可視 subset 全件の線形走査
   （現状 O(|subset| × |query_terms|)）を避けられる
2. **スコアリングパス（区間 3）**: 同様に posting リスト経由で「その語を含む
   文書」だけを走査すればよくなり、subset 全件を舐めて `term_freq.get` する
   現状より削減余地がある（ただし本実測では区間 3 は区間 2 より小さい寄与
   〔35〜40% 対 50%〕であり、優先度は区間 2 より低い）
3. 疎側再取得ループの発火回数自体（本ベンチの同点誘発が強い合成コーパスでは
   6〜7 回）は転置索引化では直接減らない（境界同点判定のロジックは変わらない
   ため）。ただし 1 回あたりのコストが下がれば累積コストは比例して下がる

### 限界・申し送り

- 本実測は非専有環境（並行エージェントあり）での 1 回実測であり、専有環境
  での再実測はオーナー／実装担当の判断で別途実施する
- 本ベンチの合成コーパス（40 語の語彙を等間隔で回転させるだけの構成）は
  BM25 スコアが同点になりやすく、疎側再取得の発火回数（6〜7 回・うち過半が
  exhaustive 到達）は実コーパスでの発火回数の上限に近い可能性がある。実
  コーパスでの疎側再取得発火回数は本実測より少ない可能性がある点に注意
- production 側フック案（`hybrid_search_with_diagnostics` のような診断 API を
  `hybrid.rs` へ追加する案）は、当初の実測ではベンチ側複製＋忠実性検証のみで
  要件を満たせると判断し一旦不採用としたが、後述の codex-review P1 指摘への
  対応で production の疎側再取得ループ本体を共有内部関数へ抽出したうえ診断用
  の公開フックを追加する形へ変更した（`crates/engine/src/` は最終的に変更あり。
  詳細は直後の記述を参照）。
  `SparseIndex::search_within` は当初 `hybrid.rs::hybrid_search_boosted` から
  具象型 `&SparseIndex` へ直接呼ばれる構造で、密側の `RefetchTrackingProvider`
  （`&dyn SearchProvider` を介した外部観測）と同型の呼び出し回数観測フックが
  存在しなかった（codex-review P1 指摘・PR #416。当初の `sparse_refetch_schedule`
  はベンチ側複製〔`boundary_tie_decision`〕による予測値であり、production の
  実呼び出し列そのものとは突き合わせていなかった）。この指摘への対応として、
  `hybrid_search_boosted` の疎側再取得ループ本体を `hybrid.rs::sparse_refetch_loop`
  （private）へ抽出し、テスト・ベンチ向けの薄い公開フック
  `engine::hybrid::sparse_refetch_observed`（署名・挙動は同一。実際に呼ばれた
  `fetch_k` の列も返す）を追加した（production の疎側検索処理自体は無変更・
  1 実装を production 経路とフックの双方が共有）。`harness/hybrid_profile.rs::
  sparse_refetch_schedule` はこのフック経由で production と同一のコードパスを
  実行するようになったため、`fetch_ks` はベンチ側予測ではなく実観測であり、
  境界同点判定の複製（`boundary_tie_decision`）はもはや疎側スケジュールの
  算出に使わない（密側 `dense_refetch_schedule` の予測 ↔
  `RefetchTrackingProvider` 実測突き合わせでのみ引き続き使用）。この変更に伴い、
  間接検証だった `verify_sparse_schedule_terminal_is_stable`（終端 1 段先の
  プレフィックス固定点チェック）は不要になったため削除した（実観測に対する
  終端安定性の間接検証という位置づけ自体が意味を失うため。加えて cursor[bot]
  レビュー〔PR #416〕は同チェックが `k >= pool_depth` の場合に境界同点グループ
  が成長中でもプレフィックスが不変になり早期停止を検出できない構造的な穴を
  指摘していた）
- 転置索引化そのもの（設計・実装）は本 Issue のスコープ外。本節の帰属分析・
  優先順位は次段の設計判断の入力として記録するに留める

## Issue #388: term インターニング後の build・`search_within` 実測

親 Issue #386（転置索引化）の基盤整備として、`SparseIndex` の内部表現を
`BTreeMap<String, _>` から term 辞書（`String -> TermId(u32)`）＋
`TermId` キーの `Vec` へ置き換えた（`crates/engine/src/sparse.rs`。
公開 API のシグネチャ・契約は不変）。本節は Issue #356・#387 と同一の
`make bench-hybrid-profile` を変更後に再実行した実測値を、変更前
（本 ADR 前節時点。以下「基線」）と比較する。

### 測定条件

- 測定環境・コーパス・クエリ集合・プロトコルは Issue #356・#387 と同一
  （25,000 行・dim 128・単一テナント・全行 `Visibility::Public`・
  `MeasurementConfig::new(20, 30, SEED)`）。基線・本実測はいずれも
  非専有環境（並行エージェントあり）での 1 回実測であり、時点も異なる
  （厳密な同一マシン・同一時刻での前後比較ではなく、Issue #356・#387 の
  ADR 記載値を参考基線として扱う）
- ベンチ起動時の忠実性検証（`replica matches real search_within` を含む
  `hybrid_profile: fidelity checks passed ...`）は変更後も通過しており、
  クエリ語の走査順（辞書順）を維持したことによるスコアのビット一致は
  この検証と `sparse.rs` 内の参照実装比較テスト（`search_score_is_bit_
  identical_to_reference_btreemap_implementation` 等）の双方で担保される
- `harness/hybrid_profile.rs::ProfileSparseIndex`（`search_within` の
  区間分解複製）・build 3 段複製（`tokenize_term_freq`／
  `tokenize_term_doc_freq`）は本 Issue では変更していない（3.6 節の設計
  判断どおり、旧 `BTreeMap<String,u32>` 構造の参照実装として据え置き）。
  そのため下表の `search_within_subset_only`／`subset_df`／
  `replica_full`（複製経由の区間別測定）は変更の影響を受けず基線と
  近い値のままであり、**実装の変更を反映するのは `sparse_build_total`・
  `hybrid_search_cached_index`・`search_within_fetch_k=<k>`
  （いずれも実 `SparseIndex` の公開 API を直接呼ぶ測定）のみ**である点に
  注意する

### 実測結果（前後比較）

| 段 | 基線 median | 本実測 median | 変化 |
| -- | ----------: | -------------: | ---- |
| `sparse_build_total`（`SparseIndex::build` 単体） | 176.4ms | 50.1ms | **約 71.6% 短縮** |
| `tokenize_only`（累積） | 20.3ms | 22.2ms | ほぼ不変（ノイズ帯。tokenize 段自体は本 Issue の対象外） |
| `tokenize_term_freq`（累積） | 85.2ms | 82.3ms | ほぼ不変（複製は旧構造のまま） |
| `tokenize_term_doc_freq`（累積） | 150.9ms | 159.5ms | ほぼ不変（同上） |
| `hybrid_search_cached_index`（`provider_calls_max=1`） | 115.7ms | 25.3ms | **約 78.1% 短縮** |
| `search_within_fetch_k=400`〜`25000`（実 API 単体） | 17.2〜18.8ms | 2.6〜3.4ms | **約 82〜86% 短縮** |
| `search_within_subset_only`（複製・区間 1） | 0.78ms | 0.80ms | ほぼ不変（複製は旧構造のまま） |
| `search_within_subset_df`（複製・区間 1+2） | 9.66ms | 9.61ms | ほぼ不変（同上） |
| `search_within_replica_full`（複製・区間 1+2+3） | 15.25〜16.76ms | 15.42〜16.27ms | ほぼ不変（同上） |

疎側再取得ループの発火回数・`fetch_ks`（`engine::hybrid::
sparse_refetch_observed` 経由の実観測）は基線と完全に一致した
（`calls=7/7/7/6/6`・各 `fetch_ks` 列・`reached_cap_count=3` すべて同一）。
これは term インターニングがスコア・順位を変えないことの追加の裏付けであり、
境界同点判定（`hybrid.rs`）はそもそも本 Issue の変更対象外なので当然の帰結
でもある。

### 解釈

- `sparse_build_total`・`search_within_fetch_k`・`hybrid_search_cached_index`
  （いずれも実装を直接測る経路）はすべて基線を大きく下回っており、
  ノイズ帯（本開発環境の非専有実測で見られる数 % 〜十数 % の run-to-run
  差分）を明確に超える改善である。一方、`tokenize_*`・
  `search_within_subset_*`／`replica_full`（いずれも旧構造の複製経由）は
  ほぼ横ばいであり、これは複製が変更対象外であることの整合的な裏付けに
  なっている（変更の影響を受けるべき経路とそうでない経路が期待どおりに
  分離して観測された）
- `search_within_fetch_k` の改善幅（82〜86%）が `search_within_subset_df`
  （複製・ほぼ不変）と乖離しているのは、複製が模しているのは旧
  `BTreeMap<String,u32>` 方式の df 再計算パスであり、実装済みの新方式
  （`TermId` 添字での `Vec<u32>` アクセス・`binary_search` によるクエリ側
  `term_freq` 参照）は複製に反映されていないため。実装の効果を見るには
  `search_within_fetch_k`（実 API）を正とする（3.6 節と同じ位置づけ）
- `hybrid_search_cached_index` の改善（78.1%）は `search_within` 1 回あたりの
  短縮が疎側再取得ループ（5〜7 回発火）を通じて積算されたものであり、
  受け入れ条件 2（`SparseIndex::build` の短縮を実測・記録）に加えて
  検索経路全体への波及効果も確認できた
- `sparse_build_total` の内訳（tokenize／term_freq 構築／doc_freq マージが
  ほぼ均等）を前提にすると、term インターニングは term_freq 構築・
  doc_freq マージの 2 段（Issue #356 実測で合計約 6 割）が主な短縮対象と
  見込んでいたが、実測の短縮幅（71.6%）はその見込みを上回った。tokenize 後
  の `Vec<TermId>` への intern・sort・ランレングス圧縮が `BTreeMap` の
  都度挿入（比較木の再バランス・エントリごとのヒープ確保）より単純な
  線形処理で完結することが寄与していると考えられる（詳細な段別内訳への
  分解は本 Issue のスコープ外）

### 受け入れ条件との対応

計画（`_/local-plans` 相当）の受け入れ条件 2「`SparseIndex::build` の所要
時間が基線比で短縮していることを `make bench-hybrid-profile` で実測・記録」
を、上表の `sparse_build_total`（176.4ms → 50.1ms）で満たしたと判断する。

### 申し送り

- 本節の実測は非専有環境・1 回実測であり、専有環境での再実測はオーナー／
  実装担当の判断で別途実施する
- `harness/hybrid_profile.rs` の build 3 段複製・`ProfileSparseIndex` は
  旧 `BTreeMap<String,u32>` 構造の参照実装のまま残置した。これらの複製の
  刷新（`TermId` 構造を反映した複製への更新）は本 Issue のスコープ外とし、
  後続（#389／#390 または別 Issue）への申し送りとする
- 後続 #389（posting list）・#390（可視ビットマップ＋posting 走査）は
  `TermId` を前提にする。本節の実測（特に df 再計算パス・スコアリング
  パスの改善余地の見立て）は前節（Issue #387）「転置索引化（Phase 1
  後続）への示唆」の優先順位付けを変えるものではない（転置索引化は
  `search_within` 単体のアルゴリズム的な計算量を変える話であり、本 Issue
  の定数倍改善とは独立に効果が見込まれる）

## Issue #389: posting list・doc_len／doc_ids 配列追加後の build・常駐メモリ実測

親 Issue #386（転置索引化）Phase 1 の 3 番目のタスクとして、`SparseIndex`
（`crates/engine/src/sparse.rs`）へ `TermId` 添字の転置索引
（`postings: Vec<Vec<(u32, u32)>>`）・`doc_idx` 添字の `doc_len`／`doc_ids`
配列を追加し、`id_index` を `BTreeMap` から `HashMap` へ置換した。文書を
`doc_idx` 昇順に処理する構築順序自体が posting list の
「`doc_idx` 昇順・重複なし」不変条件を満たすため、ソートは不要（`with_params`
内でランレングス圧縮済みの `term_freq` を 1 回走査して各 `postings[t]` へ
`append` するのみ）。`search`／`search_within` は本 Issue では未参照のまま
残置し（撤去・経路切替は #390）、公開 API のシグネチャ・契約・スコアの
ビット一致は不変（`search_score_is_bit_identical_to_reference_btreemap_
implementation` 等の既存参照実装比較テストが green のまま）。

### 測定条件

- 測定環境・コーパス・クエリ集合・プロトコルは Issue #356・#387・#388 と
  同一（25,000 行・dim 128・単一テナント・全行 `Visibility::Public`・
  `MeasurementConfig::new(20, 30, SEED)`）。非専有環境（並行エージェントあり）
  での 1 回実測であり、Issue #388 の実測値を参考基線として扱う（厳密な
  同一マシン・同一時刻での前後比較ではない）
- ベンチ起動時の忠実性検証（`fidelity checks passed ...`）は本 Issue の
  変更後も通過している

### 実測結果

| 指標 | Issue #388（参考基線） | 本実測 |
| ---- | ----------------------: | -----: |
| `sparse_build_total` median | 50.1ms | 48.1ms（ノイズ帯内。転置索引構築分の増加は測定誤差に埋もれる規模） |
| `sparse_build_total` p95 | — | 49.5ms |

| 指標（新規計測。Issue #389） | 実測値 |
| ---- | -----: |
| `approx_heap_bytes()`（`SparseIndex` 保持時） | 19,553,564 バイト（約 18.65 MiB） |
| `vm_rss_kb_before` → `vm_rss_kb_after`（`SparseIndex::build` 1 回分の直前直後） | 23,416 kB → 43,928 kB（差分 20,512 kB。約 20.03 MiB） |
| `vm_hwm_kb`（測定時点までのピーク RSS） | 43,928 kB |

RSS 計測は、プロセス内でこの 1 回が最初かつ唯一の `SparseIndex::build`
呼び出しになる位置へ置いている（codex-review 指摘・Cursor Bugbot 指摘・
PR #424。当初は複製実装の構造的整合性チェック〔`build_actually_
succeeds`〕を別途 `SparseIndex::build` して破棄する形で RSS 計測の直前に
残しており、その確保・解放でアロケータ／ページがウォームになって、
続く RSS 計測が「未ウォーム状態からの増分」にならず過小評価していた
（後述の旧実測値）。整合性チェックの目的〔複製実装の転記ミスで build
自体が失敗する入力を検出する〕は「同一入力で `SparseIndex::build` が
成功するか」の確認に尽きるため、RSS 計測用に構築するインデックスの
`is_ok()` をそのままその判定に使う形へ統合し、二重構築を無くした。
これにより `core.execute_sql` 未呼び出し〔`sql/sparse_cache.rs::
SparseIndexCache`（Issue #357）経由の構築機会も無い〕に加えて、コーパス
生成後の `SparseIndex::build` としても最初の 1 回になっている）。

### 解釈

- `sparse_build_total` は Issue #388 実測（50.1ms）とノイズ帯内で同水準
  （48.1ms）であり、posting list・`doc_len`／`doc_ids` 配列の構築（ランレングス
  圧縮済み `term_freq` を 1 回追加走査するだけの線形処理）が build 全体の
  所要時間へ与える影響は、本測定の分解能では有意な劣化として観測されな
  かった
- RSS 差分は 20,512 kB（約 20.03 MiB）であり、`approx_heap_bytes()`
  （約 18.65 MiB）と近い水準まで一致した。修正前は事前ウォームアップの
  影響で RSS 差分が 4,408 kB（約 4.31 MiB）と大きく過小評価されていたが
  （上記「RSS 計測」節参照）、二重構築を解消したことで
  `approx_heap_bytes()` の概算（実確保量を下回らない側に倒す設計。
  `approx_heap_bytes` のドキュメンテーションコメント参照）との差が
  実装の付随確保分（`HashMap`／`Vec` の予約容量の余剰等）相当の範囲に
  収まった。なお RSS 増分は「その時点でのプロセス全体に対する新規
  ページイン量」であり `approx_heap_bytes()` とは厳密には異なる量を
  指すため、コーパス生成（`generate_corpus`。25,000 件の本文・ベクトル
  生成）や一時 DB オープン等の先行処理が確保したページの再利用分が
  含まれていない保証はない。真に隔離された増分（他の処理を一切行わない、
  プロセス起動直後の 1 回の `SparseIndex::build` 前後の RSS 差分）を
  見たい場合は、この計測点のみを単独プロセスで実行する運用
  （`BENCH_CORE16_DIAG` 系の「1 プロセス = 1 規模点」運用と同様の方針）が
  必要であり、これは今回のスコープでは実施していない（オーナー・運用者
  への申し送り）
- 受け入れ条件 5「メモリ増分を `bench-hybrid-profile` の RSS で記録する」は
  上記のとおり記録した

### 受け入れ条件との対応

1. posting list（`doc_idx` 昇順・順序構築）・`doc_len`・`doc_ids` を
   `SparseIndex` へ追加し、`DocEntry` は残置した（3.1〜3.2 節。実装は
   `crates/engine/src/sparse.rs`）
2. `id_index` を `HashMap<DocId, usize>` へ置換した
3. 単体テスト（`postings_reconstruct_tf_and_df_matching_doc_entry_for_
   all_docs` 等）で posting list から復元した tf／df が `DocEntry` 経由の
   値と全件一致することを固定した
4. 既存テスト（`cargo test -p engine --all-features`）は green・依存追加
   なし
5. メモリ増分を `bench-hybrid-profile` の RSS で記録した（上表。解釈欄の
   限界も含めて記録）

### 申し送り

- `search`／`search_within` の posting 走査化・`docs: Vec<DocEntry>` の
  撤去は #390 のスコープ
- `postings` の CSR 形（単一 `Vec` ＋ offsets）や量子化・skip list への
  圧縮は #391 以降・#394 の判断材料
- `harness/hybrid_profile.rs::ProfileSparseIndex`・build 3 段複製は
  Issue #388 からの申し送りどおり旧構造（`BTreeMap<String,u32>`）の参照
  実装のまま据え置いた（本 Issue でも変更していない）
- 専有環境での再実測・真に隔離された RSS 増分の単独プロセス実測は
  オーナー／運用者判断で別途実施する

## Issue #390: 可視ビットマップ＋posting 走査 1 パス化後の実測

親 Issue #386（転置索引化）Phase 1 の最終タスクとして、`search`／
`search_within`（`crates/engine/src/sparse.rs`）を Issue #389 で追加した
転置索引（`postings`）へ切り替え、可視部分集合の全件線形走査（`docs: Vec<
DocEntry>` を経由し各文書の `term_freq` を二分探索する方式）を、可視集合を
`doc_idx` 空間のビットマップへ変換したうえでクエリ語ごとに `postings[t]`
だけを辿る 1 パス走査（共通コア `score_by_postings`）へ置き換えた。
`DocEntry`／`docs` フィールド自体を撤去し、`search`（`ScoreScope::All`）・
`search_within`（`ScoreScope::Visible`。RLS 相当のテナント境界縮約契約は
不変）の両方をこの共通コアへ統一した。スコアの f64 ビット一致は不変
（`crates/engine/tests/hybrid_profile_accept.rs::profile_sparse_index_
replica_matches_real_search_within`〔ベンチ起動時の忠実性検証としても実行〕・
`tests/sparse_cache_recall.rs` の cold/hot 等価性・大規模段〔25,000 件〕・
`tests/hybrid_recall.rs` 層 A 固定値アサーションで検証済み）。

### 測定条件

Issue #356・#387・#388・#389 と同一環境・同一コーパス・クエリ集合・
プロトコル（25,000 行・dim 128・単一テナント・`MeasurementConfig::new(20,
30, SEED)`）。非専有環境（並行エージェントあり）での 1 回実測であり、
Issue #389 の実測値を参考基線として扱う。

### 実測結果

| 指標 | Issue #388/#389（参考基線） | 本実測（Issue #390） |
| ---- | ---------------------------: | --------------------: |
| `sparse_build_total` median | 48.1ms（#389） | 47.1ms（ノイズ帯内・不変。posting 走査化は build 経路を変更しないため想定どおり） |
| `approx_heap_bytes()`（`SparseIndex` 保持時） | 19,553,564 バイト（約 18.65 MiB。#389） | 12,153,564 バイト（約 11.59 MiB。`docs: Vec<DocEntry>` 撤去分だけ縮小） |
| `search_within_fetch_k=25000`（全可視集合。実 API・単発呼び出し） | 2.6〜3.4ms（#388） | median 1,267µs／p95 1,318µs（約 1.27〜1.32ms。基線比で概ね 2〜2.7 倍短縮） |
| `hybrid_search_cached_index`（事前構築済み `SparseIndex`。実クエリ経由の hybrid 経路全体） | 25.3ms（#388） | median 9,699µs／p95 10,318µs（約 9.7〜10.3ms。基線比で概ね 2.5 倍短縮） |

`search_within_fetch_k` は疎側再取得ループの各段（400〜25,000）でいずれも
明確に短縮しており（例: `fetch_k=400` は median 478µs、`fetch_k=25000` は
median 1,267µs で、可視集合サイズに対して準線形に近い伸び方を保っている）、
可視集合の大きさに関わらず「密検索と同オーダー」（`sql_dense_knn` median
6,152µs）を下回る水準まで達した。

### 解釈

- `search_within_fetch_k` の短縮は、旧実装（可視部分集合の全件線形走査＋
  文書ごとの `term_freq` 二分探索）から「クエリ語ごとに posting list だけを
  辿る」方式への計算量オーダーの変化（モジュール doc コメント参照）が
  そのまま実測へ反映されたものと解釈できる
- `hybrid_search_cached_index`（sparse index はキャッシュヒット・実際の
  hybrid 検索経路。疎側再取得ループを含む）が 25.3ms → 9.7ms（概ね 2.5 倍）
  短縮したことは、Issue #387 で確認した「疎側再取得ループが単発クエリ
  レイテンシへ寄与する」構造（`sparse_refetch query=* calls=6〜7`）を踏まえ、
  ループ 1 回あたりのコストが本 Issue の変更で大きく下がったことを裏付ける
- `approx_heap_bytes()` の縮小（約 18.65 MiB → 約 11.59 MiB）は
  `docs: Vec<DocEntry>`（各文書の `term_freq: Vec<(TermId, u32)>` を含む）
  撤去分にほぼ相当する。`postings`・`doc_len`・`doc_ids` は既存のまま
  （Issue #389 で構築済み）であり本 Issue では増減していない
- `sparse_build_total` が変化しないのは想定どおり（本 Issue の変更対象は
  `search`／`search_within` のみで、build 経路〔posting 構築含む〕は
  Issue #389 のまま無変更）

### 受け入れ条件との対応

1. `search`／`search_within` を可視ビットマップ＋posting 走査の共通コアへ
   統一し、`DocEntry`／`docs` を撤去した（`crates/engine/src/sparse.rs`）
2. 旧実装（`BTreeMap<String,u32>` 参照実装・`harness/hybrid_profile.rs::
   ProfileSparseIndex` 複製）との等価性を単体テスト（部分可視・境界値
   ケース含む）・25,000 件規模の cold/hot 等価性テスト
   （`tests/sparse_cache_recall.rs`）で固定した
3. `make bench-hybrid-profile` を実行し、`search_within_fetch_k`・
   `hybrid_search_cached_index`・`approx_heap_bytes` の前後比較を上表に
   記録した（目標「密検索と同オーダー」を達成）
4. Recall 層 A（固定値アサーション。`tests/hybrid_recall.rs`）は
   green のまま不変（スコアのビット一致契約により構造的に不変）

### 申し送り

- ビットマップ構築（`VisibleBitmap::build`。`BTreeSet` 走査＋`HashMap`
  lookup × 可視集合サイズ）が疎側再取得ループの各段で毎回再構築される点
  （再取得ループ間でのビットマップ再利用）は #392 領域の判断材料として
  申し送る
- `score_by_postings`（`sparse.rs`）のスコアアキュムレータ `acc: Vec<f64>`
  はコーパス全体の文書数 `N`（`self.doc_ids.len()`）で毎呼び出し新規確保・
  ゼロ初期化しており、モジュール doc コメントが述べる「コーパス文書数 `N`
  そのものには比例しない」計算量契約は走査量についてのみ成立し、この
  確保・ゼロ初期化自体は `O(N)` のまま残っている（Issue #390 レビュー指摘。
  旧実装からの退行ではなく改善だが契約には未到達）。`search_within` は
  疎側再取得ループから 1 クエリあたり 6〜7 回呼ばれる（Issue #387）ため、
  大規模コーパスでは無視できないコストになり得る。真に N 非依存化するには
  呼び出し間で `acc` バッファを再利用し `touched` でタッチ済み要素のみ
  リセットする方式等が考えられるが、現行の `&self`（非 `&mut self`）
  シグネチャを維持したまま呼び出し間で状態を持ち越す設計変更を要するため、
  ビットマップ再利用と合わせて #392 領域の判断材料として申し送る
- `postings` の CSR 形・量子化・skip list への圧縮は引き続き #391 以降・
  #394 の判断材料
- 段別観測フック（`search_within` 内部の bitmap 構築／posting 走査／
  Top-k 選出の内訳）は本 Issue では追加していない（`search_within_fetch_k`
  の外形計測に留めた）。より細かい内訳が必要になった場合の追加ポイントは
  `sparse.rs::score_by_postings` 内の各段
- 専有環境での再実測はオーナー／運用者判断で別途実施する

## Issue #391: 文書長クラス別テーブルと select_nth 型 Top-k 導入後の実測

対応: Issue #391（`perf(engine): fieldnorm 量子化スコアテーブルと select_nth 型
top-k を SparseIndex へ導入`）。前提: TASK-102・TASK-104、
`docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3（判定内容・数値基準は
spec 側が SSOT。本節も spec 由来の pass/fail 閾値を持たない情報提供専用の
実測記録）。

### 変更内容

`crates/engine/src/sparse.rs::score_by_postings` のホットパスを 2 点変更した。

1. **文書長正規化項のキャッシュ化**: BM25 の `k1 * len_norm`（`len_norm =
   1-b+b*doc_len/avgdl`）はクエリ内で `avgdl` が確定した後は文書長のみに
   依存するため、ヒットごとに再計算する代わりに、build 時に構築した文書長
   クラス表（`len_classes: Vec<u32>`。コーパス中の相異なる文書長を昇順・
   重複なしに並べたもの・`doc_len_class: Vec<u32>`。`doc_idx` → クラス添字）
   をもとに、当初はクエリ時（`avgdl` 確定直後）に `len_classes` 全体を
   走査して `k1_len_norm: Vec<f64>`（クラス数分）を一括構築する設計を
   採ったが、これは実際にヒットする文書長クラス数（<= ヒット数 `M`）に
   関わらず常に `len_classes.len()`（コーパス中の相異なる文書長数。最悪
   `O(N)`）分の計算を行ってしまい、選択性の高いクエリで `search`/
   `search_within` の走査量に比例する計算量契約から外れるという指摘
   （PR #426 codex-review）を受け、ヒットループ内で実際にタッチした
   クラスのみを `k1_len_norm_cache: HashMap<u32, f64>` へ遅延計算・
   キャッシュする方式へ変更した。式・演算順（`k1_len_norm[c]` は旧実装の
   `self.k1 * len_norm` と同一算出）は不変のため `denominator` は旧実装と
   引き続きビット一致する。`HashMap` のキー・値は決定的に定まる（`class`
   → 一意な `f64`）ためスコア自体の決定性には影響しない
2. **Top-k 選出の select_nth 型化**: `BinaryHeap<Reverse<Candidate>>` への
   逐次 `O(log k)` 挿入を、tantivy の `TopNComputer` を参考にした
   `TopKSelector`（2k 件バッファ＋`select_nth_unstable_by` による一括切り詰め
   ＋`threshold` による早期棄却）へ置き換えた。出力する Top-k 集合・順序は
   いずれも `Candidate` の全順序（スコア降順・同点は `doc_id` 昇順）だけで
   一意に決まるため、選出アルゴリズムの違いは出力に影響しない

`SparseIndex` の公開 API（`build`/`with_params`/`search`/`search_within`/
`approx_heap_bytes`）シグネチャは無変更。新規エラー変種
`SparseError::LenClassBuildFailed`（文書長クラス表構築の不変条件違反。
構築ロジック上到達不能だが `unwrap`/`expect` を使わず fail-closed に拒否する
防御）を追加したが、呼び出し側（`sql/exec.rs::map_hybrid_error`）はワイルド
カード分岐で受けるため既存の分類・応答契約に影響しない。

### 受け入れ条件 1（ビット一致）の検証

`crates/engine/src/sparse.rs` 内の単体テストに以下を追加し、いずれも green
であることを確認した（詳細な観点は各テスト名を参照）。

- `len_classes_are_sorted_unique_and_map_back_to_doc_len`
- `len_norm_table_reproduces_inline_formula_bitwise`
- `search_and_search_within_top_k_bit_identical_to_reference_on_mixed_corpus`
  （2,000 件規模の混成コーパス・複数クエリ・`k ∈ {1, 5, 20, 100, M/2, M, M+1}`
  で `search`／`search_within` 双方が参照実装〔`BTreeMap` 手計算〕とビット一致）
- `top_k_selector_matches_full_sort_for_random_candidates`（`M`/`k` の境界
  `M > 2k`・`M <= k`・`k == 0`・`M == 0` を網羅）
- `top_k_selector_boundary_tie_group_prefers_smaller_doc_id`
- `search_within_top_k_cut_inside_tie_group_is_deterministic_and_prefix_consistent`
  （`hybrid.rs::sparse_refetch_loop` の倍増再取得と整合する前方一致契約）
- `search_within_len_norm_table_uses_visible_avgdl_only`（RLS 縮約契約の回帰。
  可視外の文書長がテーブル・スコアへ影響しないことを固定）
- `approx_heap_bytes_accounts_len_class_arrays`

既存のビット一致テスト（`search_score_is_bit_identical_to_reference_
btreemap_implementation`・`search_within_score_is_bit_identical_to_reference_
for_visible_subset`・`search_selection_boundary_prefers_smaller_doc_id_on_
tie`・`search_tie_breaks_by_doc_id_ascending`）・`hybrid_profile_accept.rs::
profile_sparse_index_replica_matches_real_search_within`（ベンチ起動時
fail-closed の忠実性検証）・`sparse_cache_recall.rs`（cold/hot 等価性）・
`hybrid_recall.rs` 層 A の固定値アサーション（`hits20 == 182`・
`sparse_hits20 == 166`・大規模段 `hits20 == 385`/`hits100 == 648`）は無変更で
green のまま（スコアのビット一致契約により構造的に不変）。

**256 段ロッシー量子化の実験（Step 4・不採用判断）**: `sparse.rs` 内の
`#[ignore]` 手動実験テスト `fieldnorm_256_quantization_rank_divergence_
report`（`crates/engine/src/sparse.rs` 内 test-only。tantivy の実装・テーブル
値は転記せず自作の代表値写像規則のみを使う）で、production の厳密クラス表と
256 段代表値写像を 5,000 件規模のコーパス・複数クエリ・複数 `k` で比較できる
ようにした。5 クエリ × 3 段の `k`（10・20・100）＝ 15 組の比較で、実測は
「0/15 件で Top-k の doc_id 列が量子化により変動」（本開発環境・1 回実測。
決定的フィクスチャのため再現性はある）だった。production は本実験の結果
によらず厳密クラス表（`len_classes`）を採用し、256 段ロッシー量子化は
不採用（Rejected）と判断する。理由はビット一致契約（`replica_matches_real`・
Recall 層 A 固定値・`sparse_cache_recall.rs` cold/hot 等価性が前提とする）に
反すること自体であり、変動件数が実測どおり 0 であっても判断は変わらない
（Issue #391 実装計画の「変動が 0 だった場合も同じ結論とし、その事実を記録
する」方針どおり）。性能面でも、テーブル参照コストはクラス数に依存しない
ため厳密表（クラス数 = 相異なる文書長の個数、通常は文書数以下）に対する
優位はない。

### 受け入れ条件 3（前後比較）の実測

`make bench-hybrid-profile`（`cargo bench --bench hybrid_profile_bench -p
engine --features bench-internals`）を本開発環境（非専有・並行エージェント
あり）で本変更の前後（before: 本 Issue の変更を `git stash` で除いた状態・
after: 本 Issue の変更を適用した状態）それぞれ 1 回ずつ実行した。

| 指標 | before | after |
| ---- | -----: | ----: |
| `hybrid_search_cached_index` p95 | 10,602µs | 9,014µs |
| `hybrid_search_cached_index` median | 9,785µs | 8,288µs |
| `search_within_fetch_k=25000`（全可視集合）p95 | 1,317µs | 813µs |
| `search_within_fetch_k=25000`（全可視集合）median | 1,289µs | 798µs |
| `sparse_index_resident` `approx_heap_bytes()` | 12,153,564 | 12,253,568 |

`search_within_fetch_k` の他段（400〜12,800）も一様に短縮しており
（例: `fetch_k=12800` median 1,102µs → 637µs）、Top-k 選出の
`select_nth_unstable_by` 化・文書長正規化項テーブル化の効果は `k`（フェッチ
件数）が大きいほど顕著に表れている。`sparse_build_total`（build 経路。本
Issue は `score_by_postings` のみを変更対象とし build 側の文書長クラス表
構築コスト自体は変更していない）は before/after で誤差の範囲（47,476µs →
47,091µs）に留まり、想定どおり変化していない。

`approx_heap_bytes()` はクラス表 2 本（`len_classes`・`doc_len_class`）の追加
分だけわずかに増加した（+100,004 bytes ≈ +97.7 KiB。25,000 件規模コーパス
での実測）。文書長のバリエーションが多いほど `len_classes` が大きくなるため
この増加量はコーパス依存だが、`search_within_fetch_k` の短縮幅（数百µs〜
数百µs、大きな `k` ほど拡大）に対して無視できる規模と判断する。

### 受け入れ条件との対応

1. §「受け入れ条件 1（ビット一致）の検証」参照。決定的フィクスチャで
   Top-k・同点グループは導入前と一致することを機械検証した（不一致は
   検出されなかった）
2. Recall 層 A は green のまま不変（スコアのビット一致契約により構造的に
   不変）。層 B（`recall.yml`。environment `recall-gate` の secrets 閾値）は
   ローカルで実行できないため、マージ後の `workflow_dispatch` 実測は
   引き続き管理者作業として申し送る
3. §「受け入れ条件 3（前後比較）の実測」参照

### 申し送り

- `acc: Vec<f64>` の `O(N)` 確保は Issue #546 で解消（索引の寿命内で再利用する
  スクラッチプールへ置換。詳細は本ドキュメント「Issue #546」節参照）。
  `VisibleBitmap` の再取得ループ間再利用は引き続き #392 領域の判断材料
- posting の CSR 化・量子化・skip list への圧縮は引き続き #394 の判断材料
- 専有環境での `bench-hybrid-profile` 再実測はオーナー／運用者判断で別途実施
- 256 段ロッシー fieldnorm 量子化は本 Issue で不採用と判断した（理由は上記
  「256 段ロッシー量子化の実験」節参照）。将来スコア型・ビット一致契約自体を
  見直す場合の再検討ポイントとして記録する

## Issue #392: 疎側再取得ループの再スコアリング回避後の実測

対応: Issue #392（`perf(engine): 境界同点グループ再取得ループで疎側の
再スコアリングを回避`）。前提: TASK-102・TASK-104、
`docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3（判定内容・数値基準は
spec 側が SSOT。本節も spec 由来の pass/fail 閾値を持たない情報提供専用の
実測記録）。関連: Issue #387（本節が言及する `sparse_refetch_observed` の
導入元）・Issue #390 申し送り（「真に N 非依存化するフォローアップ」として
本 Issue を予告していた記述）・CORE-7（`hybrid_search` を通らない測定経路の
ため本変更の影響を受けない。`docs/design/hybrid-refetch-latency.md` 参照）。

### 変更内容

`hybrid.rs::sparse_refetch_loop`（疎側再取得ループ。境界同点グループ完全化
のため `fetch_k` を倍増させながら同じクエリを複数回再評価する。Issue #320）
が各ラウンドで `SparseIndex::search_within` を呼び直していたのを、
`SparseIndex::score_within`（新設）を 1 回だけ呼んでスコア `> 0` の全候補
（`SparseScored`）を確定させ、各ラウンドは `SparseScored::top`（新設）で
その候補集合から前方一致な Top-k を切り出す方式へ変更した。

- `sparse.rs::score_pass`（新設・private）: `score_by_postings` から
  「posting 走査によるスコアアキュムレータ（`acc: Vec<f64>`）・タッチ済み
  `doc_idx` 列（`touched`）の構築」部分を切り出した共通コア。`score_by_
  postings`（`search`/`search_within` が使う 1 回限りの Top-k 選出）・
  `score_within`（新設）の両方がこれを共有する
- `sparse.rs::SparseScored`（新設・公開型）: `score_within` が返すスコア
  済み候補集合。`top(k)` は `candidates` の先頭 `sorted_len` 件が
  `Candidate` の全順序（スコア降順・同点 `doc_id` 昇順）で確定済みという
  不変条件を保ちながら、未整列の尾部だけを `select_nth_unstable_by`＋
  `sort_unstable_by` で段階的に整列する（`TopKSelector` と同じ選出方式・
  同じ sort-determinism マーカー運用）。前方一致・冪等性はこの構造その
  ものから導かれる（同一の候補配列から切り出すため）
- `sparse.rs::score_within`（新設・公開）: 入力検証・空結果ケース・RLS
  相当のテナント境界縮約契約（統計母数・候補選出を可視集合へ限定し、
  インデックス全体の統計を参照しない）は `search_within` と同一。`k` を
  受け取らず `SparseScored` を返す点のみが異なる
- `hybrid.rs::sparse_refetch_loop`: ループ前に `score_within` を 1 回呼び、
  ループ内の `search_within` 呼び出しを `SparseScored::top` へ置換。
  `fetch_k` のスケジュール（`record_fetch_k` の列）・`TooManyCandidates`
  長さ検証・`validate_extended_pool`・`exhaustive` 判定・
  `resolve_boundary_tie_group`／`exclude_undetermined_boundary_group`・
  `MAX_FETCH_K` はいずれも 1 行も変更していない。密側再取得ループ・
  `complete_boundary_tie_group(_by)` も無変更

`SparseIndex` の既存公開 API（`build`/`with_params`/`search`/
`search_within`/`approx_heap_bytes`）シグネチャは無変更。新規公開面は
`SparseScored`・`score_within` のみの追加であり、既存呼び出し元
（`tests/hybrid.rs` の独立オラクル等）は `search_within` を使い続けられる。

### 受け入れ条件（等価性）の検証

`crates/engine/src/sparse.rs` 内の単体テストに以下を追加し、いずれも green
であることを確認した。

- `score_within_top_is_bit_identical_to_search_within_on_mixed_corpus`
  （2,000 件規模の混成コーパス・複数クエリ・`k ∈ {1, 5, 20, 100, M/2, M,
  M+1}` と疎側再取得ループの倍増スケジュール相当の値で `score_within(q,
  vis).top(k)` と `search_within(q, k, vis)` がビット一致）
- `sparse_scored_top_is_prefix_consistent_and_idempotent`（前方一致・冪等
  性・`k > M` での頭打ちを固定）
- `sparse_scored_top_k_zero_and_empty_scored_return_empty`
- `score_within_input_validation_precedes_empty_cases`（`QueryTooLong`・
  `TooManyQueryTerms` が空可視集合より優先する契約が `search_within` と
  同一であることの回帰）
- `score_within_statistics_are_isolated_from_invisible_docs`（RLS 縮約契約
  の回帰。`search_within_statistics_are_isolated_from_invisible_docs` と
  同型のシナリオを `score_within` で固定）

`crates/engine/src/hybrid.rs` 内の単体テストに以下を追加した。

- `sparse_refetch_loop_matches_per_round_search_within_oracle`: 本 Issue
  導入前の実装（各ラウンドで `search_within` を呼び直す）をテスト内に
  独立再実装したオラクルと、現行の `sparse_refetch_loop` が通常コーパス・
  全件同点コーパス・境界同点コーパス（`hybrid_search_boosted_sparse_
  tie_group_across_pool_boundary_is_id_independent` と同型）の 3 種で
  `(hits, sparse_limit)` を完全一致で返すことを固定

既存テスト（`tests/hybrid.rs` の独立オラクル、`tests/hybrid_recall.rs` 層 A
固定値、`tests/sparse_cache_recall.rs` cold/hot 等価性、
`tests/hybrid_profile_accept.rs`〔`--features bench-internals`〕、
`tests/soft_boost.rs`・`tests/plan_rls_boost.rs`）はすべて無変更で green の
まま（等価性契約により構造的に不変）。

### 実測

本開発環境（非専有・並行エージェントあり）で `make bench-hybrid-profile`・
`make bench-hybrid` を before（`origin/main` 9fd5afe。本 Issue の変更を含ま
ない）・after（本 Issue の変更を適用した状態）それぞれ 1 回ずつ実行した
（25,000 件規模コーパス・`fetch_k` 倍増スケジュールは同一〔400→800→…→25000。
7 ラウンド〕）。

| 指標 | before | after |
| ---- | -----: | ----: |
| `hybrid_search_cached_index`（実 hybrid 経路。cached index）p95 | 11,601µs | 6,320µs |
| `hybrid_search_cached_index` median | 11,020µs | 5,571µs |
| 疎側再取得ループ累積コスト median | 6,416µs（推定） | 3,427µs（実測） |

疎側再取得ループ累積コストは before/after で測定方法が異なる点に注意
（本 Issue によりベンチ側の計測手段自体が変わったため）: before は
`sparse_refetch_summary` の `estimated_cumulative_mixed_median_us`
（各ラウンドの `search_within` 単体実測 median を実スケジュールに沿って
合算した推定値。旧実装では疎側再取得ループそのものを直接計測する手段が
無かった）、after は `sparse_refetch_loop`（`sparse_refetch_observed` に
よる、疎側再取得ループ本体そのものの実測値）。

`hybrid_search_cached_index`（実際に production が通る hybrid 検索経路
そのもの）は median で約 1.98 倍（11,020µs → 5,571µs）短縮した。疎側再取得
ループの累積コストも約 1.87 倍（6,416µs → 3,427µs）縮小しており、
`hybrid_search_cached_index` の短縮幅の大半をこの疎側最適化が占める
（密側再取得ループ・`complete_boundary_tie_group` は無変更のため寄与しない）。

`make bench-hybrid`（Issue #324・同点誘発コーパス〔m=6,400 規模〕での A/B）
は次の通り、非劣化を確認した（本ベンチのコーパス規模は
`bench-hybrid-profile` の 25,000 件より小さく、`acc: Vec<f64>` の `O(N)`
再確保削減の絶対効果が相対的に小さいため、この規模では改善幅が測定ノイズと
同程度に留まる。CORE-7 は `hybrid_search` を通らないため測定経路として
本変更の影響を受けない）。

| 指標 | before | after |
| ---- | -----: | ----: |
| `large_tie_refetch` p95 | 6,292µs | 6,800µs |
| `large_tie_refetch` median | 5,467µs | 5,393µs |
| `large_no_refetch` p95 | 872µs | 1,361µs |
| `large_no_refetch` median | 823µs | 846µs |

`large_no_refetch`（再取得ループを 1 ラウンドで終える経路。本 Issue が変更
した再取得ループのコード自体は通るが `score_within`→`top` 1 回のみで
旧実装の `search_within` 1 回呼び出しと理論上ほぼ等価）の差分は non-tie 系
の実行に対する測定環境（非専有）のノイズの範囲と判断する。

### 受け入れ条件との対応

1. §「受け入れ条件（等価性）の検証」参照。決定的フィクスチャで融合結果・
   境界同点グループが導入前と一致することを機械検証した（不一致は
   検出されなかった）
2. `make bench-hybrid`（同点誘発コーパス側 `*_tie_refetch` 段）は非劣化
   （§「実測」参照。改善方向のシグナルは `bench-hybrid-profile`〔25,000 件
   規模〕側でより明確に確認できた）
3. Recall 層 A は green のまま不変（等価性契約により構造的に不変）。層 B
   （`recall.yml`。environment `recall-gate` の secrets 閾値）はローカルで
   実行できないため、マージ後の `workflow_dispatch` 実測は引き続き管理者
   作業として申し送る

### 申し送り

- 密側再取得ループの同種最適化は本 Issue のスコープ外（Issue 本文どおり）
- クエリ単位で残る `acc: Vec<f64>` の `O(N)` 確保 1 回（呼び出し間バッファ
  再利用は `&self`（非 `&mut self`）シグネチャ変更を要するため別途）
- posting の CSR 化・圧縮は引き続き #394 の判断材料
- 専有環境での再実測・層 B `recall.yml` の実測は引き続きオーナー／管理者
  作業

## Issue #394: Phase 1 通し前後比較へのポインタ

本ドキュメントの各節（#387〜#392）はサブ Issue 単位の隣接差分実測である。
Phase 1 全体（`8bfaaa4`〔#388 直前〕→ 導入後）を通した設計判断の集約・
`feature_bench` 13 フェーズの前後比較・`bench-hybrid-profile` 段別の通し
前後比較・外部実装（tantivy・qdrant）参照の整理は
`docs/design/sparse-inverted-index.md`（Issue #394）に記録した。数値の
二重管理を避けるため、本ドキュメントへの転記はしない。

## 最新基線（2026-09-06・Issue #465）

### 目的

`docs/design/crossdb-bench.md` で `hybrid_rrf`（25,000 行・dim 128・wire 経由・
p50）は self 6,178µs・最速の他 DB（sqlite-vec）3,508µs で約 1.8 倍遅いが、
疎索引側の最適化（Issue #388〜#392）後の段別内訳が 1 つの基線として整理されて
いなかった。本節は `bench-hybrid-profile`（engine 内 B0s〜B8）・
`bench-hybrid-wire-profile`（engine/SQL 表層/wire T1p〜T3）の交互複数ラウンド
実測（`docs/design/benchmark-judgement-policy.md` §3〜4 準拠）で最新基線を
記録し、上位区分を Issue #548（Phase 6・上位 2 段の最適化）へ引き継ぐ。

### 計測条件

- commit: `ee99db3`（`origin/main` 分岐元。#552〜#555 適用済み）
- 環境: `QEMU Virtual CPU version 2.5+`・12 vCPU・Avx2Fma・`loadavg` 約 1.2〜2.5
  （`BENCH_DEDICATED_ENV` 未設定・共有環境の参考値。専有環境での再実測は運用者
  判断）
- rounds: `BENCH_HYBRID_PROFILE_ROUNDS=5`・`BENCH_HYBRID_WIRE_ROUNDS=5`（いずれも
  規約下限）を各 2 回実行
- `bench-hybrid-wire-profile` は PR #556（codex-review P1/P2 指摘対応）で各ラウンドが同一のクエリ部分集合（先頭 iterations_per_stage 件）を測定するよう修正済み（修正前はラウンドごとに異なる 50 クエリを計測しており、T1p のラウンド間変動を環境ノイズ帯として使う際にクエリ内容差が混入していた）。本節の数値はこの修正後の実測

### engine 内段別（`bench-hybrid-profile` B0s〜B8。単一テナント・25,000 行・dim 128）

per-round 生データ（1 回目の実測。単位 µs）:

| round | B0s | B0 | B1 | B2 | B3 | B4 | B5 | B8 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 378 | 619 | 8917 | 10872 | 978 | 5605 | 3417 | 124 |
| 2 | 322 | 431 | 9769 | 10884 | 684 | 5777 | 3441 | 125 |
| 3 | 374 | 457 | 9620 | 10773 | 741 | 5629 | 3429 | 124 |
| 4 | 325 | 430 | 9030 | 10746 | 996 | 5677 | 3425 | 125 |
| 5 | 331 | 436 | 9184 | 10885 | 650 | 5604 | 3417 | 124 |

min-of-5／median-of-5（1 回目）: B0s(322/331) B0(430/436) B1(8917/9184)
B2(10746/10872) B3(650/741) B4(5604/5629) B5(3417/3425) B8(124/124)。参照区間帯
（B0s）17.37%。

per-round 生データ（2 回目の実測。単位 µs）:

| round | B0s | B0 | B1 | B2 | B3 | B4 | B5 | B8 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 338 | 506 | 9439 | 11018 | 627 | 5570 | 3449 | 125 |
| 2 | 323 | 442 | 9378 | 10970 | 652 | 5876 | 3458 | 126 |
| 3 | 371 | 449 | 9484 | 11035 | 647 | 5690 | 3478 | 126 |
| 4 | 368 | 443 | 9345 | 11329 | 656 | 5665 | 3428 | 125 |
| 5 | 322 | 538 | 9410 | 10902 | 653 | 5614 | 3424 | 124 |

min-of-5／median-of-5（2 回目）: B0s(322/338) B0(442/449) B1(9345/9410)
B2(10902/11018) B3(627/652) B4(5570/5665) B5(3424/3449) B8(124/125)。参照区間帯
（B0s）14.95%。

帰属表（min-of-5 基準。2 回とも同じ順位。`ratio_of_b1` は表示専用で B1 min に
対する構成比、`band` は
[benchmark-judgement-policy.md](./benchmark-judgement-policy.md) §4 の「before
を分母とする」規約に従い当該区間自身の before/after（`step_ratio_pct`）で
判定する——構成比の表示と band 判定を分離しており、before/after の対を持つ
sql_surface・projection のみ band を評価し、単独区分（dense/sparse/residual
系）は比較元を持たないため band は n/a とする。PR #556 codex-review 指摘対応
（[threadId PRRT_kwDOUAKASM6fqN7s]）で `hybrid_profile_bench.rs` を修正済み）:

| 区分 | 1 回目 diff(ratio_of_b1) | 2 回目 diff(ratio_of_b1) | before→after step_ratio_pct（1 回目 / 2 回目） | band（1 回目 / 2 回目） |
| --- | --- | --- | --- | --- |
| sql_surface(B1−B4) | 3313us(37.15%) | 3774us(40.39%) | 59.12% / 67.77% | above_noise_band / above_noise_band |
| projection(B2−B1) | 1955us(21.93%) | 1557us(16.66%) | 21.93% / 16.66% | above_noise_band / above_noise_band |
| sparse(B5) | 3417us(38.32%) | 3424us(36.64%) | n/a（単独区分） | n/a |
| residual(B4−B0−B5) | 1756us(19.69%) | 1704us(18.23%) | n/a（単独区分） | n/a |
| dense(B0) | 430us(4.82%) | 442us(4.73%) | n/a（単独区分） | n/a |
| visible_set_build(B8) | 124us(1.39%) | 124us(1.33%) | n/a（単独区分） | n/a |
| dense_fast_path_contrast(B3・informational) | 650us(7.29%) | 627us(6.71%) | n/a（単独区分） | n/a |

**上位 2 区分は sql_surface(B1−B4) と sparse(B5) がほぼ同水準（37〜40%）で
並び、残差（18〜20%）が僅差で 3 位**（この順位比較は表示用の `ratio_of_b1`
＝ B1 min に対する構成比であり、band 判定とは別軸）。B8（可視集合
`BTreeSet` 構築）は構成比が小さく（1%台）、B4−B0−B5 の残差の大半は「融合＋
境界同点グループ完全化」（`rrf_fuse` 本体・`complete_boundary_tie_group`）に
帰属すると推定される（下限近似 B7 は本実測では計測しておらず今後の精査対象）。
before/after の対を持つ sql_surface・projection は、それぞれの区間自身の
step_ratio_pct（59〜68%・17〜22%）が参照区間帯（B0s、両回とも 15〜17%）を
明確に上回り above_noise_band となる。単独区分（dense/sparse/residual 系）は
「B1 に対する構成比が参照帯を上回るか」という誤った判定基準を撤去したため
band を n/a とし、`ratio_of_b1` の値は帰属の目安（informational）としてのみ
扱う。

### wire／SQL 表層／engine 内訳（`bench-hybrid-wire-profile` T1p〜T3）

単一テナント・25,000 行・dim 128（engine 側とはコーパスが異なるため絶対値は横比較しない）。

per-round 生データ（1 回目の実測。単位 ms。p95 も併記）:

| round | T1p median | T1p p95 | T2 median | T2 p95 | T3 median | T3 p95 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1.559 | 1.755 | 5.260 | 9.451 | 5.548 | 8.573 |
| 2 | 1.537 | 2.319 | 5.234 | 7.396 | 5.496 | 9.717 |
| 3 | 1.546 | 1.654 | 5.234 | 5.379 | 5.402 | 5.637 |
| 4 | 1.533 | 1.678 | 5.196 | 5.317 | 5.442 | 5.990 |
| 5 | 1.550 | 2.407 | 5.178 | 5.294 | 5.475 | 5.776 |

min-of-5／median-of-5（1 回目）: T1p(1.533/1.546) T2(5.178/5.234)
T3(5.402/5.475)。参照区間帯（T1p の複数ラウンド中央値の振れ）1.73%。

per-round 生データ（2 回目の実測。単位 ms。p95 も併記）:

| round | T1p median | T1p p95 | T2 median | T2 p95 | T3 median | T3 p95 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 2.418 | 2.548 | 5.268 | 6.795 | 5.675 | 9.813 |
| 2 | 1.565 | 2.409 | 5.234 | 5.678 | 5.347 | 5.498 |
| 3 | 2.433 | 2.634 | 5.308 | 9.523 | 5.490 | 6.897 |
| 4 | 1.576 | 2.249 | 5.580 | 9.836 | 5.422 | 5.607 |
| 5 | 1.597 | 1.774 | 5.228 | 6.583 | 5.403 | 6.178 |

min-of-5／median-of-5（2 回目）: T1p(1.565/1.597) T2(5.228/5.268)
T3(5.347/5.422)。参照区間帯（T1p の複数ラウンド中央値の振れ）55.44%——
round1・round3 の T1p が他ラウンドの約 1.5〜1.6 倍（2.4ms 台）へ振れており、
共有環境のスレッドスケジューリング由来のノイズが 1 回目より大きい。

帰属表（min-of-5 基準）:

| 区分 | 1 回目 diff(ratio) | 2 回目 diff(ratio) | 1 回目 band | 2 回目 band |
| --- | --- | --- | --- | --- |
| engine_hybrid(T1p) | 1.533ms(28.37%) | 1.565ms(29.27%) | n/a（基準ゼロ） | n/a（基準ゼロ） |
| sql_surface(T2−T1p) | 3.646ms(67.49%) | 3.663ms(68.49%) | above_noise_band | above_noise_band |
| wire(T3−T2) | 223us(4.14%) | 120us(2.24%) | within_noise_band | within_noise_band |

2 回とも sql_surface(T2−T1p) が支配的（67〜68%）で 1 位、wire(T3−T2) はノイズ帯
内にとどまる点は一致する。2 回目は参照区間帯自体が 55.44% と大きいため、
wire(T3−T2) の band 判定はこの回に限り目安（参照帯が広く within/above の境界の
実質的な判別力が下がる）として扱う。

> **訂正（Issue #637）**: 上記の元表・解釈は fixture を投入した main スレッド
> 上で T2（`sql_surface_hot`）を計測していたハーネス側のアーティファクト
> （次節「Issue #634 追記」参照）の影響を受けている。`773a835` 時点（是正前）
> では main スレッド計測で T2 が T3 を一貫して上回る逆転（約 1ms 規模）が生じ、
> `bucket(wire)` が構造的に n/a（逆転・未確定）になっていた。元表の
> sql_surface diff（3.646/3.663ms）自体は「訂正版帰属表（Issue #637）」の
> ≈3.507ms と比べて約 0.14〜0.16ms 大きい程度にとどまり、wire の
> 「ノイズ帯内」判定は採用しない。なお「Issue #634 追記」節が報告する
> ≈0.7〜1.0ms の縮小は、#634 が別途実施した before（main スレッド計測）/
> after（新規スレッド計測）の 5 ペア再計測どうしの比較であり、元表を
> 過大評価と断定する根拠ではない（両者の関係は「訂正の実体」段落を参照）。
> 訂正後の内訳は「訂正版帰属表（Issue #637）」節を参照。

wire 側の SQL 表層区分（T2−T1p）が最大となるのは、T2 が `execute_sql_in_session`
のクエリパース・束縛・可視行走査（`on_visible_row`）まで含む一方、T1p は
`hybrid_search` のみを直接計測するため——engine 内訳（B1−B4）と同じ「SQL 表層の
固定コスト」を指すが、測定対象範囲が異なるため比率は単純合算できない。

> **注記**: 本表の T2（`sql_surface_hot`）は fixture を投入した main スレッド
> 上での計測値。main スレッド計測では T2 が T3 を一貫して上回る逆転が生じ
> `bucket(wire)` が構造的に n/a（逆転・未確定）になりやすいハーネス側の
> アーティファクトがあった（Issue #634）。再取得値は次節「Issue #634 追記」
> 参照。

### 訂正版帰属表（Issue #637。#634 after arm・5 ペア）

計測条件は「Issue #634 追記」節と同一（after `1900069`・
`BENCH_HYBRID_WIRE_ROUNDS=5`・交互 N=5 ペア・共有 QEMU 環境の参考値。
`docs/design/benchmark-judgement-policy.md` §5）。「Issue #634 追記」節の
per-run 生データ表・after arm の T1p／T2／T3 min-of-5 を 5 ペアぶん単純平均
し、元帰属表と同じ規約（T3 min 平均を分母とする構成比）で再構成した。

- T1p min 平均: (1.460+1.451+1.464+1.466+1.465)/5 ≈ 1.4612ms
- T2 min 平均: (5.057+4.777+5.104+4.697+5.207)/5 ≈ 4.9684ms
- T3 min 平均: (5.202+5.248+5.491+5.274+5.335)/5 ≈ 5.3100ms

| 区分 | diff（5 ペア平均） | 構成比（T3 min 平均比） | band（ペア別） |
| --- | --- | --- | --- |
| engine_hybrid(T1p) | ≈1.461ms | ≈27.5% | n/a（基準ゼロ） |
| sql_surface(T2−T1p) | ≈3.507ms | ≈66.0% | above_noise_band（元表 above_noise_band を維持） |
| wire(T3−T2) | ≈0.342ms | ≈6.4% | 5/5 ペアで正値。3/5 above_noise_band・2/5 within_noise_band（内訳は「Issue #634 追記」per-run 表参照） |

Issue 起票時点の切り分けで使った目安値（engine ≈1.45／SQL 表層 ≈3.3／
wire ≈0.6ms。3 ペア暫定値）は、上記の #634 5 ペア再計測（正値の確定・分母を
明記した平均）で置き換える。wire は目安よりやや小さい ≈0.34ms として確定
する。

順位は元表と変わらず sql_surface が 1 位（≈66%）・engine_hybrid が 2 位
（≈27.5%）・wire が 3 位（≈6.4%）のままである。訂正の実体は「sql_surface を
約 0.14〜0.16ms（元表 3.646/3.663ms 対 訂正版 ≈3.507ms）過大に、wire を過小
（ノイズ帯内／n/a）に記録していた」点にある（「Issue #634 追記」節が報告する
≈0.7〜1.0ms の縮小は、#634 自身の before/after 5 ペア再計測どうしの差分であり、
元表との差分とは別の数値であるため混同しない）。Phase 6（次節）の候補順位
自体は変わらない。

原因（main スレッド計測固有の状態と推定されるが機構は未検証の仮説）の詳細は
「Issue #634 追記」「原因の位置づけ」節を参照。engine 内の段別内訳（B0s〜B8。
`bench-hybrid-profile`）は本節の wire 内訳計測（`bench-hybrid-wire-profile`）
とは別ハーネスであり Issue #634 の影響を受けないため無変更である。

> **適用範囲の注意**: 本節・「wire／SQL 表層／engine 内訳」節はいずれも
> in-process の `bench-hybrid-wire-profile`（`WHERE` 未実行）による計測であり、
> crossdb 経由の `hybrid_rrf` p50 実測値へそのまま外挿できない。crossdb の
> `hybrid_rrf` p50 には `WHERE` 実行後にのみ発現する ScalarIndex
> （Issue #473）由来の状態依存退行（約 10%）が含まれていた期間が
> あり、Phase 1（Issue #632）で `ScalarIndex::build` のゲート是正
> （Issue #638）を対策として実装済みだが、Issue #633 の crossdb fixture
> 実測では退行は本 fixture 上では未解消（no-op）と判定されている。詳細は
> 次節「Phase 6（Issue #548）への引き継ぎ」の但し書き、および
> `docs/design/scalar-index-generation-cache.md`「判定」節・Issue #633 を
> 参照（前後比較実測は Issue #633 の担当のため本 doc へは転記しない）。

### Issue #634 追記: T2 の新規スレッド計測と wire 内訳の再取得

#### 経緯

`hybrid_wire_profile_bench.rs` の T2（`sql_surface_hot`）は fixture を投入した
main スレッド上で計測しており、T3（`wire_roundtrip`。サーバの接続スレッド上で
実行）より一貫して遅い逆転（Issue 起票時の観測値: T2 5.7〜5.8ms 対 T3
5.3〜5.4ms）が生じていた。`bucket(sql_surface)` は約 1ms 過大、
`bucket(wire)` は「逆転・未確定（n/a）」のままだった。原因の機構は未確定
（推定: fixture 投入後の main スレッド固有状態）だが、T2 を
`std::thread::scope` の新規スレッドで計測すると逆転が解消することを観測事実
として確認したため、本 Issue でハーネス側を是正した（`crates/wire-server/
benches/hybrid_wire_profile_bench.rs`。production コード無変更）。

#### 計測条件

- before: `773a835`（`origin/main`）／after: `1900069`（Issue #634 実装コミット）
- 環境: `QEMU Virtual CPU version 2.5+`・12 vCPU・`isa=Avx2Fma`・
  `loadavg` 約 1.2〜3.2（`BENCH_DEDICATED_ENV` 未設定・共有環境の参考値）
- `BENCH_HYBRID_WIRE_ROUNDS=5`・交互 N=5 ペア（before → after を 1 ペア）

#### per-run 生データ（min-of-5／median-of-5。単位 ms）

| pair | arm | loadavg(1min) | T1p min/median | T2 min/median | T3 min/median | reference_band(T1p) | bucket(wire) |
| ---: | --- | ---: | --- | --- | --- | ---: | --- |
| 1 | before | 3.24 | 1.445/1.466 | 5.672/5.730 | 5.314/5.380 | 1.99% | n/a（逆転） |
| 1 | after | 2.82 | 1.460/1.479 | 5.057/5.218 | 5.202/5.256 | 4.20% | diff=145µs ratio=2.79% (within_noise_band) |
| 2 | before | 2.46 | 1.482/1.501 | 5.737/5.754 | 5.221/5.258 | 1.82% | n/a（逆転） |
| 2 | after | 2.15 | 1.451/1.483 | 4.777/5.268 | 5.248/5.334 | 3.50% | diff=471µs ratio=8.97% (above_noise_band) |
| 3 | before | 1.98 | 1.487/1.541 | 5.636/5.714 | 5.369/5.375 | 6.81% | n/a（逆転） |
| 3 | after | 1.75 | 1.464/1.499 | 5.104/5.271 | 5.491/5.591 | 3.74% | diff=387µs ratio=7.05% (above_noise_band) |
| 4 | before | 1.55 | 1.484/1.500 | 5.667/6.026 | 5.257/5.310 | 1.33% | n/a（逆転） |
| 4 | after | 1.51 | 1.466/1.473 | 4.697/4.745 | 5.274/5.481 | 0.79% | diff=577µs ratio=10.94% (above_noise_band) |
| 5 | before | 1.35 | 1.488/1.513 | 5.693/5.724 | 5.365/5.418 | 2.20% | n/a（逆転） |
| 5 | after | 1.22 | 1.465/1.484 | 5.207/5.256 | 5.335/5.367 | 2.16% | diff=128µs ratio=2.40% (within_noise_band) |

before は 5/5 ペアすべてで `bucket(wire)` が「逆転・未確定（n/a）」——Issue
起票時の観測（main スレッド計測で構造的に逆転する）を本環境で再現した。after
は 5/5 ペアすべてで `bucket(wire)` が正の値（128〜577µs・2.40〜10.94%）として
算出され、受け入れ条件 2（`bucket(wire)` が正の値として算出される）を満たす。

#### 帰属表（across 5 pairs の平均。参考値）

| 区分 | before diff(平均) | after diff(平均) | 判定 |
| --- | --- | --- | --- |
| engine_hybrid(T1p) | 約 1.48ms | 約 1.47ms | 目安 engine≈1.45ms に整合 |
| sql_surface(T2−T1p) | 約 4.20ms | 約 3.51ms | 目安 SQL 表層≈3.3ms に近い水準まで縮小 |
| wire(T3−T2) | n/a（5/5 逆転） | 約 341µs | 目安 wire≈0.6ms より小さいが、5/5 で正値化 |

目安（engine ≈1.45／SQL 表層 ≈3.3／wire ≈0.6ms）と完全には一致しないが、
チューニングは行わず実測値をそのまま記録する。sql_surface が before→after で
縮小するのは、T2 を新規スレッドで計測することで main スレッド固有の逆転要因
（未確定）が除かれ、T2 自体の実測値が下がったため（T1p・T3 はほぼ不変）。

#### 原因の位置づけ

観測事実（main スレッド計測での逆転・新規スレッド計測での解消・wire 不変）の
みを根拠とし、機構（fixture 投入後の main スレッド固有状態）は未検証の仮説
として区別する。

#### 申し送り

- 旧値（sql_surface 67〜68%／67.1%）を引用する `docs/design/crossdb-bench.md`
  L598・`docs/design/hybrid-rrf-phase6-before-after.md` L370 と、本節の既存
  表・「Phase 6（Issue #548）への引き継ぎ」の文言の訂正は Issue #637 の担当
  （本 Issue では既存表・帰属表・Phase 6 引き継ぎ本文自体は書き換えない）
- `knn_wire_profile_bench.rs` の T2・`ingest_wire_profile_bench.rs` の S0 も
  同じ main スレッド計測パターンであり、同種アーティファクトの有無は未検証
- 逆転の機構（main スレッド固有状態の推定）の検証・専有環境（
  `BENCH_DEDICATED_ENV=1`・`ROUNDS=10`）再実測はオーナー作業
- 本ベンチ向け交互実行ドライバスクリプト（`scripts/bench_*_ab.sh` 相当）の
  新設は本 Issue のスコープ外

### Phase 6（Issue #548）への引き継ぎ

> Issue #548 は close 済み（#549・#550）。本節は #634／#637 による事後訂正
> であり、以下の候補順位そのものは変わらない。

- 上位候補は (1) SQL 表層固定コスト（`sql/exec.rs` の可視行走査・
  `on_visible_row`）、(2) 疎側再取得ループ（`hybrid.rs::sparse_refetch_loop`。
  BM25 アキュムレータ再利用は Issue #545・#546 が別途対応）——wire 内訳では
  **SQL 表層区分が最大**（「訂正版帰属表（Issue #637）」により sql_surface
  ≈66.0%・engine_hybrid ≈27.5%・wire ≈6.4% で 3 位。旧記述「wire はノイズ帯
  内・未確定」は撤回する）。engine 内訳（B1−B4 の sql_surface と B5 の
  sparse）では 1 回目 sparse 38.32% > sql_surface 37.15%・2 回目 sql_surface
  40.39% > sparse 36.64% と両区間がほぼ同水準で入れ替わっており、本訂正は
  engine 内訳の順位を変更しない。SQL 表層固定コストは engine では
  sparse とほぼ同水準の最大級候補、wire では単独最大の候補として扱う
- 残差（融合＋境界同点グループ完全化。#548 タイトルが先取りする対象）は 3 位
  （19%）にとどまり、B8（可視集合構築）はノイズ帯内。#548 の対象を融合のみに
  限定せず SQL 表層固定コストも候補に含めるべきと申し送る
- **ScalarIndex 由来の状態依存退行についての但し書き**: 本節が根拠とする
  wire 内訳・engine 内訳はいずれも in-process の
  `bench-hybrid-wire-profile`／`bench-hybrid-profile`（`WHERE` 未実行）に
  よる計測であり、crossdb 経由の `hybrid_rrf` p50（`773a835` 時点 7.1ms・
  `ee99db3` 比 +12%）には `WHERE` 実行後にのみ発現する ScalarIndex
  （Issue #473）由来の状態依存退行（約 10%）が含まれていた。Phase 1
  （Issue #632）で `ScalarIndex::build` の平均値長ゲート（Issue #638）を
  対策として実装したが、Issue #633 の crossdb fixture 実測では
  `warm_where_then_hybrid.rss_after_warm` が before/after でほぼ変化しない
  （63.80 → 63.72 MiB。除外が発火していれば約 56MiB まで下がるはず）という
  RSS 実測に基づき、対策は本 fixture に対して no-op であり退行は未解消と
  判定されている（`body` 列平均長が除外閾値をわずかに下回る事実は発火しない
  蓋然性の補助的な状況証拠に留まり、no-op 判断自体の根拠ではない。
  `docs/design/scalar-index-generation-cache.md`「判定」節・Issue #633
  参照）。in-process ベンチ（本 doc の測定経路）には現れないため、
  本節の engine 内段別・wire 内訳とは独立の要因である
- 専有環境（`BENCH_DEDICATED_ENV=1`）での `ROUNDS=10` 再実測、`rrf_fuse_with_limits`
  の下限近似（B7）実測、crossdb self の同一コミット再実行はオーナー／運用者
  作業として申し送る
- `knn_wire_profile_bench.rs` の T2・`ingest_wire_profile_bench.rs` の S0 も
  T2（`sql_surface_hot` 相当）を main スレッド上で計測する同種の構造を持つが、
  本節・「Issue #634 追記」節が扱ったのは `hybrid_wire_profile_bench.rs` の
  T2 のみで、同種アーティファクトの有無はいずれも未検証のまま（「Issue #634
  追記」節「申し送り」から引き継ぎ）

### production コード無変更

`crates/engine/src/`・`crates/wire-server/src/` は無変更。追加した計測ロジックは
`crates/engine/benches/harness/hybrid_profile.rs`（`HybridProjection`・
`sql_hybrid_statement_with_projection`・`bucket_diff`・
`render_baseline_bucket_line`）・`crates/engine/benches/hybrid_profile_bench.rs`
（B0s〜B8 ラウンド計測セクション）・`crates/wire-server/benches/harness/
hybrid_wire.rs`（新設）・`crates/wire-server/benches/hybrid_wire_profile_bench.rs`
（新設）のみ。Issue #637 は docs 専任（`crates/**` 無変更）。

## Issue #546: スコアアキュムレータの再利用（実測は #547）

対応: Issue #546（`perf(engine): score_by_postings のアキュムレータを世代
整合キャッシュ上の再利用バッファへ置換する`）。親 #545。前提: TASK-102、
`docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3。関連: Issue #390
レビュー指摘（「真に N 非依存化するフォローアップ」として本 Issue を予告して
いた記述。上記「Issue #390」節「申し送り」参照）・Issue #357（`SparseIndexCache`
のテーブル世代整合キャッシュ。本 Issue が再利用バッファの寿命源泉とする）。

### 変更内容

`sparse.rs::score_pass`（`search`/`search_within`/`score_within` が共有する
1 パス BM25 スコアリングコア）が呼び出しのたびに新規確保・ゼロ初期化していた
`acc: Vec<f64>`（コーパス全体の doc_idx 空間・長さ N）を、索引
（`SparseIndex`）の寿命内で使い回す有界スクラッチプール（`ScoreScratch`・
`MAX_SCORE_SCRATCH_POOL = 4`）へ置換した。プールは `SparseIndex` 自身が
`Mutex<Vec<ScoreScratch>>` として保持する（`&self` シグネチャは無変更）ため、
`SparseIndexCache`（Issue #357）が保持する `Arc<SparseIndex>` と寿命が一致し、
索引が世代整合を保っている間はプールも使い回され、世代進行で索引が失効・
再構築されればプールごと破棄される。

- `score_pass` の戻り値をリース型 `ScorePass<'a>`（`Drop` でプールへ返却）へ
  変更し、`score_by_postings`・`score_within` は借用アクセサ（`acc()`/
  `touched()`）経由で読み取る
- 返却時（`SparseIndex::release_scratch`）は `touched` に記録された `doc_idx`
  のみをゼロ戻しする。「`acc[idx] == 0.0` ⇔ 未加算」という既存の不変条件
  （Issue #390 設計判断）により、`touched` に載っていない要素は既に `0.0`
  であるため、全要素走査によるゼロクリアと結果は同一であり、演算式・加算順は
  一切変更していない（スコアの f64 ビット一致は不変）
- プール取り出し時（`acquire_scratch`）は `acc` の長さが索引の文書数と一致
  することを検証し、不一致（通常到達しない）は破棄して新規確保へ fail-closed
  に倒す
- `approx_heap_bytes()` へプールの**決定的な上限値**（`MAX_SCORE_SCRATCH_POOL`
  本ぶんの最悪確保量。実際のプール充填率に依存しない）を加算し、
  `SparseIndexCache::insert`（Issue #357）の容量判定
  （`MAX_SPARSE_CACHE_TOTAL_BYTES` = 1 GiB）が実確保量を下回らないようにした

### 設計判断（親 #545 の文言との関係）

親 Issue #545 の文言は「可視集合サイズで確保」を示唆するが、この方式は
クエリごとに `doc_idx → compact` 写像を可視集合サイズ分構築する必要があり、
本 Issue（#546）のスコープでは「N 長バッファを索引の寿命内で再利用し、
`touched` した要素のみゼロ戻しする」方式を採った。この方式でも初回のみ
`O(N)`・以降は `O(M)`（M = ヒット文書数 ≤ 可視集合サイズ）となるため、
親 Issue の狙い（確保コスト除去・ビット一致維持）は包含される。

配置についても、`SparseIndexCache` 自体（`sql/sparse_cache.rs`）ではなく
`SparseIndex` 内部にプールを持たせる方式を採った。キャッシュが保持する実体は
`Arc<SparseIndex>` であり、`hybrid.rs`・`sql/exec.rs`・Rust API 経路はいずれも
`&SparseIndex`／`&self` で接続されているため、`SparseIndex` 内部に置くことで
「世代整合キャッシュ上の再利用バッファ」という要件をシグネチャ変更なしに
満たせる（詳細は `sql/sparse_cache.rs` モジュール doc「スコアスクラッチ
プールとの関係」節参照）。

### 検証

`crates/engine/tests/sparse_cache_recall.rs`・`crates/engine/tests/
sparse_determinism.rs` は無変更のまま green（Recall 層 A 固定値アサーション・
cold/hot 等価性・決定性契約は構造的に不変）。`sparse.rs` 内 unit test に
交互呼び出しでのビット一致・プール返却時の全ゼロ／空検証・並行アクセス時の
ビット一致・長さ不一致バッファの fail-closed 破棄・`approx_heap_bytes` の
上限加算を固定するテストを追加した。

### 申し送り

前後比較実測（N=25k／100k・可視率 100%／10%・交互 min-of-N・ノイズ帯併記）と
採否記録は Issue #547 の担当とし、本 PR では数値を記録しない。
`approx_heap_bytes()` の表示値は上限加算分だけ増える（実 RSS は変わらない）
ため、#547 が `bench-hybrid-profile` のメモリ計測を記録する際にはこの点を
注記する必要がある。

## Issue #547: #546 の前後比較（実測）

対応: Issue #547（`test(engine): bench-hybrid-profile での前後比較
（N=25k／100k・可視率 100%／10%）`）。前提: Issue #546（本ドキュメント
「Issue #546」節）。計測規約は `docs/design/benchmark-judgement-policy.md`
（Issue #462 SSOT）に従う。

### 計測方法

`crates/engine/benches/harness/hybrid_profile.rs` へ
`BENCH_HYBRID_PROFILE_ROWS`（コーパス行数）・
`BENCH_HYBRID_PROFILE_VISIBLE_RATIO`（可視率 `1/<N>`）の fail-closed
opt-in を追加し、`scripts/bench_hybrid_profile_ab.sh`
（`make bench-hybrid-profile-ab`）で before（#546 適用前。
`crates/engine/src/sparse.rs`・`crates/engine/src/sql/sparse_cache.rs` のみ
`b161d5b^`（#565 の直前コミット）まで戻したビルド）／after（HEAD。
本 PR の計測基盤自体を含む）の 2 バイナリを N∈{25000,100000} ×
可視率∈{1/1,1/10} の 4 条件で before→after 交互 5 ペア（各ペア
`BENCH_HYBRID_PROFILE_ROUNDS=5`）実行した。可視率の意味は README「hybrid_rrf
段別内訳プロファイルと転置索引化の前後比較」節と同じ（SQL 段は索引 N
自体が縮小、直接 API 段は索引 N は常に行数で可視集合のみ縮小）。

review 指摘対応として、`scripts/bench_hybrid_profile_ab.sh` へ
`BEFORE_COMMIT`／`AFTER_COMMIT`（ビルド元コミット hash の記録を必須化）・
`AB_PAIRS >= 5` の実行前検証（`docs/design/benchmark-judgement-policy.md`
§3 の下限）・`AB_ROUNDS` の `hybrid_profile_bench` 自身の受理範囲
`5..=50` との事前整合検証を追加し、`--summarize` の出力を条件→ペア→
before/after の実行順・ファイル名付き（`grep -H`）へ変更した。あわせて
`hybrid_profile_bench.rs` の B7 参考値（密候補が可視部分集合を無視して
`corpus.ids`／`corpus.vectors` の全件から Top-`pool_depth` を拾っていた
不整合）と、B1/B4 fidelity 検証（可視件数が `TOP_K` 未満の設定で常に
fail-closed していた不整合）を修正した。以下の実測値はこれらの修正を
適用した版で再実行した結果であり、本節はこの再実行値のみを記録する
（旧実測値は本コミットで置き換え）。

- before ビルド元: `af885d6e59a56ecb481e646ce4418bc845e0dad7`
  （`crates/engine/src/sparse.rs`・`crates/engine/src/sql/sparse_cache.rs`
  のみ `91f6a1830eed7bed326a552bfc4625296a8bf371` = `b161d5b^` へ差し替え）
- after ビルド元: `af885d6e59a56ecb481e646ce4418bc845e0dad7`
  （production コード〔`sparse.rs`・`sql/sparse_cache.rs`〕は #546 適用後の
  ままで不変。今回の修正はいずれもベンチハーネス・計測ドライバのみ）

### 実測結果（min-of-5・median-of-5 併記、参照区間帯は同一条件内の B0s の (max−min)/min）

`B4`（`hybrid_search_cached_index`。engine 内 hybrid 経路の直接 API。
`score_by_postings`／`score_within` を経由し #546 の対象）・`B5`
（`sparse_refetch_loop`。同じく対象）を主対象とし、`B0s`
（`CpuScalarProvider` 単線・#546 と無関係の密側参照区間）を同一実行環境の
ノイズ帯として併記する。値は各ペアの 5 ラウンドから取った min を 5 ペア
ぶん集め、その min・median を示す（`docs/design/benchmark-judgement-policy.md`
§3 の「min-of-N と median の両方を必ず併記する」に対応）。

| 条件（N・可視率） | B4 before (min/median) | B4 after (min/median) | B4 diff (min基準) | B5 before (min/median) | B5 after (min/median) | B5 diff (min基準) | B0s 参照区間帯 (before/after) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 25,000・1/1 | 3030us / 3041us | 3011us / 3076us | −0.6% | 1673us / 1674us | 1676us / 1682us | +0.2% | 10.1% / 9.0% |
| 25,000・1/10 | 392us / 399us | 394us / 407us | +0.5% | 148us / 149us | 150us / 151us | +1.4% | 1.5% / 1.5% |
| 100,000・1/1 | 9074us / 9296us | 8924us / 9279us | −1.7% | 5784us / 5807us | 5831us / 5855us | +0.8% | 12.5% / 5.8% |
| 100,000・1/10 | 1332us / 1346us | 1319us / 1340us | −1.0% | 690us / 697us | 682us / 684us | −1.2% | 3.2% / 1.3% |

per-run 生データ（各条件・各ペア・各 side の 5 ラウンド min の値列。
5 ペアぶん）を以下に残す（`docs/design/benchmark-judgement-policy.md` §3
「per-run 生データの記録を必須とする」に対応。単位 us、ペア 1〜5 の順）:

| 条件・side | B4 各ペア min の値列 | B5 各ペア min の値列 |
| --- | --- | --- |
| 25,000・1/1・before | 3030, 3039, 3116, 3120, 3041 | 1674, 1673, 1674, 1677, 1680 |
| 25,000・1/1・after | 3011, 3205, 3092, 3076, 3066 | 1679, 1687, 1712, 1676, 1682 |
| 25,000・1/10・before | 399, 397, 417, 421, 392 | 150, 150, 148, 149, 149 |
| 25,000・1/10・after | 397, 407, 425, 413, 394 | 150, 153, 154, 151, 151 |
| 100,000・1/1・before | 9979, 9296, 9074, 9532, 9075 | 6028, 5784, 5803, 5807, 5820 |
| 100,000・1/1・after | 9279, 9070, 9739, 8924, 9666 | 5831, 5855, 5883, 5845, 5875 |
| 100,000・1/10・before | 1332, 1375, 1404, 1338, 1346 | 694, 701, 700, 697, 690 |
| 100,000・1/10・after | 1351, 1343, 1330, 1340, 1319 | 682, 683, 692, 689, 684 |

`B1`（SQL 表層 crossdb 規範形）・`B2`（既存段と同じ投影）でも同様に
before/after 差は概ね ±2〜3% 台で、絶対値に対する `score_by_postings` の
寄与自体が小さいため、傾向は B4/B5 と一致する。生ログ・summary 行は
`target/bench-hybrid-profile-ab/<ts>/*.log`（実行時生成物・本リポの
履歴には含まない）に保存されるが、上表の min／median／per-run 値は本
ドキュメントに恒久的に記録する。

**B0s 参照区間帯（前表 `B0s 参照区間帯 (before/after)` 列）の算出元について**:
上表の B4/B5 とは異なり、`B0s`（`CpuScalarProvider` 単線の参照区間）自体の
各ペア min 値列は計測実行時のログ（`target/bench-hybrid-profile-ab/<ts>/
*.log`）からこのドキュメントへ転記されなかった。生ログはベンチ実行時生成物
であり本リポの履歴に含まれないため、本コミット時点で遡って値を復元する
手段がない。したがって上表の `B0s` 参照区間帯（1.3%〜12.5%）はこのドキュメント
単独では第三者が再計算できない値であり、その旨をここに明記する
（codex-review 指摘。`docs/design/benchmark-judgement-policy.md` §4）。

`docs/design/benchmark-judgement-policy.md` §4 は「実測ノイズ帯」を
「同一計測セッションで得た参照区間の run-to-run 幅」と定義し、第三者が
その値列を検証できることを前提にしている。上記のとおり `B0s` の値列は
本コミット時点で第三者が検証できないため、`B0s` 参照区間帯は同 §4 が
定める実測ノイズ帯の要件を満たさない。したがって下記「判断」節では
`B0s` 参照区間帯を判定根拠として採用せず、§4 が要求するもう一方の
ノイズ帯である**固定相対帯（±5%。`classify_change` 基準）のみ**を根拠に
判定する（review 指摘対応。検証不能な実測帯に基づく判断を撤回し、保存
済みデータ〔上表・per-run 表に恒久記録した B4/B5 の min／median／各ペア
値列〕と固定 ±5% 帯から言える範囲へ結論を限定する）。

再測定によるギャップ解消はこの PR のスコープ外とする。理由は次の通り: (1)
B0s は B4/B5 と**同一の交互実行**から得られる値のため、B0s だけを単独で
再測定しても既存の B4/B5 per-run 値との対応関係が崩れる。整合させるには
4 条件 × 2 side × 5 ペアの全体（B4/B5/B0s すべて）を再実行し、上表・
per-run 表を丸ごと置き換える必要があり、単一の P2 指摘（生データ記録
漏れ）の範囲を超える。(2) 本リポの計測規約
（`docs/design/benchmark-judgement-policy.md`）は専有環境（`BENCH_DEDICATED_ENV`）
での計測を前提とするが、この修正作業自体が複数エージェント並列実行中の
共有環境で行われており、ここで再測定してもノイズ帯の値として信頼できない。
今後の再測定では `scripts/bench_hybrid_profile_ab.sh --summarize` が
`baseline_round_raw`／`baseline_summary`（`B0s(min=...,median=...)` を含む）
に加え、各 run 直前の `loadavg_before_run`・`running_processes_excluding_self`・
`top_cpu_processes`（同 §3 の同時実行プロセス有無の記録。本 PR で追加）
の行をそのまま列挙するため、次回実行時はこれらの行を本ドキュメントへ転記
すれば同種の指摘は再発しない。上記のとおり本節の判断は `B0s` の値列
無しでも固定 ±5% 帯のみで成立するため、このギャップは判断そのものの
妥当性には影響しない。

**実測環境の記録（`docs/design/benchmark-judgement-policy.md` §3）**:
本 4 条件の計測は本開発コンテナ（`lscpu` Model name: `QEMU Virtual CPU
version 2.5+`・`nproc`=12・命令セットフラグに `avx2`／`fma`／`f16c` あり・
`avx512*`／`neon` 無し。`BENCH_DEDICATED_ENV` 未設定の共有環境）で実行した。
この CPU/ISA 情報は本ドキュメント作成時点でも同一環境で確認できる恒久的
特性として記録するが、実測実行時点ごとの `loadavg`・同時実行プロセスの
有無は前述の生ログ（本リポ履歴に含まれない実行時生成物）にのみ記録されて
おり本コミット時点で復元できない。§3 の環境記録要件のうち静的な CPU/ISA
情報は本節で充足し、動的な `loadavg`・同時実行プロセス有無は充足できない
ことをここに明記する。

### 判断

4 条件 × 2 段（B4・B5）のいずれも、before→after の差（−1.7%〜+1.4%）は
`docs/design/benchmark-judgement-policy.md` §4 の固定相対帯（±5%）を
超えない（`classify_change` 基準で `Neutral` 分類）。同 §4 は判定に
効かせる差分を「固定相対帯・実測ノイズ帯の両方を超えていること」と
定めており、固定相対帯を超えない時点で（`B0s` 参照区間帯の検証可否に
よらず）採用条件（ノイズ帯を明確に超える改善）を満たさないと判定できる。
したがって **#546 のウォールクロック効果はこの計測環境・この 4 条件では
有意に検出できない**と判断する（Issue #366・#365 と同様の「効果なし」
実測結果。ハーネス・ドライバの review 指摘修正後に再実測した値でも結論は
変わらない。本判断は固定相対帯のみで成立し、算出元データを提示できない
`B0s` 実測ノイズ帯には依拠しない）。

推定される要因: `score_by_postings` が確保していた `acc: Vec<f64>`
（コーパス全体長）は glibc の同一サイズ再利用（tcache）により繰り返し
確保・解放してもコストが小さく、かつ本ベンチの支配的コストは posting
list 走査・BM25 スコアリング本体（Issue #388〜#392 で既に大半を削減済み）
や SQL 表層の固定コストであるため、確保コストの除去がマイクロベンチの
全体レイテンシに現れにくい。

`#546` はスコアの f64 ビット一致・RLS 相当のテナント境界縮約契約を維持した
まま `approx_heap_bytes()` の容量判定を実確保量に整合させる副次効果を持つため
（本ドキュメント「Issue #546」節参照）、ウォールクロック改善が計測されない
ことは production 変更の妥当性を損なわない。本 Issue の役割は前後比較の
実測・記録であり、#546 の採否判断そのものは対象外（既に実装・マージ済み）。

## Issue #549: RRF 融合段の id 写像・ソートの割り当て削減（融合結果はビット同一）

対応: Issue #549（`perf(engine): RRF 融合段の id 写像・ソートの割り当て削減
（融合結果はビット同一）`）。親 #548（Phase 6・hybrid 上位 2 段の最適化）→ #461
→ ルート #455。前提: Issue #465（最新基線）・Issue #546（`score_by_postings`
アキュムレータ再利用）。対象ビヘイビア: SEARCH-1・SEARCH-3。関連ポインタ:
TASK-104・TASK-84。

### 変更内容

`hybrid.rs::rrf_fuse_with_limits` の融合コアを、id をキーにした
`BTreeMap<u64, f64>`（`entry().or_insert(0.0)` による毎クエリのノード確保を
伴う累積）から、検証済み長さの**位置索引方式**へ置換した。

- `compute_contributions`（旧 `accumulate_ranked` を改称・再設計）が、密・疎
  それぞれの寄与（`weight / (k_const + rank)`）を「id ではなく列内の位置」に
  対して `contrib: Vec<f64>`（長さ `n_d + n_s`。dense は `[0..n_d)`、sparse は
  `[n_d..)`）へ書き込む。密・疎の位置は重ならないため、加算ではなく単純代入で
  足りる（各位置は必ず 1 回だけ書き込まれる）
- `index: Vec<(u64, usize)>`（`(id, pos)` の全順序タプル）を id 昇順へ**比較
  関数なし**の `sort_unstable()` で整列する（`(id, pos)` は要素ごとに一意の
  ため不安定性は観測されない。id を直接添字にする表は作らない ── id は
  呼び出し元定義の任意 `u64` であり、添字化は untrusted 入力に比例した無制限
  確保になるため）
- 整列済み `index` の等 id 連続区間ごとに `contrib` から寄与を合算し、
  `merged: Vec<HybridHit>`（id 昇順）を構築する。演算順（各位置の寄与を求めて
  から加算する順序）は旧 `BTreeMap` 版の `or_insert(0.0)` → `+= dense 寄与` →
  `+= sparse 寄与` と完全に同一であり、スコアはビット同一になる
- 最終スコアソート（`out.sort_by(|a, b| b.score.total_cmp(&a.score)
  .then(a.id.cmp(&b.id)))`）は安定ソートのまま**維持**（`docs/design/
  rrf-tie-break-determinism.md` の不変条件）。この比較器は id が一意である限り
  同値要素を生まない全順序のため、`merged` を渡す前の走査順序（本実装では id
  昇順）自体は出力に影響しない
- `has_duplicate_id`（`validate_extended_pool` からも使用）を、`BTreeSet` への
  全件挿入（要素追加が B-tree ノードの新規確保・分割を伴いうる）から、`Vec` へ収集して比較関数
  なし `sort_unstable()` の後に隣接比較する版へ置換（bool の戻り値契約・
  呼び出し位置は不変）
- `apply_soft_boost` の末尾の再ソートを、`hits` が既に融合スコア降順・同点 id
  昇順へ整列済み（production 経路の `rules` 空呼び出しでは常にこの状態）なら
  省略するガード（`is_sorted_desc_id_asc` による判定。安定ソートの入力が既に
  整列済みなら再ソートは恒等写像であり省略は観測不能）を追加した。判定は
  `rules.is_empty()` ではなく実際の整列状態で行うため、未整列入力＋空 `rules`
  という契約違反ケースの挙動（従来どおり整列される）は変えない

### 等価性検証

置換前の融合コア（`has_duplicate_id` ×2 → `BTreeMap` 累積 → 有限性 → `collect`
→ `sort_by`）を `#[cfg(test)] fn rrf_fuse_reference_with_limits`（内部で
`accumulate_ranked_reference` を使用）として逐語コピーで残置し（Issue #399
の先例に倣う）、`hybrid.rs::tests` に以下を追加した。

- `rrf_fuse_with_limits_matches_reference_bitwise`: 決定的擬似乱数
  （xorshift64*。外部クレート不使用）で 400 試行を生成し、`TieRank::GroupEnd`/
  `Positional`、`k_const`・重みの通常値と極端値（同点グループを潰す巨大
  `k_const`、オーバーフローを誘発しうる巨大重み）、密・疎間の id 部分/完全
  重複、片側空、`dense_limit != sparse_limit`（`TooManyCandidates` の一致も
  含む）を横断し、`Ok` 側は id・スコアの `to_bits()` 全件一致、`Err` 側は
  エラー variant 一致を検証する
- `rrf_fuse_with_limits_matches_reference_bitwise_on_full_id_overlap`: 密・疎が
  完全に同一の id 集合を持つ（全件が両チャネルへ寄与を加算する）退行の専用
  固定
- `rrf_fuse_priority_duplicate_id_over_post_fusion_non_finite_score`: 重複 id と
  融合後 `+Inf` を同時に含む入力で `DuplicateId` が返ること（検証順序:
  長さ → 有限性(入力) → ソート順 → 重複 → 融合後有限性）を置換後の実装でも固定
- `apply_soft_boost_skips_resort_when_hits_already_sorted_and_rules_empty` /
  `apply_soft_boost_still_sorts_unsorted_input_with_empty_rules`: 3.3 の省略が
  「整列済みなら省略」であって「`rules` が空なら省略」ではないことを固定

既存の `tests/hybrid_recall.rs` 層 A 固定値アサーション・`tests/
sparse_determinism.rs`・`tests/hybrid.rs`・`tests/sql_surface.rs` 等は無変更の
まま green（同点順位規約 `TieRank::GroupEnd`・境界同点グループ完全化（#310）・
再取得スケジュール（#392）は不変）。

### 確保回数削減の根拠（静的）

`#[global_allocator]` によるアロケーションカウントは本リポの既存方針
（`storage.rs` の判断: 並列テスト下で非決定的・依存追加回避）に従い採用しない。
変更前後の確保箇所を列挙する。

| 箇所 | 変更前 | 変更後 |
| --- | --- | --- |
| 融合コア | `BTreeSet`×2（重複検査）＋ `BTreeMap`（累積。挿入に応じた B-tree ノード確保・分割）＋ `collect` の `Vec`＋ソートのスクラッチ | `has_duplicate_id` の `Vec`×2 ＋ `contrib: Vec<f64>` ＋ `index: Vec<(u64,usize)>` ＋ `merged: Vec<HybridHit>` ＋ソートのスクラッチ（いずれも単一 `Vec`・事前確保サイズ既知） |
| `validate_extended_pool`（境界同点グループ完全化の再取得ラウンドごと） | `BTreeSet`×2 | `Vec<u64>`×2（`has_duplicate_id` 経由） |
| `apply_soft_boost`（production の空 `rules` 呼び出し） | 常に `sort_by` のスクラッチ確保 | 既整列時は確保 0 |

`BTreeMap`/`BTreeSet` は 1 ノードに複数要素を格納するため確保回数は要素数と
一致しない（要素ごとに個別ヒープ確保されるわけではない）。ただし挿入に伴う
ノードの新規確保・分割・再配置は要素数に対して非ゼロかつ事前に見積もれない
回数発生し、確保サイズも実行時の木の形状に依存する。これに対し置換後は
要素数が確定した単一 `Vec` の確保に集約される（`Vec` 自体も 1 回の連続領域
確保で済む）。

### 参考値（単一バイナリ内 A/B・B7 下限近似。採否記録は #550／#547 の担当）

`docs/design/benchmark-judgement-policy.md` §5 により、共有 QEMU 環境では
perf 動機の production 変更を本 Issue の実装担当が「Accepted」と判定できない
（#546 の先例と同じ位置づけ）。本節は参考値の記録に限る。

`crates/engine/benches/hybrid_profile_bench.rs` へ B7 段
（`fuse_lower_bound`）を追加した。密・疎それぞれの Top-`pool_depth`
候補（密は `ParallelSearchProvider`、疎は `sparse_refetch_observed(...).0` を
`pool_depth` 件へ切り詰めたもの）を計測外（ラウンドループの前）で事前に捕捉
し、`hybrid::rrf_fuse` の呼び出しのみを計測する（境界同点グループ完全化の
再取得コストを含まない「融合コアだけの処理時間」の下限近似）。

`BENCH_HYBRID_PROFILE_ROUNDS=5`・開発環境（共有 QEMU 環境。専有環境
`BENCH_DEDICATED_ENV=1` ではない）での 1 回実測（`make bench-hybrid-profile`
相当）:

```text
B7(min=10us,median=10us)
```

B1（SQL 表層 hybrid・`SELECT id`。min 6,691us）に対する比は約 0.15%、B4-B0-B5
残差（min 974us。B4=3,157us・B0=505us・B5=1,677us）に対しては約 1%
（`10 / 974 ≈ 1.03%`）である。ただしこの値は**変更後実装**への下限近似
（`pool_depth` へ切り詰めた候補を渡した `rrf_fuse` 単体の計測）に限られ、
変更前実装（`BTreeSet`/`BTreeMap` 経由）の融合コア時間・本番の境界同点
グループ完全化後の候補数（`pool_depth` 切り詰めなし）での融合時間のいずれも
計測していない。したがってこの参考値だけから「融合コアは元から残差のごく
一部」「削減の絶対効果が小さい」とは判断できない。結論は今回の入力・変更後
実装に対する参考値に限定し、削減効果（変更前後比較）は `docs/design/
benchmark-judgement-policy.md` の基準を満たす前後比較（同一バイナリの
production 変更前後を交互計測）を経るまで未確定とする。前後比較・採否の確定は
親 #548 傘下の #550（通し前後比較）・#547 の担当とする。

### スコープ外・申し送り

- `hybrid_search_boosted` の `visible_ids: BTreeSet<u64>` のソート済み `Vec`
  化（B8・約 1%。`sparse.rs::score_within` の `&BTreeSet` シグネチャ・
  `bench-internals` フック・`hybrid_profile` ハーネスへ波及するため別途）
- 最終スコアソートの `sort_unstable_by` 化（全順序のため結果は同一だが
  `docs/design/rrf-tie-break-determinism.md` の安定ソート不変条件に関わる
  オーナー判断事項）
- クエリ横断の融合スクラッチ再利用（#546 型のプール化。規模が小さく費用対
  効果が薄いため見送り）
- perf 採否の確定（#550 通し前後比較・#547）・専有環境
  （`BENCH_DEDICATED_ENV=1`）再実測はオーナー／運用者作業

## Issue #550: Phase 6 通し前後比較へのポインタ

Issue #546・#549 を通しで前後比較した `feature_bench`・`bench-hybrid-profile`・
crossdb self・Recall 3 ゲートの結果は `docs/design/
hybrid-rrf-phase6-before-after.md` に記録した（数値は同 doc 参照。本節では
転記しない）。production コード無変更・doc 専任。

## Issue #660: SQL 表層固定コスト（B1-B4）の S0〜S8 再分解

親 Issue #652・ルート #649。Issue #465 の帰属表が持つ `sql_surface(B1-B4)`
区分（B1: SQL hybrid `SELECT id`、B4: `hybrid_search` 直接呼び出し）を、より
細かい構成要素へ再分解した。

### 背景

crossdb 横断ベンチ（`docs/design/crossdb-bench.md`）では `hybrid_rrf` が self
（pgvector 比で劣後）の最大要因の一つであり、`bench-hybrid-profile`（B0s〜B8）
の帰属表では `sql_surface(B1-B4)` が全体の 37〜40% と、疎側 `sparse(B5)` と
ほぼ同水準の最大区分だった。しかし従来のベンチは `sql_surface` を単一区分
としてしか持たず、内部（パース・束縛・各キャッシュ照会・SCALAR 段の行ループ・
投影組み立て）のどこが支配的かは未確定だった。

コード調査で判明した事実: `sql/exec.rs` の hybrid `Ranking::Hybrid` 分岐は
`cache_fast_path_eligible = filters_empty && !is_hybrid`（hybrid は常に対象外）
のため、疎索引キャッシュ（`SparseIndexCache`。Issue #357）がヒットしていても
可視全行について `VectorArena::build_from_cached_rls_rows` の行ループ
（`push_visible_row` の id・vector・tenant_id 文字列複製、`on_visible_row` の
`row_codec::scan_scalar_columns` 構造検証、投影列の積み上げ）を再実行する。

### 追加した段（`hybrid_profile_bench.rs`・`harness/hybrid_profile.rs`）

| 段 | 内容 | 性質 |
| --- | --- | --- |
| S0 `dense_topk_ref` | `ParallelSearchProvider.search(k=TOP_K)`（B3 の減算対象。B0 は k=dense_fetch_k のため k を揃えた別対照が必要） | 実 API |
| S1 `parse` | `validate_sql`（双子 DB。同一スキーマ・0 行の別 `Storage`） | 実 API |
| S2 `schema` | `Storage::get_table_schema`（双子 DB） | 実 API |
| S3 `bind` | `bind_in_session`（計測区間外で構築済み `ValidatedStatement` を使い、束縛のみ計る） | 実 API |
| S1d `parse_dense` | S1 と同じ双子 DB・round-robin 規則で、文だけを dense SQL（B3 と同一文）へ差し替えたパース | 実 API |
| S3d `bind_dense` | S3 と同じ規則で dense SQL（B3 と同一文）の束縛を計る | 実 API |
| S4 `scan_replica` | `on_visible_row` の構造検証部（`row_codec::scan_scalar_columns` を可視全行分）の複製 | 複製近似 |
| S5 `rowcopy_replica` | `push_visible_row`（id・vector・tenant_id 文字列複製・可視性フラグ）＋投影スロット積み上げの複製 | 複製近似（下限） |
| S6 `slotmap_replica` | `slot_ids`／`visible_id_counts`（`HashMap`）の毎クエリ O(N) 再構築の複製 | 複製近似 |
| S7 `bodyclone_replica` | `SELECT *` 投影が行う本文列複製（既存 `projection(B2-B1)` の実体）の複製 | 複製近似 |
| S8 `tail` | `RlsSafetyNet::apply`（Top-k のみ）＋ id 投影（`project_id_only_rows`。後述「S8 の id 投影追記」参照） | 実 API |

S1〜S3・S1d・S3d は行数に依存しないパース・束縛コストを、可視全行を持つ計測用
DB ではなく同一スキーマ・0 行の「双子 DB」で計測する。move 前に実 DB で 1 回
だけ束縛して得た `BoundStatement`（`PartialEq` 導出済み）と、双子 DB での束縛
結果が構造的に一致することを fail-closed に検証してから使う。

**S1d・S3d を追加した理由（codex-review 指摘・スレッド
`PRRT_kwDOUAKASM6gV23s`・Issue #660 追記）**: `common_fixed(B3-S0)` は dense
SQL（`sql_dense_statement_with_projection` で組み立てた B3 と同一文）の
round trip から dense-topk 対照区間（S0）を引いた値であり、内訳は dense SQL
自身のパース・束縛を含む。当初実装はここから引く `unexplained_common` の
減算対象に S1・S3（hybrid SQL のパース・束縛）を流用していたため、hybrid
固有の構文（`hybrid_rrf(...)`）・列解決コストの分だけ `unexplained_common`
（本来は arena／sparse／scalar キャッシュ照会等の未計測 `pub(crate)` 内部
処理の上限であるべき区分）へ混入していた。S1d・S3d（dense SQL・B3 と同一文
でのパース・束縛）を新設して `unexplained_common` の減算対象を dense 側へ
揃え、hybrid と dense のパース・束縛差は独立区分 `parse_bind_hybrid_delta`
として切り出す（後述）。

**S8 の id 投影追記（codex-review P2 指摘・PR #668・スレッド
`PRRT_kwDOUAKASM6gWEdt`）**: 当初実装の S8 は `RlsSafetyNet::apply` の後に
`verified.hits().len()` を返すだけで、production `project_rows`（`sql/exec.rs`）
が行うスロット番号→行 `id` の解決・`ResultRow`／`Cell` の構築が計測対象に
含まれておらず、そのコストが `unexplained_common` の残差へ混入していた。
`sql/exec.rs` に非既定 feature `bench-internals` 限定の
`project_id_only_rows`（`project_rows` の `ProjectedColumn::Id` 分岐を単体で
再現する関数。スカラー列投影・`Computed` 列は本ベンチの SQL が `SELECT id`
固定のため対象外）を追加し、S8 が `RlsSafetyNet::apply` に加えてこの投影
処理まで測定するよう変更した。`RlsSafetyNet::apply` の `label_of` 閉包も、
定数を返すだけの簡略版から `issue660_s8_arena`（`storage` から一度だけ
構築した実 `VectorArena`）の `tenant_id`／`visibility` を引く production 相当の
閉包へ差し替えた。`hybrid_search` が返す hit の `id` は実際の行 `id`（アリーナの
スロット番号ではない）であるため、計測区間の外で 1 回だけ id→スロット写像を
作り、hits をスロット番号へ変換してから測定ラウンドへ渡している。

### 複製近似の限界

S4〜S7 は `on_visible_row`／`push_visible_row` の該当ロジックを手動転記した
近似であり、production 経路が追加で行う予算検証（`try_accumulate_budget`）・
`debug_assert!`・`Vec` の成長パターン差（`try_reserve_exact` と `Vec::push` の
償却確保の違い）は複製しない。そのため実測値は production コストの**下限**
として読み、production との差は `unexplained_hybrid`（後述）として上限
（ceiling）のみ報告する。

### 差分区分（min-of-R 基準・飽和差分）

| 区分 | 式 | 意味 |
| --- | --- | --- |
| `common_fixed` | `B3 - S0` | 全 SELECT 共通の SQL 表層固定コスト（パース・束縛・スキーマ・arena 照会・投影組み立て等。dense SQL 基準） |
| `hybrid_only` | `(B1 - B4) - common_fixed` | hybrid 限定の上乗せ（SCALAR 段の全行再構築＋疎／scalar キャッシュ照会＋hybrid 固有のパース・束縛差） |
| `parse_bind` | `S1 + S2 + S3` | hybrid SQL のパース・束縛・スキーマ取得（informational。`unexplained_common` の減算には使わない） |
| `parse_bind_dense` | `S1d + S2 + S3d` | dense SQL（B3 と同一文）のパース・束縛・スキーマ取得。`unexplained_common` の減算対象 |
| `parse_bind_hybrid_delta` | `parse_bind - parse_bind_dense` | hybrid SQL が dense SQL 比で余分に負う構文・列解決コスト（`hybrid_only` の内訳の一部として読む informational 区分。`hybrid_only` の式自体は変更しない） |
| `scalar_stage_replica` | `S4 + S5` | 全行再構築コストの複製下限（`hybrid_only` の上限と対にして挟む） |
| `unexplained_common` | `common_fixed - (S1d+S2+S3d+S6+S8)` | arena／sparse／scalar キャッシュ照会・`begin_read`・結果組み立て等、直接計測できない `pub(crate)` 内部処理の合計（上限。dense SQL のパース・束縛〔S1d・S3d〕は実測済みのため差し引く） |
| `unexplained_hybrid` | `hybrid_only - scalar_stage_replica` | 疎／scalar キャッシュ照会・予算検証・hybrid 固有パース束縛差等、複製で表せない残差（上限。`parse_bind_hybrid_delta` を含む） |
| `projection_replica(S7)` | `S7` そのもの | 既存 `projection(B2-B1)` の実体（本文列複製）の複製近似 |

**解釈規則**: 行ループ（SCALAR 段の全行再構築）のコストは `hybrid_only` を
上限、`scalar_stage_replica(S4+S5)` を下限とする区間で挟んで読む。両者の差
`unexplained_hybrid` は sparse／scalar キャッシュ照会＋予算検証コストの上限
（ceiling）であり、それ以上の意味付け（個別照会ごとの値）は行わない。

**キャッシュ照会（arena／sparse／scalar）の直接計測不可について**:
`SparseIndexCache::lookup`・`SqlArenaCache::lookup`・`ScalarIndexCache::lookup`
はいずれも `pub(crate)` のため、production コード無変更（A5）を優先し
本ベンチから直接呼べない。代わりに `EngineCore::{sparse_index_cache_stats,
sql_arena_cache_stats, scalar_index_cache_stats, hnsw_index_cache_stats}`
（いずれも公開 observability API）の hit カウンタを、各ラウンドの B1
`run(...)` 呼び出し **直前・直後** でのみ採取して増分をラウンド間で合算する
（B0s〜B3・B5・B7・B8 等、B1 以外の計測が挟まる区間を含めて前後比較すると
それらの照会増分も混入し B1 単独の命中を証明できないため。codex-review
指摘・Issue #660 追記）。B1 は 1 回の `execute_sql` ごとに sql_arena・
sparse をそれぞれ高々 1 回照会するため、cache-hot 経路が成立していれば
合算増分は B1 の総実行回数（`rounds × (warmup_iterations + measured_
iterations)`）と厳密に一致するはずであり、`cache_stats_delta` 行の
`before`／`after` にはこの合算増分そのもの（`before=0`・`after=<合算増分>`）
を渡し、`expected_min` には上記の総実行回数を渡して `nonvacuous` を
「合算増分 == 総実行回数」の厳密一致で判定する（fail-closed。厳密一致に
満たない場合は打ち切る）。既定エンジン（`SearchEngineKind::Hnsw` を使わない
構築）では `hnsw_index_cache_stats` の hits 増分は構造的に 0 のはずであり、
0 以外なら fail-closed で打ち切る。`scalar_index_hits` は本クエリ形状では
照会経路自体が対象外のため、増分の値のみを informational として報告する
（`nonvacuous` は常に `true`）。

### 計測条件・実測方法

`make bench-hybrid-profile`（`BENCH_HYBRID_PROFILE_ROUNDS` で交互ラウンド数を
指定・既定 5）。段別内訳は既存 B0s〜B8 のラウンドループの直後に独立した
S0〜S8 のラウンドループとして実行し（既存出力行はバイト同一のまま無変更）、
新規出力行は `sql_surface_breakdown_raw`／`_summary`／`_bucket`・
`cache_stats_delta` 接頭辞で識別できる（`scripts/bench_hybrid_profile_ab.sh
--summarize` の grep パターンへ追加済み）。共有 QEMU 環境（本開発環境）での
実測はいずれも参考値であり、`docs/design/benchmark-judgement-policy.md` の
専有環境実測（`BENCH_DEDICATED_ENV=1`）と交互 N≥5 判定はオーナー／運用者
作業として申し送る（`bench-hybrid-profile` 全体の既存運用と同方針）。

### スモークテスト実測（本開発環境・共有 QEMU・rows=500・rounds=5・参考値）

`BENCH_HYBRID_PROFILE_ROWS=500 BENCH_HYBRID_PROFILE_ROUNDS=5` での 1 回実行例
（行数を縮小した動作確認用の値であり、25,000 行既定の実測値ではない。段の
相対的な内訳の傾向を確認する目的のみに使う）。

S8 の id 投影追記（PR #668）後の再実行例:

```
hybrid_profile: sql_surface_breakdown_summary stage=S0_dense_topk_ref min=28us median=28us
hybrid_profile: sql_surface_breakdown_summary stage=S1_parse min=3us median=3us
hybrid_profile: sql_surface_breakdown_summary stage=S2_schema min=0us median=0us
hybrid_profile: sql_surface_breakdown_summary stage=S3_bind min=2us median=2us
hybrid_profile: sql_surface_breakdown_summary stage=S1d_parse_dense min=3us median=3us
hybrid_profile: sql_surface_breakdown_summary stage=S3d_bind_dense min=2us median=2us
hybrid_profile: sql_surface_breakdown_summary stage=S4_scan_replica min=10us median=10us
hybrid_profile: sql_surface_breakdown_summary stage=S5_rowcopy_replica min=12us median=12us
hybrid_profile: sql_surface_breakdown_summary stage=S6_slotmap_replica min=3us median=3us
hybrid_profile: sql_surface_breakdown_summary stage=S7_bodyclone_replica min=16us median=16us
hybrid_profile: sql_surface_breakdown_summary stage=S8_tail min=0us median=0us
hybrid_profile: sql_surface_breakdown_bucket label=common_fixed(B3-S0) diff=22us ratio_of_b1=12.57%
hybrid_profile: sql_surface_breakdown_bucket label=hybrid_only((B1-B4)-common_fixed) diff=44us ratio_of_b1=25.14%
hybrid_profile: sql_surface_breakdown_bucket label=parse_bind(S1+S2+S3) diff=6us ratio_of_b1=3.43%
hybrid_profile: sql_surface_breakdown_bucket label=parse_bind_dense(S1d+S2+S3d) diff=6us ratio_of_b1=3.43%
hybrid_profile: sql_surface_breakdown_bucket label=parse_bind_hybrid_delta(parse_bind-parse_bind_dense) diff=0us ratio_of_b1=0.00%
hybrid_profile: sql_surface_breakdown_bucket label=scalar_stage_replica(S4+S5) diff=22us ratio_of_b1=12.57%
hybrid_profile: sql_surface_breakdown_bucket label=unexplained_common(common_fixed-(S1d+S2+S3d+S6+S8)) diff=12us ratio_of_b1=6.86%
hybrid_profile: sql_surface_breakdown_bucket label=unexplained_hybrid(hybrid_only-scalar_stage_replica) diff=21us ratio_of_b1=12.00%
hybrid_profile: sql_surface_breakdown_bucket label=projection_replica(S7) diff=16us ratio_of_b1=9.14%
hybrid_profile: cache_stats_delta name=sql_arena_hits before=0 after=250 delta=250 expected_min=250 nonvacuous=true
hybrid_profile: cache_stats_delta name=sparse_index_hits before=0 after=250 delta=250 expected_min=250 nonvacuous=true
hybrid_profile: cache_stats_delta name=scalar_index_hits before=0 after=0 delta=0 expected_min=0 nonvacuous=true
hybrid_profile: cache_stats_delta name=hnsw_index_hits before=0 after=0 delta=0 expected_min=0 nonvacuous=true
```

`rows=500` のスモーク規模では Top-`TOP_K` 件の id 投影（`Cell::Integer` 構築
1 件のみ）自体が µs 未満のため `S8_tail` の見かけの値は追記前後で変わらない
（`min=0us`）。追記の意図は S8 の**測定対象の正しさ**（id 投影処理を含む
かどうか）であり、本スモーク規模でのマイクロ秒単位の変化を確認するもの
ではない。25,000 行既定規模での前後比較は未実施（本追記は測定対象の修正が
目的で、性能上の採否判断は伴わない）。

この規模（500 行）では `scalar_stage_replica(S4+S5)`（下限、約 14%）と
`hybrid_only`（上限、約 25%）の両方が `hybrid_only ≥ scalar_stage_replica`
という設計上の期待どおりの順序を保っており、逆転（`n/a`）は発生していない。
`parse_bind_hybrid_delta`（hybrid SQL のパース・束縛 − dense SQL〔B3 と同一
文〕のパース・束縛）は本実測では `diff=0us` で、この規模・この 1 回実測では
hybrid 固有の構文・列解決コストが `unexplained_common`／`unexplained_hybrid`
の値へ有意な影響を与えていないことを示す（`parse_bind` と `parse_bind_dense`
がともに 6us で一致）。`cache_stats_delta` の `before`／`after` は各ラウンドの
B1 `run(...)` 呼び出し直前・直後でのみ採取した増分の合算値（本例では
warmup 20・measured 30 × rounds 5 = 250）であり、`sql_arena_hits`／
`sparse_index_hits` とも `expected_min`（B1 の総実行回数）と厳密一致
（`nonvacuous=true`）することで B1 単独の cache-hot 経路が成立していることを
裏付ける。`hnsw_index_hits` は既定エンジンのため期待どおり 0 のまま。
25,000 行既定規模での実測値・専有環境実測はオーナー／運用者作業として
申し送る。

### 改善候補（優先度付き起票案。実装は本 Issue の対象外・ユーザー承認後に別 Issue）

| 優先度 | 候補 | seam | 期待効果（仮説） | リスク・維持すべき契約 |
| --- | --- | --- | --- | --- |
| P1 | hybrid でも疎キャッシュヒット時（`skip_sparse_accumulation && needed_column_indices.is_empty()`）は `cache_fast_path_eligible` 相当の高速経路を通し、`build_from_cached_rls_rows` の全行再構築を省略する | `sql/exec.rs` `cache_fast_path_eligible` 判定 | `hybrid_only` 区分（本スモークテストの規模では B1 の約 26%）の大半 | 単一スナップショット契約・`RlsSafetyNet` 不変・`SqlArenaSnapshot` 借用時のスロット添字契約 |
| P2 | hybrid の投影列（`SELECT *`・`body`）にも `defer_projection` を適用する（疎キャッシュヒット時は本文複製が不要） | `sql/exec.rs` `defer_projection` 判定・`ScalarSource::Deferred` | `projection(B2-B1)`（既存区分。`projection_replica(S7)` 実測値ぶん） | 疎キャッシュミス時は従来経路へ戻す fail-closed 分岐が必要 |
| P3 | `slot_ids`／`visible_id_counts` の毎クエリ O(N) 確保を削減する（スナップショットに同梱、または恒等写像時の省略） | `sql/exec.rs` の slot map 構築箇所・`core::visible_id_counts` | S6 実測値ぶん | `provider_result_is_valid` の検証契約を維持する必要 |
| P4 | パース・束縛のセッション内キャッシュ | Issue #360 で 5% ゲート評価済み | S1〜S3 実測値ぶん | 再提案ではなく Issue #360 の結論へのリンクに留め、S1〜S3 が閾値を超える場合のみ再検討を提案 |

いずれも production コード（`crates/engine/src/`）を変更する前にユーザー承認
が必要（実装は本 Issue の対象外・テスト専任）。

### スコープ外・申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測・25,000 行既定規模での確定
  実測値の記録はオーナー／運用者作業
- wire 側（`bench-hybrid-wire-profile` の `T2−T1p`）の内訳分解は本 Issue の
  対象外（本 Issue の対象は engine 内 `B1−B4` 区分）
- `bench-internals` フックによる `SqlArenaCache`／`SparseIndexCache` の直接
  計測は production 無変更の受入基準に反するため見送り。必要なら後続 Issue
  として起票する
- 改善候補 P1〜P4 の実装は本 Issue では行わない
- production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
  無変更・テスト・ベンチ・docs 専任

## Issue #3216: コサイン経路の並列化の A/B

`ParallelSearchProvider` に `search_cosine`／`search_cosine_subset` を実装し、`<=>` の行ループを
内積経路と同じ分割方針（`MIN_ROWS_PER_THREAD`・`MAX_THREADS_PER_QUERY`・`GLOBAL_WORKER_BUDGET`）で
並列化した。結果は単一スレッド版とビット一致する。

計測は `aggregate-regression-bisect/run_bisect_step.sh`（25,000 行・128 次元・self exact）で、
変更前 `3c8056a9` を 3 回、変更後を 2 回。Apple M4 Max の共有機（loadavg 約 20〜23）で取った
**参考値**で、判定は min-of-N。生データは `bench-data/cosine-parallel-3216/`。

| phase | 対応する形 | 変更前 p50（各回, min） | 変更後 p50（各回, min） |
| ----- | ---------- | ----------------------- | ----------------------- |
| `mode_precision` | k10 | 872 / 901 / 909 µs（872） | 542 / 555 µs（542） |
| `hybrid_rrf` | k400（密側） | 2567 / 2814 / 2808 µs（2567） | 2487 / 2472 µs（2472） |
| `bulk_hybrid_k200` | k400（密側） | 2699 / 4842 / 2821 µs（2699） | 2607 / 2535 µs（2535） |

3 phase とも p50 の min は変更前より下がった。k10 の改善（約 330 µs）が最も大きい。hybrid 系は
BM25・RRF 融合などコサイン以外の段が支配的なため、下げ幅は小さい。
