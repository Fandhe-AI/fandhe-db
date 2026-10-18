# `DROP TABLE` と DDL 実行権限ゲートの設計判断

Issue #902・対象ビヘイビア: SQL-23・TABLE-15（TASK-203）。関連ポインタ:
RECOVER-2（`operation_id` 台帳のテーブル単位削除）・RLS-9（他テナント存在情報の
非漏えい）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
DROP TABLE [IF EXISTS] <table> [, <table>]*
```

- `IF EXISTS` は Issue #1614 で受理に変わった（末尾の「Issue #1614 追記」参照）。
- 複数テーブル列挙は Issue #1639 で受理に変わった（末尾の「Issue #1639 追記」参照）。
- `CASCADE`・`RESTRICT`・`USING OPERATION_ID` 句は
  いずれも構造的に受理しない（許可リスト外・`42601`）。SQL-8 の「受理形以外は
  `42601`」方針に従う。`CASCADE` は spec でも対象外と明示されている。
- `EXPLAIN DROP TABLE ...` は既存の `EXPLAIN` 分岐（次トークンが `SELECT` である
  ことを要求）へ流れて自然に `42601` になる（`TRUNCATE`・`INSERT` と同じ経路）。
- `operation_id` を要求しない——`CREATE TABLE`・`ALTER TABLE` と同じく、DDL は
  台帳（`op_ledger`）の対象外である。`DROP TABLE` 自体はテーブル単位で台帳
  エントリを丸ごと削除する側であり（`catalog::Storage::drop_table`）、自身の
  再送判定対象にはならない。
- セッションを持たない後方互換 API（`EngineCore::execute_sql`）は `DROP TABLE` を
  一律 `42601` で拒否する。DDL 実行権限はセッション単位の状態
  （`SessionState::ddl_allowed`）のため、セッションを持たない入口では原理的に
  権限を評価できない（`SET`・`CREATE FUNCTION` と同じ契約）。

## DDL 実行権限ゲート（SQL-23）: 単一の判定点

`DROP TABLE` はカタログ定義・全テナントの行・台帳エントリを不可逆に削除する
DDL であり、`TRUNCATE`（テナントスコープの書き込み系操作）とは異なり
`PolicyContext` を取らない（`catalog::Storage::drop_table` は元々全テナント対象の
生 API として実装済みだった）。ゲートなしで untrusted 経路（SQL 表層・
wire-server）へ配線すると、任意の認証済みテナントが他テナントのデータを含む
テーブル全体を削除できてしまう。

- **engine 側**: `sql::mode::SessionState` に `ddl_allowed: bool`（既定
  `false`）を追加し、`allow_ddl()`／`ddl_allowed()` のみを公開する。
  `sql::ddl::require_ddl_permission(session)` が DDL 実行権限の**唯一の判定点**
  であり、将来の `CREATE TABLE`（#899）・`ALTER TABLE`（#900・#901）・`VIEW`
  （#909）・`FOREIGN KEY`（#907）等の DDL もここを経由する想定で設計した。
- **`PolicyContext` ではなく `SessionState` に持たせた理由**: `PolicyContext` は
  テナント ID と可視性のみを運び認証主体（username）を持たない。DDL 権限は
  テナント境界とは別軸の権限（テーブル・カタログは全テナント共有であり、
  「どのテナントが実行したか」ではなく「どの認証主体が実行したか」で判定する
  必要がある）。
- **wire-server 側**: 許可主体の集合は `UserStore`（`--users` で読み込む
  ユーザーストア）に `ddl_allowed: HashSet<String>` として持たせ、
  `UserStore::with_ddl_allowed_users`（`--ddl-allowed-users` からのみ適用する
  opt-in）・`UserStore::is_ddl_allowed(username)` を公開する。`handshake.rs` が
  `auth::verify` 成功直後（username が確定した時点）に 1 回だけ
  `session.allow_ddl()` を呼ぶ。SQL 文経由でセッションが自身の権限を昇格する
  経路は構造的に存在しない。
- **CLI**: `--ddl-allowed-users <user1>[,<user2>...]` を追加した。値の解決は
  `wire_server::ddl_permission_opt::parse`（カンマ分割・空要素/重複要素の拒否）
  に一本化し、`--search-engine`・`--durability` と同型の「起動後に変更できない
  構成値」として 2 回目以降の指定を fail-closed に拒否する。未指定なら
  `UserStore::with_ddl_allowed_users` を呼ばず、DDL 実行権限を持つユーザーが
  0 人のまま（全 DDL 文が `42501` で拒否される既定）。列挙した username が
  ユーザーストアに実在しない場合は起動時エラー（fail-closed。typo による
  意図しない権限未付与を検出する）。
- **HTTP（NoSQL）表層**: SQL テキストを実行しないため対象外。`op: drop_table`
  は op 許可リスト外（`0A000`）のまま変更しない（NOSQL-13・#910 の担当）。
  HTTP セッションの `SessionState` は既定のまま（DDL 拒否）。

## エラー分類: `ForbiddenTenantMismatch`（`42501`）を再利用

`SqlSurfaceError::InsufficientPrivilege` を新設し、既存の
`ErrorClass::ForbiddenTenantMismatch`（`42501`）へ写像する。新しい `ErrorClass`
分類は追加していない——`ErrorClass` は「`wire_code` への一意対応」を型で保証する
単位であり（`error_format.rs` の `define_error_classes!` マクロが `count` の
不一致でコンパイルを失敗させる）、分類の**名前**ではなく **`wire_code`** が
確定契約である。テナント帰属不一致と DDL 実行権限不足は原因が異なるが、
`wire_code` は同じ `42501`（PostgreSQL の `insufficient_privilege` 系）が自然に
対応するため、新規分類を増やさずラベルの意味を一般化する形を選んだ
（`error_format.rs` の該当ドキュメンテーションコメントを更新済み）。

`client_message()` は固定文言 `"permission denied for DDL statement"` のみを
返し、テーブル名・username を一切含めない。

## 判定順序（fail-closed）: 構文 → 権限 → 存在

1. **構文検証**（`sql::allowlist::validate_drop_table_tokens`）。カタログ照会を
   一切行わない。`CASCADE` 等は許可リスト外として `42601`（`IF EXISTS` は Issue #1614 で受理）。
2. **DDL 実行権限ゲート**（`sql::ddl::require_ddl_permission`）。カタログ照会・
   書き込みトランザクション開始のいずれよりも前に判定する。`42501`。
3. **実行本体**（`sql::ddl::execute_drop_table` → `catalog::Storage::
   drop_table`）。書き込みトランザクション内で対象テーブルの存在を判定する
   （事前の `table_exists` 照会を行わず TOCTOU を避ける）。不在は `42P01`。

この順序により、**DDL 実行権限を持たない主体には、対象テーブルの有無を問わず
常に同一の `42501` 応答が返る**（存在するテーブルへの `DROP TABLE` も、存在しない
テーブル名への `DROP TABLE` も区別できない）。DDL 権限をテーブル存在のオラクルに
しないための意図的な設計判断であり、`crates/engine/tests/sql_drop_table.rs::
drop_table_without_permission_is_rejected_with_42501_regardless_of_table_existence`
で固定している。

## 応答: 件数を返さない

`DropTableOutcome` はフィールドを一切持たない構造体（`TruncateOutcome` と同型）。
`DROP TABLE` は全テナント分の行を削除する操作であり、件数を返すと他テナントの
行数を推測できてしまう（RLS-9 と同じ判断）。`CommandComplete` タグは固定文字列
`DROP TABLE`（`TRUNCATE TABLE` と同じ設計）。

## キャッシュ失効: 世代カウンタへの一本化

`DROP TABLE` は `catalog::Storage::drop_table` 内で `bump_table_generation_in_txn`
を呼ぶ（既存実装。本 Issue で変更していない）。SQL 表層の各種インメモリ
キャッシュ（`SqlArenaCache`〔#363〕・`SparseIndexCache`〔#357〕・
`ScalarIndexCache`〔#473〕・`VisibleBitmapCache`〔#478〕・HNSW 索引キャッシュ
〔#408〕・`PrefilterCache`〔TASK-169〕・`DictionaryCache`）はいずれもテーブル
単位世代との照合による fail-closed 失効契約（Issue #280 で統一済み）を持つため、
`DROP TABLE` 専用の新しい失効機構は追加していない。世代が進行した時点で
すべてのキャッシュエントリが構造的に無効化され、同名テーブルを再作成した後の
クエリは新しいスナップショットから再構築される。drop 済みテーブルのキャッシュ
エントリを能動的に解放する処理は実装していない（各キャッシュの容量上限に
より有界のまま維持され、正しさは世代照合が担保する。能動的解放はスコープ外）。

## `2BP01`（VIEW からの参照）実装済み（Issue #909）

`CREATE VIEW`／`DROP VIEW`（TABLE-18・SQL-23・TASK-205）の実装により、
`Storage::drop_table` は対象テーブルを参照するビューが 1 つでも残っている場合
`CatalogError::DependentViewsExist` → `SqlSurfaceError::DependentObjectsStillExist`
（`2BP01`）で拒否するようになった（同一 write txn 内・TOCTOU 回避。詳細は
`docs/design/create-view.md` 参照）。対象名がビューだった場合は
`CatalogError::WrongObjectKind` → `SqlSurfaceError::WrongObjectType`（`42809`）。

## `2BP01`（FOREIGN KEY からの参照）実装済み（Issue #907）

`FOREIGN KEY`（TABLE-17・TASK-205）の実装により、`Storage::drop_table` は
他テーブルの `FOREIGN KEY` が対象テーブルを参照先にしている場合、データの
有無を問わずカタログ情報のみで `CatalogError::DependentObjectsStillExist` →
`SqlSurfaceError::DependentObjectsStillExist`（`2BP01`）として拒否する
（同一 write txn 内。自己参照は参照元ごと削除されるため依存に数えない。詳細は
`docs/design/foreign-key.md` 参照）。

## DROP 途中の同期点（Issue #1363）

- DROP とクエリの実行区間の重なりを決定的に作るため、feature `test-sync-points`（既定オフ・依存なし・wire-server では無効）で 2 つの同期点を設けた。`Storage::drop_table` の commit 直前（`DropTableBeforeCommit`）と、読み取り文の開始時点＝スナップショット確定後（`ReadStatementSnapshotAcquired`）である。
- コールバックへ渡すのは種別だけで、行・テナント・テーブル名は渡さない。RLS・権限ゲート・fail-closed の経路には触れない。コールバックはロックを解放してから呼ぶ。
- テスト（`tests/table15_drop_table_concurrency.rs` の `sync_point_overlap`）: DROP を commit 前で止めた間のクエリ、クエリをスナップショット確定後で止めた間の DROP、さらに同名の次元違い再作成を挟んだ場合のいずれも、結果は「DROP 前と完全一致」か `42P01` に限る。同期点の発火回数も固定する。

## 対象外・申し送り

- NoSQL 表層の `drop_table` op（#910 の担当）
- `CREATE TABLE`・`ALTER TABLE` の SQL 構文（#899〜#901）
- ドロップ済みテーブルのキャッシュエントリの能動的解放（現状は世代照合による
  遅延失効で正しさは担保済み）
- `ErrorClass::ForbiddenTenantMismatch` の改名・汎化（ラベル変更は別 Issue 相当）

## Issue #1614 追記: `IF EXISTS`（TABLE・INDEX・VIEW・TYPE 共通）

対象ビヘイビア: TABLE-36（親 Issue #1612）。`DROP TABLE|INDEX|VIEW|TYPE IF EXISTS <name>` を
受理する。PostgreSQL 17 の公開挙動に揃え、対象が無ければ何もせず成功する。

- **構文**: `IF`・`EXISTS` は予約語化していないため、`Parser::consume_if_exists` が
  2 トークン先読みで判定する（`DROP TABLE if` のように `if` という名前を指す従来解釈を保つ）。
  `ValidatedDropTable.if_exists`（pub）と `ValidatedDrop{View,Index,Type}::if_exists()` に保持する。
- **no-op の判定は `sql::ddl` 層**: `execute_drop_*` が、`if_exists` かつ対応する
  `*NotFound`（`TableNotFound`／`ViewNotFound`／`IndexNotFound`／`TypeNotFound`）の場合だけ
  成功に読み替える。`Storage::drop_*` はこれらを commit 前に返し write txn が破棄されるため、
  カタログ・行・`operation_id` 台帳・各世代は変わらない（副作用ゼロ）。`Storage::drop_*` の
  fail-closed 契約は変えない。
- **従来どおり**: 種類違い（`42809`）・依存あり（`2BP01`）・`55P03` 等は読み替えない。
  判定順序（構文 → DDL 権限 `42501` → 存在）も不変で、権限の無いセッションは存否に関わらず
  `42501`（no-op を存在オラクルにしない）。明示トランザクション内の DDL は `0A000`。
- **成功応答**: `Drop*Outcome::skipped()` で no-op かを取得できる。`CommandComplete` タグは
  通常の DROP と同一。
- **対象外**: NoticeResponse（`... does not exist, skipping`）の送出（wire-server に encoder が
  無く後続課題）、複数名・`CASCADE`／`RESTRICT`、NoSQL 表層 `drop_table` op への `if_exists` 追加。
- **破壊的変更**: `ValidatedDropTable` へのフィールド追加と `Drop*Outcome` の非公開フィールド追加
  により、crate 外での構造体リテラル構築ができなくなる。

## Issue #1639 追記: 複数テーブル指定（`DROP TABLE a, b`）

対象ビヘイビア: TABLE-37 (1)・(6)（親 Issue #1638）。`DROP TABLE [IF EXISTS] a, b, ...` を受理し、
文全体を 1 つの write txn・1 回の commit で原子的に実行する。

- **構文**: `ValidatedDropTable.table_names: Vec<String>`（非空・記述順。旧 `table_name` を置き換えた
  破壊的変更）。末尾カンマ・空要素・カンマ無しの並びは `42601`。`CASCADE`／`RESTRICT` は従来どおり `42601`。
- **実行**: `Storage::drop_tables(&[String], if_exists) -> DropTablesReport`。`Storage::drop_table` は
  その単一表 wrapper で、エラー variant・同期点の発火回数・世代 bump は従来どおり。
- **判定順（決定的）**: 名前を記述順に解決（種類違い `42809`・不在 `42P01`。`IF EXISTS` は不在だけ skip）→
  依存（`2BP01`）→ 削除。どこで失敗しても commit 前に txn を破棄し何も削除しない。全件 skip なら
  commit せず世代も不変（`DropTableOutcome::skipped()` は「1 件も削除しなかった」）。
- **依存**: カタログ情報だけを単一パスで調べる（ビュー本文の参照、削除対象外の表の `FOREIGN KEY`）。
  同じ文に並べた名前どうしの参照（自己参照を含む）は数えない。参照元の表名・テナント情報はエラーに含めない。
  FK 保持版の接頭辞は既存と同じ `FK_BEARING_FORMAT_VERSIONS` を使う（見落とすと fail-open）。
- **重複名**: 初出の 1 回として扱う。表世代の bump も 1 回。
- **権限**: ゲートは名前数に関係なく 1 回、存在確認より前に通す（存否のオラクルにならない）。
  明示トランザクション内は `0A000`。
- **NoSQL 表層**: `drop_table` op は単一の表のまま（TABLE-37 (6)。JSON の `table` は 1 トークン限定）。
- **対象外**: `CASCADE`／`RESTRICT`（#1641）、依存オブジェクトの汎用列挙（#1640）、複数指定の
  `DROP VIEW`／`INDEX`／`TYPE`、skip した名前ごとの NoticeResponse。
