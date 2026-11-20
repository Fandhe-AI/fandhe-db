# ADR: SCHEMA とテナントの関係・名前解決規則（Issue #1712）

- ステータス: Accepted（設計記録。後続 #1713〜#1719・#2234・#3195〜#3199 で実装済み。現状は末尾の「追記（Issue #3199）」を参照）
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
  トークンは `42601`。明示トランザクション内の要素なしの `CREATE SCHEMA`・`DROP SCHEMA`（`CASCADE` なし）・`ALTER SCHEMA` は Issue #3315 で受理に変わった（要素つき・`CASCADE`・SCHEMA 配下は `0A000`。受け皿 #3311）
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
- **対象外**: MATERIALIZED VIEW（Issue #3287 で対応済み。末尾の追記を参照）・ENUM 型の SCHEMA 配置（`0A000`／`42601` のまま。ADR D5 は ENUM 型の
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
- **現状の挙動として固定したもの（fail-closed）**: 本節の記述は #1719 時点。未修飾名の解決・作成は #3195〜#3197 で、テナント SCHEMA 間の `SET SCHEMA` は #3198・#3199 で受理に変わった（現状は末尾の「追記（Issue #3199）」）
- **未カバー**: 物理表へ他テナントの行を直接差し込む多層防御（D3）の検査。SQL 表は採番方式で、`tenant::insert_row` などの公開 API が拒否するため再現できず、raw redb での行エンコードが要る。処理時間の差による存在の推測（D4 の時間的な側面）は決定的に検査できない
- **対象外**: wire-server の応答（#1718）、SCHEMA 単位の GRANT（#2234）、利用者定義の RLS ポリシー、crash テストの新設

## 追記（Issue #2236）

SCHEMA の所有者は、上記の blob の予約領域ではなく `object_owners` 表（kind=SCHEMA・内部 ID キー）に保持する。blob の形式は変えていない
（[role-privilege-model.md](role-privilege-model.md) §14 O7）。

## 追記（Issue #3195）

読み取りの未修飾名は `search_path` の要素順に自テナントの SCHEMA まで解決する（D5 の実装。親 #3194）。§12 の「現状の挙動として固定したもの」のうち、未修飾名の読み取りの解決は本追記で置き換わった。
作成・DML・DDL・`COPY`・拡張クエリで SCHEMA に解決される参照は `0A000`（受け皿: #3196・#3197）。テナント SCHEMA 間の `SET SCHEMA` は #3198。本書の全面改訂と横断回帰の総点検は #3199。

## 追記（Issue #3198）

テナント SCHEMA 間の `ALTER TABLE <s1.t> SET SCHEMA <s2>` を実装した（§9 の申し送りの解消。ポインタ: TABLE-27 (7)）。

- **移るもの**: 表（行ストア・制約・FK・TRIGGER・POLICY・ACL・所有者・台帳等。`ALTER TABLE ... RENAME TO` と同じ付け替えを共有する実装 `rename_table_key_in_txn`）、表に属する索引（宣言のキー `#<旧>.<索引>` → `#<新>.<索引>`、GIN の物理表 `user_gin/{索引キー}` を含む）、表が所有する SEQUENCE（定義キーのみ。内部 ID・採番状態・ACL は ID キーなので不変）と、表自身の `DEFAULT nextval`・IDENTITY の参照名。**ビューは移さない**（ビュー本文は表の新しい名前へ追随する）
- **単一 txn・1 回の commit**: 判定をすべて書き込みの前に終え、失敗は txn ごと破棄する。移動元・移動先が呼び出し元テナントの生存 SCHEMA であることを write txn 内で再確認する（不在・他テナントは区別なく `TableNotFound`）
- **判定順**: DDL 権限ゲート → 表の存在（`42P01`／`42809`）→ 移動先の解決（不在・他テナントは同一の `3F000`、システム SCHEMA・`pg_temp` は `0A000`）→ `public` との境界（`42501`）→ 同一 SCHEMA は何も書かない成功 → 移動先の CREATE 権限（`42501`）→ 実移動
- **エラー**: 移動先での名前の衝突（表・ビュー・索引・matview・SEQUENCE。移る索引名・SEQUENCE 名を含む）は `42P07`。関数・matview の依存は `ALTER TABLE ... RENAME TO` と同じ `2BP01`。`OWNED BY` の SEQUENCE を他の表が `DEFAULT` で参照しているときも `2BP01`
- **意図的な拒否（`intentional`）**: `public` とテナント SCHEMA の間の移動は両方向とも `42501` のまま。共有の `public` の表は全テナントの行を持つため、移動で他テナントの行の所有が移りうる（D7・TABLE-27 (7) のポインタ）。システム SCHEMA・一時表への移動の `0A000` も従来どおり
- **残した `0A000`**: `ALTER SEQUENCE／VIEW／INDEX ... SET SCHEMA` は未対応のまま。未修飾の移動元は #3199 で受理に変えた（末尾の追記）
- **診断 API**: `verify_gin_index_read_only` は SCHEMA 配下の索引キー（`#<id>.<名>`）も引けるようにした（検証用。読み取り専用）

## 追記（Issue #3196）

DML・DDL・`COPY` の未修飾名と、作成系 DDL の作成先も `search_path` で決める（D5 の実装。親 #3194。読み取りは #3195）。

- **方式**: 未修飾名は、解析の前に修飾名と同じ内部キー（`#<schema_id>.<name>`）へ置き換える。置換後のトークン列は利用者が `q1.t` と書いた文と同じなので、ACL・一時表の振り分け・ビュー本文の保存・実行は修飾形と同じ経路に乗り、受理・拒否も修飾形と一致する（public のビューから SCHEMA の表を参照する形の `42501` など）。文種ごとの個別の実行経路は作らない
- **作成先**: path の先頭の要素。自テナントの SCHEMA なら内部キー、`public` なら裸のまま。`pg_catalog`・`information_schema`・`pg_temp` は `0A000`、どの要素にも作れなければ `3F000`。USAGE の無い自テナントの SCHEMA は飛ばさず `42501`（K4）。作成先の SCHEMA には CREATE 権限を検査する（K5）。権限ゲート（`42501`）→ 作成先（`3F000`／`0A000`／`42501`）→ 既存名（`42P07`）の順は変えない
- **安全弁**（`CREATE MATERIALIZED VIEW` は Issue #3287 で解析時の書き換えに乗った）: 解析時の書き換えを通らない経路（`CREATE TYPE`・`CREATE MATERIALIZED VIEW`・構文語と衝突する名前・解析と実行の間の SCHEMA 作成の競合）は、path 先頭が自テナントの SCHEMA のとき固定文言の `0A000` にし、`public` へ黙って作らない
- **自己参照 FK**: `REFERENCES <作成名>` は作成先に合わせる（作成先が SCHEMA なら同じ内部キー、`public` なら裸のまま）。書き換え自体は単体テストで固定している（行 ID モードの親への `REFERENCES parent (id)` は #3385 で受理に変わった）
- **明示トランザクション**: 要素なしの SCHEMA の DDL は Issue #3315 で受理（SCHEMA 配下の DDL は従来どおり `0A000`）。DML の存在照会は確定済みのスナップショットを見る（`COPY` は `BEGIN` 時点）
- **残る `0A000`**（MATERIALIZED VIEW の SCHEMA 配置は Issue #3287 で対応済み）: `ALTER TABLE .. SET SCHEMA` の未修飾の対象が SCHEMA に解決される形（#3198）、`ENUM` 型・`MATERIALIZED VIEW` の SCHEMA 配置と組み込み SCHEMA への作成（本 ADR の対象外。専用の Issue が必要）
- **意図的な差**: 重複作成の `42P07` の文言は、PostgreSQL の裸の名前ではなく `q1.t` 形（修飾形と同じ）で出す。内部 ID は出ない

## 追記（Issue #3197）

拡張クエリ（Parse・Bind・Describe）も未修飾名を `search_path` で解決し、`SET search_path` の後の Bind・Describe に反映する（親 #3194。#3195・#3196 が固定文言の `0A000` で拒否していた形の再解決）。

- **Parse**: セッション対応の Parse は、`schema.name` 修飾の解決の直後（`$n` の位置検査・型推論・構造検証より前）に、簡易クエリと同じ `resolve_statement_refs` で未修飾名を内部キーへ置き換える。判定順は 構文 `42601` → 解決エラー → 参照ゲート `42P01`（書き換え後のトークンに対して）→ 解析エラー → `SET SCHEMA` の `0A000` → 権限 `42501`。`public.` で明示修飾した作成名は作成先を path で決めない
- **刻印**: 準備済み文に、元の SQL・Parse 時の path 世代・path 適用前のトークン列・適用した書き換え（解決の指紋）を持たせる。保持量は SQL とトークン列 1 組分で、wire 層の文あたり・セッションあたりの上限が抑える
- **Bind・Describe**: 先頭で再解決する（`refresh_prepared_in_txn`。engine の Bind・Describe 系 API も内部で同じ処理を通す）。path 適用前のトークン列を現在の path・カタログ（明示 Tx は `BEGIN` 時点）で解決し直し、指紋と同じなら使い回す。違えば元の SQL を再 Parse する。世代が同じでもカタログが変わると解決先が変わりうるため、path 解決が有効なセッションは毎回この照会を行う（`$user` の SCHEMA 1 件の get と、SCHEMA が在れば名前ごとの存在照会）
- **再 Parse の互換検査**: `$n` の型・結果列が変わる場合、または一時表の経路へ変わる場合は固定文言の `0A000`（PostgreSQL の cached plan must not change result type と同じ。公告済みの `ParameterDescription`・`RowDescription` と食い違う文を実行しない。名前・SCHEMA を含めない）
- **portal**: Bind 時の解決のまま実行する（`public` が見える path では PostgreSQL と同じ。`public` が隠れる path は従来どおり世代の食い違いで `0A000`）。portal が保持する準備済み文も再解決後の文にし、Execute の権限・同一性の再検査が実行される本文と同じ解決を見る。`COPY` は Execute で元の SQL を現在の path へ再解決するため、path 解決が有効なセッションでは Bind で世代を刻み、食い違えば `0A000`
- **互換の安全弁**: セッションを持たない Parse（Rust API の `parse_sql_prepared_with_declared`）の文は path を知らないため、解決が有効なセッションの Bind・Describe では従来どおり `0A000`
- **テナント境界**: 再解決の照会は既存の `with_search_path_lookup`（自テナントの USAGE の有る SCHEMA だけ）のみ。他テナントの SCHEMA を引く経路は無く、エラー文言は利用者が書いた名前だけで内部キーを含まない
- **残る未対応**: 再 Parse した文を文のストアへ書き戻す最適化はしない（path・カタログが変わった後の Bind ごとに再 Parse する）

## 追記（Issue #3199）

親 #3194 の締めとして、`search_path` の固定テストを新挙動へ更新し、越境試験を足した（ポインタ: TABLE-27）。

- **未修飾の `ALTER TABLE t SET SCHEMA <s>` を受理に変えた**: 未修飾名は解析前に修飾名と同じ内部キーへ書き換わるため、以降は修飾形（`q1.t SET SCHEMA ..`）と同じ経路に乗る。`PathGate` にあった「対象が SCHEMA に解決された SET SCHEMA は `0A000`」の特例は、修飾形の `42501` に先回りするためだけに残っていたので撤去した。判定順・エラーは #3198 のまま（DDL 権限 → 存在 → 移動先の解決（不在・他テナントは同一の `3F000`）→ `public` 境界の `42501` → CREATE 権限 → 実移動）。移動先の識別子が同名の表に引きずられて書き換わらないことは、結合テスト（`table27_search_path.rs`）で固定した
- **意図的な拒否（`intentional`）**: `public` とテナント SCHEMA の間の移動の `42501`、他テナントの SCHEMA への移動（不在と同じ `3F000`）、システム SCHEMA・一時表への移動の `0A000`、`ALTER SEQUENCE／VIEW／INDEX ... SET SCHEMA` の `0A000`、明示トランザクション内の SCHEMA 配下の DDL・要素つき `CREATE SCHEMA`・`DROP SCHEMA ... CASCADE` の `0A000`、SCHEMA 配置が未対応の作成（`CREATE TYPE`・`CREATE MATERIALIZED VIEW`）の `0A000`（`CREATE TYPE` は Issue #3288、`CREATE MATERIALIZED VIEW` は Issue #3287 で対応済み。「追記」参照）は、いずれも fail-closed の安全弁として残す
- **越境試験（`table27_tenant_boundary.rs` の T9）**: 他テナントだけが持つ SCHEMA 名を `search_path` に置いた閲覧側の未修飾の書き込み・DDL・移動・削除の応答が、その SCHEMA がどこにも無い DB と完全一致する（autocommit・明示トランザクション・可視性 2 モード）。2 テナントが同名の SCHEMA・同名の表・同じ id を持つとき、一方の未修飾の INSERT／UPDATE／UPSERT／DELETE／`CREATE INDEX`／`ADD COLUMN`／TRUNCATE／`SET SCHEMA`／DROP が他方の行・構造を変えない（試行総数の固定と負の対照つき）
- **wire の結合試験（`wire_search_path.rs`）**: psql 形（末尾 `;` 付き・1 メッセージ 1 文）で、同名 SCHEMA を持つ 2 接続の未修飾の書き込み・`SET SCHEMA`・TRUNCATE・DROP の分離と、他テナントの SCHEMA 名を path に置いた接続の拒否（`#` を含む文言なし）、拒否後も接続が使えることを固定した
- **全面改訂の扱い**: §12 の「現状の挙動として固定したもの」と #3196 追記の自己参照 FK の記述は、陳腐化した箇所に現状へのポインタを足した（履歴として本文は残す）。D5・D11 の契約自体は変わらない

## 追記（Issue #3286）

`CREATE SCHEMA <名> [AUTHORIZATION <ロール>] <要素>...` の要素（`CREATE TABLE`・`CREATE VIEW`・`CREATE SEQUENCE`）を受理する（親 #3285・ポインタ: TABLE-27・TABLE-44・SQL-39・SQL-40・RLS-7・RLS-8・RLS-10）。§6 と D10 が固定していた「schema_element は `0A000`」は、本節の形に限り置き換える（それ以外の要素は引き続き `0A000`）。既存の節は書き換えず、本節で追記する。

- **D1 受理する形**: 要素は `CREATE TABLE`（永続表。SERIAL の暗黙 SEQUENCE、同じ文の表への FK を含む）・`CREATE VIEW`（列別名・`WITH CHECK OPTION`・`OR REPLACE` なしの単純形）・`CREATE SEQUENCE`。名前を省略する `CREATE SCHEMA AUTHORIZATION <ロール> <要素>` も同じ規則で受理する。要素の作成名は未修飾、または作成中の SCHEMA と同じ名前での修飾に限る
- **D2 `0A000` で拒否する形（fail-closed）**: `CREATE INDEX`・`GRANT`・`CREATE TRIGGER`・その他の要素（FUNCTION・TYPE・MATERIALIZED VIEW・CTAS・TEMP／UNLOGGED TABLE・POLICY 等）、VIEW 要素の列別名リスト・`WITH CHECK OPTION`・`OR REPLACE`、要素側の `IF NOT EXISTS`、一時表の名前を参照する要素（PostgreSQL では pg_temp が暗黙に先頭のため、永続オブジェクトが一時表へ依存する形になる）、`AUTHORIZATION` で現在ロール以外を所有者に指定した要素つき作成（要素の所有者決定は昇格経路になり得るため。所有者が現在ロール、または未束縛セッションで `None` なら受理する）。INDEX・GRANT の write txn 内本体は、式の束縛・一意索引の展開と対象種別ごとの分岐が長く、begin・commit を外す切り出しが大きいため本 Issue の対象外とした
- **D3 PostgreSQL 17 との照合**（実装時点の PostgreSQL 17 の公開挙動の知識に基づく。実機・ソースでの再照合は未実施）: (a) `CREATE SCHEMA IF NOT EXISTS` と要素の併用は `0A000`（"cannot include schema elements"）。(b) 作成名が別 SCHEMA で修飾されていれば `42P15`（"CREATE specifies a schema (X) different from the one being created (S)"。文言には利用者が書いた名前だけを入れる）。同名 SCHEMA での修飾は受理する。(c) 要素は種別ごとに SEQUENCE → TABLE → VIEW の順に実行する（同種の中は記述順）。(d) 要素内の未修飾の参照は新しい SCHEMA を path の先頭に置いた扱いで解決する（同じ文で先に作られる名前は新 SCHEMA、それ以外はセッションの `search_path`）。SCHEMA 名の比較は SCHEMA の照会と同じく大文字小文字を区別する
- **D4 要素の分割（構文段・カタログ非依存）**: 括弧の深さ 0 の `CREATE` で、直後が要素を導く語（TABLE・VIEW・SEQUENCE・INDEX・UNIQUE・OR・TEMP・TEMPORARY・GLOBAL・LOCAL・UNLOGGED・MATERIALIZED・TRIGGER・FUNCTION・PROCEDURE・TYPE・POLICY・DOMAIN）のものを境界にする。深さ 0 の `GRANT` で直後が権限語のものも境界にする。列名 `"create"`・表名 `CREATE TABLE "create" (...)` は直後が導入語でないため境界にならない。字句段では引用付き識別子を区別できないため、病的な入力で誤って分割しても、各要素は個別の検証と実行段の権限ゲートを通り、`42601`／`0A000` で終わる（権限・テナント境界を越える経路にならない）
- **D5 上限**: 要素数の上限は 64。分割中に数え、`Vec` へ積む前に検査する。超過は `54000`（固定文言）
- **D6 判定順**: 構文段（`42601` → `0A000`／`42P15` → `54000`。カタログ非依存）→ 実行段で要素ごとの `require_ddl`（カタログを引く前に `42501`。単独の DDL と同じゲート）→ 所有者の解決と `AUTHORIZATION` の検査 → 要素の名前解決（`3F000`・`42501`・`42P01`・`0A000`）→ write txn で SCHEMA 作成（`42P06`・`42939`・`54000`）→ 要素の作成
- **D7 原子性**: SCHEMA の作成と全要素の作成を 1 つの write txn・1 回の `commit_boundary::commit` で行う。`catalog::namespace::create_schema_in_txn`・`Storage::prepare_create_view`／`create_view_in_txn`・`catalog::sequence::create_sequence_with_default_acl_in_txn`・`sql::ddl::create_*_element_in_txn` は、単独の DDL と共有する txn 内本体（挙動・保存形式は不変）で、第 2 の作成経路は作らない。どこかで `Err` になれば commit せず txn を破棄するため、SCHEMA も作成済みの要素も残らない。クラッシュ時は redb の単一 commit が「全部ある」か「全部無い」かのどちらかだけを見せる。作成した表の世代 bump は commit の直前に行い、commit 記録（`note_ddl`）は要素ごとに単独の DDL と同じに残す。台帳（`USING OPERATION_ID`）は要素つきの形でも消費せず、付いていれば `42601`（`CREATE SCHEMA`・`TABLE`・`VIEW`・`SEQUENCE` は単独でも台帳を取らない）
- **D8 汎用パスの除外**: 要素つき `CREATE SCHEMA` は、`relation_name::rewrite_relation_names_full`（`nextval('s.q')` の文字列の解決を含む）・`search_path` の参照走査と解決・読み取り先の参照名集めのどれでも文全体を走査しない（走査すると文全体が 1 つの `CREATE TABLE` に見え、作成中でまだ存在しない SCHEMA への修飾が `3F000` になる）。要素の解決は `core::schema_elements` だけが行う。作成中の SCHEMA の内部 ID は write txn で払い出されるため、write txn の外では「新 SCHEMA を指す位置」だけを集めて裸の名前にしておき、ID が決まった後に `#<id>.<名前>` へ 1 対 1 で置換して、単独の DDL と同じ検証器を再実行する。他の SCHEMA・`public` の修飾は既存の解決器（USAGE の検査つき）、残りの未修飾名は既存の `search_path` の解決器と参照ゲートで解く（`public` が隠れた path での未解決は `42P01`）
- **D9 明示トランザクション内**: 要素つき `CREATE SCHEMA` は `0A000`（writer を取る前に拒否）。何も作らない（要素なしの `CREATE SCHEMA` は Issue #3315 で受理）
- **テナント境界（P0）**: SCHEMA の照会・払い出しは `(呼び出し元テナント, 名前)` だけ。他テナントの同名 SCHEMA・オブジェクトは存在しない名前と同じに扱い、エラー文言は利用者が書いた名前だけを含む（内部 ID・内部キー・他テナントの情報を含めない）。SCHEMA → `public` への依存（FK・VIEW 本文）は、単独の DDL と同じく `sql::schema_dependency` が `42501` で拒否する
- **限界**: (1) SERIAL・IDENTITY が作る暗黙 SEQUENCE の名前（`<表>_<列>_seq`）を、同じ文の後続要素の `nextval('<名>')` の文字列引数から未修飾で参照する形は、暗黙名が write txn の中で決まるため新 SCHEMA へは解決されない（`search_path` の解決に回る。明示的な `CREATE SEQUENCE` の名前は解決される）。(2) 要素内の未修飾名の解決は、同じ文で作る名前を新 SCHEMA、それ以外を確定済みカタログのスナップショットで解くため、解決と write txn の間に他セッションが同名を作る競合は単独の DDL と同じ（commit 前の txn 内の存在検査が最終判定）
- **後続（受け皿 Issue 未起票。起票にはユーザー承認が要る）**: 要素 `CREATE INDEX`・`GRANT`・`CREATE TRIGGER`、VIEW 要素の列別名・`WITH CHECK OPTION`・`OR REPLACE`、要素側の `IF NOT EXISTS`、`AUTHORIZATION` に別ロールを指定した要素つき作成。いずれも `0A000` で fail-closed にしており、既存の拒否固定テストは Issue 番号が確定するまで `intentional:` 注記（本 ADR）を付けている。番号が決まったら `tracked:` 注記へ付け替える
- **テスト**: `crates/engine/src/sql/allowlist/schema_elements/tests.rs`（分割境界・拒否・上限の単体）、`crates/engine/tests/table_rls/table27_schema_elements.rs`（原子性・再オープン・テナント越境・権限・`search_path`・上限・明示 Tx）、`sql40_object_owner.rs`（所有者）、`crates/wire-server/tests/wire_sql/wire_schema_ddl.rs`（簡易・拡張クエリ。末尾 `;` 付き）。拒否を固定していた `table27_schema_ddl.rs` は新挙動へ更新した（要素つきの成功形を外し、未対応の要素を `0A000` の一覧へ足した）。依存の追加・更新なし

## 追記（Issue #3288）

ENUM 型を自テナントの SCHEMA 配下に作成・削除できるようにする（親 #3285・ポインタ: TABLE-27・TABLE-14・SQL-39・SQL-40・RLS-7・RLS-8・RLS-10）。§5 対象外・#3196／#3199 追記が固定していた「`CREATE TYPE`／`DROP TYPE` の SCHEMA 配置は未対応（修飾名は `42601`、path 先頭が SCHEMA の未修飾 `CREATE TYPE` は `0A000`）」は、本節の形に限り置き換える。既存の節は書き換えない。

- **K1 内部キー**: SCHEMA 配下の型は `enum_types` に `#<schema_id>.<name>`（relation と同じ D2 方式）で格納する。public の型は裸の名前のまま（既存 DB のバイト列・格納形式の版は不変）。型は relation と別の名前空間で、同じ SCHEMA に同名の表と型が共存できる
- **K2 解決位置**: 修飾名の解決は `CREATE TYPE <名>`・`DROP TYPE [IF EXISTS] <名>` の型名位置だけ（`sql::relation_name`）。関係位置（`relation_positions`）には足さない（参照ゲート・読み取り権限の参照名集め・RLS の名前集めが型名を relation と誤認しないため）。`public.` は裸の名前、自テナントの SCHEMA は内部キー、不在・他テナントは `3F000`、組み込み SCHEMA・`pg_temp` は `0A000`。列型・キャスト・`ALTER TABLE .. TYPE` の `s.mood` は型名位置に当たらず従来どおり `42601`（受け皿 #3289）
- **K3 `public.` 明示**: `ValidatedCreateType::public_qualified` を足し、`public` が隠れた path でも `CREATE TYPE public.m` は public へ作る（`CREATE SEQUENCE` と同じ）
- **K4 未修飾の作成**: path 先頭が自テナントの SCHEMA なら内部キーへ書き換えて SCHEMA へ作る（#3196 の `CREATE SEQUENCE` と同じ）。`require_creation_target` の `0A000` は、書き換えを通らない経路（構文語と衝突する名前・解析と実行の間の SCHEMA 作成の競合）の安全弁として残す
- **K5 未修飾の `DROP TYPE`**: `search_path::NameKind::Type` を足し、型の名前空間（`catalog::enum_type_exists_in`）で path を引く。これが無いと `SET search_path = s, public; DROP TYPE mood` が関係の名前空間で引かれ、`s.mood` ではなく共有 `public` の `mood` を落とす（PostgreSQL と異なる破壊的な取り違え）。型に一時版は無いため、一時表と同名でも型の参照は一時表で除外しない
- **K6・K7 検証**: カタログ API（`create_enum_type`・`get_enum_type`・`alter_enum_type_add_value`・`drop_enum_type`）は内部キーを受理する（下位 `pub` 関数は組み込み側向けで内部キーを受理する前例に従う）。予約名（組み込み型名・別名）の判定は名前部だけで行うため、`s.int4` も `int4` と同じに拒否される。利用者の生入力（HTTP NoSQL の `enum_type`）は厳格な識別子検証のまま。構文段の名前検証は内部キーを受理する（内部キーを作れるのは解決器だけで、字句解析器は `#` を作らない）
- **K8 `DROP SCHEMA`**: RESTRICT は型が残っていれば従来どおり `2BP01`。CASCADE は表の後・SCHEMA 行の前に配下の型を同じ write txn・1 回の commit で落とす。落とす前に `catalog` を 1 回走査し、配下の型を参照する列（配列要素を含む）が SCHEMA の外に残っていれば何も消さず `2BP01`（fail-closed）。これは多層防御で、現状は SQL からも Storage API からも到達しない（SQL では SCHEMA 配下の型を列に使えず（K2・D7）、`create_table` も列の型名に内部キーを受理しない）。列型の解決（#3289）が入ると到達しうるため、走査自体は単体テストで直接検証している。依存判定の走査は各値の解析を 1 回にする述語版（型数 × 表数にしない）。所有者検査（`OwnerGate`）は配下に型があれば共有カタログの relation と同じ扱い
- **K9 DDL 書き出し**: `export_enums` は SCHEMA 配下の型を `CREATE TYPE <schema>.<name> AS ENUM (...)` で出す（SEQUENCE と同じ引用符なしの修飾形）。出力順は描画名順（共有が先）で内部 ID に依存しない。他テナントの型は存在を示さず無視する。以前は「SQL で戻せない」として失敗させていたが、本 Issue で SQL から作れるようになったため退行になる
- **K10 実行時のエラー**: 解析と実行の間に作成先の SCHEMA が消えた競合は、表・SEQUENCE の作成と同じ `42P01` に写す（名前は内部キーのまま渡し、ctx を持つ入口の出口が公開名へ戻す）
- **判定順**: 修飾名の解決（`3F000`／`0A000`）は解析段で起こり、`CREATE SEQUENCE nosuch.q` と同じ順（DDL 権限の無いセッションでも同じ）。既存の SCHEMA でも権限の無いセッションは `42501`。他テナントの SCHEMA は不在と同じ応答（照会は `(自テナント, SCHEMA 名)` だけ）
- **意図的な差**: `public` が隠れた path で未修飾 `DROP TYPE ty` の対象が無いとき、従来どおり `42P01`（PostgreSQL は `42704`。§10 の参照ゲートと同じ fail-closed）
- **既知の制限**: 型の総数上限（`MAX_ENUM_TYPES`）は表全体の件数で数えるため、全テナントの public 型と SCHEMA 配下の型が共有する。1 テナントが上限を使い切れる点・上限エラーで総数を推測できる点は、既存の共有 public 型と同じ性質で本 Issue で新しく生じるものではない。テナント単位の割り当ては変えない（受け皿 Issue は未起票。起票にはユーザー承認が要る）
- **残る未対応**: SCHEMA 配下の型を列型・キャスト・配列要素型で解決すること（#3289）。`DROP TYPE ... CASCADE`・要素つき `CREATE SCHEMA` の `CREATE TYPE` 要素・`ALTER TYPE ... SET SCHEMA`／`ADD VALUE` の SQL 表層・SCHEMA 配下の表の ENUM 列（D7）は既存の fail-closed のまま（受け皿 Issue 未起票。起票にはユーザー承認が要る）
- **テスト**: `crates/engine/tests/table_rls/table27_enum_schema.rs`（共存・path・CASCADE・RENAME・2 テナント・判定順・未対応の形・上限・拡張クエリ）、`catalog::namespace`・`sql::relation_name`・`sql::search_path`・`catalog::ddl_export` の単体、`crates/wire-server/tests/wire_sql/wire_schema_ddl.rs`（簡易・拡張クエリ。末尾 `;` 付き）。拒否を固定していた `sql_enum_type_ddl.rs`・`table27_schema_objects.rs`・`table27_search_path.rs` は新挙動へ更新した。依存の追加・更新なし

## 追記（Issue #3289）

SCHEMA 配下の ENUM 型を、列型（`CREATE TABLE`・`ALTER TABLE .. ADD COLUMN`・`ALTER COLUMN .. TYPE`・配列要素 `s.mood[]`）とキャスト（`::s.mood`・`CAST(x AS s.mood)`）の型名位置で解決する（親 #3285・前提 #3288。ポインタ: TABLE-27・TABLE-44・SQL-39・SQL-40・RLS-7・RLS-8・RLS-10）。#3288 追記の K2 と「残る未対応」が固定していた「列型・キャストの修飾名は `42601`」は、本節の形に限り置き換える。既存の節は書き換えない。

- **L1 型名位置の走査器**: `sql::type_name_position::type_name_positions` を新設した。トークン列から型名の位置（`::` の直後・`CAST ( .. AS` の直後・`CREATE [TEMP|UNLOGGED] TABLE` の各列定義の 2 語目・`ADD [COLUMN] [IF NOT EXISTS] <列> <型>`・`ALTER [COLUMN] <列> [SET DATA] TYPE <型>`）の添字だけを集め、トークンは書き換えない。関係位置（`relation_positions`）には足さない（K2 と同じ理由。参照ゲート・読み取り権限の参照名集め・RLS の名前集めが型名を relation と取り違えないため）。括弧の段数には上限 256 を設ける。取りこぼしは従来どおり構文段の `42601`（拒否側）、誤検出した位置の修飾名は型名として解決できなければ必ずエラーで返る（別オブジェクトへ化ける経路は無い）
- **L2 修飾名の解決**: `rewrite_relation_names_full` が走査器の位置の `QualifiedIdent` を K2 と同じ `resolve` で解く。`public.` は裸の名前、自テナントの SCHEMA は内部キー、不在・他テナントは `3F000`（同一応答）、組み込み SCHEMA・`pg_temp` は `0A000`、テナント不明の入口は `3F000`。型名位置の修飾名は列修飾の残り（`residual_column_qualifier`）に数えない。`public.` の修飾は一時表との衝突検査用の名前に含めない（型は別の名前空間）
- **L3 未修飾の列型名**: `search_path` の解決（`resolve_statement_refs`）が、列型の位置の未修飾名を `NameKind::Type` で path の順に引き、自テナントの SCHEMA の型に当たれば内部キーへ書き換える（`public` の型・不在は書き換えない）。組み込み型名（`ddl_column_type` の型名解決表の語）と解決済みの内部キーは引かない。型に一時版は無い。キャストの未修飾の型名は引かない（従来どおりカタログ非参照）。SCHEMA が path に無い既定の path ではカタログを引かない。参照ゲート（`check_unqualified_refs`）は、`public` が隠れた path で解決されない列型の未修飾名を `42704`（利用者が書いた名前だけの固定文言）にする。判定順は relation と同じ（構文 `42601` → DDL 権限の無い DDL 先頭は飛ばす → ゲート）。拡張クエリの Parse・Bind・Describe の再解決は同じ関数を通るため、`SET search_path` 後の Bind は新しい path で解き直す
- **L4 永続化**: 列定義の ENUM 型名は解決後の内部キーで持つ（SCHEMA の改名は内部 ID を変えないため追随する。SCHEMA を削除して作り直すと別の ID になり古い参照は解決しない）。カタログ列行の `param` は、`enum` タグの `param` と `array` タグの `enum,<名>,<max_len>[,d<N>]` の型名欄に限り内部キー `#<正準 10 進>.<識別子>` を受理する（`validate_catalog_param_for_tag`・`validate_enum_type_ref`）。他のタグ・欄の文字集合は変えず、区切り文字（`:`・`,`・改行）は引き続き拒否する。public の型の列は裸の名前のままでバイト列は不変（格納形式の版は上げない）
- **L5 D7 の置き換え**: ENUM 型については「SCHEMA 配下の表は一律拒否」をやめ、表と型の所属の比較にした。public の表 → SCHEMA 配下の型・SCHEMA 配下の表 → public の型はどちらも `42501`（固定文言）。SCHEMA 配下どうし（同一テナントの別 SCHEMA を含む）は FK・SEQUENCE と同じ粒度で許可する。判定はトークン由来のキーだけで行い、カタログを引かない（存在オラクルにならない）。CTAS は解決済みの列型で同じ判定を受ける
- **L6 依存判定（K8 が到達可能になる）**: `DROP TYPE s.mood`（RESTRICT）は SCHEMA 配下の表（配列要素を含む）が使っていれば `2BP01`。`DROP SCHEMA s CASCADE` は配下の表 → 型の順に 1 回の commit で落とし、別の SCHEMA の表が `s` の型を使っていれば何も消さず `2BP01`（K8 の走査）。SQL 経由の結合テストで固定した
- **L7 DDL 書き出し**: SCHEMA 配下の型を使う列は `<schema>.<name>`（引用符なしの修飾形。`CREATE TYPE` の書き出しと同じ）で出す。内部キー（`#`）を出さず、他テナントのキー・逆引きできないキーは固定文言で失敗させる。SCHEMA の改名後は改名後の名前で出る
- **L8 キャストの扱い（判断記録）**: ENUM ⇄ 文字列の変換そのもの（SQL-47 の範囲）は本 Issue の対象外。`cast-conversion-rules.md` の「ENUM 型名はカタログを引かずに `42704`」を保ち、`::s.mood`・`CAST(x AS s.mood)`・`::public.mood` は型名の解決だけを public と同じ規則に載せ、解決後は public の型と同じ静的な `42704` になる（文言は公開名。内部キーを出さない）。型の存在を引かないので、キャストが存在オラクルになる経路は作らない。`DEFAULT 'a'::s.mood`・CHECK 内のキャストは public の型と同じ既存の `0A000`
- **L9 意図的な差と挙動の変化**: (1) `public` が隠れた path での未修飾の列型名は、path に無ければ `42704`（以前は public の型が黙って引かれていた。拒否側への変更）。`DROP TYPE` の `42P01`（#3288 の意図的な差）は変えない。(2) `::public.<組み込み型>`・列型の `public.<組み込み型>` は修飾なしと同じに受理される（以前は `42601`。拒否側から受理側への変化はこの形だけで、`public.` は裸の名前へ 1 対 1 で置き換わるだけ）。(3) `::pg_catalog.int4` 等は `42601` から `0A000`（D4。拒否のまま）。(4) 内部キーの `param` を持つ DB を旧バイナリで開くと、列定義の復号の検証で `CorruptSchema` になる（fail-closed。ビュー本文 D5 の前例に従い格納形式の版は上げない）
- **一時表**: 一時表の ENUM 列は、型が public でも SCHEMA 配下でも従来どおり `0A000`
- **NoSQL（HTTP）**: 修飾名を扱わず public の表だけ。public の表は D7 で SCHEMA 配下の型を持てないため、`columns[].type`（種別のみ）・エラー文言へ内部キーは届かない
- **残る未対応（受け皿 Issue 未起票。起票にはユーザー承認が要る）**: ENUM ⇄ 文字列のキャスト変換本体（SQL-47）、`DROP TYPE ... CASCADE` の SQL 表層、要素つき `CREATE SCHEMA` の `CREATE TYPE` 要素と要素内の `CREATE TABLE` での SCHEMA 配下の型の解決（D8 の実行腕）、`ALTER TYPE ... SET SCHEMA`／`ADD VALUE` の SQL 表層、`CREATE FUNCTION` の引数型・`RETURNS`・PL/pgSQL の `DECLARE` での型名の解決、型の総数上限のテナント単位の割り当て。いずれも従来どおり fail-closed
- **テスト**: `crates/engine/tests/table_rls/table27_enum_schema.rs`（列型・ALTER・配列・public との比較・path・D7 両向き・依存・改名・2 テナント・キャスト・一時表・拡張クエリ）、`logical_dump_schema.rs`（DDL 書き出しの往復）、`sql::type_name_position`・`sql::relation_name`・`sql::search_path`・`catalog` の単体、`crates/wire-server/tests/wire_sql/wire_schema_ddl.rs`（簡易・拡張クエリ。末尾 `;` 付き）。拒否を固定していた `unsupported_forms_stay_rejected`・`table27_schema_objects.rs`・`wire_schema_ddl.rs`・`relation_name.rs` の単体は新挙動へ更新した。`table_generation_bump_coverage.rs` の行番号を追随。依存の追加・更新なし

## 追記（Issue #3287）

MATERIALIZED VIEW を SCHEMA 配下に置けるようにした（ポインタ: TABLE-27・TABLE-42・TABLE-44・SQL-39・SQL-40・RLS-7・RLS-8・RLS-10）。§8 と #3196・#3199 追記の「対象外」「安全弁」「残る `0A000`」のうち MATERIALIZED VIEW に関する部分は、本追記が現状である（履歴として本文は残す）。

- **方式**: 新しい実行経路は作らない。名前解決器の「MATERIALIZED VIEW は `0A000`」の分岐を外し、`s.mv` を表・ビューと同じ内部キー（`#<schema_id>.<name>`）の `Ident` へ置き換える。未修飾名（`search_path` 経由の作成・参照・更新・削除）は #3195・#3196 の既存の書き換えに乗る。`REFRESH MATERIALIZED VIEW CONCURRENTLY <名>` の名前位置も解決の対象にした。`CREATE`・`REFRESH`・`DROP`・`SELECT` の実行腕は内部キーを受けるだけで、拡張クエリ（Parse・Bind・Describe）も #3197 の再解決で同じ契約になる
- **本文の参照先（D7）**: 既存の `view_body_relations` による検査をそのまま使う。SCHEMA 配下の matview は SCHEMA 配下の relation だけを読め、public の relation を読む形（と public の matview が SCHEMA 配下を読む形）は `42501`。matview から matview への連鎖は同じ SCHEMA 内で受理する
- **物理名**: 定義表のキー・実体メタ・行表（`matview_rows/{内部キー}`）は `user_rows/{内部キー}` と同じ流儀で内部キーをそのまま使う。`#`・`.` は識別子に現れないため public の同名と衝突しない。格納形式の版は上げない（内部キーを持つ DB を旧バイナリで開くと復号で `CorruptSchema`。fail-closed）。基表名の復号上限は、内部キーを収める専用の定数（`#` + `u64` の 10 進最大 20 桁 + `.` + 識別子上限）に変えた
- **実体化の文脈（テナント境界）**: 内部キーの matview は、実体化の対象文脈を作成・更新者と同じテナントの文脈（可視性の違いは残す）に絞る。他テナントの文脈は名前解決でこの matview に到達できないため、その文脈で本文を評価して他テナントの文脈キーの実体を書く理由が無い。public の matview は従来どおり。作成の第 1 段で SCHEMA の存在を同じ txn 内で再確認する（解析と書き込みの間の `DROP SCHEMA` で孤児を作らない）
- **`DROP SCHEMA`**: RESTRICT は matview があれば `2BP01`（従来どおり）。CASCADE は配下の matview を、実体・索引宣言・ACL・所有者・世代まで同じ txn・1 回の commit で消す。別 SCHEMA の matview が消える SCHEMA の表を読む外部依存も、根集合に matview を加えた依存列挙で先に消える。所有者ゲートの「配下に共有カタログの relation がある」判定に matview を含める。ENUM 型は `enum_types` に列挙され、`unsupported_kinds` を設定する種別は現在ない
- **論理ダンプ**: SCHEMA 配下の matview を含むダンプを固定文言で全体失敗させていた 2 か所の拒否を外した。拒否の根拠（SQL で作り直せない）が消えたため。`CREATE MATERIALIZED VIEW s.mv` の描画と本文の内部キーの逆変換は表・ビューと同じ経路
- **意図的な拒否（`intentional`）**: `ALTER MATERIALIZED VIEW ... SET SCHEMA`（修飾・未修飾とも。fail-closed の拒否のまま。専用の Issue が必要）のみ引き続き対象外。拒否する形は修飾名（`s.mv`）・`search_path` 経由の未修飾名の 2 つで、移動元の名前に付いた修飾子が存在するテナントの SCHEMA を指す形（テストが使う形）では契約上の SQLSTATE は、セッションが ACL 検査対象でない（所有者相当・ロール未束縛）か、束縛ロールがその SCHEMA の USAGE を持つ場合はどちらも `42601`（`ALTER MATERIALIZED VIEW ... OWNER TO` は所有者変更の専用分岐で受理される一方、`SET SCHEMA` は専用分岐に該当せず、汎用の `ALTER` 分岐が `TABLE` を要求するため解析段階で固定文言の構文エラーになる。移動は起きない）。ただし解析の前に移動元の修飾 matview 名の解決が先に走るため、セッション付きの修飾名解決（`schema_resolver_for`）が自テナント内で移動元の SCHEMA を見つけた直後に USAGE を検査し、ACL 検査対象のセッション（所有者相当でなく、ロールが束縛済み）で束縛ロールに USAGE がなければ構文解析へ進む前に `42501` を返す（所有者相当のセッションと現在ロール未束縛のセッションは `schema_privilege::acl_checked_principal` が ACL 検査を省略するため、USAGE の付与が無くても名前解決に成功すれば構文解析の `42601` に到達する。上記の `42601` は移動元の名前解決に成功した場合に限る）。この `42501` は修飾名（`s.mv`）に限る。未修飾名は `search_path::gate_exempt` が DDL 権限のないセッションの文を解決処理から除外するため SCHEMA の照会自体が行われず、構文解析で `42601` になる。また、移動元の名前に付いた修飾子（`s.mv` の `s`）が存在しなければ `3F000`、`pg_catalog` など名前解決段で拒否される修飾子なら `0A000` が先に返る（`public` は名前解決が通常名へ変換するため先には返らず、後続の構文解析で `42601`。ただしセッションに同名の一時表がない場合に限る。同名の一時表があると `parse_tokens_gated_inner` の `reject_public_qualified_temp` が構文解析より先に `0A000` を返す）（移動先 `SET SCHEMA dst` の `dst` は名前解決の前に構文エラー `42601` になり、`3F000`・`0A000` を決めない）。修飾子が存在する SCHEMA を指すときは `0A000` ではない。理由は、移動の対象（所有者変更の `ALTER MATERIALIZED VIEW ... OWNER TO` とは別の文）を受ける文法そのものをまだ持たない未対応の構文だからで、SCHEMA 配置の安全弁（`0A000`）とは層が異なる
- **PostgreSQL との差**: 本追記で新たに生じた差は無い（D7 による SCHEMA をまたぐ本文参照の `42501` は既存の意図的な差）
