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

## 7. Issue #2193 実装記録（第 1 段: 物理キーの型と採番器）

ADR §3 の D1・D2・D3 と、§4「表のライフサイクル」の項、D8 の一時表・旧 `rows` 表の項を挙動不変で実装した。
契約の詳細は公開境界の注記のとおり本書に記載せず、spec のポインタ（TABLE-23。関連: TABLE-12・TABLE-16・RLS-9・RLS-10）を正とする。

- **`RowId`（`storage::rowid`）**: 行ストアの物理キーの要素を `pub(crate)` の newtype として区別する。`Display` を実装しない。
  物理キーの組み立て・分解は `row_key`／`split_row_key` に集約した（`storage.rs`・`txn.rs`・`tenant.rs`）。
- **行識別方式（`catalog::RowIdentity`）**: カタログ属性。永続化形式・互換性の詳細は記載しない（ポインタ: TABLE-23）。
- **採番器（`catalog::rowid_alloc`）**: 実装は `crates/engine/src/catalog/rowid_alloc.rs`。契約の詳細は記載しない（ポインタ: TABLE-23）。
- **ゲート**: 採番方式の表は、後続 sub-issue の対応が揃うまで DML・読み取りを固定文言（値を含まない）で拒否する。
- **非露出の回帰テスト**: `tests/table23_rowid_storage.rs`、`catalog/rowid_gate_tests.rs`。

### 公開 API 棚卸し（§5 の #2193 成果物）

| API | 本 PR での扱い | 後続担当 |
| --- | --- | --- |
| `tenant::insert_row`／`insert_rows`／`insert_typed_row`／`update_row`／`delete_row` の `id` | 従来方式の `id` 列値として不変（内部で `RowId` へ変換） | #2197 |
| `Storage::put`／`put_batch`／`get`、`txn::WriteTxn::put`（旧 `rows` 表） | 恒久的に従来方式（rowid = 呼び出し元 `id`。Rust API 専用でカタログ外） | なし |
| `storage::Row.id`・`catalog::StorageRow` | `id` 列値（従来方式で rowid と恒等）。core API スナップショット不変 | #2197 |
| `Storage::get_row_from_table`・`scan_table_page`・`RowCursor (String, u64)` | 従来方式のみ受理。採番方式の表は拒否（カーソルに rowid を出さない） | #2197（不透明カーソル化の要否） |
| `StorageError::NotFound(u64)`・`CatalogError::RowNotFound(u64)` の文面 | 従来方式では利用者 `id`。採番方式の表へは到達しない（ゲート） | #2197 |
| `SearchHit` 等の検索結果 id | 不変 | #2195 |
| NoSQL `where.id`・SQL `WHERE id = n`・`ON CONFLICT (id)` | 不変 | #2197／#2199 |
| 分割実行 DML のカーソル | 不変 | #2196 |

未移行モジュール（`constraint.rs`・`key_index.rs`・`column_rewrite.rs`・`sql/*`・`arena.rs`・`rls.rs`・`recovery/ledger.rs`
等）は `user_rows` のキーの `u64` を従来どおり直接扱う。これらは担当 sub-issue（#2194〜#2196）が `RowId` へ移す。

## 8. Issue #2194 実装記録（第 2 段: 索引・FK の行参照）

ADR §3 の D8「永続一意索引・FK 用 `key_index`」の項を挙動不変で実装した。契約の詳細は本書に記載せず、
spec のポインタ（TABLE-23・TABLE-16。関連: RLS-9・RLS-10）を正とする。

- **型の境界だけを変えた**: 索引が持つ「行への参照」を `RowId` で扱う。対象は `constraint/unique_index.rs`（正引き値・逆引き
  サブキー・バックフィル・stale 判定）、`key_index.rs`（`sync_rows_in_txn`・`clear_tenant_in_txn`・バックフィル）、
  `constraint.rs` の入口（検査・後片付け・参照アクションの対象行）、`sql/pk_point.rs` の locator。行テーブルのキーは
  `row_key`／`split_row_key` を通して読む。
- **永続バイト列は変えない**: 索引へ書く行参照の `u64` は `RowId::to_index_raw`／`from_index_raw` を通り、従来方式の表では
  従来の `id` と同一のバイト列になる。このため `unique_index::FORMAT_VERSION`・カタログの版は据え置き、再オープンで
  マーカーは無効にならず再構築も起きない（バイト列の golden テストと再オープンの結合テストで固定）。
- **FK の `id` 参照の解決を 2 か所に集約した**（`constraint.rs`）: 参照先の `id` 値から親行の物理キーを得る
  `parent_row_key_by_id_value` と、削除された親行の rowid から失われた `id` 値を得る `referenced_id_value_of_removed_row`。
  採番方式の親は固定文言（`0A000`）で拒否する。DDL 段（`resolve_foreign_key_target`）でも同じ拒否を行う。
- **ゲートは維持した**: 採番方式の表の DML は開放しない（外すのは #2195〜#2197 が揃った後）。
- **検査順序・テナント境界は不変**: `enforce_row_constraints_impl` と `enforce_referencing_rows_in_txn` の順序、および
  `(tenant, ..)` の範囲は変えていない（RLS-9・RLS-10）。`23503`／`23505` の SQLSTATE・文面も変わらない。
- **スカラー二次索引・式索引・部分索引は変更なし**: これらはメモリ上の派生索引で、行の参照に `SqlArenaSnapshot` のスロットを使い、
  行ストアの物理キーに触れない。`id_index`（利用者の `id` 列値の順序索引）はアリーナの ids の意味づけとあわせて #2195 が扱う。
- **残した境界**: 検索経路への出口（`visit` コールバックの `id`）、行の読み書き API の `id`、`UniqueKeyPlan::row_keys` の
  `id` 引数は従来方式の `id` のまま（`RowId::legacy_id` の呼び出し箇所が後続の作業一覧）。
- **テスト**: `tests/table23_index_rowid_reference.rs`、`constraint.rs`・`constraint/unique_index.rs`・`catalog/rowid_gate_tests.rs`
  の単体テスト（rowid とキー値が食い違うケース・採番方式の親の拒否を含む）。
