# 権限モデルの現状と検査経路一覧

- ステータス: 記録（現行実装の一覧。記録時点の origin/main `eed0319f`）
- 対応 Issue: #1973（親 #1970・Phase 6 #1929）
- ビヘイビア ID ポインタ: RLS-12・SQL-40（`docs/spec/04-behavior/`。契約本文は spec を正とし、本書へは転記しない）。
  DDL 権限に関する SQL-23、`wire_code` の WIRE-2 も同様にポインタのみ
- 関連コード: `crates/engine/src/sql/ddl.rs`（`require_ddl_permission`）・`crates/engine/src/core.rs`
  （`execute_parsed_statement`・`execute_in_active_txn`・`execute_validated_with_dict`）・
  `crates/wire-server/src/{auth.rs,ddl_permission_opt.rs,handshake.rs,http/}`
- 関連 doc: [sql-create-table.md](sql-create-table.md)・[drop-table.md](drop-table.md)・
  [create-view.md](create-view.md)・[index-ddl-declaration.md](index-ddl-declaration.md)・
  [udf-catalog.md](udf-catalog.md)・[schema-tenant-namespace.md](schema-tenant-namespace.md)・
  [temp-table.md](temp-table.md)・[trigger-catalog.md](trigger-catalog.md)・
  [nosql-ddl-mapping.md](nosql-ddl-mapping.md)・
  [rls-generalized-read-paths.md](rls-generalized-read-paths.md)・
  [explicit-transaction.md](explicit-transaction.md)

## 目的と範囲

判定点がコードコメントと個別の設計文書に分散しているため、**現行実装の**権限検査を 1 か所へ固める。
Phase 6 の共通ゲートへ移行する前に結線漏れと README の再陳腐化を防ぐことが目的で、
ロール・GRANT・共通ゲートは未実装のため「移行先」（後述）として担当 Issue のポインタのみ示し、挙動は記述しない。
ロール・権限モデルの ADR は #1960（PR #2828 で提案中）が扱う。

## 権限モデルの現状（2 軸）

| 軸 | 内容 | 主な実装 |
| -- | ---- | -------- |
| 1. テナント境界（RLS 相当） | `--users`（`username:tenant_id:phc`）の照合結果からサーバー側でテナントを導出し `PolicyContext` に載せる。DML・検索の可視性はこの軸で決まる。`PolicyContext` は認証主体（ロール）を持たない | `engine::policy`・`engine::tenant` |
| 2. DDL 実行権限 | `--ddl-allowed-users` で列挙した username の接続だけが DDL を実行できる。判定点は `sql::privilege_gate::check` ただ 1 つ（`sql::ddl::require_ddl_permission` はそのファサード。カタログ照会なし・既定拒否 `42501`）。SQL 文から権限ビットを変える経路は無い | 下表「権限ビットの注入点」 |

ロールと権限のカタログ（#1961）と `GRANT`／`REVOKE` の SQL 文（#1965）は実装済みだが、現在ロールを束縛する認証層（#1971）と
権限検査ゲートは DDL（#1966）・読み取り系の文（#1967）・書き込み系の文（#1968）・COPY と HTTP の bound 書き込み入口（#1969）まで実装済み。wire の全セッションは現在ロールが未束縛で、
`GRANT`／`REVOKE` は対象・ロールの有無を問わず `42501` になる（後述）。所有者の概念は #2236 で追加済み（`object_owners`。`ddl_allowed` の所有者相当に加算）。
DDL 権限は全テナント共有のカタログを変更する操作の運用者権限であり、軸 1 とは独立に評価される。

Issue #2238 以降、オブジェクト権限の「所有者相当」は運用者権限（`ddl_allowed`）に加えて、ブートストラップで作られた初期ロールの全権限
（`SessionState::owner_equivalent`。現在ロール＝セッションのロールのときだけ）で成立する。判定点は増やさず、DDL の可否は従来どおり `ddl_allowed` だけで決まる。
HTTP の `SessionPrincipal::session_state()` も同名ロールがあれば現在ロールを束縛する（ADR §19）。

## 権限ビットの注入点（本番コード）

権限ビットは認証成功後に 1 回だけ `SessionState::allow_ddl()`（`crates/engine/src/sql/mode.rs`）で立てる。

| 経路 | 注入点 |
| ---- | ------ |
| SQL（pg wire） | `crates/wire-server/src/handshake.rs`: 認証成功後に `UserStore::is_ddl_allowed(&username)` が真なら `session.allow_ddl()` |
| HTTP（NoSQL） | `http/session/issue.rs` がセッション発行時に `is_ddl_allowed` を確定 → `http/session/store.rs` が保持 → `http/query/ddl.rs` が `principal.ddl_allowed()` が真のときだけ `allow_ddl()` |

`extended_query.rs` の `allow_ddl()` は `#[cfg(test)]` 内でありテスト専用。engine の `examples/` と `*_tests.rs` も本番経路ではない。
起動引数 `--ddl-allowed-users` は `ddl_permission_opt.rs` で検証し（未知ユーザー・空要素・重複・フラグ重複は起動エラー）、
`auth.rs` の `UserStore::with_ddl_allowed_users` へ渡る。

## 検査経路一覧

SQL 表層のディスパッチは `EngineCore::execute_parsed_statement`（自動コミット）と
`execute_in_active_txn`（明示トランザクション内）。DDL ゲートは「構文検証 → 権限 `42501` → カタログ判定」の順で、
権限ゲートは常にカタログ照会・実行本体より前に置く。

### ゲートあり（自動コミット）

| 文種 | `ParsedSql` variant | 判定点 | 備考 |
| ---- | ------------------- | ------ | ---- |
| `DROP TABLE`（永続表） | `DropTable` | `core.rs` の腕 | 一時表のみの指定は下記「対象外」。一時表と永続表の混在は `0A000` |
| `CREATE TABLE` | `CreateTable` | `core.rs` の腕 | |
| `CREATE TABLE AS` | `CreateTableAs` | `core.rs` の腕 | |
| `ALTER TABLE` | `AlterTable` | `core.rs` の腕 | |
| `CREATE VIEW` | `CreateView` | `core.rs` の腕 | |
| `CREATE MATERIALIZED VIEW` | `CreateMaterializedView` | `core.rs` の腕 | |
| `REFRESH MATERIALIZED VIEW` | `RefreshMaterializedView` | `core.rs` の腕 | |
| `DROP VIEW` | `DropView` | `core.rs` の腕 | |
| `DROP MATERIALIZED VIEW` | `DropMaterializedView` | `core.rs` の腕 | |
| `CREATE INDEX` | `CreateIndex` | `core.rs` の腕 | 下記「`CREATE INDEX` の特例」 |
| `DROP INDEX` | `DropIndex` | `core.rs` の腕 | |
| `CREATE TYPE ... AS ENUM` | `CreateType` | `core.rs` の腕 | |
| `DROP TYPE` | `DropType` | `core.rs` の腕 | |
| `CREATE SEQUENCE` | `CreateSequence` | `core.rs` の腕 | |
| `DROP SEQUENCE` | `DropSequence` | `core.rs` の腕 | |
| `ALTER SEQUENCE` | `AlterSequence` | `core.rs` の腕 | |
| `CREATE TRIGGER` | `CreateTrigger` | `core.rs` の腕 | |
| `DROP TRIGGER` | `DropTrigger` | `core.rs` の腕 | |
| `CREATE POLICY` | `CreatePolicy` | `core.rs` の腕 | 表の所有者の判定はロール機構が入るまでこのゲートで代替（RLS-13・Issue #2274） |
| `ALTER POLICY` | `AlterPolicy` | `core.rs` の腕 | |
| `DROP POLICY` | `DropPolicy` | `core.rs` の腕 | `IF EXISTS` でも `42501` |
| `CREATE SCHEMA` | `CreateSchema` | `core.rs` の腕 | |
| `GRANT`／`REVOKE` | `GrantRevoke` | `core.rs` の腕（`sql::grant_ddl::require_grant_principal`） | DDL 権限ではなく現在ロールの束縛を判定する（未束縛は `42501`。カタログ照会・名前解決より前）。DDL 権限ゲートを通過した主体は所有者の機構（#2236）までの代替として所有者相当に扱う（SQL-40・Issue #1965。[role-privilege-model.md](role-privilege-model.md) §8）。構文エラー `42601`・範囲外の形 `0A000`・不正な付与 `0LP01` は状態に依存しないため判定点より前（構文段）に返る。`search_path` の参照ゲートは現在ロール未束縛のセッションでは飛ばす（`42501` を先にする） |
| `DROP SCHEMA` | `DropSchema` | `core.rs` の腕 | 運用者権限またはテナント内の所有者（書き込み txn 内で検査。未束縛は照会より前に `42501`。Issue #2236） |
| `ALTER SCHEMA` | `AlterSchema` | `core.rs` の腕 | `RENAME TO` は運用者権限または所有者（Issue #2236）。`OWNER TO` は下の `ALTER ... OWNER TO` |
| `ALTER {TABLE\|VIEW\|MATERIALIZED VIEW\|SCHEMA} ... OWNER TO` | `AlterOwner` | `core.rs` の腕（`sql::owner_ddl`） | 現在ロールの束縛（未束縛は `42501`）→ 運用者権限または所有者（新所有者へ SET 可能であること等）を書き込み txn 内で判定。明示 Tx 内は `0A000`（Issue #2236。[role-privilege-model.md](role-privilege-model.md) §14） |
| `CREATE [OR REPLACE] FUNCTION` | `Statement::CreateFunction` | `core.rs` の `execute_validated_with_dict` 内の腕 | |
| `DROP FUNCTION` | `DropFunction` | **実行本体** `sql::udf_ddl::execute_drop_function` の先頭 | `core.rs` の腕ではゲートを呼ばない非対称（下記） |

### 明示トランザクション内（`execute_in_active_txn`）

- `CREATE FUNCTION`・`DROP FUNCTION`・`CREATE POLICY`・`ALTER POLICY`・`DROP POLICY` の腕だけが
  `require_ddl_permission` → `0A000`（Tx 内では非対応）の順で評価する。
  権限の無いセッションは `42501`、有るセッションは `0A000`
- `GRANT`／`REVOKE`（Issue #1965）の腕は `require_grant_principal`（現在ロールの束縛。未束縛は `42501`）→ `0A000`
  の順で評価する（ACL の書き込みは独立した write txn で commit され ROLLBACK で取り消せないため）
- その他の DDL は末尾の包括腕で `0A000`（`transaction_feature_not_supported`）となり、権限ゲートは通らない。
  権限の有無に関わらず同じ拒否のため存在情報は出ない。Tx 内で DDL を受理する変更を入れる場合は、
  各 DDL の腕でゲートを先に呼ぶこと（チェックリスト参照）

### 個別の特例

- **`DropFunction` の非対称**: 自動コミットの腕は `execute_drop_function` 内部でゲートを呼び、`core.rs` の腕では呼ばない。
  明示 Tx 内の腕は `core.rs` で呼ぶ。いずれもカタログ・レジストリに触れる前に評価され `IF EXISTS` でも `42501`
- **`CREATE INDEX` の特例**: 組み込み以外の関数を含む索引式（`pending_user_function`）は、権限の無いセッションには
  カタログを引かず従来の `0A000` を返し、権限があればゲート後に自テナントの UDF カタログで判定する（`sql::index_udf`）

### 読み取り系の文（SELECT 権限。#1967）

現在ロールが束縛済みのセッションにだけ適用する（未束縛・運用者権限のセッションはトークン走査も ACL の照会もしない）。
判定は `privilege_gate::check`（`PrivilegeTarget::Relation`）、収集と ACL 照会は `sql/privilege_gate/read.rs`。

| 経路 | 検査する時点 | 実装 |
| ---- | ------------ | ---- |
| 簡易クエリ（`execute_sql_in_session`・`execute_sql_in_txn`）・PL/pgSQL の埋め込み SQL | 構文解析・名前解決の成功直後、実行より前 | `core.rs` の `parse_tokens_gated` |
| `DECLARE` | DECLARE の時点（FETCH／CLOSE は再検査しない。[sql-cursor.md](sql-cursor.md) §1） | 同上（`ParsedSql::Cursor`） |
| `EXPLAIN` | 計画を作る前に、対象の文と同じ検査 | 同上（`Statement::Explain`） |
| 行ロック句つき SELECT | SELECT に加えて UPDATE | 同上（`ParsedSql::LockingSelect`） |
| 拡張クエリ Parse・Bind・Describe | Parse と Describe で拒否し、Bind は束縛済みのセッションで毎回再検査する | `core/temp_route.rs` の `require_prepared_read_privileges`、wire の `needs_session_revalidation`・Bind の再検証条件 |
| HTTP の bound 読み取り API | 入口で、束縛・計画・読み取りより前 | `EngineCore::require_table_select_privilege` |
| ビュー・matview | それ自身の SELECT だけ（基底表は検査しない） | 収集の対象はトークン上の名前のみ |

### COPY（INSERT／SELECT 権限。#1969）

現在ロールが束縛済みのセッションにだけ適用する。判定は `privilege_gate::check`、COPY の要求の組み立ては
`sql/privilege_gate/copy.rs`（`require_copy_privileges`）。`COPY ... FROM STDIN` は対象表の INSERT、
`COPY ... TO STDOUT` は読む relation の SELECT を要求する。

| 経路 | 検査する時点 | 実装 |
| ---- | ------------ | ---- |
| 簡易クエリ（`begin_copy`） | 構文検証・名前解決の後、CopyInResponse・走査より前 | `core.rs` の `begin_copy_unscrubbed` |
| 明示 Tx（`begin_copy_in_txn`） | 同上。読み取り専用 Tx の `25006`・文数登録・走査より前（権限 `42501` が先） | `core.rs` の `begin_copy_active_inner` |
| 拡張クエリ Parse・Bind・Describe | Parse と Describe で拒否する（列の型情報を返さない）。Execute は上の `begin_copy*` | `core/temp_route.rs` の `require_prepared_read_privileges` |
| CopyDone 後の commit | 書き込みの直前に INSERT を再判定する（begin から commit までの REVOKE を検出）。明示 Tx では write txn から ACL を読む | `core.rs` の `commit_copy_in_with_target`（`CopyInBatch.privilege_recheck`） |
| 一時表の COPY | 対象外（セッションの所有物）。`begin_copy_temp` が先に分岐する | `core/temp_route.rs` |

Autocommit の commit 時の再判定は別 txn で ACL を読むため、判定と書き込みの間の REVOKE の窓が残る
（単発 DML と同じ。分割実行のようなチャンク単位の write txn 内判定ではない）。

### HTTP（NoSQL）表層の bound 書き込み入口（#1969）

HTTP の各 op ハンドラは `SessionPrincipal::session_state()`（`http/session/middleware.rs`）で `SessionState` を作り、
engine の bound API へ渡す。`SessionState::default()` を `http/query/` の本番コードで直接組み立てない。
HTTP の現在ロールの束縛は #2238 がこの関数へ足す拡張点で、現状は未束縛（本番の挙動は不変）。

| op・入口 | 必要ビット（SQL 表層の同等の文と同じ関数が決める） | 検査点 |
| -------- | -------------------------------------------------- | ------ |
| `insert`（`execute_bound_insert_in_session`） | INSERT | 名前解決・束縛の後、書き込みより前 |
| 単一行 `update`（`execute_bound_update_in_session`） | UPDATE + SELECT | 同上 |
| 単一行 `delete`（`execute_bound_delete_in_session`・`execute_bound_delete_checked_in_session`） | DELETE + SELECT | 同上（前者は束縛済みの主体のときだけ存在確認 `42P01` を先に行う） |
| 述語形 `update`／`delete`（`execute_bound_predicate_*_in_session`） | `write::partitioned_update_bits`／`partitioned_delete_bits` | 束縛・形検査の後、書き込みより前 |
| 分割実行（`execute_bound_partitioned_*_in_session`） | 同上 | 入口と、各チャンクの write txn 内の再判定（`RelationPrivilegeRecheck`） |
| `scan`・`search`・`aggregate`・`explain` | SELECT（#1967） | 入口 |
| `create_table` 等の DDL op | DDL 権限（`ddl_allowed`） | `execute_parsed_in_session` |
| `show_partitioned_dml`・`cancel_partitioned_dml` | 対象外 | ジョブ表だけに触れる（ロール単位のジョブ所有はスコープ外） |

### 主体（`SessionState`）を持たない Rust API

次の engine の公開 API は主体を取らないため権限を検査しない。`crates/wire-server/src` からは呼ばれない
（テスト用の `parse_sql`・`execute_*_sql` を除く。再検証手順の grep で確認する）。組み込み利用者が直接呼ぶ入口であり、
SQL 表層・HTTP 表層の主体付き入口だけがゲートを通る。

| API | 備考 |
| --- | ---- |
| `insert_row`・`update_row`・`delete_row` | 行単位の生 API |
| `execute_insert_sql`・`execute_update_sql`・`execute_delete_sql`・`execute_truncate_sql` | セッションなしの SQL 入口 |
| `parse_sql` + `execute_parsed_*`（セッションなし） | 同上 |
| `commit_copy_in`・`commit_copy_in_txn` | バッチが `privilege_recheck` を運ぶため、COPY を `begin_copy*` で開始した主体の権限は commit で再判定される |

## ゲート対象外・特例の経路

| 経路 | 扱い | 根拠・理由 |
| ---- | ---- | ---------- |
| 一時表 | `CreateTempTable`（`sql::ddl::execute_create_temp_table`）はゲート対象外。`DROP TABLE` の一時表のみ分岐もゲートを通らない。`SessionRouted`（一時表だけを参照する文を一時コアへ回す `core/temp_route.rs`）も同様 | セッション内で閉じ、共有カタログを変更しないため（[temp-table.md](temp-table.md)） |
| `SET`／`SHOW`（search_mode・TimeZone・search_path） | DDL ゲートなし。セッション状態のみを変える | 作用がセッションに閉じる |
| `setval`／`lastval` | `SequenceFunction`。DDL ゲートなし。ACL で判定すべき主体（束縛済みかつ所有者相当でない）には SEQUENCE の ACL を判定する（`setval`=UPDATE、`lastval`=USAGE か SELECT。Issue #2235） | 作用が呼び出し元テナントに閉じる（コード中の設計記録参照）。判定は `sql/object_privilege.rs` |
| `nextval`／`currval`（INSERT の VALUES・DEFAULT） | `tenant::assign_sequence_values` が採番の前に SEQUENCE の ACL を判定する（`nextval`=USAGE か UPDATE、`currval`=USAGE か SELECT）。SERIAL・IDENTITY・`OWNED BY` 済みの列 DEFAULT の暗黙の採番は免除（SQL-40 (6)(a)）。採番文脈（`SequenceScope`）が主体を運び、セッションの無い経路は検査しない | Issue #2235（設計記録 `role-privilege-model.md` §14） |
| UDF・`LANGUAGE sql\|plpgsql` 関数の呼び出し | 文の実行区間の頭で `core.rs::function_acl_scope` が EXECUTE を持たないカタログ由来の関数キーの集合を張り、解決点（`udf_call::bind_call` の宣言的 UDF 展開・`routine_fold` のルーチン呼び出し・`function_scan::plan`）で `42501` にする。新規の関数は PUBLIC が EXECUTE を持つ（既定） | Issue #2235（§14 S5・S7）。wire-server が core の外で行う INSERT 値・UPDATE／DELETE 述語の束縛は未結線（既知の差） |
| COPY の一時表 | ゲート対象外 | セッションの所有物。永続表の COPY は上の「COPY」節 |
| `SHOW`／`CANCEL PARTITIONED DML` | 書き込みゲートの対象外 | ジョブ表だけを読み書きし利用者の行に触れない（ロール単位のジョブ所有はスコープ外） |
| トリガ起点の内部書き込み・FK の参照アクション | 呼び出し元の権限を検査しない | 所有者実行の意味（PostgreSQL と同じ）。文の入口で対象表を判定済み |
| `LOCK TABLE` | 本 Issue（#1968）の対象外 | スコープ外の候補 |
| UDF の呼び出し | DDL 権限の無いセッションでも呼べる（ACL の EXECUTE は別軸で、束縛ロールにだけ課す。次の行）。本体は呼び出し元の権限・可視性で実行し `SECURITY DEFINER` は拒否 | 根拠テスト `crates/engine/tests/sql_t40_up/sql67_function_privileges.rs` |
| `search_path` の参照ゲート | `sql/search_path.rs` の `is_ddl_head`（先頭が CREATE／DROP／ALTER／REFRESH）かつ権限なしのときは参照走査を飛ばし、存在判定より前に `42501` を返す契約を保つ | 存在オラクル防止 |
| NoSQL（HTTP `POST /v1/query`） | `create_table`／`alter_table`／`drop_table` の op だけがトークン列として SQL 表層と同じ実行器へ到達する（`http/query/ddl.rs`。第 2 の判定は無い）。それ以外の DDL 相当の op は `classify_op` の許可リストで `0A000` | `http/query/gate.rs`・`op.rs` |
| engine の Rust API | `Storage::create_table`／`delete_table` 等の `PolicyContext` を取らない生 DDL はゲートを持たない。SQL 表層からはゲート通過後にだけ到達する | `catalog.rs` の doc コメント。移行先で扱う |

## 権限ゲート以外の `42501` 発生源

`42501` は DDL 権限だけの SQLSTATE ではない。次はテナント境界・依存制約由来で、権限ビットとは独立に発生する。

| 発生源 | 内容 |
| ------ | ---- |
| `error_format.rs` の `ForbiddenTenantMismatch` / `tenant.rs` | テナント帰属不一致の書き込み拒否 |
| `TenantWriteError::ConflictTargetNotVisible`（`tenant.rs`・`sql/upsert_where.rs`） | UPSERT の衝突先が不可視な行 |
| `sql/schema_dependency.rs`（`SchemaDependencyForbidden`） | public と非 public の間をまたぐ依存の拒否 |
| `sql/exec.rs` ほか | 不可視行に関する `42501` の維持 |

## 存在オラクルへの配慮（現状）

権限ゲートがカタログ照会より前に評価されるため、権限の無い主体には対象の有無を問わず `42501` のみが返る。
他テナントの SCHEMA・FUNCTION 等はテナント単位のキーで構造上引けない。根拠テスト:
`crates/wire-server/tests/wire_cli/wire_ddl_permission_cli.rs`・`sql67_function_privileges.rs`・
`crates/wire-server/tests/http_nosql/nosql13_ddl.rs` 等。3 ケース（存在する・しない・他テナント所有）の網羅試験は Issue #1972 で追加した
（RLS-12・SQL-40 ポインタ）。守る不変条件は、束縛ロールの応答が「他テナントだけに在る名前」と「どこにも無い名前」で
完全一致すること（双子 DB のバイト一致）と、ゲート前置の経路（GRANT／REVOKE／SET ROLE／ロール DDL）で 3 ケースが
`42501` に揃うこと。自テナントに在る名前との差（束縛ロールの `42501` 対 `42P01`／`3F000`、SCHEMA 修飾の表 DDL と
拡張クエリの Parse 段）はテナント内の区別として許容する。根拠テスト:
`crates/engine/tests/table_rls/rls12_privilege_existence_oracle.rs`（応答）・
`crates/engine/tests/rls12_privilege_oracle_latency.rs`（応答時間の粗い確認。統計的ゲートではない）・
`crates/wire-server/tests/wire_sql/wire_privilege_oracle.rs`（ErrorResponse のフィールド集合）。

## DDL 文種と必要権限の表

実体は `DdlStatementKind::required_privilege`（網羅 `match`）。`ServerDdl` は `--ddl-allowed-users` による運用者権限。

| `DdlStatementKind` | 現在の必要権限 | 移行先（ポインタ） |
| ------------------ | -------------- | ------------------ |
| `CreateTable`・`CreateTableAs`・`CreateView`・`CreateMaterializedView`・`CreateIndex`・`CreateType`・`CreateSequence`・`CreateTrigger`・`CreateFunction` | `ServerDdl` | SCHEMA の CREATE（#2234） |
| `DropTable`・`AlterTable`・`RefreshMaterializedView`・`DropView`・`DropMaterializedView`・`DropIndex`・`DropType`・`DropSequence`・`AlterSequence`・`DropTrigger`・`CreatePolicy`・`AlterPolicy`・`DropPolicy` | `ServerDdl` | `ServerDdl` のまま（共有定義の変更は運用者権限。所有者は ACL 管理と `OWNER TO` だけ。§14 O2） |
| `CreateRole`・`DropRole` | `ServerDdl` | CREATEROLE（#1971） |
| `CreateSchema` | `ServerDdl` | データベース権限（#2234 以降） |
| `DropSchema`・`AlterSchema`・`DropFunction`・`AlterOwner` | `ServerDdlOrOwner`（運用者権限、または所有者を実行本体が write txn 内で検査） | 所有者のメンバーシップ継承（#2232） |

ゲート対象外または別判定: 一時表、`SET`／`SHOW`、`setval`、GRANT／REVOKE（`privilege_gate::bound_principal` による現在ロール束縛の判定）。

## 新規文種追加時の権限検査チェックリスト

- [ ] DDL に当たる文は、自動コミットの腕と明示トランザクション内の腕の**両方**で、カタログ照会・実行本体・`0A000`
  判定より前にゲートを呼ぶ（`DdlStatementKind` へ文種を足し、`required_privilege` の表と本書の「DDL 文種と必要権限の表」を更新したうえで `privilege_gate::require_ddl` を呼ぶ）。呼ぶ場所を `core.rs` の腕か実行本体かで揃え、
  `DropFunction` のような非対称を新たに作らない
- [ ] 構文検証（`validate_*_tokens`）はカタログ照会をしない契約を守る
- [ ] 新しい先頭キーワード（将来の GRANT／REVOKE／COMMENT 等）を持つ DDL は `sql/search_path.rs` の `is_ddl_head`
  へ追加するか、追加不要な理由を記録する（未追加だと権限なしで `42P01` が先に返り得る）
- [ ] 一時表のように意図的にゲート対象外とする場合は、理由を doc コメントと本書の「対象外」表へ記載する
- [ ] NoSQL へ写像する場合は SQL テキストを組み立てずトークン列で同じ実行器へ渡し、第 2 の判定を作らない
- [ ] 権限なしで対象の有無（存在する・しない・他テナント所有）を問わず同じ `42501` になるテストを、既存の根拠テストの型に
  倣って追加する（ビヘイビア ID はポインタで紐付ける）
- [ ] 権限ビットを SQL 文から変える経路を作らない。テナント境界（`PolicyContext`）を緩めない
- [ ] 読み取り系の文種（`ParsedSql`・`Statement` の新しい variant）は `privilege_gate::read::classify` の網羅 `match` に分類を足し、
  読み取りなら SELECT を要求する側へ、そうでなければ対象外の理由をコメントへ記録する
- [ ] 書き込み系の文種（`ParsedSql` の新しい variant）は `privilege_gate::write::classify_write` の網羅 `match` に分類を足し、
  書き込み先と必要ビットを返すか、対象外の理由をコメントへ記録する。CASCADE のように文の外へ及ぶ書き込みは write txn 内で再判定する
- [ ] HTTP の op は `principal.session_state()` で `SessionState` を作り、engine の bound API へ渡す（`SessionState::default()` を
  本番コードで直接組み立てない）。bound 書き込み API は SQL 表層と同じビット関数から必要ビットを決める
- [ ] 本書の検査経路一覧・README の `--ddl-allowed-users` 節・`implementation-status.md` を更新する

## 移行先（未実装・ポインタのみ）

| 項目 | 担当 Issue |
| ---- | ---------- |
| ロール・権限モデルの ADR（共通ゲートへ `require_ddl_permission` を寄せる方針を含む） | #1960（PR #2828 で提案中） |
| 権限検査の共通ゲート導入と DDL 経路への結線 | #1966（実装済み。`sql/privilege_gate.rs`） |
| ロールのカタログ・`CREATE ROLE` 等 | #1959 配下（#1961〜#1963・#2232・#2233） |
| GRANT／REVOKE・DML／検索／COPY 経路への結線 | #1964 配下（#1965〜#1969・#2234〜#2237）。SELECT・カーソル・検索は #1967 で実装済み（`sql/privilege_gate/read.rs`）、DML は #1968、COPY は #1969。`ALTER DEFAULT PRIVILEGES` は #2237 で実装済み（権限ゲートは置かず、現在ロールの束縛と `FOR ROLE` の SET 経路で認可。判定は `catalog/privilege/default_acl.rs`） |
| 認証済みユーザーとロールの紐付け・users ファイルのブートストラップ化 | #1971・#2238 |
| 存在オラクルの網羅試験 | #1972（実装済み。上記「存在オラクルへの配慮」の根拠テスト） |

共通ゲート導入時は、本書の一覧を移行の突き合わせ表として使い、各行がゲートへ結線されたことを確認する。

## 再検証手順

記録時点（`eed0319f`）の件数。差が出たら本書の表を更新する。

| コマンド | 記録時点の結果 |
| -------- | -------------- |
| `git grep -n 'privilege_gate::require_ddl(' -- crates/engine/src` | `core.rs` の DDL 腕（`CreateIndex` の特例で 1 腕あたり 2 呼び出し。`CreateFunction` は自動コミットの腕のみ、`DropFunction` の自動コミット側は `sql/udf_ddl.rs`）。束ねた Tx 内の腕は `require_server_ddl` |
| `git grep -n 'privilege_gate::read::' -- crates/engine/src` | `core.rs`（`parse_tokens_gated`・bound 読み取り API の入口）と `core/temp_route.rs`（拡張クエリ）。#1967 の記録時点 |
| `git grep -n 'privilege_gate::write::' -- crates/engine/src` | `core.rs`（`parse_tokens_gated`・分割実行・TRUNCATE）、`core/temp_route.rs`（拡張クエリ・TRUNCATE）、`sql/exec.rs`・`tenant.rs`（再判定の受け渡し）。#1968 の記録時点 |
| `git grep -n 'privilege_gate::copy::' -- crates/engine/src` | `core.rs`（`begin_copy_unscrubbed`・`begin_copy_active_inner`）と `core/temp_route.rs`（拡張クエリ）。#1969 の記録時点 |
| `git grep -n 'SessionState::default()' -- crates/wire-server/src/http` | `http/query/` の本番コードには無い（`session/middleware.rs` の `session_state()` のみ。残りは `#[cfg(test)]`）。#1969 の記録時点 |
| `git grep -n 'ddl_allowed()' -- crates/engine/src` | `sql/privilege_gate.rs`（`Principal::from_session`）と `sql/mode.rs`（定義・`owner_equivalent`）、`sql/grant_ddl.rs`・`sql/role_ddl.rs`（メンバーシップの root 判定。#2238 後も DDL 権限のまま）、テストのみ |
| `git grep -n 'owner_equivalent()' -- crates/engine/src` | `sql/privilege_gate.rs`（`Principal::from_session`・`is_owner_equivalent`）と `sql/schema_privilege.rs`。初期ロールの全権限（#2238）はここだけに作用し、DDL の判定（`ddl_allowed`）には混ぜない |
| `git grep -n 'allow_ddl()' -- crates/wire-server/src` | 本番注入点は `handshake.rs` と `http/query/ddl.rs`。`extended_query.rs` は `#[cfg(test)]` |
| `git grep -n '42501' -- crates/engine/src crates/wire-server/src` | 権限ゲート以外の発生源は上表のとおり |

## セキュリティ観点（OWASP Top 10）

| 観点 | 本書での扱い |
| ---- | ------------ |
| アクセス制御の不備 | 文書のみで挙動を変えない。一覧とチェックリストでゲート結線漏れ・`is_ddl_head` の追随漏れを検出しやすくする |
| 情報漏えい（存在オラクル） | ゲートがカタログ照会より前である契約を記録し、新規文種にも要求する |
| 認証の不備 | 権限ビットは認証成功後に 1 回だけ立てる。SQL 文から変える経路を作らない |
| 不安全な設計 | 既定拒否（fail-closed）を明記し、ゲート対象外は理由付きで列挙する |
| インジェクション | NoSQL の DDL はトークン列で渡す現行設計をチェックリストに含める |
| 秘密情報 | 実ユーザー名・資格情報・PHC 文字列の例を載せない。spec 本文を転記しない |
