# ADR: SCHEMA とテナントの関係・名前解決規則（Issue #1712）

- ステータス: Accepted（設計記録。後続 #1713〜#1719・#2234 で実装する）
- 対応 Issue: #1712（親 #1711。後続 #1713・#1714・#1715・#1716・#1717・#1718・#1719、
  Phase 6 の #2234）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-6・TABLE-9・TABLE-24・
  TABLE-27・TABLE-44）・`docs/spec/04-behavior/rls.md`（RLS-7・RLS-8・RLS-10・RLS-12・
  RLS-13）・`docs/spec/04-behavior/sql-surface.md`（SQL-39・SQL-40）。
  spec 本文は転記しない。契約は spec を正とし、本書では再掲しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/catalog.rs`・`catalog/sequence.rs`・`catalog/materialized.rs`・
  `catalog/dependency.rs`・`policy.rs`・`recovery/ledger.rs`・`storage.rs`・
  `sql/mode.rs`・`sql/temp_catalog.rs`・`sql/qualified_column.rs`・`sql/ddl.rs`・
  `crates/wire-server/src/ddl_permission_opt.rs`
- 検証コード: なし（本 Issue は設計記録のみ。コードは変更しない）

## 1. 背景・目的

SCHEMA（親 #1711）の実装前に、次の 4 点を実装設計として固め、後続 sub-issue の判断の
揺れをなくす。

1. テナント内 SCHEMA カタログの構造（SCHEMA はテナント境界に使わない）
2. 既存表の `public` への暗黙所属と、`search_path`（セッション変数）による未修飾名の解決順
3. `information_schema` 相当の扱いと権限（現行の DDL 権限フラグと、Phase 6 の SCHEMA 単位
   GRANT〔#2234〕との関係）
4. RLS ポリシーの SCHEMA 単位の扱い

本書の決定は、公開 Issue の記述・PostgreSQL の公開挙動・本 ADR 独自の実装上の決定だけで
構成する。独自の決定は「#17xx で確定」と明記し、spec が確定させた事項と区別する。

## 2. 公開コードの現状（設計の前提）

| 事実 | 所在 |
| ---- | ---- |
| カタログは全テナント共有の平らな redb テーブルで、キーは裸の名前（`&str`）。表・ビュー・索引・matview・SEQUENCE が relation の名前空間を共有し、衝突は `42P07`。ENUM 型は別の名前空間 | `catalog.rs`（`CATALOG_TABLE`・`VIEWS_TABLE`・`INDEX_CATALOG_TABLE`・`ENUM_TYPES_TABLE`）・`catalog/materialized.rs` |
| SEQUENCE 定義のキーは `(schema, name)` で、schema は `"public"` 固定。キーの形は本 ADR に合わせるよう申し送られている | `catalog/sequence.rs`・[sequence-namespace-and-ledger.md](sequence-namespace-and-ledger.md) D3 |
| 行ストアの物理表は `user_rows/{table_name}`、キーは `(tenant_id, rowid)` | `catalog.rs::user_rows_table_name`・`storage.rs` |
| 識別子は `[A-Za-z_][A-Za-z0-9_]*` に限定し、encode・decode の両側で検証する（TABLE-6） | `catalog.rs::validate_identifier` |
| `tenant_id` は空でない任意のバイト列で、長さに上限がある | `policy.rs::validate_tenant_id` |
| 台帳のキーは `(tenant_id, table_name, operation_id)` | `recovery/ledger.rs` |
| DDL は `--ddl-allowed-users` で許可したユーザーだけが実行でき、既定では `42501`。`PolicyContext` は認証主体（ユーザー名）を持たない | `ddl_permission_opt.rs`・`sql/mode.rs` |
| セッション変数（TimeZone）・一時カタログ・UDF レジストリは `SessionState` が持ち、ROLLBACK で巻き戻る | `sql/mode.rs`・`sql/transaction.rs` |
| 未修飾名は一時表 → 永続表の順で解決する。修飾名 `pg_temp.` は TABLE-27 へ申し送られている | `sql/temp_catalog.rs`・[temp-table.md](temp-table.md) |
| `SET search_path` は現状 `42601`（回帰テストで固定）。`CURRENT_SCHEMA` も定数式では `42601` | `crates/engine/tests/rls_implicit.rs`・`sql/const_expr.rs` |
| 利用者定義の行セキュリティポリシー（RLS-13）のカタログは未実装 | （該当コードなし） |
| 修飾列参照は 2 部の `t.col` まで実装済み。3 部の `s.t.col` は未対応 | `sql/qualified_column.rs`（#1533・#1535） |

## 3. 決定

### D1. SCHEMA カタログの構造（#1713）

- 新しい redb テーブル `SCHEMAS_TABLE: TableDefinition<(&str, &str), &[u8]>`。キーは
  `(tenant_id, schema_name)`、値は版バイト付き blob（内部 ID `u64`・所有者の予約領域〔SQL-39
  までは空〕）
- 内部 ID は単調カウンタ表（`sequence.rs` の ID カウンタと同形）で払い出し、再利用しない。
  同名で作り直した SCHEMA が古いオブジェクトを拾わないようにするため
- `tenant_id` をキーの先頭に置き、テナント単位の range 走査で完結させる。他テナントの行は
  構造上引けない
- `public` は**仮想**とし、`SCHEMAS_TABLE` に行を持たない。既存 DB のバイト列を変えない
  （#1713 の受け入れ条件）。`CREATE SCHEMA public` は PostgreSQL と同じく重複扱い。
  `public` の削除・改名の扱いは TABLE-27 のポインタに従う
- テナントあたりの SCHEMA 数の上限は既存の `MAX_LIST_TABLES` を流用する（DoS 対策）。名前には
  `validate_identifier` を適用する。予約名（`pg_` 接頭辞など）の判定は TABLE-27 のポインタに
  従い、#1713 で確定する
- 却下案: (a) SCHEMA 名だけをキーにする（同名の SCHEMA をテナントごとに独立に持てない）。
  (b) テナントごとに別の redb テーブルにする（テーブル数が無制限に増え、動的なテーブル名を
  組み立てる経路が増える）

### D2. relation の内部キー形（#1713・#1714・#1717）

- `public` のオブジェクトは**従来の裸の名前キーのまま**にする（各カタログの `&str` キー・
  `user_rows/{name}`・台帳の `table_name` のバイト列を変えない）
- テナントの SCHEMA に属するオブジェクトは、内部キー `#<schema_id>.<name>`（`schema_id` は
  10 進）で既存の各カタログに格納する。`#` と `.` は識別子に現れないので、`public` の名前とは
  構造上衝突しない。SCHEMA の改名（#1716）はオブジェクトのキーを書き換えずに済む
- SEQUENCE の `(schema, name)` キーの第 1 要素は、`public` なら `"public"`、テナントの SCHEMA
  なら `"#<schema_id>"` にする（sequence ADR D3 の申し送りへの回答）
- 内部キーの組み立てと解析は 1 つの型（例: `RelationKey`）に集約する。decode 側の検証は
  「裸の識別子」か「`#` + 数字 + `.` + 識別子」だけを受理するよう `validate_identifier` の
  呼び出し点を置き換える（TABLE-6 の fail-closed を保つ）。未検証の文字列を連結して物理表名・
  キーを組み立てる経路を作らない
- 内部キー・内部 ID は**利用者への応答（エラー文・`SHOW`・`current_schemas`・EXPLAIN）に
  出さない**。表示は常に利用者が書いた修飾名か `schema.name` へ逆変換する。内部 ID が漏れると、
  他テナントの SCHEMA 作成回数を推測する手がかりになる
- 表名をキーにする派生物（台帳 `table_name`、分割実行 DML のジョブ記録、表単位の世代カウンタ、
  スカラー索引・HNSW・可視ビットマップ等のキャッシュ）はすべて内部キーで引く。裸の名前で引くと
  `a.t` と `public.t` が同じキャッシュを共有し、テナント境界を越える（P0）。#1713／#1717 の
  実装チェック項目とする
- 却下案: (a) オブジェクト用の修飾版カタログを別に作る（カタログ 6 種以上と全経路が二重化する）。
  (b) キーに `tenant_id` 文字列を埋め込む（任意バイト列で区切りが曖昧になり、改名で書き換えが
  必要）。(c) 既存キーを `("public", name)` のタプルへ移行する（redb の型不一致で既存 DB を
  開けなくなる）

### D3. 行ストアと暗黙 RLS（RLS ポリシーの SCHEMA 単位の扱い）

- テナントの SCHEMA に属する表も、行ストアのキーは `(tenant_id, rowid)` のまま、物理表名は
  `user_rows/{内部キー}`。暗黙 RLS（RLS-7・RLS-8・RLS-10 のポインタ）は SCHEMA に関係なく常に
  適用する。SCHEMA の解決が通っても行のフィルタは省かない（多層防御）
- 利用者定義のポリシー（RLS-13 のポインタ。未実装）は、表の内部キーに紐付けて保持する。
  `ALTER TABLE ... SET SCHEMA` と `ALTER SCHEMA ... RENAME` には内部キーとともに追随させる。
  SCHEMA 単位でまとめてポリシーを持つことはしない（PostgreSQL と同じく表単位）
- `BYPASSRLS` の扱いは RLS-12・RLS-13 のポインタに従う。SCHEMA はこれに影響しない

### D4. 修飾名の解決（#1714）

- `<schema>.<name>` の各部分に `validate_identifier` を適用する（TABLE-6）。3 部の名前と
  `s.t.col` 形式の列参照は TABLE-27 のポインタに従う。#1714 が受理範囲を明記するまでは
  fail-closed にする
- 解決手順:
  1. `public` → 従来の裸の名前キー
  2. `pg_temp` → セッションの一時カタログ（temp-table.md の申し送り。#1714 で結線する。
     それまでは現状の `0A000`）
  3. `pg_catalog`・`information_schema` → システムカタログは対象外のため `0A000`（D9）
  4. その他 → `SCHEMAS_TABLE` を `(ctx.tenant_id, schema)` で 1 回引く。無ければ `3F000`。
     あれば内部キーで対象を引き、無ければ `42P01`
- 他テナントの SCHEMA について、応答コード・文言・処理時間が不在の場合と区別できないように
  する（RLS-12 のポインタ）。テナント外の判定のために別の表を引く分岐を作らない

### D5. `search_path` と未修飾名の解決順（#1715）

- `SessionState` に `search_path`（要素の列）を持たせる。既定値は `"$user", public`。
  `SET search_path TO ...`・`= ...`・`TO DEFAULT`・`RESET search_path`・`SHOW search_path` を
  受理する。`SET LOCAL` は TimeZone と同じく `0A000`。ROLLBACK では `SessionState` ごと
  巻き戻る
- 要素は PostgreSQL と同じく、存在しない名前・他テナントの SCHEMA 名でも受理して保持し、解決時に
  飛ばす（#1715）。untrusted 入力なので、要素数とバイト長に上限を設ける（具体値は #1715 で
  確定し、超過は `22023` 等で拒否する）。識別子として不正な要素は保持できても解決では一致しない
- 未修飾の relation の解決順: (1) 一時表（既存の `SessionRelationLookup`。PostgreSQL の暗黙の
  `pg_temp` と同じ）→ (2) `search_path` の順に、各要素を D4 と同じ規則で引く（`"$user"` は D6 で
  展開し、`public` は裸の名前キー）→ (3) どれにも無ければ `42P01`
- 作成系 DDL の作成先: `search_path` の先頭から見て、セッションのテナントで実在する最初の
  SCHEMA。無ければ `3F000`。一時表は従来どおり一時カタログ
- 組み込み関数・型の解決（暗黙の `pg_catalog`）は現行の優先順を変えない。UDF・ENUM 型の SCHEMA
  修飾は #1717
- 既定値のままで `"$user"` の SCHEMA が無ければ、解決結果は現行と同じ（#1711「未修飾名の既存
  挙動が変わらない」）。テナントが自分のユーザー名と同名の SCHEMA を作った場合だけ `public`
  より優先される。これは PostgreSQL と同じで、影響はそのテナントに閉じる
- 拡張クエリ（Parse／Bind）でも同じ解決を使う。解決結果をキャッシュする場合は、キーに
  `search_path` とテナントを含めるか、`SET search_path` で無効化する。裸の SQL 文字列だけを
  キーにしない
- ビュー本文・FK・索引など永続化する参照は、作成時に解決した内部キーで保存する（早期束縛）。
  作成後に `search_path` を変えても意味が変わらない
- NoSQL 表層の `table` の修飾名と未修飾名の扱いは TABLE-27 のポインタに従う。HTTP は
  ステートレスなので、未修飾名は既定値で解決する

### D6. `"$user"` の展開主体（#1715）

- `PolicyContext` は認証主体を持たないので、wire の handshake が認証に成功した後、
  `ddl_allowed` と同じく 1 回だけ `SessionState` へユーザー名を設定する（SQL 文から変更する
  手段を作らない）
- ユーザー名が無い経路（engine の Rust API・ユーザー名を持たない NoSQL 経路）では、
  `"$user"` を存在しない SCHEMA として飛ばす。結果として既定値は `public` 単独と同じになる
- ロール（SQL-39）導入後の `$user` の意味は SQL-39 のポインタに従う（本 ADR では決めない）

### D7. テナント境界を越える移動・依存の禁止（#1716・#1717）

- 共有の `public` とテナントの SCHEMA の間の表の移動は `42501`（#1716）
- **本 ADR 独自の決定（fail-closed）**: `public` のオブジェクトが、テナントの SCHEMA の
  オブジェクトに依存することを禁止し `42501` で拒否する。対象は、`public` のビューの本文、
  `public` の表の FK・索引、`OWNED BY`、`DEFAULT nextval` から、テナントの SCHEMA の
  オブジェクトへの参照。共有オブジェクトを経由すると、他テナントのセッションが名前解決（D4）を
  通らずにテナント内のオブジェクトへ到達できてしまうため
- 逆向き（テナントの SCHEMA から `public` への参照）は許可する。`public` の行には暗黙 RLS が
  常に掛かる
- 受理範囲は #1717 で確定する。それまでは拒否側に倒す

### D8. DROP SCHEMA と CASCADE の決定的順序（#1713・#1717）

- RESTRICT（既定）: 内部キーの接頭辞 `#<id>.` の range 走査で中身が 1 件でもあれば `2BP01`
- CASCADE: 対象 SCHEMA に属するオブジェクトを各カタログから接頭辞走査で列挙する。外部からの
  依存は `catalog::dependency`（既存の DROP TABLE CASCADE・TRUNCATE CASCADE の閉包計算）で
  閉包をとる。削除順は「依存の逆トポロジカル順 → 種別の固定順 → 内部キーの昇順」で決定的に
  する。全体を 1 つの write txn・1 回の commit で行う
- SEQUENCE の状態行は sequence ADR D3 の全走査削除をそのまま使う
- SCHEMA 内部 ID のカウンタは巻き戻さない

### D9. `information_schema` 相当・システムカタログ（#1718）

- `information_schema.*`・`pg_catalog.*`・`pg_namespace` 等のシステムカタログ照会は TABLE-27 の
  対象外であり、#1718 で対象外が宣言済み。従来どおり fail-closed で拒否する（修飾名は `0A000`）
- `information_schema` と `pg_catalog` を利用者の SCHEMA 名として作ることは拒否する
  （PostgreSQL ではいずれも常に存在するため重複扱い。`pg_` 接頭辞の規則とあわせて #1713 で
  確定する）
- `current_schema()`・`current_schemas()` は #1718 で実装し、search_path に従う値を返す。意味と
  他テナントの SCHEMA を含めない点は TABLE-27 のポインタに従う。それまでは `CURRENT_SCHEMA` の
  現行の `42601` を維持する

### D10. 権限（#1713・#2234）

- Phase 6 まで: `CREATE`・`DROP`・`ALTER SCHEMA` と、SCHEMA を指定する作成系 DDL は、既存の
  `--ddl-allowed-users` のゲート（`sql::ddl::require_ddl_permission`）に従う。既定拒否を維持
  する。同じテナントのセッションは、そのテナントのすべての SCHEMA を参照できる（境界は
  テナントだけ）
- `AUTHORIZATION <ロール>`・`OWNER TO` は、ロール（SQL-39）が入るまで `0A000`
- Phase 6（#2234）: SCHEMA の USAGE・CREATE の検査と一括付与は SQL-40 のポインタに従う。
  ACL のキーは `(tenant_id, schema 内部 ID)`、共有の `public` はテナントごとに独立した ACL と
  する。検査は必ず D4 のテナント解決の**後**に行い、他テナントの SCHEMA は `42501` ではなく
  不在と同じ応答にする（RLS-12 のポインタ）
- 起動時の権限フラグを権限カタログへ置き換える方針は SQL-40 のポインタに従う（本 ADR では
  再掲しない）

### D11. 既存の回帰テストとの関係（#1715・#1719）

- `rls_implicit.rs` の `rls6_disallowed_sql_forms_rejected` は、`SET search_path = tenant_b` が
  `42601` になることを固定している。#1715 で `SET search_path` を受理すると、このアサーションは
  成り立たなくなる
- 対応方針: 期待値を単に消したり弱めたりしない。「`SET search_path = <他テナント名や任意の名前>`
  は受理されるが、その後の `SELECT` の可視行は自テナントのオラクルに収まる（他テナントの行は
  0 件）」というアサーションに置き換え、意図（search_path でテナント境界を越えられない）を保つ。
  `set_config('fandhe_db.tenant', ...)` の `42601` はそのまま残す
- #1719 では、他テナントの SCHEMA 名・オブジェクト名がエラー文に出ないこと、修飾名と
  `search_path` のどちらを経由しても RLS が外れないことを固定する

### D12. 一時表・SEQUENCE との接続

- 一時表: `pg_temp` は D4・D5 のとおり。`current_schemas(true)` に一時スキーマを含める扱いは
  TABLE-27・TABLE-44 のポインタに従う（#1718）
- SEQUENCE: D2 の `(schema, name)` キーの第 1 要素の規則を #1717 で適用する。`public` の
  SEQUENCE はキーのバイト列を変えない

## 4. 後続 sub-issue の前提

| Issue | 前提とする決定 |
| ----- | -------------- |
| #1713 | D1・D2（内部キー型・`public` の仮想化）・D8（RESTRICT）・D9（予約名）・D10 |
| #1714 | D2・D4 |
| #1715 | D5・D6・D11 |
| #1716 | D2（改名でオブジェクトのキーを書き換えない）・D3・D7 |
| #1717 | D2（各カタログ・派生キャッシュの内部キー化）・D5（早期束縛）・D7・D8（CASCADE）・D12 |
| #1718 | D9・D12 |
| #1719 | D4（存在の秘匿）・D11 |
| #2234 | D10 |

## 5. 対象外

- コードの変更（本 Issue は設計記録のみ）
- システムカタログ（`pg_namespace` 等）・psql の `\dn`（#1718 で対象外が宣言済み）
- ロール・所有者（SQL-39）、SCHEMA 単位の GRANT（#2234）、利用者定義ポリシーの実装（RLS-13）
- テーブル単位の物理分離（TABLE-9 のポインタ。SCHEMA はテナント境界に使わない）
