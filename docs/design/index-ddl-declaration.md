# `CREATE INDEX` / `DROP INDEX` 宣言構文（Issue #908・TASK-206・INDEX-7）

## ステータス

Accepted（実装済み。宣言の効果の結線は Issue #1065 で実装済み。詳細は
[index-declaration-effects.md](index-declaration-effects.md) 参照）。

## ポインタ

- spec: `docs/spec/05-tasks.md` TASK-206・`docs/spec/04-behavior/indexing.md` INDEX-7
- 関連ビヘイビア: SQL-23（DDL 実行権限ゲート）・ERR-6（`42P07`／`42704`／`42703`／
  `42809` の分類行）・ERR-4（HTTP 射影）・SQL-31（明示トランザクション）・
  TABLE-18（ビュー）・INDEX-5／6・CORE-9／10・SQL-6（`EXPLAIN`）

spec 本文はここへ転記しない（`.claude/rules/spec-confidentiality.md`）。以下は本リポの
実装既定値・設計判断の記録。

## 背景・目的

既存の索引はすべて自動構築（スカラー列二次索引・疎索引）か起動時 opt-in
（`--search-engine hnsw*`）で、「この列に索引を作る」と宣言する DDL が無かった。
本 Issue で次の 3 文を SQL 表層へ追加した。

```text
CREATE INDEX <name> ON <table> ( <col>[, <col>...] )   -- スカラー宣言
CREATE INDEX <name> ON <table> USING hnsw ( <col> )     -- HNSW 宣言（VECTOR 列 1 本）
DROP INDEX <name>
```

宣言はスキーマカタログ（`redb` の `index_catalog` テーブル）への永続化のみを行い、
行データには一切触れない（実行時間は行数に依存しない）。

## 責務境界

| 層 | 責務 |
| --- | --- |
| `sql::allowlist::validate_create_index_tokens`／`validate_drop_index_tokens` | 構文の許可形状判定（`0A000`／`42601`／`54000`）。カタログを参照しない |
| `core.rs::EngineCore::parse_tokens` | 先頭 2 トークンの覗き見で `ParsedSql::CreateIndex`／`DropIndex` へ束縛（`DROP INDEX` は汎用の `DROP TABLE` 分岐より前） |
| `core.rs::EngineCore::execute_parsed_in_session` | `sql::ddl::require_ddl_permission`（全 DDL 共通の唯一の権限判定点）→ 実行本体の順で呼ぶ |
| `sql::ddl::execute_create_index`／`execute_drop_index` | `CatalogError` を SQL 表層の契約（ERR-6）へ写像 |
| `catalog::Storage::create_index`／`drop_index` | 単一 write txn 内で名前衝突・対象の種別と存在・列整合・件数上限を判定して保存し、対象テーブルの世代を進める |
| 既存の索引キャッシュ（`sql::scalar_index::ScalarIndexCache`・`sql::hnsw_cache::HnswIndexCache`） | 索引の物理表現と構築。宣言の構築対象への反映は Issue #1065（[index-declaration-effects.md](index-declaration-effects.md)）参照 |

「カタログ＝宣言だけを持つ、キャッシュ＝物理表現と構築」という分離を維持し、
新しい索引の物理表現や第 2 の実行器は作らない。

## 判定順序（fail-closed・決定的）

`CREATE TABLE`／`DROP TABLE`／`CREATE VIEW`／`DROP VIEW` と同じ流儀とする。

1. 字句解析・構造検証（カタログを参照しない。`42601`／構文由来の `0A000`／`54000`）
2. DDL 実行権限ゲート（`sql::ddl::require_ddl_permission`。`42501`）。カタログ照会・
   書き込みトランザクション開始のいずれよりも前に判定し、権限を持たない主体には
   対象テーブル・索引の有無を問わず同一の `42501` を返す
3. 実行本体（`Storage::create_index`／`drop_index`）。以下をすべて同一 write txn 内で
   判定する（TOCTOU なし）

`CREATE INDEX` の txn 内判定順序: 索引名の衝突（テーブル・ビュー・既存索引のいずれか。
`42P07`）→ 対象がビュー・既存索引（`42809`）→ 対象テーブルの不在（`42P01`）→ 列の不在
（`42703`）→ 種別と列型の不整合（`0A000`）→ 登録件数上限（`54000`）→ 保存・世代 bump。

`DROP INDEX` の txn 内判定: 索引として存在すれば削除・世代 bump。存在しなければ
テーブル・ビュー名なら `42809`、いずれでもなければ `42704`。

明示トランザクション（SQL-31）内の `CREATE INDEX`／`DROP INDEX` は、他の DDL と同じく
`EngineCore::execute_in_active_txn` の既存分岐により `0A000` で拒否される（トランザクション
は失敗状態へ遷移し、カタログは変更されない）。

## エラー分類（実装既定値）

| ケース | wire_code | HTTP 射影 |
| --- | --- | --- |
| 索引名がテーブル・ビュー・既存索引と衝突（relation 名前空間を共有） | `42P07`（`DUPLICATE_TABLE` を共有） | 409（既存行） |
| `CREATE TABLE`／`CREATE VIEW` の名前が既存索引と衝突 | `42P07` | 409 |
| `DROP INDEX` で索引が存在しない | `42704`（`UNDEFINED_OBJECT`。新設） | 400 |
| 参照列が存在しない（`id` は暗黙列として常に有効） | `42703`（`UNDEFINED_COLUMN`。新設） | 400 |
| `CREATE INDEX` の対象がビュー・既存索引、`DROP INDEX` にテーブル・ビュー名、`DROP TABLE`／`DROP VIEW`／`ALTER TABLE ADD COLUMN`／書き込み系 DML（`INSERT`・UPSERT・`UPDATE`・`DELETE`・`TRUNCATE`）に索引名 | `42809` | 400 |
| 対象テーブルが存在しない | `42P01` | 404 |
| `USING` が `hnsw` 以外（`btree`・`bm25` 等）／`USING hnsw` の複数列・非 `VECTOR` 列（`id` を含む）／スカラー宣言が `VECTOR` 列・索引化非対応の型／部分索引の `WHERE`／式・関数呼び出し・リテラルの列指定 | `0A000` | 501 |
| `UNIQUE`／`IF [NOT] EXISTS`／`ASC`・`DESC`／列の重複指定／`DROP` の複数名・`CASCADE`／`(col + 1)` 等の余分なトークン | `42601` | 400 |
| 列リストの要素数上限（256）超過・索引宣言の総数上限（10,000）超過 | `54000` | 413 |
| DDL 実行権限なし | `42501` | 403 |

`42704`／`42703` の HTTP 射影は ERR-4 の射影規則（ERR-6 新設行は 400）に従う。
NoSQL 表層の `op` 許可リストには索引 DDL が無く、両分類とも NoSQL の実要求からは
到達しない（射影は production の応答エンコーダ経由でのみ固定する）。

スカラー宣言で索引化対応とみなす列型は `TEXT`／`ENUM`／`DATE`／`TIMESTAMP`／
`NUMERIC`／`UUID`（および暗黙列 `id`）に限る。`BOOLEAN`／`BYTEA`／`JSON(B)`／
`ARRAY`・未結線の数値型（`INTEGER`／`BIGINT`／`REAL`／`DOUBLE`）は、宣言しても
`ScalarIndex` に一切効かない状態を作らないため `0A000` で拒否する。

## 名前空間とライフサイクル

- 索引名はテーブル名・ビュー名と同じ relation 名前空間を共有する（PostgreSQL と同じ）。
  `Storage::create_table`／`create_view`／`create_index` はいずれも 3 者を同一 write txn
  で確認する。
- `DROP TABLE` は対象テーブルの索引宣言を同一 txn で一掃する（残置した宣言名が同名
  テーブル再作成後の `CREATE INDEX` を無関係に `42P07` で塞ぐ事故を防ぐ。
  `recovery::ledger::delete_table_in_txn` と同じ判断）。
- `Storage::alter_table_drop_column`（Rust API）は、削除する列を含む索引宣言を同一 txn で
  削除する（PostgreSQL の `DROP COLUMN` と同じ扱い。消えた列名を指す宣言が残り、
  後の同名列の再追加で意図せず復活する事故を防ぐ）。
  ただし UNIQUE 制約（TABLE-16、Issue #905）・`PRIMARY KEY` の構成列は `DROP COLUMN`
  自体が拒否されるため、その場合は索引宣言も含め何も変更しない。
- UNIQUE 制約は名前を持たないため、relation 名前空間の衝突対象にはならない。
- 列リストの重複は SQL 表層（構文検証段・`42601`）だけでなく公開 Rust API
  `Storage::create_index` でも `CatalogError::Invalid`（SQL 表層では `42601`）として
  拒否し、読み出し（カタログ値のデコード）時に重複を含む値は破損として fail-closed に
  拒否する。

## 既存キャッシュ・RLS との関係

- `CREATE INDEX`／`DROP INDEX` の成功は対象テーブルの世代
  （`catalog::bump_table_generation_in_txn`）を進める。テーブル単位世代整合キャッシュ
  （`ScalarIndexCache`・`HnswIndexCache`・`SqlArenaCache` 等）は次のクエリで失効・
  再構築され、宣言変更前のキャッシュが残り続ける経路を作らない。
- 索引の物理表現は引き続き `(table, PolicyContext)` 可視スナップショットから構築される
  （索引が他テナントの行を含むことはない）。索引宣言は `PolicyContext` を取らない
  全テナント共有の DDL であり、RLS の暗黙適用を一切変更しない。
- 宣言の作成前後・削除後でテナントごとの検索・述語付き取得・集計の結果が完全に
  一致することを `crates/engine/tests/sql_index_ddl.rs` で固定している。
  `--search-engine hnsw*` opt-in 下でも、HNSW 適格性ゲートはテーブル単位で宣言の
  有無により検索経路を切り替えないため、HNSW 宣言の追加・削除は宣言したテーブル・
  他テーブルのいずれの Top-k も変えない（`crates/engine/tests/index_declaration_targets.rs`・
  [index-declaration-effects.md](index-declaration-effects.md) を参照）。

**索引宣言の構築対象への反映**（本ドキュメント初版で「対象外」としていた事項）は
Issue #1065 で実装済み。詳細・設計判断は
[index-declaration-effects.md](index-declaration-effects.md) を参照。

## 対象外（申し送り）

1. **`EXPLAIN` への索引名露出**: `scalar_plan:`／`ann_plan:` 行への索引名（宣言名）
   そのものの追記は対象外のまま（引き続き未実装）。`ann_plan:` の**判定結果**
   （HNSW／brute-force のいずれで実行されるか）は Issue #1065 で実行時判定と同じ
   `catalog::hnsw_targeted_in_txn` を経由するが、このゲートはテーブル単位で宣言の
   有無により結果を変えない（索引カタログの読み取り失敗時のみ brute-force 表示へ
   倒す）ため、HNSW 宣言（`CREATE INDEX ... USING hnsw`）の作成・削除で `ann_plan:`
   表示は変わらない。`scalar_plan:` は束縛時の静的判定のまま宣言の影響を受けない
   （[index-declaration-effects.md](index-declaration-effects.md) 参照）。
2. **疎索引（BM25）の宣言**: Issue #908 本文は「ベクトル・スカラー・疎」の 3 種別を
   挙げるが、本実装は INDEX-7 のポインタに従いスカラー宣言と `USING hnsw` のみを
   受理し、`USING bm25` 等は `0A000` とする（疎索引は hybrid のたびに自動構築する
   既存契約のまま）。Issue 本文との差分は spec 側の再判断事項として報告する。
3. **NoSQL 表層**: `op` 許可リストに索引 DDL を含めない（NOSQL-13 の担当）。

## 既知の制約

- 読み取り系（`SELECT`・集計・`EXPLAIN`・`CREATE VIEW` の参照先）で索引名を
  `FROM` に指定した場合は、テーブルとして存在しない名前と同じく `42P01` のまま
  （書き込み系 DDL/DML のみ `42809` へ揃えた）。

- 列の直後に二項演算子が続く形（`(col + 1)`）は、列リストの閉じ括弧を期待する検査の
  不一致により `42601` になる（`0A000` ではない）。空の列リスト `()` は列要素の
  検査で `0A000` になる。現状の実装挙動をテストで固定するに留めた。

## テスト

- `crates/engine/src/catalog.rs`（unit）: 世代 bump が対象テーブルのみに効くこと・
  失敗時は bump しないこと、カタログ値の往復と破損の fail-closed 拒否、列整合、
  `DROP TABLE`／`DROP COLUMN` による宣言の掃除。
- `crates/engine/src/sql/ddl.rs`（unit）: `CatalogError` → ERR-6 の写像。
- `crates/engine/tests/sql_index_ddl.rs`（結合）: 権限ゲートの非漏えい・構文の許可形状
  （カタログ非参照）・名前空間と種別・列の判定・明示トランザクション内の `0A000`・
  結果集合と RLS 境界の不変・永続化・`DROP TABLE` による一掃。
- `crates/wire-server/tests/wire_index_ddl.rs`（層 A）: `CommandComplete` タグ・
  `--ddl-allowed-users` のユーザー単位ゲート・ERR-6 分類の wire 経由観測。
- `crates/wire-server/src/http/status.rs`・`crates/wire-server/tests/err4_http_projection.rs`:
  新設 2 分類の HTTP 射影。
