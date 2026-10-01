# `ALTER TABLE ADD COLUMN` の設計判断

Issue #900・対象ビヘイビア: SQL-23（TASK-202）・TABLE-5（O(1) 列追加・既存行のバイト列
不変）。関連ポインタ: ERR-2・ERR-4・ERR-6（エラー分類）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
ALTER TABLE <table> ADD COLUMN <column> <type>
```

- `ALTER`・`TABLE`・`ADD`・`COLUMN` はいずれも `lexer::Keyword` へ含めない（`lexer.rs` の
  モジュールドキュメント「`INSERT`/`INTO`/`VALUES`/`USING`/`OPERATION_ID` は
  `Keyword` へ含めない」設計方針を踏襲）。`Token::Ident` のまま字句解析し、
  `sql::allowlist::Parser::parse_alter_table_add_column` が文脈的に大文字小文字を
  無視して照合する。
- 列制約は `NOT NULL`・`DEFAULT <literal>` のみを順不同・各 1 回まで受理する（Issue #1169。
  `CREATE TABLE` と同じ `parse_column_constraints` を共有）。`UNIQUE`・`PRIMARY KEY`・
  `CHECK`・`REFERENCES`・`DEFAULT NULL` は構文として受理しない（`42601`）。制約を持たない
  追加列は従来どおり nullable（TABLE-5）。
- 既存行は書き換えない（TABLE-5。O(1)・既存行のバイト列不変）。`DEFAULT` 付きで追加した列は、
  行バッファ末尾で欠落している既存行を読み出し時に既定値で補う
  （`row_codec::scan_scalar_columns_validated`。PostgreSQL の fast default 相当）。補完値は
  `INSERT` の列省略補完と同じ `row_codec::column_default_scalar` を通す。列の `DEFAULT` は
  作成後に変わらないことが前提で、`ALTER COLUMN SET DEFAULT` を導入する場合は補完専用の値の
  分離が必要。
- `DEFAULT` なしの `NOT NULL` は、行の有無（自テナント・他テナント・空）にかかわらず構造検証段階で
  一律 `42601` 拒否する（TABLE-16。カタログ・行ストアを参照しない）。行の有無で成否を変えると、
  DDL 主体が他テナントの行の存在を判別できてしまう（テナント境界 P0）ため、判定を行データに
  依存させない。Rust API（`Storage::alter_table_add_column`）も同じ条件を `CatalogError::Invalid` で
  拒否する。
- `DEFAULT` の判定順序（決定的）: テーブル存在（`42P01`／`42809`）→ 型名解決（VECTOR `0A000`）→
  DEFAULT 非対応型（`0A000`）→ リテラル種別不一致（`42601`）・長さ超過（`54000`）→ 値の束縛
  （数値の範囲外 `22003`・不正 `22000`・`DATE`／`TIMESTAMP` の書式違反 `22007`・範囲外／暦上不正 `22008`・
  `UUID` の文法不正 `22P02`）→
  カタログ更新。
- `DATE` 列の `DEFAULT`（Issue #1279）: 文字列リテラルのみ受理し（`DATE DEFAULT 1` は `42601`）、
  `ColumnDefault::Text` が文字列リテラルの原文として列型に従い解釈される
  （`row_codec::default_scalar` が `datetime::parse_date` へ委譲。INSERT のリテラル束縛と同じ
  文法・SQLSTATE）。新 variant は足さず、カタログ符号化（v5 の `s` タグ）も変えない。後続の
  `ENUM`（#1282）も同じ対応表へ腕を足して拡張する。DDL の成否は
  カタログと入力のみで決まり、行ストアは参照しない（テナント境界）。HTTP の
  `alter_table.add_column` は引き続き DEFAULT を受け取らない。
- `UUID` 列の `DEFAULT`（Issue #1281）: 文字列リテラルのみ受理し（`UUID DEFAULT 1` は `42601`）、
  `ColumnDefault::Text` の原文を `row_codec::default_scalar` が `uuid::parse_uuid_text` で解釈する
  （INSERT の UUID リテラル束縛と同じ厳密文法・SQLSTATE `22P02`。UUID は全 128bit 値が有効で
  範囲外の概念は無い）。新 variant・カタログ符号化の変更なし。読み出しは正規小文字表記。
  DDL の成否は行ストアを参照しない（テナント境界）。HTTP は DEFAULT 非対応のまま。
- `TIMESTAMP` 列の `DEFAULT`（Issue #1280）: `DATE` と同じ方式で、`row_codec::default_scalar` が
  `datetime::parse_timestamp`（空白または `T` 区切り・小数秒 1〜6 桁。INSERT と同じ文法・SQLSTATE）へ
  委譲する。受理するのは文字列リテラルのみで、揮発性の既定値（`CURRENT_TIMESTAMP`・`now()`）は
  読み出し時補完が定数でしか正しくないため `42601` で拒否する。
- 予約列名（`id`・`tenant_id`・`visibility`。ASCII の大文字小文字を無視）は
  `CREATE TABLE`（Issue #899）の列定義と同じく構造検証段階で `42601` 拒否する
  （`sql::parser` が疑似列・RLS 内部列として扱う名前を DDL で隠蔽させない）。
  カタログを参照しない判定のため、権限ゲートより前に置いても存在オラクルにならない。
  `check`／`constraint` も `CREATE TABLE`（Issue #906。`CHECK` 制約構文との曖昧さ
  排除）と揃えて予約列名として拒否する。
- `IF NOT EXISTS`・複数 `ADD`・`DROP COLUMN`・`ALTER COLUMN TYPE`・`RETURNING`・
  `USING OPERATION_ID` の併用はいずれも許可リスト外（`42601`）。DDL（テーブル定義の
  変更）であり `operation_id` 台帳（TASK-93）の対象外のため、`TRUNCATE`・`INSERT` と
  異なり文末専用句を持たない。
- `$n` パラメータは許可位置の一覧（`sql::params` モジュールドキュメント）に
  `ALTER TABLE` 由来の位置が含まれないため、拡張クエリプロトコルの Parse
  （`validate_param_positions`）で構造的に `42601` になる。個別の対応は不要。

## 型名パーサ（`sql::ddl_column_type`）

SQL テキストの型名を構文木（`SqlColumnTypeName`）へ変換する専用モジュールを新設した。
`sql::allowlist::Parser`（内部状態 `tokens`／`pos` は private）には触れず、トークン
スライスと読み取り位置（`&mut usize`）だけを引数に取る独立実装とする——
`Parser::parse_alter_table_add_column` が自身の `tokens`／`pos` をそのまま共有して呼ぶ
（`split_parenthesized` と同じ「呼び出し元パーサー種別に依存しない自己完結の走査」
方針）。

受理する型: `TEXT`・`INTEGER`・`BIGINT`・`REAL`・`DOUBLE PRECISION`（2 語）・
`BOOLEAN`・`DATE`・`TIMESTAMP`・`BYTEA`・`JSON`・`JSONB`・`UUID`・
`NUMERIC(p,s)`／`DECIMAL(p,s)`・未知の識別子（ENUM 型名候補）。

`NUMERIC`/`DECIMAL` の精度・位取りの範囲検証（`1 <= precision <= 38`・
`scale <= precision`）は本パーサでは行わない。二重実装を避けるため、
`catalog::alter_table_add_column` が内部で呼ぶ `validate_column`（`catalog.rs` の
既存の非公開検証関数）に一本化し、構文段では「`u8` として妥当な非負整数（先頭ゼロ・
符号・小数を除く）か」だけを検証する。範囲外の値は実行段で `CatalogError::Invalid`
経由の `42601` として拒否される。

ENUM 型名候補の存在確認（`Storage::get_enum_type`）は実行段（`sql::ddl::
execute_alter_table_add_column`）が権限ゲート通過後に行う。

### `VECTOR` 列を保留した理由

`VECTOR(N)` は構文としては受理するが、実行段で常に `0A000`
（`SqlSurfaceError::FeatureNotSupported`）として拒否する。既存行が埋め込みバイトを
持たないテーブルへ `VECTOR` 列を追加した場合の挙動——`VectorArena` の構築・KNN・
HNSW 各経路が「埋め込み列を持つがバイトを一切持たない既存行」を安全に扱えるか——を
本 Issue の実装時点では検証していない。受け入れ基準の「既存行が NULL として読める」
契約を検索経路について保証できない状態で解禁すると fail-open になりうるため、
安全側に倒して保留した。実装者が安全性を実測で確認できた場合に限り、根拠を本
ドキュメントに追記のうえ緩めてよい。

### 配列型を扱わない理由

配列型（`<型>[]`）は対象外とした。`lexer` が `[`／`]` を字句化できないため、字句解析
段階で `42601` になる。`lexer.rs` を広げる変更は本 Issue のスコープ外（`CREATE TABLE`
等、型名構文を共有しうる後続実装が配列型の表記を導入する場合はそちらで判断する）。

## DDL 権限ゲート（SQL-23 の単一判定点）

DDL の実行は、RLS 相当のテナント境界とは別軸の権限として扱い、既定を拒否
（fail-closed）にする。`ALTER TABLE ADD COLUMN` は独自のゲートを持たず、
`CREATE TABLE`（Issue #899）・`DROP TABLE`（Issue #902）が先行して導入した
DDL 実行権限ゲートをそのまま共有する（判定点・拒否コード・CLI 設定をすべて共通化し、
重複する仕組みを作らない）。

- **保持する場所**: `SessionState::ddl_allowed`（`sql/mode.rs`）。`Default` は
  `false`。変更手段は `SessionState::allow_ddl` のみ。
- **判定する場所**: `sql::ddl::require_ddl_permission`（全 DDL 共通の 1 箇所）。
  `core.rs::execute_parsed_in_session` の `AlterTable` 分岐の先頭で、カタログ照会
  （ENUM 型名・テーブル・列の存在確認）より必ず先に呼ぶ。
- **判定順序**: (1) 字句解析・構造検証（`42601`）→ (2) 権限ゲート（`42501`）→
  (3) 対象テーブルの存在確認（不在なら `42P01`、同名のビューなら `42809`）→
  (4) 型名解決（`VECTOR` は `0A000`、未登録 ENUM 型名は `42601`）→ (5) write
  トランザクション内のカタログ操作（テーブル存在の再確認〔競合で消えた場合も
  (3) と同じ写像〕・`42701`／`54000`／`42601`／`55P03`／`XX000`）。権限の無い主体
  には、テーブル・列・ビューが存在するかどうかを一切返さない（存在オラクル化の防止）。
  (3) を (4) より先に置くのは、存在しないテーブルへの要求が型名の不正で答えられ
  ないようにするため（PR #1052 レビュー指摘）。
- **ビューが対象の場合**: `CREATE VIEW`（Issue #909）の書き込み系方針と揃え、
  ビューへの `ALTER TABLE ADD COLUMN` は `42809`（`WrongObjectType`）で拒否する。
  判定は `catalog::Storage::view_definition` を単一の情報源とし、読み直し自体の
  失敗は「ビューではない」と同一視せず `catalog::table_lookup_error` で写像する。
- **wire-server 側**: `--ddl-allowed-users <user[,user...]>`（`CREATE TABLE`／
  `DROP TABLE` と共通。`wire_server::ddl_permission_opt`・
  `UserStore::with_ddl_allowed_users`）。認証成功直後に認証済み username が許可
  リストに含まれる場合のみ `SessionState::allow_ddl` を呼ぶ。未指定なら全 DDL が
  `42501` になる。起動時検証の詳細は同オプションの実装・テスト
  （`crates/wire-server/tests/wire_ddl_permission_cli.rs`）を参照。
- **明示トランザクション内**: `BEGIN` 〜 `COMMIT` 内の DDL は `CREATE TABLE`／
  `DROP TABLE` と同じく、権限の有無に関わらず `0A000` で拒否しトランザクションを
  `Failed` へ遷移させる（SQL-31・TASK-221 の既存方針を継承。`core.rs::
  execute_in_active_txn` の既存分岐に委ね、`ALTER TABLE` 専用の扱いは追加しない）。
- **NoSQL 表層は対象外**。`SessionState::default()`（`ddl_allowed() == false`）の
  ままとし、DDL の op（`ALTER TABLE` 相当）は許可リストに追加しない（別 Issue の
  担当）。

## 列数上限を `54000` にした選択

`CatalogError::TooManyColumns { count }`（新設）を追加し、`alter_table_add_column` の
既存の列数上限判定（`MAX_COLUMN_COUNT = 256`。Issue #901 以降は生存列＋削除済み列の
墓標を合わせた物理スロット総数に適用）をこの variant へ置き換えた。
`SqlSurfaceError::PayloadTooLarge`（`54000`）へ写像する——untrusted 入力（列追加の
繰り返し）が構造的な上限を超えたことを表す既存の `54000` の意味論（ベクトルリテラル
64 KiB 超過等）と一致させ、識別子・型不正を表す `42601`（`UnsupportedSyntax`）とは
区別した（`CREATE TABLE` の列数上限超過も `54000` であり、DDL 間で一致する）。

## エラー写像表（`sql::ddl::map_add_column_error`）

`catalog::table_lookup_error`（読み取り専用経路向け）とは意図的に共有しない
——`ColumnAlreadyExists`・`TooManyColumns` は読み取り経路には現れない DDL 固有の
意味を持つため、独立した写像を持つ。

| `CatalogError` | 写像先 | `wire_code` |
| --- | --- | --- |
| `TableNotFound`（同名のビューが無い） | `UndefinedTable` | `42P01` |
| `TableNotFound`（同名のビューが有る）・`WrongObjectKind` | `WrongObjectType` | `42809` |
| `ColumnAlreadyExists` | `DuplicateColumn`（`CREATE TABLE` と共有） | `42701` |
| `TooManyColumns` | `PayloadTooLarge` | `54000` |
| `Invalid` | `UnsupportedSyntax` | `42601` |
| `TypeNotFound` | `UnsupportedSyntax` | `42601` |
| `WriteLockTimeout` | `LockNotAvailable` | `55P03` |
| その他（`Backend`・`CorruptSchema`・`TableAlreadyExists`・`RowNotFound`・`IncompatibleRowKeyFormat`・`TableGenerationCounterOverflow`・`TypeAlreadyExists`・`DependentObjectsStillExist`・`ColumnNotFound`・`ProtectedColumn`・`IncompatibleTypeChange`・`ViewNotFound`・`DependentViewsExist`・`ViewLimitExceeded`） | `Internal`（固定文言） | `XX000` |

ワイルドカード腕は置かず、`CatalogError` の全 variant を明示列挙する（将来の variant
追加をコンパイラが検出できるようにするため）。

## 実行結線

`sql::allowlist::validate_alter_table`／`validate_alter_table_tokens`
（`ValidatedAlterTableAddColumn` を返す。カタログ照会を一切行わない——3.1 の判定順序
参照）→ `core.rs::EngineCore::execute_parsed_in_session` の `AlterTable` 分岐
（`require_ddl_permission` → `sql::ddl::execute_alter_table_add_column`）→
`catalog::Storage::alter_table_add_column`（TABLE-5。既存実装。単一 write
トランザクション・ENUM 定義の再解決・`bump_table_generation_in_txn`・
`commit_boundary::commit` を内包）。

世代（`bump_table_generation_in_txn`）は `alter_table_add_column` 内で既に進行するため、
`SqlArenaCache`（Issue #363）・`SparseIndexCache`（Issue #357）・
`VisibleBitmapCache`（Issue #478）・`ScalarIndexCache`（Issue #473）等のテーブル単位
世代整合キャッシュは追加実装なしに自動的に失効する。

`SqlOutcome::AlterTable(ddl::AlterTableOutcome)`（`table_name`・`column_name` のみを
保持。`TruncateOutcome` と同じく最小限の応答）・`ParsedSql::AlterTable` を新設した——
**BREAKING CHANGE**: `SqlOutcome`・`ParsedSql`・`CatalogError` への網羅的 `match` は
追随が必要。
wire 応答は pg 互換の `CommandComplete` タグ `ALTER TABLE`（件数を持たない固定タグ。
`TRUNCATE TABLE` と同じ設計）。

`describe_parsed_in_session_impl` は `ParsedSql::AlterTable(_) => Ok(None)`
（結果列を持たない）。

## セキュリティ考慮

- **A01 アクセス制御の不備**: 判定点を全 DDL 共通の 1 箇所（`require_ddl_permission`）に集約し、
  カタログ照会・write トランザクションの前に必ず判定する。RLS の行可視性・暗黙
  適用・行のバイトには一切触れない（TABLE-5 の O(1) 契約）。
- **A03 インジェクション**: SQL の組み立ては行わない。トークン列から AST
  （`SqlColumnTypeName`）を作り、`ColumnDef` へ変換して既存の `catalog::Storage`
  API を呼ぶだけ。識別子は `catalog::validate_identifier`
  （`alter_table_add_column` 内部で必ず経由）を通す。
- **A04 不安全な設計**: 列数上限の判定は write トランザクション内で行われる
  （`alter_table_add_column` 既存実装。TOCTOU 回避）。安全性を検証していない
  `VECTOR` の追加は `0A000` で保留する。`Internal`（`XX000`）は格納データの断片を
  返さない固定文言。
- **A05 セキュリティ設定ミス**: `--ddl-allowed-users` が未指定なら全 DDL を拒否する
  （`CREATE TABLE`／`DROP TABLE` と共通の設定。ADD COLUMN 専用の緩い設定経路は持たない）。

## 対象外（out-of-scope）

- `CREATE TABLE`: #899（実装済み。DDL 権限ゲートを共有）
- `DROP COLUMN`・`ALTER COLUMN TYPE` の SQL 表層構文: #901（Rust API のみ実装済み）
- `NOT NULL`・`DEFAULT` を伴う `ADD COLUMN`: #1169 で実装済み（`DATE`・`TIMESTAMP`・`UUID` の DEFAULT は #1279・#1280・#1281 で
  実装済み。`ENUM` は #1282）
- 制約（`PRIMARY KEY`・`UNIQUE`・`CHECK`・`REFERENCES` 等）: #903・#905〜#907
- NoSQL 表層の DDL op: #910
- 配列型の DDL 表記: #899 へ申し送り
- `VECTOR` 列の `ADD COLUMN` 解禁: 検証を伴う後続作業（本ドキュメント「`VECTOR` 列を
  保留した理由」参照）
