# NoSQL API

`wire-server --surface nosql` が公開する 3 エンドポイント（`POST /v1/session`・
`POST /v1/session/close`・`POST /v1/query`）と、`POST /v1/query` の `op` 9 値
（`search`／`scan`／`aggregate`／`insert`／`update`／`delete`／`create_table`／
`alter_table`／`drop_table`）の JSON スキーマを利用者向けに整理する。
`update`／`delete` は `where`（単一行・`id` 完全一致形）・`filter`（述語形）の
いずれも束縛・実行結線済み（後述の各節参照）。`create_table`／`alter_table`／
`drop_table`（DDL 3 op）は
`--ddl-allowed-users` に列挙したユーザーのみ実行できる（後述の各節参照）。

**この文書の情報源はコードとテストのみ**であり、`docs/spec`（private submodule）
の本文は転記しない。参照が必要な箇所は TASK-nn・ビヘイビア ID のポインタ表記に
限る（詳細は同 ID を持つ spec 側ファイルを、アクセス権のある人が別途参照する）。
本文中の数値・上限・応答コードは実装既定値であり、将来の変更で上書きされうる
（規範文書ではない）。

## 目次

- [概要・位置づけ](#概要位置づけ)
- [転送路の共通規則](#転送路の共通規則)
- [セッション認証](#セッション認証)
- [`POST /v1/query` 共通規則](#post-v1query-共通規則)
- [op 別スキーマ](#op-別スキーマ)
- [`filter` 配列](#filter-配列)
- [`explain`](#explain)
- [応答スキーマ](#応答スキーマ)
- [SQL ↔ NoSQL 対応表](#sql--nosql-対応表)
- [エラー応答](#エラー応答)
- [curl 例](#curl-例)
- [検証コード索引](#検証コード索引)
- [spec 側への申し送り候補](#spec-側への申し送り候補)

## 概要・位置づけ

NoSQL 表層は `wire-server --surface nosql` で SQL wire（pg wire v3 互換実装）と
排他選択する、HTTP/1.1 最小サブセット上の転送路である（TASK-171・HTTP-1・
HTTP-9）。`AGENTS.md` がスコープ外とする汎用「Web API」フレームワークとは別物で、
spec で規範化された内部転送プロトコルの実装にすぎない。

エラー契約は SQL 表層と完全共有する。NoSQL 表層は新規 `wire_code` を追加せず、
`engine::error_format::ErrorClass` を HTTP 応答（ステータス・本文）へ写像するのみ
（ERR-4）。

spec ポインタ一覧: TASK-184（基盤）・HTTP-1〜13・NOSQL-1〜11・ERR-4・SQL-1〜15・
RLS-7・RLS-9・TABLE-12。

## 転送路の共通規則

`wire-server --users <path> --db <path> --surface nosql [--bind <addr:port>]
[--tls-cert <pem> --tls-key <pem> [--tls-mode require|allow]]` で起動する。
`--users`・`--db` は必須（fail-closed。省略時は起動しない）。`--bind` の既定値は
`127.0.0.1:5432`。TLS 未構成時は非ループバックアドレスへの bind を起動時に
拒否する（loopback 限定）。

`--tls-cert`／`--tls-key`／`--tls-mode` は SQL 表層（pg wire）と同じ意味で
NoSQL 表層にも適用される（Issue #968）。HTTP には `SSLRequest` のような明示
ネゴシエーションが無いため、接続受理直後の先頭バイトで TLS レコード
（`0x16`）か平文 HTTP かを判定し、TLS と判定した接続だけをハンドシェイクへ
進める。`--tls-mode require`（既定）の下では平文 HTTP 接続へ要求を解釈せず
応答なしで切断し、`allow` の下では平文・TLS の双方を受理する。TLS 構成時は
非ループバックアドレスへの bind が許可される（WIRE-9）。

要求の受理条件（いずれも接続ハンドラ層で判定し、違反はすべて `08P01`）:

- メソッドは `POST` のみ、バージョンは `HTTP/1.1` のみ
- 要求行は 4 KiB 以下
- ヘッダ部は合計 8 KiB 以下・32 個以下
- `Content-Length` は必須・一意（欠落・重複・非数字は `08P01`）
- `Transfer-Encoding` は拒否
- `Content-Type` は `application/json`（パラメータなし、または
  `charset=utf-8` 1 個のみ）。不一致・欠落は `08P01`
- 本文長は 1 MiB 以下（超過は `54000`）。UTF-8 として不正な本文は `42601`
- 要求読み取りには 30 秒のタイムアウトがある

同時接続数の上限は 64（`MAX_CONNECTIONS`）。超過接続はメインの接続枠を消費しない
拒否専用ワーカースレッドへ委譲され、通常は `503`／`53300` を返してから切断する。
ただしこの拒否ワーカー自体にも別枠の上限（`MAX_REJECT_WORKERS`＝16）があり、
拒否ワーカーの枠まで枯渇している場合はスレッドを生成せず応答を書かずに
即座に切断する（fail-closed 優先の縮退。運用上は稀）。

応答は常に `Content-Type: application/json; charset=utf-8`・`Content-Length`・
`Connection: close`・`Date` を付け、1 要求ごとに接続を閉じる。`401` 応答のみ
`WWW-Authenticate: Bearer` を追加で付ける（RFC 9110 §11.6.1 準拠）。

検証コード: `crates/wire-server/tests/http2_framing.rs`・`http3_content_type.rs`・
`http11_limits.rs`・`http12_fail_closed.rs`・`http_limits.rs`。

## セッション認証

### `POST /v1/session`

要求本文は `user`・`password` の 2 つの必須文字列フィールドのみ（未知キーは
`42601`）。

```json
{"user": "alice", "password": "pw-alice"}
```

成功時（`200`）:

```json
{"token": "<43文字の base64url 文字列>", "expires_in": 3600}
```

- トークンは 256bit（32 バイト）の CSPRNG 出力をパディングなし base64url
  （RFC 4648 §5）で符号化した固定 43 文字
- TTL は発行時刻から 3600 秒固定（スライドしない）
- 同時有効セッション数の上限は 256（超過は `53300`）
- 資格情報の照合失敗・未知ユーザーはいずれも区別せず `28P01`（`401`）で拒否し、
  Argon2id の固定遅延・ダミー KDF による対称性を維持する（存在オラクルを与えない）

### `POST /v1/session/close`

`Authorization: Bearer <token>` ヘッダが必須。本文は空、または `{}`（それ以外の
キーを持つ本文は `42601`）。

成功時（`200`）:

```json
{"closed": true}
```

ワンタイム失効のため、二重 `close`・未知トークン・期限切れトークンはいずれも
区別せず `28000`（`401`）になる。

### `Authorization: Bearer` の扱い（`/v1/query` 共通）

`Authorization: Bearer` ヘッダの欠落・スキーム不一致（例: `Basic`）・トークン
不正・未知・期限切れ・close 済みは、すべて同一の `28000`（`401`）・固定文言へ
収束する（存在オラクルを与えない設計）。

テナント文脈はセッションに束縛された内部状態からのみ導出する。要求側が
JSON キー・ヘッダ（`tenant`／`tenant-id`／`tenantid` 相当。`x-` 接頭辞・
`_`/`-` の揺れを正規化してから判定）でテナントを自己申告しても無視されず、
`42601` で拒否される。

検証コード: `crates/wire-server/tests/http4_session.rs`・`http5_query_bearer.rs`・
`http6_auth_failure.rs`・`http8_session_close.rs`。

## `POST /v1/query` 共通規則

3 パスとも要求ターゲットのバイト厳密一致でのみ受理する（クエリ文字列付き・
末尾スラッシュ・大文字小文字違いはすべて `08P01`）。

判定順序（fail-closed。この順が契約）:

1. パス上の `tenant_id` 相当マーカー（`/v1/query?tenant_id=...`・
   `/v1/query/tenant-id/...` 等）の拒否（`42601`。認証より前）
2. `Authorization: Bearer` 認証（`28000`）
3. ヘッダの `tenant_id` 相当拒否（`42601`）
4. 本文の UTF-8／JSON 構文／`op` フィールドの形（`42601`）
5. `op` 許可リスト判定（6 値の厳密一致。語彙外は `0A000`。DDL・UDF 呼び出し・
   トランザクション制御を含む）
6. op 別スキーマ検証（必須キー欠落・未知キー・型不一致・`null` は `42601`）
7. op 別の意味検証・実行（`search` は `explain: true` を通常実行より先に判定）

`op` は完全一致のみで判定する（大文字小文字の読み替え・前後空白のトリムは
しない）。`"SEARCH"`・`" search"`・`"select"`・`"explain"`・`"begin"` 等はいずれも
`0A000`。

JSON 本文の構文受理規則は `engine::json`（NOSQL-8）に従う: ネスト深さ 16 まで・
文字列 1 個あたり 1 MiB まで・配列/オブジェクトの要素数 65,536 まで、オブジェクト
の重複キーは拒否（後勝ちで無警告に上書きしない）、数値リテラルは RFC 8259 準拠
（先頭ゼロ・小数部/指数部の数字欠落は拒否）。いずれの違反も `42601`。

検証コード: `crates/wire-server/tests/nosql1_endpoint_routing.rs`・
`nosql9_op_allowlist.rs`・`nosql1_op_vocabulary.rs`・`nosql8_schema_validation.rs`。

## op 別スキーマ

以下、各 op のトップレベルフィールドと `wire_code` を表にする。値の**語彙・範囲**
（`filter[].op` の語彙、`aggregates[].fn`／`having[].op` の語彙等）は別途本節の
説明文で扱う。

### `search`

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"search"` |
| `table` | ○ | string | 識別子形状（後述） |
| `limit` | ○ | number | `1..=10000`。非整数・負値・`u32` 超過等の形状不正は `42601`、`0` または `10001` 以上の範囲外は `22000` |
| `vector` | △ | number[] | `plan` と排他かつどちらか必須 |
| `plan` | △ | string | `vector` と排他かつどちらか必須。LLM クエリ展開 |
| `hybrid` | △ | `{"text": string}` | `vector` とのみ併用可（`plan` と併用は `42601`）。疎側テキスト列は固定で `body` 列 |
| `mode` | △ | string | `"recall"`（`vector` 検索の既定）／`"precision"`。`plan` 検索で省略時は下記参照 |
| `columns` | △ | string[]（非空） | 省略時は `id`＋全実列 |
| `filter` | △ | object[] | [`filter` 配列](#filter-配列)参照 |
| `explain` | △ | bool | [`explain`](#explain)参照 |

要求例（ベクトル検索）:

```json
{"op": "search", "table": "docs", "vector": [0.1, 0.2, 0.3, 0.4], "limit": 10,
 "columns": ["id", "lang"]}
```

要求例（ハイブリッド検索）:

```json
{"op": "search", "table": "docs", "vector": [1.0, 0.0], "limit": 3,
 "columns": ["id"], "hybrid": {"text": "alpha"}}
```

要求例（クエリ展開検索）:

```json
{"op": "search", "table": "docs", "plan": "find content", "limit": 10}
```

`mode` の解決（`resolve_mode_with_planner`。優先順位: 要求の `mode` フィールド
＞ セッション変数（`SET` 相当。NoSQL 表層には対応する構文が無く、`/v1/query` は
要求ごとに既定の `SessionState` で実行されるため常に未設定）＞ プランナー推定
＞ 既定 `recall`）:

- `vector` 検索: `mode` 省略時は常に既定 `recall`（プランナーを経由しないため
  推定ヒントが存在しない）
- `plan` 検索: `mode` 省略時はクエリ展開（LLM プランナー）の推定結果
  `mode_hint`（TASK-164・PLAN-11）が採用されうる。`mode_hint` が
  `"precision"` と推定されれば `mode` を明示指定しなくても `precision`
  モードで実行される（確信度ゲート・`explain` での `mode_source` 確認は
  SQL 表層の `USING PLAN` と同一契約）

応答例は [応答スキーマ](#応答スキーマ)を参照。

主な `wire_code`:

- `vector`／`plan` 両方指定・両方欠落・`plan`＋`hybrid` 併用・`columns: []`・
  識別子形状不正 → `42601`
- 未知テーブル → `42P01`
- 未知列・`VECTOR` 列でない列への `ORDER BY` 相当・非有限ベクトル要素等 → `22000`

`sort`（`scan`・`aggregate` op が持つ、決定的な並べ替え指定。後述）は
本スキーマに宣言していないため、指定すると未知キー `42601` になる（ベクトル
順位付けとの相互排他。NOSQL-15・SQL-25 (a)・Issue #946）。

### `scan`

順序保証なしの広域取得（SQL-15 の bare 形 `SELECT ... [WHERE ...] LIMIT n` と
同一実行意味論。`limit` 件到達で早期終了・取得モード非適用）。`sort` を指定
すると SQL 表層のスカラー `ORDER BY`（SQL-25 (a)）と同一の決定的な順序になる
（Issue #946・NOSQL-15・TASK-224）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"scan"` |
| `table` | ○ | string | |
| `limit` | ○ | number | `1..=10000`。非整数・負値・`u32` 超過等の形状不正は `42601`、`0` または `10001` 以上の範囲外は `22000` |
| `offset` | △ | number | `0..=10000`。非整数・負値・`u32` 超過等の形状不正は `42601`、`10001` 以上の範囲外は `22000`。省略時の既定値は `0`（`0` を明示指定した場合と等価） |
| `filter` | △ | object[] | |
| `columns` | △ | string[]（非空） | 省略時は `id`＋全実列 |
| `explain` | △ | bool | [`explain`](#explain)参照。`true` は `QUERY PLAN` を返す。`false`／省略時は通常実行 |
| `sort` | △ | object[]（`{"column","dir"}`。非空、上限 8 要素） | `dir` は `"asc"`／`"desc"`（小文字完全一致）。省略時は順序保証なし |

`vector`／`plan`／`mode`／`hybrid` はスキーマが宣言しないフィールドのため、
未知キーとして `42601` になる（`scan` への付与自体を個別に判定するロジックは
持たない）。`offset` も同じ理由で `search` へ付与すると `42601` になる
（Issue #947・NOSQL-15）。`aggregate` の `offset` は `group_by` 付きに限り別途
受理する（[`aggregate`](#aggregate) 参照。Issue #1198）。

要求例:

```json
{"op": "scan", "table": "docs", "limit": 10, "offset": 20, "columns": ["id", "lang"],
 "filter": [{"column": "lang", "op": "eq", "value": "ja"}]}
```

要求例（`sort` 指定）:

```json
{"op": "scan", "table": "docs", "limit": 10, "columns": ["id", "lang"],
 "sort": [{"column": "lang", "dir": "desc"}, {"column": "id", "dir": "asc"}]}
```

応答には `score` 列相当が一切含まれない（`ORDER BY`／`hybrid` を経由しないため
合成スコア列が構造上存在しない。`sort` 指定〔スカラー `ORDER BY`〕でも同様）。

#### `offset`（ページング）と `sort` 未指定時の意味論

`offset`（Issue #947・NOSQL-15・TASK-224）は SQL 表層の広域取得
`LIMIT n OFFSET m`（SQL-25 (b)）と同一の実行計画へ写像する（第 2 の実行器は
作らない）。`sort`（NOSQL-15。Issue #946）を指定していない場合でも `offset`
は拒否されない——SQL 表層が `ORDER BY` なしの `LIMIT n OFFSET m` を受理する
契約とパリティを保つためで、この場合の `offset` は物理走査順の上で適用され、
値による意味的な順序ではない。同一スナップショット内では決定的だが、
ページ取得の合間に書き込みがあると行の重複や欠落が起こりうる。安定した
ページングが必要な場合は `sort`（`id` をキーにする）の導入後に組み合わせる
ことを推奨する。詳細は `docs/design/sql-offset-paging.md`「`ORDER BY` なし
`OFFSET` の意味論」節を参照（本節はその要約のみで内容を重複しない）。

`explain: true`（[`explain`](#explain) 参照。Issue #948・NOSQL-16）は `sort`・
`offset` を指定した場合も同じ束縛（`PreparedScan::bind`）を経由するため
併用できる。

`sort` の主な `wire_code`:

- `sort` が空配列・非オブジェクト要素・`column`／`dir` 欠落・`dir` が
  `"asc"`／`"desc"` 以外（大文字混じりを含む）・`column` の識別子形状不正
  → `42601`
- `sort[].column` が未知列、または `VECTOR`／`ARRAY`／`BYTEA`／`JSON`／
  `JSONB` 列（並べ替え不能な型） → `22000`
- `sort` の要素数が 8 を超える → `54000`（HTTP `413`）

### `aggregate`

単一行集計（`GROUP BY` なし）と `GROUP BY`／`HAVING` 集計の両方を、SQL テキストを
一切組み立てずに束縛・実行する（SQL-13・SQL-14 と同一の実行計画）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"aggregate"` |
| `table` | ○ | string | |
| `aggregates` | ○ | object[]（`{"fn","column"}`。1〜32 要素） | `fn` は `count`／`sum`／`avg`／`min`／`max`（小文字完全一致）。`column` は列名、または `count` 専用の `"*"` |
| `filter` | △ | object[] | |
| `group_by` | △ | string \| string[]（配列は 1〜8 要素。`engine::sql::allowlist::MAX_GROUP_BY_COLUMNS`） | 単一文字列形 `"lang"` は 1 要素配列 `["lang"]` と完全に同じ扱い（NOSQL-16 (b)。Issue #1198）。`TEXT`・`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列（Issue #1183。数値キーは昇順・NULL 末尾・`-0.0` と `0.0` は同一グループ） |
| `having` | △ | object[]（`{"fn","column","op","value"}`） | `group_by` 必須。`op` は `=`／`<`／`<=`／`>`／`>=` の完全一致 |
| `sort` | △ | object[]（`{"column","dir"}`。非空、上限 8 要素） | `group_by` 必須。`column` は集計結果の出力列名（`group_by` 列名、または集計項目の既定エイリアス `count`／`sum`／`avg`／`min`／`max`）。`dir` は `"asc"`／`"desc"`（小文字完全一致）。同値はグループキー順。SQL の `GROUP BY ... ORDER BY` と同一結果（Issue #1198・NOSQL-15） |
| `offset` | △ | number | `group_by` 必須。`0..=10000`。ソート後の結果から先頭 `offset` グループを読み飛ばす（RLS 適用後の可視グループのみが対象）。`sort` 省略時はグループキー昇順の上で適用（Issue #1198・NOSQL-15） |
| `explain` | △ | bool | [`explain`](#explain)参照。`true` は `QUERY PLAN` を返す（`group_by`／`having`／`sort`／`offset` 付きでも受理）。`false`／省略時は通常実行 |

要求例（単一行集計）:

```json
{"op": "aggregate", "table": "docs",
 "aggregates": [{"fn": "count", "column": "*"}, {"fn": "sum", "column": "id"}]}
```

要求例（`GROUP BY`／`HAVING`）:

```json
{"op": "aggregate", "table": "docs",
 "aggregates": [{"fn": "count", "column": "*"}],
 "group_by": ["lang"],
 "having": [{"fn": "count", "column": "*", "op": ">=", "value": 2}]}
```

要求例（単一文字列形 `group_by`＋`sort`＋`offset`）:

```json
{"op": "aggregate", "table": "docs",
 "aggregates": [{"fn": "count", "column": "*"}],
 "group_by": "lang",
 "sort": [{"column": "count", "dir": "desc"}],
 "offset": 1}
```

主な `wire_code`:

- `aggregates` が空配列 → `group_by`／`having` の有無を問わず一律 `42601`
  （`having` の参照解決より必ず先に検査する）
- `group_by` 要素数が 0・`group_by` なしの `having`／`sort`／`offset`
  （`offset: 0` の明示を含む。SQL 表層に単一行集計への `ORDER BY`／`OFFSET` の
  受理形がなく、黙って無視すると fail-open になるため）・
  `fn`／`op` が語彙外・識別子形状不正（`group_by` の空文字列を含む）→ `42601`
- `sort` が空配列・非オブジェクト要素・`column`／`dir` 欠落・`dir` が語彙外・
  `column` が識別子形状不正（`"*"` を含む）→ `42601`。要素数が 8 超過 → `54000`
  （HTTP `413`）。`column` が出力列名に存在しない・複数の出力列に一致
  （例: `count(*)` と `count(lang)` を併記して `"count"` を指定）→ `22000`
- `offset` が非整数・負値・`u32` 超過 → `42601`、`10001` 以上 → `22000`
- `group_by` 要素数が 8（`MAX_GROUP_BY_COLUMNS`）超過・グループ数上限
  （10,000）・グループキー累計バイト・`having` 述語数上限超過 → `54000`
- `group_by` 列が `TEXT`／数値列（INTEGER／BIGINT／REAL／DOUBLE）でない・`having` が `MIN`/`MAX(<TEXT列>)` を参照・
  参照先が `aggregates` に存在しない／曖昧 → `22000`
- `VECTOR` 列の集計: `count` は列の裸の列参照を受理し非 `NULL` 行数を数える
  （`resolve_aggregate_input` の `AggregateInput::VectorColumnPresence`）。
  `sum`／`avg`／`min`／`max` は同じ `VECTOR` 列参照を一律 `22000` で拒否
- `sum` オーバーフロー → `22003`

`sort`／`offset` は engine の `BoundAggregate::with_group_order_by`／
`with_group_offset`（SQL テキスト経由の `GROUP BY ... ORDER BY ... LIMIT ...
OFFSET ...` と同じ対象名解決・実行器を共有。第 2 の実行器は持たない）へ写像する。
`limit` 相当のキーは持たない（SQL の `OFFSET` 単独が受理されないため、`offset`
のみの要求と SQL の一致は `LIMIT 10000 OFFSET m` で確認する）。

### `insert`

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"insert"` |
| `table` | ○ | string | |
| `rows` | ○ | object[] | 各行は `id`（非負整数）＋スキーマ列名をキーとする値 |
| `operation_id` | △（実質必須） | string | 欠落・`null`・空文字はいずれも `23502` |

要求例:

```json
{"op": "insert", "table": "docs",
 "rows": [{"id": 1, "embedding": [0.1, 0.2, 0.3], "lang": "ja"}],
 "operation_id": "op-1"}
```

成功時（`200`）:

```json
{"inserted": 1, "operation_id": "op-1"}
```

- `VECTOR` 列は数値配列。`nullable` の宣言値に関わらず常に必須。省略・`null`
  はいずれも拒否されるが、`wire_code` は列の `nullable` 宣言で分岐する
  （TABLE-16・TASK-204、Issue #904）: `nullable = false` の `VECTOR` 列は
  SQL 表層（`fill_omitted_columns`）と同じ `23502`
  （`NOT_NULL_VIOLATION`）、`nullable = true` の `VECTOR` 列は従来どおり
  `22000`（NOT NULL 違反ではなく「VECTOR は nullable でも常に必須」という
  NoSQL 表層固有の制約のため）
- 列型ごとの JSON 表現（Issue #896・NOSQL-17。`docs/design/
  nosql-typed-json-binding.md` 参照）: `INTEGER`／`BIGINT` は JSON 整数
  （小数・指数表記は `22P02`、非数値は `42601`。範囲外は `22003`）、`REAL`／`DOUBLE PRECISION`
  は JSON 数値（指数表記を受理。範囲外は `22003`）、`NUMERIC` は JSON 数値または数値文字列
  （桁あふれは `22003`）、`BOOLEAN` は JSON 真偽値、`DATE`／`TIMESTAMP`／
  `UUID` は JSON 文字列（書式違反は `22007`、範囲外・暦上不正は `22008`、`UUID` の形式不正は `22P02`）、
  `TEXT[]`／`BOOLEAN[]` は JSON 配列（要素種別不一致は `42601`、要素数
  超過は `54000`）。`TEXT`／`VECTOR`（旧来型）の型不一致のみ引き続き
  `22000` を維持する（新型は `42601`。表層内の非対称は既知の制約）
- 全型共通（`VECTOR` 列を除く。上記参照）: 「省略」（キー自体を持たない）と
  「明示的な JSON `null`」を区別する（TABLE-16・TASK-204、Issue #904。
  `DEFAULT` は省略にのみ適用し、明示 `null` には適用しない）。省略時は
  `DEFAULT` 句を持つ列なら既定値を補い、持たない列は nullable なら
  `NULL`・非 nullable なら `23502`（`NOT_NULL_VIOLATION`）。明示 `null` は
  `DEFAULT` の有無に関わらず、nullable なら `NULL`・非 nullable なら
  `23502`（`NOT_NULL_VIOLATION`。旧 `22000` から契約変更）
- 未知キー・次元不一致・型不一致は `22000`（新型の型不一致は `42601`。上記参照）
- 同一 `operation_id` の再送: 内容が一致すれば `23505`（`code`=`DUPLICATE_OPERATION_ID`）、不一致なら `22023`
  （台帳照合。TASK-101・RECOVER-10 の再送判定を透過する）
- 同一テナント内の `id` 重複は `23505`（`code`=`UNIQUE_VIOLATION`。他テナントの同 `id` とは衝突せず、
  応答は「不在時」と同一——TABLE-12・RLS-9）
- `rows` の行数上限は既定 64（`EngineCore::execute_bound_insert_in_session` が
  `rows.len()` を INDEX-4 の件数上限相当として判定。起動時 CLI
  `--batch-max-files`〔優先〕または環境変数 `VECTOR_DB_BATCH_MAX_FILES` で上書き可能）。超過は `54000`
- 行・バッチ単位のバイト上限（INDEX-4 ②③。`batch_limits::validate_batch_shape`）:
  各行のバイト量を `Σ TEXT 列.len() + VECTOR 列.len() × 4`（`Null` は 0）として
  積算し、1 行あたり `chunking::MAX_INPUT_BYTES`（固定）、またはバッチ合計
  `VECTOR_DB_BATCH_MAX_TOTAL_BYTES`（未設定時は
  `incremental::MAX_INDEX_TOTAL_BYTES`）を超えると `54000`
- 行数は上記の件数上限（①）とは別枠でチャンク数上限（④。
  `batch_limits::validate_chunk_total`。1 行＝1 チャンク換算。既定は
  `incremental::MAX_CHUNKS_PER_FILE`、環境変数 `VECTOR_DB_BATCH_MAX_CHUNKS` で
  上書き可能）でも判定され、超過は同じく `54000`

検証コード: `crates/wire-server/tests/nosql2_search.rs`・
`nosql2_search_binding.rs`・`nosql3_scan_mapping.rs`・`nosql3_scan_wire_parity.rs`・
`nosql4_aggregate.rs`・`nosql5_group_by.rs`・`nosql4_5_aggregate_wire_parity.rs`・
`nosql6_insert.rs`・`nosql6_tenant_row_id_scope.rs`・
`http_insert_response_boundary.rs`・`wire_insert_operation_id.rs`。

### `update`

`where`（単一行・`id` 完全一致形）は束縛・実行結線済み（Issue #876・
TASK-186・NOSQL-6・NOSQL-12）で、SQL 表層 `UPDATE ... WHERE id = <n>
USING OPERATION_ID`（SQL-17）と同一の実行器
（`engine::sql::exec::execute_update_with_schema`）・同一の台帳キー空間
（`(tenant, table, operation_id)`）へ到達する。`filter`（述語形。非空）も
束縛・実行結線済み（Issue #1062・TASK-186・NOSQL-12）で、SQL 表層の述語形
`UPDATE ... WHERE <述語> USING OPERATION_ID`（SQL-19）と同一の実行器
（`engine::sql::exec::execute_predicate_update`）・同一の台帳キー空間へ
到達する。`filter: []`（空配列）のみ `42601` で拒否する（engine を呼ばない。
SQL の `WHERE` 句省略とのパリティ）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"update"` |
| `table` | ○ | string | |
| `set` | ○ | object（任意キー・非空） | 列名をキーに持つ部分更新。`id`／`tenant_id`／`visibility` は `42601`。`TEXT` 列は JSON 文字列、`VECTOR` 列は数値配列（**文字列形のベクトルリテラルは受理しない**）のみ受理。他の列型は `insert` と同じ JSON 表現（Issue #896・NOSQL-17。上記参照）。`null` は列型を問わず nullable 判定込みで `bind_update` へ委譲する（非 nullable 列への `null` は `22000`）。型不一致・未知列・次元不一致は `22000`（新型の型不一致は `42601`） |
| `where` | △ | `{"id": number}` | 単一行・`id` 完全一致形。小数は `22000`、負数は `42601`（SQL 表層の字句解析・束縛とのパリティ。詳細は design doc 参照）。`filter` との排他（両方・双方欠落はいずれも `42601`） |
| `filter` | △ | object[] | [`filter` 配列](#filter-配列)参照。空配列は `42601`。非空は述語形 `UPDATE` として実行される（影響行数上限 `MAX_DML_AFFECTED_ROWS` 超過は `54000`。副作用ゼロ） |
| `operation_id` | △ | string | 欠落・`null`・空文字は `23502`。同一値への再送は台帳照合により内容一致 `23505`（`DUPLICATE_OPERATION_ID`）・不一致 `22023`（SQL 表層と共有） |

成功応答: `{"updated":<n>,"operation_id":"<echo>"}`。`where` 形（単一行）は
`n` が `0` または `1`（他テナント所有 id・未存在 id はいずれも `updated:0`・
`200` で応答バイト列が完全一致する。RLS-9）。`filter`（述語形）は `n` が
`0` 以上 `MAX_DML_AFFECTED_ROWS` 以下（一致した自テナント所有行数。上限超過は
`54000` で応答が返らない）。

複数列 `set` は JSON パース時点でキーのアルファベット順へ正規化される一方、
SQL 表層の `UPDATE ... SET col1 = .., col2 = ..` はクライアントが記述した
宣言順をそのまま保持する。台帳の内容照合ハッシュはこの列の記述順に依存
しないようスキーマの列定義順へ正規化済み（PR #992）のため、SQL 表層が
アルファベット順でない宣言順で書いた `UPDATE` と同一値の NoSQL `update`
は、同一 `operation_id` への再送であれば内容一致の再送（`23505`）として
正しく判定される（詳細は `docs/design/nosql-update-delete-mapping.md`
「複数列 `set` の宣言順と `content_hash`」節参照）。`VECTOR` 列の値
（SQL 表層のベクトルリテラル文字列と NoSQL 表層の数値配列）は、この
`where` 形（単一行）では元々同一の内容照合ハッシュに一致しており、
Issue #1061 では層 A の固定テスト（`nosql12_update_delete.rs::
cross_surface_vector_value_*`）を追加した（詳細は
`docs/design/nosql-update-delete-mapping.md`「述語形 VECTOR 割当の表現
統一と既存台帳エントリの互換性」節参照）。述語形（`filter`。`WHERE` 対象列
は `VECTOR` 以外だが、`SET` 側に `VECTOR` 列を含めることは単一行形と同様に
可能）自体の跨表層一致・台帳照合は Issue #1062・`nosql12_update_delete.rs::
cross_surface_predicate_*`（`SET` に `VECTOR` 列を含むケースを含む）で
固定する。

要求例（`where` 形）:

```json
{"op": "update", "table": "docs", "set": {"lang": "en"}, "where": {"id": 1},
 "operation_id": "op-1"}
```

### `delete`

`where`（単一行・`id` 完全一致形）は束縛・実行結線済み（Issue #876・
TASK-186・NOSQL-6・NOSQL-12）で、SQL 表層 `DELETE FROM ... WHERE id = <n>
USING OPERATION_ID`（SQL-18）と同一の実行器（`engine::sql::exec::
execute_delete`）・同一の台帳キー空間を共有する。`filter`（述語形。非空）も
束縛・実行結線済み（Issue #1062・TASK-186・NOSQL-12）で、SQL 表層の述語形
`DELETE FROM ... WHERE <述語> USING OPERATION_ID`（SQL-19）と同一の実行器
（`engine::sql::exec::execute_predicate_delete`）へ到達する。`filter: []`
（空配列）のみ `42601` で拒否する（engine を呼ばない）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"delete"` |
| `table` | ○ | string | |
| `where` | △ | `{"id": number}` | 単一行・`id` 完全一致形。小数は `22000`、負数は `42601`（SQL 表層とのパリティ）。`filter` との排他（両方・双方欠落はいずれも `42601`） |
| `filter` | △ | object[] | [`filter` 配列](#filter-配列)参照。空配列は `42601`。非空は述語形 `DELETE` として実行される（影響行数上限 `MAX_DML_AFFECTED_ROWS` 超過は `54000`。副作用ゼロ） |
| `operation_id` | △ | string | 欠落・`null`・空文字は `23502`。同一値への再送は台帳照合により `23505`（`DUPLICATE_OPERATION_ID`。内容一致。`DELETE` は行の有無に関わらず同一内容） |

成功応答: `{"deleted":<n>,"operation_id":"<echo>"}`。`where` 形（単一行）は
`n` が `0` または `1`（他テナント所有 id・未存在 id はいずれも `deleted:0`・
`200` で応答バイト列が完全一致する。RLS-9）。`filter`（述語形）は `n` が
`0` 以上 `MAX_DML_AFFECTED_ROWS` 以下（一致した自テナント所有行数。上限超過は
`54000` で応答が返らない）。

要求例:

```json
{"op": "delete", "table": "docs", "where": {"id": 1}, "operation_id": "op-1"}
```

検証コード: `crates/wire-server/src/http/query/op.rs`・`schema.rs`・
`dml_target.rs`・`update.rs`・`delete.rs`・`gate.rs`（単体テスト）・
`crates/wire-server/tests/nosql9_op_allowlist.rs`・`nosql1_op_vocabulary.rs`・
`nosql12_update_delete.rs`・
`crates/engine/tests/sql_update_delete_session_public_api.rs`。
実バイナリ・無改造クライアント（psql・curl・urllib・fetch）経由での
SQL 表層とのパリティ・RLS-9 応答同一性・台帳のプロセス・表層横断永続は
層 B `three_client_http_e2e.rs::run_sql_nosql_dml_parity_scenario`
（Issue #877）が検証する。

### `create_table`／`alter_table`／`drop_table`（DDL）

NOSQL-13・TASK-207（Issue #910）で `op` 語彙へ加わった DDL 3 op。SQL 表層の
`CREATE TABLE`／`ALTER TABLE ... ADD COLUMN`／`DROP TABLE`（SQL-23）と
**同一の実行器**（`engine::core::EngineCore::execute_parsed_in_session`）へ、
JSON の各フィールドを SQL 表層と同じ字句トークン列へ写像したうえで到達させる
（第 2 の DDL 実行器・第 2 の権限判定は存在しない。写像の実装は
`crates/wire-server/src/http/query/ddl.rs`）。

**DDL 実行権限**: `--ddl-allowed-users` に列挙したユーザーのみ実行できる
（`POST /v1/session` のログイン成功直後に確定し、以後そのセッションの寿命中
固定される）。権限の無いセッションは、対象テーブルの有無にかかわらず常に
`42501`（`403`）のみを返す（存在オラクル非公開。テナント境界とは別軸の判定）。

`create_table.columns[].type` の受理集合は `text`／`vector`／`integer`／
`bigint`（SQL 表層の `CREATE TABLE` と同じ）。`alter_table.add_column.type` は
`text`／`integer`／`bigint`／`real`／`double`（`DOUBLE PRECISION`）／
`boolean`／`date`／`timestamp`／`bytea`／`json`／`jsonb`／`uuid`／
`numeric`（`precision`／`scale` 必須）／`vector`（`dim` 必須。構文は通るが
実行段で `0A000`）／`enum`（`enum_type` 必須）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"create_table"`／`"alter_table"`／`"drop_table"` |
| `table` | ○ | string | |
| `columns`（`create_table`） | ○ | object[] | `{"name","type","dim"?,"nullable"?,"default"?}`。予約列名（`id`／`tenant_id`／`visibility`／`check`／`constraint`）は `42601` |
| `constraints`（`create_table`） | △ | object[] | `{"kind":"primary_key"｜"unique"｜"foreign_key"｜"check","columns"?,"references"?}`。`references`＝`{"table","columns"?,"on_delete"?,"on_update"?}`。`check` は `0A000`（述語の JSON 写像は別論点。後続 Issue の担当）。`foreign_key` の `on_delete`／`on_update` は `"no_action"｜"restrict"｜"cascade"｜"set_null"｜"set_default"` の固定語彙（小文字 snake_case・完全一致。Issue #1148）で `ON DELETE`／`ON UPDATE` 参照アクション（TABLE-17・TASK-205、Issue #907）を宣言できる。省略時・`"no_action"`／`"restrict"` はいずれも `NO ACTION` と同じカタログ表現になる。語彙外・大文字混じり・非文字列値は `42601`（副作用ゼロ）。参照元列が `NOT NULL` の状態で `"set_null"` を付ける・DEFAULT の無い `NOT NULL` 列に `"set_default"` を付けるなど宣言時に常に失敗する組み合わせは `42830`。宣言済みテーブルへの `update`／`delete` op は SQL 表層と同一の単一検査点を通るため連鎖が発火し、連鎖の深さ・行数の上限超過は `54000`（HTTP `413`。副作用ゼロ）として到達する |
| `add_column`（`alter_table`） | △ | object | `{"name","type","dim"?,"precision"?,"scale"?,"enum_type"?}`。`drop_column` と排他必須（両方・双方欠落は `42601`） |
| `drop_column`（`alter_table`） | ○ | object | `{"name"}`。SQL 表層の `ALTER TABLE ... DROP COLUMN` と同じ入口へ結線（Issue #1167。エラー契約は SQL 表層と同一）。`add_column` と排他必須 |

成功応答は 3 op 共通で `{"ok":true}`（行数・件数を返さない）。

要求例（`create_table`）:

```json
{"op": "create_table", "table": "docs", "columns": [
  {"name": "embedding", "type": "vector", "dim": 3},
  {"name": "lang", "type": "text", "nullable": true}
]}
```

要求例（`create_table`。`FOREIGN KEY` の参照アクション。Issue #1148）:

```json
{"op": "create_table", "table": "children", "columns": [
  {"name": "parent_id", "type": "integer", "nullable": true},
  {"name": "note", "type": "text"}
], "constraints": [
  {"kind": "foreign_key", "columns": ["parent_id"],
   "references": {"table": "parents", "columns": ["id"],
   "on_delete": "cascade", "on_update": "set_null"}}
]}
```

要求例（`alter_table`）:

```json
{"op": "alter_table", "table": "docs",
 "add_column": {"name": "note", "type": "text"}}
```

要求例（`drop_table`）:

```json
{"op": "drop_table", "table": "docs"}
```

`create_index`／`drop_index`／`create_view`／`drop_view` は NOSQL-13 の対象外
のまま語彙外（`0A000`）に据え置く。

検証コード: `crates/wire-server/src/http/query/op.rs`・`schema.rs`・
`ddl.rs`（単体テスト）・`gate.rs`・
`crates/wire-server/tests/nosql13_ddl.rs`・`nosql1_op_vocabulary.rs`・
`nosql8_schema_validation.rs`・`nosql9_op_allowlist.rs`・
`crates/engine/tests/sql_ddl_tokens_public_api.rs`。

## `filter` 配列

`search`／`scan`／`aggregate` 共通で使える事前フィルタ配列。要素は「葉」
（`column`／`op`／`value`）または「グループ」（`or`）のいずれかの形を取り、
配列自体・グループ内の分岐はいずれも暗黙に `AND` 結合として扱う（Issue #945・
NOSQL-14 で範囲比較・`IN`・`OR` へ拡張。それ以前は `eq`／`prefix` の 2 語彙・
`AND` のみだった。Issue #1197 で `ne`・`between`・`like`・`is_null`／
`not_null`・`not` グループを追加）。`update`／`delete` の述語形（Issue #1062）でも
同じ配列表現を使うが、対応語彙は `eq`／`ne`／`prefix`／`like`／`between`／
`is_null`／`not_null` と `not` グループ・`AND` 結合のみに留まる（範囲比較・`IN`・
`OR` グループへの拡張は Issue #1118 が明示的に対象外とした。`WHERE <述語> USING OPERATION_ID` の意味論〔影響行数上限 `54000`・
台帳照合 `23505`／`22023`〕は各 op の節を参照）。

```json
[
  {"column": "lang", "op": "eq", "value": "ja"},
  {"column": "price", "op": "gte", "value": "10"}
]
```

（`price` は `NUMERIC` 列を想定。`lt`／`le`／`lte`／`gt`／`ge`／`gte` は
`DATE`・`TIMESTAMP`・`UUID`・`NUMERIC`・`BYTEA`・`TEXT`（バイト順）列と、
`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列（JSON 数値のみ。Issue #1183）
を受理する。数値列・`TEXT` の範囲比較は式レーンで束縛され、同じ条件の SQL
`WHERE` と結果集合が一致する——後述「葉（leaf）」節参照）

```json
[{"or": [
  {"column": "lang", "op": "eq", "value": "ja"},
  {"column": "lang", "op": "in", "value": ["en", "fr"]}
]}]
```

追加語彙の例（Issue #1197。SQL の `NOT`・`BETWEEN`・`LIKE`・`IS [NOT] NULL` と
同じ結果集合を返す）:

```json
[
  {"column": "lang", "op": "ne", "value": "ja"},
  {"column": "created", "op": "between", "value": ["2024-01-01", "2024-12-31"]},
  {"column": "path", "op": "like", "value": "%/docs/_%"},
  {"column": "note", "op": "not_null"},
  {"not": {"or": [
    {"column": "lang", "op": "eq", "value": "en"},
    {"column": "note", "op": "is_null"}
  ]}}
]
```

### 葉（leaf）

- `column`（文字列）・`op`（文字列）・`value`（文字列・数値・真偽値、`in`／
  `between` の配列、ARRAY／JSON 列への `eq`／`ne` の配列・オブジェクト）の 3 つの
  フィールドのみ。`is_null`／`not_null` は `value` を**持たない**（`null` を含め
  付いていれば `42601`）
- `op` は次の 14 語彙（完全一致。大文字小文字の読み替えなし。`"op":"not"` は
  語彙外で `42601`）:
  - `eq`（一致）・`prefix`（前方一致。従来どおり）
  - `ne`（`NOT col = <値>` と同じ。値のレーンは `eq` と同じ。NULL 行は除外）
  - `like`（`TEXT` 列。文字列のみ。`%`・`_`・`\` をワイルドカード・エスケープと
    して解釈する。列型違反は `22000`、パターン長超過は `54000`）
  - `between`（`[low, high]` の要素ちょうど 2 個のスカラー配列。違反は `42601`。
    `DATE`／`TIMESTAMP`／`UUID`／`BYTEA`／`NUMERIC` と `INTEGER`／`BIGINT`／`REAL`／
    `DOUBLE PRECISION`（JSON 数値のみ。`>= low AND <= high` の式レーン）。
    `TEXT` 等は SQL の `BETWEEN` と同じく `22000`）
  - `is_null`／`not_null`（`IS NULL`／`IS NOT NULL`。`VECTOR` 列は `22000`）
  - `lt`／`le`／`lte`／`gt`／`ge`／`gte`（範囲比較。`le`/`lte`・`ge`/`gte` は
    それぞれ完全一致の同義語として両方受理する——Issue の受け入れ条件と
    対象ビヘイビア NOSQL-14 とで表記が食い違うため安全側に倒した判断。
    どちらに一本化するかは spec 側のオーナー判断事項）
  - `in`（配列の要素のいずれかと一致。空配列は `42601`、256 要素超は
    `54000`）
- `column` にサーバー側 RLS 述語名相当（`visible`／`visible()`。大文字小文字
  非区別）を指定する経路は `42601`（`or` 分岐の内側を含め再帰的に検査する。
  RLS はサーバー側暗黙適用のみで、クライアントは述語を書けない）
- `eq` は対象列の型に応じたレーンへ振り分ける（Issue #896・NOSQL-17。詳細は
  `docs/design/nosql-typed-json-binding.md`「filter（`eq` の型別レーン）」節
  参照）: `TEXT`（旧来型。値・型不一致は `42601`。insert/update の「TEXT は旧来型
  = `22000`」非対称は filter には適用しない）／`ENUM`（`42601`。語彙外は
  `22P02`）／`BOOLEAN`（`42601`）／`DATE`・`TIMESTAMP`・`UUID`（`42601`。形式・
  範囲は engine 側で検証）／`BYTEA`（base64 の JSON string。`42601`／`54000`。
  復号後 4 MiB 超で `54000`）／`NUMERIC`（数値または数値文字列。`42601`）
- `lt`／`le`／`lte`／`gt`／`ge`／`gte` は `DATE`・`TIMESTAMP`・`UUID`（文字列）・
  `NUMERIC`（数値または数値文字列）・`BYTEA`（base64 の JSON string）・`TEXT`
  （文字列。バイト順の式レーン比較。Issue #1183）・`INTEGER`／`BIGINT`／`REAL`／
  `DOUBLE PRECISION`（JSON 数値。下記）を受理する。`ENUM`／`BOOLEAN`／`VECTOR`／
  `ARRAY`／`JSON`／`JSONB` 列は engine 側の「範囲比較非対応列」判定（`22000`）へ
  委譲する
- `in` は `TEXT`／`ENUM`（文字列配列）・`DATE`／`TIMESTAMP`／`UUID`（文字列配列）・
  `NUMERIC`（数値または数値文字列の配列）・`BYTEA`（base64 の JSON string の
  配列）のみ受理する。他の列型は engine 側の「IN 非対応列」判定（`22000`）へ
  委譲する
- `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq`・範囲比較は
  JSON 数値のみ受理し（文字列・真偽値は型不一致）、式レーン
  （`udf_call::bind_expr`）で束縛する（Issue #1183）。`BIGINT` の |値| が
  2^53 を超える場合（JSON リテラル・格納値とも）は `22000`。`TEXT` の範囲比較も
  式レーン（バイト順）で受理する。`in` は数値列では従来どおり `22000`。
  述語形 `update`／`delete` の `filter` では数値列の `eq`／`ne`／`between` を
  `0A000` で拒否する
- `prefix` は従来どおり `TEXT` 列限定（他の列型は `22000`）
- `in` は列型に関わらず対応する場合のみ受理する（対象外の列型は `22000`）
- 未知列・`VECTOR`／`ARRAY`／`JSON`／`JSONB` 列拒否（`22000`）は
  `engine::declarative_filter` の既存契約をそのまま透過する

### グループ（`not`）

- `{"not": <要素>}` の形のみ許可する（キーは `not` 1 つ、値はオブジェクトで、
  葉・`or` グループ・入れ子の `not` のいずれか）。違反は `42601`
- 否定は**葉まで押し下げて**束縛する（De Morgan。`or` 群の上に否定を置くと
  NULL 行が UNKNOWN から真へ反転する fail-open になるため）。結果は SQL の
  `NOT ( ... )` と一致する。内側の葉の RLS 述語名検査も `not` を貫通する
- ネスト深さは `or` と共有して数える（上限 32。超過は `54000`）
- JSON 上の葉の数で事前検査するため、数値列の `between`／`ne`／`not eq` のように
  展開で 1 葉が 2 葉になる場合、JSON 上で 256 葉ちょうどのとき engine の事後検査
  だけが `54000` になりうる（拒否側に倒れる既知の差分）

### グループ（`or`）

- `{"or": [<要素>, ...]}` の形のみ許可する。`or` 以外のキーが混在する・`or`
  の値が配列でない・空配列はいずれも `42601`
- 各分岐は葉または入れ子の `or` グループ 1 つ。分岐が 1 つだけの場合は
  親の `AND` 列へ平坦化する（`{"or": [X]}` は `X` と等価）
- ネスト深さの上限は 32（超過は `54000`）。HTTP 経由では JSON 自体の深さ上限
  16（NOSQL-8）が先に効くため、実際に 32 段の `or` へ届くのは engine の
  `sql::declarative_predicate` API を直接呼び出す経路に限られる

### 上限（`Vec` 確保より前に検査する）

- 葉（`Leaf`）の総数: 256 個まで（超過は `54000`）
- `or` のネスト深さ: 32 段まで（超過は `54000`）
- `in` の要素数: 256 個まで（超過は `54000`。空配列は `42601`）

検証コード: `crates/wire-server/tests/nosql7_filter_mapping.rs`（`eq`／`prefix`・
`AND` のみの既存回帰）・`crates/wire-server/tests/nosql14_filter_operators.rs`
（範囲比較・`IN`・`OR`。Issue #945）。

**`update`／`delete` の `filter`（述語形）における対応範囲**: 範囲比較 6 語彙・
`in`・`or` グループは `search`／`scan`／`aggregate` 専用。`update`／`delete` の
`filter` は `eq`／`ne`／`prefix`／`like`／`between`／`is_null`／`not_null` と、
それらを包む `not` グループ・`AND` 結合のみに対応し（Issue #1197。SQL の述語形
`UPDATE`／`DELETE` と同一の構文形へ写像するため、SQL⇄NoSQL の台帳照合も成立
する）、範囲比較・`in`・`or`（`not` の内側を含む）を渡すと [`filter::
map_predicate_dml_items`](../src/http/query/filter.rs) が
`FilterError::UnsupportedOperatorForPredicateDml`（`42601`）で拒否する
（Issue #1118 が明示的に対象外とした範囲。上記「`update`」「`delete`」節参照）。
数値列（`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION`）への `eq`／`ne`／`between`
は `0A000`。

## `explain`

`explain: true` は `search`（`vector`・`plan` いずれも）・`scan`・`aggregate`
（`group_by`／`having` の有無を問わない）で受理し、検索・走査・集計の本体を
実行せず SQL 表層の対応する `EXPLAIN` 文と同一内容の `QUERY PLAN` を返す
（`vector` 指定 `search`・`scan`・`aggregate` は Issue #948・NOSQL-16・SQL-27、
`plan` 指定 `search` は TASK-186・NOSQL-10・Issue #765）。索引の構築・
ルックアップ・キャッシュ消費・行走査・書き込みはいずれの op でも一切行わない。

要求例（`plan` 指定 `search`）:

```json
{"op": "search", "table": "docs", "plan": "find content", "limit": 10,
 "explain": true}
```

要求例（`vector` 指定 `search`）:

```json
{"op": "search", "table": "docs", "vector": [0.1, 0.2, 0.3, 0.4], "limit": 10,
 "explain": true}
```

要求例（`scan`）:

```json
{"op": "scan", "table": "docs", "limit": 10, "explain": true}
```

要求例（`aggregate`）:

```json
{"op": "aggregate", "table": "docs",
 "aggregates": [{"fn": "count", "column": "*"}], "explain": true}
```

応答例（`200`）:

```json
{"explain": ["<QUERY PLAN の行>", "..."]}
```

- `vector`・`plan` 同時指定＋`explain: true` → `42601`
- `vector`・`plan` 両方欠落＋`explain: true` → `42601`
- `insert`／`update`／`delete` への `explain` はいずれもスキーマが宣言しない
  未知キーとして `42601`
- `explain` が非 bool → `42601`

検証コード: `crates/wire-server/tests/nosql10_explain.rs`（`plan` 指定
`search` の網羅的カバレッジ）・`crates/wire-server/tests/
nosql16_explain_targets.rs`（`vector` 指定 `search`・`scan`・`aggregate` の
網羅的カバレッジ）・`wire_explain.rs`。

## 応答スキーマ

`search`（`explain` なし）／`scan`／`aggregate` 成功時は共通形:

```json
{"columns": [{"name": "id", "type": "numeric"}, {"name": "lang", "type": "text"}],
 "rows": [[1, "ja"], [2, "en"]],
 "row_count": 2}
```

- キー順固定（`columns` → `rows` → `row_count`）・空白なし
- `row_count` は常に `rows` の長さ
- `columns[].type` は列型ごとの名前を返す（Issue #896・NOSQL-17。SQL 表層
  `RowDescription`（Issue #895）の OID 写像とは**独立**の対応表——SQL wire
  側は後方互換のため多くの新型を `text`（OID 25）へ丸めるが、NoSQL の JSON
  API は型情報をそのまま伝える）: 疑似列 `id`／式項目は `"numeric"`／
  `"text"`（wire 側と一致）、それ以外は `"text"`／`"vector"`／`"integer"`／
  `"bigint"`／`"real"`／`"double precision"`／`"boolean"`／`"date"`／
  `"timestamp"`／`"numeric"`／`"uuid"`／`"bytea"`／`"json"`／`"jsonb"`／
  `"enum"`／`"text[]"`／`"boolean[]"`（列型に対応）。値そのものは常に
  native JSON（配列・数値・真偽値・文字列）で返し、値表現を型名に合わせて
  文字列化してはいない
- `BIGINT` の値（`Cell::SignedInteger`）は `±(2^53-1)` を超える場合のみ
  JSON 文字列で送出する（Issue #896。JS 系クライアントの `JSON.parse` に
  よる精度誤解を防ぐ。`id`／`COUNT` の値表現は不変のまま TASK-185 の担当）
- `Cell::Vector` は `[1,2.5]` のような JSON 数値配列として返る
- 非有限（`NaN`／`±Infinity`）な浮動小数は `null` に丸めず内部エラーとして
  fail-closed に拒否する（通常は engine 側の評価時点で `22000` になり到達しない）

`insert`・`explain: true` の応答形は前節の専用形（`{"inserted",...}`／
`{"explain":[...]}`）を参照。

検証コード: `crates/wire-server/tests/nosql11_response_schema.rs`。

## SQL ↔ NoSQL 対応表

| SQL | NoSQL |
| --- | --- |
| `SELECT id, lang FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10` | `search` + `vector` + `columns` |
| `SELECT id FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'alpha') LIMIT 3` | `search` + `vector` + `hybrid.text`（疎側列は `body` 固定） |
| `SELECT id FROM docs USING PLAN('find content') LIMIT 10` | `search` + `plan` |
| `... LIMIT n USING MODE 'recall'` | `"mode":"recall"` |
| `WHERE lang = 'ja'` | `filter` 要素 `{"op":"eq",...}` |
| `WHERE lang LIKE 'j%'` | `filter` 要素 `{"op":"prefix",...}` |
| `EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10` | `search` + `plan` + `"explain":true` |
| `EXPLAIN SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10` | `search` + `vector` + `"explain":true`（Issue #948） |
| `SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10`（広域取得 SQL-15） | `scan` |
| `EXPLAIN SELECT id FROM docs LIMIT 10` | `scan` + `"explain":true`（Issue #948） |
| `SELECT id, lang FROM docs ORDER BY lang DESC LIMIT 10`（スカラー `ORDER BY`。SQL-25 (a)） | `scan` + `sort`（Issue #946・NOSQL-15） |
| `SELECT id, lang FROM docs LIMIT 10 OFFSET 20`（広域取得 `OFFSET`。SQL-25 (b)） | `scan` + `offset`（Issue #947・NOSQL-15） |
| `SELECT COUNT(*), SUM(id) FROM docs` | `aggregate` |
| `SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING count >= 2` | `aggregate` + `group_by` + `having` |
| `SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY count DESC LIMIT 10000 OFFSET 1`（SQL-25 (a)(b)） | `aggregate` + `group_by` + `sort` + `offset`（Issue #1198・NOSQL-15。`limit` 相当のキーは無く、`LIMIT` は SQL 側のグループ数上限と同値で対応） |
| `EXPLAIN SELECT COUNT(*) FROM docs` | `aggregate` + `"explain":true`（Issue #948） |
| `INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-1'` | `insert` + `operation_id` |
| `UPDATE docs SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-1'` | `update` + `where.id` + `operation_id`（結線済み。同一実行器・同一台帳キー空間） |
| `UPDATE docs SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-1'` | `update` + `filter` + `operation_id`（結線済み。同一実行器・同一台帳キー空間。Issue #1062） |
| `DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'op-1'` | `delete` + `where.id` + `operation_id`（結線済み。同一実行器・同一台帳キー空間） |
| `DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID 'op-1'` | `delete` + `filter` + `operation_id`（結線済み。同一実行器・同一台帳キー空間。Issue #1062） |
| `CREATE TABLE docs (embedding VECTOR(3), lang TEXT)` | `create_table` + `columns`（Issue #910。同一実行器） |
| `FOREIGN KEY (parent_id) REFERENCES parents (id) ON DELETE CASCADE ON UPDATE SET NULL` | `constraints[kind=foreign_key].references.on_delete`／`on_update`（Issue #1148。同一実行器） |
| `ALTER TABLE docs ADD COLUMN note TEXT` | `alter_table` + `add_column`（Issue #910。同一実行器） |
| `DROP TABLE docs` | `drop_table`（Issue #910。同一実行器） |

対応の無いもの（NoSQL 側に受理形が存在しない。実際の応答は語彙外 `op` として
`0A000`、または未知キーとして `42601`）:

- `HINT ORDER(...)` によるソフトブースト
- UDF 呼び出し・`CREATE FUNCTION`
- `SET`（`search_mode` 等のセッション変数設定）
- 定数のみの `SELECT`
- `aggregate` への `limit`（`GROUP BY ... LIMIT n` 相当。`sort`／`offset` は
  Issue #1198 で対応済みだが、`limit` 相当のキーは未対応）
- `INSERT` のファイル形（`path`／`body` 列指定の増分インデックス投入）
- `GROUP BY` への `LIMIT` の付与（`ORDER BY` は `aggregate` の `sort` で対応済み）
- `ALTER TABLE ... ALTER COLUMN TYPE` 相当の op（`alter_table` に語彙なし。別論点）
- `CREATE TABLE` の `CHECK` 制約（`create_table.constraints[].kind == "check"` は `0A000`）
- `CREATE INDEX`／`DROP INDEX`／`CREATE VIEW`／`DROP VIEW`（NOSQL-13 の対象外）
- `FOREIGN KEY` の `MATCH {SIMPLE|FULL}`／`[NOT] DEFERRABLE`／
  `INITIALLY {DEFERRED|IMMEDIATE}` 句（NoSQL `references` に対応キーが無い。
  Issue #1148 の対象外。列リスト形の `SET NULL (col, ...)`／
  `SET DEFAULT (col, ...)` も同様に SQL 表層でも未実装）

逆方向（NoSQL にあって SQL に対応形がないもの）は無い。

検証コード: `crates/wire-server/tests/nosql3_scan_wire_parity.rs`・
`nosql4_5_aggregate_wire_parity.rs`・`wire_using_plan.rs`・
`wire_insert_operation_id.rs`・`crates/engine/tests/default_preset.rs`。
本対応表の 10 ケース（search-1〜5・scan-1・agg-1〜4）は無改造 `psql`（SQL
表層）と無改造 HTTP クライアント（NoSQL 表層）の双方を実バイナリ経由で
実行して列名・型・行集合の一致を検証する層 B `three_client_http_e2e.rs`
（Issue #779。`make e2e-three-client-http`）でも固定している。

## エラー応答

本節の数値・文言は spec 由来の閾値ではなく、`crates/wire-server/src/http/
status.rs`（`wire_code` → HTTP ステータスの射影）・`error_body.rs`（JSON 本文
エンコーダ）・`response.rs`（ステータス行・ヘッダ）を単一情報源とする実装
既定値である（ERR-4・ERR-5 ポインタ）。表とコードの一致は
`tests/nosql_api_doc.rs` が機械検証する。

### 本文仕様

通常応答の本文形（`http::error_body::encode`）:

```json
{"wire_code": "XX000", "code": "INTERNAL_ERROR", "message": "internal error"}
```

- 実際にはトップレベルが `{"error": { ... }}` で包まれる。上の例はキー順・値の
  固定を示すための `error` オブジェクトの中身のみの抜粋であり、下記の
  golden 例が実際のトップレベル形を示す
- キー順は `wire_code` → `code` → `message` →（緊急応答時のみ）`data` に固定。
  空白を含まないコンパクト形・改行を含まない 1 行（0x20 未満のバイトを一切
  含まない）
- `code` は `ErrorClass::label()`（`SCREAMING_SNAKE_CASE`。人間可読な補助
  ラベルであり、契約として確定しているのは `wire_code` のみ）
- エスケープ規則: `"`・`\`・U+0000〜U+001F のみをエスケープする。
  `\b`／`\t`／`\n`／`\f`／`\r` はよく使う短縮形、それ以外の一般制御文字は
  小文字 `\u00xx`。非 ASCII（日本語・補助面文字を含む）・U+007F は
  エスケープせず UTF-8 のまま透過する
- `message` の契約: 固定の英語文言、または内部エラー時に固定文言へ
  差し替えられた `WireError` 由来の値のみ（長さ上限あり）。他テナントの
  データ・存在情報・内部詳細を含まない（存在オラクルを提供しない）。
  利用者は `message` の具体的な文言そのものを契約として依存しないこと
- 通常応答は `data` キーを**決して**含まない。緊急応答専用の
  `encode_may_be_committed` と本文組み立てが構造的に分離されている
  （[緊急応答の `data`](#緊急応答の-data) を参照）

golden 例（`ErrorClass::InternalError`・`message="internal error"` から
`error_body::encode`／`encode_may_be_committed` が実際に返す本文。
バイト単位で一致することをテストが固定する）:

```json
{"error":{"wire_code":"XX000","code":"INTERNAL_ERROR","message":"internal error"}}
```

```json
{"error":{"wire_code":"XX000","code":"INTERNAL_ERROR","message":"internal error","data":{"state":"may_be_committed"}}}
```

### ステータス行・ヘッダ

```text
HTTP/1.1 400 Bad Request
Content-Type: application/json; charset=utf-8
Content-Length: <本文バイト長>
Connection: close
Date: <IMF-fixdate>
```

- `Content-Type` は常に `application/json; charset=utf-8`
- `Content-Length` は本文の実バイト長（`Content-Type` と同じく必須固定ヘッダ）
- `Connection: close` を常に付ける（1 応答ごとに接続を閉じる。
  [転送路の共通規則](#転送路の共通規則)参照）
- `Date` は RFC 9110 IMF-fixdate。システムクロックが `UNIX_EPOCH` より前の
  異常値を指す場合でも応答送出自体は止めず、`UNIX_EPOCH` 相当へ fail-closed
  に縮退する（可用性を優先し `Date` の正確性を犠牲にする設計判断）
- `401`（`AuthRequired`／`AuthInvalid`）応答のみ、RFC 9110 §11.6.1 が要求する
  認証チャレンジとして `WWW-Authenticate: Bearer` を追加する。他のステータス
  では付与しない
- 射影表の値域（`{400, 401, 403, 404, 409, 413, 500, 501, 503}`）外のステータス
  が渡された場合、理由句を捏造せず `500`＋`XX000` 固定本文へ fail-closed に
  縮退する。`ErrorClass` の値域は `#[deny(clippy::wildcard_enum_match_arm)]`
  で網羅性が強制されるため、現状はこの縮退経路自体が到達不能な防波堤

### `wire_code` → HTTP ステータス射影表

「1 つの `wire_code` → 常に 1 つの HTTP ステータス」の方向にのみ 1:1 の射影
であり、逆方向（ステータス → `wire_code`）は 1:1 ではない（例えば `400` は
25 分類が共有する）。

| `wire_code` | `code` | HTTP ステータス | 理由句 | NoSQL 表層での主な発生源 |
| --- | --- | --- | --- | --- |
| `08P01` | `PROTOCOL_VIOLATION` | 400 | Bad Request | 要求行・ヘッダ形状違反、未知ターゲットへのアクセス |
| `22000` | `INVALID_INPUT` | 400 | Bad Request | `op` 別スキーマ検証での値の型・形状不正 |
| `22003` | `NUMERIC_OUT_OF_RANGE` | 400 | Bad Request | 集計（`aggregate`）でのオーバーフロー、`CHECK` 制約式の数値あふれ |
| `22007` | `INVALID_DATETIME_FORMAT` | 400 | Bad Request | `DATE`／`TIMESTAMP` リテラルの書式違反（`insert`／`update`／`filter`。Issue #1187） |
| `22008` | `DATETIME_FIELD_OVERFLOW` | 400 | Bad Request | `DATE`／`TIMESTAMP` リテラルの範囲外・暦上不正（`update` の `set` 経由） |
| `22012` | `DIVISION_BY_ZERO` | 400 | Bad Request | `insert`／`update` が書き込む行の `CHECK` 制約（TABLE-16・TASK-204）の式評価での 0 除算 |
| `22023` | `OPERATION_ID_CONTENT_MISMATCH` | 400 | Bad Request | `insert` の `operation_id` 再送時の内容不一致 |
| `22P02` | `INVALID_TEXT_REPRESENTATION` | 400 | Bad Request | 値の形式不正: ENUM 列の語彙外ラベル・INTEGER／BIGINT 列への小数／指数表記・BYTEA の不正 base64・NUMERIC／配列／JSON 列の形式不正（`insert`／`update`／`filter`。Issue #1187） |
| `23502` | `MISSING_OPERATION_ID` | 400 | Bad Request | `insert` の `operation_id` 欠落 |
| `23502` | `NOT_NULL_VIOLATION` | 400 | Bad Request | `NOT NULL` 列（TABLE-16・TASK-204）への `insert`／`update` での省略・明示 `null` |
| `25000` | `INVALID_TRANSACTION_STATE` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（明示トランザクション制御が op 語彙に無い。後述） |
| `25001` | `ACTIVE_SQL_TRANSACTION` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（同上） |
| `25P01` | `NO_ACTIVE_SQL_TRANSACTION` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（同上） |
| `25P02` | `IN_FAILED_SQL_TRANSACTION` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（同上） |
| `2BP01` | `DEPENDENT_OBJECTS_STILL_EXIST` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`DROP TABLE`／`DROP VIEW` は SQL 表層専用の DDL。後述） |
| `42601` | `UNSUPPORTED_SQL_SYNTAX` | 400 | Bad Request | JSON 構文エラー、`op` 別スキーマ違反、`tenant_id` 相当値の自己申告 |
| `42701` | `DUPLICATE_COLUMN` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`CREATE TABLE`・`ALTER TABLE ADD COLUMN` は op 許可リスト外。後述） |
| `42702` | `AMBIGUOUS_COLUMN` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`INNER JOIN`〔SQL-28・RLS-10、Issue #925〕で SQL 表層からは到達可能になったが、NoSQL 表層の op 語彙に JOIN 相当が無いため。後述） |
| `42703` | `UNDEFINED_COLUMN` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`CREATE INDEX` は op 許可リスト外。後述） |
| `42704` | `UNDEFINED_OBJECT` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`DROP INDEX` は op 許可リスト外。後述） |
| `42723` | `DUPLICATE_FUNCTION` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`CREATE FUNCTION` は SQL 表層専用。後述） |
| `42804` | `DATATYPE_MISMATCH` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`CASE`／`COALESCE`／`NULLIF`・集合演算・`INNER JOIN` の結合キー型不一致〔SQL-28・RLS-10、Issue #925〕・式層の演算子／関数引数の型不一致〔Issue #1186〕のいずれも SQL 表層専用。後述） |
| `42809` | `WRONG_OBJECT_TYPE` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`DROP TABLE`／`DROP VIEW`・ビューへの書き込みは SQL 表層専用の DDL。後述） |
| `42830` | `INVALID_FOREIGN_KEY` | 400 | Bad Request | `create_table.constraints[kind=foreign_key].references.on_delete`／`on_update`（Issue #1148）が宣言時に常に失敗する組み合わせ（`NOT NULL` 列への `set_null`・DEFAULT の無い `NOT NULL` 列への `set_default` 等） |
| `42883` | `UNDEFINED_FUNCTION` | 400 | Bad Request | `aggregate` の `sum`／`avg` を `DATE`／`TIMESTAMP` 列に指定した場合（SQL-26、Issue #1186。未知関数・非決定的関数の呼び出しは SQL 表層専用で到達しない） |
| `28000` | `AUTH_REQUIRED` | 401 | Unauthorized | `Authorization` ヘッダ欠落 |
| `28P01` | `AUTH_INVALID` | 401 | Unauthorized | トークン形式不正・失効・セッション未存在 |
| `42501` | `FORBIDDEN_TENANT_MISMATCH` | 403 | Forbidden | NoSQL 表層の実要求からは到達不能（射影のみ production エンコーダで固定。後述） |
| `34000` | `INVALID_CURSOR_NAME` | 404 | Not Found | NoSQL 表層の実要求からは到達不能（カーソル〔`DECLARE`／`FETCH`／`CLOSE`〕は SQL 表層専用で op 許可リストに無い。後述） |
| `42P01` | `TABLE_NOT_FOUND` | 404 | Not Found | 未定義テーブルへの `search`／`scan`／`aggregate`／`insert` |
| `P0002` | `ROW_NOT_FOUND` | 404 | Not Found | NoSQL 表層の実要求からは到達不能（対応する op が許可リストに無い。後述） |
| `23503` | `FOREIGN_KEY_VIOLATION` | 409 | Conflict | `FOREIGN KEY`（TABLE-17・TASK-205）を宣言したテーブルへの `insert`／`update` で参照先の値が同一テナント内に無い、または参照先の `update`／`delete` で参照元の行が残る |
| `23505` | `UNIQUE_VIOLATION` | 409 | Conflict | 行 `id` の重複、`PRIMARY KEY`／UNIQUE 制約のテナント内一意性違反（`insert`／`update`）。行制約由来のため commit 済みの根拠にならない |
| `23505` | `DUPLICATE_OPERATION_ID` | 409 | Conflict | 台帳照合で内容一致と判定された `operation_id` の再送（`insert`／`update`／`delete`。`wire_code` は `UNIQUE_VIOLATION` と共有し `code` で区別。commit 済み確定の根拠。Issue #1180） |
| `23514` | `CHECK_VIOLATION` | 409 | Conflict | `insert`／`update` が書き込む行が `CHECK` 制約（TABLE-16・TASK-204）を満たさない |
| `42P07` | `DUPLICATE_TABLE` | 409 | Conflict | NoSQL 表層の実要求からは到達不能（`CREATE TABLE`／`CREATE VIEW`／`CREATE INDEX` は op 許可リスト外。後述） |
| `54000` | `PAYLOAD_TOO_LARGE` | 413 | Content Too Large | 要求本文サイズ超過、`filter` 件数超過、INDEX-4 バッチ上限超過、`FOREIGN KEY` 参照アクション連鎖（`ON DELETE CASCADE` 等。宣言が SQL／NoSQL いずれでも。Issue #1148）の深さ・行数上限超過（副作用ゼロ） |
| `XX000` | `INTERNAL_ERROR` | 500 | Internal Server Error | 内部エラー（詳細は非開示。`message` は固定文言へ差し替え） |
| `0A000` | `FEATURE_NOT_SUPPORTED` | 501 | Not Implemented | 語彙外の `op` 指定 |
| `53300` | `CONNECTION_LIMIT_EXCEEDED` | 503 | Service Unavailable | 接続数上限（64）超過、同時有効セッション数上限（256）超過 |
| `55P03` | `LOCK_NOT_AVAILABLE` | 503 | Service Unavailable | SQL 表層の明示トランザクション（SQL-31・TASK-221）が単一ライタを保持している間に、書き込み op（`insert`／`update`／`delete`）が書き込みゲートの待機上限を超えた |

到達不能な 16 分類（`42501`・`34000`・`P0002`・`42701`・`42702`・`42P07`・`2BP01`・
`42809`・`42703`・`42704`・`42804`・`42723`・`25000`・`25001`・`25P01`・`25P02`）の理由: NoSQL 表層はテナントをセッション
（`SessionPrincipal::policy_context()`）からのみ導出し、クライアント自己申告の
`tenant_id` 相当値は JSON／ヘッダ／パスいずれの位置でも `42601` で先に拒否する
ため、`ForbiddenTenantMismatch` を実要求から誘発する経路が構造的に存在しない。
`RowNotFound` に対応する op（更新・削除系）も NoSQL 表層の許可リストに無い。
`InvalidCursorName`（`34000`。WIRE-15・TASK-218）はカーソル（`DECLARE`／
`FETCH`／`CLOSE`）専用の分類だが、NoSQL 表層の `op` 許可リストにカーソル
操作は無いため実要求からは到達しない。
`DuplicateColumn`（`42701`）・`DuplicateTable`（`42P07`）は `CREATE TABLE`
（SQL-23・TASK-202・Issue #899）・`ALTER TABLE ADD COLUMN`（`42701` のみ。
Issue #900）が誘発する分類だが、NoSQL 表層の `op` 許可リストに DDL 相当が
無いため実要求からは到達しない（`docs/design/sql-create-table.md`・
`docs/design/sql-alter-table-add-column.md` 参照）。`AmbiguousColumn`（`42702`。
SQL-28・RLS-10）は複数テーブル参照スコープの束縛基盤（`sql::relation`）が
新設した分類で、`INNER JOIN`（Issue #925）により SQL 表層からは到達可能に
なった（`docs/design/inner-join.md` 参照）。ただし NoSQL 表層の `op` 許可
リストに JOIN 相当が無いため、実要求（HTTP API 経由）からは引き続き到達
しない。`DuplicateFunction`（`42723`。SQL-26、Issue #1186）は
`CREATE FUNCTION`／WASM UDF 登録の名前衝突で、NoSQL `op` 許可リストに関数
登録が無いため到達しない。`CREATE VIEW`／`DROP VIEW`（TABLE-18・SQL-23・
TASK-205、Issue #909）は SQL 表層専用の DDL で、NoSQL `op` 許可リストに
`view` 相当の語彙が無いため `42P07`（名前衝突を `CREATE TABLE` と共有）・
`2BP01`・`42809` も同様に到達不能。`CREATE INDEX`／`DROP INDEX`（INDEX-7・
SQL-23・TASK-206、Issue #908）も SQL 表層専用の DDL で、`42P07`（名前衝突を
共有）・`42703`・`42704` は同様に到達不能。`FOREIGN KEY`（TABLE-17・TASK-205、
Issue #907）は SQL 表層の `CREATE TABLE` に加え、NoSQL `create_table` からも
参照アクション込みで宣言できる（Issue #1148）ため、宣言時検査の `42830`・
連鎖適用後の `23503`・連鎖上限超過の `54000` のいずれも到達する（上表。
`crates/wire-server/tests/nosql13_ddl.rs`・`crates/wire-server/tests/
err4_http_projection.rs` の `err4_f_foreign_key_violation_reachable_via_*`・
`err4_f_invalid_foreign_key_reachable_via_nosql_create_table`・
`err4_f_referential_action_limit_reachable_via_nosql_delete`）。
集合演算（`UNION`／`UNION ALL`／`INTERSECT`／`EXCEPT`。SQL-29 (c)・
RLS-10 (b)・TASK-213、Issue #929）も SQL 表層専用で、NoSQL `op` 許可リストに
対応する語彙が無いため `42804` は到達しない。
参照先が他テナントにだけ存在する場合と、どのテナントにも存在しない場合の
応答は区別できない（RLS-9・RLS-10）。`CASE`／`COALESCE`／`NULLIF`
（対象ビヘイビア: SQL-26、Issue #921）は SQL 表層の式レーン専用の構文で、
NoSQL 表層の式レーンの入口（`plan`／`filter`）にこれらの構文は無いため
`42804` は到達しない。明示トランザクション（SQL-31・TASK-221）の
`BEGIN`／`COMMIT`／`ROLLBACK` は SQL 表層専用の機構で、NoSQL 表層の `op`
許可リストにトランザクション制御に対応する語彙が無いため、その状態エラー
（`25xxx`）は到達しない（一方、ロック待ちの `55P03` は SQL 表層のトランザク
ションがライタを保持している間の NoSQL 書き込みで発生しうる。上表参照）。
テナント境界の検査を緩める・バイパスする production 経路をこれらの分類のために
新設することはせず（`.claude/rules/security.md` P0）、射影表としての一致のみを
production の応答エンコーダ経由で固定する。

本節の各 op スキーマ節（[op 別スキーマ](#op-別スキーマ)・
[`filter` 配列](#filter-配列)・[`explain`](#explain)）では引き続き
`wire_code` のみを示す。HTTP ステータスを引く際は本表を参照すること。

### 緊急応答の `data`

`data` キーは緊急応答（ERR-5・`RECOVER-5` (3) ポインタ。commit 成功境界を
跨いだ panic 時に「commit は成功しているかもしれない」ことを伝える契約）
にのみ付き、通常応答の直後の 3 キーに加えて
`"data":{"state":"may_be_committed"}` を追加する（golden 例は
[本文仕様](#本文仕様)を参照）。SQL 表層側（`ErrorResponse` の `D` フィールド
`state=may_be_committed`）と状態語を共有しており、両表層で乖離しないことを
テストで固定している。

**現状の到達性**: `http::response::encode_error_may_be_committed` を呼び出す
production 経路は本リポジトリの `http/` 配下にまだ存在せず、commit 成功境界を
跨いだ panic は RECOVER-8 の panic hook が既にプロセス abort へ倒すため、
NoSQL 表層の実要求からは `data` 付き応答は現時点で観測できない。エンコーダの
契約としては予約済みであり、production 経路が接続された場合に備えてここに
記載する。

利用者向け指針（接続され次第有効になる契約。現時点では観測不能）:
`data.state == "may_be_committed"` を受け取った場合、同一 `operation_id` で
`insert` を再送し、台帳照合の結果（内容一致なら `23505`・不一致なら
`22023`）で確定させる。これは `insert` の既存の再送契約をそのまま使うもので
あり、`data` 付き応答専用の新たな再送契約を追加するものではない。

## curl 例

```sh
# 1) セッション発行
TOKEN=$(curl -s -X POST http://127.0.0.1:5432/v1/session \
  -H 'Content-Type: application/json' \
  -d '{"user":"alice","password":"pw-alice"}' | \
  sed -n 's/.*"token":"\([^"]*\)".*/\1/p')

# 2) 検索
curl -s -X POST http://127.0.0.1:5432/v1/query \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10}'

# 3) セッション終了
curl -s -X POST http://127.0.0.1:5432/v1/session/close \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{}'
```

（`alice`／`pw-alice` は既存テストと同じダミー資格情報の一例。実運用では
`--users` で構成したストアの実クレデンシャルを使う。）

## 検証コード索引

- 転送路: `crates/wire-server/tests/http1_surface_select.rs`・
  `http2_framing.rs`・`http3_content_type.rs`・`http11_limits.rs`・
  `http12_fail_closed.rs`・`http_limits.rs`
- セッション: `http4_session.rs`・`http4_session_issue.rs`・
  `http5_query_bearer.rs`・`http6_auth_failure.rs`・`http8_session_close.rs`
- ルーティング・op 許可リスト・スキーマ: `nosql1_endpoint_routing.rs`・
  `nosql1_op_vocabulary.rs`・`nosql9_op_allowlist.rs`・`nosql8_schema_validation.rs`
- `search`: `nosql2_search.rs`・`nosql2_search_binding.rs`・`nosql10_explain.rs`・
  `wire_using_plan.rs`・`wire_explain.rs`
- `scan`: `nosql3_scan_mapping.rs`・`nosql3_scan_wire_parity.rs`・
  `nosql15_scan_sort.rs`（`sort`。Issue #946・NOSQL-15）・
  `nosql15_offset.rs`（`offset`。Issue #947・NOSQL-15）
- `aggregate`: `nosql4_aggregate.rs`・`nosql5_group_by.rs`・
  `nosql4_5_aggregate_wire_parity.rs`・`nosql16_multi_group_by.rs`・
  `nosql15_aggregate_sort_offset.rs`（`sort`／`offset`・単一文字列形 `group_by`。
  Issue #1198・NOSQL-15）
- `explain`（`vector` 指定 `search`・`scan`・`aggregate` への対象拡大。
  Issue #948・NOSQL-16・SQL-27）: `nosql16_explain_targets.rs`
- `insert`: `nosql6_insert.rs`・`nosql6_tenant_row_id_scope.rs`・
  `http_insert_response_boundary.rs`・`wire_insert_operation_id.rs`
- `filter`: `nosql7_filter_mapping.rs`
- 応答形: `nosql11_response_schema.rs`
- エラー射影: `err4_http_projection.rs`・`nosql_api_doc.rs`
- 層 B（無改造の外部 HTTP クライアント。SQL 経路〔`psql`〕との search／
  scan／aggregate 結果一致比較を含む・Issue #779）: `three_client_http_e2e.rs`・
  `tests/three_client_http/{urllib_client.py,fetch_client.js}`
  （`make e2e-three-client-http`。opt-in・`ci` 非包含）。実行記録の様式は
  `docs/design/three-client-e2e-harness.md` 参照
- 層 B パリティ総合検証（Phase 7 機能・insert・DDL 3 op を含む `Op::ALL`
  全網羅・Issue #950）: 同じ `three_client_http_e2e.rs` の
  `PARITY_CASES`（範囲比較・`IN`・`OR`・`sort`・`offset`・複数列
  `group_by`）・`REJECTION_CASES`・`run_phase7_write_parity_scenario`
  （insert／DDL／述語形 `delete`）・`parity_matrix_covers_every_nosql_op`
  （op カバレッジガード。`#[ignore]` なし・常時 `make ci`）。詳細は
  `docs/design/three-client-e2e-harness.md`「パリティ総合検証（Issue
  #950）」節参照

## spec 側への申し送り候補

以下は既存モジュールコメントが「spec 判断」として明示している事項の一覧で
あり、本文書執筆にあたって新たな判断は行っていない:

- `hybrid.text` の疎側テキスト列を JSON で選択可能にするか（現状は `body`
  固定）
- `/v1/session/close` 成功応答のキー名（`{"closed":true}`。実装既定値）
- `/v1/query` 配下のパス上 `tenant_id` マーカーと未知ターゲットの優先順位
  （本リポの実装判断であり spec 側での明文化は未定）
- 集計 `id` 列等の巨大整数（`u64`。2^53 超）を JSON number としてそのまま返す
  ことの是非（文字列化への変更は spec 側判断に委ねられている）
- `aggregate` への `sort` は Issue #1198 で解消済み（`group_by` 必須・複数キー
  〔上限 8〕対応。`group_by` なしの単一行集計への `sort`／`offset` は SQL 表層に
  受理形がないため `42601` を維持しており、spec 側で扱いを明文化するかは
  申し送り候補）
- `sort[].dir` を必須・小文字完全一致（`"asc"`／`"desc"`）とした実装既定
  （Issue #946。`filter[].op`・`having[].op` と同じ厳格な語彙判断を踏襲）
