//! wire-server: PostgreSQL wire プロトコル v3 互換の自作実装ライブラリ層。
//!
//! `src/main.rs`（バイナリ）と `tests/`（結合テスト）の双方から利用できるよう、
//! TASK-66 の stub から lib+bin 構成へ再編した（バイナリのみでは結合テストが
//! 内部モジュールへアクセスできないため）。責務境界はクライアント接続の受け付け・
//! wire プロトコルのパース/応答整形であり、クエリの実処理は `engine` クレート
//! （コアロジック層）へ委譲する方針を維持する。
//!
//! モジュール構成:
//! - [`auth`]: ユーザーストア・Argon2id 照合・`PolicyContext` へのテナント導出（WIRE-2, WIRE-3）
//! - [`framing`]: メッセージフレーミングの長さ検証・fail-closed エラー分類（WIRE-4, WIRE-10）
//! - [`handshake`]: TCP 接続ごとのメッセージ読み書き・StartupMessage・認証フロー（WIRE-1）
//! - [`http`]: NoSQL 表層（HTTP/1.1 最小サブセットの転送路。TASK-173〜175。
//!   wire プロトコルとは独立した経路）の入口。`http::status` は `ErrorClass` →
//!   HTTP ステータスの決定的射影（Issue #744・ERR-4）。`http::error_body` は
//!   `ErrorClass` → JSON エラー本文（Issue #745・ERR-4・ERR-5）。`http::response`
//!   はステータスコード＋JSON 本文 → HTTP/1.1 応答バイト列（ステータス行・
//!   `Connection: close`・`Content-Type`／`Content-Length`・CRLF の組み立て。
//!   Issue #746・HTTP-2・HTTP-3・ERR-4・ERR-5）。`http::listener`
//!   は `--surface nosql` の accept ループ本体（Issue #735・#743・HTTP-9。
//!   読み取り 30 秒タイムアウト・同時接続数 64（超過は 503／`53300`）を
//!   SQL wire と同一契約で適用する。要求の解釈・応答生成は `http::conn` の
//!   暫定ハンドラにとどまり、本体は #747）。`http::body` は本文を読み取る前の
//!   `Content-Type`（`application/json`・`charset=utf-8` のみ）検証と本文長
//!   1 MiB 上限判定（`08P01`／`54000`）、読み取り後の UTF-8 昇格
//!   （`42601`。Issue #742）を担う。
//!   `http::session` はセッション認証のトークン生成・base64url 表現
//!   （TASK-174・HTTP-4・Issue #750）と、TTL 固定〔[`limits::SESSION_TTL`]〕・
//!   同時有効数上限〔[`limits::SessionLimiter`]／[`limits::MAX_SESSIONS`]〕
//!   付きのメモリ内セッションストア `http::session::store::SessionStore`
//!   （TASK-174・HTTP-4・HTTP-5・Issue #751）、`Authorization: Bearer` の
//!   受信データ経路 `http::session::bearer::extract_bearer_token`・
//!   `POST /v1/session/close` のワンタイム失効パイプライン
//!   `http::session::close::handle`（TASK-174・HTTP-8・Issue #753）、
//!   `/v1/query` 前段の Bearer ミドルウェア `http::session::middleware::
//!   authenticate`（`SessionPrincipal` 導出・`tenant_id` 相当ヘッダ拒否。
//!   TASK-174・HTTP-5・HTTP-6・HTTP-7・Issue #754）を担う。
//!   `http::query::schema` は `POST /v1/query` の JSON クエリオブジェクトの
//!   意味的検証ヘルパー（必須欠落・未知キー・型不一致 → `42601`。Issue #760）。
//!   `http::query::filter` は `filter` 配列の `op` 語彙（`eq`／`prefix`）を
//!   `engine::declarative_filter::DeclarativeFilter` へ写像し `bind_all` で
//!   `TableSchema` へ束縛する（Issue #761・NOSQL-7）。`http::query::response`
//!   は `search`／`scan`／`aggregate` 成功時の `engine::sql::exec::QueryResult`
//!   → JSON 応答本文（`columns`／`rows`／`row_count`、`result_encoder` と
//!   同じ型写像。Issue #762・NOSQL-11）。接続ハンドラ本体（要求パース・
//!   ルーティング）は #747 が `http` 配下へ追加していく
//! - [`bind_guard`]: bind アドレスの通信路保護要件検証（TLS 未構成時は loopback 限定。
//!   TASK-70・WIRE-7）。`main.rs::run_server` の唯一の bind 経路
//! - [`durability_opt`]: `--durability` opt-in CLI 引数の閉じた語彙パーサ
//!   （Issue #850。`engine::storage::WriteDurability` へ untrusted な CLI
//!   文字列から到達する唯一の入口。非既定値選択時の起動ログ警告は
//!   `main.rs::run_server` の責務）
//! - [`server`]: 接続受け付けループ・同時接続数の有界化・I/O タイムアウト適用
//!   （契約値・実装は [`limits`] に委譲）
//! - [`limits`]: 読み取りタイムアウト・共有接続数リミッター（TASK-69・WIRE-5, WIRE-6）
//! - [`protocol_dispatch`][]: 認証後メッセージの型バイト分類と、拡張クエリ
//!   プロトコル等の未対応メッセージへの fail-closed 拒否応答＋切断（TASK-71・WIRE-8）
//! - [`simple_query`][]: 簡易クエリ（'Q'）1 文の `engine::core::EngineCore` への
//!   委譲・成功/失敗応答の組み立て（TASK-73・WIRE-1）
//! - [`result_encoder`][]: `RowDescription`/`DataRow`/`CommandComplete`/
//!   `EmptyQueryResponse` のバイト列生成（純関数。TASK-73・WIRE-1）。
//!   バイナリ形式（format code 1）の結果エンコーディング（WIRE-14・
//!   TASK-218・Issue #936。`ResultFormats::resolve`・
//!   `validate_binary_formats`・`encode_row_description_with_formats`・
//!   `encode_data_row_into_with_formats`）も本モジュールが提供するが、
//!   拡張クエリプロトコルの Bind／Describe（#933・#934）が未実装のため
//!   wire 経由でバイナリ形式を要求する経路は本 crate にまだ無い（Phase A。
//!   結線は #934 の担当）
//! - [`error_response`][]: `engine::error_format::ErrorClass` → `ErrorResponse`
//!   （'E'）バイト列への横断写像（TASK-153・ERR-1・`RECOVER-5` (3) ポインタ）
//! - `response_buffer`（crate 内限定）: 簡易クエリ応答の `DataRow` 群を上限付き
//!   バッファへ組み立て、1 回の `write_all` で送出するための組み立て器
//!   （Issue #481）
//! - [`search_engine_opt`]: `--search-engine` opt-in CLI 引数の閉じた語彙
//!   パーサ（Issue #656。`engine::search_engine::SearchEngineKind` へ
//!   untrusted な CLI 文字列から到達する唯一の入口）
//! - [`surface`]: `--surface` opt-in CLI 引数の閉じた語彙パーサ（Issue #734・
//!   TASK-171／HTTP-1。SQL／NoSQL の 2 表層排他選択へ untrusted な CLI
//!   文字列から到達する唯一の入口。`main.rs::run_server` がこの選択に応じて
//!   `server::accept_loop_with_engine`／`http::listener::accept_loop_with_limiter`
//!   のいずれか 1 本だけを呼ぶ（Issue #735・#743）
//! - [`tls`]: 自作 TLS 1.3 サーバー側実装の入口（TASK-228・WIRE-9・HTTP-10。
//!   親 Issue #941 の 2 時間単位 sub-issue 群の 1 つ）。X25519 鍵交換
//!   （RFC 7748・定数時間。Issue #955）・`tls::record`（レコード層。
//!   RFC 8446 §5.1。Issue #952）・`tls::record_protection`（レコード保護層。
//!   RFC 8446 §5.2〜§5.5。Issue #959）・`tls::ed25519`（Ed25519 署名生成・
//!   検証。RFC 8032 §5.1。Issue #961）・`tls::certificate_verify`
//!   （`CertificateVerify` の署名対象構成・生成・検証。RFC 8446 §4.4.3。
//!   Issue #961）・`tls::server_handshake`（サーバー側ハンドシェイク状態機械。
//!   Issue #965）まで実装済み。`SSLRequest` への `'S'` 応答・接続への実結線
//!   （[`tls::stream::TlsStream`]・`handshake.rs` の二段ネゴシエーション）は
//!   Issue #966（詳細は `docs/design/tls-wire-connection.md`）。CLI からの
//!   証明書・鍵読み込み（#967）・HTTPS 表層（#968）・channel binding
//!   （#970）は実装済み。実クライアント接続試験（#969）は後続 sub-issue の
//!   担当
//! - [`wire_stream`][]: 認証後接続ハンドラが読み書きするストリームの抽象
//!   [`wire_stream::WireStream`]（`Read + Write` + タイムアウト設定・
//!   shutdown・緊急応答用複製・終了処理）。`TcpStream`（平文）・
//!   [`tls::stream::TlsStream`]（TLS。Issue #966）の双方に実装し、
//!   `handshake`・`simple_query`・`extended_query`・`copy`・
//!   `protocol_dispatch` のハンドラ本体は本 trait のオブジェクトを介して
//!   同一ロジックを共有する
//! - `fault_injection`（feature `fault-injection` 限定・テスト専用。
//!   Issue #705）: `--fault-inject post-commit-panic` opt-in CLI 引数の閉じた
//!   語彙パーサと、`simple_query::execute_and_respond` の登録ブロック内から
//!   呼ばれる commit 後 panic 注入。default features には含まれない
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）、
//! TASK-68（対象ビヘイビア WIRE-4, WIRE-10）、TASK-69（対象ビヘイビア WIRE-5, WIRE-6）、
//! TASK-70（対象ビヘイビア WIRE-7）、TASK-71（対象ビヘイビア WIRE-8）、
//! TASK-73（対象ビヘイビア WIRE-1: 簡易クエリを engine SQL 表層へ接続）、
//! TASK-153（対象ビヘイビア ERR-1: ErrorResponse 正式写像）。

pub mod auth;
pub mod auth_method_opt;
pub mod bind_guard;
pub(crate) mod copy;
pub mod ddl_permission_opt;
pub mod durability_opt;
pub mod error_response;
pub mod extended_query;
#[cfg(feature = "fault-injection")]
pub mod fault_injection;
pub mod framing;
pub mod handshake;
pub mod http;
pub mod limits;
pub mod protocol_dispatch;
pub(crate) mod response_buffer;
pub mod result_encoder;
pub mod search_engine_opt;
pub mod server;
pub mod simple_query;
pub mod surface;
pub mod tls;
pub mod tls_opt;
pub mod wire_stream;
