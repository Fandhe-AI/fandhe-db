# SQL 表層の投影を Top-k 確定後へ遅延デコードする（Issue #453）

## 背景

`SELECT id, body FROM docs ORDER BY embedding <=> '<vec>' LIMIT k` のように
スカラー列を投影すると、`sql::exec::execute_statement_with_cache` の RLS→SCALAR
段の行フック（`on_visible_row`）が投影列（`needed_column_indices`）を持つ場合、
**全可視行**へ `row_codec::scan_scalar_columns`（構造検証・UTF-8 検証・`Text` 列の
`String` 複製）を適用してから Top-k を選出していた。`LIMIT` に関係なく行数に
比例する固定コストが乗る（`docs/design/crossdb-bench.md`「self の投影コスト
切り分け」節参照）。

`SELECT id` のみ（投影がスカラー列を参照しない場合）は既に Issue #314・#363 の
高速経路でこのコストを避けられていたが、1 列でも投影すると外れていた。

## 方針: `defer_projection`

以下の 3 条件がすべて成り立つ場合に限り、SCALAR 段（`on_visible_row`）は
`WHERE`・式述語のいずれも判定しない恒等写像になる:

- `WHERE`（メタデータフィルタ）が空
- `WHERE` の式述語が空
- hybrid でない（疎コーパス蓄積が本文列を要さない）

この条件が成り立つとき、投影がスカラー列を参照していても `on_visible_row` は
`scan_scalar_columns` を呼ばず即座に可視行を通過させる（`defer_projection`）。
投影に必要な列のデコードは `RlsSafetyNet::apply` 通過後の Top-k 行に限って
`project_rows` の `ScalarSource::Deferred` が行う。

`defer_projection` が真のとき常に `cache_fast_path_eligible`（投影列の有無を
条件から外し 3 条件のみへ簡素化）も真になるため、`SqlArenaCache` ヒット時は
従来の `SELECT id` 高速経路と同じ「キャッシュ済み `VectorArena` を直接借用し
行単位ループを丸ごと省略する」経路に自然に乗る。

## metadata の取得元（`DeferredScalars`）

候補選択と同一スナップショットに閉じたまま、Top-k の各スロットに対応する
metadata バイト列を 2 通りの経路で取得する:

| 経路 | 取得元 |
| --- | --- |
| キャッシュヒット | `SqlArenaSnapshot::metadata()[slot]`（`arena()` とスロット添字が 1 対 1） |
| キャッシュミス／`arena_cache == None` | 候補選択と同一 `read_txn` 上で行テーブルを再度開き、`(tenant_id, id)` の複合キーで再取得 |

候補選択のアリーナ構築経路（`arena.rs`）は可視行を格納する時点で
`storage::verify_row_key_tenant` によりキー/ヘッダ tenant 整合（TABLE-12）を
保証しており、redb 再取得経路（本節）では同じ検査を再度行う（defense-in-depth。
アリーナ構築後に物理データが変化しても再取得結果の整合を独立に確認できる）。

## 契約上の注記

遅延適用クエリでは、可視行のうち **Top-k に含まれない行**のスカラー列
ペイロード破損（不正 UTF-8 等）を当該クエリでは検出しなくなる。根拠:

1. `SELECT id` の高速経路（Issue #314・#363）が既にこの挙動であること
2. ヘッダ・tenant・可視性・dim・embedding 境界・metadata 境界の構造検証
   （`decode_row_embedding_and_metadata_into`）は全可視行で従来どおり実施
   されること
3. 誤った値が応答へ混入することはないこと（Top-k に入った行は必ず
   デコード・検証される）

`crates/engine/src/sql/exec.rs` の `sql::exec::tests::deferred_projection_corruption`
（crate 内単体テスト）で、Top-k 外の破損行はクエリを失敗させず、Top-k 内の
破損行は `SqlSurfaceError::Internal` として fail-closed に拒否されることを
固定している。

## テスト

- `crates/engine/tests/sql_deferred_projection.rs`: TABLE-12（同一 `id` の
  異なるテナント行が混線しない）・cold/warm（`Redb`/`Snapshot` 双方の取得元）
  一致・`USING MODE 'precision'` との整合・`WHERE` 付き（eager 経路のまま）の
  非影響を固定する結合テスト。
- `crates/engine/tests/sql_arena_cache.rs::deferred_scalar_projection_cache_hit_matches_cold_cache`:
  既存の cold/warm 完全一致ハーネスへスカラー列投影ケースを追加。
- `crates/engine/src/sql/exec.rs` 内 `#[cfg(test)]` 単体テスト
  （`deferred_projection_corruption`）: 上記の fail-closed 契約。

## 距離列（Issue #2007）

投影位置の距離列（`embedding <=> q AS d`。SQL-33 (7) ポインタ）は `ProjectedColumn::Computed`
として `project_rows` が評価する。評価入力の埋め込みは Top-k 確定後にアリーナ
（`arena.vector(slot)`）から取るため、metadata の再取得・遅延デコードの適格条件
（`defer_projection`・キャッシュ高速経路）には影響しない。距離項目はスカラー列を参照しない
ので `needed_column_indices` にも列を足さない。

## 系統別順位・融合スコア（Issue #2137）

`hybrid_dense_rank()`／`hybrid_sparse_rank()`／`hybrid_score()`（SEARCH-17 ポインタ）は
`ProjectedColumn::HybridDetail` として `project_rows` が融合段の対応表とヒットのスコアから
作る。スカラー列を参照しないので `needed_column_indices` に列を足さず、遅延デコードの適格条件
は変わらない。

## スコープ外・申し送り

- hybrid（`Ranking::Hybrid`）で `SparseIndexCache` ヒット時の遅延投影化。
- `WHERE` メタデータフィルタ付き DISTANCE の投影遅延化（SCALAR 段でのスキャン
  自体は避けられないため）。
- `feature_bench`／`crossdb_bench` での定量的な前後比較実測（本変更は engine
  クレートの実装のみ。ベンチフェーズの追加・実測はオーナー計測へ申し送り）。
