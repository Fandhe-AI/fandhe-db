# ADR: リランカーの結線方針

- ステータス: Accepted（本リポの実装判断。振る舞い契約の SSOT は spec の SEARCH-15 で、
  本書へは転記しない）
- 対応 Issue: #2141（親 #2140。後続は #2142・#2143・#2144・#2147）
- ビヘイビア ID ポインタ: SEARCH-15（`docs/spec/04-behavior/search.md`）。関連として
  SEARCH-7・SEARCH-9・SEARCH-13・SQL-6・NOSQL-2 をポインタで挙げる
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード（行番号は書かずシンボル名で示す）:
  - `crates/engine/src/rerank.rs`（`Reranker`・`rerank_candidates`・`RerankConfig`・
    `RerankError`・`IdentityReranker`・`LexicalOverlapReranker`・`CrossEncoderReranker`・
    `CrossEncoderConfig`）
  - `crates/engine/src/rerank/cross_encoder_onnx.rs`（`OnnxCrossEncoderBackend::from_files`）
  - `crates/engine/src/sql/exec.rs`（`execute_statement` の DISTANCE 段・事後 SCALAR・
    `precision::apply_gate`・`RlsSafetyNet::apply`・`sparse_docs`）
  - `crates/engine/src/sql/parser.rs`（`Ranking::{Distance, Hybrid}`）
  - `crates/engine/src/precision.rs`・`crates/engine/src/sql/explain.rs`（`engine_token`）
  - `crates/wire-server/src/search_engine_opt.rs`・`statement_timeout_opt.rs`・
    `result_limits_opt.rs`
- 関連 doc: `rerank-recall-regression.md`・`hybrid-tuning-parameters.md`・
  `explain-search-engine-exposure.md`・`hnsw-search-engine-wiring.md`・
  `limit-omitted-result-cap.md`・`result-limits-cli.md`・`nosql-explain-mapping.md`・
  `statement-interrupt.md`
- 検証コード: なし（設計記録のみ。コード変更は後続 Issue）

## 1. 背景

リランキング層は実装済みだが、`sql::exec`・`core`・`wire-server` からは参照されていない
（`precision.rs` のモジュールドキュメントにも「未接続」とある）。`rerank_candidates` は
契約違反を検索全体の拒否（fail-closed）として返すため、結線時にこれを縮退方針とどう整合
させるかが論点になる。opt-in・既定 off は公開 Issue #2140 の前提である。本書は結線の
入口・位置・件数・期限・縮退・可視化を本リポの実装判断として確定する。

## 2. 公開コードの現状

| 項目 | 現状 |
| ---- | ---- |
| rerank の候補上限 | `rerank.rs` の `MAX_POOL_DEPTH` = 10,000（非公開定数）。`RerankConfig::default()` は pool 200・final_k 20 |
| hybrid の上限 | `hybrid.rs` の `MAX_POOL_DEPTH` = 100,000。実行器の pool は `max(k_eff, 200)` |
| 結果行数 | `--max-result-rows` 既定 10,000（`DEFAULT_MAX_RESULT_ROWS`）、ハード上限 100,000 |
| クロスエンコーダ上限 | batch 256・seq_len 8192（`CrossEncoderConfig`） |
| `Ranking` の種類 | `Distance` と `Hybrid` のみ。疎だけで順位付けする SQL 形状は無い |
| `cross-encoder` feature | engine にのみあり既定無効。wire-server からは転送していない |

## 3. 決定事項

### D1 設定の入口

- 起動時 CLI だけで設定する。新規 `crates/wire-server/src/rerank_opt.rs` に置く
  （`search_engine_opt.rs` と同じ形）。
- セッション変数・SQL 構文・NoSQL キー・テナント単位の切り替えは設けない。
  根拠: #2140・#2141 の本文、および `precision.rs` の方針（外部入力から変更できない）。
- engine 側は `FromStr` を持たず、wire-server が閉じた語彙で判定する
  （`hnsw-search-engine-wiring.md` の判断を踏襲）。
- engine 側の検証は検証付きコンストラクタ 1 つに一本化し、`MAX_POOL_DEPTH` は非公開のまま
  `validate_*` を公開する（`validate_statement_timeout_ms` と同形）。注入は `EngineCore` の
  builder で 1 回だけ行う（#2142 で core-api snapshot を更新する）。

### D2 CLI フラグと値域（確定）

| フラグ | 値域 | 既定 | 条件 |
| ------ | ---- | ---- | ---- |
| `--reranker` | `off`／`lexical`／`mmr`／`cross_encoder`（閉じた 4 語） | `off` | - |
| `--rerank-window` | `1..=10000` の整数 | 200 | `off` 以外のときだけ指定可 |
| `--rerank-mmr-lambda` | `0.0..=1.0` の閉区間（厳密な 10 進パース） | 0.5 | `mmr` のときだけ指定可 |
| `--rerank-model`／`--rerank-tokenizer` | パス | なし | `cross_encoder` のとき必須 |
| `--rerank-timeout-ms` | `1..=60000` | 1000 | `cross_encoder` のときだけ指定可 |

- `--rerank-window` は `rerank.rs` の `MAX_POOL_DEPTH` と、`RerankConfig::default()`・
  hybrid 既定 pool の 200 に揃える。
- `--rerank-timeout-ms` は 0（無効）を受理しない。縮退には有限の期限が必要なため。
- 起動を拒否する（非 0 で終了）条件: 範囲外・非数値・先頭 `+` や空白付きの値・重複指定、
  `off` のままの `--rerank-*` 指定（指定が黙って無視される事故の防止。`--hnsw-*` と同方針）、
  種別に合わないフラグ、feature 無効ビルドでの `cross_encoder` 指定、モデルを読み込めない場合。
- エラー文に含めるのはフラグ名・入力値・範囲だけとする（`statement_timeout_opt.rs` と同じ流儀）。
- `identity` は CLI に出さない（テスト用に Rust API でのみ使う）。
- モデル・トークナイザのパス検証方式とサイズ上限は #2144 で決め、本書へ追記する。

### D3 挿入位置と適用形状

- 位置: `execute_statement` の DISTANCE 段（`HINT ORDER` で SCALAR が後なら事後 SCALAR
  フィルタ）の後、`RlsSafetyNet::apply` の前。`precision::apply_gate` と同じ位置とする
  （根拠: `precision.rs` のモジュールドキュメント）。
- テキストの供給元: 疎コーパスキャッシュ（`SparseIndexCache`）がヒットしたフィルタなしの
  ハイブリッド経路では `skip_sparse_accumulation` により `sparse_docs` が空のままになり、
  projection のデコードも安全網の後へ遅延される。このため `sparse_docs` を一律の供給元には
  しない。リランカーが読む候補テキストは、キャッシュのヒット・ミスに関わらず、RLS 暗黙適用後の
  可視アリーナに残った上位 W 件の候補行から、テキスト列だけをこの挿入点で都度デコードして
  得る（認可済みの行に限り、列権限を広げない）。`sparse_docs` が埋まっている場合も同じ経路を
  使い、供給元を 1 つに統一する。デコードできない行・テキストが無い行は、その行だけ縮退
  （元順位を維持）し、キャッシュヒットを理由に lexical／cross_encoder をスキップしない。
  実装（#2142）は、この都度デコードがキャッシュ高速経路の利点（全行蓄積の省略）を損なわない
  よう W 件に限って行う。
- SQL と NoSQL `search`（NOSQL-2。同じ実行器を通る）の両方に同じ扱いで適用する。

| 形状 | lexical／cross_encoder | mmr |
| ---- | ---------------------- | --- |
| `Ranking::Hybrid` | 適用（可視アリーナ上位 W 件の候補行のテキスト列を使う。キャッシュヒット時も同じ） | 適用 |
| `Ranking::Distance`（密のみ） | 文ごとに「適用外」で素通し。off と同じ結果。拒否も縮退ログも出さない。判定は文の形と設定だけで決まる静的判定 | 適用 |
| 疎のみ | SQL 形状が無く対象外 | 同左 |

- `USING MODE 'precision'` ではリランキングを適用しない。確信度閾値（`ConfidenceThresholds`）は
  dense／RRF の尺度で校正されており、別尺度のスコアに当てると誤った空集合や誤った確信を
  生むため。`precision.rs` の「リランク後のスコアでゲートする」旨の記述は #2142 で本書に
  合わせて改める。spec 側での確認事項は第 6 節に置く。

### D4 件数（`LIMIT`・`pool_depth`・`final_k`）

記号: `k_eff = LIMIT + OFFSET`、W = `--rerank-window`。

- 候補取得: 密は `max(k_eff, W)` 件。ハイブリッドは融合済みリストの先頭 `max(k_eff, W)` 件
  （上限は融合リスト長）。RRF の `pool_depth` は変えず、ハイブリッドの候補生成は off と同一とする。
  密は HNSW の探索幅が `ef.max(k)` で決まるため、`W > k_eff` で k を広げると探索経路と上位候補が
  off と変わりうる。よって密の候補集合そのものは off と同一とは保証しない（D6 の縮退で扱う）。
- リランク対象は先頭 `min(W, 候補数)` 件。`final_k = min(k_eff, W, 候補数)`。
- リランカー出力は厳密に `final_k` 件の並び替えでなければならない（欠落も契約違反）。
- 最終順位 = 出力 ++ 出力に入らなかった候補（元の順位のまま）。これを `k_eff` で切り、
  OFFSET を適用する。`k_eff > W` でも文は拒否せず（新しい SQLSTATE を作らない）、
  W より後ろは元の順位を保つ。保証するのはハイブリッドに限り「リランク対象の候補集合（融合済みリストの先頭
  `max(k_eff, W)` 件）が off と同一」までで、密検索では候補集合の同一性も保証しない
  （`W > k_eff` で HNSW の探索経路が変わりうる）。いずれも `W > k_eff` のとき元の上位 `k_eff` 件より後ろの候補が昇格しうるため、
  `LIMIT`・`OFFSET` 適用後の最終返却集合は off と異なりうる（並びだけが変わるとは限らない）。
- `LIMIT` 省略（`cap + 1` 方式。`limit-omitted-result-cap.md`）にも同じ式を適用する。
- `--max-result-rows` は従来どおり k の上限として働き、W の上限とは独立。
- MMR の計算量予算: `W × min(k_eff, W) ≤ 1,000,000`（静的な上界。実際の `final_k` は候補数で
  さらに小さくなりうるが、判定には使わない）。超える文は縮退（理由 `mmr_budget`）とする。
  判定は設定値と `k_eff` だけで行い、表の内容（候補数）に依存させない。値は #2143 のベンチ結果で
  本書を更新しうる。

### D5 期限

- `--rerank-timeout-ms` はクロスエンコーダの推論にだけ適用する。lexical と MMR は既存の
  `MAX_*` 定数と D4 の予算で有界。
- 方式: `CrossEncoderReranker` のバッチ境界（`batch_size ≤ 256`）で協調的に確認する。
  スレッドは生成せず、超過は最大 1 バッチ分。超過は縮退（理由 `timeout`）。
- `statement_timeout`／キャンセルによる文の中断は従来どおり 57014 系の既存契約が優先し、
  縮退にしない（`statement-interrupt.md`）。

### D6 縮退

- `rerank_candidates` の入出力検証は維持する。実行器の層が `RerankError` とバックエンドの
  エラーを受け、リランキング前の順位で応答する。対象: 候補外 id・重複・欠落・順序違反・
  非有限スコア、テキスト長の上限、期限超過、MMR の予算超過。
- 縮退の前後で RLS・列権限の検査は変えない。縮退時の応答は off と同一の結果とする。
  ハイブリッドは融合済みリストの先頭 `k_eff` 件が off と同一なので、それを返す。
  密は `W > k_eff` で拡張した候補の先頭 `k_eff` 件が off と一致するとは限らないため、
  off と同じ条件（`k_eff` 件）で検索をやり直して返す。再取得の追加コストは縮退時のみ発生し、
  期限（D5）の超過分には含めない。
- 根拠: #2140・#2142・#2144 の公開本文。契約は SEARCH-15 をポインタで示す。

### D7 可視化

- EXPLAIN（#2147）: `explain-search-engine-exposure.md` 決定 1 に従い静的な行のみ。
  `rerank` 系の行（種別・W・λ）を既存の行の末尾に追加し、名前は網羅的な `match` で閉じた
  語彙へ変換する。off では行を出さず、出力をバイト単位で不変に保つ。D3 の静的な適用外判定
  も静的な行で示す。NoSQL の explain への写像も #2147 で行う（`nosql-explain-mapping.md`）。
- 縮退（実行時に決まる）: EXPLAIN ではなく文の実行ログに記録する。固定の英語メッセージと、
  閉じた理由語彙（`timeout`／`backend_error`／`contract_violation`／`input_limit`／
  `mmr_budget`）、種別だけを出す。id・本文・表名・テナント情報は含めない。観測用の縮退
  カウンタを engine の統計 API に置く（#2142）。

### D8 決定性

同点は id 昇順（`rerank.rs` の既存契約）。同じ入力・設定・モデルなら同じ結果になる。
ただし期限縮退は実行時間に依存するため例外になりうる。

### D9 ビルド・依存

新しい依存は追加しない。wire-server への `cross-encoder` feature 転送とモデルの配布方法は
承認事項として第 6 節に置く（[dependency-policy](../../.claude/rules/dependency-policy.md)）。

## 4. 後続 Issue の分担

| Issue | 担当 |
| ----- | ---- |
| #2142 | D1 の engine API・D3・D4・D6・D7 のログとカウンタ・`precision.rs` のドキュメント改訂・`--reranker off/lexical` と `--rerank-window` |
| #2143 | MMR・λ・D4 の予算 |
| #2144 | cross_encoder・D2 のモデルパス検証とサイズ上限・D5 |
| #2147 | D7 の EXPLAIN 行と NoSQL explain |

## 5. セキュリティ

- 候補は RLS 暗黙適用後の可視アリーナ上位 W 件の行だけから取る（`sparse_docs` の有無に依存しない）。リランカーへ渡す
  テキストはハイブリッド検索で既に認可された列に限り、列権限を広げない。
- 縮退で行の集合は増えない。EXPLAIN は静的で件数や存在を漏らさず、ログとエラー文は固定
  文言と閉じた語彙のみとする。
- DoS 対策: W ≤ 10,000、候補テキスト長の上限（`rerank.rs`）、MMR 予算、有限の期限、
  batch・seq_len の上限。
- 設定を外部入力から変更する経路を作らない。モデルパスの検証は #2144 で定める。

## 6. 対象外・未決

- `cross-encoder` を既定ビルドや wire-server の feature に含めるか、モデルの配布方法
  （ユーザー承認待ち）。
- precision モードでの適用可否（spec 側の確認待ち）。
- クエリ単位の on／off（本書では設けない）。
- SEARCH-17 との関係（ポインタのみ）。
- モデルのサイズ上限の値（#2144）。

## 7. 実装メモ（#2142）

- 適用範囲: 実際にリランクが走るのは `Ranking::Hybrid`（recall モード）の文だけ。密のみの
  lexical と precision は「適用外」で素通しし、`RerankStats::not_applicable` だけを増やす。
  MMR・密経路の `k` 拡張・密側の再検索は #2143 の担当で、本 Issue では作らない（#2143 で実装済み。
  次項の「実装メモ（#2143）」参照）。
- 注入: `EngineCore::with_rerank(RerankSettings)`（起動時 1 回）。`RerankMethod` は
  `#[non_exhaustive]`（`Off`／`Identity`／`Lexical`／`Custom`）。`Identity`・`Custom` は
  Rust API 専用で CLI からは到達できない。CLI は `--reranker off|lexical` と
  `--rerank-window`（`mmr`・`cross_encoder` は語彙として受理した上で「未対応」の起動エラー）。
- 取得件数: RRF 設定は常に外側の `k_eff` から作り、取得件数 `k_fetch = max(k_eff, W)`
  （上限は `pool_depth`）だけを広げる。`hybrid_search_inner` は `k` を `InvalidK` 判定と最後の
  `truncate` にしか使わないため、先頭 `k_eff` 件は off と一致する（D6 の縮退の根拠）。
- 候補 id はアリーナのスロット番号（融合スコア降順・同点スロット昇順の入力契約が成り立ち、
  TABLE-12 の重複 `id` でも `DuplicateId` にならない）。
- テキスト: 窓内の行だけを、キャッシュのヒット・ミスに関わらず挿入点で都度デコードする
  （スナップショットの metadata、または行テーブルをテナント・行キーで引く。キーとヘッダの
  tenant 整合も検査）。`needed_column_indices` にはテキスト列を足さない（eager 経路の複製量が
  増え、off で成功する文が `54000` 相当になりうるため）。1 件 1 MiB・累計 64 MiB を確保前に
  検査し、超過は `input_limit` で縮退する。
- テキストが NULL・デコード不能の行は、その行だけ窓内の元の位置に固定する。リランカーへは
  テキストのある行だけを渡す（空文字で代用すると順位が動くため採らない）。
- 出力は `rerank_candidates` の検証に加え、件数が `final_k` ちょうどであることを実行器が検査する
  （欠落も `contract_violation`）。
- スコア: 並べ替えだけを行い、各行のスコア（`hybrid_score()` 等）は融合スコアのまま保持する
  （SEARCH-17 (1) のスコア一致を保つ）。このため `W > k_eff` の昇格があると、返却スコアは
  単調降順にならないことがある。
- 縮退: 検索全体は拒否せず、事後フィルタ前に控えた「off の先頭 `k_eff` 件」のスロット集合で
  絞り直して off と同一の列に戻す。固定の英語 1 行（理由・種別のみ）を実行ログへ出し、理由別
  カウンタ（`EngineCore::rerank_stats`）を増やす。`statement_timeout`／キャンセルは 57014 が
  優先で縮退にしない。`timeout` は #2144 で到達し、`mmr_budget` は語彙のみで #2143 で到達する。
- EXPLAIN は変更しない（off ではバイト単位で不変。行の追加は #2147）。
- 性能（受け入れ 1。off の無劣化）: 同一マシンで base（main）と head を
  `cargo bench --bench hybrid_profile_bench -p fandhe-db-engine --features bench-internals` で
  比較した（25,000 行・dim 128・共有環境の参考値）。`sql_hybrid` は p95/median
  4407/4062 us（base）→ 4328/4018 us（head）、`sql_dense_knn` は 948/889 us → 930/879 us で、
  ノイズ幅に収まる。bench 末尾の B1/B4 id 集合不一致の検査は base でも同じく失敗する
  既存事象で、本変更の影響ではない。

## 8. 実装メモ（#2143）

MMR リランカーと、それに伴う密経路の結線（D3・D4・D6・D8 の実装）。

- 関連度: 入力位置から導く順位ベースの値 `1 - i/(n-1)`（`n = 1` は 1.0）。融合スコアの値そのものは
  使わないため、λ=1 は元の順位を厳密に再現する（min-max 正規化で起きる浮動小数の衝突が無い）。
- 類似度: 埋め込み同士のコサイン類似度。ノルム² は候補ごとに 1 回だけ前計算し、対ごとに
  `kernel::dot` と `kernel::cosine_distance_from_parts` を使う（依存追加なし）。
- 選択: 1 件目は入力先頭に固定（選択集合が空の段は類似度項が全候補で 0 になり、λ=0 で全件が
  同点になるため）。2 件目以降は `λ·rel − (1−λ)·max_sim` の最大、同点は候補 id（アリーナのスロット
  番号）昇順（D8）。`max_sim` は選択のたびに増分更新し、内積の計算回数は約 `n × final_k`。
  スロット番号は主キーと一致しない場合がある（TABLE-12）が、#2142 と同じ方針でキーに使う。
- 出力スコア: 貪欲法の MMR 値は単調にならないため、`rerank_candidates` の出力検証を通す人工の
  狭義単調減少値（`final_k - 位置`）を返す。実行器は融合スコア（密は元の距離）のまま応答する。
- 入力種別: `RerankMethod::required_input()` が `Text`（lexical 等）か `Embedding`（`Mmr`・
  `CustomEmbedding`）かを返す。`Embedding` では行テーブルのテキストをデコードしない。NULL・次元不一致・
  ノルム 0／非有限の行は、テキスト無しの行と同じく窓内の元の位置に固定する。
- 予算: `mmr_budget_allows(W, k_eff)` を取得の前に判定する。超過は `mmr_budget` で縮退し、窓を広げず
  リランクもしないため、結果は off と同一（`off_prefix`・再検索は不要）。参考計測（共有環境）として
  W=1000・final_k=1000・dim 768 の MMR は約 80 ms（予算の上限付近）。D4 の値は変えていない。
- 密のみ: 距離は昇順で `rerank_candidates` のスコア降順契約に合わないため、位置由来の人工スコアで
  リランカーへ渡し、結果は元の距離へ戻す。取得幅は `max(k_eff, W)`（上限 `MAX_SEARCH_K`）。反復
  スキャンは目標件数と初期ラウンドを広げ、`iterative_scan = off` は `ef_search` が頭打ちになるため
  窓は実質 `ef` までになる。取得幅を変えるのは MMR が有効な密の文だけで、off と字句一致系は従来のまま。
- 密の縮退: 広げた取得の先頭 `k_eff` 件は HNSW で off と一致する保証が無いため、縮退時は DISTANCE 段
  （事後 SCALAR フィルタを含む）を `k_eff` でやり直す（D6）。2 周目のキャッシュ観測は重ねて数えず、
  実行統計（EXPLAIN ANALYZE 相当）の Distance 段は再検索の分だけ追加で記録される。再検索の前後で
  57014 の確認点を置き、期限・キャンセルを縮退より優先する（D5）。
- CLI: `--rerank-mmr-lambda` は `^[0-9]+(\.[0-9]+)?$` だけを受理し（符号・空白・指数・`nan`／`inf`・
  `.5`／`1.`・全角を拒否）、範囲 `0..=1` は engine の `validate_mmr_lambda` で判定する。`mmr` 以外での
  指定は起動エラー。
- 破壊的変更: `RerankCandidate` への `embedding` 追加、`RerankError` への `InvalidEmbedding` 追加
  （`CHANGELOG.md` の 0.2.0 参照）。
- NoSQL `search` は同じ実行器を通るため追加の結線は無い。EXPLAIN の rerank 行は #2147 の担当で変更しない。

## 9. 実装メモ（#2144）

- 結線: `--reranker cross_encoder` は `--rerank-model`・`--rerank-tokenizer`（必須）と
  `--rerank-timeout-ms`（`1..=60000`、既定 1000）を伴う。判定順は、語彙、フラグの組み合わせ、
  窓幅・期限の書式、ビルドの対応可否、ファイルの読み込みで、前半は feature の有無によらず同じ結果になる。
- ビルド構成: `wire-server` の `Cargo.toml` は変えていない。`#[cfg(feature = "cross-encoder")]` は
  engine 側だけに置き、wire-server は `engine::rerank::cross_encoder_available()` で実行時に分岐する。
  有効化は `--features engine/cross-encoder` で行う。**第 6 節の wire-server への feature 転送と、
  既定ビルドへ含めるかどうかは、本変更では解決していない（ユーザー承認待ちのまま）**。
- パス検証（D2 の持ち越しを確定）: `..` を含むパスを拒否し、通常ファイルであることをパスと open 後の
  ハンドルの両方で確かめ、`take(上限 + 1)` で有界に読む（TOCTOU・無限ストリーム対策）。エラーは
  パスを含まない閉じた固定語（`parent_dir_component`・`not_regular_file`・`too_large`・`unreadable`）。
- サイズ上限（自動運転下の暫定判断で、**人間による確認が必要**）: モデル 2 GiB（protobuf の単一ファイル
  上限。外部データ形式の ONNX は非対応）、トークナイザ 64 MiB。`batch_size` 32・`max_seq_len` 512 も同様。
  読み込み中はモデルの約 2 倍のメモリを一時的に使う（ort が内部でコピーするため）。
- 期限（D5）: `CrossEncoderReranker::with_timeout` が `rerank` の入口から計測し、各バッチの後に超過を
  検査する。超過は `CrossEncoderError::Timeout` で、実行器は `degraded_timeout` として縮退する
  （検索は拒否しない）。スレッドは作らず、超過幅は最大 1 バッチ分。
- 検証: スタブバックエンドで決定的に確かめた。実モデルでの起動は `ORT_DYLIB_PATH` とモデルが要る手動手順で、
  本変更では未検証。
