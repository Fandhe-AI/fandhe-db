# 読み取りタイムアウト・最大接続数・prepared statement 件数の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（設計段階。実装は #2454、境界・非連動のテストは #2455 で実装済み。WIRE-25・WIRE-5・WIRE-6・WIRE-11・SQL-46 のポインタのみ。spec 本文は転記しない）
- 親: TASK-338（`docs/spec/05-tasks.md`）、WIRE-25（`docs/spec/04-behavior/wire-protocol.md`）
- 対象フラグ: `--read-timeout-secs`・`--max-connections`・`--max-prepared-statements`
- 先行: [transaction-limits-cli.md](transaction-limits-cli.md)・[write-lock-wait-cli.md](write-lock-wait-cli.md)・[result-limits-cli.md](result-limits-cli.md)と同型。接続モデルは [wire-connection-model.md](wire-connection-model.md)、明示トランザクションは [explicit-transaction.md](explicit-transaction.md)

## 背景

次の 3 値は `crates/wire-server/src/limits.rs` の定数（`READ_TIMEOUT` = 30 秒、`MAX_CONNECTIONS` = 64、`MAX_PREPARED_STATEMENTS_PER_SESSION` = 64）で、運用者が起動時に変えられなかった。プロセス単位の起動フラグとして設定できるようにする。本メモは #2454・#2455 の仕様となる。

## 決定

| # | 内容 |
| - | ---- |
| D1 | フラグ名は `--read-timeout-secs <1-600>`（秒）、`--max-connections <1-256>`、`--max-prepared-statements <1-1024>`。読み取りタイムアウトだけ兄弟フラグ（`-ms`）と違い**秒単位**にする。理由は (a) WIRE-25 の下限が 1 で、ミリ秒の 1 ms は実用にならず TLS ハンドシェイク期限も兼ねる、(b) 既定 30 秒が整数秒で表せる、(c) 兄弟の下限 100 ms を流用すると WIRE-25 の下限と食い違う。この不一致は意図的 |
| D2 | 3 つとも既定値の上下どちらへも設定できる（既定値は構造上の上限ではない。`transaction-limits-cli.md` D1 と同型）。未指定の既定値は従来と同値（30 秒・64・64）で、挙動はビット同一 |
| D3 | ハード上限は暫定値で**オーナー確認待ち**。いずれも独立したリテラル定数で、他の定数を参照しない（SQL-46 (6) の原則）。根拠は下記 |
| D4 | 下限は 3 つとも 1。0 は受け付けない（「無制限」「無効」のどちらにも読めるため。`write-lock-wait-cli.md` D3 と同じ理由） |
| D5 | 定数（`MIN_*`/`DEFAULT_*`/`MAX_*`）と `validate_*` は、wire 層だけの概念なので `wire_server::limits` に置き engine の公開面を増やさない（兄弟フラグが engine に置くのとの意図的な逸脱）。CLI パーサーは新規モジュール（仮称 `connection_limits_opt.rs`。#2454 で確定）とし、ASCII 数字だけを受理する厳密パースで既存の `*_opt.rs` と同型にする（`+`・空白・全角数字・オーバーフローを拒否） |
| D6 | fail-closed: 範囲外・非数値・値欠落・重複指定は bind より前に起動エラーで非 0 終了（last-wins にしない）。環境変数・セッション変数・SQL・NoSQL 要求・テナント単位からは変更できず、プロセス全体・全テナントに同じ値を適用する |
| D7 | 値の運搬: `ConnectionLimiter::new(max)` と accept ループの `read_timeout` 引数は値を受け取る形で、`main.rs` が定数の代わりに解決済みの値を渡す。リミッターは `match surface` の前に 1 回だけ構築して SQL・NoSQL 表層で共有する（現行維持）。prepared statement 件数は `ExtendedQueryState::new()` から `PreparedStatementStore` へ上限値を注入する（グローバル状態を使わない。並列テストの干渉回避） |
| D8 | 書き込みロック待機（SQL-46。`--write-lock-wait-ms`、既定 30 秒）と**連動しない**。既定値が同じ 30 秒なのは偶然で、定数を共有せず、一方から他方を導出せず、両者を比べる起動時警告も出さない。どちらかを変えても他方の解決値は変わらない（#2455 で検証済み） |
| D9 | keepalive を送らない（WIRE-25 (4)）。現行の実装に TCP keepalive の設定は無く、アプリケーション層の死活確認も無い。設定値を超えて無通信の接続は WIRE-5 のとおり応答なしで閉じる |
| D10 | 超過時の応答は従来どおり: 読み取りタイムアウトは応答なしで切断（WIRE-5）、接続数超過は `53300`（WIRE-6。NoSQL は既存の HTTP 射影）、件数超過は `54000`（WIRE-11）。境界は設定値に従い、上限ちょうどまで受理し 1 つ超えたら拒否する。エラー文は英語でフラグ名・値・範囲だけを含む |

### D3 ハード上限の根拠

| フラグ | 上限 | 根拠 |
| ------ | ---- | ---- |
| `--max-connections` | 256 | 1 接続 = 1 スレッド（`thread::Builder::spawn`、既定スタック 2 MiB）。1 接続の最悪常駐量は応答バッファ 1 MiB + prepared SQL 4 MiB + 束縛値 4 MiB + 保留 portal 16 MiB + 受信フレーム 1 MiB で約 28〜30 MiB。256 接続で最悪約 7.5 GiB。Argon2 の KDF メモリは `MAX_CONCURRENT_ARGON2_KDF`（8）で接続数と切り離し済み |
| `--read-timeout-secs` | 600 | 個々の読み取りの待機上限（接続全体の絶対期限ではない）。接続プールのアイドル既定（10 分程度）より短く設定できる余地を残す。SQL 経路では期限内に少量ずつ送る相手が接続を維持できるため、「上限値 × 最大接続数」を枠占有時間の上界とは扱わない。絶対期限が効くのは TLS ハンドシェイクと HTTP 要求読み取り全体のみ（実装メモ 2・3 参照） |
| `--max-prepared-statements` | 1024 | SQL テキスト合計は 4 MiB 上限で件数と独立に有界。件数上限は 1 件ごとのメタデータ（`ParsedSql`・`ParamSlot`・HashMap エントリ）を抑える。文ごとにキャッシュするドライバの一般的な既定（例: pgjdbc の 256）を収められる |

## `READ_TIMEOUT` との連動の洗い出し

### 値を共有する箇所（#2454 で設定値へ置き換える）

1. `server.rs` と `http/listener.rs` の accept ループ: 受理直後の `apply_read_timeout` が読み取りと書き込みの両方へ同じ値を設定する。書き込みタイムアウトも連動する。
2. `tls/server_handshake.rs`: `HANDSHAKE_READ_TIMEOUT = READ_TIMEOUT` は TLS ハンドシェイクの絶対期限。ハンドシェイク成功後にソケットのタイムアウトを**定数**のまま戻している（`handshake.rs` の該当コメント、`http/tls_transport.rs` の同経路も同様）。設定値へ変えないと TLS 経路だけが既定 30 秒に戻り黙って食い違う。**最重要の申し送り**。
3. `http/conn.rs`: `REQUEST_READ_DEADLINE = READ_TIMEOUT` は HTTP 要求読み取り全体の絶対期限（Slowloris 対策）。production 経路は引数で同じ値を渡す契約のため設定値がそのまま効く。定数はテストと doc 用に残る。
4. `server.rs` の非推奨別名 `CONNECTION_IO_TIMEOUT`・`MAX_CONCURRENT_CONNECTIONS`: 既定値の別名として残し、設定値には追随しない。

### 明示トランザクションとの連動（値はソケットから読む形のためコード変更不要）

5. 明示トランザクション中の受信待ち（`handshake::post_auth_loop`・`read_next_frame_header`）は `base_timeout` をソケットから読み、`min(持続時間の残り, 読み取りタイムアウト)` に切り詰める。設定値は自動で反映される。
6. 自動 ROLLBACK（WIRE-5）: 読み取りタイムアウトで接続を閉じると `SessionTransaction` が drop され、共有の書き込みトランザクションが abort してライタが解放される。
   - 読み取りタイムアウトを持続時間上限（既定 20 秒・最大 60 秒）より**短く**すると、無通信の明示トランザクションは持続時間上限より前に切断で終わる。
   - **長く**してもライタの保持は延びない。期限到来で `release_if_expired` が `Failed` へ遷移させ、接続は読み取りタイムアウトまで維持される。
   - `transaction-limits-cli.md` D9(a) の「30 秒」は設定値を指すよう読み替える（#2454 で追随）。
7. `transaction_limits_opt.rs` の既存警告が比べるのは持続時間と書き込みロック待機上限で、読み取りタイムアウトは関与しない。変更しない。「読み取りタイムアウト < 持続時間」の起動時警告は追加しない（早く解放する安全側にしか働かないため）。

### 連動しないもの（固定のまま）

8. `REJECT_WRITE_TIMEOUT`（1 秒）・`REJECT_TLS_PROBE_TIMEOUT`（1 秒）・`EMERGENCY_RESPONSE_WRITE_TIMEOUT`（5 秒）・`SESSION_TTL`（3600 秒）・書き込みロック待機上限。

## 効く経路

| 値 | SQL 表層 | NoSQL（HTTP）表層 | 備考 |
| -- | -------- | ----------------- | ---- |
| 読み取りタイムアウト | 効く | 効く | TLS 経路を含む |
| 最大接続数 | 効く | 効く | 共有リミッター |
| prepared statement 件数 | 効く | 対象外 | NoSQL に相当する概念が無い |

最大接続数と連動しないもの: `MAX_SESSIONS`（256。HTTP セッション）・`MAX_REJECT_WORKERS`（16）・`MAX_CONCURRENT_ARGON2_KDF`（8）。

## SQLSTATE（従来と同じ）

接続数超過 `53300`、prepared statement 件数超過 `54000`。読み取りタイムアウトは応答なしで切断。

## 運用上の注意

- 最大接続数を上げるとスレッドとメモリが線形に増える（D3 の見積もり）。
- 読み取りタイムアウトを上げると、遅い・無通信のクライアントが接続枠を長く占有する。
- 接続プールを使うクライアントは、アイドル上限を設定値未満にするか、取り出し前の確認を行う。

## 後続への申し送り

- #2454: `README.md` の CLI 一覧と説明、`transaction-limits-cli.md` D9(a) と README の「30 秒」の記述、`auth/argon2id.rs` コメントの「`MAX_CONNECTIONS` = 64」を既定値の表記へ。
- #2454: `extended_query.rs` の Parse は全キー走査で名前付き件数を数えている（O(n)）。上限 1024 でも実害は小さいが、名前付き件数をカウンタで持つ方が望ましい。無名（`""`）を数えない現行契約は維持する。
- #2455: 64 や 30 秒を直書きしているテストの洗い出し（`wire_concurrency_throughput.rs`・`http_limits.rs`・`wire11_parse_describe.rs`・`http4_session_issue.rs`・`http10_tls_surface.rs`・`server.rs` の別名テスト）。

## 対象外

- ハード上限・下限・秒単位の確定（オーナー判断）
- portal 件数（`MAX_PORTALS_PER_SESSION`）・ステートメント名長・prepared SQL／束縛値／保留 portal の合計上限
- `MAX_SESSIONS`・`MAX_REJECT_WORKERS`
- 「読み取りタイムアウト < 持続時間」の起動時警告
- 環境変数・セッション単位・テナント単位の設定
