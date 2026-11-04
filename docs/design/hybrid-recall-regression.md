# ADR: ハイブリッド検索 Recall 回帰ハーネス（TASK-104）

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-104（`docs/spec/05-tasks.md`・`docs/spec/06-roadmap.md` 参照）
- 対象ビヘイビア: SEARCH-1, SEARCH-2（`docs/spec/04-behavior/search.md`）
- 前提: TASK-102（BM25 疎検索カーネル・`crates/engine/src/sparse.rs`）・TASK-103
  （RRF 融合・`crates/engine/src/hybrid.rs`）・TASK-105（CJK ストップワード除去）は
  いずれもマージ済み（PR #138・#142・#144）
- 関連: TASK-106（`docs/design/cjk-tokenizer-impact-ja-corpus.md`。決定的合成コーパス
  生成・固定値回帰トラッキング方式の先行実装）・TASK-127（`crates/engine/benches/
  simd_bench.rs`。spec 閾値の Actions secrets 注入パターンの先行実装。
  当初は variables を使っていたが Issue #286 で secrets へ移行）・TASK-110〜113
  （`docs/design/query-planning-recall-regression.md`。クエリ展開クライアント・
  決定的スタブ `LlmClient`・展開あり Recall 回帰の先行実装）・Issue #306
  （大規模段層 B へのクエリ展開結線）

## 背景

ハイブリッド検索カーネル（疎 BM25・RRF 融合・CJK ストップワード除去）は実装・マージ
済みだが、検索品質（Recall）の受け入れ基準を自動チェックする回帰テストが存在しな
かった。本 ADR は TASK-104 に対応し、小規模・大規模の 2 段のスケール条件付き Recall
基準を自動チェックする回帰テスト＋CI workflow を追加した設計を記録する。

**位置づけ**: TASK-106 の先例（`docs/design/cjk-tokenizer-impact-ja-corpus.md`）に
倣い、本タスクは production コード（`crates/engine/src/`）を変更せず、既存 API に
対する実測ハーネス（回帰テスト）・本レポート・CI workflow の追加に限定した。

## 検証設計

### 2 層構成（PR CI と閾値ゲートの分離。役割分担は spec 機密保持が優先）

- **層 A**（`#[test]`。常時 `cargo test` 対象）: 決定的コーパスでのヒット数を固定値
  アサーションで回帰トラッキングする（TASK-106 と同方式）。spec の数値基準を使わない
  ため public 資産に閾値を持ち込まない。`.github/workflows/ci.yml` の `cargo test`
  で PR ごとに常時実行されるため、**PR のマージ判定は層 A が担う**
- **層 B**（`#[ignore]`。`make recall-regression` 経由）: spec 由来の Recall 下限
  （`HYBRID_RECALL_MIN_R20_SMALL`・`HYBRID_RECALL_MIN_R20_LARGE`・
  `HYBRID_RECALL_MIN_R100_LARGE`。`.github/workflows/recall.yml` が environment
  `recall-gate` の Actions secrets から注入。Issue #286・`env:` ブロックの
  ログ印字による漏えいを防ぐため variables ではなく secrets を使う）と実測値を
  比較する閾値ゲート。
  ローカルの `make recall-regression`（`HYBRID_RECALL_REQUIRE_THRESHOLDS` を
  注入しない）では未設定（GitHub Actions では空文字列に解決される variable も
  含む）は「ゲート未設定＝明示的に対象外」を出力して成功終了し
  （`crates/engine/benches/simd_bench.rs::core5_requested_from_env` と同じ
  opt-in パターン）、設定済みで非数値・範囲外は fail-closed でテスト失敗とする。
  `recall.yml` からの実行は strict モード（下記「strict モードによる誤 green
  防止」参照）が既定で有効なため、未設定も fail-closed になる。ログには対象名と
  pass/fail のみを出力する（README「Recall 回帰ハーネスの repo secrets」参照。
  出力方針の詳細は下記「出力方針（実測値の既定非出力・Issue #303）」参照）。

  `.github/workflows/recall.yml` は `pull_request` トリガを**意図的に持たない**
  （`workflow_dispatch` + 週次 `schedule`。下記「strict モードによる誤 green
  防止」の疎通確認後、Issue #168 で `schedule` を再追加済み）。`pull_request` で起動する job は
  PR 側の untrusted なコード（Makefile・テストコード含む）を checkout して実行する
  ため、層 B を PR トリガにすると PR がコードを書き換えて `HYBRID_RECALL_MIN_*`
  （spec 由来の非公開閾値）を標準出力へ書き出すだけで public な Actions ログから
  spec の数値基準を取得できてしまう（`.claude/rules/spec-confidentiality.md` の
  P0 違反。PR #147 codex-review で一度 `pull_request` トリガを追加したが、この
  P0 指摘により巻き戻した）。この経緯から、層 B は「trusted なコードのみが走る
  非公開閾値ゲート」、層 A は「PR ごとに走る public な固定値ゲート」という役割分担を
  設計判断として採用する

  トリガを絞るだけでは不十分な点にも注意が必要である: `workflow_dispatch` は
  本来任意の ref を選んで手動起動できるため、Makefile・テストコードを書き換えた
  任意 ref を選んで実行すれば `pull_request` と同じ経路（書き換えたコードが
  `HYBRID_RECALL_MIN_*` を受け取り標準出力へ書き出す）で spec 閾値を漏えいできる
  （PR #147 codex-review 継続指摘）。そのため `recall-regression` job には
  `if: github.ref == 'refs/heads/main'` を付け、main 以外の ref を選んで
  `workflow_dispatch` を起動した場合は job ごとスキップする。`actions/checkout` も
  dispatch 側の ref 選択に依存させず `ref: main` で明示的に固定する。

  ただし `if`／`checkout ref` は workflow YAML 自体に書かれた条件であり、
  workflow_dispatch は選択した ref の YAML 定義をそのまま実行するため、write
  権限者が別ブランチでこのガードを外した `recall.yml` を push して
  `workflow_dispatch` すれば実行境界として機能しない。加えて repo レベルの
  Actions secrets はどのブランチのどの workflow からも読めるため、YAML 内の
  if 条件だけでは `HYBRID_RECALL_MIN_*` の参照自体を防げない
  （Cursor Bugbot High 指摘）。

  そのため実際の実行境界は YAML の条件式ではなく GitHub Environments の
  ブランチ保護で作る: `recall-regression` job に `environment: recall-gate`
  を指定し、`HYBRID_RECALL_MIN_*` は repo レベルではなく environment
  `recall-gate` の secrets として設定する（参照記法は repo レベル secrets と
  同じ `secrets.*` だが、job の `environment:` 指定により解決スコープが
  environment レベルへ切り替わる）。environment `recall-gate` は deployment
  branch policy で `main` のみに制限して作成する（リポジトリ管理者作業。
  README「Recall 回帰ハーネスの repo secrets」参照）。main 以外の ref から
  起動した run は environment `recall-gate` にアクセスできないため、別ブランチの
  改変 YAML から `if`／`checkout ref` を外して `workflow_dispatch` したとしても
  environment 自体にアクセスできず閾値を取得できない。`if:
  github.ref == 'refs/heads/main'`・`checkout ref: main` は environment 保護に
  対する defense-in-depth として維持する。

### strict モードによる誤 green 防止（`HYBRID_RECALL_REQUIRE_THRESHOLDS`）

層 B の opt-in 方式（未設定＝「対象外」として成功終了）は PR 実行を想定した
設計だったが、`recall.yml` からの実行（現状 `workflow_dispatch` のみ）では
別の問題を生む: environment `recall-gate` の作成漏れ・secret 名の打ち間違い・
secret の誤削除で `HYBRID_RECALL_MIN_*` が読めなくなった場合、opt-in 方式では
「一度も評価していない run」が「基準を満たした run」と同じ green になり、閾値
ゲートが実質的に機能を失っていても気付けない（codex-review P1 継続指摘）。

そこで `crates/engine/tests/hybrid_recall.rs` に `HYBRID_RECALL_REQUIRE_THRESHOLDS`
環境変数（`"1"` のときのみ true。[`strict_thresholds_required`]）による strict
モードを追加した。`recall.yml` は Run step で常にこのフラグを注入し、strict
モード下では未設定を非数値・範囲外と同様に fail-closed でテスト失敗とする
（[`resolve_gate_threshold`]）。ローカルの `make recall-regression` にはこの
フラグを注入しないため、そちらは従来どおり opt-in 挙動を維持する。

`.github/workflows/bench.yml`（TASK-127）で codex-review に受理された前例
（CORE-5 未接続の間は `schedule` を有効化せず `workflow_dispatch` のみに限定し、
接続確認後に `schedule` を再度追加する）と同型の判断として、`recall.yml` も
一旦 `schedule` トリガを外し `workflow_dispatch` のみとしていた。Issue #168 の
オーナー判断により `schedule`（週次・月曜 04:00 UTC）を再度追加済みだが、
environment `recall-gate` の secrets 設定・strict モードでの手動実行による
疎通確認はリポジトリ管理者作業として別途必要（未実施のまま週次 run が走った
場合は fail-closed で red になる。false green にはならない。README「Recall
回帰ハーネスの repo secrets」参照）。

### 出力方針（実測値の既定非出力・Issue #303）

層 B ゲートは層 A と同一の決定的コーパス（同一 seed・件数）を測定するため、
実測 Recall は層 A の固定値定数（`hits20`/`ceil20` 等）から public に導出
可能である。失敗時（`pass=false`）は「非公開閾値 > 実測値」という上界が、
成功時は下界が推定できてしまうため、`recall@k=<実測値> pass=<bool>` の
併記による非公開閾値の逆算を防ぐ defense-in-depth・方針統一として、以下を
既定挙動とする（`crates/engine/benches/batch_bench.rs::
verbose_requested_from_env`・Issue #277〜#279 で確立した「既定出力は真偽値
のみ」方針の Recall テスト側への横展開）。

- **既定出力**: 層 B ゲート（`hybrid_recall_small_scale_threshold_gate`・
  `hybrid_recall_large_scale_threshold_gate`）・層 A 回帰
  （`hybrid_recall_small_scale_regression`・`hybrid_recall_large_scale_
  regression`）のいずれも、標準出力には対象名（テスト名・指標名）と
  `pass=<bool>` のみを含み、実測値の数値を含めない
  （`resolve_verbose`・`render_gate_line`）。
- **`RECALL_VERBOSE=1` opt-in**: ローカルでの実測値確認は明示的な opt-in に
  限定する。値は厳密一致 `"1"` のみ有効（`crates/engine/benches/harness/
  sql_c1.rs::resolve_verbose` と同一規約。空白付き値・他表記は無効側へ倒す）。
- **`GITHUB_ACTIONS` 下の拒否**: opt-in と `GITHUB_ACTIONS`（値を解釈せず
  存在有無のみ判定）が同時に立っている場合は、コーパス生成・測定の前に
  `panic!`（固定文言。数値を含まない）で fail-closed に拒否する
  （`resolve_verbose`）。
- **`recall.yml` への非注入**: `.github/workflows/recall.yml` は
  `RECALL_VERBOSE` を意図的に注入しない運用と、上記テスト側の拒否を
  二重化する（`bench.yml` の `BENCH_VERBOSE` と同型）。
- **転記禁止**: `RECALL_VERBOSE=1` で得た実測値そのものを本ドキュメント・
  PR 本文・Issue・コミットメッセージ等の public 資産へ転記しない
  （`.claude/rules/spec-confidentiality.md`）。

同種の出力方針は `crates/engine/tests/rerank_recall.rs`・
`crates/engine/tests/query_planning_recall.rs`・
`crates/engine/tests/precision_eval.rs` へも横展開済み（`query_planning_
recall.rs`・`precision_eval.rs` の既定出力は本 Issue 以前から実測値を含んで
おらず、`RECALL_VERBOSE` はローカル診断用の opt-in として方針統一のために
追加した。`precision_eval.rs` の `precision_eval_report`／
`precision_eval_policy_sweep`〔判断材料専用の常時実測値出力テスト〕は
`GITHUB_ACTIONS` 検出時に測定前 `panic!` で拒否する専用ガードを持つ）。

### コーパス・QA セットの生成

`crates/engine/tests/hybrid_recall.rs` は決定的シード付き擬似乱数（xorshift64*。
外部クレート不使用）で合成コーパスと QA セットを生成する（TASK-106 の設計を踏襲）。

- 文書の**潜在**トピック集合（`Doc::keywords`）は Zipf 近似分布（重み `1/(i+1)`）で
  語彙プールから 3〜6 語を選ぶ。この潜在集合が正解判定（inverted index・QA の
  `correct`）の唯一の情報源であり、検索特徴量（テキスト・密ベクトル）とは独立に
  扱う（次項「疎・密チャネルの lossy view」参照。PR #147 codex-review 指摘対応:
  以前は潜在集合をそのままテキスト・密ベクトル両方へ符号化しており、正解判定と
  検索特徴量が実質同一だったため全指標が機械的に 1.0 に近づいていた）
- QA セットは各文書から出現頻度が最も低いキーワード 2 語を選び、その AND 一致
  文書集合を正解集合とする `direct` カテゴリ相当のクエリ。正規化した語ペア
  （`min`/`max`）を `BTreeSet` で追跡し、同一ペアの重複登録を除外する（重複登録は
  Recall を特定の語ペアへ偏らせるため。PR #147 codex-review 指摘対応）
- 語彙数（`vocab_size`）はコーパス規模に応じて可変にする: 固定語彙表を使うと大規模
  段で正解集合が肥大化し、Recall@k の理論上限（Σmin(k,\|correct_q\|)）に対して
  文書規模とほぼ独立な比較ができなくなるため、規模に応じた語彙数（小規模: 60 語・
  大規模: 800 語）で正解集合の絞り込み度を揃えている
- `sparse.rs` の各上限（`MAX_CORPUS_DOCS`・`MAX_DOC_BYTES`・`MAX_CORPUS_BYTES`）に
  十分収まる規模で設計した（コーパス規模は環境変数から受け取らず、テスト内定数のみで
  決める。無制限アロケーション防止）

### 疎・密チャネルの lossy view（正解判定との分離）

疎検索（BM25）は「クエリ語を両方含む文書」を自然に上位へ置くが、密ベクトルを
トピック方向のランダムベクトルの平均として素朴に合成すると、無関係なトピック間の
交差項ノイズが AND 一致の信号を上回り得ることを実測で確認した（`RrfConfig::default()`
は密・疎を等重みで融合するため、密チャネルが弱く無相関だと疎チャネルの正しい順位が
押し流される）。そのため密ベクトルは語彙数と同じ次元数の one-hot 基底ベクトルの和
として合成する（`crates/engine/tests/hybrid_recall.rs::one_hot_sum`）方針を維持する。

一方、テキスト・密ベクトルのいずれも文書の潜在トピック集合（`Doc::keywords`）を
**そのまま**符号化すると、正解判定（潜在集合の AND 一致）と検索特徴量が実質同一に
なり、Recall が構造的に理論上限（1.0）へ張り付いてしまう。これを避けるため、
両チャネルを潜在集合の非完全な観測（lossy view）として独立に生成する
（`generate_corpus` のドキュメント参照）:

- 疎チャネル（`text`）: 各潜在トピックを確率 `TEXT_KEYWORD_DROPOUT_PROB` で脱落させる
  （埋め込み・索引が潜在トピックを捉え損ねる状況を模す）
- 密チャネル（`vector`）: 各潜在トピックを確率 `VECTOR_KEYWORD_DROPOUT_PROB` で独立に
  脱落させ、確率 `VECTOR_DECOY_PROB` で無関係なトピック次元を 1 つ混入させる（decoy。
  埋め込みが無関係トピックへ誤って反応する状況を模す）

これにより「疎のみ／密のみでしか見つからない正解例」「両方脱落し、どちらからも
見つけにくい正解例」が構造的に生まれる。ドロップアウト・デコイの確率はいずれも
テストハーネスの fixture パラメータであり、spec 由来の数値基準ではない
（`.claude/rules/spec-confidentiality.md`）。脱落・混入はいずれも 0/1 の one-hot
次元への操作のみで、浮動小数点の連続ノイズは加えない——スコアが常に小さい整数値の
厳密な和になるため、`ParallelSearchProvider` の行範囲分割（スレッド数）に依存する
丸め誤差や順序依存が生じず、層 A の固定値アサーションが決定的に再現できる。
クエリ側の密ベクトル・テキストは潜在集合から直接構成し、lossy view はドキュメント
側にのみ適用する（「意図が明確なクエリ」を表す簡略化。「既知の制約」参照）。

### 検索経路

production API（[`SparseIndex::build`]・[`ParallelSearchProvider`]・
`hybrid::hybrid_search` ＋ `RrfConfig::default()`）のみを使用し、BM25/RRF の再実装は
行わない。`RrfConfig::default()` は spec 採用構成（等重み・pool_depth 200・
k_const 60）に一致する。

### 指標

Recall@20（小規模段）・Recall@20/Recall@100（大規模段）は、正解文書の総数
（`total_correct`）ではなく理論上限（`ceil` = Σmin(k,\|correct_q\|)）を分母とする
到達率（`hits / ceil`）として測定する。正解集合が k 件を超えるクエリが混ざると
`total_correct` を分母にした場合に理論上の最大値が 1.0 未満へ頭打ちになり
（層 A・層 B で分母の意味が揃わなくなる問題があったため）、層 A（回帰トラッキング）・
層 B（spec 閾値ゲート）とも `ceil` を分母に統一している
（`crates/engine/tests/hybrid_recall.rs::RecallResult::recall20`/`recall100`）。
「疎・密チャネルの lossy view」の導入後は `hits < ceil`（到達率 100% 未満）が通常
であり、その `hits`/`ceil`/`total_correct`/QA 件数（重複除外後）を層 A の固定値
アサーションで回帰トラッキングする。

## 小規模段ゲート未達の engine 側原因調査（Issue #307）

- 対応: TASK-103／TASK-104／TASK-84（`docs/spec/05-tasks.md`）
- 対象ビヘイビア: SEARCH-1・SEARCH-3（`docs/spec/04-behavior/search.md`）
- 前提: 親 Issue #301。大規模段（SEARCH-2・クエリ展開結線）は別 Issue（#306）の管轄

**層 B（spec 閾値ゲート）の実測値・閾値・pass/fail は引き続き本ドキュメントにも
public テストにも記録しない（`.claude/rules/spec-confidentiality.md`）**。層 A
（回帰トラッキング）の実測値は 2026-08-29 のオーナー判断（数値基準・実測値の
public 記載許可。`.claude/rules/spec-confidentiality.md`）以降、下記「Issue #310」
節・「実測結果」節に固定値として記録する。密単体・疎単体・融合後の
Recall@20 比較（受け入れ条件 1）は `crates/engine/tests/hybrid_recall.rs::measure_channel_recall20`
を使い、融合が両チャネル単体のいずれも下回らない（＝ SEARCH-3 相当の非劣化）
ことを実測値どうしの関係アサーションとしてのみ回帰トラッキングする。

### 原因調査（受け入れ条件 1・3・4）

原因調査・検討した代替方式・オーナー判断待ち事項の詳細は非公開の内部分析に
あたるため本ドキュメントには記載しない。production の融合挙動
（`hybrid.rs::accumulate_ranked`・`rrf_fuse`）・既定値（`k_const`・`pool_depth`・
重み・BM25 パラメータ）はいずれも本 Issue の範囲では変更していない。
受け入れ条件・判断事項は spec のビヘイビア定義（SEARCH-1・SEARCH-3。
`docs/spec/04-behavior/search.md`）を参照。

## Issue #310: engine 側改善（同点順位規約・境界同点グループ）

- 対応: TASK-84・TASK-103（`docs/spec/05-tasks.md`）
- 対象ビヘイビア: SEARCH-1・SEARCH-3（`docs/spec/04-behavior/search.md`）
- 前提: 親 Issue #301。先行: #306（大規模段へのクエリ展開結線）・#307（小規模段の
  engine 側原因調査。上記節）

Issue #307 の調査（同節参照）で「原因調査対象」として残していた RRF 融合の同点
順位規約を確定した。`hybrid.rs::RrfConfig` に同点順位規約 `TieRank`
（`GroupEnd`（既定。同点グループ全員へグループ末尾の順位を割り当てる
modified competition ranking）・`Positional`（従来挙動。撤回用に残置）の
2 バリアント）を追加し、`accumulate_ranked` の同点処理を `Positional` から
`GroupEnd` へ切り替えた。あわせて `hybrid_search_boosted` の密・疎プール取得を
`fetch_k`（初期値は `pool_depth` の 2 倍）で行い、`pool_depth` 境界が同点グループの
途中を切る場合はグループ全体を含める。境界の終端が取得済み範囲内で確定できない
場合は、`fetch_k` を倍増して再取得し（`MAX_FETCH_K`・可視集合の大きさで有界化）、
終端確定を試みる（Issue #320 codex-review P1 指摘対応）。再取得の上限に達しても
なお終端確定できない場合は、境界の同点グループ全体（開始位置から観測末尾まで）を
丸ごと除外し、そのグループより上位（スコアが厳密に高い候補）だけをそのチャネルの
最終結果として確定する（`exclude_undetermined_boundary_group`。位置ベースで
`pool_depth` 件へ部分採用する・観測できた分をそのまま保持するのいずれも、拡張
取得列が常に同点 id 昇順であることに起因して最小 id 側だけが根拠なく生き残る
id 依存バイアスが残るため採らない）。除外は密・疎いずれのチャネルにも
対称に適用し、そのチャネルの結果が空になった場合でも `hybrid_search_boosted` は
もう一方のチャネルで融合を継続する（両方空なら空結果を返す。エラーにしない）。
`complete_boundary_tie_group` を追加した。

**採用した同点順位規約**: RRF 融合の同点順位処理を、位置順位（従来挙動）から
グループ末尾順位（`TieRank::GroupEnd`・modified competition ranking）へ変更
（SEARCH-1・SEARCH-3。`crates/engine/src/hybrid.rs::accumulate_ranked` の
`TieRank` 分岐。詳細は `docs/spec/04-behavior/search.md` 参照）。同時に密プール
境界の同点グループ完全化（`complete_boundary_tie_group`・`complete_boundary_
tie_group_by`）を追加し、`pool_depth` 境界が同点グループの途中を切る場合に
グループ全体を含める（終端が確定できない場合は `fetch_k` を倍増して再取得し、
再取得の上限に達してもなお確定できない場合は境界の同点グループ全体を除外する。
位置ベースの部分採用・観測できた範囲の全保持のいずれも id 依存バイアスが残る
ため採らない）。疎チャネル（BM25 スコア）についても、同一語頻度・
同一文書長で同点が発生しうるため同じ完全化ロジックを適用（`crates/engine/src/
hybrid.rs`）。
`rerank.rs::LexicalOverlapReranker::rank_fused` の `TieRank` 規約統一実装済み
（`rank_fused` を位置順位から `GroupEnd` へ揃えた。実装の詳細は
`docs/design/rerank-recall-regression.md`「実測結果」節参照）。Issue #330 で
`rank_lexical`（字句一致順位）も同じ GroupEnd 規約へ統一した
（`docs/design/rerank-recall-regression.md`「Issue #330」節参照）。

**フィクスチャ非変更**: `TEXT_KEYWORD_DROPOUT_PROB`・`VECTOR_KEYWORD_DROPOUT_PROB`・
`VECTOR_DECOY_PROB` を含む fixture パラメータ・seed・規模定数は本 Issue でも
変更していない（Issue #310 の受け入れ条件 2）。`ceil20`／`ceil100`／
`total_correct`（正解集合の理論上限・総数）が変更前後で一致することは、この
非変更の実測による裏付けでもある（下記「実測結果」節）。層 B（spec 閾値ゲート）
の pass/fail・閾値・導出係数は引き続き本ドキュメントにも public テストにも
記録しない（受け入れ条件 3・`.claude/rules/spec-confidentiality.md`）。

## Issue #320 大規模段追加調査: 非正スコア候補の順位付け除外

- 対応: TASK-84・TASK-103（`docs/spec/05-tasks.md`）
- 対象ビヘイビア: SEARCH-1・SEARCH-3（`docs/spec/04-behavior/search.md`）
- 前提: 上記「Issue #310: engine 側改善」節（境界同点グループ完全化の再取得ループ）

Issue #310 の再取得ループ（`fetch_k` 倍増による境界同点グループの終端確定）を
大規模段コーパス（100 クエリ・可視集合 20,000 件）で実測したところ、100 クエリ中
53 件で密チャネルの再取得が可視集合全体（20,000 件）まで到達していた。原因は
「クエリと一致しない文書（疎はスコア 0。密は 0 に限らずスコアが低い文書一般）」
の長い同点尾が境界同点グループとして扱われ、完全化ループがその尾を可視集合の
末端まで追い続けてしまうことにあった。

**採用した契約**: 非正スコア（`score <= 0.0`）の候補の扱いを、拡張取得列が
exhaustive（可視集合全体を覆い切っている）かどうかで分岐する
（`hybrid.rs::resolve_boundary_tie_group`。`precision` モード（SEARCH-9）の
密・疎順位不一致 → RRF 同点 → 空集合契約は exhaustive なケースでスコア 0 の
候補にも依存するため、exhaustive 時は除外しない）。

非 exhaustive 時の除外は**疎（BM25）チャネル限定**（`zero_is_no_signal` 引数）で
適用する。密（`CandidateHit::score`。内積・コサイン類似度）は 0・負値も有効な
相対順位を持ち「クエリと無関係」を意味しないため、密チャネルには非 exhaustive
時の除外を適用しない（Issue #320 codex-review P1 指摘対応。密の上位 `pool_depth`
件が全て非正スコアになるクエリ・埋め込み分布は珍しくなく、除外すると密チャネル
の候補が丸ごと消え recall が落ちるため）。BM25 は語一致が一切ない文書のスコアが
定義上 0 になるため、疎チャネルに限っては非正スコア＝無シグナルが保証される。

- **exhaustive**（密・疎共通）: 取得済み範囲がそのまま可視集合全体であることが
  確定しているため、非正スコアも含め全件をそのまま境界同点グループ完全化へ渡す
  （除外しない）。
- **非 exhaustive・疎チャネルのみ**: 境界位置（0-based で `pool_depth - 1`）の
  スコアが非正なら、そのグループは「クエリとのシグナルが一切ない」無シグナル群
  であり全メンバーが等価（ID に依存しない）とみなし、再取得ループへ進めずその場
  で非正スコアの末尾を丸ごと除外して確定する（拡張取得列は呼び出し元契約により
  スコア降順のため、非正スコアの範囲は必ず末尾の連続区間になる）。境界位置の
  スコアが正なら非正の末尾を切らずに完全化ロジックへそのまま渡す（正の境界同点
  グループは非正スコアと隣接しないため影響しない）。
- **非 exhaustive・密チャネル**: `zero_is_no_signal = false` により上記の除外を
  適用せず、常に完全化ロジック（通常の再取得ループ、上限到達時は
  `exclude_undetermined_boundary_group` による対称除外）へそのまま渡す。

切り詰めに先立つ契約検証（有限性・ソート順・重複 id。`validate_extended_pool`）は
どの分岐でも切り詰め**前**の拡張取得列全体に対して行い、この検証範囲は変更して
いない。

この分岐により、非 exhaustive な再取得ループが「クエリと無関係」と保証できる
無シグナル群（疎チャネルのスコア 0）の同点尾を可視集合全体まで追い続ける経路は
疎チャネル限定で断ちつつ、exhaustive な小規模可視集合ではスコア 0 の候補も境界
同点判定に正しく参加できるようにした。**密チャネルは非 exhaustive でもこの経路
を断たない**（0・負値が有効な相対順位を持つため除外できない。Issue #320
codex-review P1 指摘対応で密チャネル限定にこの除外を撤廃した結果、密チャネルの
再取得が可視集合全体まで到達するクエリが再び生じうる。実測は下記「検証」節
参照）。

**検証**: `crates/engine/tests/hybrid_recall.rs::
hybrid_recall_large_scale_dense_refetch_is_bounded_by_visible_set_size`が、
`ParallelSearchProvider` をラップする診断用 `SearchProvider`
（`MaxKTrackingProvider`）で密側再取得ループが要求する `fetch_k` の最大値を
クエリ単位で観測する。密チャネルは非正スコアでも除外しない契約（上記 codex-review
P1 指摘対応）のため、密側再取得ループが可視集合全体（20,000 件）まで到達する
クエリ数（`reached_visible_set`）は決定的コーパスに対する実測値として固定値
アサーション（`== 40`）で回帰トラッキングする（codex-review P1 指摘対応前は
密チャネルの非正スコア除外により 0 件、Issue #310 時点〔境界完全化導入前〕は
53 件だった）。加えて、再取得ループが常に `dense_cap`（このフィクスチャでは
`MAX_FETCH_K` > 可視集合サイズのため実質的な上限＝可視集合サイズ）を超えて
`fetch_k` を要求しない（＝無限に伸び続けない）ことも構造的な保証として固定する。
密チャネルに `crates/engine/src/hybrid.rs::
AllNegativeDenseProvider`（テスト専用）を使う単体テスト
（`hybrid_search_boosted_keeps_dense_candidates_when_boundary_scores_are_non_positive`）
で、上位 `pool_depth` 件が全て非正スコアでも密チャネルの候補が除外されず融合
結果に現れることを直接固定する。

**hits20 への影響**: 小規模段 182・大規模段 385/648（下記「実測結果」節）。
密チャネルの非 exhaustive 時除外撤廃（codex-review P1 指摘対応）後もこれらの
固定値は変化しなかった（本フィクスチャでは密チャネルの境界が非正スコアになる
クエリが、除外の有無に関わらず融合結果へ影響しなかった）。

**`crates/engine/tests/rerank_recall.rs` への影響なし**: 同ハーネスの大規模段
固定値（baseline hits20=387・pool hits100=837・pool hits200=951）はこの変更
適用前後で数値の変化がなかった（本フィクスチャでは非正スコア候補がリランキング
測定対象クエリの結果へ影響しなかった）。当時（本調査の時点）は
`after_hits20 >= baseline_hits20` の非劣化アサーションが after hits20=383 で
red だったが、本調査（可視集合全体までの再取得到達の解消）とは独立の事象
だった。この red は後日 Issue #310 対応（`LexicalOverlapReranker` の既定重み
変更）で解消済み。詳細は `docs/design/rerank-recall-regression.md`「実測結果」
節参照。

## 実測結果

（`crates/engine/tests/hybrid_recall.rs`、層 A 2/2 pass。決定的コーパスのため
再現可能。hit 数は同テストのアサーションに固定済み）

| 段 | 文書数 | QA 件数 | total_correct | ceil20 | hits20 | ceil100 | hits100 | Recall@20 | Recall@100 |
| -- | ------ | ------- | -------------- | ------ | ------ | ------- | ------- | --------- | ---------- |
| 小規模 | 400 | 60 | 202 | 202 | 182 | - | - | 0.9010 | - |
| 大規模 | 20,000 | 100 | 997 | 421 | 385 | 707 | 648 | 0.9145 | 0.9165 |

小規模段の密単体 Recall@20 は 151/202（0.7475）、疎単体は 166/202（0.8218）
であり（`crates/engine/tests/hybrid_recall.rs::measure_channel_recall20`）、
融合（182/202）はいずれの単体チャネルも下回らない。上表の小規模段 `hits20` は
上記「Issue #320 大規模段追加調査: 非正スコア候補の順位付け除外」節の契約
適用後の値。大規模段は 385/648。

以前は測定タプル全体を `DefaultHasher`（固定キー）で畳み込んだダイジェスト
1 個を固定値アサーションの対象にすることで数値非開示のまま退行検出しようと
していたが、フィクスチャ生成コード自体が本ファイル内に公開されており各
フィールドが少値域の整数に収まるため、ダイジェスト値からの総当たり復元が
現実的で数値非開示の目的を達成できないと判明した（codex-review P0 指摘・
PR #320）。2026-08-29 のオーナー判断（数値基準・実測値の public 記載許可）に
より、この制約自体が解消したため、ダイジェスト方式・関係アサーションのみへの
置換は撤回し、上記の固定値（実測値そのもの）による層 A 回帰トラッキングへ
戻した。Issue #310（`GroupEnd`＋境界同点グループ完全化）の適用前、`hits20` は
小規模段 171・大規模段 328、`hits100` は大規模段 645 であった（`ceil20`／
`ceil100`／`total_correct` はいずれも上表と同一。フィクスチャ非変更のため不変）。
両段とも `hits20`（大規模段は `hits100` も）の到達率が改善している。境界同点
グループを終端確定できない場合の契約は上記「Issue #310: engine 側改善」節の
とおり（再取得ループ、上限到達時は境界同点グループ全体を除外し厳密に上位の
候補のみ保持）。この契約での実測値が上表（大規模段 `hits20=385`／`hits100=648`）であり、`ceil20`／`ceil100`／
`total_correct` はいずれも不変のため測定前提自体は崩れていない。数値そのものの
固定値ゲートに加え、spec 由来の絶対閾値に対する pass/fail 判定は既存どおり
層 B（`environment: recall-gate` の secrets 経由）側で扱う。

疎・密チャネルの lossy view（ドロップアウト・デコイ）により、いずれの段も
Recall@k が 1.0 未満の現実的な値になっている。QA 件数はいずれも重複除外前の
クエリ候補数（小規模 60・大規模 100）と一致しており、本コーパス規模では語ペアの
重複は発生していない（コーパス規模・語彙数が変わると重複除外により QA 件数が
`num_queries` を下回る可能性がある。`generate_qa_set` のドキュメント参照）。

大規模段のデバッグビルド実行時間はローカル実測で約 4.5 秒であり、PR CI
（`cargo test`）に含めても許容範囲と判断し、層 A の両テストとも `#[ignore]` に
していない。ただし `ubuntu-latest`（GitHub Actions ランナー）での実測はまだ行って
おらず、CI 実測後に実行時間が悪化していれば `#[ignore]` 側へ移し
`.github/workflows/recall.yml` 専用にする判断を再検討する
（`crates/engine/tests/hybrid_recall.rs::hybrid_recall_large_scale_regression` の
ドキュメンテーションコメントと同方針）。なお `.github/workflows/recall.yml` は
`pull_request` トリガを持たない（「2 層構成」参照。spec 機密保持が優先）ため、
PR ごとの実行コストは層 A（layer A の `cargo test` 分のみ）に限られる。

### クエリ展開の結線（Issue #306）

大規模段の層 B（`hybrid_recall_large_scale_threshold_gate`）は、TASK-110〜113
（`docs/design/query-planning-recall-regression.md`）で確立した決定的スタブ
`LlmClient`（`query_planning_recall.rs::MockLlmClient` と同一実装。`tests/`
直下は独立 test crate で共有モジュールを持たないため `hybrid_recall.rs` へ複製）を
`EngineCore::plan_query`（TASK-110。`with_query_planner`・辞書スナップショット
〔`dictionary_snapshot` → `render_prompt_prefix`〕を経由する production のクエリ展開
結線）に通した展開ありクエリで Recall@20・Recall@100 を測定する。これにより、
SEARCH-2 が前提とする「クエリ展開あり」の測定条件に層 B の構成を揃えた。

この結線は PR #308 で 2 段階の codex-review P1 指摘対応を経て確立した:

1. 当初、層 B は QA セットを `direct` カテゴリ（コーパス語彙 `kw_XXXX` に一致する
   クエリ）のまま展開経路へ渡していたが、`MockLlmClient` はその形式の語を無変換で
   通すため、展開ありの実測値が展開なしと構造的に一致してしまい、展開結線の
   欠落・破損を検出できない問題があった（1 件目の指摘）。
   `hybrid_recall.rs::to_intent_query` でクエリテキストのみを `intent` 形
   （`syn_XXXX` 形式。コーパス語彙とは一致しない）へ書き換えたうえで展開経路に
   通すよう変更し、`MockLlmClient` の同義語写像（展開結果を検索入力へ反映する
   経路）を経由して初めて Recall が出る非恒等なゲートにした。
2. しかし (1) の時点では、展開経路自体（`hybrid_recall.rs::
   expand_and_reconstruct_with`）が `query_planner::render_full_prompt` →
   `LlmClient::complete` → `query_planner::parse_expansion` を固定接頭辞 `""` で
   直接呼ぶ手組みのままだったため、`EngineCore::plan_query`・`with_query_planner`・
   辞書スナップショットという production の結線自体が欠落・破損しても検出できない
   （2 件目の指摘・本件）。展開経路を `hybrid_recall.rs::PlannerFixture`（`path`/
   `body` 各 1 行のみの最小テーブルを持つ `EngineCore`。検索対象コーパスとは無関係で
   `plan_query` を呼ぶためだけの器）経由の `EngineCore::plan_query` へ置き換え、
   辞書スナップショットに含めたセンチネル値（`hybrid_recall.rs::
   DICTIONARY_WIRING_SENTINEL_PATH`）が展開時のプロンプトに現れない場合は
   `MockLlmClient::complete` が `Err(PlanError::InvalidResponse)` を返して展開自体を
   失敗させるよう変更した。

これにより、(1) の同義語写像と (2) の `plan_query`/`with_query_planner`/辞書
スナップショット結線のいずれが壊れても Recall 崩壊・ゲート失敗する二重の非恒等
ゲートになった。正解集合（`correct`）は変更しないため、既存の
`HYBRID_RECALL_MIN_R20_LARGE`/`HYBRID_RECALL_MIN_R100_LARGE`（Actions secrets 由来）
の較正・意味は変わらない（結線が壊れていなければ実測値は従来の `direct` 経路と
一致する）。再埋め込み（`plan_and_embed_query`）・`USING PLAN` SQL 表層は対象外の
まま（密チャネルの再構成は既存の `one_hot_sum` 契約を維持）。層 A
（`hybrid_recall_large_scale_regression`）は従来どおり `direct` 形クエリのままの
展開あり経路（`QuerySource::Expanded`。同じく `PlannerFixture` 経由）を測定対象と
し、展開なしとの hits/ceil 完全一致をパススルー回帰ガードとして維持する——この
等式が崩れた場合は展開パーサ・スタブ・再構成経路のいずれかの回帰を意味する（層 B
と異なり QA を書き換えないため、この層 A の等式・変更対象は無変更）。小規模段の
層 B・層 A の主測定は引き続き展開なし（`QuerySource::Baseline`）で行う。fixture
パラメータ・seed・規模定数（`LARGE_*`/`SMALL_*`）は変更していない。

層 B の pass/fail・実測値は本ドキュメントに記載しない（「実測値の既定非出力
（Issue #303）」節と同方針）。TASK-113（PLAN-3）が定義する `intent` カテゴリ
（言い換え語彙の QA セット）自体の大規模測定は、`to_intent_query` による疑似
`intent` 形クエリ（正解集合・fixture は `direct` カテゴリ由来のまま）とは別物であり
本結線でも対象外のまま（下記「既知の制約・スコープ外」参照）。

## 既知の制約・スコープ外

- **合成コーパスによる暫定測定**: 実コーパスでの評価は未了（TASK-106 と同種の制約）
- **密ベクトルの簡略化**: 「疎・密チャネルの lossy view」で述べた通り、one-hot
  AND 信号＋構造的ドロップアウト／デコイは埋め込みモデルの類似度分布（連続値・
  高次元の相関構造）を忠実には模倣しないため、実際の埋め込み品質の回帰検出には
  使えない。あくまで RRF 融合パイプライン自体（密・疎の統合・pool_depth・k_const の
  挙動）が、正解判定と検索特徴量が分離された非自明な入力に対しても機能することの
  回帰検出が目的
- **クエリ側は lossy view を適用しない**: ドキュメント側のみ非完全な観測にし、
  クエリは潜在集合から直接構成する簡略化のため、クエリ自体の embedding 品質劣化は
  対象外
- **fixture パラメータの経験的選定**: `TEXT_KEYWORD_DROPOUT_PROB`・
  `VECTOR_KEYWORD_DROPOUT_PROB`・`VECTOR_DECOY_PROB` は Recall を 1.0 未満の
  非退化な範囲に収めるために実験的に選んだ値であり、spec 由来の受け入れ基準では
  ない（層 B の閾値のみが spec 由来。「2 層構成」参照）
- **クエリ展開との統合測定**: 大規模段の層 B は Issue #306（PR #308 で `to_intent_query`
  による非恒等化・`PlannerFixture` 経由の `EngineCore::plan_query` 結線まで対応済み）
  で決定的スタブ `LlmClient` による展開ありの経路へ結線済み（上記「クエリ展開の結線
  （Issue #306）」節参照）。ただし QA の正解集合・
  fixture は `direct` カテゴリ由来のままで、TASK-113（PLAN-3）が定義する `intent`
  カテゴリ（言い換え語彙の QA セット）自体での大規模測定は本ハーネスのスコープ外の
  まま（フォローアップ候補。Issue 起票はユーザー判断待ち）
- Actions secrets（`HYBRID_RECALL_MIN_*`）の実値設定はマージ後のリポジトリ管理者
  作業（README「Recall 回帰ハーネスの repo secrets」参照。secret ↔ spec
  ポインタの対応表・設定手順は `docs/design/ci-gate-variables.md` に集約した。
  当初は Actions variables を使っていたが、`env:` ブロックのログ印字による
  漏えいを防ぐため secrets へ移行した（Issue #286）
- PR #320 が追加した境界同点グループ再取得ループ（本 ADR「Issue #310: engine
  側改善」節）の単発クエリレイテンシへの寄与計測は `docs/design/
  hybrid-refetch-latency.md`（Issue #324）を参照

[`SparseIndex::build`]: ../../crates/engine/src/sparse.rs
[`ParallelSearchProvider`]: ../../crates/engine/src/parallel_search.rs
[`strict_thresholds_required`]: ../../crates/engine/tests/hybrid_recall.rs
[`resolve_gate_threshold`]: ../../crates/engine/tests/hybrid_recall.rs

## Issue #2001 追記: 密側 metric 別の小規模段

親 #1996（HNSW の距離種別対応）の検証として、小規模段（400 行・60 クエリ）を密側 metric 別に
固定した。

| 経路 | 密側 metric | hits20 / ceil20 |
| ---- | ----------- | --------------- |
| in-memory `hybrid_search`（`hybrid_recall_small_scale_regression`） | 内積 | 182 / 202 |
| SQL 既定エンジン（`hybrid_recall_small_scale_cosine_dense_sql_regression`） | コサイン | 179 / 202 |

- 後者は `SqlHybridFixture` + `RecallEngine::BruteForce`（400 行は `MIN_INDEXED_ROWS` 未満で
  索引を構築しない。`builds == 0` も固定）。`total_correct`・`ceil20` は前者と同じ 202。
- HYBRID に metric を選ぶ手段は無い。SQL の HNSW 経路（`HnswDenseProvider`）の密側は内積のまま
  （#2931 の記録）で、brute-force 経路のコサインとは非対称。解消後に hnsw 系 matrix の実測を取り直す。
- 層 B ゲート（`recall.yml`）と閾値は変更していない。
