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
親 #1710 は、物理キーを利用者から見えない rowid（PostgreSQL の `ctid` 相当）に替え、
利用者定義 PK（任意型・複合）を一意索引として分離する破壊的変更である。後続 #2193〜#2196 が
判断の揺れなく進むよう、実装前に次の 6 点を固定する。

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

### D1 識別子の 2 分離

型レベルで「行の物理参照 = `RowId`（u64 newtype・非公開）」と「利用者に見える行の値
（PK 列・`id` 列）」を分ける。engine 内部の行参照（索引の値・キャッシュ・カーソル）はすべて
`RowId`、利用者・ハッシュ・応答に出るのは列値のみとする。

### D2 物理キー形式

redb キー型 `(&str, u64)` は**変えない**。`(tenant_id, rowid)` と意味だけを読み替える。
表軸は従来どおり redb テーブル名 `user_rows/{table}`。物理キーの再書き込みなしで移行できる
ようにするため（ポインタ: TABLE-23・TABLE-12）。

### D3 行識別モード（カタログ属性）

表ごとに 2 モードを持つ。

| モード | rowid | `id` の扱い | 対象 |
| ------ | ----- | ----------- | ---- |
| `IdKeyed` | 利用者指定 `id` と同一 | 値に格納せずキーから復元（現行） | 既存表・#2199 以前に作られる表 |
| `Allocated` | サーバーが採番 | 他の列と同じく行値に格納 | #2199 以降の `CREATE TABLE`・#2200 で移行した表 |

モードは**新しいカタログ版**で表す。版番号は実装時に origin/main と open PR を確認して次の
未使用番号を採り、ADR には固定番号を書かない。`IdKeyed` 表のカタログバイト列・ゴールデン
テストは不変とする。

却下案 A（#2193 で全表を一斉に採番方式へ切替）: 索引・高速経路・台帳の rowid 化
（#2194〜#2196）が揃う前に `id ≠ rowid` の行が生じ、一意性・点アドレス・FK が中間状態で壊れる。
各 sub-PR を挙動不変（既存 crash-test 緑）で出せるモード導入・段階切替を採る。

### D4 rowid 採番器

- `(table, tenant)` ごとの単調カウンタ（高水位）を新設 redb テーブル（例 `rowid_counter`、
  キー `(table, tenant)`、値は次に割り当てる u64）に持ち、**行の挿入と同一 write txn** で更新する。
  redb は単一ライターのため直列化済み。ROLLBACK で割り当ても戻るが、未 commit の rowid は
  何からも参照されないため観測できない
- カウンタ欠落時は当該テナント範囲の最大キー + 1 で遅延初期化する（`range(...).next_back()`、
  O(log n)）
- **再利用禁止**: commit 済みの rowid は DELETE 後も再割り当てしない（カウンタは減らない）。
  保証範囲は「採番開始以降」とする。採番開始前（`IdKeyed` 期間）に削除された最大 `id` と同値が
  割り当たり得るが、その値を参照する永続構造は削除と同一 txn で消える（台帳は rowid を持たず、
  索引エントリは同時削除）ため観測できない。#2193 で回帰テストにより確認する義務を課す
- u64 枯渇は fail-closed（固定文言・値を含まないエラー）。SQLSTATE は既存エラー契約から
  #2193 で選定し本書へ追記する
- DROP TABLE は同一 txn でカウンタ行も削除する（[drop-table.md](drop-table.md) の削除対象一覧に追加）
- カウンタは常にサーバー導出テナントでキー付けし、他テナントの採番状態は観測できない（RLS-9）

### D5 非公開性

rowid は SQL・NoSQL・wire の応答・EXPLAIN・エラー文面・ログのいずれにも出さない。`ctid` 等の
名前の投影は未知列として拒否する（ポインタ: TABLE-23）。`pub` API で rowid 意味になるもの
（`RowCursor = (String, u64)`・`StorageRow.id`・`VectorArena::ids()` 等）は #2193 が棚卸しし、
公開 API 変更として CHANGELOG の `BREAKING CHANGE:` に書く。

### D6 PK の格納

- `Allocated` 表の PK は既存の永続一意索引 `user_uniq/{table}` を PK 一意索引として使う
  （正引き: 正準キーバイト → rowid、逆引き: rowid → キー）
- PK による点アドレス（`WHERE <PK> = …`・`ON CONFLICT`・NoSQL 単一行 `where`）は
  「正引き 1 回 → rowid で行取得」の 2 回の点照会になる。これを許容する。`IdKeyed` 表は従来どおりキー直引き
- `IdKeyed` 表の `id` 一意性は物理キーで担保する（索引エントリを作らない）
- 宣言 PK のカタログ表現は新版で「`pk:` 空 = PK なし」「非空 = 宣言 PK（#2199 以降は `id` も通常列として含め得る）」
  とし、旧版の「`pk:` 欠落 = `id` 暗黙 PK」と意味を分ける（ポインタ: TABLE-16・TABLE-23）

### D7 PK なし表

行は rowid でのみ区別される。PK を要する操作（`ON CONFLICT` の対象推論・NoSQL 単一行
アドレス等）は fail-closed で拒否する。具体的な SQLSTATE は #2197／#2199 が spec に合わせて確定する。

### D8 行識別子の移行先

| 参照箇所 | 現在 | 移行先 | 担当 |
| -------- | ---- | ------ | ---- |
| 永続一意索引 `user_uniq`・FK 用 `key_index` の行参照 | 物理 id | `RowId`。FK の参照先解決は参照先 PK／UNIQUE の**値**で行い、行参照のみ rowid。`parent_id_key_bytes` は `IdKeyed` 限定、`Allocated` では `id` を通常列として扱う | #2194 |
| スカラー二次索引の候補（slot → id） | 物理 id | `RowId`。`id` 単純比較の候補削減は `IdKeyed` 限定、`Allocated` は PK 索引／通常列索引経由 | #2194 |
| ベクトル arena（`ids`）・HNSW ノード対応・GPU バッチ・疎索引の doc id・RRF の融合キー・rerank・可視ビットマップ／可視行キャッシュ | u64 id | `RowId`。Fast tier の `SUM/MIN/MAX(id)` をキーから計算する経路は `IdKeyed` 限定（`Allocated` は通常列として decode tier を上げる） | #2195 |
| `operation_id` 台帳 | `(tenant, table, operation_id)` | キー・値レイアウト不変（rowid を持たない） | #2196 |
| `content_hash` | 材料にクライアント送信の `id` | **ハッシュ材料に rowid を絶対に入れない**（再送で新しい rowid が割り当たり `23505` 判定が壊れるため）。材料はクライアント送信値のみで、`IdKeyed` 表の既存バイトレイアウトは不変 → 旧台帳エントリは移行後もバイト一致（ポインタ: RECOVER-10） | #2196 |
| 分割実行 DML のカーソル | 物理キー `(tenant, id)` | 物理キー `(tenant, rowid)` の昇順のまま。PK は UPDATE で変わり得るためカーソルに使わない（[partitioned-dml.md](partitioned-dml.md) のハロウィン問題の論証を継承）。`IdKeyed` 表の永続ジョブ状態はそのまま有効、`Allocated` の新規行は単調採番によりカーソルより後ろに入る（ポインタ: SQL-32） | #2196 |
| 一時表 | 永続と同一レイアウト・同一実行器 | 同じモード規則に従う | #2193 |
| matview 実体 | `(context_key, u64)` | u64 は実体内の連番で行参照ではないため対象外 | なし |
| 旧 `rows` 表（Rust API 専用・カタログ外） | 同じキー型 | `IdKeyed` 固定。扱いは未決事項（§6） | #2193 |

### D9 同点順序

「識別（identity）」と「順序キー（order key）」を分離する。検索カーネル（`kernel.rs`・`hnsw.rs`・
`sparse.rs`・`hybrid.rs`・`rerank.rs`・GPU）の同点比較は従来どおり**スロットごとの u64** で行い、
型付き複合 PK をホットループで比較しない。u64 の中身は次のとおり。

| 表の状態 | 順序キー |
| -------- | -------- |
| `IdKeyed` | `id`（= rowid。現行と完全一致） |
| PK なし | rowid |
| 単一整数列 PK | 順序保存の u64 写像 |
| それ以外の PK（TEXT・複合等） | テーブル世代ごとに構築する PK 順位（その列の `ORDER BY` と同じ比較器で順位付け） |

これにより top-k の打ち切り境界も PK 順で決定的になる。`ORDER BY` の同値は PK 列（PK なしは
rowid）を暗黙の最終ソートキーに付加する。不変条件「安定ソート・`sort_unstable_by` 不使用」
（`check_sort_determinism.sh`、[rrf-tie-break-determinism.md](rrf-tie-break-determinism.md)）は維持する。増分更新経路で順位を
保守できない表は全再構築へフォールバックし、コストは #2195 で実測して記録する（ポインタ:
TABLE-23・SEARCH-1・SQL-4）。

### D10 既存 DB の移行・後方互換・ロールバック不可範囲

- #2193〜#2196 の間は全表 `IdKeyed` のままで、物理キー・行値・カタログ・台帳のバイト列は不変
  （旧バイナリで開ける）
- #2200 の移行: 既存行の rowid = 旧 `id`（物理キー再書き込みなし）。`id` を通常の BIGINT 列 + PK
  としてカタログに登録し `Allocated` へ切替える。旧行（行値に `id` スロットを持たない）は
  「`id` = rowid」と復元するデコード規則をカタログ属性で示し、移行後に書いた行は `id` を値に持つ。
  採番カウンタは各テナントの既存最大 + 1 から始める（D4）
- 宣言済みの非 `id` PK を持つ表は、宣言 PK を PK のまま残し、`id` は NOT NULL + UNIQUE として
  残す（`ON CONFLICT (id)` を維持）。この扱いは spec 側確認事項（§6）
- 旧バイナリの扱い: キー型が不変のため `TableTypeMismatch` は発火せず、旧バイナリは `Allocated`
  表の rowid を `id` と誤読し得る（fail-open）。これを**新カタログ版の未知版拒否（fail-closed）**
  で防ぐ。拒否が表単位か DB 起動単位かは #2193 で確認して本書に追記する義務を課す
- ロールバック不可の境界: いずれかの表が新カタログ版で書かれた時点（#2199 の新形式表の作成、
  または #2200 の移行 commit）以降は旧バイナリへ戻せない。移行前バックアップ手順は #2200 で運用文書化する

### D11 テナント境界

採番カウンタ・PK 索引・順位表はすべてサーバー導出テナントで名前空間化する。一意性判定の母集合
（可視性を問わない自テナント全行）とエラーの固定文言は現行契約
（[sql-primary-key.md](sql-primary-key.md)・[unique-index.md](unique-index.md)）を維持する（ポインタ: RLS-9・RLS-10）。

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

- **#2193**: D1（`RowId` 型）・D2（キー型不変）・D3（モード属性と新カタログ版）・D4（採番器・
  テーブル名・初期化・枯渇・DROP 時削除）・D5（公開 API 棚卸しと BREAKING CHANGE）。
  全表 `IdKeyed` のまま挙動不変で crash-test 群が通ることが前提。`Allocated` は engine テスト（Rust API）で検証する
- **#2194**: D6（PK = `user_uniq` 索引・点アドレス 2 段）と D8 の索引・FK 項。`IdKeyed` 限定の
  高速経路の分岐。索引あり／なしの結果一致と `23503`／`23505` の不変
- **#2195**: D8 の検索系項と D9（順序キー）。`IdKeyed` 表で結果完全一致、Recall 層 A の再計測
- **#2196**: D8 の台帳・`content_hash`（rowid 不使用の不変条件）・分割実行 DML カーソル。
  旧台帳エントリの再送判定が不変であること
- 前方ポインタのみ: #2197／#2199（`id` 予約の解除・新形式表で `Allocated` を既定化）、#2200
  （D10 の移行）、#1706（`ON CONFLICT (<PK 列>)`）、#1693／#1696（SERIAL／IDENTITY・移行後 `id`
  の採番。ポインタ: TABLE-24）

## 6. セキュリティ・未決事項

セキュリティ上の要点:

- rowid を応答・EXPLAIN・エラー・ログに出さない（D5）。挿入順や存在情報の推測材料を与えない
- 採番カウンタ・PK 索引・順位表をテナントで名前空間化し、一意性の母集合を維持する（D11）
- 旧バイナリの誤読は新カタログ版の未知版拒否で fail-closed にする（D10）。枯渇・カウンタ破損も fail-closed（D4）

未決・確認事項（自動運転のため安全側の暫定方針で進め、各 sub-issue で確定する）:

| 事項 | 暫定方針 | 確定先 |
| ---- | -------- | ------ |
| 宣言 PK あり既存表の `id` の扱い | 宣言 PK を残し `id` は NOT NULL + UNIQUE（D10）。spec 側確認事項 | #2200 |
| 旧 `rows` 表の扱い | `IdKeyed` 固定 | #2193 |
| 新カタログ版の未知版拒否の粒度（表単位か起動単位か） | 起動単位（より厳格な側） | #2193 |
| rowid 枯渇時の SQLSTATE | 既存エラー契約から選定 | #2193 |
