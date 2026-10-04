# `CREATE TABLE` の設計判断

Issue #899・対象ビヘイビア: SQL-23（TASK-85・TASK-202）。関連ポインタ:
TABLE-1（`VECTOR(N)` 列の宣言）・TABLE-2（テーブル間の次元独立）・TABLE-4（O(1)
カタログ変更）・TABLE-6（識別子・型の許可リスト）・ERR-2／ERR-4／ERR-6（`42P07`・
`42701` の分類）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
CREATE TABLE <table> (<col> <type>[, <col> <type>]*) [;]
```

- `<type>` は `ALTER TABLE ADD COLUMN` と共有する型名パーサー（`sql::ddl_column_type`）で
  読む（Issue #1348）。受理するのは、全スカラー型（`TEXT`・`INTEGER`・`BIGINT`・`REAL`・
  `DOUBLE PRECISION`・`BOOLEAN`・`DATE`・`TIMESTAMP`・`BYTEA`・`JSON`・`JSONB`・`UUID`・
  `NUMERIC(p,s)`）・`VECTOR ( <N> )`・配列型（`<型>[]`・`<型>[N]`。TABLE-14）・登録済みの
  ENUM 型名。未知の型名は `42601`。`VECTOR` は従来どおり常に非 nullable で `DEFAULT`・
  `UNIQUE` を持てない。DEFAULT の型別変換は `ADD COLUMN` と同じ純関数
  （`sql::ddl::add_column_default`）を共有する。`IF NOT EXISTS`・
  `USING OPERATION_ID` の付与はいずれも許可リスト外（構造的に受理しない・
  `42601`）。列制約 `REFERENCES <table> [(<col>[, ...])]`・表制約
  `FOREIGN KEY (<col>[, ...]) REFERENCES ...`（Issue #907。
  `docs/design/foreign-key.md`）も制約構文として受理する。`CONSTRAINT <name>` は
  `CHECK` の前置にのみ受理する。列制約 `PRIMARY KEY`・表制約 `PRIMARY KEY (<col>[, ...])`（複合キーを
  含む。TABLE-16・TASK-204、Issue #903。`docs/design/sql-primary-key.md`）と、
  列制約 `UNIQUE`・表制約 `UNIQUE (<col>[, ...])`（Issue #905。
  `docs/design/unique-constraint.md`）、列制約・表制約
  `[CONSTRAINT <name>] CHECK (<述語>)`（Issue #906。
  `docs/design/sql-check-constraint.md`）を制約構文として追加受理する。
  列数の上限（256）は列定義 1 個をパースする直前に確定済みの列数だけで判定し、
  表制約の位置（先頭・中間・末尾）に依存しない。
- `TEXT`／`VECTOR` は `lexer::Keyword` へ追加しない（`SET`・`CREATE`・`TRUNCATE`・
  `TABLE` と同方針。statement 中の所定位置でのみ文脈的キーワードとして照合し、
  同名の列名・テーブル名として使う既存 SQL を壊さない）。
- 識別子は大文字小文字をそのまま保持する（SELECT／INSERT と同じ既存の照合方針）。
- 列は 1 件以上必須。空列リスト・末尾カンマは構造的に受理しない。

## 列の既定

- `VECTOR(N)` 列は常に `nullable = false`（TABLE-1。全フィクスチャ・
  `validate_embedding_dim` が vector の存在を前提とする既存設計と整合）。
- `TEXT`・`INTEGER`・`BIGINT` 列は PostgreSQL に倣い `nullable = true`。
- `VECTOR` 列は 0 本または 1 本（catalog の既存 `validate_schema` 規則。2 本以上の
  宣言は `catalog::CatalogError::Invalid` → `42601`）。
- 予約列名 `id`／`tenant_id`／`visibility`（ASCII 大文字小文字を無視して照合）は
  列宣言として拒否する（`42601`）。`sql::parser` がこれら 3 語を疑似列・RLS 内部列
  として扱う契約と整合させ、SQL 表層の DDL でこれらを隠蔽する列を作らせないため
  （fail-closed）。Rust API（`Storage::create_table`）の挙動は変更しない。
- `check`／`constraint` も予約列名として拒否する（`42601`。Issue #906）。列リスト
  要素先頭の `CHECK (`／`CONSTRAINT` を列定義と曖昧さなく制約宣言として解釈する
  ため（`docs/design/sql-check-constraint.md`「曖昧さの排除」参照）。

## 上限検証（確保より前に拒否）

| 検証 | 上限 | 違反時 |
| ---- | ---- | ------ |
| 識別子長（テーブル名・列名） | 63 バイト | `42601` |
| 列数 | 256 | `54000`（`Vec::push` の**前**に判定） |
| `VECTOR` 次元 | `1..=65536` | `42601`（`catalog::validate_schema` へ委譲） |
| 同一文内の列名重複 | — | `42701` |

列数の上限（`sql::allowlist::MAX_CREATE_TABLE_COLUMNS`）は `catalog::
MAX_COLUMN_COUNT` と同値を採用し、`MAX_INSERT_COLUMNS` と同じ「値を複製し
コメントで対応関係を明示する」既存流儀に従う（`catalog` モジュールの可視性を
変えない）。

`VECTOR` の次元は構文解析段階で `Token::Number`（文字列）を `u32` へ checked に
parse し、オーバーフロー・小数点混入は `UnsupportedSyntax`（`42601`）へ落ちる
（`u32::from_str` が自然に拒否するため専用ロジックは持たない）。範囲
（`1..=MAX_VECTOR_DIM`）検証そのものは構文解析段階では重複実装せず、
`sql::ddl::execute_create_table` が `Storage::create_table` を通じて
`catalog::validate_schema` に委譲する（`CatalogError::Invalid` → `42601` への
写像は同一分類のため二重実装しない）。

## 権限ゲート（SQL-23・TASK-202）

DDL は全テナント共有のカタログを変更するため、既定拒否（fail-closed）の実行権限
ゲートを導入した。

- `engine::sql::mode::SessionState::ddl_allowed`（既定 `false`）・`allow_ddl()`
  （wire-server の認証成功後にのみ呼ばれる想定。`DROP TABLE`〔Issue #902〕と
  共有する単一のフィールド・セッタ）。
- 判定点は `engine::sql::ddl::require_ddl_permission` の 1 箇所のみ
  （`DROP TABLE` と共通）。
- **判定順序（決定的。`core.rs::EngineCore::parse_tokens` →
  `execute_parsed_in_session`）**:
  1. 字句解析
  2. 先頭 2 トークンが `CREATE`／`TABLE`（`sql::allowlist::
     is_create_table_statement`）→ 構造・上限の検証
     （`sql::allowlist::validate_create_table_tokens`。カタログ照会なし）を
     経て `ParsedSql::CreateTable` へ束縛（`parse_tokens`）
  3. **権限ゲート**（`ParsedSql::CreateTable` 分岐の先頭。未許可なら即 `42501`）
  4. 書き込みトランザクション内での存在判定（`sql::ddl::execute_create_table`）
- 構造検証（手順 2）はカタログを一切参照しないため、許可形状に一致しない構文は
  権限の有無に関わらず `42601` になる（構文の正誤自体はテナント・カタログの
  存在情報ではないためオラクルにならない）。権限ゲート（手順 3）をカタログ照会
  （手順 4）より**前**に置くことで、許可形状に一致した以降は未許可の主体が
  対象テーブルの有無に関わらず常に同じ `SqlSurfaceError::InsufficientPrivilege`
  （`42501`）のみを受け取り、カタログの状態を一切観測できない
  （`crates/engine/tests/sql_create_table.rs::
  create_table_rejects_unauthorized_session_for_garbage_syntax`・
  `create_table_rejects_unauthorized_session_for_existing_table_name` で固定）。

`42501` は既存の `ErrorClass::ForbiddenTenantMismatch` へ写像する（`23505` が
`IdConflict`／`DuplicateOperationId` の複数原因を束ねているのと同じ運用。新規
`ErrorClass` は追加しない）。

### wire-server 側の付与経路: `--ddl-allowed-users`

`--ddl-allowed-users <user[,user...]>`（`wire_server::ddl_permission_opt`。
`DROP TABLE`〔Issue #902〕と共有する単一の CLI フラグ）で起動時 opt-in する。

- 構文検証（カンマ区切り・空要素禁止・重複禁止）は `ddl_permission_opt::parse`
  が担う。
- ユーザーストアへの実在確認は `auth::UserStore::with_ddl_allowed_users`
  （未知ユーザーを指す場合は起動失敗。fail-closed）。
- `handshake.rs` は認証成功直後・`post_auth_loop` 呼び出しより前に
  `store.is_ddl_allowed(&username)` を判定し、真なら `session.allow_ddl()` を
  呼ぶ。
- **未指定のサーバーは許可主体が存在せず、全ユーザーの全 `CREATE TABLE`／
  `DROP TABLE` が `42501` になる**（既定 fail-closed）。
- NoSQL 表層は本 Issue の対象外のまま（下記「スコープ外」参照）。拡張クエリ
  プロトコルの Parse（`EngineCore::parse_sql`／`ParsedSql`）は Issue #902 の
  `DropTable` variant 追加を機に `CreateTable` variant としても対応済み
  （`ParsedSql` はセッション非依存の構造検証のみを担い、DDL 実行権限ゲートは
  `execute_parsed_in_session` が実行時にセッション状態を見て適用する）。

## 実行本体: 既存 `Storage::create_table` への委譲

`sql::ddl::execute_create_table` は `catalog::TableSchema` を組み立て、既存の
`Storage::create_table`（TASK-85）へそのまま委譲する。第 2 の DDL 実行器は作らない。
`create_table` は単一 write トランザクション内で存在確認・挿入・
`bump_table_generation_in_txn`・commit を行うため、事前の `table_exists` 照会は
行わない（TOCTOU を避けるため。同名での並行 `CREATE` は redb の単一ライタで直列化
され、後着が `42P07` になる）。

`CatalogError` の写像:

- `TableAlreadyExists` → `SqlSurfaceError::DuplicateTable`（`42P07`）
- `Invalid`（`VECTOR` 次元範囲外・複数 `VECTOR` 列宣言等）→ `SqlSurfaceError::
  unsupported`（`42601`）
- その他（`redb` I/O 等）→ `SqlSurfaceError::Internal`（`XX000`。詳細を
  クライアントへ渡さない）

## 応答: 件数を返さない

`CreateTableOutcome`（`sql::ddl`）はフィールドを持たない空構造体
（`TruncateOutcome` と同じ設計）。wire 応答の `CommandComplete` タグは
PostgreSQL 互換の `CREATE TABLE`（件数なし）。

## `SqlOutcome::CreateTable`（BREAKING CHANGE）

`SqlOutcome` へ新 variant `CreateTable(ddl::CreateTableOutcome)` を追加した
（`Insert`／`Truncate`／`Delete`／`Update` と同じ薄いラッパー設計）。`SqlOutcome`
を網羅的にマッチする既存コード（`core.rs`・`wire-server::simple_query`・
`crates/engine/tests/describe_parity.rs`）はすべて更新済み。クレート外で
`SqlOutcome` を網羅的にマッチするコードがあれば追随が必要。

## 既知の制約

- **ファイル形 INSERT（`path`／`body` 列）は SQL で作ったテーブルでは使えない**:
  ファイル形 INSERT は `path`／`body` の non-nullable `TEXT` 列を要求するが、本
  Issue の `TEXT` 列は常に `nullable = true`。`NOT NULL` 構文（別 Issue）の実装
  まで、SQL 表層で作成したテーブルへのファイル形 INSERT は使えない。
- **`VECTOR` 列を持たないテーブルへの INSERT は空 embedding のみ受理**: 行ストア層
  （`tenant::insert_typed_row` 系）は `TableSchema::validate_row_embedding_dim`
  （Issue #995・TABLE-1）を経由し、`VECTOR` 列を持たないテーブルでは
  `dim == 0`（embedding 省略）の行のみ受理し、非空 embedding は
  `table has no VECTOR column` で fail-closed に拒否する。本 Issue はこの
  契約自体を変更しない（テーブル定義は 0 本の `VECTOR` 列を許し、行の書き込み
  可否は `validate_row_embedding_dim` の契約に従う）。
- **テーブル総数の上限は未設定**: `catalog::MAX_LIST_TABLES`（列挙 API 側の上限）
  とは別に、許可された DDL 主体からの `CREATE TABLE` 連打（テーブル数の枯渇）に
  対する専用の上限は本 Issue では導入しない。DDL 主体はオーナーが明示的に許可した
  信頼された運用者を想定するため優先度は低いが、起票候補として記録する
  （out-of-scope-tracking）。

## 列制約（`NOT NULL`／`DEFAULT`。TABLE-16・TASK-204、Issue #904）

`<type>` の直後に、順序自由・各々最大 1 回まで `NOT NULL`／`DEFAULT <literal>` を
受理する。詳細な設計判断は `docs/design/not-null-default.md` 参照。

## スコープ外・後続 Issue

- `ALTER TABLE ADD COLUMN`／DROP／MODIFY COLUMN・各種制約
  （`PRIMARY KEY`・`UNIQUE`・`CHECK`・`REFERENCES` は Issue #903・#905・#906・#907 で
  実装済み）・
  `CREATE INDEX`・`VIEW`・NoSQL 表層の DDL op はいずれも別 Issue の担当（本 Issue の
  権限ゲート（`require_ddl_permission`・`--ddl-allowed-users`）・
  `DuplicateColumn` 分類の再利用を前提とする）。`DROP TABLE` は Issue #902 で
  同じ権限ゲートを共有する形で実装済み（`docs/design/drop-table.md` 参照）。
  `NOT NULL`／`DEFAULT` は Issue #904 で実装済み（上記節参照）。
- `EXPLAIN CREATE TABLE`・`CREATE TABLE` への `USING OPERATION_ID` 付与はいずれも
  許可形状に存在しないため構造的に `42601`。

## 列型の解決順序（Issue #1348）

1. 構文検証（`sql::allowlist`）はカタログを参照しない。ENUM 型名候補は語彙を持たない
   未解決マーカー（`EnumTypeDef::unresolved`）として運ぶ。要素が ENUM 型名候補の配列
   （Issue #1357）も、未解決マーカーを持つ `ArrayType::new_enum` を列型として仮置きする。
2. DDL 権限ゲート（`42501`）を通過した後、`sql::ddl::execute_create_table` が ENUM 型名を
   `Storage::get_enum_type` で解決する。未登録は `42601`（型の存在有無は権限のない
   セッションには見えない）。要素が ENUM の配列も同じ経路で登録済みの語彙へ差し替える。
3. 解決後の列型で `DEFAULT` を束縛検証する（ENUM の語彙外は `22P02`）。
4. `Storage::create_table`（書き込みトランザクション内で型名を再解決する）。解決から
   書き込みまでの間に `DROP TYPE` が割り込んだ場合の `TypeNotFound` も `42601` へ写す。

配列の `[N]` は書き込み時の要素数上限（超過は `54000`）として意味を持ち、範囲は
`1..=1024`（`[]` は 1024）。範囲外・多次元・`VECTOR` 要素は `42601`。`NUMERIC(p,s)`・
`BYTEA`・`JSON`・`JSONB`・ENUM 要素は受理する（Issue #1357。`NUMERIC` の精度・位取りの
範囲外は `42601`）。`CREATE TABLE` が列型キーワードとして予約する語
（制約名の衝突判定）は、受理するスカラー型名すべてに広がった。
