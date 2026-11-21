# ADR: システムカタログのテナント境界での見え方と提供範囲（Issue #3342）

- ステータス: 承認待ち（判断事項 O1〜O10 はオーナーが 2026-11-21 に決定済み。§7）。security-auditor の監査指摘は反映済み。
  本書を追加する PR のマージをもって承認（Accepted）とする。実装 Issue（§5）の起票はオーナーの判断を待つ
- 対応 Issue: #3342（親 #3306）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-51・TABLE-27・TABLE-44）・
  `docs/spec/04-behavior/wire-protocol.md`（WIRE-13・WIRE-14）・`docs/spec/04-behavior/sql-surface.md`
  （SQL-34・SQL-40・SQL-80）・`docs/spec/04-behavior/rls.md`（RLS-12）・`docs/spec/05-tasks.md`（TASK-350・TASK-351）。
  spec 本文は転記しない。契約は spec を正とし、本書では再掲しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連 doc: [schema-tenant-namespace.md](schema-tenant-namespace.md)（D2・D4・D5・D9）・
  [wire-type-oid-mapping.md](wire-type-oid-mapping.md)・[role-privilege-model.md](role-privilege-model.md)（D1・D5・D6・D10）・
  [fixed-tenant-boundary.md](fixed-tenant-boundary.md)・[temp-table.md](temp-table.md)・
  [limit-omitted-result-cap.md](limit-omitted-result-cap.md)・[explicit-transaction.md](explicit-transaction.md)
- 検証コード: なし（本 Issue は設計記録のみ。コードは変更しない）

## 1. 背景・目的

`information_schema.*`・`pg_catalog.*` の表への参照は、現状すべて固定文言の `0A000` で拒否される。
ドライバ・ORM・マイグレーションの道具・psql のメタコマンドはこれらを引くため、そのままでは接続直後や
最初の reflection で失敗する。一方でカタログは全テナント共有の平らなキーで保持しており、PostgreSQL の
カタログをそのまま写すと、テナント間で表の存在・OID・件数が漏れる。

本書は次の 4 点を決め、実装 Issue の範囲を確定する入力にする。

1. 提供する `information_schema`・`pg_catalog` の表・ビューと列の部分集合（道具の問い合わせの棚卸しから、必須度順に段階を分ける）
2. OID の割り当て方式（他テナントの存在・件数を漏らさず、テナント間で値が重なっても観測できない方式）
3. `regclass` などの OID の別名の型の扱い
4. 読み取り専用の強制、テナントの文脈に限った行の出し方（fail-closed）、ロール権限による絞り込み、件数・サイズ系の扱い

本書の決定は、公開 Issue の記述・PostgreSQL 17 の公開挙動（出典を付ける）・公開コードの現状だけで構成する。
spec との対応は ID のポインタに留める。

## 2. 公開コードの現状（main 7617fafaa 時点）

Issue #3342 本文の file:line は main 3ef68d68 時点のため、ここで再特定した。

| 事実 | 所在 |
| ---- | ---- |
| 修飾名の修飾子が組み込み SCHEMA（`pg_catalog`・`information_schema`）か `pg_temp` なら `0A000`（`relations in this schema are not supported`） | `crates/engine/src/sql/relation_name.rs:1114`〜`:1118`（`resolve`） |
| 組み込み SCHEMA 名の定数（`public`・`pg_catalog`・`information_schema`） | `crates/engine/src/catalog/namespace.rs:61`（`BUILTIN_SCHEMA_NAMES`）・`:77`（`is_builtin_schema_name`） |
| `search_path` 要素の正規化で組み込み名を小文字へ畳む | `crates/engine/src/sql/search_path.rs:117`〜`:118`（`normalize_name`） |
| `current_schemas(true)` が `pg_catalog` を暗黙に前置する（明示リストに無いときだけ） | `crates/engine/src/sql/search_path.rs:1105`〜`:1120`（`SchemaFnValues::schemas`） |
| `pg_catalog`・`information_schema` は `current_schemas` 上は常に実在扱い | `crates/engine/src/sql/search_path.rs:1140` |
| 未修飾の relation 名の解決は組み込み SCHEMA を飛ばす（暗黙の `pg_catalog` はまだ relation を持たない） | `crates/engine/src/sql/search_path.rs:727`〜`:749`（`resolve_path_refs` とその doc） |
| 作成系 DDL の作成先が組み込み SCHEMA なら `0A000` | `crates/engine/src/sql/search_path.rs:921`〜`:942`（`creation_target`。拒否は `:932`〜`:936`） |
| `pg_catalog` 修飾の型名（`'a'::pg_catalog.int4`）は `0A000`。`pg_catalog.f(x)` は書き換えずに後段へ流れる | `crates/engine/src/sql/relation_name.rs` の単体テスト（`:1589`・`:1919`） |
| システムカタログは対象外として `0A000` を維持する、という過去の決定 | [schema-tenant-namespace.md](schema-tenant-namespace.md) D9（`:186`）・D4 の手順 3（`:115`） |
| 型 OID の写像は wire-server の `WireType` が単一情報源。ENUM・VECTOR 系は `text`（25）、`id` は `numeric`（1700） | `crates/wire-server/src/result_encoder.rs`・[wire-type-oid-mapping.md](wire-type-oid-mapping.md) |
| RowDescription の table OID と attnum は常に 0 | `crates/wire-server/src/result_encoder.rs:1232`〜`:1233` |
| 起動時の ParameterStatus は `server_version=14.0` | `crates/wire-server/src/handshake.rs:2049` |

### 2.1 カタログのキー構造

| カタログ | キー | テナントとの関係 |
| -------- | ---- | ---------------- |
| 表（`catalog`）・ビュー（`views`）・索引（`index_catalog`）・matview・ENUM 型（`enum_types`） | 名前（`&str`）。`public` は裸の名前、テナントの SCHEMA 配下は内部キー `#<schema_id>.<name>` | `public` の定義は全テナントで共有。SCHEMA 配下はテナントに属する（ADR D2） |
| SEQUENCE 定義（`sequences`）／状態（`sequence_state`） | `(schema, name)`／`(tenant_id, sequence_id)` | 定義は `public` なら共有、状態はテナント別 |
| SCHEMA（`schemas`） | `(tenant_id, schema_name)` | テナント別 |
| ロール（`roles`・`role_memberships`） | `(tenant_id, ...)`。ログイン名対応表だけサーバー全体 | テナント別（role ADR D1） |
| UDF（`functions`） | `(scope=tenant_id, function_key, arg_signature)` | テナント別 |
| トリガ（`triggers`）・利用者ポリシー（`policies`） | `(table_key, scope=tenant_id, name)` | 共有の表にもテナント別に付く |
| ACL（`acl`） | `(tenant_id, object_kind, object_key, column, grantee_id, grantor_id)`（テナントが先頭） | 共有の表にもテナント別に持つ（role ADR D5） |
| 所有者（`object_owners`） | `(object_kind, object_key, tenant_id)`（テナントが**末尾**。`crates/engine/src/catalog/privilege/owner.rs:34`） | 共有の表にもテナント別に持つ（role ADR D6）。テナント単位の列挙 `list_owners_for_tenant_in_txn`（同 `:133`〜`:150`）は全行走査 |
| 行ストア（`user_rows/{内部キー}`） | `(tenant_id, rowid)` | `public` の表は全テナントの行が同じ物理表に混在する |

- SCHEMA・ロール・UDF・トリガ・ポリシー・SEQUENCE の内部 ID は、いずれも**全テナント共有の単調カウンタ**から払い出す。
- 表・ビュー・索引は名前がキーで、改名しても変わらない内部 ID を持たない（改名は redb の `rename_table` で付け替える。
  [alter-table-rename-table.md](alter-table-rename-table.md)）。
- 列挙のコスト（D5 の前提）。
  - `public` の relation は裸の名前の範囲、SCHEMA 配下は自テナントの各 `#<id>.` 接頭辞の範囲、UDF・ロール・ACL は `tenant_id`
    先頭の範囲で引ける。走査量は自テナントから見える件数で決まる
  - トリガ・ポリシーはキーの先頭が表なので、可視な表ごとに `(table_key, tenant_id)` の接頭辞で走査する。列挙のコストは
    可視な表の数（と自テナントのトリガ・ポリシーの数）に比例し、他テナントの件数には依らない
  - 所有者はテナントがキーの末尾なので、テナント単位の範囲走査では引けない。カタログの経路では、可視な relation・SCHEMA・
    関数ごとに `(種別, 対象, 自テナント)` を点引きする。テナント単位の全行走査（`list_owners_for_tenant_in_txn`。論理ダンプ専用）は使わない

### 2.2 テナント・ロール・RLS の境界（要約）

- テナントはサーバーが認証時に導出し、SQL から変えられない。管理者バイパスは無い（[fixed-tenant-boundary.md](fixed-tenant-boundary.md)）。
- SCHEMA はテナントの内側の名前空間で、他テナントの SCHEMA は不在と同じ応答にする（schema ADR D4）。
- 名前解決は「テナント内の解決 → 権限検査」の順で、他テナントのオブジェクトに権限検査の分岐が走らない（role ADR D10）。
- 一時表はセッションの `SessionState` が持ち、他セッションから構造上見えない（[temp-table.md](temp-table.md)）。

## 3. 問い合わせの棚卸し

### 3.1 調査の方法と信頼性

- 2026-11-21 に各プロジェクトのソースを WebFetch で読んだ。読んだのはブランチの先端（`REL_17_STABLE`・`main`・`master`）か
  タグ（pgjdbc `REL42.7.8`）で、URL は下の各表に記す。ブランチの URL は今後内容が変わりうる。
- WebFetch は取得したページを小型モデルで要約して返すため、表・列・関数の名前は要約を経ている。WHERE の細部と列の並びは
  実装 Issue で原文と突き合わせる（各段の Issue の受け入れ条件に含める）。読めなかった箇所は「未確認」と書く。
- 棚卸し表の出典の列の末尾に、行ごとの確認の度合いを付けた（〔要約経由〕は WebFetch の要約だけに基づく行、〔原文確認〕は
  該当行を直接確かめた部分）。
- 次の 3 点は原文の該当行を直接確認した（段の境界を決めるため）。
  - psql の `\d` は、引数が無いとき `listTables("tvmsE", ...)`、あるときに `describeTableDetails` を呼ぶ（`command.c` の `exec_command_d`）
  - psql のパターン句は `<列> OPERATOR(pg_catalog.~) '^(...)$' COLLATE pg_catalog.default` で、SCHEMA を指定しないと
    `pg_catalog.pg_table_is_visible(<oid>)` を足す（`fe_utils/string_utils.c` の `processSQLNamePattern`。`COLLATE` はサーバー版 12 以上のとき）
  - Rails の `load_additional_types` の呼び出し元は `get_oid_type` だけで、型マップに無い OID を初めて見たときに遅延で `pg_type` を引く

### 3.2 棚卸し表

必須度の区分:

- **接続**: 接続確立（またはドライバの初期化）に必須。応答しないと接続できない
- **通常**: アプリの通常動作（モデルのスキーマ読み込み・未知 OID の解決）またはマイグレーションの実行に必須
- **反映**: reflection・introspection・メタデータ API の呼び出し時だけ
- **psql**: psql のメタコマンドの実行時だけ

| 道具 | 発行タイミング | 引く表・ビュー／列／関数（主なもの） | OID 別名の型 | 必須度 | 出典 |
| ---- | -------------- | ------------------------------------ | ------------ | ------ | ---- |
| psql 17 | 接続直後 | 発行しない。版数は ParameterStatus の `server_version` から得る | — | — | [startup.c](https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/bin/psql/startup.c)・[command.c](https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/bin/psql/command.c) 〔要約経由〕 |
| psql 17 `\d`（引数なし）・`\dt` | メタコマンド時 | `pg_class`・`pg_namespace`・`pg_am`（12 以上）。`pg_get_userbyid(relowner)`。`+` で `pg_table_size`・`pg_size_pretty`・`obj_description`。`relkind IN (...)`、既定で `pg_catalog`・`information_schema`・`pg_toast` を除外、パターン時は `pg_table_is_visible` | — | psql | [describe.c](https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/bin/psql/describe.c)（`listTables`） 〔`\d` の振り分けは原文確認。表・列・条件は要約経由〕 |
| psql 17 `\d <名前>` | メタコマンド時 | 1 回目: `pg_class`・`pg_namespace`（`c.oid`・`nspname`・`relname`）。2 回目以降（OID を文字列で埋め込む）: `pg_attribute`・`pg_attrdef`・`pg_collation`・`pg_type`・`pg_index`・`pg_constraint`・`pg_inherits`・`pg_policy`・`pg_roles`・`pg_statistic_ext`・`pg_rewrite`・`pg_trigger`・`pg_publication` 系・`pg_depend`・`pg_sequence`・`pg_am`。関数 `format_type`・`pg_get_expr`・`pg_get_indexdef`・`pg_get_constraintdef`・`pg_get_triggerdef`・`pg_get_viewdef`・`pg_get_ruledef`・`pg_get_partkeydef`・`pg_get_partition_constraintdef`・`pg_partition_ancestors`・`col_description`・`pg_relation_is_publishable`・`array_to_string`・`quote_ident` | `'<oid>'::pg_catalog.oid`・`::regclass`（`conrelid`・`inhparent` ほか）・`::regnamespace`・`reloftype::regtype`・`'pg_catalog.pg_class'::regclass` | psql | 同上（`describeTableDetails`・`describeOneTableDetails`）。行番号は未確認 〔要約経由。パターン句の形は原文確認〕 |
| psql 17 `\dn` | メタコマンド時 | `pg_namespace`・`pg_get_userbyid(nspowner)`。`+` で `nspacl`・`obj_description`。既定で `^pg_` と `information_schema` を除外 | — | psql | 同上（`listSchemas`） 〔要約経由〕 |
| psql 17 `\df` | メタコマンド時 | `pg_proc`・`pg_namespace`・`pg_type`・`pg_language`（`+`）。`pg_get_function_result`・`pg_get_function_arguments`・`pg_function_is_visible`・`pg_type_is_visible`。`prokind` で絞る | `'pg_catalog.trigger'::regtype` | psql | 同上（`describeFunctions`） 〔要約経由〕 |
| psql 17 `\l` | メタコマンド時（`psql -l` は起動直後） | `pg_database`（`datname`・`datdba`・`encoding`・`datcollate`・`datctype`・版数で `datlocprovider` など）・`pg_encoding_to_char`・`pg_get_userbyid`。`+` で `pg_database_size`・`has_database_privilege`・`pg_tablespace` | — | psql | 同上（`listAllDbs`） 〔要約経由〕 |
| psql 17 `\du` | メタコマンド時 | `pg_roles`（`rolname`・`rolsuper`・`rolinherit`・`rolcreaterole`・`rolcreatedb`・`rolcanlogin`・`rolconnlimit`・`rolvaliduntil`・`rolreplication`・`rolbypassrls`）。既定で `^pg_` を除外 | — | psql | 同上（`describeRoles`） 〔要約経由〕 |
| libpq 17 | 接続時 | 発行しない。`target_session_attrs` 指定時だけ `SHOW transaction_read_only` か `pg_is_in_recovery()` | — | 接続（指定時のみ） | [fe-connect.c](https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/interfaces/libpq/fe-connect.c)・[fe-exec.c](https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/interfaces/libpq/fe-exec.c) 〔要約経由〕 |
| pgjdbc 42.7.8 | 接続時 | カタログは引かない。サーバー版 12 未満で `SET extra_float_digits`、条件付きで `SET application_name`、`targetServerType` 指定時に `show transaction_read_only` | — | 接続（条件付き） | [ConnectionFactoryImpl.java](https://raw.githubusercontent.com/pgjdbc/pgjdbc/REL42.7.8/pgjdbc/src/main/java/org/postgresql/core/v3/ConnectionFactoryImpl.java) 〔要約経由〕 |
| pgjdbc `TypeInfoCache` | 未知 OID・型名の初回解決時（遅延） | `pg_type`（`oid`・`typname`・`typnamespace`・`typtype`・`typinput`・`typelem`・`typarray`・`typdelim`）・`pg_namespace`・`current_schemas(true/false)`・`generate_series`・`array_upper` | `'pg_catalog.array_in'::regproc` | 通常 | [TypeInfoCache.java](https://raw.githubusercontent.com/pgjdbc/pgjdbc/REL42.7.8/pgjdbc/src/main/java/org/postgresql/jdbc/TypeInfoCache.java) 〔要約経由〕 |
| pgjdbc `ResultSetMetaData` | `isNullable` 等の初回呼び出し時 | `pg_class`・`pg_namespace`・`pg_attribute`・`pg_type`・`pg_attrdef`・`pg_get_expr`。RowDescription の table OID と attnum で引く | — | 反映 | [PgResultSetMetaData.java](https://raw.githubusercontent.com/pgjdbc/pgjdbc/REL42.7.8/pgjdbc/src/main/java/org/postgresql/jdbc/PgResultSetMetaData.java) 〔要約経由〕 |
| pgjdbc `getTables`・`getColumns`・`getPrimaryKeys`・`getSchemas` | メタデータ API 呼び出し時 | `pg_namespace`・`pg_class`・`pg_description`・`pg_attribute`・`pg_type`・`pg_attrdef`・`pg_index`・`current_database()`・`pg_get_expr`・`row_number() OVER`・`information_schema._pg_expandarray(indkey)`・設定次第で `has_table_privilege`・`has_schema_privilege`。`pg_database`（`getCatalogs`） | `'pg_class'::regclass` | 反映 | [PgDatabaseMetaData.java](https://raw.githubusercontent.com/pgjdbc/pgjdbc/REL42.7.8/pgjdbc/src/main/java/org/postgresql/jdbc/PgDatabaseMetaData.java)（`getIndexInfo` 以降は未確認） 〔要約経由〕 |
| Npgsql（main） | データソースごとに最初の物理接続の Open 時に 1 回（結果をキャッシュ） | `version()`。型ロード: `pg_type`（`oid`・`typname`・`typtype`・`typnotnull`・`typelem`・`typbasetype`・`typcategory`・`typrelid`・`typreceive`）・`pg_namespace`・`pg_class`（`relkind`）・`pg_proc`（`proname='array_recv'` で配列を判定）・`pg_range`（`rngtypid`・`rngsubtype`・`rngmultitypid`）・`pg_attribute`（複合型の列）・`pg_enum`（`enumlabel`・`enumsortorder`） | — | 接続 | [PostgresDatabaseInfo.cs](https://raw.githubusercontent.com/npgsql/npgsql/main/src/Npgsql/PostgresDatabaseInfo.cs)・[NpgsqlDataSource.cs](https://raw.githubusercontent.com/npgsql/npgsql/main/src/Npgsql/NpgsqlDataSource.cs) 〔要約経由〕 |
| Npgsql | Open 時（条件は未確認） | `pg_is_in_recovery()`・`SHOW default_transaction_read_only` | — | 未確認 | [NpgsqlConnector.cs](https://raw.githubusercontent.com/npgsql/npgsql/main/src/Npgsql/Internal/NpgsqlConnector.cs) 〔要約経由〕 |
| psycopg 3 | 接続時 | 発行しない（`connection.py`。`_connect_gen` の確認は未了） | — | — | [connection.py](https://raw.githubusercontent.com/psycopg/psycopg/master/psycopg/psycopg/connection.py) 〔要約経由〕 |
| psycopg 3 `TypeInfo.fetch`・`register_composite`・`register_enum` | 利用者が明示的に呼んだとき | `pg_type`（`typname`・`oid`・`typarray`・`typdelim`）・`pg_attribute`・`pg_enum`・`pg_range` | `to_regtype(%s)` または `%s::regtype`、`oid::regtype::text` | 反映 | [_typeinfo.py](https://raw.githubusercontent.com/psycopg/psycopg/master/psycopg/psycopg/_typeinfo.py)・[composite.py](https://raw.githubusercontent.com/psycopg/psycopg/master/psycopg/psycopg/types/composite.py)・[enum.py](https://raw.githubusercontent.com/psycopg/psycopg/master/psycopg/psycopg/types/enum.py) 〔要約経由〕 |
| psycopg2 `register_composite` 等 | 明示呼び出し時 | `pg_type`・`pg_namespace`・`pg_attribute`・`pg_range`。0 行のときだけ `%s::regtype` で再検索 | `%s::regtype` | 反映 | [extras.py](https://raw.githubusercontent.com/psycopg/psycopg2/master/lib/extras.py)（接続時の挙動は未確認） 〔要約経由〕 |
| node-postgres | 接続時 | 発行しない。型 OID は `pg-types` が静的に持つ | — | — | [client.js](https://raw.githubusercontent.com/brianc/node-postgres/master/packages/pg/lib/client.js)・[builtins.js](https://raw.githubusercontent.com/brianc/node-pg-types/master/lib/builtins.js) 〔要約経由〕 |
| pgx（参考） | 接続時は発行しない。`LoadType` の明示呼び出し時 | `pg_type`（`typtype`・`typbasetype`・`typelem`・`typdelim`・`typrelid`）・`pg_attribute`・`pg_range` | `$1::text::regtype::oid` | 反映 | [conn.go](https://raw.githubusercontent.com/jackc/pgx/master/conn.go) 〔要約経由〕 |
| SQLAlchemy 2.x | 接続時（dialect の初期化） | `pg_catalog.version()`・`current_schema()`・`show standard_conforming_strings` | — | 接続 | [base.py](https://raw.githubusercontent.com/sqlalchemy/sqlalchemy/main/lib/sqlalchemy/dialects/postgresql/base.py) 〔要約経由〕 |
| SQLAlchemy 2.x | reflection（`has_table`・`get_columns`・`get_pk_constraint`・`get_foreign_keys`・`get_indexes`・`get_check_constraints`・`get_enums` ほか） | `pg_class`（`relpersistence`・`relkind`）・`pg_namespace`・`pg_attribute`（`attgenerated`・`attidentity`）・`pg_attrdef`・`pg_description`・`pg_collation`・`pg_type`・`pg_enum`・`pg_sequence`・`pg_constraint`・`pg_index`（`indkey`・`indclass`・`indoption`・`indnkeyatts`・`indnullsnotdistinct`）・`pg_am`・`pg_opclass`・`pg_inherits`・`pg_tablespace`。関数 `pg_table_is_visible`・`pg_type_is_visible`・`format_type`・`pg_get_expr`・`pg_get_constraintdef`・`pg_get_indexdef`・`pg_get_viewdef`・`pg_get_serial_sequence`・`unnest`・`generate_subscripts` | `attrelid::regclass::text`・`'pg_catalog.pg_class'::regclass` | 反映（`has_table` は Alembic 経由で通常） | 同上・[pg_catalog.py](https://raw.githubusercontent.com/sqlalchemy/sqlalchemy/main/lib/sqlalchemy/dialects/postgresql/pg_catalog.py) 〔要約経由〕 |
| Alembic | migrate 時・autogenerate 時 | `alembic_version` の存在確認は SQLAlchemy の `has_table`。autogenerate の serial 判定で `pg_class`・`pg_depend`・`pg_attribute` | `'pg_class'::regclass`・`cast(:seqname as regclass)` | 通常（存在確認）・反映 | [ddl/postgresql.py](https://raw.githubusercontent.com/sqlalchemy/alembic/main/alembic/ddl/postgresql.py)・[runtime/migration.py](https://raw.githubusercontent.com/sqlalchemy/alembic/main/alembic/runtime/migration.py) 〔要約経由〕 |
| Prisma（prisma-engines） | migrate の接続直後 | `pg_namespace` の存在確認・`version()`・`current_setting('server_version_num')`・`pg_advisory_lock(...)`。`_prisma_migrations` の存在は `pg_class`・`pg_namespace`（`relkind='r'`・`nspname = ANY($1)`） | — | 通常 | [flavour/postgres.rs](https://raw.githubusercontent.com/prisma/prisma-engines/main/schema-engine/connectors/sql-schema-connector/src/flavour/postgres.rs) 〔要約経由〕 |
| Prisma | `db pull`・`migrate dev` の差分計算 | `pg_class`（`relispartition`・`relhassubclass`・`relrowsecurity`・`reloptions`・`reltype > 0`）・`pg_namespace`・`pg_constraint`・`pg_views`・`pg_type`・`pg_enum`・`information_schema.columns`（`data_type`・`udt_name`・`numeric_precision` ほか）・`pg_attribute`・`pg_attrdef`・`pg_index`・`pg_am`・`pg_opclass`・`pg_proc`・`pg_language`・`pg_extension`・`information_schema.sequences`。関数 `obj_description`・`col_description`・`format_type`・`pg_get_expr`・`pg_get_constraintdef`・`pg_get_functiondef` | `typinput::regproc::text` | 反映 | [postgres.rs](https://raw.githubusercontent.com/prisma/prisma-engines/main/schema-engine/sql-schema-describer/src/postgres.rs)（`load_migrations_table` の SQL は未確認） 〔要約経由〕 |
| Rails（ActiveRecord main） | 接続確立直後 | `SET SESSION`（`standard_conforming_strings`・`intervalstyle`・`client_min_messages`・`timezone`。値が ParameterStatus と違うときだけ）・`SET search_path`。型マップは既知 OID の固定表で、問い合わせしない | — | 接続 | [postgresql_adapter.rb](https://raw.githubusercontent.com/rails/rails/main/activerecord/lib/active_record/connection_adapters/postgresql_adapter.rb) 〔要約経由〕 |
| Rails | 型マップに無い OID を初めて見たとき | `pg_type`（`oid`・`typname`・`typelem`・`typdelim`・`typinput`・`typtype`・`typbasetype`）LEFT JOIN `pg_range`（`rngsubtype`）。初回は `typtype IN ('r','e','d')` の一括も | 旧版のみ `'pg_catalog'::regnamespace` | 通常 | 同上 〔呼び出し元は原文確認。SQL の列は要約経由〕 |
| Rails | モデルのスキーマ読み込み・`table_exists?`・マイグレーション | `pg_class`・`pg_namespace`（`relkind IN ('r','v','m','p','f')`・`ANY (current_schemas(false))`）・`pg_attribute`・`pg_attrdef`・`pg_type`・`pg_collation`・`format_type`・`pg_get_expr`・`col_description`・`pg_constraint`（PK）・`pg_depend`・`pg_get_serial_sequence` | `<表名>::regclass`・`'pg_class'::regclass`・`oid::regclass::text` | 通常 | 同上・[schema_statements.rb](https://raw.githubusercontent.com/rails/rails/main/activerecord/lib/active_record/connection_adapters/postgresql/schema_statements.rb) 〔要約経由〕 |
| Rails | `db:schema:dump`・reflection | `pg_index`・`pg_am`・`pg_constraint`（`f`・`c`・`u`・`x`）・`pg_get_indexdef`・`pg_get_constraintdef`・`obj_description`・`pg_inherits`・`pg_get_partkeydef`・`pg_depend`（拡張のメンバー） | `c.confrelid::regclass::text`・`'pg_extension'::regclass` ほか | 反映 | 同上 〔要約経由〕 |
| Django（main） | 接続時 | `set_config('TimeZone', ...)`（ずれているときだけ）。版数は `server_version`（問い合わせなし） | — | 接続 | [base.py](https://raw.githubusercontent.com/django/django/main/django/db/backends/postgresql/base.py)・[operations.py](https://raw.githubusercontent.com/django/django/main/django/db/backends/postgresql/operations.py) 〔要約経由〕 |
| Django | `migrate`（`django_migrations` の存在確認は `get_table_list`） | `pg_class`・`pg_namespace`（`relkind IN ('f','m','p','r','v')`・`relispartition`・`pg_table_is_visible`）・`obj_description` | — | 通常 | [introspection.py](https://raw.githubusercontent.com/django/django/main/django/db/backends/postgresql/introspection.py)・[recorder.py](https://raw.githubusercontent.com/django/django/main/django/db/migrations/recorder.py) 〔要約経由〕 |
| Django | `inspectdb`・`get_constraints` ほか | `pg_attribute`・`pg_attrdef`・`pg_collation`・`pg_type`・`pg_constraint`・`pg_index`・`pg_am`・`pg_depend`・`pg_get_expr`・`pg_get_indexdef`・`col_description`・`unnest ... WITH ORDINALITY` | `'pg_class'::regclass` | 反映 | 同上 〔要約経由〕 |

### 3.3 棚卸しから読み取れること

- **接続時にカタログを必ず引くのは Npgsql だけ**だった（型ロード）。psql・libpq・pgjdbc・psycopg・node-postgres・pgx は
  接続時にカタログを引かない。SQLAlchemy・Prisma・Rails・Django は接続時に**カタログ以外**の文（`version()`・`SHOW`・
  `SET`・`set_config`・`current_setting`・`pg_advisory_lock`）を送る。
- **通常動作で必須**なのは、未知 OID の型照会（pgjdbc・Rails）と、表の存在確認・列定義の読み込み（Rails・Alembic・Django・
  Prisma）。本サーバーが公告する型 OID はすべて組み込み型（WIRE-13 のポインタ）なので、未知 OID の照会は現状では起きにくい。
- 権限関数（`has_table_privilege` 等）で絞るのは pgjdbc の設定時と psql `\l+` だけで、他は `pg_table_is_visible` と
  名前の除外（`pg_catalog`・`pg_toast`・`information_schema`）で絞る。
- `information_schema` を使うのは Prisma（`columns`・`sequences`）と pgjdbc の `_pg_expandarray` だけだった。
- psql の問い合わせの形はサーバー版数で分岐する。本サーバーは `server_version=14.0` を公告しているため、psql 17 は
  PostgreSQL 14 向けの形を送る（§7 の O3）。

### 3.4 カタログ以外の前提（カタログを提供しても解消しない差分）

次の構文・関数は現状の engine に無いか、FROM の無い SELECT に限られる（コード検索で確認）。カタログの段の前提として
別 Issue で扱う必要がある（起票はオーナー承認の後。分け方は §5 の P 群と §7 の O7）。

| ID | 前提 | 使う道具 | 現状 |
| -- | ---- | -------- | ---- |
| P1 | `pg_catalog.` で修飾した関数名・型名（`pg_catalog.format_type(...)`・`::pg_catalog.regclass`） | psql・SQLAlchemy | 型名は `0A000`、関数名は解決されない |
| P2 | `OPERATOR(pg_catalog.~)` 構文・`COLLATE pg_catalog.default` | psql | `OPERATOR(` 構文は無い |
| P3 | `oid`・`name`・`"char"`・OID 別名の型（D7）、段 2 の `pg_node_tree`、段 3 以降の `int2vector`・`oidvector`、段 4 の `aclitem`、それらの text／binary 入出力 | 全般 | 型として無い |
| P4 | `version()`・`current_database()`・`current_setting(...)`・`set_config(...)` | SQLAlchemy・Prisma・Npgsql・Django・pgjdbc | 無い（`set_config('fandhe_db.tenant', ...)` は `42601` で固定） |
| P5 | `SHOW` の一般の GUC（`standard_conforming_strings`・`transaction_read_only`・`default_transaction_read_only`）・`SET SESSION <GUC>`（`intervalstyle`・`client_min_messages`・`extra_float_digits`・`application_name`） | SQLAlchemy・Rails・libpq・pgjdbc・Npgsql | `SHOW` は `timezone`・`search_path` ほか限られた形だけ |
| P6 | `current_schemas(...)`・`current_schema()` を FROM のある文の中で使う（`nspname = ANY(current_schemas(false))`） | pgjdbc・Rails | FROM の無い SELECT だけ（schema ADR §11） |
| P7 | `pg_advisory_lock`（ロックの名前空間は認証済みテナントとキーの組。同じテナントの別セッション間では競合し、別テナント間では同じキーでも競合しない） | Prisma | 無い |
| P8 | `pg_is_in_recovery()` | libpq・pgx・Npgsql（条件付き） | 無い |

## 4. 決定

### D1. 見える行の規則（fail-closed）

- 可視集合（どのオブジェクトの行を返すか）は TABLE-51 (1) に従う。本書はその実装上の追加判断だけを決める。
- 組み込みの固定行（D4 の組み込み範囲。`pg_catalog`・`information_schema`・`public`・組み込み型・アクセス方式など）を加える。
  固定行はテナントに依存しない。
- 導出は、自テナントのスコープを先頭にした範囲走査と、可視な relation ごとの点引き・接頭辞走査だけで行う（§2.1・D5）。
  他テナントの行を読んでから捨てる実装をしない。全行走査で列挙する既存の関数（所有者表の `list_owners_for_tenant_in_txn` 等）は
  カタログの経路で使わない。走査量・応答時間を他テナントのオブジェクト数に依存させないため。
- 行を返すのに必要な情報が欠ける場合（OID が未割り当て等）はその行を返さない。参照先が見えない列は、PostgreSQL が「該当なし」に
  使う値（OID 0・NULL）にし、他テナントの値を出さない。
- `public` の表に付く「テナントごとの情報」（所有者・ACL・トリガ・ポリシー・行セキュリティの有効化）は、自テナントの分だけで
  計算する。例: `relhastriggers` は自テナントのトリガの有無、`relowner` は自テナントの所有者行（無ければ O6 の共有定義の
  仮想所有者）。
- 並び順: `ORDER BY` の無い問い合わせの行順は、可視の内容だけから決まる順（SCHEMA 名・オブジェクト名・列番号などの辞書順）とし、
  OID の大小に依らせない。共有の `public` の OID の払い出し方（D4・O1）によっては、OID の大小が他テナントの DDL の前後関係を
  映しうるため（P1-1 の監査指摘）。PostgreSQL も `ORDER BY` の無い行順を保証しないので、互換性は損なわない。
- 本書はテナントをまたいで共有する名前空間を新しく作らない。組み込みの固定行（共有定義の仮想所有者を含む）の名前は全テナントで
  同じ予約名とし、その名前のロール等の作成は、他テナントの状態に依らない固定の応答で拒否する（O6）。
- エラー: カタログの照会では他テナントの名前・内部キーを解決しないので、エラー文に出る名前は利用者が書いたものだけになる。
  内部キー `#<id>.` は表示しない（schema ADR D2）。
- SQL-80 (2) と TABLE-51 (4) に従う。カタログは文と同じ読み取り元から導く。
- 結果の行数は他の SELECT と同じ上限に従う（[limit-omitted-result-cap.md](limit-omitted-result-cap.md)）。上限に当たる
  かどうかは自テナントの可視集合の大きさだけで決まる。

### D2. 読み取り専用の強制

- 提供するカタログの表・ビュー（D3）への `INSERT`・`UPDATE`・`DELETE`・`TRUNCATE`・`COPY ... FROM`・`MERGE`・`LOCK`、
  およびそれらを対象にする DDL（`ALTER`・`DROP`・`CREATE INDEX`・`CREATE TRIGGER`・`CREATE POLICY`・`GRANT`・`REVOKE`・`COMMENT`）は
  `42501`（`permission denied for table <名前>`。PostgreSQL 17 で一般の利用者がカタログへ書いたときの応答に合わせる。SQLSTATE と文言の
  実機での確認は実装 Issue I2 で行う）。
- 判定は束縛段で、名前が組み込み SCHEMA に解決した時点で行う（副作用ゼロ。利用者のオブジェクトの存否を見ない）。
  テナントに依存しない固定の応答になる。
- `pg_catalog`・`information_schema` を作成先にする作成系 DDL（`CREATE TABLE pg_catalog.x` 等）も `42501` に揃える（PostgreSQL 17 の応答の実機確認は I2）
  （現状の `0A000` から変わる。拒否の固定テストは [rejection-pin-annotation.md](rejection-pin-annotation.md) の手順で付け替える）。
- **永続して保存される式・本文のすべて**からカタログを参照することは `0A000` で拒否する。少なくとも次を含む。
  - ビュー・matview の本文
  - CHECK・DEFAULT・生成列の式、式索引の式、部分索引の `WHERE`
  - 利用者ポリシーの `USING`・`WITH CHECK`、トリガの `WHEN`
  - UDF・トリガ関数・`DO` ブロックの本文のうち、定義として保存されるもの
  - 上の列挙に無い永続定義も既定で拒否する（default-deny。新しい永続定義の種類を足すときは、カタログを参照できないことを
    その Issue で確認し、許可する場合は本書を改める）
  - 理由: 共有の `public` の定義がカタログを読むと、定義者と評価時のテナントが食い違い、可視性の基準があいまいになる
    （fail-closed。必要になったら別 ADR で再検討する）
- 1 回限りの文のうち、カタログを読めるのは通常の SELECT（CTE・副問い合わせ・利用者の表との結合を含む）・`EXPLAIN`・拡張クエリの
  準備済み文だけとする。次は初期は `0A000` とする（§7 の O8。カタログの内容が書き込みの対象・値を左右する文をまとめて扱うため）。
  - `CREATE TABLE ... AS SELECT`・`INSERT ... SELECT`・`COPY (SELECT ...) TO` の元
  - `UPDATE`・`DELETE`・`MERGE` の読み側（`FROM`・`USING` 句、`WHERE` の副問い合わせ）
- NoSQL（HTTP）表層: TABLE-51 (4) に従い、NoSQL へは加えない（`0A000` のまま）。HTTP 表層には修飾名の `pg_catalog` に
  相当する概念が無く、ドライバ互換の需要も無い。

### D3. 提供範囲と段階

棚卸し（§3）の必須度の順に 4 段に分ける。各段は前の段に依存する。表・ビュー・列の名前・型・意味は PostgreSQL 17 の定義に合わせる。

#### 列の扱いの原則

- 提供する表は、PostgreSQL 17 の列のうち次の 2 種類を持つ。
  - **実値列**: 棚卸しで参照されている列。カタログから実際の値を導く
  - **固定値列**: 棚卸しでは参照されないが、型が提供済みで値を固定できる列（`relisshared = false`・`attinhcount = 0` 等）。
    `SELECT *` の形を PostgreSQL に近づけるため
- `xid`・`aclitem`・`pg_lsn` など未提供の型の列（`relfrozenxid`・`relminmxid`・`datfrozenxid` 等）は持たない（参照は `42703`）。
  PostgreSQL との既知の差として記録する。ACL の列（`relacl`・`nspacl`）は `aclitem[]` を提供する段（段 4）までは持たない。
- 漏えい経路になる列は D8 のとおり固定値にする。

#### 段 1: 接続・型照会（Npgsql の型ロード、pgjdbc・Rails・psycopg・pgx の型照会）

| 表・関数 | 実値列（主なもの） | 備考 |
| -------- | ------------------ | ---- |
| `pg_namespace` | `oid`・`nspname`・`nspowner` | 組み込み（`pg_catalog`・`information_schema`・`public`）＋自テナントの SCHEMA＋（一時表があれば）一時スキーマ |
| `pg_type` | `oid`・`typname`・`typnamespace`・`typowner`・`typlen`・`typbyval`・`typtype`・`typcategory`・`typispreferred`・`typisdefined`・`typdelim`・`typrelid`・`typelem`・`typarray`・`typinput`・`typoutput`・`typreceive`・`typsend`・`typbasetype`・`typtypmod`・`typnotnull`・`typcollation`・`typndims` | 組み込み型（WIRE-13 の公告にある型と、その配列型）＋見える ENUM 型とその配列型＋見える relation の行型（`typtype='c'`。Prisma が `reltype > 0` で絞るため） |
| `pg_class`（型ロードに要る範囲） | `oid`・`relname`・`relnamespace`・`reltype`・`relkind` | 行型の `typrelid` の結合先として。全列は段 2 |
| `pg_attribute`（複合型の列） | `attrelid`・`attname`・`atttypid`・`attnum`・`attisdropped` | 全列は段 2 |
| `pg_enum` | `oid`・`enumtypid`・`enumsortorder`・`enumlabel` | 見える ENUM 型だけ |
| `pg_range` | `rngtypid`・`rngsubtype`・`rngmultitypid` ほか | 範囲型が無いため常に 0 行 |
| `pg_proc`（組み込みの入出力関数） | `oid`・`proname`・`pronamespace` | `pg_type.typinput` 等が指す組み込み関数の固定行（Npgsql は `typreceive` を `pg_proc` へ結合する） |
| 関数 | `to_regtype`・`format_type`（型名の表示に限る） | |
| OID 別名の型 | `oid`・`regtype`・`regproc`（出力と、組み込みの固定行の関数名の入力。pgjdbc の `'pg_catalog.array_in'::regproc` 用）・`regnamespace` | D7 |

#### 段 2: 存在確認・列定義（マイグレーションとアプリの通常動作）

| 表・関数 | 実値列（主なもの） | 備考 |
| -------- | ------------------ | ---- |
| `pg_class` | `oid`・`relname`・`relnamespace`・`reltype`・`reloftype`・`relowner`・`relam`・`relpersistence`・`relkind`・`relnatts`・`relchecks`・`relhasindex`・`relhastriggers`・`relrowsecurity`・`relforcerowsecurity`・`relispopulated`・`relispartition`・`relhassubclass`・`reloptions`・`relpartbound`。固定: `relpages`・`reltuples`・`relallvisible`（D8） | 表（`r`）・ビュー（`v`）・matview（`m`）・索引（`i`）・SEQUENCE（`S`）。組み込みカタログ自身の行（`pg_catalog` 配下）も固定行で持つ |
| `pg_attribute` | `attrelid`・`attname`・`atttypid`・`attlen`・`attnum`・`atttypmod`・`attndims`・`attnotnull`・`atthasdef`・`attidentity`・`attgenerated`・`attisdropped`・`attcollation`・`attislocal` | `attnum > 0` の列だけ（システム列は持たない）。`atttypid` は D6 |
| `pg_attrdef` | `oid`・`adrelid`・`adnum`・`adbin` | `adbin` は `pg_node_tree` 型（P3。段 2 で提供）。契約: 値は本サーバー独自の表現で、PostgreSQL の内部表現（`nodeToString` の形）とは一致しない。使える操作は `pg_get_expr(adbin, adrelid)`（既定値の式を正規化した SQL テキストで返す）と `text` へのキャスト（独自の表現のまま）だけで、binary 形式は `0A000`。`pg_index.indexprs`・`indpred`（段 3）も同じ契約 |
| `pg_constraint` | `oid`・`conname`・`connamespace`・`contype`・`condeferrable`・`condeferred`・`convalidated`・`conrelid`・`contypid`・`conindid`・`confrelid`・`confupdtype`・`confdeltype`・`confmatchtype`・`conkey`・`confkey` | PK・UNIQUE・FK・CHECK |
| `pg_depend` | `classid`・`objid`・`objsubid`・`refclassid`・`refobjid`・`refobjsubid`・`deptype` | `OWNED BY` の SEQUENCE → 列（`a`）と、制約 → 索引（`i`）だけ |
| `pg_collation` | `oid`・`collname`・`collnamespace`・`collprovider` ほか | 組み込みの固定行（`default`・`C`・`POSIX` と提供済みの照合） |
| `pg_description` | 全列 | `COMMENT` が無いため常に 0 行 |
| 関数 | `pg_table_is_visible`・`pg_type_is_visible`・`format_type`・`pg_get_expr`・`pg_get_serial_sequence`・`obj_description`・`col_description`・`current_schemas`（FROM のある文）・`to_regclass` | 前提 P1〜P6 |
| OID 別名の型 | `regclass`（入出力・キャスト） | D7 |

#### 段 3: reflection・introspection・メタデータ API

| 表・関数 | 実値列（主なもの） | 備考 |
| -------- | ------------------ | ---- |
| `pg_index` | `indexrelid`・`indrelid`・`indnatts`・`indnkeyatts`・`indisunique`・`indnullsnotdistinct`・`indisprimary`・`indisexclusion`・`indimmediate`・`indisvalid`・`indkey`・`indcollation`・`indclass`・`indoption`・`indexprs`・`indpred` | `int2vector`・`oidvector`・`pg_node_tree` の型（前提 P3） |
| `pg_am`・`pg_opclass` | `oid`・`amname`・`amtype`／`oid`・`opcmethod`・`opcname`・`opcnamespace`・`opcintype`・`opcdefault` | 組み込みの固定行（`heap`・`btree`・`hnsw`・`gin` など提供済みのもの） |
| `pg_sequence` | `seqrelid`・`seqtypid`・`seqstart`・`seqincrement`・`seqmax`・`seqmin`・`seqcache`・`seqcycle` | 定義だけ。採番状態（テナント別）は出さない |
| `pg_views` | `schemaname`・`viewname`・`viewowner`・`definition` | |
| `pg_proc`（UDF） | `oid`・`proname`・`pronamespace`・`proowner`・`prolang`・`prokind`・`proretset`・`prorettype`・`pronargs`・`proargtypes`・`proargnames`・`prosrc` | 自テナントの UDF だけ |
| `pg_language` | `oid`・`lanname` | 固定行（提供済みの言語） |
| `pg_roles` | `oid`・`rolname`・`rolsuper`・`rolinherit`・`rolcreaterole`・`rolcreatedb`・`rolcanlogin`・`rolreplication`・`rolconnlimit`・`rolvaliduntil`・`rolbypassrls`・`rolpassword`（常に `********`） | 自テナントのロール＋共有定義の仮想所有者（D4）。ログイン識別子（テナント修飾子つき）は出さない |
| `pg_inherits`・`pg_extension` | 全列 | 継承・拡張が無いため常に 0 行 |
| `information_schema.schemata`・`tables`・`columns`・`views`・`sequences` | PostgreSQL 17 の列のうち棚卸しで参照されるもの（`columns` は `data_type`・`udt_schema`・`udt_name`・`is_nullable`・`column_default`・`character_maximum_length`・`numeric_precision`・`numeric_precision_radix`・`numeric_scale`・`datetime_precision`・`is_identity`・`is_generated` ほか） | `schemata`・`tables`・`views` は棚卸しの外だが利用が多いため含める。行の絞り込みは D9 |
| 関数 | `pg_get_constraintdef`・`pg_get_indexdef`・`pg_get_viewdef`・`pg_get_functiondef`・`pg_get_userbyid`・`has_table_privilege`・`has_schema_privilege`・`information_schema._pg_expandarray`・`current_database()` | |
| OID 別名の型 | `regrole`・`regprocedure` | D7 |
| wire | RowDescription の table OID と attnum を D4 の OID と列番号で埋める | pgjdbc `ResultSetMetaData` 用。現状の 0 は「不明」として扱われる |

#### 段 4: psql のメタコマンド

| 表・関数 | 備考 |
| -------- | ---- |
| `pg_database`（`oid`・`datname`・`datdba`・`encoding`・`datcollate`・`datctype`・`datlocprovider`・`datallowconn`） | 1 行だけ（D4。名前は O5） |
| `pg_trigger`（`oid`・`tgrelid`・`tgname`・`tgfoid`・`tgtype`・`tgenabled`・`tgisinternal`）・`pg_policy`（`oid`・`polname`・`polrelid`・`polcmd`・`polpermissive`・`polroles`・`polqual`・`polwithcheck`） | 自テナントの分だけ |
| `pg_rewrite`（ビューの `_RETURN`） | 見えるビューだけ |
| `pg_tablespace` | `pg_default` の固定行 |
| `pg_statistic_ext`・`pg_publication`・`pg_publication_rel`・`pg_publication_namespace` | 常に 0 行 |
| 関数 | `pg_get_triggerdef`・`pg_get_ruledef`・`pg_get_function_result`・`pg_get_function_arguments`・`pg_function_is_visible`・`pg_encoding_to_char`・`pg_get_partkeydef`（常に NULL）・`pg_relation_is_publishable`（常に false）・`pg_partition_ancestors`・`array_to_string` |
| ACL の列（`relacl`・`nspacl`・`datacl`）と `aclitem` 型 | ACL（role ADR D5）から自テナントの付与だけを出す。PUBLIC は「同じテナントのロール全体」 |

#### 提供しないもの

D8 の漏えい評価で出さないと決めたもの（`pg_stat_*`・`pg_statio_*`・`pg_stat_activity`・`pg_locks`・`pg_prepared_xacts`・
サイズ系関数・`pg_authid`・`pg_shadow`・`pg_statistic`・`pg_settings`）と、上の表に無いカタログの表・ビュー・関数は `0A000`
（固定文言。現状と同じ）とする。未提供の名前を `42P01` にしない理由: 実在する PostgreSQL のカタログについて「無い」と答えると
道具が誤った分岐をとるため、「未対応」と区別して返す。

### D4. OID の割り当て方式

#### 要件

- (R1) 見える OID の値・空き・順序が、他テナントの操作（作成・削除・ロールバック）で変わらない
- (R2) テナント A と B のオブジェクトの OID が同じ値でも、どちらのテナントからも観測できない（OID は呼び出し元の可視集合の中で
  だけ解釈する）
- (R3) 1 つのテナントの可視集合の中で一意（`pg_class.oid` 等で結合できる）。0 と組み込みの範囲を使わない
- (R4) 安定: 同じオブジェクトの OID は、再起動・接続・改名をまたいで変わらない。削除したオブジェクトの OID を同じ範囲で
  再利用しない（Npgsql はデータソース単位、pgjdbc・Rails は接続単位で OID を鍵に型情報をキャッシュする）
- (R5) 逆引き（OID → オブジェクト）のコストが自テナントの可視集合だけで決まる（psql `\d` は OID を文字列で次の問い合わせへ渡す）
- (R6) 読み取りだけの文（`SELECT`・読み取り専用トランザクション・レプリカ）で書き込みを起こさない

#### 範囲の分割

| 範囲 | 用途 | 払い出し |
| ---- | ---- | -------- |
| 1〜16383 | 組み込み（`pg_catalog`=11・`public`=2200・組み込み型・組み込みカタログの relation・入出力関数・アクセス方式・照合・言語・共有定義の仮想所有者） | PostgreSQL 17 の固定値に合わせる（`pg_catalog`=11・`public`=2200 は [pg_namespace.dat](https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/include/catalog/pg_namespace.dat)。`information_schema` は PostgreSQL では initdb ごとに変わるため、16384 未満の未使用値を 1 つ固定で選ぶ） |
| 16384〜2^30−1 | 共有の `public` のオブジェクトとその従属物（下の P1 案 b・c・d を採る場合だけ使う。既定の案 a では使わず、`public` のオブジェクトもテナントの範囲で払い出す） | P1 案による |
| 2^30〜2^31−2^24−1 | テナントのオブジェクト（SCHEMA・その配下・UDF・ロール・トリガ・ポリシー）とその従属物。案 a では、そのテナントから見た `public` のオブジェクトも含む | テナントごとのカウンタ。各テナントが同じ初期値から数える |
| 2^31−2^24〜2^31−1 | 当該セッションの一時表とその従属物 | セッション内のカウンタ（永続化しない。セッションごとに同じ初期値から数える） |

- 2^31 以上を使わない。OID を符号付き 32 bit で扱うクライアントで負の値にならないようにする。
- 範囲から公開範囲かテナント範囲かが分かるが、これは `relnamespace` から既に分かる情報で、他テナントの情報を含まない。
- 範囲を使い切ったら、その範囲を進める DDL を固定文言の `54000` で拒否する。判定はそのスコープ（自テナント・自セッション。
  案 b〜d の共有の範囲は共有の集合）の状態だけで決まる。

#### 採用: スコープ別の単調カウンタと OID 台帳（案 B）

テナントの SCHEMA 配下・UDF・ロール・トリガ・ポリシーと一時表はこの方式で決める。共有の `public` のオブジェクトの払い出しは、
次節の P1 の比較（オーナー決定 O1: 案 a と案 d を I1 で比較）で決める。

- 新しい redb 表を 2 つ置く（遅延作成）。
  - 正引き: キー `(スコープ, 種別, オブジェクトの鍵)` → OID
  - 逆引き: キー `(スコープ, OID)` → `(種別, オブジェクトの鍵)`
  - スコープは「テナント（`tenant_id`）」（案 b〜d では加えて「共有」）。**キーの先頭に置き**、他テナントの行を構造上引かない
    （SCHEMA・ロールと同じ流儀。所有者表のようにテナントを末尾に置く形〔§2.1〕にはしない）
  - カウンタはスコープごとに 1 行。再利用しない
- オブジェクトの鍵
  - 上位のオブジェクト（relation・ENUM 型・SCHEMA・UDF・ロール・トリガ・ポリシー）は内部キー（`public` は裸の名前、SCHEMA 配下は
    `#<schema_id>.<name>`、UDF は `(function_key, arg_signature)` 等）
  - 従属物（制約・既定値・行型・配列型・`_RETURN` 規則）は `(親の OID, 従属の種別, 名前または列番号)`。親の OID は改名で
    変わらないため、改名で書き換えるのは上位のオブジェクトの正引き 1 行だけで済む
- 追随は DDL と同じ write txn で行う。作成で正引き・逆引きを書き、削除で両方を消す。改名・`SET SCHEMA` では、正引きの鍵の
  付け替えと、逆引きの値（オブジェクトの鍵）の書き換えを**同じ write txn で両方**行う。逆引きに旧い鍵を残さない。改名の後に
  旧い名前で作り直したオブジェクトは新しい OID を得て、元の OID は改名先を指し続ける。
- 値の払い出しは PostgreSQL の OID カウンタと同じく**非トランザクション**とし、未 commit の DDL で払い出した値も再利用しない。
  - SQL-80 のポインタのとおり、DDL を行ったセッション自身はトランザクションの中で未 commit の OID を読める。ROLLBACK・
    savepoint への巻き戻しでカウンタを戻すと、同じ値が別のオブジェクトに付き、その値を読んだクライアントのキャッシュ（R4）が
    取り違える。
  - 規則: プロセス内にスコープごとの「次の値」を持ち、払い出しのたびに進め、巻き戻しでは戻さない。永続側には commit 済みの
    「先取りの上限」を持ち、プロセス内の次の値がこの上限に近づいたら、そのスコープの次の commit で上限を先へ進める。再起動時は
    永続の上限から数え始める（先取りした未使用の値は欠番になる）。1 つのトランザクションで上限を超えて払い出す場合だけ、その
    トランザクションの中で上限を進める（そのトランザクションが巻き戻ったうえで再起動すると、未 commit のまま見えた値が再び
    払い出されうる。これは既知の残余として I1 で幅を測る）。
  - 帰結: 自テナントの巻き戻し・再起動で自テナントのカウンタに欠番ができる。欠番は自テナントの操作か再起動だけで決まり、
    他テナントの情報を含まない（R1 は満たす）。先取りはスコープごとに独立で、他テナントの commit では進まない。
- 一時表はセッションの一時カタログに OID を持たせ、台帳を使わない。
- 既存 DB の移行: 台帳の無いオブジェクトには、プライマリの起動時に 1 つの write txn で OID を付与する（スコープごとに
  `(種別, 鍵)` の昇順。テナントごとに独立に数えるため、あるテナントの付与順は他テナントのオブジェクトに依存しない）。冪等に作り、
  付与済みなら何もしない。
- レプリカ・読み取り専用の文は付与しない（R6）。台帳に OID が無いオブジェクトは D1 のとおり返さない。
- バックアップ・PITR・レプリケーションと savepoint の巻き戻しは、`storage/table_image.rs` の `SHAPES`（engine が使う
  `(K, V)` の型の組の一覧）に載った形の表だけを扱い、未知の形の表は fail-closed で拒否する。台帳の表は既存の形を再利用するか、
  新しい形を `SHAPES` に登録する必要がある。どちらにするかと、各経路で OID が保たれることは実装 Issue I1 の受け入れ条件にする。
  論理ダンプ（[logical-dump.md](logical-dump.md)）からの復元は DDL を再実行するため OID が振り直される（`pg_dump` と同じ）。

#### P1. 共有の `public` のオブジェクトの OID（監査指摘 P1-1）

共有のカウンタ 1 本で `public` のオブジェクトに払い出すと、OID の欠番と進み幅から、他テナントが行った `public` の DDL の有無・
回数と、従属物（制約・既定値・行型など）の数が観測できる（`public` へ DDL できるテナントは、自分の DDL の前後の OID を比べて
間を数えられる）。`public` のオブジェクトは存在中は全テナントに見えるが、「作って消した」履歴と従属物の数はテナントの操作の
痕跡であり、既定で塞ぐ。**オーナー決定（2026-11-21・O1）: 既定で塞ぎ、案 a と案 d を I1 で比較して決める。**
案 c は `public` の DDL をサーバー管理経路に限る場合だけ選びうる。

| 案 | 内容 | 塞げるか | 書き込み量・R4・R6 ほか |
| -- | ---- | -------- | ----------------------- |
| a. テナントのカウンタで払い出す（I1 の比較対象） | `public` のオブジェクトにも、各テナントのカウンタから OID を払い出し、台帳に `(テナント, 種別, public の鍵)` で持つ。払い出すのは**そのテナントの書き込みの時点**（プライマリでの接続確立時と、そのテナントの DDL・書き込みの write txn の中）で、その時点に存在し、まだ OID の無い `public` のオブジェクトだけに付ける | 塞げる。2 回の払い出しの間に作られて消えた `public` のオブジェクトはどのテナントのカウンタも進めない。OID の順序もテナント自身が見た順になる | 書き込み量は「前回の払い出し以降に増えた `public` のオブジェクト数 × 払い出すテナント数」。毎回の要否は `public` のカタログ世代とテナントごとの払い出し済みの世代の比較（定数時間）で判定する。R6 は満たす（SELECT では書かない）が、接続確立が書き込みになりうる。払い出し前の `public` のオブジェクトはそのテナントのカタログに出ない（長い接続では次の書き込みまで遅れる。レプリカではプライマリでの払い出しが届くまで出ない）。`public` のオブジェクトの削除時に全テナントの行を消すと、削除の所要時間が他テナントの払い出し数を映すため、削除は行わず、各テナントの次の払い出し時に不要な行を片付ける（同名の作り直しを区別するため、`public` のオブジェクトに作成時の世代を持たせる） |
| a'. 案 a を `public` の DDL の時点で全テナントへ払い出す | `public` の DDL の write txn で全テナントのカウンタを進める | **塞げない**。作って消したオブジェクトも各テナントのカウンタを進めるため、欠番が残る | 書き込み量がテナント数に比例し、DDL の所要時間がテナント数を映す。採らない |
| a''. 案 a を初めて見えた時点（カタログの照会時）に払い出す | | 塞げる | R6 違反（代替案 F と同じ）。採らない |
| b. `public` の relation だけ HMAC 写像 | `OID = 共有の範囲の先頭 + HMAC(サーバー鍵, public の鍵) mod 範囲の幅`。カウンタを持たない | 欠番・進み幅は生じない | 鍵が名前なので、改名で OID が変わり、同名で作り直すと同じ OID が戻る（R4 違反。代替案 C と同じ）。2^30 の範囲では約 1 万件で 5% 程度の衝突があり、振り直しの順序次第で既存の OID がずれうる。サーバー鍵の保管・レプリカとの共有・復元時の扱いが要る（HMAC は engine の `crypto::hmac_sha256` で足り、依存の追加は要らない） |
| d. 作成時に乱数で引く（案 b の変形。I1 の比較対象） | `public` のオブジェクトの作成の write txn で、共有の範囲から暗号論的乱数で OID を引き、共有の逆引きに既にあれば引き直す。共有の台帳に持ち、改名で鍵だけ付け替える | 欠番・進み幅に履歴が乗らない（値は乱数で、引き直しの要否は全テナントに見える `public` の集合だけで決まる） | 改名で変わらず、作り直せば新しい値になる。ただし削除で逆引きを消すと、乱数が削除済みの値を引いて再利用しうる（R4 違反）。R4 を満たすには、削除した値を墓標として共有の台帳に残し、衝突判定に含める必要がある。その場合、引き直しの回数と範囲の枯渇は削除済みの `public` の DDL の履歴に依存する（下の両立条件）。書き込みは DDL の write txn の中だけ（R6 を満たす）。値が飛び飛びで PostgreSQL らしくない。乱数源は engine の `crypto::random` を使う（依存の追加なし）。案 a の複雑さ（遅延の払い出し・片付け）を負わないため、I1 では案 a の有力な対抗として比較する |
| c. 共有のカウンタのまま許容する | 現行の案 B のまま | 塞がない | 許容できる条件: `public` の DDL がテナントの外のサーバー管理経路だけに限られる（role ADR D6 の方向。テナントのロールが `public` に DDL できない）。その場合に観測できるのは、管理経路が行った `public` の DDL の回数と従属物の数が上限で、テナントの操作の痕跡は含まない。現状は `--ddl-allowed-users` のテナントが `public` に DDL できるため、この条件は満たさない |

- 案 d で R1 と R4 を両立する条件（I1 で検証して明記する）。
  - 削除した値は墓標として残し、衝突判定に含める（再利用しない）
  - 1 回の作成で引く候補の数を固定し（例: 常に k 個引いて未使用の最初を採る）、引き直しの有無で応答・所要時間を変えない
  - 生存と墓標を合わせた件数に、範囲の幅より十分小さい上限を設け、超えたら固定文言の `54000` にする。上限への到達は
    `public` の DDL の総量（共有の履歴）だけで決まり、特定のテナントの操作とは結び付かないことを確かめる
  - k 個すべてが衝突する確率が、上限の件数のもとで無視できる大きさに収まることを示す
- 案 a でも、カウンタの値は削除や巻き戻しで再利用しない（上の非トランザクションの払い出しと同じ規則）。
- 決定は I1 で行う。I1 では、案 a・d（と条件つきの c）について、3 テナントでの欠番・進み幅・順序の非観測性のテスト、書き込み量、
  接続確立の遅延、レプリカでの見え方を比べ、結果と選んだ案を本書に追記してオーナーの承認を得る。

#### 採らない代替案

| 案 | 内容 | 採らない理由 |
| -- | ---- | ------------ |
| A. 内部 ID をそのまま出す | SCHEMA・UDF・ロール等の内部 ID を OID にする | 内部 ID は全テナント共有のカウンタで、値と空きから他テナントの作成回数が分かる（R1 違反）。schema ADR D2 も内部 ID を応答に出さないと決めている |
| A'. PostgreSQL と同じ全体で 1 本のカウンタ | 全オブジェクトを 1 本の新しいカウンタで数える | 他テナントの DDL で空きができる（R1 違反） |
| C. テナント鍵つきの写像（全オブジェクト） | `OID = 範囲の先頭 + HMAC(サーバー鍵, スコープ ‖ 種別 ‖ 鍵) mod 範囲の幅`。台帳を持たない | 表・ビュー・索引には改名で変わらない内部 ID が無く、鍵が名前になるため、改名で OID が変わり、同名で作り直すと同じ OID が戻る（R4 違反。キャッシュした型情報を取り違える）。2^30 の範囲では可視集合の中の衝突が無視できず（約 1 万件で 5% 程度）、衝突の解決順を可視集合だけで決めると、後から作ったオブジェクトが既存の OID をずらしうる。サーバー鍵の保管・レプリカとの共有・復元時の扱いも要る。`public` に限った形は P1 の案 b として比較する |
| D. 問い合わせ時の順位 | 可視集合を並べた順位を OID にする | 作成・削除のたびに他のオブジェクトの OID が変わる（R4 違反） |
| E. セッション単位の仮想 OID | 接続ごとに初めて見た順で払い出す | 同じテナントの別接続で OID が食い違う。Npgsql はデータソース（接続プール）単位で型情報を共有するため壊れる（R4 違反） |
| F. 照会時に台帳へ遅延で書く | カタログを読んだ時点で未割り当てのオブジェクトに払い出す | 読み取りの文が書き込みになり、読み取り専用トランザクション・レプリカで動かず、単一ライタの待ちを読み取りに持ち込む（R6 違反） |

### D5. 列挙と逆引きのコスト

- 正引き・逆引きともにスコープ先頭のキーで、自テナント（と共有・自セッション）の範囲だけを引く。
- 逆引きで他テナントの OID の値を与えられても、自テナントのスコープにその値が無ければ「無い」になる。他テナントの台帳は読まない。
- カタログの照会は、可視集合の件数に比例するコストに収める。可視集合の上限は既存の上限（`MAX_LIST_TABLES` 等）から決まる。
- トリガ・ポリシーは可視な表ごとの `(table_key, 自テナント)` の接頭辞走査で引く。`pg_trigger`・`pg_policy` の列挙コストは可視な表の
  数に比例する（他テナントのトリガ・ポリシーの件数には依らない）。
- 所有者は可視な relation・SCHEMA・関数ごとの点引き（`(種別, 対象, 自テナント)`）で引く。所有者表はテナントがキーの末尾にあり、
  テナント単位の列挙は全行走査になるため（§2.1）、カタログの経路では列挙しない。

### D6. 型 OID と WIRE-13 の整合

- 組み込み型の OID・`typname`・`typlen` の情報源は 1 つにする。engine 側（カタログを導く側）に組み込み型の表を置き、
  wire-server の `WireType` との一致を単体テストで固定する（crate の境界を越えて wire-server から engine へ依存する向きは
  既存どおり）。どちらを正にするかの機械的な形（`WireType` を engine の表から生成するか、往復テストで照合するか）は実装 Issue I3 で決める。
- `pg_attribute.atttypid`・`format_type`・`information_schema.columns.data_type` は、RowDescription で公告する型 OID と一致させる。
  現状の公告では ENUM 列・VECTOR 系列は `text`（25）なので、カタログでも `text` を返す（O2）。
- ENUM 型そのものは `pg_type`（`typtype='e'`）・`pg_enum` に出す（Npgsql・SQLAlchemy・Prisma が ENUM の語彙を読むため）。
  列の型と型の一覧が食い違う点は O2 で判断を仰ぐ。
- `id` 疑似列は `numeric`（1700）として出す（WIRE-13 と同じ）。

### D7. OID の別名の型

PostgreSQL 17 の挙動（[datatype-oid](https://www.postgresql.org/docs/17/datatype-oid.html)・
[regproc.c](https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/backend/utils/adt/regproc.c)）に合わせ、
名前の解決だけをテナントの可視集合に閉じる。

| 型 | 段 | 入力（名前 → OID） | 出力（OID → 名前） |
| -- | -- | ------------------ | ------------------ |
| `oid` | 1 | 10 進の数 | 10 進の数 |
| `regtype` | 1 | 型名（別名 `integer` 等と `[]` を含む。ENUM は可視集合で解決） | `format_type` と同じ表示 |
| `regproc` | 1（出力と組み込みの入力）・3（UDF の入力） | 関数名。段 1 は組み込みの固定行（`array_in` 等の入出力関数）だけ、段 3 で可視の UDF を加える | 関数名 |
| `regnamespace` | 1 | SCHEMA 名（D1 の可視集合） | SCHEMA 名 |
| `regclass` | 2 | relation 名（`search_path`・暗黙の `pg_catalog`・一時表の順。修飾名も可） | relation 名。`search_path` で見えないときは SCHEMA で修飾 |
| `regrole`・`regprocedure` | 3 | ロール名（自テナント）・関数名と引数型 | 同左 |
| `regoper`・`regoperator`・`regcollation`・`regconfig`・`regdictionary` | 提供しない | `0A000` | — |

- 名前の解決は、FROM 句の relation と同じ解決器（schema ADR D4・D5）を使う。第 2 の解決器を作らない。
- 不在と他テナント: 名前が可視集合に無ければ、PostgreSQL と同じく `42P01`（`relation "<入力>" does not exist`。`regtype` は
  `42704` とみているが実機確認は I3）。修飾子が他テナントの SCHEMA でも存在しない SCHEMA でも同じ応答にする（PostgreSQL の `regclassin` は
  `RangeVarGetRelid` を `missing_ok=true` で呼び、見つからなければ `42P01` を返す。修飾子の SCHEMA が無い場合も同じ経路になる
  と読めるが、実機での確認は実装 Issue I4 で行う）。エラー文に出るのは利用者が書いた入力だけ。
- `to_regclass`・`to_regtype` 等は、不在と他テナントのどちらでも NULL。
- 数の入力（`'16390'::regclass`）は、PostgreSQL と同じく存在を確かめずに受け付ける（実機確認は I4）。
- 出力: 0 は `-`、可視集合に無い OID は 10 進の数（PostgreSQL と同じ）。他テナントのオブジェクトの OID と同じ値を渡されても、
  自テナントの可視集合で解釈するだけなので、他テナントの名前は出ない。
- 見えない OID を裸の数で出すことの帰結（いずれも仕様として固定する）。
  - OID は呼び出し元の可視集合の中でだけ解釈するので、他テナントのオブジェクトの OID と同じ値が、範囲の重なる自テナントの
    別のオブジェクトに当たることがある。これは仕様で、他テナントの情報は出ない（R2）
  - 台帳の欠損などで自テナントのオブジェクトの OID が逆引きできない場合も裸の数になる。名前を推測して出さないので、fail-closed の
    期待どおりの挙動である
  - 上の 2 点はテストで固定し、PostgreSQL 17 の出力（`-`・裸の数）との一致は I4 の実機確認を必須にする
- `pg_get_userbyid`・`has_table_privilege(oid, ...)` 等の OID を受け取る関数も同じ規則で、可視集合に無い OID には PostgreSQL が
  存在しない OID に返す値（`unknown (OID=<n>)`・NULL。実機確認は I4・I5）を返す。
- キャスト: `oid` と各別名の型の間、`int4`・`int8` から `oid`、別名の型から `text` を提供する。別名の型と `oid` の比較
  （`c.oid = 'x'::regclass`）を許す。binary 形式（WIRE-14 のポインタ）の対応は段 1 の Issue で決める（Npgsql が binary を
  要求するかは未確認）。

### D8. 件数・サイズ・統計の扱い（漏えい経路の評価）

| 対象 | 漏えい経路 | 決定 |
| ---- | ---------- | ---- |
| `pg_class.reltuples`・`relpages`・`relallvisible` | `public` の表は全テナントの行を同じ物理表に持つため、件数・ページ数が他テナントの行数を含む | 固定値（`reltuples = -1`〔一度も VACUUM・ANALYZE されていない「件数不明」の値。[catalog-pg-class](https://www.postgresql.org/docs/17/catalog-pg-class.html)〕・`relpages = 0`・`relallvisible = 0`） |
| `pg_stat_*`・`pg_statio_*`（`n_live_tup`・`seq_scan` 等） | 同上。加えて他テナントの読み書きの頻度 | 提供しない（`0A000`） |
| `pg_stat_activity`・`pg_locks`・`pg_prepared_xacts` | 他セッション・他テナントの接続と文 | 提供しない |
| `pg_table_size`・`pg_relation_size`・`pg_total_relation_size`・`pg_database_size` | 物理サイズが他テナントの行を含む | 提供しない（psql の `\dt+`・`\l+` は動かない。O4） |
| `xid` 系の列・`txid_current()`・システム列 `xmin` 等 | 全体のトランザクションの進み方から他テナントの書き込み量が分かる | 持たない |
| SEQUENCE の採番状態 | テナント別に持つが、`pg_sequence` は定義だけを出す | `pg_sequence` に状態を出さない。状態は既存の `currval` 等の関数だけ |
| `pg_settings`・GUC | サーバー設定はテナント固有の情報ではないが、運用上の上限等を出す | `pg_settings` は提供しない。`SHOW`・`current_setting` で読める GUC を前提 P4・P5 で個別に選ぶ |
| 応答時間 | 列挙が他テナントの件数に比例すると時間差が出る | D1・D5 のとおり自テナントの範囲だけを走査する |

### D9. ロール権限による絞り込み

- 順序は「テナントの可視集合（D1）→ ロール権限」とし、他テナントのオブジェクトに権限判定を走らせない（role ADR D10 と同じ）。
- `pg_catalog` の表・ビュー: PostgreSQL 17 と同じく、ロール権限では絞らない。PostgreSQL ではカタログは一般の利用者が読め、
  権限の無いオブジェクトの行も見える。本サーバーでは、それを自テナントの可視集合の中に限る。
- `information_schema`: PostgreSQL 17 と同じく、権限で絞る。例えば `tables` は「所有者であるか、何らかの権限を持つ表・ビューだけ」
  （[infoschema-tables](https://www.postgresql.org/docs/17/infoschema-tables.html)）。判定は role ADR D8 の共通ゲートの関数を使い、
  ACL は自テナントの分だけを見る。
- `has_table_privilege`・`has_schema_privilege` 等は、可視集合で名前を解決してから同じゲートで判定する。他テナントの名前は不在と同じ応答。

### D10. 名前解決の変更点

- 暗黙の `pg_catalog` を relation の解決にも入れる。PostgreSQL と同じく、`search_path` に `pg_catalog` が明示されていなければ
  一時表の次・`search_path` の要素より前に `pg_catalog` を引く。
- 互換性: 現状は組み込み SCHEMA を飛ばすため、`public` に `pg_class` 等と同名の利用者の表があれば未修飾名で引ける。変更後は
  カタログが先に当たる（PostgreSQL と同じ）。O10 の決定のとおり、既存 DB にカタログと同名の利用者の表があるかを
  起動時に検出して警告し、破壊的変更として CHANGELOG に記す。どちらも実装 Issue I2 の成果物に含める。
- `information_schema` は `search_path` に明示したときだけ引く（PostgreSQL と同じ）。
- `current_schema()`・`current_schemas()`・`SHOW search_path` の値は現状のまま（schema ADR §11）。`pg_namespace` に出す SCHEMA の
  集合と `current_schemas` の要素の実在判定は同じ可視集合から導く。
- 一時スキーマの表示名は `pg_temp`（schema ADR §11 と同じ。PostgreSQL の `pg_temp_N` ではない）。`pg_my_temp_schema()` はその OID。
- [schema-tenant-namespace.md](schema-tenant-namespace.md) の D9（システムカタログは対象外で `0A000`）と D4 の手順 3 は、
  本書の承認時に本書で置き換える。D9 の本文は本書の承認後に別コミットで「本書へのポインタ」に改める（本コミットでは変えない）。
  spec 側の追記（TASK-350 の成果物のうち spec に属するもの）は spec リポで扱う。

## 5. 実装 Issue の分け方（案。起票はオーナー承認の後）

| Issue | 内容 | 前提 | テスト観点 |
| ----- | ---- | ---- | ---------- |
| I1 OID 台帳 | D4 の台帳・範囲・カウンタ、全 DDL 経路（作成・削除・改名・`SET SCHEMA`・`CASCADE`・ロールバック）への結線、起動時の付与、一時表のセッション内 OID | なし | (a) テナント B の作成・削除・ロールバックの前後で、テナント A の次の OID と既存の OID が変わらない (b) A と B の最初のオブジェクトが同じ OID 値になる (c) 再起動・改名で OID が変わらず、削除後に再利用しない (d) レプリカ・読み取り専用トランザクションで台帳へ書かない (e) バックアップ・PITR・レプリケーション後に OID が一致 (f) 起動時の付与が冪等で、付与順が他テナントに依存しない (g) 範囲の枯渇が `54000` で、他テナントの状態に依存しない (h) 台帳のキーはスコープ（テナント）を先頭にし、テナント単位の列挙が範囲走査で済む。所有者はカタログの経路で可視な relation ごとの点引きに限り、所有者表 `object_owners`（テナントが末尾）をテナント先頭へ組み替えるかを比較して決める (i) D4 の P1 の案 a・d・c を比較し、`public` の DDL の作成・削除・従属物の増減が他テナントの OID の欠番・進み幅・順序に現れないことを 3 テナントで確かめ、選んだ案を本書に追記する (j) 改名・`SET SCHEMA` の後、`regclass` の出力が新しい名前になり、旧い名前で作り直したオブジェクトは新しい OID を得て、元の OID は改名先を指す (k) トランザクションの中で読んだ未 commit の OID が、ROLLBACK・savepoint への巻き戻しの後に別のオブジェクトへ払い出されない（再起動をまたぐ残余の幅を測って記録する） (l) 案 d を採る場合、削除済みの値を再利用せず、引き直しの有無で応答・所要時間が変わらない |
| P1〜P8 カタログ外の前提 | §3.4 の表（修飾した関数名・型名、`OPERATOR(...)`・`COLLATE pg_catalog.default`、`oid`・`name`・`"char"` 型、`version()`・`current_database()`・`current_setting`・`set_config`、一般の `SHOW`／`SET SESSION`、FROM のある文での `current_schemas`、`pg_advisory_lock`、`pg_is_in_recovery()`） | なし | 各関数・構文の PostgreSQL 17 との突き合わせ。テナント依存の値を返さないこと（`version()`・`current_setting` はテナントに依らない固定値）。P7: (a) 同じテナントの 2 セッションが同じキーを取り合うと後者が待つ (b) 別テナントの 2 セッションは同じキーでも待たない (c) 他テナントがロックを保持しているかどうかで、自テナントの取得の成否・待ち時間・`pg_try_advisory_lock` の結果が変わらない |
| I2 仮想カタログの基盤 | 組み込み SCHEMA の relation を読み取り専用の仮想表として解決する層、D2 の `42501`・`0A000`、D10 の暗黙の `pg_catalog`、結果の上限、キャッシュの規則（可視行はキャッシュせず実行のたびに導く。束縛の結果を再利用するなら、再利用の条件にテナント・`search_path`・実効ロール・ACL の世代・カタログ世代・セッションの一時カタログの状態を含めるか、可視性に依存する部分を実行時に再評価する）、D10 の起動時の検出・警告と CHANGELOG への記載 | I1 | (a) 書き込み・DDL がすべて `42501` で副作用ゼロ (b) 永続定義からの参照が `0A000` (c) NoSQL が `0A000` (d) 準備済み文の再利用がテナント・ロール・セッションをまたがない（同じテナントで `SET ROLE` の前後、`GRANT`／`REVOKE` の前後、一時表の作成・削除の前後、別セッションで結果が正しく変わる） (e) 未提供の表が `0A000` のまま (f) カタログと同名の利用者の表がある DB を開くと警告が出る |
| I3 段 1 | D3 段 1 の表・関数・型、D6 の組み込み型の情報源の一本化 | I2・P1〜P4（Npgsql は `version()` を送る） | (a) 3 テナントで互いの ENUM 型が `pg_type`・`pg_enum` に出ない (b) B の ENUM の作成・削除で A の行・件数・OID・並びが変わらない（A の出力をバイト単位で比較） (c) `pg_type` の組み込み行と `WireType` の一致 (d) 層 B: Npgsql の接続、pgjdbc の未知 OID 照会、psycopg の `TypeInfo.fetch`、Rails の型の遅延読み込み |
| I4 段 2 | D3 段 2 と `regclass` | I3・P4〜P7 | (a) B の SCHEMA・表の名前で `'b.t'::regclass`・`'t'::regclass`・`to_regclass` が不在と同じ応答・同じ文言 (b) B のオブジェクトの OID 値 v を A の `regclass` 出力に渡すと、v が A の可視集合に無ければ数のまま、A の可視集合に同じ値のオブジェクトがあれば A のオブジェクトの名前になる（B の名前は出ない。R2・D7）。両方の場合をテストで固定する (c) `public` の表の `reltuples` 等が固定値で、B の行数に依存しない (d) `relowner`・`relhastriggers` が自テナントの分だけで決まる (e) 明示トランザクションの中の DDL が commit 前に他セッションへ出ない (f) 一時表が同じテナントの別セッションに出ない (g) 層 B: Rails のスキーマ読み込みと `db:migrate`、Django の `migrate`、Alembic の `upgrade`、Prisma の `migrate deploy` (h) SQLAlchemy の dialect 初期化（`pg_catalog.version()`〔P1・P4〕・`current_schema()`・`SHOW standard_conforming_strings`〔P5〕）を通って Alembic が接続できる |
| I5 段 3 | D3 段 3、`information_schema`、RowDescription の table OID・attnum | I4 | (a) `information_schema` の行がロール権限で PostgreSQL 17 と同じく絞られ、他テナントの ACL を見ない (b) `pg_roles` にログイン識別子・他テナントのロールが出ない (c) B のロールの OID 値を A の `pg_get_userbyid` に渡すと、A に無ければ `unknown (OID=<n>)`、A に同じ値のロールがあれば A のロール名（D7） (d) 層 B: SQLAlchemy の reflection、Alembic の autogenerate、Prisma の `db pull`、Django の `inspectdb`、pgjdbc の `getTables`・`getColumns`・`getPrimaryKeys` |
| I6 段 4 | D3 段 4、`aclitem` | I5 | 層 B: psql 17 の `\d`・`\d <表>`・`\dt`・`\dn`・`\df`・`\du`・`\l` の出力を PostgreSQL 17 と突き合わせる（公告する `server_version` のもとで） |

- 各段の Issue は、棚卸しの該当行の SQL を原文で確かめ直すことを受け入れ条件に含める（§3.1）。
- 共通の検証: 3 テナント A・B・C で、B と C がオブジェクトの作成・削除・改名・ロールバックを繰り返す場合と何もしない場合とで、
  A が引くすべての提供カタログの出力（行・件数・OID・並び・エラーの SQLSTATE と文言）が一致することを、オラクル比較で固定する。

## 6. OWASP Top 10 の観点

| 観点 | 本書での対応 |
| ---- | ------------ |
| アクセス制御の不備 | 可視集合を名前解決と同じ規則で導き、テナント先頭のキーの範囲だけを引く（D1・D5）。ロール権限は可視集合の後段（D9） |
| インジェクション | カタログの行は内部の値から組み立て、SQL 文字列を連結して別の問い合わせを作らない。`pg_get_*def` の出力は既存の正規化済み定義から作る |
| 不安全な設計 | OID 等が欠けて行を組み立てられないときは返さない。永続定義からの参照は拒否（D2）。読み取りで書き込まない（D4 R6） |
| 情報漏えい | 件数・サイズ・統計・xid を出さない（D8）。内部 ID を OID にしない（D4 案 A を不採用） |
| DoS | 結果の行数は既存の上限、走査は可視集合の大きさ、台帳はスコープごとの上限に収まる |
| 脆弱な依存 | 依存を追加しない（案 C を採る場合も既存の `crypto::hmac_sha256` で足りる） |

## 7. 決定（オーナー、2026-11-21）

本書の初版で未決事項として挙げた O1〜O10 は、オーナーが 2026-11-21 に次のとおり決定した（いずれも本書の提案どおり）。

| ID | 事項 | 決定 |
| -- | ---- | ---- |
| O1 | 共有の `public` のオブジェクトの OID の払い出し方（D4 の P1。共有カウンタのままでは、他テナントの `public` の DDL の有無・回数・従属物の数が欠番と進み幅から観測できる） | 既定で塞ぐ。案 a（テナントのカウンタで、テナントの書き込みの時点に払い出す）と案 d（作成時の乱数）を I1 で比較して決める。案 c（許容）は `public` の DDL をサーバー管理経路に限る場合だけ |
| O2 | ENUM・VECTOR 系の列の型を、現状の公告（`text`）に合わせるか、WIRE-13 を変えるか | 段 1〜3 は公告に合わせて `text`。ENUM 型の OID の公告は、カタログの提供後に別 Issue（BREAKING）で検討する |
| O3 | `server_version=14.0` の公告を維持するか、17 系へ上げるか | 本書の範囲では維持し、層 B は公告する版数で確かめる。17 系への変更は全クライアントに影響するため別 Issue |
| O4 | サイズ系関数を提供しない（psql の `\dt+`・`\l+` が動かない）ままでよいか | 当面は提供しない。テナントの行だけを数える測り方を設計できたら再検討する |
| O5 | `pg_database` の 1 行の `datname` | 接続時の `database` パラメータ（テナントに依らない値に限る）。識別子の規則（TABLE-6 のポインタ。長さ上限・文字種）で検証し、不正なら固定の値に倒すことを実装 Issue の受け入れ条件にする |
| O6 | 共有の定義の仮想所有者（自テナントの所有者行が無い `public` のオブジェクトの `relowner`）の OID と名前 | 組み込みの範囲の固定 OID と固定の予約名の NOLOGIN ロールを全テナントで同じに出す。その名前での作成は他テナントの状態に依らない固定の応答で拒否する |
| O7 | 実装 Issue の分け方と、P 群（カタログ外の前提）を本ツリーに含めるか | §5 の表（I1〜I6・P1〜P8）のとおり。P 群は段 1 の前に置く。起票の時期はオーナーが判断する |
| O8 | 書き込みの文の元としてカタログを使うこと（CTAS・`INSERT ... SELECT`・`COPY (SELECT ...) TO`、`UPDATE`・`DELETE`・`MERGE` の読み側）を許すか | 初期はすべて `0A000`（D2）。需要が出たら、書き込み先が自テナントの行に限られ、カタログの内容で書き込みの成否が他テナントに依らないことを確かめてから文ごとに解禁する |
| O9 | 層 B の対象に Npgsql・pgjdbc・Rails・Django・Prisma・SQLAlchemy を加えるか | 段ごとに必要なものだけ opt-in で加える。既存の 3 クライアントの基盤（[three-client-e2e-harness.md](three-client-e2e-harness.md)）を広げる |
| O10 | 既存 DB にカタログと同名の利用者の表がある場合の扱い（D10） | 起動時に検出して警告し、破壊的変更として CHANGELOG に記す |

## 8. 対象外

- コードの変更（本 Issue は設計記録のみ）
- spec の TABLE-51 への追記（spec リポで扱う）
- `pg_stat_*` 等の統計・サイズ系（D8）、`COMMENT ON`、継承・パーティション・拡張・publication の実体
- NoSQL（HTTP）表層へのカタログの追加
