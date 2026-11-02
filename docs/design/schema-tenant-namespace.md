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
構成する。spec が定める契約（権限・予約名・`current_schema` の意味など）はポインタ表記に留め、
README の公開済み要点を超えて具体化しない。公開境界を広げる場合は管理者の承認を得てから追記する。
独自の決定は「#17xx で確定」と明記し、spec が確定させた事項と区別する。

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
- 逆向き（テナントの SCHEMA から `public` への参照）も、public 側の DDL（`DROP` の依存判定・
  `CASCADE`）がテナント境界に対応するまでは拒否する（#1717 のレビューで確定）。`public` の `DROP` の
  依存判定は全テナントを走査するため、許すと他テナントの非公開の依存の有無で結果（成功／`2BP01`）が
  分かれて存在情報が漏れ、`CASCADE` では他テナントの定義まで消える。対応後に再検討する
- 受理範囲は #1717 で確定した（同じ所属内の参照だけを許可する）

### D8. DROP SCHEMA と CASCADE の決定的順序（#1713・#1717）

- 列挙規則: 各カタログのキーは `#<id>.` 接頭辞の range 走査で列挙する。ただし SEQUENCE 定義の
  キーは `(schema, name)` のタプルで第 1 要素が `#<schema_id>`（ドットなし）なので、第 1 要素が
  `#<schema_id>` と一致するタプル範囲で走査する（接頭辞走査では SEQUENCE のみを持つ SCHEMA を
  空と誤判定し、CASCADE で定義を取り残すため）
- RESTRICT（既定）: 上記の列挙で中身が 1 件でもあれば `2BP01`
- CASCADE: 対象 SCHEMA に属するオブジェクトを上記の規則で各カタログから列挙する。外部からの
  依存は `catalog::dependency`（既存の DROP TABLE CASCADE・TRUNCATE CASCADE の閉包計算）で
  閉包をとる。削除順は「依存の逆トポロジカル順 → 種別の固定順 → 内部キーの昇順」で決定的に
  する。全体を 1 つの write txn・1 回の commit で行う
- SEQUENCE の定義キーは上記のタプル範囲走査で削除し、状態行は sequence ADR D3 の全走査削除を
  そのまま使う
- SCHEMA 内部 ID のカウンタは巻き戻さない

### D9. `information_schema` 相当・システムカタログ（#1718）

- `information_schema.*`・`pg_catalog.*`・`pg_namespace` 等のシステムカタログ照会は TABLE-27 の
  対象外であり、#1718 で対象外が宣言済み。従来どおり fail-closed で拒否する（修飾名は `0A000`）
- `information_schema` と `pg_catalog` を利用者の SCHEMA 名として作ることは拒否する
  （PostgreSQL ではいずれも常に存在するため重複扱い。`pg_` 接頭辞の規則とあわせて #1713 で
  確定する）
- `current_schema()`・`current_schemas()` の意味は TABLE-27 (7) のポインタに従い、#1718 で確定した
  （§11）。`INSERT VALUES`・`DEFAULT` の裸の `CURRENT_SCHEMA` は現行の `42601` を維持する

### D10. 権限（#1713・#2234）

Phase 6（#2234）の契約と実装記録は [role-privilege-model.md](role-privilege-model.md) §10 を参照する。

- Phase 6 まで: `CREATE`・`DROP`・`ALTER SCHEMA` と、SCHEMA を指定する作成系 DDL は、既存の
  `--ddl-allowed-users` のゲート（`sql::ddl::require_ddl_permission`）に従う。既定拒否を維持
  する。同じテナントのセッションは、そのテナントのすべての SCHEMA を参照できる（境界は
  テナントだけ）
- `AUTHORIZATION <ロール>`・`OWNER TO` は、ロール（SQL-39）が入るまで `0A000`
- Phase 6（#2234）: SCHEMA 単位の権限の契約は SQL-40・RLS-12 のポインタに従い、#2234 で確定する。
  本 ADR では決めない（再掲しない）

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

## 6. Issue #1713 実装記録

TABLE-27・TABLE-36 のポインタ。`CREATE SCHEMA`／`DROP SCHEMA` と SCHEMA カタログを engine に入れた。

- **移行処理なし**: `public` は仮想 SCHEMA で `schemas` 表に行を持たない。既存のカタログキー・
  `user_rows/{name}`・台帳キーは書き換えず、格納形式の版も上げない。`schemas`・`schema_id_counter` は
  最初の `CREATE SCHEMA` まで作らない（遅延作成。存在判定は読み取り専用オープン）。既存データの
  バイト列が変わらないことは `catalog::namespace` の単体テスト（全表の件数と主要カタログの
  key/value の前後比較）で固定した
- **格納**: `catalog/namespace.rs`。`schemas` のキーは `(tenant_id, schema_name)`、値は固定 10 バイト
  （版 1・内部 ID `u64` LE・所有者予約領域 1 バイト〔SQL-39 まで常に 0〕）。未知の版・長さ不一致・
  予約領域の非零・id=0 は `CorruptSchema`（fail-closed）。ID カウンタは単調で再利用しない
- **受理範囲**: `CREATE SCHEMA [IF NOT EXISTS] <名>`、`DROP SCHEMA [IF EXISTS] <名> [RESTRICT]`。
  `AUTHORIZATION`・schema_element・`CASCADE`・複数名は `0A000`、修飾名・不正な識別子・余分な
  トークンは `42601`。明示トランザクション内は既存の DDL 方針どおり `0A000`
- **判定順序**: CREATE は 構文 → DDL 権限 → 予約名（`IF NOT EXISTS` より先）→ 組み込み名・同名の
  既存 → 件数上限 → ID 払い出しと保存。DROP は 構文 → DDL 権限 → 組み込み名 → 存在
  （`(tenant_id, name)` の 1 回 get）→ 空判定 → 削除。予約名（`pg_` 接頭辞）と組み込み名
  （`public`・`pg_catalog`・`information_schema`）は ASCII の大文字小文字を無視して判定する
  （lexer が畳み込みをしないため、拒否側に倒す）
- **エラー分類**: `42P06`（DuplicateSchema）・`3F000`（InvalidSchemaName）・`42939`（ReservedName）を
  `ErrorClass` に追加した（`count` 73 → 76）。組み込み SCHEMA の DROP は既存の `42501`、中身ありは
  既存の `2BP01`。文言は利用者が書いた名前だけを含み、内部 ID・依存オブジェクト名・他テナントの
  情報を含めない
- **`RelationKey`**: 内部キー（`name`／`#<id>.<name>`）の組み立てと解析を `namespace.rs` に集約した。
  本 Issue で使うのは DROP の空判定の接頭辞だけ。既存の decode 側 `validate_identifier` 呼び出し点の
  置き換えは、`#<id>.` キーを書く経路を入れる #1714／#1717 で行う（書き込み経路が無い段階で受理
  範囲を広げると fail-closed が弱まるため）
- **対象外**: 修飾名の解決（#1714）、`search_path`（#1715）、`ALTER SCHEMA`（#1716）、tenant SCHEMA
  へのオブジェクト作成と `CASCADE`（#1717）、システムカタログ（#1718）、横断テスト（#1719）、
  SCHEMA 単位の GRANT（#2234）。`DROP SCHEMA` の複数名指定は本 Issue では `0A000`

## 7. Issue #1714 実装記録

TABLE-27 のポインタ。`schema.table` のテーブル参照を解決する層を engine に入れた（本文は ADR D2・D4 に従う）。

- **方式**: パーサ（`sql::allowlist::Parser`）は変えず、字句解析の直後・構文解析の前に
  `sql::relation_name::rewrite_relation_names` が**テーブル参照の位置**の `QualifiedIdent` を解決して
  `Ident(<名前>)` へ置き換える。置き換え後は未修飾の文とトークン列が同一になるため、暗黙 RLS・
  DDL 権限ゲート・一時表の振り分け・ビュー本文の保存（解決後の名前で保存）が修飾の有無で変わらない。
  既存テストは 1 行も変更していない
- **対象位置**: `FROM`（表参照の `FROM` のみ。`IS DISTINCT FROM`・`EXTRACT(.. FROM x)` を除く）・
  `FROM a, b` のカンマ・`JOIN`・`INSERT INTO`・`UPDATE`・`TRUNCATE`（複数表）・`TABLE`
  （`CREATE`／`ALTER`／`DROP`・`IF [NOT] EXISTS`）・`ONLY`・`REFERENCES`・`CREATE INDEX .. ON`・
  `COPY`。ビュー・SEQUENCE・索引名・`RENAME TO` などオブジェクト名の位置は従来どおり（#1717）
- **解決規則**: `public`（大小無視）は裸の名前（`CREATE TEMP TABLE public.t` は `42P16`）。
  `pg_catalog`／`information_schema`／`pg_temp` は `0A000`。それ以外はテナント不明の入口で `3F000`、
  ctx を持つ入口（`execute_sql*`・`begin_copy*`）は `3F000` を受けて `(tenant, schema)` の 1 回の get で
  精密化する（無ければ `3F000`・他テナントの SCHEMA も同じ応答、あれば参照系 `42P01`・作成系 `0A000`）。
  `42P01` の文言は利用者が書いた `s.t` だけを含む。通常の文は精密化の追加コストを払わない
- **判定順序の変化**: 解決エラーは構文解析より先に返る（置き換え用の名前で解析を続けると別の
  オブジェクトに誤解決しうるため）。ただし選択リストなどに列修飾（`x.id`）が残る文は、従来の
  `42601`（SQL-44 の既存契約）を保つため解決エラーを保留して書き換えない
- **一時表**: `public.<一時表の名前>` は永続側を指すが、解決後は未修飾名と同じになり一時表に当たる。
  取り違え（例: `DROP TABLE public.t` が一時表を落とす）を避けるため、解決より前に `0A000` で
  拒否する（`temp_catalog::reject_public_qualified_temp`）。`pg_temp.x` は全経路で `0A000` のまま
  （永続の `x` に落ちない）。`pg_temp` の結線は未実施
- **拡張クエリ**: Parse／Bind の経路は ctx を持たないため、テナント SCHEMA の存在判定ができず
  `3F000` のままになる（`42P01` にならない。fail-closed）。ctx を渡すシグネチャ変更は #1717 で
  再検討する
- **#1717 への申し送り**: `#<id>.` キーの書き込み、decode 側 `validate_identifier` の
  `RelationKey::parse` への置き換え、テナント SCHEMA 解決が成功しうるようになった後の実行時の再検証
- **対象外**: `search_path`（#1715）、オブジェクト名の修飾、NoSQL 表層の `table`、
  `SHOW/CANCEL PARTITIONED DML .. ON <table>`、引用付き修飾名・3 部名（従来の `42601`）

## 8. Issue #1717 実装記録

TABLE-27 のポインタ。テナントの SCHEMA 配下に表・ビュー・索引・SEQUENCE・UDF を置けるようにし、
`DROP SCHEMA ... CASCADE` を入れた（本文は ADR D2・D5・D7・D8 に従う）。

- **解決器**: #1714 の解決は bool（存在するか）から内部 ID（`Option<u64>`）を返す `SchemaResolver`
  へ変えた。解決できたテナント SCHEMA の名前位置は `Token::Ident("#<id>.<name>")`（内部キー）へ
  置き換わる。ctx を持つ入口（`execute_sql*`・`begin_copy*`）は `Storage::schema_id(tenant, schema)` の
  1 回の get を渡し、ctx を持たない入口（`parse_sql`・文字列の検証器・Rust API）は `None` のまま
  `3F000`。#1714 の「`3F000` を受けて後から精密化する」処理（`refine_*`）は不要になり削除した。
  存在しない SCHEMA と他テナントの SCHEMA は同じ `3F000`・同じ文言
- **名前位置**: #1714 の表参照に加え、`CREATE`／`DROP`／`ALTER` の `VIEW`・`SEQUENCE`、`DROP INDEX`、
  `IF [NOT] EXISTS` の後、カンマ区切りの DROP 一覧、関数名の位置（`CREATE FUNCTION s.f(`・式中の
  `s.f(x)`）、`nextval`／`currval`／`setval` の文字列引数（regclass 相当。解決後は文字列ではなく
  `#` を含む `Ident` に置き換え、利用者が文字列で内部キーを直接指定できないようにする）。
  `RENAME TO` の新しい名前・`CREATE INDEX` の索引名は修飾できない（PG と同じ。`42601`）。
  関数名の位置で SCHEMA が見つからない場合は従来の `42601`（SQL-44 の範囲外の形）のまま
- **内部キーを作れる者**: 名前解決器だけ。字句解析器は `#` を含む識別子を作らない（引用形でも拒否）。
  保存済みビュー本文の再読込（`lexer::tokenize_stored_view_body`）と本実装自身の描画の再読込
  （`tokenize_rendered`）だけが、正規形の引用形 `"#<id>.<name>"` を 1 個の `Ident` として受理する。
  `render_tokens`・単純形ビュー本文の描画は内部キーを引用形で出力する
- **検証器の二分**: `catalog::validate_relation_key`（裸の識別子または内部キー）を新設し、relation 名
  （表・ビュー・索引・SEQUENCE のカタログキー、FK の参照先、`ColumnDefault::Sequence`、UDF の登録キー）の
  位置だけを差し替えた。列名・SCHEMA 名・制約名・別名・利用者の生入力は厳格な `validate_identifier` の
  まま。利用者の生の表名を受ける入口は厳格なまま: NoSQL（HTTP）の `table` は `check_identifier`、
  Rust API の `EngineCore::insert_row`／`update_row`／`delete_row` は `validate_identifier` を先に通す。
  下位の `tenant`・`catalog` モジュールの `pub` 関数は組み込み側（信頼できる呼び出し元）向けで、内部キーを
  受理する
- **派生名**: 制約の既定名（`<table>_pkey`・`_key`・`_fkey`・`_check`）・暗黙 SEQUENCE（SERIAL・IDENTITY）・
  一意索引の実体の UNIQUE 制約名は、表の**名前部**から導出する（内部 ID を含めない）。暗黙 SEQUENCE と
  索引は表と同じ SCHEMA に置く。`ALTER ... RENAME TO` は SCHEMA を変えない（`catalog::namespace::sibling_key`）。
  行ストア・世代・台帳・分割実行ジョブ・rowid 採番状態・キャッシュは、カタログ水準のキー（内部キー）で
  引くため public の同名と混ざらない（`tests/table27_schema_objects.rs` で固定）
- **SEQUENCE**: 定義キーは `("#<id>", name)`（`sequence_key` は部分文字列を借用するだけ。`public` のバイト列は
  不変）。表を走査して名前を取り出す箇所は `sequence_entry_name` を通す
- **ビュー本文（D5）**: 作成時に解決した内部キーで保存する（早期束縛）。SCHEMA の削除・再作成で内部 ID は
  再利用されないため、古い本文は `42P01` になる。格納形式の版は上げない。内部キーを含む本文を持つ DB を
  旧バイナリで開くと decode の検証で `CorruptSchema`（fail-closed）になる
- **D7（`42501`）**: public と SCHEMA 配下の間をまたぐ依存は両方向とも `SchemaDependencyForbidden`
  （固定文言）で拒否する（`sql::schema_dependency`。DDL 権限ゲートの直後・カタログ書き込みの前）。
  対象は表の FK の参照先・`DEFAULT nextval`（`ALTER` の追加 FK・`SET DEFAULT` を含む）とビュー・
  materialized view の本文の参照先。ビューの SCHEMA 配下 → public は、DROP の依存判定と同じ導出
  （`catalog::view_body_relations`）で調べる。同一 SCHEMA 内は許可。SCHEMA 配下 → public を拒否する理由は、
  public 側の `DROP` の依存判定が全テナントを走査し、他テナントの非公開の依存の有無が成否に表れるため。
  `OWNED BY` は構文上テナント表を指せない（3 部名）ため対象外
- **UDF**: セッション単位の登録（永続化しない）は変えず、登録キー・呼び出しキーを内部キーにした。
  SCHEMA の削除後は解決できず、作り直した SCHEMA は別の内部 ID のため古い登録に当たらない。
  `public.f`・組み込み SCHEMA の修飾は解決しない（組み込み関数へ化けない）
- **`DROP SCHEMA ... CASCADE`（D8）**: 単一の write txn・1 回の commit。(1) SCHEMA 配下の表・ビューを根に、
  既存の依存列挙（`catalog::dependency`）で外部の依存（別 SCHEMA のビュー・FK）を求めて先に削除 →
  (2) 配下のビュー（キー昇順）→ (3) 配下の表（既存の `DROP TABLE ... CASCADE` 本体。索引宣言・行ストア・
  世代・台帳・所有 SEQUENCE を継承）→ (4) 残りの SEQUENCE（キー昇順）→ (5) SCHEMA 行。依存を正確に
  判定できない場合、または外部の表が配下の SEQUENCE を `DEFAULT nextval` で参照している場合は
  `2BP01`（DEFAULT は連鎖削除しない）。RESTRICT は従来どおり `2BP01`。ID カウンタは巻き戻さない
- **表示変換（D2）**: ctx を持つ入口の出口で、エラー文言（`SqlSurfaceError::map_text`）と EXPLAIN の
  計画テキストに現れる内部キーを、そのテナントの SCHEMA 名で `<schema>.<name>` へ戻す
  （`relation_name::display_relation_keys`。逆引きできない ID は `#<id>.` を落とす）。修飾列参照の
  公開名は名前部（`SELECT t.id FROM s.t` を受理）。成功応答の構造体（`AlterTableOutcome` 等）は
  wire へ出ないため変換しない
- **一覧 API**: `Storage::list_tables`・`list_indexes` は public の名前だけを返す（内部キーを返さない）
- **拡張クエリ**: Parse は ctx を受け取らないため、接続の認証済みテナントを
  `SessionState::bind_resolution_tenant` で認証成功後に 1 回だけ束縛し（wire-server の handshake。
  SQL 文からは変更できない）、Parse の解決に使う。未束縛の Parse は従来どおり `3F000`。Bind は
  Parse 時に解決済みのトークン列から再 parse する。束縛済みセッションを別テナントの ctx で使う実行入口
  （`execute_*`・`begin_copy*`）は `XX000` で拒否する（実行時の再検証）。Bind・Describe・Parse の
  エラー文言も、Parse に使ったテナントの SCHEMA 名へ戻す
- **破壊的変更**: `SqlSurfaceError::SchemaDependencyForbidden` の追加（外部の網羅的 `match` は追随が必要）、
  `Storage::list_tables`／`list_indexes` が SCHEMA 配下を返さなくなる、`ValidatedDropSchema` が `CASCADE` を保持
- **対象外**: MATERIALIZED VIEW・ENUM 型の SCHEMA 配置（`0A000`／`42601` のまま。ADR D5 は ENUM 型の
  修飾を #1717 としていたが、型名の解決位置〔列型・キャスト〕が加わり範囲が倍増するため見送った）、
  NoSQL（HTTP）表層・Rust API の修飾名、`search_path`（#1715）、`SET SCHEMA`・SCHEMA の改名（#1716）、
  `ALTER VIEW`（未実装）、UDF の永続化
- **既知の制限**: 単独の `SELECT nextval('s.q')` は public の `SELECT nextval('q')` と同じく未対応だが、
  SQLSTATE は `42703`（public は `0A000`）になる（どちらも拒否。内部キーは文言に出ない）。
  解決（Parse）から書き込みまでの間に別セッションが同じ SCHEMA を削除する競合は、表・ビュー・索引・
  SEQUENCE の作成 txn 内で `ensure_key_schema_alive_in_txn` が SCHEMA の存在を確認して拒否する
  （削除済みの内部 ID へ作成できない）。UDF はセッション単位の登録で永続化しないため対象外。

## 9. Issue #1716 実装記録

親 #1711・TABLE-27 のポインタ。`ALTER SCHEMA ... RENAME TO` と `ALTER TABLE ... SET SCHEMA` を追加した。

- **ALTER SCHEMA RENAME TO**: `schemas` 表の 1 行を付け替えるだけで、内部 ID は不変（D2）。中の
  オブジェクト（表・索引・SEQUENCE・VIEW 等）は内部 ID のキーで格納されているため追随する
  （単体テストで内部 ID の保持と中身を持つ SCHEMA の DROP 拒否を確認済み）。判定順は、旧名が
  組み込み名（`42501`）→ 旧名の不在・他テナントの同名（`3F000`、同一応答）→ 新名が組み込み名・
  既存・同名（`42P06`）→ 新名が予約名（`42939`）。`OWNER TO` は `0A000`（D10）。
  1 回の write txn・1 回の commit で行う
- **ALTER TABLE SET SCHEMA**: 判定順は、DDL 権限ゲート → 表の存在（`42P01`／`42809`）→ 移動先の
  解決（`public` は ASCII の大小を無視。`pg_temp`・`pg_catalog`・`information_schema` は `0A000`。
  それ以外は `(tenant, name)` の 1 回 get で、不在と他テナントの SCHEMA は同一の `3F000`）→
  テナント境界（`public` の表をテナントの SCHEMA へ移す経路は `42501`。D7）→ `public` から
  `public` は何も書かない成功。複数アクションとの併用は `42601`。一時表は従来どおり `0A000`
- **スコープの制約（#1717 の取り込み後の確認事項）**: テナント SCHEMA に表を置く経路（`#<id>.` キーの書き込みと
  decode 側の受理）が未実装のため、移動元は常に `public` の表になる。次は #1717 の取り込み後に
  実装・検証する: テナント SCHEMA 間の実移動（表・索引・制約・`OWNED BY` SEQUENCE のキー付け替え、
  移動先の名前衝突 `42P07`、ビューは移さない）、テナント SCHEMA から `public` への移動（`42501`）、
  実移動後の索引と行の保持の検証
- **既知の制限**: `ALTER TABLE t SET SCHEMA <一時表と同名の名前>` は一時表のトークン走査に当たり
  `0A000` になる（拒否側に倒れるだけで境界は緩まない）
- **対象外**: `ALTER SEQUENCE／VIEW／INDEX／MATERIALIZED VIEW／TYPE ... SET SCHEMA`（#1717。
  `ALTER SEQUENCE ... SET SCHEMA` は従来の `0A000` を維持）、`ALTER TABLE IF EXISTS`（TABLE-36）、
  `ALTER SCHEMA ... OWNER TO`（SQL-39）

## 10. Issue #1715 実装記録

TABLE-27 (3) のポインタ。セッション変数 `search_path` を engine に入れた（本文は ADR D5・D6・D11 に従う）。

- **値と構文**: 既定は `"$user", public`。`SET [SESSION] search_path {TO|=} <要素>[, ..]`・`= DEFAULT`・
  `RESET search_path`・`SHOW search_path` を受理する（`SET LOCAL` は TimeZone と同じ `0A000`。他の `RESET`・
  変数は従来どおり `42601`）。要素は識別子・`"$user"`・文字列リテラル（中身が識別子形か `$user` のとき
  だけ）。要素数 64・要素長 63 バイトの上限を確保前に検査し、超過・不正な文字列は固定文言の `22023`。
  `public`・`pg_catalog`・`information_schema`・`pg_temp` は ASCII 大小無視で照合して小文字へ正規化する。
  `pg_temp` を `public` より後ろに置く指定は `0A000`（一時表は常に先頭で探すため、黙って無視しない）。
  SCHEMA の存在は SET 時に検査せず、他テナントの名前・存在しない名前も保持する
- **`"$user"` の字句化**: 引用識別子の中身が `$` を含むと従来は拒否される。ちょうど `"$user"` だけを専用
  トークン（`Token::QuotedDollarUser`）にし、受理するのは `SET search_path` の値の位置だけ（他は `42601`）
- **`$user` の展開**: handshake が認証成功後に確定した username を `SessionState::set_session_user` で 1 回
  だけ設定する。SQL から変える経路は持たない（D6）。未設定のセッション（Rust API・NoSQL 表層）は飛ばす
- **セッション状態**: `SessionState` に `search_path`・単調な世代（`search_path_epoch`）・`session_user` を
  追加した。ROLLBACK／abort は `SessionState` ごと巻き戻るため `SET search_path` も取り消される（D5）。
  `TxnState::Failed` の巻き戻し用セッションは `Box` にした（型サイズ差の clippy 対応）
- **未修飾名の解決（参照ゲート）**: テナント SCHEMA の中にはまだオブジェクトを作れない（#1717）ため、
  解決結果は「実効 path に `public` が含まれるか」だけで決まる。含まれる（既定の）セッションは追加コストなしで
  従来と同一。含まれないセッションは、テーブル参照位置（#1714 と共有する `relation_positions`）の未修飾名が
  一時表以外なら `42P01`（文言は利用者が書いた名前だけ。カタログを引かないので存在オラクルにならない）。
  CTE 名・構文語・表関数・`public.` 修飾・定数式は対象外。`DROP`／`ALTER`／`REFRESH` の
  VIEW・MATERIALIZED VIEW・SEQUENCE・INDEX・TYPE の対象名も参照として扱う（共有 `public` の取り違えを防ぐ）。
  判定順は 構文 `42601` → DDL 権限の無い DDL 先頭は飛ばす（`42501` を維持）→ 参照ゲート → その他の検証
- **作成先**: 永続オブジェクトを作る各実行腕（`CREATE TABLE`・`VIEW`・`MATERIALIZED VIEW`・`SEQUENCE`・`TYPE`）が
  DDL 権限ゲートの直後・既存名の判定より前に path の先頭から作成先を判定する。`public` は従来どおり、
  自テナントの既存 SCHEMA・`pg_catalog`・`information_schema`・`pg_temp` は `0A000`、どれも無ければ
  `3F000`（固定文言 `no schema has been selected to create in`）。`public.` で明示修飾した `CREATE TABLE`
  （`ValidatedCreateTable.public_qualified`）・一時表・UDF は判定しない。既定の path のまま自分の username と
  同名の SCHEMA を作ったテナントは、未修飾の `CREATE TABLE` が `0A000` になる（PG と同じく `$user` が優先。
  #1717 まで）
- **拡張クエリ**: Parse・Bind・Describe・`COPY` も同じゲートを通す。Parse 後に `SET search_path` された文は
  Bind（wire は path が隠れているセッションで cached の `$n` なし文も `bind_prepared_in_session` で
  検証し直す）で `42P01` になる。Bind 後・Execute 前に path が変わった文と、`parse_sql` で作った
  未検証の文は、`public` が隠れている間 `0A000`（`SessionRoutedSql` の path 世代。セッション変数・
  トランザクション制御・一時表の作成・定数式・カーソルの `FETCH`／`CLOSE` は免除）
- **暗黙 RLS**: path に関係なく常に掛かる（D3）。`SET search_path` は境界を越える経路にならない
  （`rls_implicit.rs` に D11 のテストを追加。セッションの無い入口の `SET search_path` は従来どおり `42601`）
- **意図的な PostgreSQL との差**: 長すぎる要素名は切り詰めず `22023`／文字列リテラル要素は識別子形か
  `$user` だけ（カンマ分割しない）／`DROP`・`ALTER` の `IF EXISTS` も `public` が隠れていれば no-op に
  せず `42P01`／`pg_temp` を `public` より後ろに置くと `0A000`／`quote_identifier` の予約語判定は lexer の
  予約語に限る
- **公開 API の破壊的変更**: `Statement::SetSearchPath`・`ShowSearchPath`、`SqlOutcome::SetSearchPath`、
  `Token::QuotedDollarUser`、`SqlSurfaceError::NoSchemaSelected`、`ValidatedCreateTable.public_qualified`
- **対象外**: テナント SCHEMA 内のオブジェクトの解決と作成・要素ごとの読み取り probe（#1717）、
  `nextval('s')` など文字列引数のオブジェクト名・UDF・ENUM 型の参照解決・組み込み関数の `pg_catalog` の
  優先順（#1717）、`pg_temp.x` の結線、`current_schema()`（#1718）、StartupMessage の `search_path`・
  `ParameterStatus` 通知、`set_config('search_path', ..)`・`ALTER ROLE .. SET search_path`、NoSQL 表層
  （ステートレスで既定の path）

## 11. Issue #1718 実装記録

`current_schema()`・`current_schemas()`（TABLE-27 (7)）を psql／ドライバ互換の応答として返す。
システムカタログ（`pg_namespace` 等）は D9 のとおり対象外のまま。

- **受理範囲**: FROM なし SELECT（`ConstSelect`）の式の中だけ。`current_schema()`・裸の `current_schema`
  （ASCII 大小無視。Rails 形。text 型のため投影・`WHERE`・`CASE`・`COALESCE` 等の式位置で使える）・
  `current_schemas(true|false|NULL)`（配列型のため、既存の配列値の規約どおり投影と `CAST` の被演算子だけ。
  `CASE`／`COALESCE` 等へ流すと `0A000`）。
  それ以外の文脈（`FROM` あり SELECT・`INSERT VALUES`・`DEFAULT`・CHECK・索引・ビュー本文・UDF 定義）は
  束縛のスコープが無いため `0A000`（固定文言）で fail-closed。引数の arity 違反と非 bool は `42883`
- **解決規則**: path の各要素を展開（`$user` は handshake の username だけから。未設定なら飛ばす）し、
  実在する要素だけを重複除去して並べる。`public`・`pg_catalog`・`information_schema` は常に実在、
  `pg_temp` はセッションに一時表があるときだけ実在、それ以外は `(呼び出し元テナント, 名前)` の
  1 回の get で判定する（他テナントの SCHEMA は不在と同じ扱い・同じ応答。D4）。`current_schema()` は
  先頭（無ければ NULL）、`current_schemas(false)` は全体、`(true)` は暗黙の一時スキーマ・`pg_catalog` を
  PostgreSQL と同じ順序で、明示リストに無いものだけ前置する（明示した `pg_temp` は位置を保持する）
- **実装**: `sql::search_path` に path 要素の展開の共通化（`creation_target` と共有。挙動は不変）・
  `resolve_schema_functions`・thread-local RAII の `SchemaFnScope` を置く。スコープを張るのは
  `core.rs` の `ConstSelect` の実行腕（式木が `current_schema` 系を含むときだけ probe。含まない文は
  照会コストゼロ）と Describe 腕（型だけ公告。`PolicyContext` を持たないため probe しない）。
  `udf_call::bind_call` が `now()` と同じ位置で定数へ畳む。`BuiltinFn`・`ValueFunctionKind` の variant は増やさない
- **出力**: 結果列の型は `text`／`text[]`。内部キー・内部 ID（`#<id>.`）は応答・エラーに出さない（D2）
- **意図的な PostgreSQL との差**: 型は `name`／`name[]`（OID 19／1003）ではなく `text`／`text[]`（WIRE-13 のポインタ）／
  一時スキーマの表示名は `pg_temp_N` ではなく固定の `pg_temp`（backend 番号を出さない）／一時スキーマの存在は
  「一時表が 1 つ以上あるか」で近似（PostgreSQL は全一時表を DROP しても残る）／引用した `"current_schema"` も
  値を返す（lexer が引用の有無を区別しない。PostgreSQL では `42703`）／`FROM` ありの文での使用は `0A000`
- **互換性の変更**: UDF 名 `current_schema`・`current_schemas` を予約名にした（UDF はセッション単位で
  永続化しないため保存済みデータへの影響なし）
- **対象外**: システムカタログと psql の `\dn`・`FROM` あり／INSERT／DEFAULT／CHECK／索引／ビュー本文での
  `current_schema` 系・修飾つき `pg_catalog.current_schema()`・`current_setting('search_path')`・
  StartupMessage の `search_path` と `ParameterStatus`・`name` 型の導入・`pg_temp.x` の結線

## 12. Issue #1719 実装記録

SCHEMA 導入（#1713〜#1717）後もテナント境界が保たれることの横断回帰テスト。ポインタ: TABLE-27（関連: RLS-7・RLS-10・ERR-6）。**テストのみの追加**で、engine 本体・公開 API・依存は不変。

- **新設**: `crates/engine/tests/table27_tenant_boundary.rs`。各 `table27_*.rs` の機能別テストとは重複させず、まだ無い軸を足す（3 テナントの回転、可視性 2 モード、テナントごとの別セッション、独立オラクル、複数の入口、試行総数の固定、負の対照）
- **存在の秘匿（D4）**: 他テナントの SCHEMA（表・ビュー・索引・SEQUENCE・名前付き制約・UDF）がある DB と、どこにも無い DB の「双子」で、閲覧側が同じ文を実行し、SQLSTATE と文言が**完全一致**することを確かめる。入口は autocommit・明示トランザクション（失敗後の文と `ROLLBACK` を含む）・拡張クエリ（Parse／Describe／Bind／Execute のどの段で失敗したかも含む）・`COPY`。加えて応答に `#`・他テナントだけが知る名前・閲覧側が書いていない数字が出ないことを検査する。`CREATE SCHEMA` は他テナントの同名があっても成功し、互いに独立する
- **search_path（D5・D11）**: 他テナントの SCHEMA 名を path に置く／username と同名にする（`$user`）／`ROLLBACK` で戻す／Parse 後に変更する、のいずれも双子と一致し、閲覧側自身の行だけが見える
- **暗黙 RLS（D3）**: 3 テナント × 可視性 2 モード × 参照形（未修飾・`public.`・`s.`・ビュー・JOIN・IN・UNION・EXISTS・COUNT・索引経由）の結果 id 集合を、`PolicyContext` の判定を呼ばないテスト側の真実値で照合する。書き込み（UPDATE・DELETE・UPSERT・TRUNCATE）は autocommit と明示トランザクションの両方で、他テナントの事後状態が不変であることを確かめる。Public 行は SQL 表が採番方式で Rust API から書けないため、Rust API で作った従来方式の共有表を使って別に検査する
- **内部 ID の接頭辞**: 内部 ID `#1.` と `#10.` 以上の物理表が両方ある並びで、DROP CASCADE・RENAME・TRUNCATE・SEQUENCE の操作が別テナントの SCHEMA に及ばない。物理表名の存在は raw redb の表名で前提確認する
- **名前の衝突**: 3 テナントが同名の SCHEMA に同名のオブジェクトを作れ（`42P07`／`42710` にならない）、どれかの DROP／RENAME が他テナントに影響しない。UDF はセッション単位で保持される
- **試行総数の固定と負の対照**: 各テストの末尾で検査した組の数を `assert_eq!` で固定し、検査器（漏えい検出・オラクル・双子比較）へ捏造した違反を与えて検出されることを確かめる
- **現状の挙動として固定したもの（fail-closed）**: `search_path` に自テナントの SCHEMA だけを置いた未修飾名の解決・作成は未対応（`42P01`／`0A000`）、テナント SCHEMA 間の `SET SCHEMA` は `42501`。受理されるようになったら本テストの期待を更新する
- **未カバー**: 物理表へ他テナントの行を直接差し込む多層防御（D3）の検査。SQL 表は採番方式で、`tenant::insert_row` などの公開 API が拒否するため再現できず、raw redb での行エンコードが要る。処理時間の差による存在の推測（D4 の時間的な側面）は決定的に検査できない
- **対象外**: wire-server の応答（#1718）、SCHEMA 単位の GRANT（#2234）、利用者定義の RLS ポリシー、crash テストの新設
