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
| 2. DDL 実行権限 | `--ddl-allowed-users` で列挙した username の接続だけが DDL を実行できる。判定点は `sql::ddl::require_ddl_permission` ただ 1 つ（カタログ照会なし・既定拒否 `42501`）。SQL 文から権限ビットを変える経路は無い | 下表「権限ビットの注入点」 |

ロールと権限のカタログ（#1961）と `GRANT`／`REVOKE` の SQL 文（#1965）は実装済みだが、現在ロールを束縛する認証層（#1971）と
権限検査ゲート（#1966〜#1969）は未実装のため、権限はまだ DML・SELECT・COPY の判定に効かない。wire の全セッションは現在ロールが未束縛で、
`GRANT`／`REVOKE` は対象・ロールの有無を問わず `42501` になる（後述）。所有者の概念は未実装（#2236）。
DDL 権限は全テナント共有のカタログを変更する操作の運用者権限であり、軸 1 とは独立に評価される。

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
| `DROP SCHEMA` | `DropSchema` | `core.rs` の腕 | |
| `ALTER SCHEMA` | `AlterSchema` | `core.rs` の腕 | |
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

## ゲート対象外・特例の経路

| 経路 | 扱い | 根拠・理由 |
| ---- | ---- | ---------- |
| 一時表 | `CreateTempTable`（`sql::ddl::execute_create_temp_table`）はゲート対象外。`DROP TABLE` の一時表のみ分岐もゲートを通らない。`SessionRouted`（一時表だけを参照する文を一時コアへ回す `core/temp_route.rs`）も同様 | セッション内で閉じ、共有カタログを変更しないため（[temp-table.md](temp-table.md)） |
| `SET`／`SHOW`（search_mode・TimeZone・search_path） | DDL ゲートなし。セッション状態のみを変える | 作用がセッションに閉じる |
| `setval`／`lastval` | `SequenceFunction`。DDL ゲートなし | 作用が呼び出し元テナントに閉じる（コード中の設計記録参照） |
| DML・SELECT・カーソル・COPY・TRUNCATE・検索 | DDL ゲートなし。軸 1（テナント境界・可視性）のみで制御 | 現状はロール別権限を持たない |
| UDF の呼び出し | 権限の無いセッションでも呼べる。本体は呼び出し元の権限・可視性で実行し `SECURITY DEFINER` は拒否 | 根拠テスト `crates/engine/tests/sql_t40_up/sql67_function_privileges.rs` |
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
`crates/wire-server/tests/http_nosql/nosql13_ddl.rs` 等。3 ケース（存在する・しない・他テナント所有）の網羅試験は
Issue #1972 の担当（RLS-12 ポインタ）。

## 新規文種追加時の権限検査チェックリスト

- [ ] DDL に当たる文は、自動コミットの腕と明示トランザクション内の腕の**両方**で、カタログ照会・実行本体・`0A000`
  判定より前に `require_ddl_permission` を呼ぶ。呼ぶ場所を `core.rs` の腕か実行本体かで揃え、
  `DropFunction` のような非対称を新たに作らない
- [ ] 構文検証（`validate_*_tokens`）はカタログ照会をしない契約を守る
- [ ] 新しい先頭キーワード（将来の GRANT／REVOKE／COMMENT 等）を持つ DDL は `sql/search_path.rs` の `is_ddl_head`
  へ追加するか、追加不要な理由を記録する（未追加だと権限なしで `42P01` が先に返り得る）
- [ ] 一時表のように意図的にゲート対象外とする場合は、理由を doc コメントと本書の「対象外」表へ記載する
- [ ] NoSQL へ写像する場合は SQL テキストを組み立てずトークン列で同じ実行器へ渡し、第 2 の判定を作らない
- [ ] 権限なしで対象の有無（存在する・しない・他テナント所有）を問わず同じ `42501` になるテストを、既存の根拠テストの型に
  倣って追加する（ビヘイビア ID はポインタで紐付ける）
- [ ] 権限ビットを SQL 文から変える経路を作らない。テナント境界（`PolicyContext`）を緩めない
- [ ] 本書の検査経路一覧・README の `--ddl-allowed-users` 節・`implementation-status.md` を更新する

## 移行先（未実装・ポインタのみ）

| 項目 | 担当 Issue |
| ---- | ---------- |
| ロール・権限モデルの ADR（共通ゲートへ `require_ddl_permission` を寄せる方針を含む） | #1960（PR #2828 で提案中） |
| ロールのカタログ・`CREATE ROLE` 等 | #1959 配下（#1961〜#1963・#2232・#2233） |
| GRANT／REVOKE・DML／検索／COPY 経路への結線 | #1964 配下（#1965〜#1969・#2234〜#2237） |
| 認証済みユーザーとロールの紐付け・users ファイルのブートストラップ化 | #1971・#2238 |
| 存在オラクルの網羅試験 | #1972 |

共通ゲート導入時は、本書の一覧を移行の突き合わせ表として使い、各行がゲートへ結線されたことを確認する。

## 再検証手順

記録時点（`eed0319f`）の件数。差が出たら本書の表を更新する。

| コマンド | 記録時点の結果 |
| -------- | -------------- |
| `git grep -n 'require_ddl_permission(session)' -- crates/engine/src` | `core.rs` 25 件（`CreateIndex` の特例で 1 腕あたり 2 呼び出し）と `sql/udf_ddl.rs` 1 件。ほかに `sql/ddl.rs` のテスト内 |
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
