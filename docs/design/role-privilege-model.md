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
| J5 | 継承（メンバーシップ経由の権限）は見ない。付与者本人の直接の行と PUBLIC の行（「何らかの権限を持つか」の判定だけ）で判定する | 継承は #2232 の担当 |
| J6 | 表と列で許す権限は `PrivilegeObject::allowed_privileges` と一致させる。対象に許されない権限（`USAGE ON TABLE` 等）と PUBLIC への GRANT OPTION は `0LP01`（`ErrorClass::InvalidGrantOperation` を追加。HTTP 射影は 400・NoSQL 表層からは到達不能） | PostgreSQL 17 と同じ SQLSTATE |
| J7 | 付与先ロールは自テナントのカタログだけで引く。不在と「他テナントにだけ在る」は同じ `42704`・同じ文言形式（`SqlSurfaceError::UndefinedRole`。利用者が書いた名前だけを含む） | D10 |
| J8 | 範囲外の形は構文段で固定の `0A000`: `ON SCHEMA`・`ON SEQUENCE`・`ON FUNCTION`・`ON DATABASE` などの対象種別・`ALL ... IN SCHEMA`（#2234・#2235）、`GRANTED BY`、`ALTER DEFAULT PRIVILEGES`（#2237。汎用 `ALTER` より前で分岐） | 状態に依存しない拒否なので存在オラクルにならない |
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
| `UNENCRYPTED PASSWORD`・`IN ROLE`・`ROLE`・`ADMIN`・`VALID UNTIL`・`CONNECTION LIMIT` | `0A000`（#2232・#2233 の担当） |
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
