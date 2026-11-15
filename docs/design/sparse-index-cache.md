# ADR: SparseIndex のテーブル世代整合キャッシュ（Issue #357）

- ステータス: Accepted
- 対応: Issue #357
- 関連ポインタ: `crates/engine/src/core.rs`（`PrefilterCache`〔TASK-169〕・
  `DictionaryCache`〔TASK-109・PLAN-5〕。同型キャッシュの既存実装）・
  `docs/design/table-generation-rejection-granularity.md`（Issue #285。テーブル
  単位世代の粒度判断）。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 検証コード: `crates/engine/src/sql/sparse_cache.rs`（`#[cfg(test)] mod tests`）・
  `crates/engine/src/sparse.rs`（`approx_heap_bytes` 単体テスト）・
  `crates/engine/tests/sparse_cache.rs`（受け入れ基準 1〜3・WHERE 非経由・
  text_column_index 別エントリ化の結合テスト）

## 背景

hybrid 実行（`ORDER BY hybrid_rrf(...)` / `HYBRID(...)`。`sql/exec.rs::execute_statement`
の `Ranking::Hybrid` 分岐）は、毎クエリ RLS 可視行走査中に本文テキストを疎コーパスへ
複製蓄積し、`SparseIndex::build`（BM25 統計・語彙 `BTreeMap` 構築）を再実行していた。
テーブル・可視集合が無変化でも build コストがクエリごとに発生する。

## 設計方針

### キャッシュキーと世代

`core.rs::PrefilterCache`（TASK-169）・`core.rs::DictionaryCache`（TASK-109・PLAN-5）と
同じ設計を踏襲する。

- **キー**: `(table, PolicyContext, text_column_index)` の完全一致。`PolicyContext`
  はテナント境界を含むため、キーが一致する限り他テナントのコーパスを構造的に
  参照不能にする。`text_column_index`（`Ranking::Hybrid` が保持する、hybrid 本文
  として使う TEXT 列のインデックス。`hybrid_rrf(embedding, '<vec>', <col>,
  '<query>')` の 3 引数目に対応）をキーへ含めなければ、同一テーブルの異なる
  TEXT 列を本文に指定する 2 つの hybrid クエリが互いのキャッシュへ誤ヒットし、
  片方の列で構築した索引をもう片方の列のクエリへ提供してしまう（レビューで
  検出。`tests/sparse_cache.rs::hybrid_queries_on_different_text_columns_do_not_share_cache_entry`
  で固定）
- **世代**: テーブル単位世代（`catalog::table_generation_in_txn`。粒度の設計判断は
  `docs/design/table-generation-rejection-granularity.md` 参照）を使う。グローバル
  `storage.current_generation()` ではなく、無関係な他テーブルへの書き込みでは
  失効しない

### 適用条件: フィルタなし hybrid クエリに限定

疎コーパス（`DocId` = アリーナのスロット番号）は SCALAR 事前フィルタの通過行にのみ
割り当てられる。`bound.metadata_filters`・`bound.expr_filters` がともに空の hybrid
クエリに限り、コーパスは「RLS 可視行のうち本文非 NULL の全行」というクエリ非依存の
集合になり、同一世代内であればスロット番号の割当も含めて完全に再現される（redb の
走査順・RLS 判定の純粋性に依存する不変条件）。フィルタ付きクエリ・DISTANCE 専用
クエリはキャッシュへ到達させない（`sql/exec.rs::execute_statement` の
`sparse_cache_eligible` 判定）。この条件の緩和（DISTANCE 先行経路への拡張等）は
将来課題とする。

### fail-closed 契約: lookup と insert の非対称

- `SparseIndexCache::lookup`: 世代不一致・ロック毒化・世代読み取り失敗はいずれも
  「見つからなかった」として扱う（`PrefilterCache::lookup` と同じ方針）
- `SparseIndexCache::insert`: 他の 2 キャッシュ（Issue #280 で `None` 統一済み）とは
  **意図的に異なる契約**を持つ。挿入対象自身が既に古い場合・ロック毒化時は
  キャッシュへ反映しないが、呼び出し元へは常に構築済みの `Arc<SparseIndex>` を
  返す（`Option` ではない）。呼び出し元がこの索引を構築したのは自分自身のクエリの
  `read_txn`（単一スナップショット）上であり、そのスナップショットの中でのみ使う
  限り stale にはなり得ない。契約が禁じるのは「キャッシュへ古い索引を常駐させる」
  ことのみであり、「呼び出し元が自分で構築した索引を使う」ことまでは禁じない
  （`PrefilterCache`/`DictionaryCache` が防いでいるのは前者の経路のみ）

### 配置

`check_core_api.sh` のスナップショット対象は `VectorCore`/`SearchProvider` trait と
`lib.rs` の `pub mod`/`pub use` のみであり、`lib.rs` へ新規 `pub mod` を足すと
スナップショットが割れる。キャッシュは `sql` 配下のサブモジュール
`crates/engine/src/sql/sparse_cache.rs`（`sql.rs` に `pub(crate) mod sparse_cache;`
を追加。`lib.rs` 無変更）へ置いた。`make core-api-check`・`make
sort-determinism-check` はいずれも green のまま（`BTreeMap`/`min_by_key` による
決定的な走査・追い出しのみを使用）。

`execute_statement` へは `SparseCacheAccess<'a> { storage: &'a Storage, cache: &'a
SparseIndexCache }` を 1 引数として追加した（引数を素で 2 つ増やすと clippy の
`too_many_arguments`（既定閾値 7）に抵触するため）。`execute_statement` 自体が
`pub fn` であるため `SparseCacheAccess` も `pub` にした（フィールドは
`pub(crate)`。`Storage`/`SparseIndexCache` の型が crate 外へ公開されるわけではない
ため、crate 外からこの型を構築・分解することはできない）。

`SparseIndex::search`/`search_within`/`score_within` が使うスコアアキュムレータの
再利用スクラッチプール（Issue #546）は `SparseIndexCache` 自体（本ファイル）
ではなく `SparseIndex` 内部（`sparse.rs`）に配置した。キャッシュが保持する
実体が `Arc<SparseIndex>` であるため、`SparseIndex` 内部に置くだけで
「キャッシュと同寿命の再利用バッファ」という要件をキャッシュ側の変更
（`CacheEntry` の型変更・呼び出し元シグネチャの波及）なしに満たせる。詳細は
`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」節参照。

### スコープ外（実装しない）

- 語彙マップ（`sparse.rs` の `BTreeMap`）のハッシュ系構造化: 決定性確保のための
  設計判断（`sort-determinism-check` の対象）であり、影響評価が別途必要なため
  本 PR に混入させない
- WHERE（メタデータ/式）フィルタ付き hybrid クエリへのキャッシュ適用拡張
- SCALAR 事後フィルタ（DISTANCE 先行）経路への適用拡張

## 受け入れ基準との対応

| # | 内容 | 検証 |
| - | ---- | ---- |
| 1 | 同一世代内の hybrid 連続クエリで build が 1 回に償却される | `tests/sparse_cache.rs::acceptance1_repeated_hybrid_query_amortizes_sparse_index_build`（`sparse_index_cache_stats()` の hits/misses・両実行の結果行完全一致） |
| 2 | 世代競合・DML 後の整合が fail-closed（stale 索引で応答しない） | `tests/sparse_cache.rs::acceptance2_insert_after_hybrid_query_invalidates_cache_and_reflects_new_row`（SQL INSERT 後の再実行で新行が反映され、stale_evictions が記録される） |
| 3 | hybrid 経由の RLS 不変テストが green | `tests/sparse_cache.rs::acceptance3_hybrid_cache_does_not_leak_across_tenants`（別テナントの同一 SQL がキャッシュをヒットせず、他テナントの Private 行が結果に漏れない）。既存の hybrid RLS 検証（`tests/sql_surface.rs` の sql3 系）も green を維持 |

補足（レビューで検出した追加観点）: `text_column_index` をキーへ含めない実装では、
同一テーブル・同一 ctx で異なる TEXT 列を本文に指定する 2 クエリが誤ヒットし得た。
`tests/sparse_cache.rs::hybrid_queries_on_different_text_columns_do_not_share_cache_entry`
で固定した（上記「キャッシュキーと世代」節参照）。

導入後の Recall 非劣化・RRF 同点順位規約の不変・前後比較実測は Issue #358・
`docs/design/sparse-index-cache-verification.md` を参照。

## 全可視ビットマップの付随（Issue #3221）

フィルタなしの hybrid（可視スロット列が `0..n` の恒等列）では、可視集合の `BTreeSet` 構築と
可視ビットマップの再構築（`id_index` の SipHash 引き）は結果が索引の構築時点で決まっている。
そのため全可視の統計（文書数・文書長合計）と最大 DocId を `SparseIndex` に構築時に固定し、
最大 DocId が可視長より小さいときは確保も走査もせずに全可視ビットマップを返す。

- 保持単位は別のキャッシュ構造を作らず `Arc<SparseIndex>` に付随させる。上記「キャッシュキーと世代」の
  （表・テナント・本文列・テキスト処理）と表世代でしか取り出せないため、テナント × 世代の分離をそのまま継承し、
  他テナントの索引・統計を参照する経路は構造上存在しない（RLS-8 ポインタ）。
- 全可視かどうかは呼び出し元のフラグを信用せず、入力列（恒等列の線形確認）と索引の最大 DocId からデータで毎回判定する。
  不成立なら従来の `BTreeSet` 経路、または `doc_ids` の添字走査へ倒れ、可視集合は広がらない（fail-closed）。
- 統計の母数は従来どおり可視集合だけから決まる（`ScoreScope::Visible`）。`ScoreScope::All` には切り替えないため、
  結果は従来経路とビット一致する。
