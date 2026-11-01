# ADR: ロール・権限モデル（Issue #1960）

- ステータス: Proposed（オーナー承認待ち。承認後に Accepted へ更新し、承認の記録先は #1960 とする）
- 対応 Issue: #1960（親 #1959、Phase 6 は #1929。後続 #1961・#1962・#1963・#1965〜#1969・
  #1971〜#1973・#2232〜#2238、Phase 8 の #2081）
- 関連ポインタ: `docs/spec/04-behavior/sql-surface.md`（SQL-23・SQL-39・SQL-40）・
  `docs/spec/04-behavior/rls.md`（RLS-12）・`docs/spec/04-behavior/wire-protocol.md`
  （WIRE-2・WIRE-18）・`docs/spec/04-behavior/http-transport.md`（HTTP-4）。
  spec 本文は転記しない。契約は spec を正とし、本書では再掲しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/wire-server/src/auth.rs`（`UserStore::load_from_file`・`verify`・
  `session_policy_context`）・`crates/wire-server/src/auth/scram.rs`・
  `crates/wire-server/src/ddl_permission_opt.rs`・`crates/wire-server/src/http/session/issue.rs`・
  `crates/engine/src/policy.rs`（`PolicyContext`）・`crates/engine/src/sql/mode.rs`
  （`SessionState::ddl_allowed`）・`crates/engine/src/sql/ddl.rs`（`require_ddl_permission`）・
  `crates/engine/src/catalog/namespace.rs`・`catalog/function.rs`・`catalog/sequence.rs`
- 関連 doc: [schema-tenant-namespace.md](schema-tenant-namespace.md)（D10）・
  [udf-catalog.md](udf-catalog.md)・
  [sequence-namespace-and-ledger.md](sequence-namespace-and-ledger.md)・
  [tls-scram-design.md](tls-scram-design.md)・
  [rls-generalized-read-paths.md](rls-generalized-read-paths.md)
- 検証コード: なし（本 Issue は設計記録のみ。コードは変更しない）

## 1. 背景・目的

Phase 6（#1929）では PostgreSQL 準拠のロールと GRANT／REVOKE を段階的に入れる。後続 sub-issue
（ロールのカタログ・DDL・メンバーシップ・SCRAM、GRANT／REVOKE と検査ゲート、各オブジェクト種別、
所有者・PUBLIC・ALTER DEFAULT PRIVILEGES、認証の紐付け、users ファイルのブートストラップ化、
存在オラクル試験、経路一覧、Phase 8 の列レベル制御）に着手する前に、権限モデルの骨格
（カタログ構造・主体の伝わり方・検査点・既存機構からの移行・テナント境界との関係）を
1 つの ADR に固め、判断のぶれを防ぐ。

本書の決定は、公開 Issue の記述・PostgreSQL 17 の公開挙動・本 ADR 独自の実装上の決定だけで
構成する。spec が定める契約（個別の SQLSTATE・認証応答の区別・接続数上限・メンバーシップ
オプションの既定・検査時点の細目・ブートストラップロールの属性の細目など）はポインタ表記に
留め、README の公開済み要点を超えて具体化しない。公開境界を広げる場合は管理者の承認を得てから
追記する。独自の決定は「#NNNN で確定」と明記し、spec が確定させた事項と区別する。

## 2. 公開コードの現状（設計の前提）

| 事実 | 所在 |
| ---- | ---- |
| 利用者は `--users` ファイル（`username:tenant_id:phc`、Argon2id）で定義し、テナントはユーザーストアからサーバー側で導出する | `auth.rs` |
| DDL 権限は `--ddl-allowed-users` → `SessionState::ddl_allowed`（認証直後に 1 回だけ設定）→ `require_ddl_permission` の順で決まり、既定は `42501` | `ddl_permission_opt.rs`・`sql/mode.rs`・`sql/ddl.rs` |
| `PolicyContext` はテナントと可視性だけを持ち、認証主体を持たない。等値性は `PrefilterIndex` のキャッシュ照合の契約に使われる | `policy.rs` |
| `CREATE ROLE`／`CREATE USER`／`SET ROLE` は現状 `42601` | Issue #1959 の記述 |
| 表・ビュー等のカタログは全テナント共有。一方 SCHEMA は `(tenant_id, schema_name)`、FUNCTION は `(scope, …)` のキーでテナント単位に構造化され、SEQUENCE は `(schema, name)` | `catalog/namespace.rs`・`catalog/function.rs`・`catalog/sequence.rs` |
| SCHEMA の定義 blob には所有者の予約領域がある。`AUTHORIZATION`・`OWNER TO` は `0A000` | [schema-tenant-namespace.md](schema-tenant-namespace.md) D10 |
| SCRAM-SHA-256（`--auth-method`）は実装済み | `auth/scram.rs`・[tls-scram-design.md](tls-scram-design.md) |
| HTTP の `POST /v1/session` は `auth::verify` を SQL wire と共有する | `http/session/issue.rs` |

## 3. 決定

### D1. ロールのカタログ（#1961・#1962）

- 新しい redb テーブルを 3 つ置く（#1961 で確定）。
  - `ROLES_TABLE`: キー `(tenant_id, role_name)`、値は版バイト付き blob（内部 ID `u64`・属性
    ビット・パスワード検証子の領域）
  - `ROLE_ID_COUNTER_TABLE`: 単調カウンタ。ID は再利用しない（同名で作り直したロールが古い
    権限を拾わないようにするため）
  - メンバーシップ表: キー `(tenant_id, member_id, granted_role_id)`、値はオプションのビット
- キーの先頭を `tenant_id` にし、他テナントの行を構造上引けないようにする（SCHEMA・FUNCTION と
  同じ流儀）。表は遅延作成とし、decode は未知の版を `CorruptSchema` として fail-closed にする。

### D2. ロールの意味（#1962・#1963・#2232）

- LOGIN／NOLOGIN、INHERIT によるメンバーシップの継承、`SET ROLE` の意味は PostgreSQL 17 に
  揃える。個別の契約は SQL-39 のポインタに委ねる。
- テナント境界を越える属性は作れない（RLS-12 のポインタ）。継承の解決はテナント内の
  メンバーシップ表だけを辿り、循環を検出する。経路の探索上限は sub-issue で決める。

### D3. パスワードと認証（#2233・#1971）

- パスワードは SCRAM-SHA-256 の検証子として保存し、平文の保存・ログ出力をしない。
- 認証はカタログの LOGIN ロールを正とする。SQL wire と HTTP は同じ照合関数を共有し続ける
  （現行の `auth::verify` 共有を維持）。
- 失敗時の応答と時間の対称性は、現行の `AUTH_FAILURE_DELAY`・ダミー KDF の方針を引き継ぐ。
  細目は WIRE-2・WIRE-18・HTTP-4 のポインタに委ねる。

### D4. 認証主体の engine への伝達（#1971・#1966）

- 認証済みロールの ID は `SessionState` に持たせ、現在のロールと元のセッションロールを分ける。
- `PolicyContext` には入れない（キャッシュ照合の契約を壊さないため）。`SET ROLE` で変わるのは
  `SessionState` のロールだけで、テナント（`PolicyContext`）は変わらない。
- SQL 文から主体を書き換える経路は作らない。

### D5. ACL のカタログ（#1961・#1965・#2234・#2235）

- ACL 表のキーは `(tenant_id, object_kind, object_id, grantee_id)`、値は権限ビットと GRANT OPTION
  ビット。`object_kind` は表・列・スキーマ・シーケンス・関数のいずれか。列は
  `(表 ID, 列の識別子)` をオブジェクトとする。
- 全テナント共有のカタログにある表への権限も `tenant_id` をキーに含め、テナントごとに独立した
  ACL として持つ。
- grantee が PUBLIC のときは予約 ID で表す。PUBLIC は「同じテナントのロール全体」を指し、
  テナントを越えない。

### D6. 所有者（#2236）

- 各オブジェクトに所有者のロール ID を持たせる。SCHEMA は blob の所有者予約領域を使う
  （[schema-tenant-namespace.md](schema-tenant-namespace.md) D10 からの申し送りを解消する）。
- 表・ビュー・シーケンス・関数は `(tenant_id, object_kind, object_id) → owner_id` の別表に持つ
  （共有カタログの表について、テナントごとに所有者を持つため）。
- 所有者は全権限を持つ。`ALTER … OWNER TO`・`AUTHORIZATION` は #2236 で `0A000` から解禁する。

### D7. ALTER DEFAULT PRIVILEGES（#2237）

- 既定権限の表を置く。キーは `(tenant_id, 対象ロール ID, schema_id または全体, object_kind)`。
  作成時の ACL の初期化に適用する。意味は PostgreSQL 17 に揃え、細目は SQL-40 のポインタに委ねる。

### D8. 権限検査の共通ゲート（#1966〜#1969）

- 判定関数は 1 つにする（`(主体, 対象, 必要な権限) -> Result<(), InsufficientPrivilege>` の形）。
- 検査は文の束縛の時点で、実行前・副作用ゼロのうちに行う。SQL 表層・NoSQL 表層・engine Rust API
  のすべての経路を同じゲートに通す。
- 既存の `require_ddl_permission` は第 2 の機構として残さず、移行後はこのゲートの薄いファサード
  にする。権限の判定は RLS の暗黙適用より前段に足す制限で、RLS を緩めない（RLS-12 のポインタ）。
- 付与が無ければ拒否する fail-closed を既定とする。

### D9. 列権限と Phase 8 の統合（#1965・#2081）

- 列権限の判定は D8 のゲートが持つ単一の関数で行う。Phase 8 の列レベル制御（#2081）は同じ関数を
  呼ぶだけで、別機構を作らない。
- 投影・述語・並べ替え・集計・RETURNING・NoSQL の filter／sort が参照する列の集合は、束縛の結果
  から一括で得て検査に渡す。

### D10. テナント境界と存在の秘匿（#1972・RLS-12 のポインタ）

- 名前解決は自テナントのカタログだけで行う。他テナントのロール・スキーマ・シーケンス・関数は、
  不在と同じ経路・同じ固定文言・同じ `wire_code` で返す（既存の固定文言パターンを再利用する）。
- 解決の順序は「テナント内の解決 → 権限検査」とし、他テナントのオブジェクトには権限検査の分岐
  そのものが走らない形にする。試験は #1972 が担う。

### D11. users ファイルのブートストラップ専用化（#2238）

- 起動時、ロールカタログが空のテナントに限り、ファイルの行から初期ロールを作る。以後は
  ファイルの内容でカタログを上書きしない。不一致は起動ログで警告し、権限を広げる側には倒さない。
- `--ddl-allowed-users` は、初期ロールに付与する権限への写像として移行する。細目は SQL-39・
  SQL-23 のポインタと #2238 で決める。

### D12. 既存 DB の移行と後方互換（#1961・#2236）

- ロール表が無い既存 DB は遅延作成で開ける。改訂前からあるオブジェクトの所有者は、ブートストラップ
  の初期ロールとみなす。旧版の blob は版バイトにより decode 時に明示的に扱い、黙って補正しない。

### D13. NoSQL 表層

- ロールと GRANT の管理文は NoSQL から発行できない（`0A000`。公開 Issue の範囲）。権限検査そのもの
  は D8 により NoSQL にも効く。HTTP のセッションは D3 に従う。

## 4. Issue と決定の対応

| Issue | 担当する決定 |
| ----- | ------------ |
| #1961 | D1・D5・D12 |
| #1962 | D1・D2 |
| #1963 | D2 |
| #2232 | D2 |
| #2233 | D3 |
| #1965 | D5・D9 |
| #1966 | D4・D8 |
| #1967〜#1969 | D8 |
| #2234・#2235 | D5 |
| #2236 | D6・D12 |
| #2237 | D7 |
| #2238 | D11 |
| #1971 | D3・D4 |
| #1972 | D10 |
| #1973 | 経路一覧（§5） |
| #2081 | D9 |

## 5. 対象外・未決事項

- 個別の SQLSTATE・検査時点の細目・接続数の上限・メンバーシップオプションの既定は、ポインタ表記で
  各 sub-issue に委ねる。
- 利用者定義の行セキュリティポリシー（#2272・#2278）は本 ADR の範囲外とする。
- 権限判定の経路一覧とチェックリストは #1973 の担当とする。

## 6. OWASP Top 10 の観点

| 観点 | 本 ADR での担保 |
| ---- | --------------- |
| アクセス制御の不備 | 権限はテナント境界の内側だけで働き、RLS を緩める経路を作らない。判定点を 1 つにし第 2 の判定系を並べない（D8・D10） |
| 情報漏えい（存在オラクル） | 他テナントのオブジェクトは不在と同じ応答・経路にし、権限検査の分岐差を作らない（D10） |
| 認証の不備 | 検証子だけを保存し平文を保存・出力しない。失敗時の応答と時間の対称性を引き継ぐ。SQL から主体を書き換えない（D3・D4） |
| 不安全な設計 | users ファイルとカタログの不一致で権限を広げない。未知の版は fail-closed。メンバーシップの循環検出と探索上限（D1・D2・D11・D12） |
| インジェクション | ロール名・オブジェクト名は既存の識別子検証を通し、カタログのキーを未検証の文字列連結で作らない |
| セキュリティ設定ミス・秘密情報 | 本書に資格情報や検証子の例を載せない。spec 本文を転記しない |
| 脆弱な依存 | 依存の追加は無い |
