# SSLRequest への 'S' 応答と TLS 層の wire 接続結線 実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #966・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）の分解 15/20。#952〜#965 で
レコード層・ハンドシェイク codec・`ClientHello` 受理判定・X25519・
HKDF/鍵スケジュール・AES-GCM・レコード保護・Ed25519・PEM/PKCS#8・X.509・
transcript/Finished・サーバー側状態機械（`tls::server_handshake`）が
そろっていたが、実接続にはまだつながっていなかった。本 Issue で
`SSLRequest` へ `'S'` を返しハンドシェイクを実行、以後の pg wire
メッセージを TLS レコード上で送受信するところまでを結線した。

## ストリーム抽象 `WireStream`

認証後の接続ハンドラ（`handshake`・`simple_query`・`extended_query`・
`copy`・`protocol_dispatch`）は従来すべて `&mut TcpStream` を直接受け取る
関数群だった。これを平文・TLS の両方で動かすため、新設
`wire_stream::WireStream`（`Read + Write` + タイムアウト設定・shutdown・
緊急応答用複製・終了処理）を定義し、各関数を `<S: WireStream>` の
ジェネリクスへ一般化した（trait object ではなく静的ディスパッチ。呼び
出し側は常にどちらの型か静的に分かっているため、`dyn` と混在させる必要が
ない）。`TcpStream: WireStream` の実装は既存の OS 呼び出しへ 1:1 で委譲し、
`graceful_close` は no-op（平文接続のビット同一性を維持）。この一般化
自体はロジックを一切変えない（`refactor(wire)` コミットとして分離し、
既存テストが無変更のまま全件 green であることで検証した）。

`try_clone()` による緊急応答（RECOVER-6）用ソケット複製は
`WireStream::emergency_channel()` へ、`shutdown(Shutdown::Write)` は
`WireStream::shutdown_write()` へ置き換えた。

## `TlsStream`（`tls/stream.rs`）

### 読み取り方式（push 型）

`handshake::read_next_frame_header` は明示トランザクションが `Active` の
間、`WouldBlock`/`TimedOut` を受けても同じストリームで読み直す契約
（SQL-31・TASK-221）を持つ。`record::read_record`（`read_exact` を使う
pull 型）でこれを実装すると、レコード途中でタイムアウトした際に既に
読み込んだ部分バイト列が失われ、TLS ストリームの復号状態が壊れる。

そこで `record::RecordBuffer::feed`／`next_record` を使う push 型で実装
した。`inner.read` が返した生バイト列を `RecordBuffer` へ蓄積するだけ
なので、`WouldBlock`/`TimedOut`/`Interrupted` を挟んで呼び出しをまたいで
も取りこぼしがない。この契約は `tls::stream::tests::
read_recovers_after_would_block_mid_record`（極短い読み取りタイムアウト
を注入し、レコードが複数回の `read` に分かれて届く状況を再現）で固定
した。

### 書き込み・fail-closed

1 回の `write` は `TlsSession::seal_application_data` で 1 個以上の
レコードへ分割した後、1 回の `write_all` にまとめて送出する。復号失敗・
alert 解析失敗・書き込み失敗（タイムアウトを含む）はいずれもこの
`TlsStream` を `failed` 状態へ固定し、以後の read/write を即座にエラーに
する（fail-closed）。復号失敗等で `RecordError`/`TlsSessionError::
Protection` の `alert_description()` が `Some` を返す場合は、
`TlsSession::seal_fatal_alert`（本 Issue で追加。RFC 8446 §5.2 の
bad_record_mac 終了に対応）で fatal alert を 1 回だけ best-effort 送出
してから失敗を返す。

### 入力終端（EOF）の契約

`read` が正常終端の `Ok(0)` を返すのは、相手の `close_notify` を受信した
後に限る（RFC 8446 §6.1。PR #1056 レビュー指摘対応）。`close_notify`
なしで生ソケットが EOF になった場合は、受信バッファに部分レコードが
残っていれば `InvalidData`（truncation。`RecordBuffer::finish`）、
レコード境界なら `UnexpectedEof` を返し、いずれも `failed` へ固定する
（切断と正常終了・応答末尾の欠落を取り違えない）。上位の pg wire 層は
メッセージ境界での `UnexpectedEof` を平文 TCP の切断と同じく
`framing::read_typed_frame_header` で接続終了（`Ok(None)`）として静かに
扱い、メッセージ途中なら `FrameError::Truncated`（応答なし）となる。
失敗状態のため、切断後に `close_notify` や ErrorResponse は送らない。

### 緊急応答（RECOVER-6）との関係・既知の制約

`WireStream::emergency_channel` は `None` を返す。
`engine::recovery::panic_hook::EmergencyResponseRegistration` は生の
`TcpStream` へ平文の ErrorResponse を書く契約のため、TLS 接続でこれを
呼ぶと平文バイト列が TLS レコードへ混入してしまう。TLS 接続での
panic 発生時の緊急応答（RECOVER-6・観測性側）は「応答なしで切断」へ
縮退する。安全性側の abort ガード（RECOVER-5）には影響しない。engine 側
API（`TcpStream` 固定）を変更すれば解消できるが、本 Issue のスコープ外
とし、後続 Issue の候補として申し送る（起票はユーザー承認後）。

## `negotiate_startup` の二段化

- **フェーズ 1**（生の `TcpStream` 上）: `tls` 設定が `Some` の場合のみ
  `negotiate_startup_or_upgrade` を使い、`SSLRequest` を受けた時点で
  `'N'` を返さず `PreTlsOutcome::UpgradeTls` を返す。`tls` が `None` の
  場合は既存の `negotiate_startup`（`'N'` 応答）へそのまま委譲し、平文
  接続の挙動をビット単位で変えない（受入基準 2）。GSSENC は本 Issue の
  対象外のまま `'N'`。
- **TLS 昇格**（`handle_tls_upgrade`）: `'S'` を書いてから
  `perform_server_handshake` を実行する。ハンドシェイク driver
  （`perform_server_handshake_with`）は成功時もソケットの読み書き
  タイムアウトを `HANDSHAKE_READ_TIMEOUT` 定数のまま残す設計
  （`docs/design/tls-server-handshake.md` の既存設計を尊重し、driver 自体
  は変更しない）ため、呼び出し側で接続設定値（`server::accept_loop_*` が
  受理直後に設定した値。WIRE-5）を退避し、ハンドシェイク完了後に
  再適用する。これにより TLS 上でも `READ_TIMEOUT`（WIRE-4/5）が同じ値で
  働く（受入基準 4）。
- **フェーズ 2**（`TlsStream` 上）: `negotiate_after_tls` を使い、
  `SSLRequest`／`GSSENCRequest` はいずれも（初回であっても）`Protocol`
  エラーとして拒否する（受入基準 3）。PostgreSQL 本体も TLS 確立後の
  再ネゴシエーション要求を拒否する。

認証成功後の共通シーケンス（`AuthenticationOk` 以降・`post_auth_loop`）は
`run_authenticated_session<S: WireStream>` として切り出し、平文接続・TLS
接続の双方が共有する（ロジックは従来の `handle_connection_inner` から
一切変更しない）。

## pipelining 対策（CVE-2021-23214 型）の確認

`framing::read_startup_frame` は `TcpStream` を直接 `read_exact` で読み、
`BufReader` を使っていない。このため `SSLRequest` の直後に平文で送られた
バイト列は TLS レコードとして解釈され、ハンドシェイクは fail-closed で
失敗する。`tests/wire_tls_connection.rs::
pipelined_plaintext_after_ssl_request_is_not_processed_as_startup` で
固定した。

## 公開 API

- `wire_stream::WireStream`（新設）
- `tls::stream::TlsStream`（新設）
- `tls::server_handshake::TlsSession::seal_fatal_alert`（新設）
- `handshake::handle_connection_with_options`（新設。TLS opt-in を含む
  新しい公開入口。`tls: None` で既存 2 関数とビット同一。Issue #967 以降は
  常に `TlsMode::Allow` で `handle_connection_with_tls_mode` へ委譲する
  後方互換ラッパー）
- `server::accept_loop_with_tls`（新設。`tls: None` で
  `accept_loop_with_engine` とビット同一。Issue #967 以降は常に
  `TlsMode::Allow` で `accept_loop_with_tls_mode` へ委譲する後方互換
  ラッパー）
- 既存の `handle_connection_bounded`・`handle_connection_with_engine`・
  `handle_connection`（deprecated）・`accept_loop_with_limiter`・
  `accept_loop_with_engine` はシグネチャ・挙動とも無変更

## CLI opt-in と平文ポリシー（Issue #967・親 #941・TASK-228）

`--tls-cert`／`--tls-key`／`--tls-mode`（`tls_opt.rs`。新設）が
`TlsServerConfig` を組み立てる唯一の CLI 入口。手順・組合せ検証・平文
ポリシー（`require`／`allow`）・秘密値の非出力方針は README「wire-server
の起動」節を参照（spec 本文の転記を避けるため詳細はそちらに集約する）。

- `handshake::TlsMode`（`tls_opt.rs` から re-export 相当。`#[non_exhaustive]`）・
  `handshake::handle_connection_with_tls_mode`（新設。`handle_connection_with_options`
  が `Allow` で委譲する先）・`negotiate_startup_or_upgrade` への `mode`
  パラメータ追加・`PreTlsOutcome::PlaintextRejected`（`require` 下で平文
  StartupMessage を startup パラメータ非解釈のまま拒否する。D8）。
- `server::accept_loop_with_tls_mode`（新設。`accept_loop_with_tls` が
  `Allow` で委譲する先）。
- `bind_guard::TransportSecurity` へ `TlsRequired`（非ループバック bind を
  許可。WIRE-9）・`TlsOptional`（`Cleartext` と同じくループバック限定。
  D1）を追加。`#[non_exhaustive]` のため下流の exhaustive match は破壊
  しない。
- **D1（意図的な逸脱）**: Issue 本文は「非ループバック × `allow` は警告
  のみ」だが、`allow` は平文接続を受理する以上 WIRE-9（非ループバックでは
  TLS 必須）を満たせないため、警告に留めず起動拒否へ倒した。`bind_guard`
  の既存ガード（`GuardedBindAddrs::resolve`）と同じ場所・同じ判定順序で
  拒否し、`--tls-mode allow` から `require` への切り替えを促す hint 行を
  `main.rs` が追加で出す。
- **D5（Issue #968 で置き換え済み）**: 当初は `--surface nosql` と TLS
  フラグの併用を拒否していた（HTTP リスナーが平文のままだと bind ガード
  だけが `TlsRequired`／`TlsOptional` へ緩み、平文 HTTP が非ループバックへ
  露出しうるため）。#968 で NoSQL 表層も HTTPS として TLS を終端するように
  なり、この拒否は撤去した。詳細は本ファイル「HTTPS 表層（#968）」節参照。
- 依存追加なし。`unsafe` なし。秘密値（鍵の seed・PKCS#8 本文）に依存する
  分岐は作らない。

`crates/wire-server/tests/wire_tls_cli.rs`（新設）: 実バイナリを子プロセスと
して起動し、既定不変（R1）・`require` の完走とログ（R2）・`require` の
平文拒否（R3）・`allow` の TLS/平文双方の受理（R4）・組合せ不正/値欠落/
重複/読み込み失敗（不存在ファイル・不正 PEM・鍵証明書不一致・期限切れ）/
`nosql` 併用の起動拒否（R5）・非ループバック × `allow` の起動拒否と hint
（R6）・秘密値の非出力（R7）を検証する。`tests/common/tls_client.rs` へ
validity 指定可能な葉証明書ビルダー・PEM ラップ（`pem_wrap`）・Ed25519
PKCS#8 DER/PEM 生成（`ed25519_pkcs8_der`／`ed25519_pkcs8_pem`。RFC 8032
§7.1 TEST 1 seed に固定プレフィクス `302e020100300506032b657004220420`
を連結する最小形）を追加した。

## テスト

- `tls::stream::tests`（単体）: 平文往復・空バッファ書き込みの no-op・
  `WouldBlock` を挟んだレコード途中読み取りの復旧・書き込み失敗後の
  fail-closed 固定・部分レコードを残した EOF（truncation）と `close_notify`
  なしの EOF のエラー化・`close_notify` 受信後の正常終端。実ハンドシェイクを経由しない `TlsSession::
  new_for_tests`（`#[cfg(test)]` 限定）で鍵スケジュールから直接組み立てた
  `Sealer`/`Opener` を使う。
- `tests/common/tls_client.rs`（新設）: `wire_server::tls::*` の公開 API
  のみを使う最小 TLS 1.3 クライアント。`tests/tls_server_handshake.rs` の
  `TestClient`／`drive_client_handshake_over_socket` と同じ構成要素を
  独立に持つ意図的な重複（既存ファイルの大規模な相互依存を崩さずに
  新規結合テストを追加するため、本 Issue の範囲では共有モジュールへの
  一本化は行わない）。
- `tests/wire_tls_connection.rs`（新設）: TLS 未設定時の `'N'` 回帰・
  TLS opt-in 時の `'S'` → ハンドシェイク → StartupMessage → cleartext
  認証 → 簡易クエリ往復 → Terminate の完走・TLS 確立後の
  `SSLRequest`/`GSSENCRequest` 拒否・pipelining 耐性・`close_notify` なしの
  クライアント切断（Terminate の有無を問わず）が panic・エラー終了・応答送出
  なしに静かに終わることを固定。

`cargo fmt --all -- --check`・`cargo clippy -p fandhe-vector-db-wire-server
--all-targets -- -D warnings`・`cargo test -p fandhe-vector-db-wire-server`
（lib 1259 件・全結合テストファイル）はすべて green。

## 対象外（後続 sub-issue）

- CLI からの証明書・鍵読み込みと平文ポリシーは Issue #967 として実装済み
  （上記「CLI opt-in と平文ポリシー」節参照）
- HTTPS 表層は Issue #968 として実装済み（下記「HTTPS 表層（#968）」節参照）
- 実クライアント（psql・openssl s_client 等）3 種での接続試験（#969）
- channel binding（#970）
- 緊急応答（RECOVER-6）の TLS 接続対応（engine 側 API の変更が必要。
  上記「既知の制約」参照。NoSQL 表層の TLS 経路でも同じ制約が残る。
  下記「HTTPS 表層（#968）」節参照）

## HTTPS 表層（#968）

NoSQL 表層（`--surface nosql`。HTTP/1.1 最小サブセット）へ SQL 表層と同じ
TLS opt-in（`--tls-cert`／`--tls-key`／`--tls-mode`）を接続した
（`crates/wire-server/src/http/tls_transport.rs`）。

設計判断（H 番号で記録）:

- **H1（TLS 判定の方式）**: HTTP には `SSLRequest` のような明示ネゴシエー
  ションが無いため、接続受理直後の先頭 1 バイトを `peek` して判定する
  （`0x16`＝TLS ハンドシェイクレコードなら TLS、それ以外は平文。HTTP 要求行
  の先頭にこのバイトは現れないため曖昧さは無い）。判定は純関数
  `tls_transport::classify_first_byte` に切り出し単体テストで網羅する。
- **H2（`--tls-mode` の意味）**: `require` は平文と判定した接続へ要求を
  一切解釈せず・応答も書かずに閉じる。`allow` は平文ハンドラへそのまま
  進む。bind ガードは SQL 表層と共有済みのため変更なし。
- **H3（ハンドシェイクの期限）**: SQL 表層と同じ `perform_server_handshake`
  （`HANDSHAKE_READ_TIMEOUT` 固定）を使う。前後で接続の read/write
  タイムアウトを退避・復元する（`crate::handshake::handle_tls_upgrade` と
  同型）。1 接続の占有時間の上限は「ハンドシェイク期限＋要求読み取り期限」
  で有界になる。
- **H4（TLS 下の同時接続超過）**: `--tls-mode` で分岐する（codex-review
  指摘・是正。旧実装は TLS 構成の有無のみで無応答クローズしており、
  `allow` 下の平文クライアントにも 503 が返らなくなる退行があった）。
  `require` は平文・TLS レコードいずれでも要求を解釈せず・応答も書かずに
  閉じる（`require` 下で平文バイト列を送出しないため、かつ TLS
  ハンドシェイクをしていない相手には意味のない応答になるため）。`allow`
  は平文接続を受理するモードのため、拒否ワーカー（`RejectWorkerLimiter`
  で有界化済み）の中で先頭バイトを期限付きで `peek` し、平文なら既存の
  503／`53300` 応答を維持する。TLS レコードと判定した場合も
  （codex-review 再指摘・是正。旧実装はハンドシェイクをせず無応答
  クローズしており、`allow` の下でも HTTPS クライアントだけがこの
  エラー契約から取り残されていた）ハンドシェイクを完了したうえで同じ
  503 応答を TLS 上で返す。`RejectWorkerLimiter` の 1 枠＝1 スレッドで
  既に有界化済みのため、ハンドシェイク自体の絶対期限
  （`HANDSHAKE_READ_TIMEOUT`）がそのままこの拒否ワーカーの専有時間の
  上限になる。拒否応答の書き込み後も H8 と同じ `DeadlineStream` を経由
  させ、トリクル送信によるワーカー専有の無期限化を防ぐ。TLS 未構成時は
  既存の 503／`53300` 経路とバイト単位で同一のまま。
- **H5（`--tls-scram-channel-binding enable` × nosql）**: 起動を拒否せず
  no-op として受理する（`--auth-method cleartext` と組み合わせたときと
  同じ扱い。NoSQL 表層は SASL 往復を持たないため実際には提示されない）。
- **H6（EOF の扱いの差）**: `close_notify` を経ない TLS 下層 EOF は
  `TlsStream` が `UnexpectedEof`／`InvalidData` を返すため、平文なら
  「宣言長より短い本文」として `08P01` 応答になる経路が、TLS では失敗した
  ストリームへ応答を書けないため無応答クローズになる（許容する既知の差）。
  `close_notify` による正常な half-close は平文と同じ応答になる。
- **H7（終端処理）**: 応答経路（`conn::respond_and_close`）は
  `drain_and_close` 内の `shutdown_write` が TLS 上では `close_notify` の
  送出を兼ねる。無応答クローズ経路（`Outcome::CloseSilently`）は
  `graceful_close`（TLS では `close_notify`。平文では no-op）してから
  `shutdown_both` する。
- **H8（TLS 上のトリクル送信に対する絶対期限）**: `tls::stream::TlsStream::
  read` は 1 レコード分がそろうまで内部で `inner.read` を複数回ループする
  ため、`http::conn` が `read` 呼び出し前に一度だけ設定するソケット
  タイムアウトは内部ループの各反復で使い回され、相手が 1 レコードの中身を
  期限ぎりぎりの間隔で 1 バイトずつ送り続けると 1 回の `read` 呼び出しが
  「レコード長 × タイムアウト値」まで際限なく延びる（Slowloris の変種。
  平文経路には無い問題）。`http::deadline_stream::DeadlineStream` で生
  ソケットを包んでから `TlsStream::new` へ渡すことで是正した。
  `set_read_timeout` を絶対時刻へ変換して保持し、内部ループから複数回
  呼ばれる `Read::read` の直前に毎回「残り時間」を下位ソケットへ
  再設定する。平文経路（`DeadlineStream` を経由しない）は無変更。

既知の制約（対象外として持ち越し）:

- curl 実クライアントとの接続試験（`tests/http10_curl_interop.rs`。
  `#[ignore]` の手動 gate。CI 常時実行化は対象外）
- TLS 接続での緊急応答（RECOVER-6）対応（engine 側 API の変更が必要）

追記（codex-review・Cursor Bugbot 指摘の是正。H4 更新）: `conn::
reject_too_many_connections` は `write_all` 直後の `shutdown(Both)` から、
`respond_and_close` と同じ「書き込み → 有界 lingering close
（`drain_and_close`） → drop」の形へ変更した。呼び出し元（`tls_transport::
serve_connection`／`reject_or_close_over_limit`）は判定のため先頭 1 バイトを
`peek` 済みで要求本体が未読のまま残るため、旧実装では未読データありの
クローズにより TCP RST が発生し、送出済みの 503 応答をクライアントが
読めなくなりうる回帰があった。

`crates/wire-server/tests/http10_tls_surface.rs`（新設・層 A・CI 常時実行）:
TLS 完走（`/v1/session` → `/v1/query` → `/v1/session/close`）・`require` 下
での平文拒否・`allow` 下での平文/TLS 双方の受理・不正な ClientHello が
panic せず後続接続に波及しないことを検証する。`tests/wire_tls_cli.rs::
tls_with_nosql_surface_serves_https`（旧
`tls_with_nosql_surface_is_rejected` を置き換え）は CLI 結線の外形
（起動ログ・TLS 完走・平文拒否）のみを確認する。
