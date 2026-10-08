# ADR: リランキング効果測定回帰ハーネス（TASK-108）

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-108（`docs/spec/05-tasks.md`・`docs/spec/06-roadmap.md` 参照）
- 対象ビヘイビア: SEARCH-7（`docs/spec/04-behavior/search.md`）
- 前提: TASK-107（リランキング層本体・`crates/engine/src/rerank.rs`。`Reranker`
  trait と暫定リランカー 2 種＝`IdentityReranker`・`LexicalOverlapReranker`、
  出力検証 `rerank_candidates`）は PR #146 でマージ済み
- 関連: TASK-104（`docs/design/hybrid-recall-regression.md`。決定的合成コーパス
  生成・2 層構成・strict モードによる誤 green 防止の先行実装。本タスクは同方式を
  複製・踏襲する）

## 背景

リランキング層（TASK-107）は実装・マージ済みだが、リランキングが実際に最終
Recall@20 を改善するかどうかを自動チェックする回帰テストが存在しなかった。本 ADR
は TASK-108 に対応し、リランキング適用前後（baseline / after）の最終 Recall@20 を
比較する回帰テスト＋CI workflow 追加を記録する。

**位置づけ**: TASK-104 の先例に倣い、本タスクは production コード
（`crates/engine/src/`）を変更せず、既存 API（`hybrid::hybrid_search`・
`rerank::rerank_candidates`）に対する実測ハーネス（回帰テスト）・本レポート・CI
workflow の追加に限定した。

## 検証設計

### 比較対象

同一クエリ・同一候補プール（`hybrid_search` を 1 回だけ呼び出す）から baseline・
after を測定することで、コーパス・プール生成のばらつきが比較結果に混入しないように
している。

- **baseline（リランキングなし）**: `hybrid::hybrid_search`（`RrfConfig::default()`・
  pool_depth 200）で候補プールを 1 回取得し、その先頭 20 件をそのまま最終結果とした
  場合の Recall@20
- **after（リランキングあり）**: 同じ候補プール 200 件を `rerank::RerankCandidate`
  （id・fused_score・文書 text）へ変換し、`rerank_candidates(&LexicalOverlapReranker
  ::default(), …, RerankConfig::default())`（final_k 20）の出力の Recall@20
- **補助計測（原因分析用）**: 候補プール自体の Recall@100・Recall@200。プールの中に
  正解があるのにリランキングが上位 20 件へ引き上げられていないのか、そもそも
  プール（pool_depth 200）に入っていないのかを切り分けるための指標

### 2 層構成（PR CI と閾値ゲートの分離。TASK-104 と同方式）

- **層 A**（`#[test]`。常時 `cargo test` 対象）: baseline/after の hits20・プールの
  hits100/hits200 を固定値アサーションで回帰トラッキングする。spec の数値基準は
  使わないため public 資産に閾値を持ち込まない。「after が baseline を下回らない」
  非劣化アサーション（`after_hits20 >= baseline_hits20`）も層 A に含む
  （`crates/engine/tests/rerank_recall.rs` の SEARCH-7 契約メモ参照。現在の
  実測状況は下記「実測結果」節）
- **層 B**（`#[ignore]`。`make rerank-regression` 経由）: spec 由来の Recall 下限
  （`RERANK_RECALL_MIN_R20_LARGE`＝リランキング後の最終 Recall@20 の絶対下限・
  `RERANK_RECALL_MIN_R20_IMPROVEMENT`＝baseline からの改善幅の下限。
  `.github/workflows/recall.yml` の同一 job・同一 environment `recall-gate` から
  注入）と実測値を比較する閾値ゲート。TASK-104 と同じ opt-in（未設定＝対象外）・
  strict モード（`RERANK_RECALL_REQUIRE_THRESHOLDS=1` で未設定を fail-closed 化）を
  持つ

`.github/workflows/recall.yml` の TASK-104 由来の設計判断（`pull_request` トリガを
持たない・`workflow_dispatch` のみ・`if: github.ref == 'refs/heads/main'`・
`checkout ref: main`・environment `recall-gate` の deployment branch policy による
実行境界）はそのまま踏襲する（`docs/design/hybrid-recall-regression.md`「2 層構成」
参照。spec 機密保持の理由も同一）。

### 出力方針

`crates/engine/tests/rerank_recall.rs` の標準出力方針（既定は対象名と pass/fail
のみ・`RECALL_VERBOSE=1` opt-in・`GITHUB_ACTIONS` 下は fail-closed 拒否・
`recall.yml` へは注入しない）は `hybrid_recall.rs` と同一実装を複製している
（`docs/design/hybrid-recall-regression.md`「出力方針（実測値の既定非出力・
Issue #303）」参照）。**例外（SEARCH-7 改訂 2026-08-31・PR #340 codex P1）**:
informational へ降格した `improvement_ratio@20` の実測値とその導出入力
（`baseline_hits20`／`after_hits20`／`pool_ceiling_hits20`）は「実測値の記録と
報告のみ必須」の契約を満たすため、`RECALL_VERBOSE`・`GITHUB_ACTIONS` の有無に
かかわらず常時出力する（実測値は公開可のため機密保持上の問題はない。閾値
`RERANK_RECALL_MIN_*` の値・閾値近傍の `after_recall@20` 実測値は従来どおり
verbose ゲートを維持する）。

### コーパス・QA セット・測定経路

`crates/engine/tests/rerank_recall.rs` は `hybrid_recall.rs` の決定的合成コーパス
生成（xorshift64*・Zipf 近似分布・疎密チャネルの lossy view）・QA セット生成
（`direct` カテゴリ相当）をそのまま複製する（`cjk_tokenizer_impact.rs` →
`hybrid_recall.rs` と同じ「複製・踏襲」方式。既存の `hybrid_recall.rs` の固定値
アサーションへは手を入れない）。シードは本ファイル専用の値を使う（`hybrid_recall.rs`
と依存関係を持たない自己完結を保つため）。

production API（[`SparseIndex::build`]・[`ParallelSearchProvider`]・
[`hybrid_search`]・[`rerank_candidates`]）のみを使用し、BM25/RRF/リランキングの
再実装はテスト内で行わない。`hybrid_search` が返す候補プールは融合スコア降順・
同点 id 昇順で整列済みであり、これは `rerank_candidates` が要求する入力順序契約と
同一であるため、変換以外の並べ替えは不要である。

### 指標

`hybrid_recall.rs::RecallResult` と同じ理由で、Recall@k は正解文書の総数
（`total_correct`）ではなく理論上限（`ceil` = Σmin(k,\|correct_q\|)）を分母とする
到達率（`hits / ceil`）として測定する。

## 実測結果

（`crates/engine/tests/rerank_recall.rs`、層 A の固定値アサーションは実測値へ
更新済み。決定的コーパスのため再現可能。非劣化アサーション
`after_hits20 >= baseline_hits20` は Issue #310 対応の既定重み変更後、現在
green。詳細は下記）

| 指標 | 値 |
| ---- | -- |
| 文書数 | 20,000 |
| QA 件数 | 100 |
| total_correct | 1,049 |
| ceil20 | 410 |
| baseline hits20（リランキングなし） | 387 |
| after hits20（リランキングあり） | 389（Issue #330 対応後。対応前は 388） |
| baseline Recall@20 | 0.9439 |
| after Recall@20 | 0.9488（Issue #330 対応前は 0.9463） |
| 改善量（after − baseline） | +0.0049（Issue #330 対応前は +0.0024） |
| ceil100 / pool hits100 | 913 / 837（Recall@100 = 0.9168） |
| ceil200 / pool hits200 | 1,049 / 951（Recall@200 = 0.9066） |
| pool_ceiling_hits20（Issue #330 改訂で導入） | 396 |
| improvement_ratio（Issue #330 改訂。`(after − baseline) / (pool_ceiling_hits20 − baseline_hits20)`） | 2/9 ≈ 0.2222 |

上表は `hybrid.rs::complete_boundary_tie_group_by` の境界同点グループ完全化を
「終端未確定時は再取得ループ（`fetch_k` 倍増）で終端確定を試み、再取得の上限に
達してもなお未確定なら境界同点グループ全体を除外し厳密に上位の候補のみ
保持する」契約へ変更した後（Issue #320
codex-review P1 指摘対応・`docs/design/hybrid-recall-regression.md`「Issue #310:
engine 側改善」節参照）の、かつ `LexicalOverlapReranker` の既定重みを Issue #310
対応で変更した後の実測値である。

`rerank.rs::LexicalOverlapReranker` の `rank_fused` 算出は
`hybrid.rs::accumulate_ranked` の `TieRank::GroupEnd` 分岐と同じグループ末尾
順位へ揃えてある。

Issue #310 対応で `LexicalOverlapReranker` の既定重みを `fused_weight:lexical_weight
= 3.0:1.0`（fused 優位）へ変更した（`crates/engine/src/rerank.rs`
[`LexicalOverlapReranker::default`]）。変更後の大規模段実測は baseline 387 /
after 388 で、非劣化アサーション `after_hits20 >= baseline_hits20` を満たす。

Issue #320 の大規模段追加調査（非正スコア候補の順位付け除外。`hybrid.rs::
resolve_boundary_tie_group`・`trim_non_positive_score_tail`。`docs/design/
hybrid-recall-regression.md`「Issue #320 大規模段追加調査」節参照。exhaustive
かどうかで除外対象を分岐する契約への改訂を含む）の適用前後で、本ハーネスの
上表の数値（baseline hits20・pool hits100/hits200）はいずれも変化していない
（本フィクスチャでは非正スコア候補が測定対象クエリの結果へ影響しなかった）。

プール自体の Recall@200（0.9066）と最終 Recall@20（after: 0.9463）を比較すると、
pool_depth 200 の候補プールがすでに正解の大半を含んでおり、リランキングは
その中の順位付けを改善する形で寄与し続けている（プールに入っていない正解＝
Recall@200 の未達分は、リランキング以前の候補生成段（`hybrid.rs`・`sparse.rs`）
の課題であり本タスクのスコープ外）。

なお本ハーネスは `LexicalOverlapReranker` の効果測定であり、方式確定前の暫定構成
（下記「既知の制約」参照）における測定値であることに注意する。

## Issue #330: 改善幅未達の原因分析と engine 側改善

`recall.yml` の rerank 大規模段ゲートのうち、改善幅ゲート
（`RERANK_RECALL_MIN_R20_IMPROVEMENT`）が未達だった（Issue #310 対応後の
improvement@20 は +0.0024 相当）。絶対下限ゲート（after Recall@20 そのもの）は
達していた。本節はこの未達要因の分析と、その分析に基づく engine 側改善
（`crates/engine/src/rerank.rs::LexicalOverlapReranker`）を記録する。

### 未達要因の分析（診断内訳。上表と同一フィクスチャ）

理論上限 ceil20（410）と baseline hits20（387）の差 23 hit のうち、リランカーの
入力である候補プール自体に含まれない（プール外）ものが 14 hit、プール内に
含まれる（＝リランキングで到達しうる）ものが 9 hit だった。プール内 9 hit を
クエリ語と候補 text の字句重なり数（overlap）で内訳すると、overlap=2（クエリ 2
トークンとも一致）が 2 件、overlap=1 が 6 件、overlap=0 が 1 件だった。

一方、top20 中の不正解の overlap 分布は overlap=1 が大半（クエリ当たり ≒16 件）を
占め、overlap=2 は 0 件だった。本フィクスチャでは「text がクエリ 2 語とも含む
（overlap=2）⇒ ほぼ確実に正解」という構造がある一方、正解の大半は overlap=1 で
top20 中の不正解と字句・fused いずれの信号でも区別がつかない。したがって字句一致
信号で追加的に引き上げ可能なのは overlap=2 の 2 件のみであり、**字句信号での改善
到達上限は 389（+2 hit・改善幅 ≈ +0.0049）** と見積もられる（プール外 14 hit・
overlap=1/0 の 7 hit はリランカー単独では原理的に到達不能）。

### 現行実装が上限に届かなかった原因と対応

`LexicalOverlapReranker::rerank` の `rank_lexical`（字句一致順位）は、overlap
降順の安定ソート後の**位置順位**（`rank_idx + 1`）で算出していた。この方式では
overlap が同点の候補グループ内でも、ソートの安定性により元の並び順（＝融合スコア
降順）がそのまま順位差として残り、`contribution_fused`（融合順位の寄与）と
`contribution_lexical`（字句順位の寄与）の両方に融合スコア順位の情報が二重に
反映される非対称が生じていた。`rank_fused` 側は Issue #310 対応で
`hybrid.rs::TieRank::GroupEnd`（同点グループは末尾の共通順位を共有）へ統一済み
だったため、字句側だけがこの規約と異なるままだった。

対応として、`rank_lexical` の算出を `rank_fused_by_idx` と同じグループ末尾順位
（GroupEnd）へ変更した（overlap が同点のグループ内では全メンバーが同一の
`rank_lexical` を共有し、順位差は `rank_fused` 側のみに由来する）。フィクスチャ
パラメータ（`*_DROPOUT_PROB`・`VECTOR_DECOY_PROB`・規模・シード・`k`）は変更せず、
`crates/engine/src/rerank.rs::LexicalOverlapReranker`（既定重み 3.0:1.0 は不変）の
みを変更した結果、after hits20 は 388 → 389（改善幅 +0.0024 → +0.0049）となり、
字句信号の到達上限（389）に一致した。

### 重要な制約: 到達上限は字句信号の構造的上限

389（改善幅 +0.0049）は本フィクスチャにおける字句一致信号の構造的上限であり、
`recall.yml` の閾値（spec 由来・非公開）がこれを上回る場合、`LexicalOverlapReranker`
単独では改善幅ゲートに到達できない。その場合はプール外 14 hit・overlap=0/1 の
7 hit をカバーするための候補生成段（`hybrid.rs`・`sparse.rs`）の改善、フィクスチャ
自体の見直し、リランカー方式（クロスエンコーダ等）の確定のいずれかが必要であり、
オーナー判断事項となる。

### Issue #330 追記: 改善幅ゲートを候補プール上限に対する相対比率へ再定義（SEARCH-7 改訂）

上記の分析が示すとおり、baseline hits20（387）と ceil20（410）の差 23 hit のうち、
14 hit はプール外（候補生成段の課題・リランキング単独では原理的に到達不能）であり、
リランキングが到達しうる改善余地はプール内の 9 hit のみである。改善幅を
`ceil20` に対する絶対差（`after_recall20 − baseline_recall20`）で測る従来の
ゲート定義は、この「原理的に到達不能な範囲」を分母に含めてしまうため、暫定
リランカー（字句一致方式）の効果を過小評価する。

そこでオーナー承認済みの spec 改訂（fandhe-db-spec#7）に合わせ、`recall.yml` の
`RERANK_RECALL_MIN_R20_IMPROVEMENT` ゲートの判定基準を以下へ再定義した
（`crates/engine/tests/rerank_recall.rs::RerankRecallResult::improvement_ratio`）:

1. 非劣化: `after_hits20 >= baseline_hits20`（既存の層 A 固定値アサーションを維持）
2. 相対比率:
   `(after_hits20 − baseline_hits20) / (pool_ceiling_hits20 − baseline_hits20) >= RERANK_RECALL_MIN_R20_IMPROVEMENT`。
   ここで `pool_ceiling_hits20`
   （＝候補プール 200 件内に完璧な並び替えを施した場合に上位 20 件で回収しうる
   理論上限。クエリ単位の `min(20, プール内正解数)` の総和）は本フィクスチャで
   実測 396（プール内回収余地 9 hit のうち可能な最大＝ 387 + 9）
3. 改善余地（分母 `pool_ceiling_hits20 − baseline_hits20`）が `ceil20` の 1%
   未満（構造的にほぼ改善不可能）の場合は条件 2 を自動充足とし、条件 1 のみで
   判定する（分母 0 に近づく不安定さを避ける fail-closed 対策を兼ねる）

Issue #330 対応後の本フィクスチャでの実測比率は
`improvement_ratio = (389 − 387) / (396 − 387) = 2 / 9 ≈ 0.2222`
（字句信号の構造的上限に達している状態での実測値）。`RERANK_RECALL_MIN_R20_LARGE`
（絶対下限）の判定方式は変更していない。

### Issue #333: クロスエンコーダ方式の導入試行と依存追加の中断

SEARCH-7 の相対基準（`improvement_ratio` の下限。オーナー承認済みの公開可能な
数値基準として ratio ≥ 0.6。`.claude/rules/spec-confidentiality.md`「許可される
参照」参照）に対し、上記「Issue #330 追記」の実測（ratio ≈ 0.2222）が字句一致
方式の構造上限であることを踏まえ、方式変更としてクロスエンコーダ型リランカーの
導入を試みた（オーナー承認 2026-08-30・Issue #333 案A）。

**実装した部分（依存追加なし・本 PR に含む）**:

- `crates/engine/src/rerank.rs` に推論バックエンドを差し替え可能にする
  [`CrossEncoderBackend`] trait（`(query, passage)` ペアのスコアリングを抽象化）・
  [`CrossEncoderConfig`]（`batch_size`/`max_candidates`/`max_seq_len` の検証付き
  構築）・[`CrossEncoderReranker`]（[`Reranker`] 実装。バッチ分割・fail-closed な
  長さ不一致/非有限スコア/バックエンド失敗の拒否）・`CrossEncoderError`
  （[`RerankError::CrossEncoder`] へ委譲）を追加した
- `crates/engine/tests/rerank_cross_encoder.rs`（feature 非依存・常時 `cargo test`
  対象）に決定的スタブ推論バックエンドによる契約テスト（順序決定性・
  `max_candidates`/バッチサイズの有界化・長さ不一致/非有限スコア/バックエンド
  失敗時の fail-closed）を追加した（受け入れ条件 2 を満たす）
- `crates/engine/tests/fixtures/nl_qa.rs` に、事前学習済みモデルの評価に適した
  自然言語 QA 決定的 fixture（60 種の潜在概念、各 2〜3 種の英語表層変種。文書は
  常に主要な言い回し、クエリは常に別の言い回しで生成し、字句一致では拾いにくい
  意味的一致ペアを意図的に作る設計）と、`&dyn Reranker` を受け取る一般化した
  測定関数 `measure_recall_with_reranker`（`rerank_recall.rs::
  measure_rerank_recall` の複製・一般化版。既存ファイル・既存ゲートは無変更）を
  追加した

**依存追加を中断した経緯**: `ort = "=2.0.0-rc.13"` + `tokenizers = "=0.23.1"`
（`optional = true` + `cross-encoder` feature。実装計画どおり `default-features =
false` + `load-dynamic` 構成）を `crates/engine/Cargo.toml` へ追加し、
`cargo tree -p engine --features cross-encoder -e features` で依存解決・
`load-dynamic` 以外の feature が有効化されていないこと・`cargo tree -p
wire-server` に両クレートが出現しないことを確認したが、`make deny` の
advisories チェックが `tokenizers` の推移的依存 `paste`（unmaintained advisory
RUSTSEC-2024-0436）で fail した。実装計画の fail-closed 方針（承認範囲を超える
`deny.toml` の allow-list 追加・別バージョン採用・別クレート追加は行わず、
計画外事項として報告する）に従い、`Cargo.toml`・`Cargo.lock` への依存追加は
ロールバックした。

**結果として本 PR に含まれないもの**（受け入れ条件 3・4 は実測未了）:

- 実 ONNX 推論バックエンド（`OnnxCrossEncoderBackend` サブモジュール。設計・
  想定 API は実装計画に記録済みだが、依存が追加できないため実装していない）
- モデルパス環境変数による opt-in 実測ハーネス（`ort`/`tokenizers` に依存する
  ため同様に未実装）
- 自然言語 fixture 上でのクロスエンコーダ実測値・SEARCH-7 相対基準への到達可否・
  #330 への報告

`CrossEncoderBackend` trait を先に確立してあるため、依存の扱い
（`RUSTSEC-2024-0436` を受容する owner 判断での `deny.toml` 例外追加、`paste` を
含まない `tokenizers` の代替バージョン選定、または別クレートの検討）が
オーナー判断で解決した後続 PR では、この trait・[`CrossEncoderReranker`] 自体の
再設計なしに `cross_encoder_onnx` サブモジュールを追加するだけで実推論・実測へ
進められる。

**参考実測（`LexicalOverlapReranker`、クロスエンコーダではない）**: 自然言語
fixture（`tests/nl_qa_fixture.rs`。seed `0x1234_5678`・200 docs・20 queries）で
`LexicalOverlapReranker::default()` を測定したところ、`baseline_hits20=43` →
`after_hits20=41`（`pool_ceiling_hits20=79`。改善余地に対して字句一致リランキング
が *悪化* させた）。既存の合成トークン fixture（`rerank_recall.rs`）とは異なり、
本 fixture は文書・クエリで意図的に異なる言い回し（表層変種）を使うため字句
一致では拾いにくい設計であり、この結果は字句一致方式の限界を裏付ける
（クロスエンコーダを検討する動機の直接的な実測根拠）。クロスエンコーダ自体の
実測値は前述のとおり未取得。

### Issue #333 追記: 依存追加の完了・実 ONNX バックエンド接続・実測結果

上記「Issue #333」節の中断後、オーナー承認（2026-08-30・Issue #333 再 open
コメント）により `deny.toml` の `[advisories] ignore` へ `RUSTSEC-2024-0436`
（`paste`。proc-macro 専用クレート・unmaintained advisory のみで既知の脆弱性
advisory ではない）を理由・承認記録付きで追加し、`ort = "=2.0.0-rc.13"`
（`default-features = false` + `load-dynamic` + `api-17`）・
`tokenizers = "=0.23.1"`（`default-features = false` + `onig`）を
`crates/engine/Cargo.toml` の `cross-encoder` feature（optional 依存）背後へ
追加した。`api-17`（`ort-sys` 側の既定 API レベル）・`onig`（`tokenizer.json` の
正規表現ベース pre-tokenizer 設定のロードに必要な既定機能）はいずれも承認済み
クレート内の最小 feature 追加であり、新規クレート・別バージョンの追加ではない
（実装計画の fail-closed 方針の範囲内）。`cargo deny --locked check advisories
bans licenses sources`（`[graph] all-features = true` により `cross-encoder`
経由の推移的依存も監査対象）は advisories・bans・licenses・sources いずれも
`ok` を確認済み。

**実 ONNX 推論バックエンド**: `crates/engine/src/rerank/cross_encoder_onnx.rs`
（`cross-encoder` feature 限定）に `OnnxCrossEncoderBackend`
（[`CrossEncoderBackend`] 実装）を追加した。設計上の要点:

- `ort` の `load-dynamic` feature は既定で固定名の共有ライブラリ
  （linux では `libonnxruntime.so`）を `dlopen` するが、多くの環境の実ファイル名は
  バージョン付き（`libonnxruntime.so.N`）でこれと一致しない。既定解決に任せて
  `ort` の他 API へ触れると、`ort` 内部の dylib ロード失敗処理が `Result` を
  返さず panic する経路があるため（coding-rust.md「ライブラリコードでは
  `Result` を返し、panic させない」に抵触）、`OnnxCrossEncoderBackend::from_files`
  は環境変数 `ORT_DYLIB_PATH` を自前で読み、未設定ならここで `Err` を返して
  `ort::` の他 API を一切呼ばずに打ち切る。設定済みの場合のみ `ort::init_from`
  （`Result` を返す明示ロード API）で dylib を確定させてから `Session` を構築する
- ロード時にモデルの入力名（`input_ids`/`attention_mask`/`token_type_ids`）を
  検査し、想定外の構成は構築時に fail-closed で拒否する
- 単体テスト（`ORT_DYLIB_PATH` 未設定・モデル/トークナイザファイル不在の環境。
  `--all-features` の `make test`/`make ci`・pre-push フックの既定経路）で
  panic せず `Err` を返すことを固定した

**opt-in 実測ハーネス**: `crates/engine/tests/rerank_cross_encoder_recall.rs`
（`#![cfg(feature = "cross-encoder")]`・`#[ignore]`）を追加し、
`make rerank-cross-encoder-eval`（Makefile）から手動実行する。`bench-tier`
（TASK-116）と同じ位置づけで CI には配線しない。`CROSS_ENCODER_MODEL_PATH`・
`CROSS_ENCODER_TOKENIZER_PATH`・`ORT_DYLIB_PATH` が未設定の場合は明確な
メッセージで fail する（fail-closed）。

**実測結果**（seed `0x1234_5678`・200 docs・20 queries。`tests/fixtures/
nl_qa.rs`。採用モデル: `cross-encoder/ms-marco-MiniLM-L-6-v2` の ONNX 変換版
〔配布元 `Xenova/ms-marco-MiniLM-L-6-v2`〕。ライセンス Apache-2.0。onnxruntime
共有ライブラリ・モデルファイルはリポジトリへコミットしていない）:

| 指標 | 値 |
| ---- | -- |
| `baseline_hits20`（リランキングなし） | 43 |
| `after_hits20`（クロスエンコーダ適用後） | 18 |
| `pool_hits100` / `pool_hits200` | 77 / 89 |
| `pool_ceiling_hits20` | 79 |
| `ceil20` / `ceil100` / `ceil200` | 79 / 89 / 89 |
| `improvement_ratio` | `0`（`after_hits20 < baseline_hits20` のため `saturating_sub` により改善幅が 0 扱い） |

同一モデル・同一入力での 2 回実測は完全一致（決定性を確認済み）。

**SEARCH-7 相対基準（ratio ≥ 0.6）には未到達**。むしろ字句一致方式
（`LexicalOverlapReranker`。上記参考実測 `after_hits20=41`）よりも悪化した。

**原因分析**（`tests/fixtures/nl_qa.rs::generate_nl_corpus` の文書生成ロジックを
踏まえた切り分け。個別クエリ・文書ペアでの生スコア確認では、正解文書が不正解
文書より高いスコアを得るケース自体は観測でき、スコアの向き自体は妥当——モデルの
呼び出し・入出力対応が反転しているような実装バグではない）:

- `Doc.text` は文書の主要概念 2 件（`{a}`/`{b}`）を流暢な英文テンプレートへ
  埋め込む一方、3 件目以降の追加概念語（キーワード数 2〜4 のうち超過分）は
  文末へ単語＋ピリオドのみを機械的に追記する構造になっている（同ファイルの
  文書生成ロジック参照）。クエリは各文書の「出現頻度が最も低い 2 概念」を
  AND 条件に選ぶため、正解判定に使う概念が文末の非流暢な追記フレーズ側に
  偏りやすい
- 事前学習済みクロスエンコーダ（MS MARCO passage ranking で学習）は文全体の
  自然な意味構造を読む前提のモデルであり、この「主要 2 概念を流暢な文で
  記述し、正解概念は文末に無関係な内容の後付けで追記する」という
  fixture 特有の構造は学習分布から外れる。文全体の主題（無関係な 2 概念の
  組み合わせ）に引きずられ、文末の短い追記フレーズ（実際の正解根拠）を
  相対的に軽視しやすいと考えられる
- 対照的に、RRF 融合ベースの baseline は文の流暢さに依存せず `keywords`
  集合（疎チャネルの語彙 one-hot・密チャネルのベクトル）を直接手がかりに
  するため、この構造による不利益を受けない

**到達可否の判断**: 本 fixture・本モデルの組み合わせでは SEARCH-7 相対基準に
到達しない。これは方式（クロスエンコーダ）自体の欠陥ではなく、fixture の
文書生成が「流暢な自然文としての意味理解」を前提とするモデル評価に適さない
構造（正解概念が非流暢な追記フレーズに偏る）を持つことが主因と考えられる。
到達可否・原因分析は Issue #330 へ報告する。fixture 側の再設計（正解概念を
主要 2 概念スロット `{a}`/`{b}` にも均等に含める等）・別モデル選定の要否は
オーナー判断に委ねる。

**再現手順**:

```bash
export ORT_DYLIB_PATH=/path/to/libonnxruntime.so
export CROSS_ENCODER_MODEL_PATH=/path/to/model.onnx
export CROSS_ENCODER_TOKENIZER_PATH=/path/to/tokenizer.json
make rerank-cross-encoder-eval
```

`model.onnx`・`tokenizer.json` は `cross-encoder/ms-marco-MiniLM-L-6-v2`
（Apache-2.0）の ONNX 変換版を運用者が取得する。onnxruntime 共有ライブラリは
ONNX Runtime（MIT）配布物を使う。いずれもリポジトリへはコミットしない。

### Issue #337: 自然言語 fixture の再設計とクロスエンコーダ improvement_ratio の再実測

上記「Issue #333 追記」節の原因分析（正解概念が文末の非流暢な追記フレーズに
偏る構造）を受け、オーナー判断（2026-08-30・Issue #330 コメント）により
`tests/fixtures/nl_qa.rs` の文書テキスト生成方式を再設計し、クロスエンコーダが
機能する条件で再実測した。

**再設計の内容**（詳細は `nl_qa.rs` モジュールドキュメント「文書テキスト生成方式
（Issue #337 で再設計）」参照）:

- 旧方式は主要 2 概念のみを流暢な英文テンプレートへ埋め込み、3 件目以降の
  概念語は文末へ単語＋ピリオドで機械的に追記していた。新方式は 3 件目以降の
  概念も `CONCEPT_SENTENCE_TEMPLATES`（10 種の完結した英文テンプレート）で
  1 概念 1 文として埋め込み、単語の裸追記を全廃した
- 正解集合（`Doc::keywords`・QA の `correct`）を決めるキーワード抽選用の rng
  ストリームと、新設した文テンプレート選択用の rng ストリームを分離した
  （`generate_nl_corpus` 内 `sentence_rng`）。これによりテキスト生成方式の
  変更だけを独立変数にし、Issue #333 時点との比較可能性を保っている
- 字句ミスマッチ設計（文書は variant 0・クエリは末尾 variant）・決定的生成
  （xorshift64* seed 固定）・疎/密チャネルの dropout・decoy 構造・
  seed `0x1234_5678`・200 docs・20 queries は変更していない

**再設計の妥当性の裏付け**: 正解集合はキーワード抽選（`kw_set`/`inverted`）
のみから決まり、テキスト生成方式には依存しない。実際、`ceil20`（コーパス全体の
正解件数上限）・`ceil100`/`ceil200` はいずれも Issue #333 時点と完全に同一
（79 / 89 / 89）であり、rng ストリーム分離により QA 抽選そのものが不変である
ことを実測で確認している。一方 `baseline_hits20`（RRF 融合のみ・43→46）・
`pool_hits100`（77→79）は文書テキストの語数増加による疎チャネル（BM25 系）側の
変動であり、これは再設計が意図した効果（3 件目以降の概念も流暢な文脈語込みで
文書テキストに反映される）の副産物として想定内である。

**層 A 固定値の更新**（`tests/nl_qa_fixture.rs::nl_qa_fixture_lexical_reranker_recall_regression`）:

| 指標 | Issue #333 時点 | Issue #337 再設計後 |
| ---- | ---------------- | -------------------- |
| `baseline_hits20`（`LexicalOverlapReranker` 適用前） | 43 | 46 |
| `after_hits20`（`LexicalOverlapReranker` 適用後） | 41 | 43 |
| `pool_ceiling_hits20` | 79 | 79（不変） |
| `ceil20` | 79 | 79（不変） |

字句一致リランカーが `after_hits20 < baseline_hits20` と悪化する傾向自体は
再設計後も変わらない（字句一致方式そのものの構造的な弱点であり、Issue #333 が
クロスエンコーダへの方式変更を検討する動機は引き続き有効）。

**クロスエンコーダ再実測結果**（seed `0x1234_5678`・200 docs・20 queries。
モデル・onnxruntime・実測手順は上記「Issue #333 追記」節と同一構成
〔`cross-encoder/ms-marco-MiniLM-L-6-v2` の ONNX 変換版・`make
rerank-cross-encoder-eval`〕。ローカル運用者環境での実測。取得したモデル・
トークナイザファイルは前回実測分がローカルに残っていなかったため再取得した）:

| 指標 | Issue #333 時点 | Issue #337 再設計後 |
| ---- | ---------------- | -------------------- |
| `baseline_hits20`（リランキングなし） | 43 | 46 |
| `after_hits20`（クロスエンコーダ適用後） | 18 | 20 |
| `pool_hits100` / `pool_hits200` | 77 / 89 | 79 / 89 |
| `pool_ceiling_hits20` | 79 | 79 |
| `ceil20` / `ceil100` / `ceil200` | 79 / 89 / 89 | 79 / 89 / 89 |
| 改善余地（headroom = `pool_ceiling_hits20 − baseline_hits20`） | 36（≥0.6 到達には `after_hits20` ≥ 65 が必要） | 33（≥0.6 到達には `after_hits20` ≥ 66 が必要） |
| `improvement_ratio` | `0`（`after_hits20 < baseline_hits20`） | `0.0000`（`after_hits20 < baseline_hits20`。`saturating_sub` により改善幅 0 扱い） |

同一モデル・同一入力での 2 回実測は完全一致（決定性を確認済み）。

**SEARCH-7 相対基準（ratio ≥ 0.6）には引き続き未到達**。fixture 再設計後も
字句一致方式（`after_hits20=43`）を下回る結果になった。

**原因分析**（個別クエリ・文書ペアでの生スコア確認。診断用の一時的なハーネス
コード〔非コミット〕で `CrossEncoderBackend::score_pairs` を直接呼び、候補プール
100 件中の順位を確認した。20 クエリ中先頭 5 件を確認）:

- クエリ単位で見ると挙動は一様ではない: 正解文書がスコア 1 位（先頭）に来る
  クエリがある一方、候補プール中で 16 位・23 位・84 位まで沈むクエリもあった。
  「常に悪化する」わけではなく「クエリによって大きくばらつく」ことが実態であり、
  fixture の非流暢な追記構造という単一要因では説明しきれない
- 高順位に沈むケースの多くで、クエリの言い換え語（variant 末尾）と表層上
  無関係な別概念が偶然語を共有し、誤って高スコアを得る現象を観測した。例:
  クエリ「horizontal data split」（`data partitioning`/`sharding` の言い換え）に
  対し、別概念「horizontal scaling」（`adding more nodes` 等の同義語群）を含む
  文書が "horizontal" という表層語の共有だけで上位に来ていた。[`CONCEPTS`] の
  語彙は概念間で意図的な重複を作っていないが、59 概念・各 2〜3 variant という
  語彙規模の中で表層語が部分的に重なる概念ペアが偶発的に生じている
- コーパス全体が同一のドメイン語彙（59 概念のデータベース・分散システム用語）を
  再利用する構造のため、正解でない候補文書も検索クエリと同じ技術分野の語彙を
  多く含み、汎用 MS MARCO 学習済みの小型モデル（MiniLM-L-6）にとって
  「意味的に近いが不正解」な候補と「正解」の区別が難しい可能性がある
  （実際のクエリログ由来の MS MARCO データセットに比べ、本 fixture は語彙密度が
  人工的に高い）

**到達可否の判断**: 本 fixture・本モデルの組み合わせでは、文書テキストの
流暢さを改善しても SEARCH-7 相対基準に到達しなかった。Issue #333 追記時点の
仮説（非流暢な追記構造が主因）は再設計後の結果により部分的に否定された——
流暢さの改善だけでは解決しない、より構造的な要因（語彙規模に対して概念数・
variant 数が少なく表層語の偶発的重複が生じやすいこと、ドメイン語彙の密度が
汎用モデルの学習分布と乖離していること）が残っている可能性が高い。方式
（クロスエンコーダ）自体の欠陥と断定する根拠はなく、これ以上の fixture
パラメータ調整（語彙規模の拡大等）でゲート到達を狙うことは Issue #337 の
受け入れ条件が禁じる「ゲートを通すためのパラメータ操作」に該当するため行わない。
到達可否・原因分析は Issue #330 へ報告する。

**再現手順**: 上記「Issue #333 追記」節と同一（`ORT_DYLIB_PATH`・
`CROSS_ENCODER_MODEL_PATH`・`CROSS_ENCODER_TOKENIZER_PATH` を設定して
`make rerank-cross-encoder-eval`）。

### SEARCH-7 改訂（2026-08-31）: improvement ゲートの informational 降格

**総括**: Issue #330・#333・#337 で 2 方式（字句一致・クロスエンコーダ）×
2 fixture（当初の合成コーパス・自然言語再設計後の合成コーパス）の組み合わせを
すべて実測したが、改善幅の相対比率
（`crates/engine/tests/rerank_recall.rs::RerankRecallResult::improvement_ratio`）
はいずれも下限に未達だった（字句一致方式: 0.222。クロスエンコーダ方式: 0）。
双方の未達要因分析（本ファイル「Issue #330」「Issue #333 追記」「Issue #337」の
各節参照）により、原因はリランキング方式の欠陥ではなく合成 fixture 側の構造要因
（キーワード抽選による正解集合の作られ方・表層語の偶発的重複・人工的に高い
語彙密度）にあると判断した。これ以上の fixture パラメータ調整による到達は
「ゲートを通すためのパラメータ操作」に該当し許容されないため、Issue #337 は
これを最終報告として Issue #330 へ差し戻した。

**spec 改訂**: 上記の実測・分析結果を受け、オーナー承認済みの spec 改訂
（fandhe-db-spec#8・2026-08-31）により、SEARCH-7 の改善幅相対基準
（improvement_ratio ≥ 下限）は実コーパス評価まで informational（非ブロッキング・
実測値の記録と報告のみ必須）へ降格された。ブロッキング判定は非劣化
（`after_hits20 >= baseline_hits20`）と絶対下限（`RERANK_RECALL_MIN_R20_LARGE`）
のみとする。

**本リポ側の変更**: `crates/engine/tests/rerank_recall.rs` の層 B ゲート
（`rerank_recall_large_scale_threshold_gate`）から `RERANK_RECALL_MIN_R20_
IMPROVEMENT` の読み取り・pass/fail への算入を削除し、`RERANK_RECALL_MIN_R20_
LARGE`（絶対下限）と非劣化のみで判定する形に変更した。`improvement_ratio`
（`RerankRecallResult::improvement_ratio`）の実測値とその導出入力は
「実測値の記録と報告のみ必須」の契約を満たすため、`RECALL_VERBOSE`・
`GITHUB_ACTIONS` の有無にかかわらず**常時出力**する（本ファイル「出力方針」節の
例外記載・PR #340 codex P1 対応参照）。層 A の固定値回帰
（`rerank_recall_large_scale_regression`）でも引き続き算出・出力する。
`.github/workflows/recall.yml` の rerank step からは `RERANK_RECALL_MIN_R20_
IMPROVEMENT` secret の注入を外した（`docs/design/ci-gate-variables.md` 参照）。

**申し送り**: 実コーパスでの評価が行われた際、改善幅相対基準を再びブロッキング
化するかどうかはその実測結果をもとにオーナーが再判断する。

## 既知の制約・スコープ外

- **暫定リランカーの効果測定である**: 同梱リランカー（[`LexicalOverlapReranker`]）は
  方式確定までの暫定実装であり、クロスエンコーダ等の本命方式は TASK-107 の残
  オーナー判断・依存承認（`.claude/rules/dependency-policy.md`）待ちである。本測定は
  「暫定構成でも Recall が悪化しないこと・字句一致順位による再順位付けが一定の改善を
  もたらすこと」の確認にとどまり、本命方式導入後の効果を保証しない。暫定構成での
  測定結果が spec 基準（層 B ゲート）に届かない場合の方式見直し・差し戻し判断は、
  本レポートの原因分析（プール活用状況・Recall@200 実測）を材料にオーナーへ報告する
- **合成コーパスによる暫定測定**: `hybrid_recall.rs`（TASK-104）と同種の制約。実
  コーパスでの評価は未了
- **`VectorCore` trait への統合・SQL 表層統合は後続タスクの管轄**: 本ハーネスは
  `hybrid_search`・`rerank_candidates` という純粋関数層の測定であり、上位層（RLS
  可視性判定・SQL 表層）との結合測定は対象外
- spec の基準充足判定（Recall@20 絶対下限・改善幅の下限）は environment
  `recall-gate` の閾値ゲート（層 B）で行う。値そのもの（spec 由来の数値基準）は
  本レポートには記載しない
- Actions secrets（`RERANK_RECALL_MIN_*`）の実値設定はマージ後のリポジトリ管理者
  作業（README「Recall 回帰ハーネスの repo secrets」参照。secret ↔ spec
  ポインタの対応表・設定手順は `docs/design/ci-gate-variables.md` に集約した。
  当初は Actions variables を使っていたが、`env:` ブロックのログ印字による
  漏えいを防ぐため secrets へ移行した（Issue #286）

[`SparseIndex::build`]: ../../crates/engine/src/sparse.rs
[`ParallelSearchProvider`]: ../../crates/engine/src/parallel_search.rs
[`hybrid_search`]: ../../crates/engine/src/hybrid.rs
[`rerank_candidates`]: ../../crates/engine/src/rerank.rs
[`LexicalOverlapReranker`]: ../../crates/engine/src/rerank.rs
[`Reranker`]: ../../crates/engine/src/rerank.rs
[`CrossEncoderBackend`]: ../../crates/engine/src/rerank.rs
[`CrossEncoderConfig`]: ../../crates/engine/src/rerank.rs
[`CrossEncoderReranker`]: ../../crates/engine/src/rerank.rs
[`RerankError::CrossEncoder`]: ../../crates/engine/src/rerank.rs
