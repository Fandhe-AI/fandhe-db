# ADR: ハイブリッド調整パラメータの公開方式

- ステータス: Proposed（ポインタ表記のみの記録。方針・契約は spec 側を正とし、本書では確定しない）
- 対応 Issue: #2118（親 #2117。後続は #2119・#2120・#2121・#2122）
- ビヘイビア ID ポインタ: SEARCH-13（`docs/spec/04-behavior/search.md`）。関連として SQL-4・
  NOSQL-2・SQL-12・SQL-34・SEARCH-17 をポインタで挙げる。契約の本文は spec を正とし、
  本書へは転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード（行番号は書かずシンボル名で示す）:
  - `crates/engine/src/hybrid.rs`（`RrfConfig::new`・`MAX_POOL_DEPTH`）
  - `crates/engine/src/sparse.rs`（`DEFAULT_K1`・`DEFAULT_B`・`with_params`）
  - `crates/engine/src/sql/allowlist.rs`（`parse_order_by_function_args`・`FunctionArg`）
  - `crates/engine/src/sql/parser.rs`（`Ranking::Hybrid`・束縛）
  - `crates/engine/src/sql/exec.rs`（`DEFAULT_HYBRID_POOL_DEPTH`、`RrfConfig::new(60.0…)` の
    箇所、precision 正規化の `RrfConfig::default()` の箇所、`map_hybrid_error`）
  - `crates/engine/src/precision.rs`（`rrf_normalized`）
  - `crates/engine/src/sql/params.rs`（HYBRID の引数位置の `$n` 拒否）
  - `crates/engine/src/sql/explain.rs`（`hnsw_params:` 行）
  - `crates/wire-server/src/http/query/schema.rs`（`HYBRID_SCHEMA`・`SchemaError` → 42601）
- 関連 doc: `hybrid-recall-regression.md`・`rrf-tie-break-determinism.md`・
  `hybrid-rrf-phase6-before-after.md`・`result-limits-cli.md`・
  `explain-search-engine-exposure.md`・`nosql-explain-mapping.md`・`sparse-index-cache.md`
- 検証コード: なし（設計記録のみ。コード変更は後続 Issue）

## 1. 背景・目的

RRF の定数・密と疎の重み・プール深さと、BM25 の `k1`・`b` は固定値で、SQL・NoSQL の
どちらからも指定できない（現状は次節）。公開方式の契約は SEARCH-13 を正とし、本書へは
転記しない。本書は公開コードの現状と、SEARCH-13・後続 Issue へのポインタだけを記録する。
spec 由来の具体的な方針・契約の公開は、オーナー承認を得るまで行わない。

## 2. 公開コードの現状

| 項目 | 現状 |
| ---- | ---- |
| RRF 定数・重み | `RrfConfig::new(60.0, 1.0, 1.0, pool_depth)` を実行器が固定で生成 |
| プール深さ | `max(limit + offset, 200)`（`DEFAULT_HYBRID_POOL_DEPTH`） |
| BM25 | `k1 = 1.2`・`b = 0.75`（`DEFAULT_K1`・`DEFAULT_B`） |
| `FunctionArg` | Ident と StringLiteral のみ。数値・名前付き引数を表せない |
| 字句 | `=>` トークンがない |
| `RrfConfig::new` | 重み 0 を拒否する（#2120 で片側 0 を受理に変更） |
| NoSQL スキーマ | `SchemaError` はすべて 42601 に写る |
| precision の正規化 | 既定の `RrfConfig::default()` を固定で使う |

## 3. 他 DB の比較（公開ドキュメントの範囲）

| 製品 | 該当する設定 | 本 ADR との違い |
| ---- | ------------ | ---------------- |
| Elasticsearch | `rank_constant`・`rank_window_size` | 窓が size 未満だとエラー。本リポの扱いは SEARCH-13 を参照 |
| Qdrant | 融合の重み | 重みは検索ごとの指定 |
| SQLite FTS5 | `bm25()` の列重み | 関数引数で指定。文ごとの指定という点で同じ流儀 |
| PostgreSQL | 名前付き引数 `name => value` | 本リポの採否は SEARCH-13 を参照 |

## 4. 決定事項

本書では設計方針を確定しない。公開方式の方針・契約は SEARCH-13（`docs/spec/04-behavior/search.md`）
を正とし、実装の分担は後続 Issue（#2119 構文・束縛、#2120 実行器、#2121 BM25、#2122 NoSQL）で扱う。

## 5. 後続 Issue

| Issue | 範囲（ポインタのみ） |
| ----- | -------------------- |
| #2119 | 構文・束縛 |
| #2120 | 実行器 |
| #2121 | BM25 |
| #2122 | NoSQL |
| #3051 | EXPLAIN への調整値表示 |

## 6. セキュリティ

セキュリティ上の要件（インジェクション・テナント境界・DoS・エラー経由の漏えい）は
SEARCH-13 と [security](../../.claude/rules/security.md) を正とし、後続 Issue の実装・レビューで検査する。
本書では方針を確定しない。

## 7. 対象外・未決

- EXPLAIN への調整値表示は #3051 で対応（§12）。当初は #2117 配下に担当 Issue がなかった
- 片側ごとの窓など spec の解釈確認が要る点は spec 側（SEARCH-13）で扱う
- `$n` 対応は別 Issue
- spec 由来の具体的な方針・契約を本書へ記載するかは、オーナー判断を待つ

## 8. Issue #2119 での実装（構文・束縛）

SQL 表層の入口だけを実装した。実行器への反映は後続 Issue で行う。

- 字句: `=>` を 1 トークン（`Token::NamedArgArrow`）にする。`=` の直後に空白なしで `>` が続く場合だけ融合し、`= >` は従来どおり 2 つの `Punct`
- 構文: `HYBRID`／`hybrid_rrf` の位置引数（2 引数形・4 引数形）の後ろにだけ `name => value` を置ける（`FunctionArg::Named`）。値は数値（単項 `-` 可）または文字列リテラルで、識別子・式・`$n` は `42601`。名前の重複・名前付き引数の後の位置引数・`:=` も `42601`。名前の個数は 5 まで。引数名は列参照として記録しない
- 束縛: `sql::hybrid_tuning::HybridTuning` が引数名（`k1`・`b`・`rrf_k`・`window`・`weights`。大文字小文字を区別しない）と値の範囲・型を検証する。範囲外・型の不一致は `22000`、未知の名前は `42601`。`Ranking::Hybrid` が `tuning` として保持する。NoSQL（#2122）は `HybridTuning::new` で同じ検証を再利用する
- `weights` の SQL 形: 文字列リテラルの JSON `'{"dense": <数>, "sparse": <数>}'`（キーはこの 2 つちょうど）。SEARCH-13 に SQL 形の定めがないため本実装で決めた解釈で、spec 側での確認を要する
- 暫定ゲート: 実行器が束縛値を使うまで、既定以外の `tuning` を持つ文は実行と EXPLAIN で `0A000` にする（指定が黙って無視される fail-open の防止）。#2120・#2121 で撤去する
- #2120 へ渡す事項: `window` の `--max-result-rows` 上限の検査、`RrfConfig::new` が重み 0 を拒否する点との整合（束縛は片側 0 の重みを受理する）、ソフトブーストの上限計算との整合

## 9. Issue #2120 での実装（実行器）

`rrf_k`・`window`・`weights` を実行器に接続した（SEARCH-13）。`k1`・`b` は #2121 の担当。

- 組み立て: `sql::hybrid_tuning::hybrid_rrf_config` が `RrfConfig` を 1 か所で作り、融合と precision 正規化が同じ値を使う。未指定は従来値（60・等重み）で、`window` 未指定の `pool_depth` は従来どおり `max(k_eff, 200)`。調整引数なしの文は結果が従来とバイト一致する
- `window` の検証（すべて `22000`・クランプなし）: 束縛段（`LIMIT + OFFSET` 以上、`--max-result-rows` 以下）、`EngineCore::check_bound_search`（外部で組み立てた計画への多層防御）、実行時（SCALAR 事後フィルタ経路などで `k_eff` が `window` を超える場合の最終防衛。自テナントの可視件数だけに依存し、文言は固定）。`USING MODE 'precision'` は `k_eff` がデータ件数に依存し束縛段で大小を決められないため、`window` との併用を束縛段で `0A000` 明示拒否する。重み 0 のチャネルがあると保証下限が 0 になり、ソフトブーストは常に `BoostSoftBoundExceeded` で拒否される（fail-closed）。`LIMIT` 省略と `window` の併用も拒否する
- `RrfConfig::new`: 重みを「有限かつ 0 以上、両方 0 は不可」に緩めた（受理範囲の拡大のみ）。重み 0 の側は寄与 0 になり、同点は id 昇順で決定的
- 極端値: スコアが非有限になる指定（巨大な重みなど）は `XX000` ではなく `22000`。巨大な `rrf_k` は順位差が潰れ、所属リスト数と id 昇順で決まる
- 暫定ゲート: `k1`／`b` を持つ文だけ `0A000` のまま（#2121 で撤去。§10）。EXPLAIN は `rrf_k`・`window`・`weights` を受理するが、実効値の表示は対象外（#3051 で対応。§12）

## 10. Issue #2121 での実装（BM25）

`k1`・`b` を疎検索のクエリ時の計算へ接続し、`0A000` の暫定ゲートを撤去した（SEARCH-13 (4) のポインタ）。

- 型と経路: `sparse::Bm25Params`（検証付き。`k1` と `b` は private）を新設し、`SparseIndex::score_within_with_params` が索引を変えずにスコア式の係数だけを差し替える。`hybrid::hybrid_search_with_bm25` が sql 層から渡す。既存の `score_within`・`hybrid_search`・`hybrid_search_boosted` は索引自身の値を使い、結果はビット一致する
- 索引は不変: 文書統計（N・avgdl・df）・postings・`SparseIndexCache` のキーと世代に `k1`／`b` は入らない。SQL 経路の索引は常に既定値で構築・キャッシュし、`k1`／`b` が違うクエリでも同じ索引を再構築なしで再利用する。文ごとの値を `with_params` に焼き込むとキャッシュ索引へ混入するため禁止（`sql/exec.rs` のコメントに明記）
- 解決: `sql::hybrid_tuning::hybrid_bm25_params` が未指定の項目を索引自身の構築時の値で補う。指定なしの文は従来とバイト一致する
- 範囲外と極端値: 範囲外は束縛段で `22000`。`Bm25Params::new` は `k1` の数値安全上限 `1e200` を超える指定も拒否する（有限でも巨大な `k1` は分子・分母が桁あふれして NaN となり候補が黙って落ちる、または `inf` スコアで `XX000` になるため）。上限は 1 文書の tf・文書長の上界と `len/avgdl` の上界から導いた、データに依存しない定数で、拒否の有無から他テナントの存在は推測できない。さらに `score_within_with_params` は非有限スコアを検出すると `SparseError::InvalidParams` で fail-closed にし、sql 層で固定文言の `22000` に写す。spec は `k1` の上限を定めていないため、上限は実装上の数値安全上限という解釈であり、spec 側（SEARCH-13）での確認を要する
- テナント境界: 統計は従来どおり可視集合へ縮約して計算し、`k1`／`b` はスコア式の係数だけに作用する
- 対象外: EXPLAIN への実効値の表示（EXPLAIN は `k1`／`b` を受理する。#3051 で対応。§12）、NoSQL の `hybrid` オブジェクトの `k1`／`b`（#2122）

## 11. Issue #2122 での実装（NoSQL）

NoSQL `search` の `hybrid` オブジェクトに調整キー `k1`・`b`・`rrf_k`・`window`・`weights`（`{"dense","sparse"}`）を追加した（NOSQL-2・SEARCH-13 のポインタ）。

- 経路: SQL テキストは組み立てず、`weights` も JSON 文字列の再パースを通さず構文解析済みの値から読む。`HybridTuning::new` を呼んで `BoundStatement` の `Ranking::Hybrid.tuning` に入れるため、同じ値を与えた NoSQL と SQL の束縛結果は一致する（束縛段のパリティテストと HTTP 経由のバイト一致テストで固定）
- 検証は 3 層: (1) 型・未知キー・`weights` の形は `HYBRID_SCHEMA`／`HYBRID_WEIGHTS_SCHEMA` とアクセサ（`42601`）、(2) 値の範囲は engine の `HybridTuning::new`（`22000`。範囲を wire-server に複製しない）、(3) `window` と `LIMIT`・`--max-result-rows` の関係、`window` と precision の併用（`0A000`）、`k1` の数値安全上限は engine の `check_bound_search`（通常実行と EXPLAIN の両入口）。調整値は `vector` の値検証より先に検証し、SQL と同じエラー優先順位にした
- `window` は整数のみ受け付ける。小数・負数・指数表記は `42601`。`usize` に収まらない値は飽和させて範囲外（`22000`）として拒否し、切り捨てない
- SQL との差分: 型・形の不正の分類だけが異なる。SQL の名前付き引数は型不一致も `22000` だが、NoSQL は NOSQL-8 の一般則（型の不一致は `42601`）に従う。値の範囲の分類と正常値での結果は一致する。SEARCH-13 との整合は spec 側での確認事項
- テナント境界: 調整値はスコア計算と融合だけに作用し、可視集合を変えない。エラー文言は固定で入力値を含まない
- 対象外: EXPLAIN への調整値の実効値の表示（#3051 で対応。§12）、`hybrid.text` の疎側列の選択

## 12. Issue #3051 での実装（EXPLAIN）

融合方式の表示は `hybrid_rrf:` 行が担う（融合は RRF のみ）。Issue #2147 では新しい行を足さず、リランカーの `rerank:` 行だけを追加した（[rerank-wiring-policy.md](rerank-wiring-policy.md) 第 10 節）。

`EXPLAIN SELECT ... ORDER BY HYBRID(...)`（SQL の `hybrid_rrf(...)` 形と、NoSQL `search` の `hybrid` を伴う
`explain: true` を含む）の検索計画に、`scalar_plan:` の後ろへ次の 2 行を追記する（TEXT・FORMAT JSON 共通。
VERBOSE／ANALYZE でも同じ行が載る）。非 hybrid の文と `USING PLAN` の EXPLAIN は出力を変えない。

```text
hybrid_rrf: rrf_k=<f>[<src>],window=<w>[<src>],dense_weight=<f>[<src>],sparse_weight=<f>[<src>]
hybrid_bm25: k1=<f>[<src>],b=<f>[<src>]
```

- `<src>` は閉じた語彙 `default`／`specified`。`weights` は 1 つの引数なので dense／sparse に同じ値を付ける。
  指定値が既定値と等しくても `specified` と表示する
- `<f>` は `{:?}` の往復可能な最短表記（`60.0`・`1.2`・`1e200`）。`-0.0` は `0.0` に正規化する
- `<w>` は整数。precision かつ DISTANCE 先行（SCALAR 事後フィルタ）で未指定のときだけ、取得件数が可視件数に
  依存するため固定トークン `dynamic` を出す（テナント間で出力が一致する。P0）
- 値は実行器と同じ解決関数（`hybrid_bm25_params`・`hybrid_rrf_config`）と、取得件数の静的導出
  （`sql::exec::static_fetch_k`）から求め、既定値をハードコードして再導出しない。BM25 の行は疎索引の有無にかかわらず出す
- 不正な引数は実行時と同じ `wire_code` で EXPLAIN も失敗する（成功する経路を新設しない）
- `USING PLAN` の EXPLAIN は調整引数を持てず、公開関数 `build_explain_result` の出力契約を保つため対象外にした
- 書式（`{:?}`）と指定元の語彙は本実装で決めた解釈で、spec 側（SEARCH-13）での確認事項
