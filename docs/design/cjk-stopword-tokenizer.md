# ADR: BM25 トークナイザの CJK ストップワード除去

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-105（対象ビヘイビア: SEARCH-5）
- 前提: TASK-102（`crates/engine/src/sparse.rs` の BM25 疎検索カーネル。PR #138 でマージ済み）
- 関連: TASK-103・TASK-104・TASK-106

## 背景

対応: TASK-105（対象ビヘイビア: SEARCH-5。詳細は `docs/spec/05-tasks.md`・
`docs/spec/04-behavior/search.md` を参照）。判断理由・受入基準は spec 側の
定義を SSOT とし、本 ADR には転記しない。

## 実装内容（コードから読める事実の要約。判断理由は TASK-105・SEARCH-5 参照）

1. 中黒 `・`（U+30FB）を `is_cjk_char` の対象から除外（非トークン文字扱い）。
   長音符 `ー`（U+30FC）は CJK 扱いのまま保持。
2. ユニグラムが `CJK_STOPWORD_UNIGRAMS`（`crates/engine/src/sparse.rs` 内の
   定数配列）に一致する場合、そのユニグラムを出力しない。バイグラムは対象外。
3. `tokenize(text)` の既定を除去 ON とし、除去有無を選べる
   `tokenize_with_options(text, remove_stopwords: bool)` を追加した。
   `SparseIndex` の公開 API（`build`/`with_params`/`search`）は `tokenize()`
   のみを使う。

## 測定・記録（本リポジトリ内スコープ）

`crates/engine/tests/sparse_stopwords.rs` に合成日本語ミニコーパスを定義し、
除去 ON/OFF によるランキング品質の相対比較を行うテストを置く。大規模コーパス
での実測は TASK-106（並行フォローアップ）のスコープとする。

## 影響

- `crates/engine/src/sparse.rs`: `is_cjk_char` の中黒除外・`CJK_STOPWORD_UNIGRAMS`・
  `tokenize_with_options` 追加・`tokenize` の既定除去 ON 化
- `crates/engine/tests/sparse.rs`（既存 TASK-102/SEARCH-1,3 テスト）: 無変更で
  通ることを確認済み（トークナイザ既定変更のリグレッションなし）
- `crates/engine/tests/sparse_stopwords.rs`（新規）: SEARCH-5 対応テスト

## 既知の制約

`CJK_STOPWORD_UNIGRAMS` は文字一致のみで判定するため、その 1 文字自体が内容語
であるケース（例: 単漢字の当て字）は用法を区別できず除去対象になる。回避手段は
`tokenize_with_options(text, false)` の直接利用（`SparseIndex` は非対応）。

## スコープ外

- 大規模コーパスでの実測（TASK-106。実測結果は
  `docs/design/cjk-tokenizer-impact-ja-corpus.md` を参照）
- RRF 融合（TASK-103）・評価ハーネス回帰テスト化（TASK-104）
- 2 文字以上の複合助詞のトークンレベル除去
- フレーズ内でのストップワードの扱いは [fulltext-query-syntax.md](fulltext-query-syntax.md) の D4（Issue #2112）で決める

## 参照

- `docs/spec/05-tasks.md`（TASK-105・TASK-102・TASK-103・TASK-104・TASK-106）
- `docs/spec/04-behavior/search.md`（SEARCH-5）
