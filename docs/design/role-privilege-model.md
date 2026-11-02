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
| 利用者は `--users` ファイル（`username:tenant_id:phc[:scram_verifier]`。phc は Argon2id、第 4 フィールドは任意）で定義し、テナントはユーザーストアからサーバー側で導出する | `auth.rs` |
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
- ロール名の一意性はテナント内に限る。ただし認証前はテナントが未確定なので、LOGIN 属性を持つ
  ロールの名前は、認証前に主体を一意に解決するための「ログイン名」を兼ねる。ログイン名は
  サーバー全体で一意とし、サーバー側の「ログイン名 → `(tenant_id, role_id)`」対応表（redb の
  別表。キーはログイン名）で認証前に解決する。現行の `auth::verify` が username だけから
  テナントを導出する挙動と同じ形で、クライアントが `tenant_id` を自己申告する経路は作らない。
  テナント利用者が任意のログイン名を自己登録する設計は採らない。他テナントの使用状況で
  `CREATE ROLE ... LOGIN` の成否が変わると、文言を固定しても成否差から他テナントのログイン名の
  存在が判別できるため（D10 の存在秘匿に反する）。ログイン名はサーバーが次の規則で発行する。
  - 発行するログイン識別子は、テナントごとの名前空間で修飾した形とし、テナント修飾子は
    テナントごとに一意でサーバーが割り当てる。修飾子とロール名の結合は単射な符号化で行う。
    単純な文字列連結は単射でなく（修飾子 `a` + ロール名 `b_c` と修飾子 `a_b` + ロール名 `c` が
    同じ連結結果になり得る）、テナント間衝突が登録の成否を左右してしまうため採らない。符号化は
    長さ接頭辞方式とし、`<修飾子のバイト長（10 進）>:<修飾子><ロール名>` の形で識別子を組む。
    修飾子の長さは識別子の先頭から一意に復号できるので、異なる `(修飾子, ロール名)` の組は
    必ず異なる識別子になる（区切り文字のエスケープを要さないため、修飾子・ロール名に
    どの文字が含まれても単射性は保たれる）。修飾子はサーバーが英数字のみから発行し、
    ロール名は識別子の長さ上限内であることを検証する。上限超過・不正なバイト列は固定文言で
    拒否する（拒否の判定は当該テナント自身の入力だけで決まり、他テナントの状態に依存しない）。
    この単射性により、識別子はサーバー全体で衝突せず、ロール名がテナント内で一意なら登録の
    成否は他テナントの登録状態に依存しない。
  - 対応表への登録は `CREATE ROLE ... LOGIN` と同一トランザクションで行う。衝突し得るのは
    同一テナント内の重複だけで、これは当該テナント自身の情報に閉じる。
  - 修飾子の割り当てはサーバー管理経路（テナントのプロビジョニング）に限り、テナント利用者の
    SQL からは変更できない。クライアントはログイン時に修飾済みの識別子を接続の user として渡す。
  - NOLOGIN ロールは対応表に載らない。

### D2. ロールの意味（#1962・#1963・#2232）

- LOGIN／NOLOGIN、INHERIT によるメンバーシップの継承、`SET ROLE` の意味は PostgreSQL 17 に
  揃える。INHERIT（権限を継承するか）と SET（`SET ROLE` で切り替えられるか）はメンバーシップ付与ごとの
  独立したオプションとして扱い、一方が許可されていても他方は許可されない。個別の契約は SQL-39 の
  ポインタに委ねる。
- テナント境界を越える属性は作れない（RLS-12 のポインタ）。継承の解決はテナント内の
  メンバーシップ表だけを辿り、循環を検出する。経路の探索上限は sub-issue で決める。

### D3. パスワードと認証（#2233・#1971）

- パスワードは SCRAM-SHA-256 の検証子として保存し、平文の保存・ログ出力をしない。
- 認証はカタログの LOGIN ロールを正とする。認証前の主体解決は D1 のログイン名対応表で行い、
  解決後にその `(tenant_id, role_id)` のカタログ行から検証子を引く。SQL wire と HTTP は同じ照合関数を共有し続ける
  （現行の `auth::verify` 共有を維持）。
- 既存ユーザーの移行: 現行の `--users` ファイルは、`username:tenant_id:phc`（Argon2id）に任意の
  第 4 フィールドとして SCRAM 検証子を持てる。`--auth-method scram-sha-256` では全行にその検証子を
  必須としている。Argon2id の PHC 文字列から SCRAM 検証子は生成できない。方針は次のとおり（担当は #2233）。
  - ブートストラップ時、ファイル行に SCRAM 検証子があれば PHC と併せてロールの認証情報として
    取り込む。これにより `scram-sha-256` 運用の既存デプロイは、旧検証子を一度も使わずそのまま
    ログインできる。検証子の形式が不正な行は取り込まず、現行の読み込みと同様に起動を拒否する。
  - 移行期間中、ロールは Argon2id の旧検証子を併存して保持し、`--auth-method` が旧方式の接続は
    旧検証子で照合する。SCRAM 方式の接続は SCRAM 検証子があるロールのみ許可し、無ければ
    fail-closed で拒否する（旧検証子から SCRAM へ自動で降格・推測しない）。
  - 追加の対象は PHC しか無い行（SCRAM 検証子を持たないロール）だけである。その検証子は、平文を
    扱えるタイミング（パスワード再設定、または旧方式での認証成功時の再導出）でのみ追加する。
    旧検証子の廃止は、全ロールに SCRAM 検証子が揃った後に別途決める。
- 失敗時の応答と時間の対称性は、現行の `AUTH_FAILURE_DELAY`・ダミー KDF の方針を引き継ぐ。
  細目は WIRE-2・WIRE-18・HTTP-4 のポインタに委ねる。

### D4. 認証主体の engine への伝達（#1971・#1966）

- 認証済みロールの ID は `SessionState` に持たせ、現在のロールと元のセッションロールを分ける。
- `PolicyContext` には入れない（キャッシュ照合の契約を壊さないため）。`SET ROLE` で変わるのは
  `SessionState` のロールだけで、テナント（`PolicyContext`）は変わらない。
- 禁止するのは、元の認証済みセッションロールとテナントを SQL 文から書き換える経路である。現在の
  ロールは、認可済みの `SET ROLE` に限って変更でき、`RESET ROLE`（`SET ROLE NONE` を含む）で
  元のセッションロールへ戻せる。
- `SET ROLE` の認可は、元のセッションロールから対象ロールまで、SET オプションを許可した付与だけで
  構成されるメンバーシップ経路が存在することを要件とする。メンバーであるだけでは足りず、SET FALSE の
  付与は経路に使えない。間接メンバーシップでは経路上の各付与が SET を許可していなければならない
  （PostgreSQL 17 と同じ）。検証できない場合は fail-closed で拒否する。具体条件は SQL-39 のポインタに委ねる。

### D5. ACL のカタログ（#1961・#1965・#2234・#2235）

- ACL 表のキーは `(tenant_id, object_kind, object_id, grantee_id, grantor_id)`、値は権限ビットと
  GRANT OPTION ビット。付与者（grantor）をキーに含め、異なる付与者による同一 grantee への付与を
  1 行へ潰さず独立した付与として保持する。これにより、REVOKE は自分が付与した行だけを対象に
  でき、GRANT OPTION の取り消しは、その grantee が下流へ行った付与（grantor がその grantee の
  行）を辿って依存関係を追跡できる（CASCADE は下流の付与を連鎖して取り消し、RESTRICT は
  下流が残る場合に拒否する。意味は PostgreSQL 17 に揃え、細目は SQL-40 のポインタに委ねる）。
  オブジェクト所有者による付与は grantor を所有者として記録する。`object_kind` は表・列・
  スキーマ・シーケンス・関数のいずれか。列は `(表 ID, 列の識別子)` をオブジェクトとする。
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
- 共有カタログの同一オブジェクトに対するテナント別の所有者は、そのテナントの ACL の管理
  （GRANT・REVOKE）と、テナント内の `ALTER … OWNER TO` にのみ作用する。共有された定義そのものの
  変更（DROP・ALTER の構造変更）は他テナントへ波及するため、所有者権限の対象外とし、共有定義を
  作成・管理するサーバー管理経路（テナント外）に限定する。テナントのロールは、所有者であっても
  共有定義を変更できない。テナントごとに分離された定義（SCHEMA・FUNCTION 等）は、自テナント
  の所有者が通常どおり DROP・ALTER できる。

### D7. ALTER DEFAULT PRIVILEGES（#2237）

- 既定権限の表を置く。キーは `(tenant_id, 対象ロール ID, schema_id または全体, object_kind)`。
  作成時の ACL の初期化に適用する。意味は PostgreSQL 17 に揃え、細目は SQL-40 のポインタに委ねる。

### D8. 権限検査の共通ゲート（#1966〜#1969）

- 判定関数は 1 つにする（`(主体, 対象, 必要な権限) -> Result<(), InsufficientPrivilege>` の形）。
- 検査は文の束縛の時点で、実行前・副作用ゼロのうちに行う。SQL 表層・NoSQL 表層・engine Rust API
  のすべての経路を同じゲートに通す。
- 既存の `require_ddl_permission` は第 2 の機構として残さず、移行後はこのゲートの薄いファサード
  にする。権限の判定は RLS の暗黙適用より前段に足す制限で、RLS を緩めない（RLS-12 のポインタ）。
  共有定義を変える DDL は D6 のとおりテナントのロールからは拒否し、自テナント所有者の検査だけで
  他テナントに影響する変更を通さない。
- 付与が無ければ拒否する fail-closed を既定とする。

### D9. 列権限と Phase 8 の統合（#1965・#2081）

- 列権限の判定は D8 のゲートが持つ単一の関数で行う。Phase 8 の列レベル制御（#2081）は同じ関数を
  呼ぶだけで、別機構を作らない。
- 投影・述語・並べ替え・集計・RETURNING・NoSQL の filter／sort が参照する列の集合は、束縛の結果
  から一括で得て検査に渡す。

### D10. テナント境界と存在の秘匿（#1972・RLS-12 のポインタ）

- 名前解決は自テナントのカタログだけで行う。他テナントのロール・スキーマ・シーケンス・関数は、
  不在と同じ経路・同じ固定文言・同じ `wire_code` で返す（既存の固定文言パターンを再利用する）。
- ログイン識別子の存在オラクルは D1 の単射符号化（長さ接頭辞）で塞ぐ。登録の成否・拒否は
  当該テナント自身の入力と自テナント内の重複だけで決まり、他テナントの識別子の有無とは
  独立である。
- 解決の順序は「テナント内の解決 → 権限検査」とし、他テナントのオブジェクトには権限検査の分岐
  そのものが走らない形にする。試験は #1972 が担う。
- シーケンスは現状の表が共有の `(schema, name)` キーであるため、名前解決を自テナント内で行う
  には、テナント別の名前空間（キーへの `tenant_id` 付与）か、共有定義のテナント別ビューが要る。
  本 ADR は、共有定義の変更をテナントのロールに許さない（D6）ことを前提に、テナント内での名前
  解決と権限検査を D8 のゲートで行う方針とし、キーの具体形は #1972 と #2236 で確定する。

### D11. users ファイルのブートストラップ専用化（#2238）

- 起動時、テナントごとの「初期化済みマーカー」が未設定のテナントに限り、ファイルの行から
  初期ロールを作る（行に SCRAM 検証子があれば D3 のとおり認証情報として取り込む）。マーカーは初期ロール投入と同一トランザクションで永続化し、以後は
  ファイルの内容でカタログを上書きしない。不一致は起動ログで警告し、権限を広げる側には倒さない。
- ブートストラップ済みかどうかはロールの有無から分離する。管理操作でテナントの最後のロールを
  削除してロールが空になっても、マーカーが立っていれば自動で再投入しない。古い users ファイルを
  残したまま再起動しても、削除済みのログイン主体・資格情報・`--ddl-allowed-users` 由来の権限は
  再作成されない（fail-closed）。空のカタログから再び始める操作は、明示的な管理操作に限る。
- ロール表はあるがマーカーが無い DB（マーカー導入前に作られたもの）は、ロールが 1 件でもあれば
  初期化済みとみなしてマーカーを補う。ロールが 0 件のときは投入せず警告に留める（安全側）。
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
| 認証の不備 | 検証子だけを保存し平文を保存・出力しない。失敗時の応答と時間の対称性を引き継ぐ。元の認証済みロール・テナントを SQL から書き換えない（現在ロールは認可済み `SET ROLE` のみ。D1・D3・D4） |
| 不安全な設計 | users ファイルとカタログの不一致で権限を広げない。未知の版は fail-closed。メンバーシップの循環検出と探索上限（D1・D2・D11・D12） |
| インジェクション | ロール名・オブジェクト名は既存の識別子検証を通し、カタログのキーを未検証の文字列連結で作らない |
| セキュリティ設定ミス・秘密情報 | 本書に資格情報や検証子の例を載せない。spec 本文を転記しない |
| 脆弱な依存 | 依存の追加は無い |

## 7. Issue #1961 実装記録

Issue #1961 は決定 D1（ロールのカタログ）・D5（ACL のカタログ）・D12（既存 DB の移行と後方互換）のうち、永続化層だけを実装した。実装は `crates/engine/src/catalog/role.rs`（ロール・メンバーシップ・ログイン名対応表）と `catalog/privilege.rs`（ACL）。認証・権限検査・DDL 構文は含まず、wire-server は無変更のため `--users` の挙動は変わらない。ポインタ: SQL-39・SQL-40・RLS-12。

### redb 表（すべて遅延作成）

| 表名 | キー | 値 |
| ---- | ---- | -- |
| `roles` | `(tenant_id, role_name)` | ロール blob |
| `role_id_counter` | 固定キー `next_id` | `u64`（1 始まり・再利用しない） |
| `role_memberships` | `(tenant_id, member_id, granted_role_id)` | メンバーシップ blob |
| `role_login_names` | 符号化済みログイン識別子 | ログイン対応 blob |
| `acl` | `(tenant_id, object_kind, object_key, column, grantee_id, grantor_id)` | ACL blob |

- 本 ADR の D5 は新しい表を 3 つと書く一方、ログイン名対応表を別表として記述している。実装ではこれを 4 表目（`role_login_names`）として定義した。ログイン名対応表だけがサーバー全体の表で、他の表はキーの先頭が `tenant_id` である。
- 旧 DB は表が無いことがそのまま「空のカタログ」になる。読み取りでも表は作らない。`Storage::open` での移行処理は無い。
- 各 blob は先頭が版バイト（現状 1）で、未知の版・予約ビット・切断・末尾の余り・上限超過は固定文言の `CorruptSchema` で拒否する。一覧と有無の判定でも破損した行を読み飛ばさない。

### blob の形

- ロール: 版 / 内部 ID `u64` / 属性ビット `u16`（LOGIN・INHERIT・CREATEROLE・CREATEDB。SUPERUSER 相当は作らない）/ connection_limit `i32` / valid_until（タグ + UTC マイクロ秒 `i64`）/ SCRAM 検証子と旧形式ハッシュ（`u16` 長 + 不透明な ASCII 印字可能バイト列。D3 の移行期間に併存）。
- メンバーシップ: 版 / オプションビット（ADMIN・INHERIT・SET）。付与者は持たない。REVOKE の付与者指定などで必要になれば、#1965 が版 2 で足す。
- ログイン対応: 版 / tenant_id / role_id。識別子は `<修飾子のバイト長>:<修飾子><ロール名>` の長さ接頭辞方式で単射に符号化する。解決時に参照先のロールが存在し、LOGIN 属性を持ち、ID と名前が一致することを確かめる。
- ACL: 版 / privileges `u16` / grant_options `u16`。権限ビットは PostgreSQL 17 の公開の権限種別に揃え、対象の種別ごとに許可集合を持つ。

### ID 0 の扱い

ロール blob の内部 ID とメンバーシップのキーの ID では 0 を破損として拒否する。ACL の `grantee_id` だけは 0 を PUBLIC（同じテナントのロール全体）の予約 ID として受理する。`grantor_id` の 0 は破損である。

### relation のライフサイクルへの追随

名前をキーにした表・列の ACL 行は、DROP TABLE（CASCADE・DROP SCHEMA の表削除を含む）・DROP VIEW・DROP MATERIALIZED VIEW・ALTER TABLE RENAME・列の DROP・列の RENAME と同じ write txn の中で掃除・移動する（同名の再作成が古い付与を引き継ぐ fail-open を作らない）。スキーマ・シーケンス・関数は内部 ID をキーにし、ID は再利用されないため掃除は不要で、GRANT を解禁する #2234・#2235 の範囲とする。

既知の制約: relation 単位の掃除は ACL 表を全件走査する（計算量は ACL の総件数に比例）。逆引き索引の追加は #1965 以降の判断とする。

## 8. Issue #1965 実装記録（GRANT／REVOKE の構文と権限カタログ更新）

ビヘイビア ID のポインタは SQL-40（`docs/spec/04-behavior/sql-surface.md`）。関連は SQL-39・SQL-23・RLS-12。意味論は
PostgreSQL 17 の公開挙動と本 ADR の D5・D8・D9・D10 に従い、spec 本文は転記しない。

### 実装した範囲

- 構文: `sql::allowlist::grant`（カタログを引かない構造検証）。権限形は表の `SELECT`・`INSERT`・`UPDATE`・
  `DELETE`・`TRUNCATE`・`REFERENCES`・`TRIGGER`・`MAINTAIN` と `ALL [PRIVILEGES]`、列リスト、`ON [TABLE] 表[, ...]`、
  付与先（ロール名・`PUBLIC`・`CURRENT_USER`／`CURRENT_ROLE`・`SESSION_USER`・旧来の `GROUP`）、`WITH GRANT OPTION`、
  `REVOKE` の `GRANT OPTION FOR`・`CASCADE`／`RESTRICT`。ロール形（`GRANT ロール TO ロール`・`WITH ADMIN|INHERIT|SET`・
  `REVOKE ... OPTION FOR`）は構造化だけを行い、実行は `0A000`（メンバーシップは #2232）。
- 実行: `sql::grant_ddl`（SQLSTATE の写像）と `catalog::privilege::apply`（単一 write txn で ACL を原子的に更新し、
  実際に行を書いた relation の世代を進めて DDL として commit 記録へ残す）。
- 結線: `ParsedSql::GrantRevoke`・`SqlOutcome::GrantRevoke`・wire のコマンドタグ `GRANT`／`REVOKE`。
  `relation_positions` は `ON [TABLE]` 直後の並びだけを関係位置にする（ロール名・列名・範囲外の対象種別は解決しない）。

### 確定した判断

| # | 判断 | 理由 |
| - | ---- | ---- |
| J1 | 付与者は `SessionState` の現在ロール ID。未束縛なら、カタログ照会・名前解決・`0A000` 判定（実行段のもの）より前に、自動コミットと明示 Tx の両方の腕で `42501` | 主体の無い付与は記録できない（fail-closed）。権限の無い主体に対象・ロールの有無を示さない |
| J2 | 現在ロール ID の格納（`current_role_id`・`session_role_id`・`bind_role_ids`）は、並行する PR が同名・同意味で定義する予定のものと同じ形で最小限だけ追加した | マージ順によらず競合を重複の除去に留める。束縛は認証層が 1 回だけ行う契約（#1971）で、SQL からは書き換えられない |
| J3 | 束縛ロールは書き込みと同じ txn で実在を確かめ、不在なら `42501` | DROP 済みの古い束縛で付与しない |
| J4 | 所有者の機構（#2236）までの代替として、DDL 権限ゲートを通過した主体を「所有者相当」（対象の全権限を GRANT OPTION 付きで持つ）とみなす。記録される付与者は常に現在ロール ID | ポリシー DDL と同じ代替。別の所有者相当の主体が付けた付与は REVOKE できない（所有者の概念が入るまでの既知の制約）。所有者相当のロール自身が明示の GRANT OPTION を持つ場合、CASCADE がその主体の「所有者として付けた付与」まで依存とみなして取り消す（RESTRICT は拒否側に倒れる） |
| J5 | （#2232 で見直し。§10 の M6）継承（INHERIT の閉包）を見る。付与者は最良のロールを選ぶ | 当初は継承を #2232 の担当として見なかった |
| J6 | 表と列で許す権限は `PrivilegeObject::allowed_privileges` と一致させる。対象に許されない権限（`USAGE ON TABLE` 等）と PUBLIC への GRANT OPTION は `0LP01`（`ErrorClass::InvalidGrantOperation` を追加。HTTP 射影は 400・NoSQL 表層からは到達不能） | PostgreSQL 17 と同じ SQLSTATE |
| J7 | 付与先ロールは自テナントのカタログだけで引く。不在と「他テナントにだけ在る」は同じ `42704`・同じ文言形式（`SqlSurfaceError::UndefinedRole`。利用者が書いた名前だけを含む） | D10 |
| J8 | 範囲外の形は構文段で固定の `0A000`（`ON SCHEMA`・`ON ALL TABLES IN SCHEMA` は #2234 で解禁。§10）: `ON SCHEMA`・`ON SEQUENCE`・`ON FUNCTION`・`ON DATABASE` などの対象種別・`ALL ... IN SCHEMA`（#2234・#2235）、`GRANTED BY`、`ALTER DEFAULT PRIVILEGES`（#2237。汎用 `ALTER` より前で分岐） | 状態に依存しない拒否なので存在オラクルにならない |
| J9 | 明示トランザクション内は J1 のゲートの後で `0A000` | ACL の書き込みは独立した write txn で commit され、ROLLBACK で取り消せない |
| J10 | ビュー・matview への表単位の GRANT／REVOKE は受理する。列リストはベース表だけで、ビュー・matview は `0A000` | ビューの列の解決（別名・式列）が未整備。fail-closed |
| J11 | 一時表は一時カタログの振り分け（`parse_tokens_gated`）で `0A000`。ACL のキーは共有カタログの relation キーで、セッション内の一時表とは名前空間が違う | 既存の混在拒否に乗る |
| J12 | PostgreSQL の警告（`01006`／`01007` 相当）は `GrantRevokeOutcome` に積むだけで wire へは出さない | NoticeResponse の仕組みが無い（ポリシー DDL と同じ扱い） |

### PostgreSQL 17 の意味論のうち実装したもの

- 解決順: 対象 relation（`42P01`・種別違い `42809`）→ 付与先ロール（`42704`）→ 列（`42703`）→ 付与権限の判定。
  D10 の「テナント内の解決 → 権限検査」と整合する（束縛済みで権限の無いロールは、存在しない表に `42P01`、存在する表に
  `42501` を受けるのは PostgreSQL と同じ。未束縛のロールには常に `42501`）。
- 付与権限: 付与者が要求された権限の GRANT OPTION を 1 つも持たず対象に何の権限も持たなければ `42501`。権限はあるが
  GRANT OPTION が無い場合は警告のみで何も変えない。GRANT OPTION のある分だけを付与し、`ALL` 以外で一部だけの場合は警告。
  REVOKE も同じ判定。列への付与は表単位の GRANT OPTION も数える。
- 書き込み: 既存の行を読んでビットを OR（GRANT）／落とす（REVOKE）。権限を落とすときは対応する GRANT OPTION も落とす。
  `GRANT OPTION FOR` は GRANT OPTION だけを落とす。結果が 0 の行は削除する。REVOKE は付与者自身の行だけが対象。
- 表単位の REVOKE は同じ付与先・付与者の列単位の行からも同じ権限を落とす。列単位の REVOKE は表単位の行に触れない。
- CASCADE／RESTRICT: GRANT OPTION を失った付与先が、残りの GRANT OPTION（他の付与者からのものを含む）で賄えない
  下流の付与を持つとき、RESTRICT は `2BP01`、CASCADE は連鎖して取り消す。訪問済みの集合と反復上限で有界にする
  （超過は `54000`）。GRANT OPTION の循環付与（上流の付与者への付与）は `0LP01`。
- 複数の対象・付与先・列は重複を除いた上で 1 つの write txn で原子的に処理する。どれか 1 つでもエラーなら commit しない。
  各リストは件数の上限（256）、1 文の対象の総数は 20,000 で検証してから確保する。

### 対象外（後続）

メンバーシップの実行・循環拒否・継承（#2232）、スキーマ・シーケンス・関数・`ALL ... IN SCHEMA`（#2234・#2235）、所有者と PUBLIC の既定
（#2236）、`ALTER DEFAULT PRIVILEGES`（#2237）、権限検査ゲート（#1966〜#1969）、列レベル制御（#2081）、認証主体とロールの束縛
（#1971・#2238。それまで wire の全セッションは GRANT／REVOKE で `42501`）、`GRANTED BY`、NoticeResponse による警告の送出、
ビュー・matview の列権限、relation 単位の ACL 掃除の逆引き索引。

## 9. Issue #1962 実装記録

Issue #1962 は決定 D1・D2 のうち、`CREATE ROLE`／`CREATE USER`／`DROP ROLE`（`DROP USER` を含む）の SQL 表層を実装した。構文検証は `sql/allowlist/role.rs`、実行は `sql/role_ddl.rs`、永続化は #1961 のカタログ（`catalog/role.rs`）に `drop_role_in_txn`・`Storage::drop_roles` と、`catalog/privilege.rs` に `acl_references_role_in_txn` を足した。ポインタ: SQL-39・RLS-12・WIRE-18。

### 検証子の生成を engine へ移した

`CREATE ROLE ... PASSWORD` は engine の実行腕で検証子を作る。engine から wire-server へは依存できないため、`hmac_sha256`・`base64_std`・SCRAM 検証子（`ScramVerifier`・`generate_verifier`）・`/dev/urandom` の読み取りを `engine::crypto` へ移した。wire-server は `pub use` で再公開するだけで、既存の呼び出し元・`hash-password`・認証の挙動は変わらない。保存形式（`SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`）は認証側と単一実装を共有する。依存の追加はない。

### 受理する構文

| 構文 | 扱い |
| ---- | ---- |
| `CREATE ROLE`／`CREATE USER <名> [WITH] LOGIN/NOLOGIN, INHERIT/NOINHERIT, CREATEROLE/NOCREATEROLE, CREATEDB/NOCREATEDB, [ENCRYPTED] PASSWORD '...'/NULL` | 受理。`CREATE USER` の既定は `LOGIN`。コマンドタグは `CREATE ROLE` |
| `NOSUPERUSER`・`NOBYPASSRLS`・`NOREPLICATION` | 受理（既定どおり） |
| `SUPERUSER`・`BYPASSRLS`・`REPLICATION` | `42501`（テナント境界を越える属性。`BYPASSRLS` は #2278 段 2 で解禁） |
| `UNENCRYPTED PASSWORD`・`VALID UNTIL`・`CONNECTION LIMIT`・`SYSID` | `0A000`（#2233 の担当。`IN ROLE`・`ROLE`・`ADMIN` は #2232 で受理。§10） |
| 重複・矛盾するオプション、未知語、`$n`、修飾名 | `42601` |
| `pg_` 接頭辞・`public`・`none` | `42939` |
| `DROP ROLE`／`DROP USER [IF EXISTS] a[, b]...` | 受理。全件を 1 つの write txn で処理し、途中で失敗したら全件取り消す |

### パスワード

平文は `RolePassword`（`Debug` は伏せ字・`Display` なし・`Drop` で 0 上書き）に閉じ込め、権限ゲートと明示 Tx ゲートを通った後に検証子へ変換する。salt は CSPRNG の 16 バイトで、乱数源が読めなければロールを作らない。印字可能 ASCII 以外と 1024 バイト超は `22023`、空文字列はパスワード無し。`SCRAM-SHA-256$` で始まる文字列は、反復回数と salt 長が認証側の要求と一致するときだけ既にハッシュ済みとしてそのまま保存し、満たさなければ `22023` で拒否する。`md5` 形は `0A000`。エラー文言にはパスワードの内容・長さ・位置を含めない。

### DROP ROLE の依存検査

1 件ごとに、解決（無ければ `42704`、`IF EXISTS` は skip）→ 依存検査（`2BP01`）→ メンバーシップの掃除（member 側・granted 側の両方。PostgreSQL と同じく依存扱いにしない）→ 削除の順で行う。依存検査は `role_has_dependents_in_txn` に集約し、現在は ACL の付与先・付与者だけを見る。所有者（#2236）・ポリシーの `TO <ロール>`（#2278 段 2）の検査はこの関数へ足す。

### 判断の記録

- **42501 の判定は DDL 権限ゲート**: `CREATEROLE`／`ADMIN OPTION` による判定はセッションにロールが束縛される #1966・#1971 の担当で、ポリシー DDL と同じく `require_ddl_permission` で代替する。
- **ログイン名対応表へは登録しない**: テナント修飾子の提供経路がまだ無く、カタログのロールで認証する経路（#2233）も無いため、`LOGIN` はロール blob に保存するだけでログインはできない（fail-closed）。修飾子の提供と既存 LOGIN ロールの後追い登録は #2233 の担当。
- **明示トランザクション内は `0A000`**: ロールの write txn は独立に commit され ROLLBACK で取り消せないため（他の DDL と同じ）。権限が無ければ `42501` を先に返す。

### 対象外

`CREATEROLE`／`ADMIN OPTION` による判定（#1966・#1971）、ログイン名対応表への登録（#2233）、`VALID UNTIL`・`CONNECTION LIMIT`（#2233）、`IN ROLE`・`ROLE`・`ADMIN`（#2232）、`ALTER ROLE`・`SET ROLE`（#1963）、`BYPASSRLS` の属性ビット（#2278 段 2）、所有者による依存検査（#2236）、自分のセッションのロールを DROP したときの `55006`（#1971 の後）、`CREATE GROUP`／`DROP GROUP`。

## 11. Issue #2232 実装記録（ロールのメンバーシップと継承）

Issue #2232 は SQL-39 (7) のうち、メンバーシップの付与・取り消し・循環の拒否・INHERIT による継承・付与権限の継承・`pg_has_role` 相当の評価関数を実装した。グラフ探索は `catalog/role/graph.rs` の `effective_roles_in_txn` に 1 本化し（D8: 判定系を 2 つ作らない）、`SET ROLE`（#1963）・共通ゲート（#1966）・ポリシーの `TO` ロール（#2278 段 2）はこの関数群だけを呼ぶ。付与・取り消しの適用は `catalog/role/membership_apply.rs`（`Storage::apply_membership_changes`）、SQL の写像は `sql/grant_ddl.rs`、`CREATE ROLE` の句は `sql/allowlist/role.rs`・`sql/role_ddl.rs`。

### 判断の記録

- **M1 メンバーシップ blob を版 2 にして付与者を持たせた**: 版 2 は `[2, bits, grantor_id(u64 LE)]`。付与者なし（版 1・crate 内の生の書き込み）は版 1 のまま保存し、読み出しは両方を受ける。付与者 0・未知の版・予約ビット・切断・末尾の余りは `CorruptSchema`。キーは `(tenant, member, granted)` のままで、組ごとに 1 行。
- **M1b 版 3（所有者相当を根拠とした付与の記録）**: 版 3 は版 2 と同じ配置で、`grantor_root` を真として表す（所有者相当の権限＝DDL 権限ゲート通過を根拠に付けた行）。ADMIN の依存評価（`has_independent_admin`・`resolve_dependents`）はこの行を付与者の ADMIN に依存しない根として扱い、RESTRICT の拒否にも CASCADE の削除にも含めない。版 1・版 2 は `grantor_root` なしとして読み、従来の判定へフォールバックする。版 3 は変更前のバイナリでは読めない（未知の版として破損扱い）。
- **M1c 1 行 = 1 つの付与根拠**: 既存行への付与（`grant_one`）は、実行者の根拠（所有者相当の root、または ADMIN による付与者）が行の根拠と同じ場合だけ通常に更新する。所有者相当が非 root 行へ付与するときは、更新後の行が今回の付与（明示＋既定値）で裏付けられる場合に限り root へ付け替える。それ以外の根拠違いでオプションを足す・変える更新は、単一行で 2 つの根拠を表せないため副作用なく拒否する（言い直しは何もしない）。
- **ACL の付与者選択と所有者**: 対象の所有者が付与者の INHERIT 閉包に含まれる場合は、所有者の暗黙の全 GRANT OPTION を認め所有者名義で付与・取り消す（`catalog/privilege/apply.rs`。継承しないメンバーシップ経由は対象外）。
- **M2 付与・取り消しの権限は ADMIN**: 現在ロールから INHERIT の辺だけを辿った閉包（自分を含む）に、対象ロールへの直接の行で ADMIN が真のロールがあれば許可し、最も近いもの（同点はホップ順・ID 昇順）を付与者に記録する。ロールは自分自身の ADMIN を持たない。DDL 権限ゲートを通過した主体は所有者相当（#2236 までの代替）として常に許可し、付与者を問わず取り消せる。CREATEROLE 属性だけでは許可しない（#1966・#1971 が引き継ぐ）。
- **判定順（計画からの変更）**: 実行ロールの現存 → 付与するロールの解決 → 付与先の解決（不在と他テナントのロールは同じ `42704`）→ ロール単位の ADMIN 判定（`42501`）→ 組ごとの循環検査（`0LP01`）→ 書き込み。計画は循環検査を ADMIN 判定より先としたが、PostgreSQL の `AddRoleMems` に合わせ権限判定を先にして、権限の無い主体にグラフの形を見せない側へ倒した。
- **M3 循環**: オプションのビットを問わず全辺を辿る。1 文の複数の組は同じ write txn で前の組の書き込みを反映して検査する。メッセージは `0LP01` の固定接頭辞つきで、利用者が書いた名前だけを含む。
- **M4 オプション**: 新規の既定は ADMIN 偽・INHERIT は付与時点の付与先ロールの属性・SET 真（後から属性を変えても遡らない）。既存行は `WITH` で明示したビットだけ更新し、明示が無ければ何もしない。REVOKE の `OPTION FOR` はそのビットだけを落とし、無指定は行を削除する。行が無い・付与者が一致しない取り消しは警告相当で何もしない。ADMIN を失った付与者が付けた行（付与者 = その主体・対象ロールが同じ）は、RESTRICT で `2BP01`、CASCADE で連鎖削除する（主体が別経路で ADMIN を保てば残す）。連鎖は 10,000 件で `54000`。
- **M6 ACL の付与権限に継承を入れた（J5 の見直し）**: `catalog/privilege/apply.rs` は付与者の INHERIT 閉包の行と PUBLIC の和で「何らかの権限」を判定し、記録・取り消しの付与者を PostgreSQL の `select_best_grantor` と同じ方法（必要な GRANT OPTION をすべて持つ最も近いロール、無ければ最も多く持つロール）で選ぶ。閉包は書き込みと同じ txn のスナップショットで計算する。所有者相当は従来どおり。
- **M9 `CREATE ROLE ... IN ROLE a / ROLE b / ADMIN c`**: 受理し（`IN GROUP`・`USER` も同義。各句の重複は `42601`）、ロール作成と同じ write txn で付与する（`Storage::create_role_with_memberships`）。1 つでも失敗すればロールごと commit しない。句があるとき現在ロールが未束縛なら `42501`。
- **`pg_has_role` 相当**: `graph::has_role_privilege`（ID 解決済み）と `Storage::has_role_privilege_by_name`。権限文字列はカンマ区切り・大文字小文字無視で、`MEMBER`・`USAGE`・`SET`・各 `WITH ADMIN OPTION`／`WITH GRANT OPTION` の許可リストを OR で評価し、未知の語は全体を先に検証して拒否する（`InvalidPrivilege`。SQL へ結線する際に `22023` へ写す）。
- **wire のタグ**: ロール形も権限形と同じ `GRANT`／`REVOKE`（PostgreSQL 17 の CMDTAG と同じ。`GrantRevokeOutcome::is_role` は API の区別用に追加）。
- **DROP ROLE**: 付与者として参照されている行は付与者なしへ付け替える（PostgreSQL は bootstrap superuser へ付け替える）。付与者なしの行は所有者相当だけが取り消せる。

### PostgreSQL 17 との逸脱（記録）

- 付与者ごとに別の行を持つ PostgreSQL 16 以降の形はキーの制約上再現できない。別の付与者が付けた行への重複の付与は、非所有者に限り固定文言の `0A000`。
- 所有者相当の主体は付与者を問わず取り消せる（PostgreSQL のスーパーユーザーの `GRANTED BY` の代替）。
- `GRANT <ロール> TO PUBLIC` は `42704`（`role "public" does not exist` 相当。実機での確認は未実施）。
- ADMIN の判定で、ADMIN が真の行そのものに INHERIT が必要かどうかは PostgreSQL の実機で未確認のため、行そのものは INHERIT を問わない側（計画どおり）にした。
- 循環の `0LP01` のメッセージは `invalid grant operation:` の接頭辞つき。

### 対象外（後続）

- `current_user`・`current_role`・`session_user`・`user` と SQL の `pg_has_role(...)` の結線（`core.rs` の FROM なし SELECT のスコープ・Describe の型公告。判定関数と名前解決は本 Issue で用意済み）。
- `SET ROLE`／`RESET ROLE`・`ALTER ROLE [NO]INHERIT`（#1963。`can_set_role` を呼ぶ）。
- 共通ゲートでの継承の利用（#1966〜#1969）、ポリシーの `TO` ロール（#2278 段 2。`has_privs_of` を呼ぶ）、認証主体の束縛（#1971）、所有者・`GRANTED BY`（#2236）。
- INHERIT・ADMIN の喪失による ADMIN の到達性の再評価（依存の解決は ADMIN の根拠である直接の行に限る）。CASCADE 中に ADMIN の行を失った付与者は所有者相当の根拠にせず、削除の確定を固定点まで反復して評価順に依存させない。
- PostgreSQL 16 以降の付与者ごとの複数行、CREATEROLE の作成者への暗黙の ADMIN。

## 10. Issue #1963 実装記録（ALTER ROLE と SET ROLE／RESET ROLE）

挙動は PostgreSQL 17 に合わせ、SQL-39（関連 RLS-12）のポインタで対応づける。spec 本文は転記しない。

### 実装した範囲

- `ALTER ROLE`／`ALTER USER <名> [WITH] <オプション>...` と `ALTER ROLE <名> RENAME TO <新名>`。コマンドタグは `ALTER ROLE`
- `SET [SESSION] ROLE <名>|NONE`・`SET ROLE TO/= ...`・`RESET ROLE`。タグは `SET`／`RESET`
- オプションの解析は `CREATE ROLE` と共有し（`ALTER` では未指定を「変更なし」とする）、拒否規則を二重管理しない
- `SqlOutcome::AlterRole`・`SqlOutcome::SetRole`、`ParsedSql::AlterRole`・`ParsedSql::SetRole` を追加した（公開 enum への variant 追加）

### 判断の記録

| 判断 | 内容 |
| ---- | ---- |
| 未束縛セッション | `SET ROLE <名>` はカタログ照会より前に `42501`（ロールの有無を示さない）。`RESET ROLE`・`SET ROLE NONE` は何も変えず成功 |
| 名前解決 | 自テナントのカタログだけを引く。不在も他テナントにだけ在る場合も同じ `42704` |
| 認可 | `session_role_id` を起点に、`SET` 真のメンバーシップの辺だけを辿って対象に到達できれば可。自分自身は常に可。到達不能は固定文言の `42501` |
| 探索の上限 | 訪問済み集合で循環を除き、ロール数と辺の走査数に上限を置く。超過は拒否側（fail-closed） |
| 影響範囲 | `SET ROLE` が変えるのは `current_role_id` だけ。`PolicyContext`・`session_role_id`・`ddl_allowed` は変えない（テナント境界・DDL 権限ゲートは広がらない） |
| 明示 Tx | `SET ROLE` は READ ONLY を含む明示 Tx 内でも実行でき、ROLLBACK で戻る。`ALTER ROLE` は DDL 権限ゲートの後に `0A000` |
| `ALTER ROLE` の拒否 | DDL 権限ゲートが最初（`42501`）。`SUPERUSER`／`BYPASSRLS`／`REPLICATION` は `42501`。`VALID UNTIL`・`CONNECTION LIMIT`・ロール単位の `SET`・`ALL`・`IN DATABASE` は `0A000` |
| `RENAME TO` | ロール表のキーだけを移し、ID・ACL・メンバーシップは保つ。セッション／現在のロール自身の改名は `0A000` |
| パスワード | `CREATE ROLE` と同じ検証子生成（平文は `RolePassword` に閉じ込める）。`PASSWORD NULL` は検証子を空にする |

### 対象外

`CREATEROLE`／`ADMIN OPTION` による判定（#1966・#1971）、認証主体の束縛（#1971。それまで wire の実セッションでは `SET ROLE <名>` は常に `42501`）、INHERIT による継承（#2232）、`VALID UNTIL`・`CONNECTION LIMIT`・改名時のログイン名対応表への追随（#2233）、`current_user`／`SHOW role`、`SET SESSION AUTHORIZATION`、`SET LOCAL ROLE`。

## 11. Issue #1966 実装記録（権限検査の共通ゲート）

SQL-40 ポインタ。判定点を `sql/privilege_gate.rs` の `check`（主体・対象・必要権限）へ 1 本化し、DDL 経路を結線した。

### 実装範囲

- `Principal`・`DdlStatementKind`・`RequiredPrivilege`・`PrivilegeTarget` と、文種から必要権限を引く表（`required_privilege`。ワイルドカード腕なしの網羅 `match`）。
- `core.rs` の DDL 腕・`udf_ddl.rs` は文種付きの `require_ddl` を呼ぶ。`ddl::require_ddl_permission`・`grant_ddl::require_grant_principal` は名前とシグネチャを保ったファサード。`search_path.rs`・`grant_ddl.rs` の `ddl_allowed()` 直読みを除去。
- 公開 API・依存・挙動の変更なし。

### 判断の記録

- **DDL は `ddl_allowed` のビットだけで判定する**: 認証層の紐付け（#1971）が未実装で全セッションが未束縛のため、ロール・ACL を混ぜると全 DDL が拒否される。ロールの束縛は DDL 権限を広げない（回帰テストあり）。
- **ファサードを残す**: 並行する PR が `require_ddl_permission` を呼ぶため、名前とシグネチャを維持した。
- **`Some(0)` の扱い**: `search_path` の GRANT 分岐を `bound_principal` に揃え、PUBLIC の予約 ID `0` は未束縛と同じく拒否側に統一した。
- **`DropFunction` の判定位置の非対称は残す**: 判定順序を変えないため（自動コミットは実行本体側、Tx 内は腕側）。

### 対象外

ロールベースの DDL 判定（#1971・#2234・#2236）、SELECT・DML・COPY・HTTP への結線（#1967〜#1969）、Rust API の生 DDL をゲートに通すこと、`DropFunction` の非対称の解消。

## 12. Issue #1967 実装記録（SELECT・カーソル・検索経路への権限検査の結線）

SQL-40 (1)(2)(3)(6)・RLS-12 ポインタ。読み取り系の文が参照する relation に、表単位の SELECT 権限検査を結線した。
判定点は §11 の `privilege_gate::check` の 1 本のまま。参照先の収集と ACL の照会は `sql/privilege_gate/read.rs`。

### 実装範囲

- 対象の文: SELECT（JOIN・サブクエリ・CTE・再帰 CTE・集合演算・ビュー・matview・SQL 内の kNN／全文／ハイブリッド検索）、
  EXPLAIN、`DECLARE`、行ロック句つき SELECT、PL/pgSQL の埋め込み SQL（`parse_tokens_gated` 経由）、
  拡張クエリの Parse・Bind・Describe、HTTP 表層の bound 読み取り API（search・plan search・scan・aggregate・各 explain）。
- 文種の分類は `read::classify`（`ParsedSql`・`Statement` の網羅 `match`。新しい variant の追加で分類の更新をコンパイラが強制）。
- 参照先の収集は `search_path` の参照ゲートと同じ位置判定（`relation_positions`・CTE 名・構文語・表関数の除外）を共有する。
  名前解決器で修飾を解決した後のトークンを走査し、解決済みの内部キー（`#<id>.<name>`）も含める。
- `SessionState::object_privileges_enforced`（公開）を追加。wire の Bind が、束縛済みのセッションでは検証済みの文を
  使い回さず毎回再検証する条件に使う。

### 判断の記録

- **未束縛のセッションは素通し**: #1971 が未実装で本番のセッションはすべて現在ロールが未束縛のため、ACL を判定すると
  すべての SELECT が拒否される。`None` は明示の腕で素通しにし、トークンの複製・走査もしない（挙動・コストとも従来と同一）。
  #1971 で束縛されたときに自動で有効になる。`Some(0)`（PUBLIC の予約 ID）は拒否。
- **運用者権限は所有者相当**: `ddl_allowed` のセッションは全権限を持つ（所有者 #2236 まで。GRANT 側の `is_owner_equivalent` と同じ）。
- **実効権限は INHERIT 閉包への付与と PUBLIC の和**: 同じスナップショットで `catalog::role::graph::effective_roles_in_txn`
  （INHERIT の辺だけ）の閉包を求め、到達するロールへの付与と所有者の暗黙権限を合算する（`catalog/privilege.rs` の
  `relation_grant_bits_in_txn`。#2860）。INHERIT 偽の辺と他テナントには波及しない。列単位の付与だけでは表単位の SELECT を満たさない（PG より厳しい暫定。列集合の判定は #2081）。
- **判定順序は「構文 → 名前解決 → 権限」**: 収集は構文解析の成功後に行う。他テナントの SCHEMA 配下の表は解決の段で不在と
  同じ応答になり、権限の分岐に入らない（D10）。
- **ビューは自身の SELECT だけを判定する**: ビュー本文の再検証は内部の parse で行われ最上位のゲートを通らない。基底表は
  ビュー所有者の権限で読む扱い（ビューを作れるのは所有者相当の主体だけで、PG の意味と一致する）。
- **カタログで relation として引けない名前は検査しない**: 存在しないものは読めないので fail-open にならない。
  別名などが偶然実在する表名と一致して余分に検査されるのは拒否側に倒れるだけ。
- **カーソルは DECLARE で検査し FETCH では再検査しない**: 結果は DECLARE の時点で確定する（[sql-cursor.md](sql-cursor.md) §1）ため、
  FETCH は確定済みの行を払い出すだけで新しい読み取りをしない。DECLARE の後に REVOKE や SET ROLE があっても開いている
  カーソルには影響しない（PG と同じ）。
- **拡張クエリは Parse でも検査する**: PG は Parse では検査しない意図的な差。権限の無い主体に列の型情報（Describe）を返さない
  fail-closed を優先し、Parse と Execute の間の REVOKE は Bind の再検証で反映する。
- **行ロック句は SELECT に加えて UPDATE を要求する**: ロック強度によらず、`OF` で絞られた表だけでなく参照先の全表に要求する（PG より厳しい側）。
- **`EXPLAIN ANALYZE` は構文として受理されない（`42601`）**: 計画を返す形は `EXPLAIN` だけで、検査は対象の文と同じ。
- **エラーは固定文言の `42501`**: 表名・ロール ID・テナントを含めない。明示 Tx 内の `42501` は Tx を `Failed` にする。
  EXPLAIN・Describe は計画や列情報を返す前に拒否する。
- **参照数の上限**: 重複除去後 256 件を超える文は ACL の照会より前に `PayloadTooLarge` で拒否する。

### 対象外

列集合の判定（#2081）、DML の読み取り側と書き込み・分割実行・DML の EXPLAIN（#1968。本書の収集関数を再利用する）、COPY（#1969）、
継承メンバーシップ（#2860 のマージ後）、所有者（#2236）、SCHEMA の USAGE（#2234）、シーケンス・関数の権限（#2235）、
認証済みユーザーとロールの紐付け（#1971。これが入るまで本番セッションでは検査が有効にならない）、
主体を持たない engine の Rust API（`VectorCore::search`／`get_row`、`parse_sql` ＋ `execute_parsed_*`、`describe_parsed_*`）、
存在オラクルの網羅試験（#1972）。

## 13. Issue #2234 実装記録（SCHEMA 単位の GRANT／REVOKE と USAGE 検査）

ビヘイビア ID ポインタ: SQL-40・RLS-12（spec 本文は転記しない）。意味論は PostgreSQL 17 の公開挙動と D5・D8・D10、
[schema-tenant-namespace.md](schema-tenant-namespace.md) D10 に従う。

### 実装した範囲

- 構文（`sql/allowlist/grant.rs`）: `GRANT／REVOKE {USAGE|CREATE|ALL [PRIVILEGES]} ON SCHEMA <名>[, ...]` と
  `GRANT／REVOKE <表の権限> ON ALL TABLES IN SCHEMA <名>[, ...]`。SCHEMA 名は裸の識別子（修飾名・列リストは `42601`）、
  SCHEMA に許されない権限は `0LP01`。
- 適用（`catalog/privilege/apply.rs`）: SCHEMA 名は書き込みと同じ txn のスナップショットで自テナントの内部 ID へ解決し
  （不在・他テナントは同じ `3F000`）、`PrivilegeObject::Schema(id)` の ACL を更新する。`ALL TABLES IN SCHEMA` は
  その txn で既に在る表・ビュー・matview だけへ展開し（後から作った表には効かない。SEQUENCE は含めない）、既存の表対象の
  判定規則（GRANT OPTION・警告・CASCADE）へ流す。展開数は走査上限（`54000`）で有界。
- 読み取りと掃除（`catalog/privilege/schema.rs`）: ロール集合と PUBLIC の行の権限ビットの和、`DROP SCHEMA`（RESTRICT・
  CASCADE）と同じ txn での ACL 行の掃除（残ると付与先ロールの `DROP ROLE` が永久に `2BP01` になるため）。
- USAGE 検査（`sql/schema_privilege.rs`・`core.rs` の `schema_resolver_for`）: 解決済みの内部キーの SCHEMA にだけ、
  「自テナント内の解決 → 権限検査」の順で行う。USAGE が無ければ `42501`。準備済み文は Bind で再検査する。

### 判断（K1〜K14 のうち主なもの）

| # | 判断 | 理由 |
| - | ---- | ---- |
| K1 | 検査対象は束縛ロールを持ち所有者相当（DDL 権限ゲート通過）でない主体だけ。未束縛・所有者相当は従来どおり検査しない | 未束縛に拒否を課すと既存の SCHEMA 利用が全て `42501` になる。権限を広げる経路は作らず、#1971 で全セッションが束縛された時点で全面適用になる |
| K2 | `public`・`pg_catalog`・`information_schema`・`pg_temp` への `ON SCHEMA`・`ALL ... IN SCHEMA` は構文段の `0A000` | 行・内部 ID を持たない仮想 SCHEMA。状態に依存しない拒否 |
| K3 | 他テナントの SCHEMA は解決器が先に `None`（`3F000`）にし、権限検査の分岐は走らない | 権限の有無が存在オラクルにならない（D10） |
| K4 | `search_path` 上の USAGE の無い SCHEMA は、作成先の判定（`creation_target`）では `42501`（受入基準どおり。他テナント・不在は従来どおり次の要素へ進み存在オラクルにしない）。`current_schema()`・`current_schemas()` の値の列挙では見えないものとして飛ばす。修飾名も `42501` | 受入基準の「`search_path` 経由も `42501`」に合わせる。値の列挙は権限エラーにせず可視な SCHEMA だけを返す。未修飾名は現状テナント SCHEMA へ解決されない |
| K5 | 作成先が SCHEMA の作成系 DDL は CREATE 判定を `require_creation_target` に結線するが、`ServerDdl` ゲートを必要条件として残す | `ServerDdl` を CREATE に置き換えるとロールの束縛だけで DDL 権限が広がる。所有者の機構（#2236）・主体の束縛（#1971）の後 |
| K7 | 修飾名の作成位置でも USAGE を要求する（PostgreSQL は CREATE だけで足りる。拒否側の意図的な差） | 解決器は位置を区別しない。権限を広げない |
| K9 | `ALL SEQUENCES／FUNCTIONS／ROUTINES／PROCEDURES IN SCHEMA` は `0A000` のまま（単一の `ON SEQUENCE`・`ON FUNCTION` は #2235 で対応済み。`ALL ... IN SCHEMA` は後続。§15 S11） | SEQUENCE・FUNCTION 単位の ACL 適用は #2235 の中核 |
| K10 | `DROP SCHEMA` の同じ txn で ACL 行を掃除（commit 呼び出しは増やさない） | `DROP ROLE` の依存検査・件数上限の消費を防ぐ |
| K11 | ビュー本文（保存済みの解決済みキー）経由の参照は USAGE を再検査しない | PostgreSQL はビューの所有者の権限で参照する。所有者の機構（#2236）まで対象外 |
| K12 | 準備済み文は Bind でトークン列の `#<id>.` キーから SCHEMA ID を集めて USAGE を再検査 | Parse 後の `REVOKE` を Execute に反映する |
| K13 | 明示トランザクション内は現在ロールのゲート（`42501`）の後で `0A000`（J9 と同じ） | ACL の書き込みは独立した write txn で ROLLBACK で取り消せない |

### 既知の差・対象外

- 受入基準の「`search_path` 経由も `42501`」は、作成先の判定で `42501` を返すことで満たす（K4）。PostgreSQL 17 は path 上の
  SCHEMA を飛ばすため、この点は PostgreSQL と異なる（受入基準を優先した意図的な差）。
- `GRANT ... ON SCHEMA` の実行は、SCHEMA の所有者の概念が無いため所有者相当（DDL 権限ゲート通過）か、対象に GRANT OPTION を
  持つ束縛ロールだけ（#2236 まで）。継承（#2232）は USAGE 判定のロール集合に未反映（直接の行と PUBLIC の行だけ）。
- 対象外: SEQUENCE・FUNCTION 単位の ACL（#2235）、所有者・PUBLIC の既定（#2236）、`ALTER DEFAULT PRIVILEGES ... IN SCHEMA`
  （#2237）、表・列権限の参照時検査（#1967〜#1969）、権限検査の共通ゲートへの統合（#1966。判定は `sql/schema_privilege.rs`
  に集約済みで、ゲートへ移す）、存在オラクルの網羅試験（#1972）。

## 13. Issue #2233 実装記録（カタログのロールで SCRAM-SHA-256 認証する）

SQL-39 (3)(7)・WIRE-18 ポインタ。ロールのカタログ（#1961）・`CREATE`／`ALTER ROLE`（#1962・#1963）の上に、認証側の照合と `VALID UNTIL`・`CONNECTION LIMIT` を実装した。

### 実装範囲

- **ログイン識別子**: `tenant_id` から決定的に導出した修飾子（`hex(SHA-256(ドメインタグ ‖ tenant_id))` の先頭 16 バイト。小文字 16 進 32 文字）と、ロール名から D1 の符号化で作る。ドメインタグ（`fandhe-db/login-qualifier/v1`）は変えない。運用者は `wire-server login-identifier --tenant <id> --role <name>`（DB を開かない）で値を得て、利用者に接続の user として渡す。
- **対応表の追随**: `CREATE ROLE` で登録、`DROP ROLE` で解除、`ALTER ROLE ... RENAME` で旧識別子の解除と新識別子の登録を、いずれもロールと同じ write txn で行う。起動時に `EngineCore::sync_login_names()` が全テナントを走査して未登録分を登録する（冪等・件数上限つき。失敗なら起動を拒否）。
- **認証**: 接続の user が識別子の正規形ならカタログの経路（SCRAM 方式のときだけ）、そうでなければ users ファイルの経路（無変更）。照合は既存の SCRAM 実装。接続のテナントは対応表の blob とロール表のキーから導出し、クライアントの申告は使わない。
- **失敗の収束**: ロール不在・`VALID UNTIL` 超過・検証子なし・カタログ読み取り失敗・検証子の解析失敗は、すべてモック検証子（`success == false`）に畳み込み、同じ `28P01` と `AUTH_FAILURE_DELAY` に揃える。期限切れでも server-first の salt はモックのもの。
- **検証後の判定**: パスワードの検証が通り `AuthenticationOk` を送った後に、`NOLOGIN` なら FATAL `28000`、`CONNECTION LIMIT` 超過なら FATAL `53300`（`BackendKeyData`・`ReadyForQuery` は送らない。文言は固定でテナント・内部 ID を含まない）。接続数は `UserStore` が持つ `RoleConnectionRegistry`（RAII ガード）で数える。`-1` は無制限、`0` はすべて拒否。
- **SQL 表層**: `VALID UNTIL '<文字列>'`（セッションの TimeZone で UTC マイクロ秒へ。`'infinity'` は期限なし。解析不能は `22007`／`22008`）と `CONNECTION LIMIT <整数>`（-1 未満・`i32` 範囲外は `22023`、整数でなければ `42601`）を `CREATE`／`ALTER ROLE` で受理する。`ALTER` は指定した項目だけを変える。
- **セッション**: 認証したロールを `bind_role_ids(role_id, role_id)` で束縛し、`set_session_user` には修飾子なしのロール名を渡す。`ddl_allowed` は偽のまま（カタログのロールの DDL 権限は #1971・#2234・#2238 の担当）。

### 判断の記録

- **D1 の置き換え（NOLOGIN も対応表へ載せる）**: D1 は NOLOGIN を対応表に載せないとしていたが、「パスワード検証の後に `28000`」を満たすには NOLOGIN のロールでも検証子を引ける必要があるため、全ロールを登録する。`resolve_login_in_txn` は `LOGIN` 属性を検査せず、判定は認証層が検証の後に行う。
- **修飾子は永続の表を持たない**: 修飾子は `tenant_id` から導出するので、新しい表も採番も要らない。衝突は 128 ビットで実質起きず、万一起きても対応表の重複登録拒否で fail-closed になる。
- **解決は点の参照のみ**: 認証経路ではテナント内の走査をしない。未登録の識別子でもロール表の点参照を 1 回行い、読む回数をそろえる。残る時間差はマイクロ秒単位で、users ファイルの HashMap 参照との差と同程度。
- **経路の振り分けは入力の形だけ**: 識別子の正規形かどうかは存在のオラクルにならない。cleartext 方式ではカタログの識別子は users ファイルに存在できず、未知ユーザーと同じ `28P01` になる。
- **改名は旧識別子を無効にする**: PostgreSQL と同じく、旧名ではログインできなくなる。

- **証明の検証後の再取得**: client-final 待ちの間に属性が変わりうるため、検証成功後・`AuthenticationOk` 前にロールを再取得し、ロール ID・テナント・検証子の一致と `VALID UNTIL` を確認する（不一致・不在・読み取り失敗は通常の `28P01`）。`LOGIN`・`CONNECTION LIMIT` は再取得した最新値で判定する。
- **日付だけの `VALID UNTIL`**: `'2030-01-01'` はセッション TimeZone の当日 0 時として受理する。

### 対象外

HTTP `POST /v1/session` でのカタログのロールの照合、cleartext 方式でのカタログのロールの照合、users ファイルのユーザーとロールの対応づけ（#1971）、ブートストラップ専用化（#2238）、カタログのロールの DDL 権限、メンバーシップ（#2232）、存在オラクルの網羅試験（#1972）、`current_user`・`session_user` の値。

## 14. Issue #1971 実装記録（認証済みユーザーをカタログのロールへ紐付ける）

SQL-39・WIRE-2 ポインタ。D3・D4 のうち、users ファイルのユーザーをカタログのロールへ対応づけて `SessionState` へ束縛する部分を実装した。

### 実装範囲

- **対応規則**: users ファイルで認証したユーザーは、その行のテナント内の同名ロールへ対応づける（`ROLES_TABLE` の点参照・バイト単位の完全一致・大文字小文字の畳み込みなし）。テナントは users ファイルからだけ導出し、他テナントの同名ロールは構造上引けない。識別子として不正な名前は不在として扱う。対応できたら `bind_role_ids(id, id)` と `set_session_user` で束縛する。
- **カタログが正のテナント**: 継ぎ目として「テナントにロールが 1 件以上ある」で判定する（`catalog::role::resolve_tenant_role_in_txn`。#2238 のマーカー判定へ差し替える。D11 の移行規則と矛盾しない）。同名ロールが無いユーザーは `28P01`、ロールが 0 件のテナントは従来どおり未束縛。「ロールの有無」と「同名ロールの点参照」は同じ read txn で行う（判定のぶれを防ぐ）。破損は 0 件と取り違えず認証失敗に倒す。
- **応答の収束**: SCRAM は証明の検証が成功した後に対応づけを判定し、失敗は誤パスワードと同じ `success = false`（同じ `28P01`・同じ遅延・同じ応答バイト）に畳む。cleartext は照合の前を起点にし、対応づけの失敗でも `AUTH_FAILURE_DELAY` まで待つ。`VALID UNTIL` 超過は `28P01`、`NOLOGIN` は検証後の `28000`、`CONNECTION LIMIT` 超過は `53300`（#2233 と同じ後段）。
- **`--ddl-allowed-users` との優先関係**: カタログを正とし、食い違いは拒否側に倒す。DDL 権限は users ファイルの経路にだけ付き（ログイン識別子経由の主体には付けない）、束縛しても DDL 権限は広がらない。指定ありで同名ロールが無ければ接続できない。ブートストラップロールへの写像の移行は #2238。
- **`engine: None`** の経路は従来どおり未束縛。

### 移行の注意

テナントに最初のロールを作ると、そのテナントの users ファイルのユーザーは同名の `LOGIN` ロールが無ければ接続できなくなる。先に `CREATE ROLE <ユーザー名> LOGIN` を作ってから他のロールを作る。

### 対象外

HTTP `POST /v1/session` での対応づけ（HTTP-4。#2238。`auth::users_file_role::verify_and_bind` は再利用できる形にしてある）、ブートストラップのマーカー・初期ロールの投入、パスワードの照合元をカタログの検証子へ切り替えること（#2238）、`CREATEROLE`／`ADMIN OPTION` の判定、メンバーシップ（#2232）、存在オラクルの網羅試験（#1972）。

## 15. Issue #2236 実装記録（オブジェクト所有者と PUBLIC）

SQL-40・RLS-12 ポインタ。表・ビュー・matview・SCHEMA・関数に所有者ロールを持たせ、`ALTER ... OWNER TO` と所有者による
DROP・ALTER・GRANT を実装した。

### 実装範囲

- **保存**: 新しい表 `object_owners`（遅延作成）。キーは `(object_kind, object_key, tenant_id)`、値は版 1 の blob（所有者の
  ロール ID）。kind と key は ACL と同じ規則（relation は relation キー、SCHEMA・関数は内部 ID の 10 進正規形）。relation は
  全テナント共有カタログにあるため所有者はテナントごとの行で、`tenant_id` を末尾に置いて DROP・改名の掃除を範囲走査にした。
  読み書きは 3 成分の完全一致で、他テナントの行は引けない。判定の唯一の関数は `catalog::privilege::owner::is_owner_in_txn`。
- **作成者の記録**: `CREATE TABLE`・CTAS・`CREATE VIEW`・`CREATE MATERIALIZED VIEW`・`CREATE SCHEMA`・`CREATE FUNCTION`
  の成功時に、束縛中の現在ロールを所有者として記録する（`OR REPLACE` は既存の所有者を変えない）。未束縛のセッションは
  記録せず、所有者の無い対象は運用者権限でしか扱えない。`CREATE SCHEMA ... AUTHORIZATION <ロール>` は指定ロールを所有者にする。
- **所有者による DROP・ALTER**: `DROP SCHEMA`・`ALTER SCHEMA ... RENAME`・`DROP FUNCTION` は `ServerDdlOrOwner`。運用者権限が
  無い束縛ロールは、書き込み txn の中で所有者を検査し、非所有者は `42501`。ゲートは `GateDecision::{Allowed, OwnerCheckRequired}`
  を返し、未束縛はカタログ照会より前に `42501`。
- **`OWNER TO`**: `ALTER {TABLE|VIEW|MATERIALIZED VIEW|SCHEMA} <名> OWNER TO <ロール>`。解決・判定・書き込み・ACL の
  書き換え・世代の更新を単一の write txn で行う（`Storage::alter_object_owner`）。
- **GRANT／REVOKE・SCHEMA の利用**: 付与者が対象の所有者なら所有者相当（`ddl_allowed` に加算）。SCHEMA の所有者は USAGE・CREATE を
  暗黙に持つ。
- **DROP ROLE**: そのテナントで所有者として現れるロールは `2BP01`（所有権を移してから削除する）。
- **`REASSIGN OWNED`・`DROP OWNED`**: 状態に依存しない固定の `0A000`。

### 判断の記録

| # | 判断 | 理由 |
| - | ---- | ---- |
| O1 | 所有者の権限は加算のみ。`ddl_allowed` による所有者相当は全経路で維持する | J4・K1、および読み取り・書き込み・COPY のゲートの前提を壊さない |
| O2 | 共有カタログの relation（表・ビュー・matview）は、所有者が行えるのを ACL 管理（GRANT／REVOKE）と `OWNER TO` に限る。構造を変える DROP・ALTER は `ServerDdl` のまま | D6。共有定義の変更は他テナントへ波及する |
| O3 | テナント別定義（SCHEMA・関数）の DROP・ALTER は「運用者権限または所有者」 | D6 |
| O4 | 判定順序: 運用者権限ならカタログ照会なしで許可。未束縛で運用者権限も無ければ照会より前に `42501`。束縛ロールは自テナントで解決し（不在・他テナントは従来どおり `3F000`／`42883`）、その後に所有者を検査 | D10。検査と書き込みを同じ txn にして TOCTOU を防ぐ |
| O5 | 所有者行の無い対象はどのロールの所有にもならない（ブートストラップロールの合成はしない） | 初期ロール所有は #2238 の担当。拒否側 |
| O6 | 作成者の記録は作成と同じ write txn で行う。作成直前に予約（スレッドローカル）を置き、作成本体が「新規に作った」ことを確認できた txn の中で所有者行を書く（別 txn だと DROP・同名再作成と競合し、作り直された対象へ元の作成者を所有者として付けるため）。保存時に所有者ロールの現存も同じ txn で検査する。予約が無い（未束縛）作成は所有者を持たず、O5 により拒否側に倒れる | 作成経路が多く、全経路の引数へ所有者を通すと公開 API の変更が大きい |
| O7 | SCHEMA の所有者も `object_owners` に置く（kind=SCHEMA・内部 ID キー）。D6 が示した SCHEMA blob の予約領域は使わない | 改名で blob を移す処理と旧バイナリの互換を増やさない。内部 ID は再利用されず、改名で変わらない |
| O8 | `OWNER TO` の実行者は運用者権限または現在の所有者。所有者が実行する場合は、新所有者へ SET 可能であること、対象がテナント SCHEMA 配下なら新所有者がその CREATE を持つことを要求する。運用者権限はこの 2 点を免除する。未束縛は照会より前に `42501`（運用者権限があっても束縛が要る） | PostgreSQL 17 と同じ制約 |
| O9 | 新所有者は自テナントのカタログだけで解決する。不在と他テナントにだけ在る場合は同じ `42704` | D10 |
| O10 | `OWNER TO` は、そのテナントの ACL 行（表なら列行も）の旧所有者を新所有者へ置き換える（grantor・grantee の両方。衝突はビットの OR。自己付与になる行は削除） | 旧所有者の付与が取り消せないまま残る fail-open を防ぐ |
| O11 | GRANT／REVOKE は、付与者が対象の所有者なら所有者相当（`apply.rs` の対象ごとの判定）。付与者は従来どおり現在ロール | O1 |
| O12 | SCHEMA の USAGE・CREATE の判定（`schema_privileges_in_txn`）に、所有者の暗黙の権限を足す | 所有者は暗黙に全権限を持つ |
| O13 | 運用者権限の無い所有者の `DROP SCHEMA ... CASCADE` は、配下に共有カタログの relation（表・ビュー・SEQUENCE）があれば `42501` | D6。PostgreSQL との意図的な差 |
| O14 | `REASSIGN OWNED`・`DROP OWNED` は構文段で固定の `0A000` | 状態に依存しない拒否は存在オラクルにならない |
| O15 | PUBLIC への既定付与（関数の EXECUTE など）は入れない | 関数の EXECUTE は #2235、既定権限は #2237 の担当 |
| O16 | 明示トランザクション内の `OWNER TO` と `DROP FUNCTION` は、ゲート（`42501`）の後で `0A000` | 独立した write txn で commit され ROLLBACK で取り消せない（J9・K13 と同じ） |
| O17 | `DROP ROLE` の依存検査（`role_has_dependents_in_txn`）に所有者を加える | §9 の拡張点 |

### PUBLIC の境界

`GRANT ... TO PUBLIC` は ACL の `grantee_id = 0` の行で、キーの先頭が `tenant_id` のため、同じテナントのロールにだけ作用する。
他テナントのロールにも、他テナントの同名 SCHEMA（内部 ID が別）にも及ばない（`tests/table_rls/sql40_object_owner.rs`）。

### 既知の差・対象外・申し送り

- 受理しない `OWNER TO`: `ALTER SEQUENCE`・`ALTER FUNCTION`・`ALTER PROCEDURE`・`ALTER ROUTINE`・`IF EXISTS`・`ALTER TABLE` の
  複数アクション（`0A000` または構文エラー）。SEQUENCE と暗黙の SEQUENCE（SERIAL・IDENTITY）の所有者は未実装で、表の所有者の移譲に追随しない。
- 所有者に基づく INDEX・TRIGGER・POLICY・TYPE の DDL 判定、`REFRESH MATERIALIZED VIEW` の所有者・MAINTAIN 判定、テナント SCHEMA 内の
  relation の所有者による DROP・ALTER は `ServerDdl` のまま。行セキュリティの `user_policies_apply(is_owner)` は実際の所有者へ結線していない
  （所有者にも RLS がかかる拒否側）。
- メンバーシップ経由の所有者権限（#2232）と、読み取り・書き込み・COPY のゲート（#1967〜#1969）への所有者の組み込みは、それぞれの
  Issue が入った時点で `is_owner_in_txn`（権限の集約点）を広げる形で結線する。
- J4 の残存制約（別の運用者が付けた付与を REVOKE できない）は変わらない。
- PostgreSQL との意図的な差: O13、および SCHEMA の改名・所有者変更でデータベースの CREATE 権限を検査しないこと。
- 既存オブジェクト（改訂前に作られたもの）の所有者は未設定のまま（O5）。初期ロールへの移行は #2238。

## 16. Issue #2235 実装記録（SEQUENCE と FUNCTION 単位の GRANT／REVOKE）

ビヘイビア ID ポインタ: SQL-40（特に (6)(a)）・RLS-12（spec 本文は転記しない）。意味論は PostgreSQL 17 の公開挙動と
D5・D8・D10、§12（#2234）の K1〜K13 に揃える。

### 実装した範囲

- 構文（`sql/allowlist/grant.rs`・`sql/relation_name.rs`）: `GRANT／REVOKE {USAGE|SELECT|UPDATE|ALL [PRIVILEGES]} ON SEQUENCE <名>[, ...]` と
  `GRANT／REVOKE {EXECUTE|ALL [PRIVILEGES]} ON FUNCTION <名>[(<型>, ...)][, ...]`。SEQUENCE は表と同じ解決器で SCHEMA 修飾を
  内部キーへ、関数名は `DROP FUNCTION` と同じ規則（括弧の有無に依らず関数位置・存在しない SCHEMA は `3F000`）で解決する。
  型の並びは `DROP FUNCTION` と共有のヘルパー（`parse_function_arg_types`）で読む。許されない権限は `0LP01`、列リストは `42601`。
- 適用（`catalog/privilege/apply.rs`）: SEQUENCE・関数は書き込みと同じ txn で内部 ID へ解決する（SEQUENCE の不在 `42P01`・表を指すと
  `42809`、関数の不在・型の不一致・他テナントの関数は同じ `42883`）。既存の GRANT OPTION・警告・CASCADE／RESTRICT の規則へ流す。
- 判定（`sql/object_privilege.rs`）: SEQUENCE は any-of（`nextval`=USAGE|UPDATE、`currval`・`lastval`=USAGE|SELECT、`setval`=UPDATE）。
  関数は文の実行区間の頭で張る拒否集合（`FunctionAclScope`）を、式束縛の宣言的 UDF 展開（`udf_call::bind_call`）と
  `LANGUAGE sql|plpgsql` の解決（`routine_fold`・`function_scan::plan`）で引く。
- 掃除: SEQUENCE の ACL 行は `drop_sequence_in_txn`・`drop_owned_sequences_in_txn`（DROP SEQUENCE・所有表の道連れ・
  `DROP SCHEMA ... CASCADE`）の同じ txn で全テナント分を消す。関数の行と印は `remove_function_in_txn` で消す。

### 判断（S1〜S14）

| # | 判断 | 理由 |
| - | ---- | ---- |
| S1 | 検査対象は `acl_checked_principal` が `Some` の主体（束縛済みかつ所有者相当でない）だけ。未束縛・所有者相当・セッションを持たない Rust API 経路は検査しない（K1 と同じ） | 未束縛に課すと既存の採番・関数呼び出しが全て `42501` になる。#1971 で全セッションが束縛された時点で全面適用になる |
| S2 | SEQUENCE の判定は any-of（いずれか 1 ビット）。all-of の `check_schema_privilege` は流用しない | PostgreSQL の `nextval_internal`・`currval`・`setval` と同じ |
| S3 | DEFAULT 採番の免除は「SEQUENCE が書き込み先の表のその列に所有されている」とき（SERIAL・IDENTITY・`OWNED BY` 済み）だけ。所有されない SEQUENCE を参照する利用者の `DEFAULT nextval('s')` と VALUES 内の明示の `nextval`／`currval` は検査する。`assign_pseudo_id`（移行済み表の所有 SEQUENCE）は免除 | 受入基準 (A2) と SQL-40 (6)(a)。PostgreSQL は SERIAL でも USAGE を要求するが仕様を優先する（既知の差） |
| S4 | 判定順はテナント内の解決を先に、権限を後に（D10）。SEQUENCE は `42P01` → `42501` → `22003`（setval）／`55000`（currval）。関数は `42883` → `42501`。他テナントの SCHEMA 修飾は解決器が先に `3F000` | 権限の有無を存在オラクルにしない |
| S5 | 関数の既定（PUBLIC が EXECUTE）は PostgreSQL の「proacl が NULL」と同じ暗黙の既定。遅延作成表 `acl_init`（キー `(tenant_id, object_kind, object_key)`、値は版 1 バイト）の印を「ACL 実体化済み」の目印にする。印が無ければ PUBLIC が EXECUTE を持つ。印が無いのに行がある場合は `CorruptSchema`（fail-closed）。印の無い関数への初回の GRANT／REVOKE は暗黙の既定（付与者は現在ロール）を作業状態へ展開してから判定し、差分があれば全行と印を同じ txn で書く | 既存の関数がそのまま誰でも呼べる（後方互換）。行だけでは「既定のまま」と「PUBLIC から取り消し済み」を区別できず、付与者自身の行で実体化を表すと後から REVOKE したとき既定へ復帰する fail-open の穴がある |
| S6 | SEQUENCE には暗黙の既定を持たせない（行が無ければ所有者相当・未束縛以外は権限なし） | PostgreSQL の既定（所有者だけ）と同じ。所有者の機構は #2236 |
| S7 | 関数の EXECUTE 検査は解決点で行う。core が文の実行区間の頭で、主体が持たない関数キーの集合（denied set。1 回の読み取り txn・関数は 1 テナント最大 64 件）を RAII のスレッドローカル `FunctionAclScope` へ張る。入れ子の入口はスコープが既にあれば何もしない | `Storage` を借用して参照点へ渡せない（unsafe 禁止）。ロード時の除外は `42501` が `42883` に化け GRANT のたびに世代 bump が要る。入れ子の展開・ビュー本文・PL/pgSQL 本体内の呼び出しも同じ点を通るので呼び出し元ロールで判定される（PostgreSQL と同じ） |
| S8 | 拒否集合は `Storage::denied_function_keys`（`catalog/privilege/function.rs`）が全カタログ関数について算出する。セッションのレジストリには ID を持たせない | `UdfRegistry`・`UdfDefinition`（pub）を変えず、作成・置換・ロードの各登録点に ID の記録を足さない |
| S9 | セッション内だけの関数（カタログに無い宣言的 UDF・WASM UDF）は ACL 対象外で GRANT の対象にもならない（`42883`） | ID が無く作成したセッション自身しか呼べない |
| S10 | トリガ関数の起動時は EXECUTE を検査しない | PostgreSQL は CREATE TRIGGER の時点で検査する。CREATE TRIGGER は ServerDdl ゲートの対象 |
| S11 | `ALL SEQUENCES／FUNCTIONS IN SCHEMA`・`PROCEDURE`・`ROUTINE`・`DATABASE` 等は `0A000` のまま | 本 Issue では単一オブジェクトの形に絞った（`ALL ... IN SCHEMA` は後続） |
| S12 | 削除への追随: SEQUENCE の行は定義の削除と同じ txn（`RESTART` が呼ぶ状態行の削除には結線しない）、関数の行と印は `remove_function_in_txn`。`CREATE OR REPLACE FUNCTION` は ID を保つので ACL を引き継ぐ | 残った行は `DROP ROLE` の依存検査を永久に `2BP01` にし、件数上限を消費する（K10 と同じ） |
| S13 | `ON TABLE <seq>` は従来どおり `42809`（PostgreSQL はシーケンスへ読み替える） | 範囲を広げない。既知の差 |
| S14 | 拒否は既存の固定文言 `InsufficientPrivilege`（`42501`）。名前・ID・ACL の断片を含めない | 存在オラクル・情報漏えいの防止 |

### 既知の差・対象外

- SERIAL 列の暗黙の採番は SEQUENCE 権限を要求しない（PostgreSQL は USAGE を要求。S3）。`ON TABLE <seq>` の読み替えなし（S13）。
- 関数の EXECUTE は、文の実行区間の頭で張るスコープで判定する。式を束縛する入口のうち、core が束縛を行う経路（簡易クエリ・
  明示 Tx・準備済み文の検索／スキャン／集計の Bind・Execute）は覆う。wire-server が core の外で行う INSERT 値・UPDATE／DELETE 述語の
  束縛（`execute_bound_insert`／`update`／`delete` 系）は core が `SessionState` を受け取らないため未結線（後続）。
  スコープが無い区間（内部経路）は検査しない。索引の式の維持（書き込み時の式評価）は呼び出し元ロールで判定される（PostgreSQL は再検査しない）。
- COPY・NoSQL・ファイル形 INSERT など採番文脈にセッションが無い経路の `DEFAULT nextval('<所有されない SEQUENCE>')` は検査しない（`acl_role` が `None`）。
- 関数の暗黙の既定を展開するときの付与者は、その時点の現在ロール（所有者の機構〔#2236〕までの暫定）。別の所有者相当の主体は
  その行を REVOKE できない（§8 の既知の制約と同じ）。
- 継承（#2232）は判定のロール集合に未反映（直接の行と PUBLIC の行だけ）。
- 対象外: 所有者の機構と付与者の記録（#2236）、`ALTER DEFAULT PRIVILEGES`（#2237）、表・列の参照時検査（#1967〜#1969）と共通ゲートへの
  統合（#1966 の後続）、`ALL SEQUENCES／FUNCTIONS IN SCHEMA`、PROCEDURE／ROUTINE、SECURITY DEFINER、存在オラクルの網羅試験（#1972）。
