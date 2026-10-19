# ADR: 物理 rowid と PRIMARY KEY の分離（Issue #2192）

- ステータス: Accepted（設計記録。後続 #2193〜#2196 で実装する）
- 対応 Issue: #2192（親 #1710。後続 #2193・#2194・#2195・#2196。さらに後続 #2197・#2199・#2200・
  #1706・#1693・#1696）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-12・TABLE-16・TABLE-23・TABLE-24）・
  `docs/spec/04-behavior/recovery.md`（RECOVER-10・RECOVER-11・RECOVER-12）・RLS-9・RLS-10・
  SEARCH-1・SQL-4・SQL-32。spec 本文は転記しない。契約は spec を正とし、本書では再掲しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/storage.rs`（`RowStoreTableDef`・`RowCursor`・`decode_row`）・
  `catalog.rs`（`user_rows_table_name`・`UserRowsTableDef`・`CATALOG_FORMAT_VERSION_*`）・
  `row_codec.rs`・`constraint.rs`・`constraint/unique_index.rs`・`key_index.rs`・`arena.rs`・
  `kernel.rs`・`hnsw.rs`・`gpu_batch.rs`・`sparse.rs`・`hybrid.rs`・`rerank.rs`・
  `recovery/ledger.rs`・`recovery/content_hash.rs`・`recovery/partitioned_job.rs`・
  `tenant/partitioned_dml.rs`・`sql/visible_cache.rs`・`sql/aggregate.rs`・`sql/scalar_index.rs`
- 検証コード: なし（本 Issue は設計記録のみ。コードは変更しない）

## 1. 背景・目的

現状、行ストアの物理キーは `(tenant_id, id)` で、`id` は利用者が指定する u64 の予約列である。
宣言 PRIMARY KEY は `id` に**追加される**一意性制約にすぎない（[sql-primary-key.md](sql-primary-key.md)）。
親 #1710 は、行ストアの物理キーと利用者定義 PK の関係を見直す破壊的変更である
（ポインタ: TABLE-23・TABLE-16）。後続 #2193〜#2196 が判断の揺れなく進むよう、
実装前に次の 6 点を整理する。具体的な契約は spec のポインタを正とし、本書には記載しない。

1. 物理キー形式と rowid の採番・非公開性・再利用禁止
2. PK 一意索引の格納形式と PK なし表の扱い
3. 行識別子を参照する全構造の移行先
4. kNN・`ORDER BY` の同点順序の置き換え先
5. 既存 DB の移行手順・後方互換・ロールバック不可の範囲
6. 影響を受ける `docs/design/*` の一覧

## 2. 公開コードの現状（設計の前提）

| 事項 | 現状 | 所在 |
| ---- | ---- | ---- |
| 行ストアのキー | redb キー型 `(&str, u64)` = `(tenant_id, id)`。表軸は redb テーブル名 `user_rows/{table}`。旧 `id` 単独キーの DB は `IncompatibleRowKeyFormat` で拒否 | `storage.rs`・`catalog.rs` |
| `id` の保持 | 行値に格納せずキーから復元（`decode_row(id, buf)`）。型付き列は `row_codec` が metadata 内に符号化 | `storage.rs`・`row_codec.rs` |
| 旧 Rust API 行ストア | `rows`（`ROWS_TABLE`）はカタログ外で同じキー型 | `storage.rs`・`recovery/commit_boundary.rs` |
| PK・UNIQUE | 永続一意索引 `user_uniq/{table}`（正引き・逆引き）で検査。カタログは版付きテキストで、`pk:` 行欠落 = `id` 暗黙 PK | `constraint.rs`・`unique-index.md` |
| FK | `key_index/{table}/{name}` = `(tenant, key_bytes, row_id)`。FK が `id` を参照する場合は物理 id を子列型へ変換（`parent_id_key_bytes`） | `key_index.rs` |
| u64 を `id` 値として直解釈する高速経路 | Fast tier の `COUNT/SUM/MIN/MAX(id)`、スカラー索引の `id` 単純比較の候補削減、FK→`id`、`WHERE id = n`・`ON CONFLICT (id)`・NoSQL `where {"id": n}` の点アドレス | `sql/visible_cache.rs`・`sql/aggregate.rs`・`sql/scalar_index.rs`・`tenant.rs` |
| 検索系の識別と同点順序 | arena・HNSW・GPU バッチ・疎索引・RRF・rerank は u64 id で行を識別し「スコア降順・同点は id 昇順」の安定ソート（`check_sort_determinism.sh` で機械確認） | `arena.rs`・`kernel.rs`・`hnsw.rs`・`gpu_batch.rs`・`sparse.rs`・`hybrid.rs`・`rerank.rs` |
| 台帳 | キー `(tenant, table, operation_id)`、値は内容照合ハッシュ。材料にクライアント送信の `id` が入る | `recovery/ledger.rs`・`recovery/content_hash.rs` |
| 分割実行 DML | カーソルは物理キー `(tenant, id)` の u64（下端排他）で、永続ジョブ状態に入る | `recovery/partitioned_job.rs`・`tenant/partitioned_dml.rs` |
| matview・一時表 | matview 実体は `(context_key, u64)`。一時表は独立 in-memory `Storage`（同一物理レイアウト） | `catalog/materialized.rs`・`sql/temp_rows.rs` |

## 3. 決定

決定は本リポの実装判断（D1〜D11）として記す。根拠の spec は ID ポインタで示す。

### D1 識別子の分離方針

行の内部参照と利用者に見える列値の分離方針、およびその型表現は、公開境界の注記のとおり
本書に記載しない。契約は spec のポインタ（TABLE-23・TABLE-16）を正とし、sub-issue #2193 が確定する。

### D2 物理キー形式

物理キー形式と移行時の扱いは、公開境界の注記のとおり本書に記載しない。
契約は spec のポインタ（TABLE-23・TABLE-12）を正とし、sub-issue #2193 が確定する。

### D3 行識別モード（カタログ属性）

行識別の方式（採番方式・`id` の格納方法・既存表の切替時期）に関する新規契約は、公開境界の
注記のとおり本書に記載しない。契約は spec のポインタ（TABLE-23・TABLE-16・TABLE-24）を正とし、
sub-issue #2193 が確定する。

却下案 A（#2193 で全表を一斉に切替）: 索引・高速経路・台帳の対応（#2194〜#2196）が揃う前に
中間状態で整合性が崩れるため採らず、各 sub-PR を挙動不変（既存 crash-test 緑）で出す段階導入を採る。

公開境界の注記: 利用者定義 PK と物理 rowid の分離に関する新規契約の詳細（採番・PK 索引・PK なし表・
同点順序・既存 DB の移行契約）は、README「実装方針（要点）」で公開済みの範囲を超えるため、
リポジトリ管理者の承認記録が得られるまで本書には記載せず、ポインタ表記に留める。

### D4〜D7 採番・非公開性・PK の格納・PK なし表

詳細は本書に記載しない（上記の公開境界の注記）。契約は次の spec ポインタを正とし、実装は
sub-issue #2193〜#2195 が spec に従って確定する。

- 採番・再利用禁止・枯渇時の扱い: `docs/spec/04-behavior/data-model.md` TABLE-23・TABLE-12
- rowid の非公開性: TABLE-23
- PK の格納・PK なし表の扱い: TABLE-16・TABLE-23

### D8 行識別子の移行先

行識別子を参照する各構造の移行先の詳細は本書に記載しない（公開境界の注記）。契約はポインタを正とし、
担当 sub-issue が spec に従って確定する。

| 参照箇所（§2 の現状に対応） | 担当 | ポインタ |
| -------------------------- | ---- | -------- |
| 永続一意索引・FK 用 `key_index`・スカラー二次索引 | #2194 | TABLE-16・TABLE-23 |
| ベクトル arena・HNSW・GPU バッチ・疎索引・RRF・rerank・可視キャッシュ・Fast tier 高速経路 | #2195 | TABLE-23・SEARCH-1 |
| `operation_id` 台帳・`content_hash`・分割実行 DML カーソル | #2196 | RECOVER-10・SQL-32 |
| 一時表・旧 `rows` 表（Rust API 専用・カタログ外） | #2193 | TABLE-23 |
| matview 実体 | 対象外 | なし |

### D9 同点順序

詳細は本書に記載しない（上記の公開境界の注記）。「識別」と「順序キー」の分離方針と各表状態での
順序キーは、ポインタ（TABLE-23・SEARCH-1・SQL-4）を正とし、#2195 が確定する。不変条件
「安定ソート・`sort_unstable_by` 不使用」（`check_sort_determinism.sh`、
[rrf-tie-break-determinism.md](rrf-tie-break-determinism.md)）は維持する。

### D10 既存 DB の移行・後方互換・ロールバック不可範囲

詳細は本書に記載しない（上記の公開境界の注記）。移行契約・値域の扱い・旧バイナリの扱い・
ロールバック不可の境界は、ポインタ（TABLE-23・TABLE-24・RECOVER-10〜RECOVER-12）を正とし、
sub-issue #2200 が spec に従って確定し、運用文書化する。

### D11 テナント境界

採番状態・索引・順位表はすべてサーバー導出テナントで名前空間化し、一意性判定の母集合とエラーの
固定文言は現行契約（[sql-primary-key.md](sql-primary-key.md)・[unique-index.md](unique-index.md)）
を維持する（ポインタ: RLS-9・RLS-10）。

## 4. 影響を受ける `docs/design` 一覧

改訂は各 sub-issue で行う（本 Issue では既存 ADR を変更しない）。

| 区分 | 文書 | 担当 |
| ---- | ---- | ---- |
| PK・一意性・FK | [sql-primary-key.md](sql-primary-key.md)・[alter-table-primary-key.md](alter-table-primary-key.md)・[unique-index.md](unique-index.md)・[unique-constraint.md](unique-constraint.md)・[alter-table-unique-constraint.md](alter-table-unique-constraint.md)・[foreign-key.md](foreign-key.md)・[alter-table-foreign-key-constraint.md](alter-table-foreign-key-constraint.md) | #2193・#2194 |
| 二次索引 | [scalar-secondary-index.md](scalar-secondary-index.md)・[scalar-index-prune.md](scalar-index-prune.md)・[scalar-index-mask-search.md](scalar-index-mask-search.md)・[partial-index.md](partial-index.md)・[expression-index.md](expression-index.md) | #2194 |
| 検索・キャッシュ | [visible-bitmap-cache.md](visible-bitmap-cache.md)・[sql-arena-generation-cache.md](sql-arena-generation-cache.md)・[knn-two-stage-topk.md](knn-two-stage-topk.md)・[hnsw-index.md](hnsw-index.md)・[hnsw-search.md](hnsw-search.md)・[hnsw-graph-construction.md](hnsw-graph-construction.md)・[gpu-batch-topk.md](gpu-batch-topk.md)・[sparse-inverted-index.md](sparse-inverted-index.md)・[rrf-tie-break-determinism.md](rrf-tie-break-determinism.md) | #2195 |
| 台帳・分割実行 | [operation-id-ledger.md](operation-id-ledger.md)・[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)・[partitioned-dml.md](partitioned-dml.md) | #2196 |
| DML・表面 | [sql-upsert.md](sql-upsert.md)・[nosql-update-delete-mapping.md](nosql-update-delete-mapping.md)・[nosql-insert-mapping.md](nosql-insert-mapping.md)・[create-view.md](create-view.md)（ビュー書き込みの基表行特定） | #2197・#2199 |
| 表のライフサイクル | [drop-table.md](drop-table.md)（カウンタ削除）・[truncate-table.md](truncate-table.md)・[temp-table.md](temp-table.md) | #2193 |

実装時は `git grep -l '(tenant_id, id)\|同点は id 昇順\|物理キー' docs/design` で漏れを再確認する。

## 5. 後続 sub-issue の前提

各 sub-issue の契約詳細は spec のポインタ（§3 の各節）を正とし、本書は担当の割り当てだけを記す。

- **#2193**: D1・D2・D3・公開 API 棚卸し。挙動不変で crash-test 群が通ることが前提
- **#2194**: D8 の索引・FK 項。`23503`／`23505` の既存契約が不変であること
- **#2195**: D8 の検索系項と D9。既存の結果が完全一致すること、Recall 層 A の再計測
- **#2196**: D8 の台帳・分割実行 DML 項。再送判定の既存契約が不変であること
- 前方ポインタのみ: #2197／#2199、#2200（移行）、#1706、#1693／#1696（ポインタ: TABLE-24）

## 6. セキュリティ・未決事項

セキュリティ上の要点: 内部識別子の非露出の契約は spec のポインタ（TABLE-23）を正とする。
テナント境界と fail-closed のエラー契約を維持する（ポインタ: RLS-9・RLS-10）。

未決・確認事項は各 sub-issue が spec に従って確定する。管理者の承認が得られた時点で、承認済みの
範囲を spec 側の「公開境界」記録と合わせて本書へ反映する。
