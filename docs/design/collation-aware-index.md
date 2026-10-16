# ADR: 照合込みのスカラー二次索引（Issue #2191）

- ステータス: Implemented（Issue #2191。collversion の扱いはオーナー確認事項。第 6 節）
- 対応: Issue #2191（親 #1607。前提 #1609・#1610・#2189・#2190）
- 関連ビヘイビア: TABLE-28（ポインタ: `docs/spec/04-behavior/`。本文は転記しない。[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連 doc: [scalar-secondary-index.md](scalar-secondary-index.md)、[index-ddl-declaration.md](index-ddl-declaration.md)、[index-declaration-effects.md](index-declaration-effects.md)、[explain-search-engine-exposure.md](explain-search-engine-exposure.md)、[icu-collator-adoption.md](icu-collator-adoption.md)

## 1. 背景

Issue #1610 までは、スカラー二次索引（`sql::scalar_index::ScalarIndex`。`(table, ctx)` ごとにメモリ上で作る派生キャッシュ）は辞書をバイト列順で並べていたため、実効照合がバイト列順でない TEXT 系列（ICU 列）を丸ごと未索引にしていた。その結果、ICU 列の `=`・`IN` も必ず全走査になり、`EXPLAIN` は `index_equality` を出すのに実行時は縮退する矛盾（D5）もあった。

## 2. 決定

### 2.1 辞書は索引の照合順で並べる

- `TextColumnIndex` が辞書の照合（`collation`）と、構築時の列の実効照合（`column_collation`。宣言的述語の式の照合）を持つ。
- 索引の照合: 自動（`Auto`）は列の実効照合。起動時 opt-in の宣言（`Declared`）は宣言の `COLLATE`。照合指定のない宣言は列の実効照合に従う。
- 同じ列に、解決後の照合が異なる宣言が複数ある場合は、構築時に 1 つへ決める（1 列に辞書は 1 本）。列の実効照合と同じ順序の宣言があればその照合、なければ索引名の昇順で最初の宣言の照合。決定は宣言の追加順・カタログの走査順に依らない（`DeclaredScalarColumn::resolved_collation`）。作成時には拒否しない。
- 構築はバイト列順で蓄積して重複を除いたあと、異なり値だけを `Collation::compare`（等しいときバイト列で二次比較する全順序）で並べ替える。比較回数は異なり値数 d に対して O(d log d)。ソートキーは作らない（文字列本体は移動するだけ）。一時領域は予算検査してから確保する。
- 宣言の照合が構築に効くのは、現行契約（[index-declaration-effects.md](index-declaration-effects.md)）どおり起動時 opt-in のときだけ。

### 2.2 使用可否の規則（単一情報源 `text_index_supports`）

| 述語 | 使える条件 |
| ---- | ---------- |
| 等価・`IN`（宣言的述語） | 索引の照合と式の照合が同じ順序。式は裸の列なので列の実効照合。`C` と `POSIX`（バイト列順同士）は同値 |
| `LIKE 'x%'`（前方一致） | 索引の照合がバイト列順（ICU 順の辞書ではバイト列の前方一致が連続しない） |
| 式レーンの比較（`col COLLATE "x" = 'v'`・`TextCompare`） | 従来どおり索引を使わない（変更なし）。照合の一致・不一致の判定は `EXPLAIN` の理由表示にだけ使う |

実行時（`ScalarIndex::candidates_for`・`filter_column_is_indexed`）と静的判定（`scalar_plan_under_target_with_reason`）が同じ関数を通るため、`EXPLAIN` と実行時の経路は一致する。述語が 1 つでも使えなければ、実行時は全走査へ縮退する（既存契約）。

### 2.3 `CREATE INDEX ... (col COLLATE "x")`

- 構文: 列リストの各要素は `<識別子> [COLLATE <照合名>]`。照合名は固定の許可リストの完全一致で解決し、未知の名前は構文段で `42704`（カタログを参照しないため存在オラクルにならない）。二重の `COLLATE` は `42601`。式索引は従来どおり `0A000`。
- 照合を持てない列（`id`・数値・`VECTOR`・`BOOLEAN` 等）と `USING hnsw` への `COLLATE` は `42804`（PostgreSQL と同じ）。判定はカタログ段（`Storage::create_index`）で、種別・列型の検査より前に行う。
- カタログ: `IndexDef.collations`（`columns` と同じ長さ）。照合を明示した列が 1 つもない宣言は従来の `v1`（バイト単位で同一）で書く。1 つでもあれば `v2`: 4 行目までは `v1` と同じで、5 行目に列と同じ数のトークン（`-` は指定なし、それ以外は `Collation::catalog_token`）をカンマ区切りで並べる。デコードは両方を読み、未知トークン・要素数の不一致・すべて `-` の `v2`（非正準形）・`hnsw` への照合は `CorruptSchema` で拒否する（fail-closed）。旧バイナリは `v2` を未知の版として拒否する（前方互換なし。#1609 の列 `param` と同じ方針）。
- `ALTER COLUMN TYPE` で、照合を明示した索引宣言のある列が照合を持てない型へ変わる経路は存在しない（受理される型変更は TEXT／VARCHAR／CHAR の間だけで、いずれも照合を持てる）。そのため専用のガードは置かない。

### 2.4 `EXPLAIN` の縮退理由

`scalar_plan:` 行の末尾に `reason=collation_mismatch` を追記する（`index=` と同じ形式。行の追加・順序変更はしない契約を守る）。最終的な計画が `plain_scan` で、照合の不一致を 1 つ以上検出したときだけ付く（宣言的述語の降格、または式レーンの `=` 比較の不一致）。語彙は固定で、値・他テナントの存在情報を含まない。`ScalarPlan` の variant は増やさない（閉じた語彙）。

### 2.5 UNIQUE・外部キー・GROUP BY

- UNIQUE（`user_uniq`）と `key_index` の正準キーは変更しない。許可リストの照合はすべて deterministic（`Collation::compare` の契約）なので、等値は照合によらずバイト列の等値と一致する。ソートキーで正準化すると等値の意味論が変わり、対象外の nondeterministic 照合へ踏み込む。受け入れ条件 (b) は「PostgreSQL の deterministic 照合と同じ判定」を結合テストで固定する（`'a'` と `'A'`、`'あ'` と `'ア'`、NFC と NFD は別の値、完全一致の重複は `23505`）。
- GROUP BY の列挙形（`column_groups`）はバイト列順の辞書だけが使える（`string_groups` がバイト列順で並べ直すため）。辞書が ICU 順の列は `None` を返して全走査へ縮退する。`classify_aggregate_access` も同じ条件を持つ。ICU キーの列挙形は集計表側の照合対応が要るため対象外。

## 3. PostgreSQL との差

- 同じ列を `(a COLLATE "C", a COLLATE "und-x-icu")` のように重複指定する宣言は、PostgreSQL は許すが本実装は `42601`。
- 式レーンの照合付き比較（`col COLLATE "x" = 'v'`）は、照合が一致していても索引を使わない。

## 4. セキュリティ

- 索引は RLS 適用済みのスナップショットだけから作り、候補を絞るだけで、候補行にも述語を再適用する契約を維持する。照合不一致で全走査へ縮退しても RLS フィルタの適用順は変えない。
- 照合名は非信頼の SQL 入力だが、固定の許可リストの完全一致でだけ解決する。カタログには `[a-z0-9_]` のトークンだけを書き、デコード側も同じ検証を通す。
- 平均値長ゲート・バイト予算は ICU 列にもそのまま効く。照合データの差し替え経路（環境変数・実行時ファイル）は作らない。

## 5. 対象外

- 式索引（`CREATE INDEX ... ((expr))`）。従来どおり `0A000`。
- 式レーンの照合付き比較を索引の等価として使うこと。
- ICU キーの GROUP BY 列挙形、TEXT の範囲比較の索引化、nondeterministic 照合と UNIQUE キーの正準化。
- `EXPLAIN` の `index=` 注記を、選定した照合と一致する宣言だけに絞ること（同じ列に照合違いの宣言が複数あるときの名前の選び方は従来の貪欲法のまま）。

## 6. 照合データの版（collversion）の扱い（オーナー確認事項）

照合順に依存する構造は、メモリ上の `ScalarIndex` だけである（プロセス起動ごと・テーブル世代ごとに作り直す派生キャッシュ）。永続化される `user_uniq`・`key_index` はバイト列の等値で、照合データの版に依存しない。照合データは `icu_collator_data = "=2.3.0"` で完全固定してバイナリへ焼き込んであり、版が変わるのはバイナリの更新（依存更新。承認制）のときだけで、その時点でメモリ上の索引は作り直される。

結論として、**現時点では索引の再構築も、カタログへの照合データの版の記録も不要**とした。将来、ソートキーを永続化する順序索引や nondeterministic 照合を導入する場合に限り、版（PostgreSQL の collversion 相当）をカタログに記録し、不一致なら再構築または拒否する設計が要る。この結論の可否はオーナーの確認事項とする。

## 7. 検証

- `crates/engine/tests/sql_collate_index.rs`: 照合ごとの列で索引・全走査・素の形・オラクルが一致、照合不一致の縮退と `EXPLAIN` の理由、`EXPLAIN` と実行時の経路の一致、起動時 opt-in の辞書の照合の選定、`CREATE INDEX` の受理・エラー分類・永続化、UNIQUE の判定（ALTER 前後・再起動後）、GROUP BY。
- 単体: `sql::scalar_index`（辞書の照合順・使用可否・宣言の決定規則・理由つき静的判定）、`catalog`（`v1` 不変・`v2` 往復・破損の拒否・照合の検証）、`sql::allowlist`（構文）。
