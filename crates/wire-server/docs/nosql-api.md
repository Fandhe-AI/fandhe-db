# NoSQL API

`wire-server --surface nosql` が公開する 3 エンドポイント（`POST /v1/session`・
`POST /v1/session/close`・`POST /v1/query`）と、`POST /v1/query` の `op` 9 値
（`search`／`scan`／`aggregate`／`insert`／`update`／`delete`／`create_table`／
`alter_table`／`drop_table`）の JSON スキーマを利用者向けに整理する。
`update`／`delete` は `where`（単一行・`id` 完全一致形）は束縛・実行結線済みで、
`filter`（述語形）は語彙・スキーマ検証のみ実装済み（実行結線は未実装。後述の
各節参照）。`create_table`／`alter_table`／`drop_table`（DDL 3 op）は
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

### `scan`

順序保証なしの広域取得（SQL-15 の bare 形 `SELECT ... [WHERE ...] LIMIT n` と
同一実行意味論。`limit` 件到達で早期終了・取得モード非適用）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"scan"` |
| `table` | ○ | string | |
| `limit` | ○ | number | `1..=10000`。非整数・負値・`u32` 超過等の形状不正は `42601`、`0` または `10001` 以上の範囲外は `22000` |
| `filter` | △ | object[] | |
| `columns` | △ | string[]（非空） | 省略時は `id`＋全実列 |
| `explain` | △ | bool | `true` は `42601`（拒否。後述）。`false`／省略時は通常実行 |

`vector`／`plan`／`mode`／`hybrid` はスキーマが宣言しないフィールドのため、
未知キーとして `42601` になる（`scan` への付与自体を個別に判定するロジックは
持たない）。

要求例:

```json
{"op": "scan", "table": "docs", "limit": 10, "columns": ["id", "lang"],
 "filter": [{"column": "lang", "op": "eq", "value": "ja"}]}
```

応答には `score` 列相当が一切含まれない（`ORDER BY`／`hybrid` を経由しないため
合成スコア列が構造上存在しない）。

`explain: true` は NOSQL-16（Issue #948）で対応するまで `42601`（SQL 表層は
Issue #922・SQL-27 で `scan` 相当の広域取得への `EXPLAIN` 前置を受理済みだが、
NoSQL 表層の束縛済み計画向けエントリはまだこれを結線していない）。

### `aggregate`

単一行集計（`GROUP BY` なし）と `GROUP BY`／`HAVING` 集計の両方を、SQL テキストを
一切組み立てずに束縛・実行する（SQL-13・SQL-14 と同一の実行計画）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"aggregate"` |
| `table` | ○ | string | |
| `aggregates` | ○ | object[]（`{"fn","column"}`。1〜32 要素） | `fn` は `count`／`sum`／`avg`／`min`／`max`（小文字完全一致）。`column` は列名、または `count` 専用の `"*"` |
| `filter` | △ | object[] | |
| `group_by` | △ | string[]（ちょうど 1 要素） | `TEXT` 列限定 |
| `having` | △ | object[]（`{"fn","column","op","value"}`） | `group_by` 必須。`op` は `=`／`<`／`<=`／`>`／`>=` の完全一致 |
| `explain` | △ | bool | `true` は `42601`（拒否）。`false`／省略時は通常実行 |

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

主な `wire_code`:

- `aggregates` が空配列 → `group_by`／`having` の有無を問わず一律 `42601`
  （`having` の参照解決より必ず先に検査する）
- `group_by` 要素数が 1 でない・`having` のみ単独指定（`group_by` なし）・
  `fn`／`op` が語彙外・識別子形状不正 → `42601`
- グループ数上限（10,000）・グループキー累計バイト・`having` 述語数上限超過
  → `54000`
- `group_by` 列が `TEXT` 列でない・`having` が `MIN`/`MAX(<TEXT列>)` を参照・
  参照先が `aggregates` に存在しない／曖昧 → `22000`
- `VECTOR` 列の集計: `count` は列の裸の列参照を受理し非 `NULL` 行数を数える
  （`resolve_aggregate_input` の `AggregateInput::VectorColumnPresence`）。
  `sum`／`avg`／`min`／`max` は同じ `VECTOR` 列参照を一律 `22000` で拒否
- `sum` オーバーフロー → `22003`

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
  （小数・非数値は `42601`。範囲外は `22003`）、`REAL`／`DOUBLE PRECISION`
  は JSON 数値（範囲外は `22003`）、`NUMERIC` は JSON 数値または数値文字列
  （桁あふれは `22003`）、`BOOLEAN` は JSON 真偽値、`DATE`／`TIMESTAMP`／
  `UUID` は JSON 文字列（形式不正はそれぞれ `22000`／`22000`／`22P02`）、
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
- 同一 `operation_id` の再送: 内容が一致すれば `23505`、不一致なら `22023`
  （台帳照合。TASK-101・RECOVER-10 の再送判定を透過する）
- 同一テナント内の `id` 重複は `23505`（他テナントの同 `id` とは衝突せず、
  応答は「不在時」と同一——TABLE-12・RLS-9）
- `rows` の行数上限は既定 64（`EngineCore::execute_bound_insert_in_session` が
  `rows.len()` を INDEX-4 の件数上限相当として判定。環境変数
  `VECTOR_DB_BATCH_MAX_FILES` で上書き可能）。超過は `54000`
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
（`(tenant, table, operation_id)`）へ到達する。`filter`（述語形）は語彙・
スキーマ検証のみ実装済みで、**実行結線は未実装**（Issue #871 の担当）の
ため、指定すると常に `0A000`／501（固定文言。`gate.rs::PLACEHOLDER_MESSAGE`
とは異なる文言）が返る。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"update"` |
| `table` | ○ | string | |
| `set` | ○ | object（任意キー・非空） | 列名をキーに持つ部分更新。`id`／`tenant_id`／`visibility` は `42601`。`TEXT` 列は JSON 文字列、`VECTOR` 列は数値配列（**文字列形のベクトルリテラルは受理しない**）のみ受理。他の列型は `insert` と同じ JSON 表現（Issue #896・NOSQL-17。上記参照）。`null` は列型を問わず nullable 判定込みで `bind_update` へ委譲する（非 nullable 列への `null` は `22000`）。型不一致・未知列・次元不一致は `22000`（新型の型不一致は `42601`） |
| `where` | △ | `{"id": number}` | 単一行・`id` 完全一致形。小数は `22000`、負数は `42601`（SQL 表層の字句解析・束縛とのパリティ。詳細は design doc 参照）。`filter` との排他（両方・双方欠落はいずれも `42601`） |
| `filter` | △ | object[] | [`filter` 配列](#filter-配列)参照。指定のみ（`where` 欠落）だと `0A000`（実行器未接続） |
| `operation_id` | △ | string | 欠落・`null`・空文字は `23502`。同一値への再送は台帳照合により内容一致 `23505`・不一致 `22023`（SQL 表層と共有） |

成功応答: `{"updated":<n>,"operation_id":"<echo>"}`（`n` は `0` または `1`。
他テナント所有 id・未存在 id はいずれも `updated:0`・`200` で応答バイト列が
完全一致する。RLS-9）。

複数列 `set` は JSON パース時点でキーのアルファベット順へ正規化される一方、
SQL 表層の `UPDATE ... SET col1 = .., col2 = ..` はクライアントが記述した
宣言順をそのまま保持する。台帳の内容照合ハッシュはこの列の記述順に依存
しないようスキーマの列定義順へ正規化済み（PR #992）のため、SQL 表層が
アルファベット順でない宣言順で書いた `UPDATE` と同一値の NoSQL `update`
は、同一 `operation_id` への再送であれば内容一致の再送（`23505`）として
正しく判定される（詳細は `docs/design/nosql-update-delete-mapping.md`
「複数列 `set` の宣言順と `content_hash`」節参照）。

要求例（`where` 形）:

```json
{"op": "update", "table": "docs", "set": {"lang": "en"}, "where": {"id": 1},
 "operation_id": "op-1"}
```

### `delete`

`where`（単一行・`id` 完全一致形）は束縛・実行結線済み（Issue #876・
TASK-186・NOSQL-6・NOSQL-12）で、SQL 表層 `DELETE FROM ... WHERE id = <n>
USING OPERATION_ID`（SQL-18）と同一の実行器（`engine::sql::exec::
execute_delete`）・同一の台帳キー空間を共有する。`filter`（述語形）は
`update` と同様に語彙・スキーマ検証のみ実装済みで `0A000`／501 が返る
（Issue #871 の担当）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"delete"` |
| `table` | ○ | string | |
| `where` | △ | `{"id": number}` | 単一行・`id` 完全一致形。小数は `22000`、負数は `42601`（SQL 表層とのパリティ）。`filter` との排他（両方・双方欠落はいずれも `42601`） |
| `filter` | △ | object[] | [`filter` 配列](#filter-配列)参照。指定のみだと `0A000`（実行器未接続） |
| `operation_id` | △ | string | 欠落・`null`・空文字は `23502`。同一値への再送は台帳照合により `23505`（内容一致。`DELETE` は行の有無に関わらず同一内容） |

成功応答: `{"deleted":<n>,"operation_id":"<echo>"}`（`n` は `0` または `1`。
他テナント所有 id・未存在 id はいずれも `deleted:0`・`200` で応答バイト列が
完全一致する。RLS-9）。

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
| `constraints`（`create_table`） | △ | object[] | `{"kind":"primary_key"｜"unique"｜"foreign_key"｜"check","columns"?,"references"?}`。`references`＝`{"table","columns"?}`。`check` は `0A000`（述語の JSON 写像は別論点。後続 Issue の担当） |
| `add_column`（`alter_table`） | △ | object | `{"name","type","dim"?,"precision"?,"scale"?,"enum_type"?}`。`drop_column` と排他必須（両方・双方欠落は `42601`） |
| `drop_column`（`alter_table`） | △ | object | `{"name"}`。SQL 表層の `ALTER TABLE ... DROP COLUMN` が未結線のため常に `0A000` |

成功応答は 3 op 共通で `{"ok":true}`（行数・件数を返さない）。

要求例（`create_table`）:

```json
{"op": "create_table", "table": "docs", "columns": [
  {"name": "embedding", "type": "vector", "dim": 3},
  {"name": "lang", "type": "text", "nullable": true}
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

`search`／`scan`／`aggregate` 共通で使える事前フィルタ配列。

```json
[{"column": "lang", "op": "eq", "value": "ja"}]
```

- 各要素は `column`（文字列）・`op`（文字列）・`value`（文字列・数値・真偽値の
  いずれか。Issue #896・NOSQL-17）の 3 つの必須フィールドのみ
- `op` は `eq`（一致）・`prefix`（前方一致）の 2 語彙のみ（`or`・否定・範囲比較の
  構文は存在しない）。複数要素は常に AND 結合
- 要素数は 256 個まで（超過は `54000`）
- `column` にサーバー側 RLS 述語名相当（`visible`／`visible()`。大文字小文字
  非区別）を指定する経路は `42601`（RLS はサーバー側暗黙適用のみで、クライアント
  は述語を書けない）
- `eq` は対象列の型に応じたレーンへ振り分ける（Issue #896・NOSQL-17。詳細は
  `docs/design/nosql-typed-json-binding.md`「filter（`eq` の型別レーン）」節
  参照）: `TEXT`（旧来型。値・型不一致は `42601`。insert/update の「TEXT は旧来型
  = `22000`」非対称は filter には適用しない）／`ENUM`（`42601`。語彙外は
  `22P02`）／`BOOLEAN`（`42601`）／`DATE`・`TIMESTAMP`・`UUID`（`42601`。形式・
  範囲は engine 側で検証）／`BYTEA`（base64 の JSON string。`42601`／`54000`。
  復号後 4 MiB 超で `54000`——`insert`／`update`・SQL 表層
  `WHERE bytea_col = '\x...'` と同じ実効上限（PR #1038 で hex 再エンコード後
  長を検査していた過小上限を是正済み。詳細は
  `docs/design/nosql-typed-json-binding.md`「BYTEA の実効長（PR #1038 是正）」節
  参照）／
  `NUMERIC`（数値または数値文字列。`42601`）。`INTEGER`／`BIGINT`／`REAL`／
  `DOUBLE PRECISION` 列への `eq` は対象外（`0A000`。式レーンの入口が無いため。
  Issue #945）
- `prefix` は従来どおり `TEXT` 列限定（他の列型は `22000`）
- 未知列・`VECTOR`／`ARRAY`／`JSON`／`JSONB` 列拒否（`22000`）は
  `engine::declarative_filter` の既存契約をそのまま透過する

検証コード: `crates/wire-server/tests/nosql7_filter_mapping.rs`。

## `explain`

`op: search` かつ `plan` 指定かつ `explain: true` のときのみ、検索本体を実行せず
SQL `EXPLAIN SELECT ... USING PLAN(...)` と同一内容を返す。

要求例:

```json
{"op": "search", "table": "docs", "plan": "find content", "limit": 10,
 "explain": true}
```

応答例（`200`）:

```json
{"explain": ["<QUERY PLAN の行>", "..."]}
```

- `vector` 指定＋`explain: true` → `42601`
- `plan` 欠落＋`explain: true`（`vector`も欠落）→ `42601`
- `scan`／`aggregate` への `explain: true` → `42601`（NOSQL-16・Issue #948 で
  対応するまで。別 op なので `search` の上記条件とは独立に、各 op のハンドラが
  拒否する）

検証コード: `crates/wire-server/tests/nosql10_explain.rs`・`wire_explain.rs`。

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
| `SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10`（広域取得 SQL-15） | `scan` |
| `SELECT COUNT(*), SUM(id) FROM docs` | `aggregate` |
| `SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING count >= 2` | `aggregate` + `group_by` + `having` |
| `INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-1'` | `insert` + `operation_id` |
| `UPDATE docs SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-1'` | `update` + `where.id` + `operation_id`（結線済み。同一実行器・同一台帳キー空間） |
| `UPDATE docs SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-1'` | `update` + `filter` + `operation_id`（語彙・スキーマのみ実装済み。実行結線は Issue #871 の担当） |
| `DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'op-1'` | `delete` + `where.id` + `operation_id`（結線済み。同一実行器・同一台帳キー空間） |
| `DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID 'op-1'` | `delete` + `filter` + `operation_id`（語彙・スキーマのみ実装済み。実行結線は Issue #871 の担当） |
| `CREATE TABLE docs (embedding VECTOR(3), lang TEXT)` | `create_table` + `columns`（Issue #910。同一実行器） |
| `ALTER TABLE docs ADD COLUMN note TEXT` | `alter_table` + `add_column`（Issue #910。同一実行器） |
| `DROP TABLE docs` | `drop_table`（Issue #910。同一実行器） |

対応の無いもの（NoSQL 側に受理形が存在しない。実際の応答は語彙外 `op` として
`0A000`、または未知キーとして `42601`）:

- `HINT ORDER(...)` によるソフトブースト
- UDF 呼び出し・`CREATE FUNCTION`
- `SET`（`search_mode` 等のセッション変数設定）
- 定数のみの `SELECT`
- `ORDER BY` 形／集計／広域取得への `EXPLAIN` 前置（`USING PLAN` 付き検索
  `SELECT` への `EXPLAIN` のみ受理）
- `LIKE` の前方一致（`prefix`）以外の一致方式（SQL 表層は Issue #914・SQL-24 で
  中間一致・後方一致・`_` を受理するが、NoSQL `filter` 側は未対応のまま。
  NoSQL 側の対応は NOSQL-14 の担当）
- `INSERT` のファイル形（`path`／`body` 列指定の増分インデックス投入）
- `GROUP BY` への `ORDER BY`／`LIMIT` の付与
- `ALTER TABLE ... DROP COLUMN`（SQL 表層が未結線。`alter_table.drop_column` は `0A000`）
- `CREATE TABLE` の `CHECK` 制約（`create_table.constraints[].kind == "check"` は `0A000`）
- `CREATE INDEX`／`DROP INDEX`／`CREATE VIEW`／`DROP VIEW`（NOSQL-13 の対象外）

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
16 分類が共有する）。

| `wire_code` | `code` | HTTP ステータス | 理由句 | NoSQL 表層での主な発生源 |
| --- | --- | --- | --- | --- |
| `08P01` | `PROTOCOL_VIOLATION` | 400 | Bad Request | 要求行・ヘッダ形状違反、未知ターゲットへのアクセス |
| `22000` | `INVALID_INPUT` | 400 | Bad Request | `op` 別スキーマ検証での値の型・形状不正 |
| `22003` | `NUMERIC_OUT_OF_RANGE` | 400 | Bad Request | 集計（`aggregate`）でのオーバーフロー |
| `22008` | `DATETIME_FIELD_OVERFLOW` | 400 | Bad Request | `DATE`／`TIMESTAMP` リテラルの範囲外・暦上不正（`update` の `set` 経由） |
| `22023` | `OPERATION_ID_CONTENT_MISMATCH` | 400 | Bad Request | `insert` の `operation_id` 再送時の内容不一致 |
| `22P02` | `INVALID_TEXT_REPRESENTATION` | 400 | Bad Request | ENUM 列の語彙外ラベル（`insert`／`update`／`filter`） |
| `23502` | `MISSING_OPERATION_ID` | 400 | Bad Request | `insert` の `operation_id` 欠落 |
| `23502` | `NOT_NULL_VIOLATION` | 400 | Bad Request | `NOT NULL` 列（TABLE-16・TASK-204）への `insert`／`update` での省略・明示 `null` |
| `25000` | `INVALID_TRANSACTION_STATE` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（明示トランザクション制御が op 語彙に無い。後述） |
| `25001` | `ACTIVE_SQL_TRANSACTION` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（同上） |
| `25P01` | `NO_ACTIVE_SQL_TRANSACTION` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（同上） |
| `25P02` | `IN_FAILED_SQL_TRANSACTION` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（同上） |
| `2BP01` | `DEPENDENT_OBJECTS_STILL_EXIST` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`DROP TABLE`／`DROP VIEW` は SQL 表層専用の DDL。後述） |
| `42601` | `UNSUPPORTED_SQL_SYNTAX` | 400 | Bad Request | JSON 構文エラー、`op` 別スキーマ違反、`tenant_id` 相当値の自己申告 |
| `42701` | `DUPLICATE_COLUMN` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`CREATE TABLE`・`ALTER TABLE ADD COLUMN` は op 許可リスト外。後述） |
| `42702` | `AMBIGUOUS_COLUMN` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（複数テーブル参照スコープの束縛基盤〔SQL-28・RLS-10、Issue #924〕は未結線。後述） |
| `42703` | `UNDEFINED_COLUMN` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`CREATE INDEX` は op 許可リスト外。後述） |
| `42704` | `UNDEFINED_OBJECT` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`DROP INDEX` は op 許可リスト外。後述） |
| `42809` | `WRONG_OBJECT_TYPE` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`DROP TABLE`／`DROP VIEW`・ビューへの書き込みは SQL 表層専用の DDL。後述） |
| `42830` | `INVALID_FOREIGN_KEY` | 400 | Bad Request | NoSQL 表層の実要求からは到達不能（`FOREIGN KEY` の宣言は SQL 表層専用の `CREATE TABLE`。後述） |
| `28000` | `AUTH_REQUIRED` | 401 | Unauthorized | `Authorization` ヘッダ欠落 |
| `28P01` | `AUTH_INVALID` | 401 | Unauthorized | トークン形式不正・失効・セッション未存在 |
| `42501` | `FORBIDDEN_TENANT_MISMATCH` | 403 | Forbidden | NoSQL 表層の実要求からは到達不能（射影のみ production エンコーダで固定。後述） |
| `34000` | `INVALID_CURSOR_NAME` | 404 | Not Found | NoSQL 表層の実要求からは到達不能（カーソル〔`DECLARE`／`FETCH`／`CLOSE`〕は SQL 表層専用で op 許可リストに無い。後述） |
| `42P01` | `TABLE_NOT_FOUND` | 404 | Not Found | 未定義テーブルへの `search`／`scan`／`aggregate`／`insert` |
| `P0002` | `ROW_NOT_FOUND` | 404 | Not Found | NoSQL 表層の実要求からは到達不能（対応する op が許可リストに無い。後述） |
| `23503` | `FOREIGN_KEY_VIOLATION` | 409 | Conflict | `FOREIGN KEY`（TABLE-17・TASK-205）を宣言したテーブルへの `insert`／`update` で参照先の値が同一テナント内に無い、または参照先の `update`／`delete` で参照元の行が残る |
| `23505` | `UNIQUE_VIOLATION` | 409 | Conflict | `insert` の `operation_id` 重複（内容一致の再送）、`PRIMARY KEY`／UNIQUE 制約のテナント内一意性違反（`insert`／`update`） |
| `23514` | `CHECK_VIOLATION` | 409 | Conflict | `insert`／`update` が書き込む行が `CHECK` 制約（TABLE-16・TASK-204）を満たさない |
| `42P07` | `DUPLICATE_TABLE` | 409 | Conflict | NoSQL 表層の実要求からは到達不能（`CREATE TABLE`／`CREATE VIEW`／`CREATE INDEX` は op 許可リスト外。後述） |
| `54000` | `PAYLOAD_TOO_LARGE` | 413 | Content Too Large | 要求本文サイズ超過、`filter` 件数超過、INDEX-4 バッチ上限超過 |
| `XX000` | `INTERNAL_ERROR` | 500 | Internal Server Error | 内部エラー（詳細は非開示。`message` は固定文言へ差し替え） |
| `0A000` | `FEATURE_NOT_SUPPORTED` | 501 | Not Implemented | 語彙外の `op` 指定 |
| `53300` | `CONNECTION_LIMIT_EXCEEDED` | 503 | Service Unavailable | 接続数上限（64）超過、同時有効セッション数上限（256）超過 |
| `55P03` | `LOCK_NOT_AVAILABLE` | 503 | Service Unavailable | SQL 表層の明示トランザクション（SQL-31・TASK-221）が単一ライタを保持している間に、書き込み op（`insert`／`update`／`delete`）が書き込みゲートの待機上限を超えた |

到達不能な 15 分類（`42501`・`34000`・`P0002`・`42701`・`42702`・`42P07`・`2BP01`・
`42809`・`42703`・`42704`・`42830`・`25000`・`25001`・`25P01`・`25P02`）の理由: NoSQL 表層はテナントをセッション
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
SQL-28・RLS-10、Issue #924）は複数テーブル参照スコープの束縛基盤（`sql::relation`）
が新設する分類だが、許可リストは本 Issue でも引き続き JOIN・複数 FROM を
`42601` で拒否し、`EngineCore` への結線も行わないため実要求からは到達しない
（`docs/design/multi-relation-plan-foundation.md` 参照）。`CREATE VIEW`／`DROP VIEW`（TABLE-18・SQL-23・
TASK-205、Issue #909）は SQL 表層専用の DDL で、NoSQL `op` 許可リストに
`view` 相当の語彙が無いため `42P07`（名前衝突を `CREATE TABLE` と共有）・
`2BP01`・`42809` も同様に到達不能。`CREATE INDEX`／`DROP INDEX`（INDEX-7・
SQL-23・TASK-206、Issue #908）も SQL 表層専用の DDL で、`42P07`（名前衝突を
共有）・`42703`・`42704` は同様に到達不能。`FOREIGN KEY`（TABLE-17・TASK-205、
Issue #907）の宣言も SQL 表層専用の `CREATE TABLE` で行うため `42830` は到達
しないが、宣言済みテーブルへの `insert`／`update`／`delete` op は engine 内の
単一検査点を通るため `23503` は到達する（上表。`crates/wire-server/tests/
err4_http_projection.rs` の `err4_f_foreign_key_violation_reachable_via_*`）。
参照先が他テナントにだけ存在する場合と、どのテナントにも存在しない場合の
応答は区別できない（RLS-9・RLS-10）。明示トランザクション（SQL-31・TASK-221）の
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
- `scan`: `nosql3_scan_mapping.rs`・`nosql3_scan_wire_parity.rs`
- `aggregate`: `nosql4_aggregate.rs`・`nosql5_group_by.rs`・
  `nosql4_5_aggregate_wire_parity.rs`
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
