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
CREATE INDEX <name> ON <table> ( <col> [COLLATE <名前>][, ...] )   -- スカラー宣言（COLLATE は Issue #2191）
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
| 索引列の `COLLATE` の名前が許可リストに無い（構文段。カタログを参照しない。Issue #2191） | `42704` | 400 |
| 照合を持てない列（`id`・数値・`VECTOR` 等）・`USING hnsw` への `COLLATE`（カタログ段。Issue #2191） | `42804` | 400 |
| `USING` が `hnsw`・`btree` 以外（`bm25`・`hash`・`gin` 等）／`USING hnsw` の複数列・非 `VECTOR` 列（`id` を含む）／スカラー宣言が `VECTOR` 列・索引化非対応の型／`USING hnsw`・`COLLATE` を伴う部分索引（`WHERE`。決定的な述語の部分索引は Issue #1654、部分 UNIQUE は Issue #1657 で受理。非決定は `42P17`・集計 `42803`・ウィンドウ `42P20` は [partial-index.md](partial-index.md)）／リテラルの列指定／式要素と `UNIQUE`・`USING hnsw`・`COLLATE` の組み合わせ／索引化できない式の結果型（Issue #1648） | `0A000` | 501 |
| 名前の無い `CREATE [UNIQUE] INDEX`／`ASC`・`DESC`／列の重複指定／`DROP` の複数名・`DROP INDEX ... CASCADE`／`(col + 1)` 等の余分なトークン | `42601` | 400 |
| 列リストの要素数上限（256）超過・索引宣言の総数上限（10,000）超過 | `54000` | 413 |
| DDL 実行権限なし | `42501` | 403 |

`42704`／`42703` の HTTP 射影は 400（ERR-4・ERR-6 ポインタ。`error_format.rs` の実装どおり）。
NoSQL 表層の `op` 許可リストには索引 DDL が無く、両分類とも NoSQL の実要求からは
到達しない（射影は production の応答エンコーダ経由でのみ固定する）。

スカラー宣言で索引化対応とみなす列型は `TEXT`／`ENUM`／`DATE`／`TIMESTAMP`／
`NUMERIC`／`UUID`／`BYTEA`／`INTEGER`／`BIGINT`／`REAL`／`DOUBLE`（および暗黙列 `id`。
`BYTEA` は Issue #1362、数値 4 型は Issue #1413）に限る。`BOOLEAN`／`JSON(B)`／
`ARRAY` は、宣言しても `ScalarIndex` に一切効かない状態を作らないため `0A000` で拒否する。

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
- UNIQUE 制約の名前は relation 名前空間ではなく、テーブル単位の制約名前空間に属する。`CREATE UNIQUE INDEX` との衝突規則は末尾「Issue #1616 追記」の表に従う。
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
  `--search-engine hnsw*` opt-in 下でも、HNSW 適格性ゲートはテーブル単位のため
  HNSW 宣言の追加・削除は他テーブルの Top-k を変えない。既定の `--hnsw-scope all`
  では宣言したテーブル自身の Top-k も変えず、`--hnsw-scope declared` では宣言した
  テーブル自身の探索方式だけが近似／厳密の間で切り替わる
  （`crates/engine/tests/index_declaration_targets.rs`・
  [index-declaration-effects.md](index-declaration-effects.md) を参照）。

**索引宣言の構築対象への反映**（本ドキュメント初版で「対象外」としていた事項）は
Issue #1065 で実装済み。詳細・設計判断は
[index-declaration-effects.md](index-declaration-effects.md) を参照。

## 対象外（申し送り）

1. **`EXPLAIN` への索引名露出**: `scalar_plan:`／`ann_plan:` 行への索引名（宣言名）
   そのものの追記は対象外のまま（引き続き未実装）。`ann_plan:` の**判定結果**
   （HNSW／brute-force のいずれで実行されるか）は Issue #1065 で実行時判定と同じ
   `catalog::hnsw_targeted_in_txn` を経由する。このゲートはテーブル単位のため、
   HNSW 宣言（`CREATE INDEX ... USING hnsw`）の作成・削除が他テーブルの `ann_plan:`
   表示を変えることはなく、既定の `--hnsw-scope all` では宣言したテーブル自身の
   表示も変わらない（`--hnsw-scope declared` では宣言したテーブル自身の表示だけが
   `hnsw_*`／`plain_scan_engine` の間で切り替わる。索引カタログの読み取り失敗時は
   brute-force 表示へ倒す）。`scalar_plan:` は束縛時の静的判定のまま宣言の影響を
   受けない（[index-declaration-effects.md](index-declaration-effects.md) 参照）。
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

## 追記（Issue #2191・TABLE-28）: 索引列の `COLLATE`

列リストの各要素は `<識別子> [COLLATE <照合名>]`。照合名は固定の許可リスト（`C`・`POSIX`・`und-x-icu`・`ja-x-icu`）の完全一致で解決する。二重の `COLLATE` は `42601`、式索引は従来どおり `0A000`。カタログは照合を明示した列があるときだけ `v2`（5 行目に列ごとの照合トークン）で書き、無いときは従来の `v1` とバイト単位で同一。設計と判断は [collation-aware-index.md](collation-aware-index.md)。

## Issue #1613 追記（TABLE-36）: `IF NOT EXISTS`

- 名前付きの `CREATE INDEX IF NOT EXISTS <name> ON ...` を受理する。名前を省いた
  `CREATE INDEX IF NOT EXISTS ON ...` は構文段で `42601`（カタログ非参照）。
- `Storage::create_index` の書き込みトランザクションは名前衝突を最初に判定し、commit・世代 bump
  の前に返す。`IF NOT EXISTS` では `IndexAlreadyExists` を no-op 成功に写す。名前衝突が最初の
  判定のため、同名があれば対象テーブル・列が不正でも成功する（定義は比較しない）。
- 同名の表・ビューがある場合も no-op。句なしの重複（`42P07`）は不変。

## Issue #1616 追記（INDEX-9）: `CREATE UNIQUE INDEX`

`CREATE UNIQUE INDEX [IF NOT EXISTS] <name> ON <t> (<cols>)` を受理し、内部では UNIQUE 制約の宣言へ展開する。
一意性の強制・`ON CONFLICT (<列>)` の推論・FK の参照先判定はすべて `TableSchema::unique_constraints()` を入力に
するため、制約として登録すれば既存の仕組みがそのまま効く（強制漏れ＝fail-open を作らない）。

- **表現**: 同一 write txn で (a) 対象テーブルのスキーマに索引名と同名の UNIQUE 制約を追加し、(b) 索引カタログに
  `unique == true` の宣言を書く。`IndexDef` に公開フィールド `unique` を追加した（破壊的変更）。
  索引カタログは `unique` のときだけ新形式 `v3`（5 行目に固定フラグ `unique`）で書き、`v1`・`v2` のバイト列は不変。
  TableSchema のカタログ形式は変更しない。
- **作成の判定順**（`Storage::create_index`。データ依存は最後の重複走査のみ）: 形状 → relation 名前空間（`42P07`）→
  対象の種別（`42809`）→ 表（`42P01`）→ 列（`42703`）→ 型ゲート（`0A000`）→ 制約名前空間（`42P07`）→ 制約数上限
  （`54000`）→ 追加後スキーマの検証（同じ列並びの既存 UNIQUE は `42601`）→ 全テナント・全可視性の既存行をテナントごとに
  独立して走査（重複は `23505`。commit せず破棄するので索引も制約も残らない）→ 保存。
- **削除**: `DROP INDEX` が同一 txn で同名の UNIQUE 制約も外し、永続一意索引を無効化して索引衛生を行う。
  FK の参照先になっている間は `2BP01`（他の UNIQUE・PK が同じ列集合を覆っていても救済しない）。
  `ALTER TABLE DROP CONSTRAINT <索引名>` は `42704`（索引由来の UNIQUE は制約として扱わない）。
  `ON CONFLICT ON CONSTRAINT <名>` は従来どおり `42601`。
- **未対応の形（fail-closed）**: `USING hnsw`・`COLLATE`（一意キーの正準化は照合を見ない）は `0A000`（構文段）。
  暗黙の `id` 列・`BOOLEAN`／`JSON(B)`／`ARRAY` など一意キー許可型または索引化対応型の外は `0A000`（カタログ段）。
  名前を省いた形は `42601`。`NULLS NOT DISTINCT`・`INCLUDE`・`CONCURRENTLY` は構文として受理しない。
  エラー文言は固定で、テナント・値・件数を含めない。

名前空間の衝突規則:

| 操作 | 衝突相手 | 結果 |
| ---- | -------- | ---- |
| `CREATE UNIQUE INDEX n` | テーブル・ビュー・索引 `n` | `42P07`（`IF NOT EXISTS` なら no-op） |
| `CREATE UNIQUE INDEX n ON t` | `t` の UNIQUE・CHECK・FK・PK 実効名 `n` | `42P07`（`IF NOT EXISTS` でも拒否） |
| `CREATE UNIQUE INDEX n ON t` | 他テーブルの制約名 `n` | 許可（制約名はテーブル単位。PG とは異なる） |
| `ALTER TABLE t ADD [CONSTRAINT n] UNIQUE` | `t` の unique 索引 `n` | 明示名は `42P07`、省略時は既定名が衝突を回避 |
| `ALTER TABLE t ADD CONSTRAINT n CHECK/FK` | `t` の unique 索引 `n` | 既存の `42710`（PG は許可。fail-closed 側の差分） |
| `DROP CONSTRAINT n`（n は unique 索引） | — | `42704` |

PG との差分（いずれも fail-closed 側）: 上記の未対応型・`COLLATE`・`id`、既存 UNIQUE と同じ列並びの unique 索引、
unique 索引の構成列への `DROP COLUMN`（PG は索引ごと削除するが、本実装は UNIQUE 制約の既存規則で拒否）。

対象外: 名前を省いた `CREATE [UNIQUE] INDEX ON ...` の自動命名、`pg_indexes`／`pg_constraint` 相当の照会手段。

テスト: `crates/engine/tests/sql_unique_index.rs`（結合）・`crates/wire-server/tests/wire_index_ddl.rs`（層 A）・
`crates/engine/examples/crash_tool_unique_index.rs`（`make crash-test-unique-index` が `docs_uidx` で検証）。

## Issue #1617 追記（INDEX-9・TABLE-36）: `USING btree`

- `CREATE INDEX [IF NOT EXISTS] <name> ON <table> [USING btree｜hnsw] (...)` の `USING btree` を、
  `USING` 省略時と同一のスカラー宣言として受理する（構文段のみの変更。`ValidatedCreateIndex` の形は不変）。
- カタログ表現は不変。`btree` はディスク上の種別トークンにならず、`IndexKind::Scalar` のまま書く
  （カタログ側の `btree` 種別トークンは従来どおり破損として検出する）。
- `hnsw`・`btree` 以外の手法（`bm25`・`hash`・`gin` 等）は従来どおり `0A000`。
- 名前付き `IF NOT EXISTS` との併用が可能。名前なしは `42601` のまま。

## Issue #1647 追記（INDEX-10: 式索引の宣言側）

エラー分類表の「式・関数呼び出し」行は、式の種類により `42P17`（非決定式）・`42803`（集計）・
`42P20`（ウィンドウ）・`0A000`（部分クエリ・SRF・組み込み以外の関数・決定的な式の最終ゲート）に分かれる。
Issue #1647 時点では式索引を作成できなかった（宣言を永続化しない）。判定表・判定順・PostgreSQL との差・後続 Issue への
申し送りは [expression-index.md](expression-index.md) を参照。

## Issue #1648 追記（INDEX-10: 式索引の格納側）

決定的な式の索引は `CREATE INDEX` で作成できる。式は write txn 内で束縛され、正規化 SQL テキストと参照列を
カタログ版 `v4` で永続化する（式を含まない宣言の `v1`／`v2`／`v3` は不変。旧バイナリは `v4` を読めない）。
`UNIQUE`・`USING hnsw`・式への `COLLATE` との組み合わせは引き続き `0A000`。参照列の `DROP COLUMN` は式索引を
連鎖削除し、`RENAME COLUMN`／`ALTER COLUMN TYPE` は `0A000` で拒否する。式キーは起動時 opt-in が有効なとき
`ScalarIndex` の構築時に作られるが、現時点ではクエリから消費されない（プランナ選択は #1650）。詳細は
[expression-index.md](expression-index.md) の「Issue #1648」節。
