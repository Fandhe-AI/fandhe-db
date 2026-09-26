//! NoSQL 表層（`--surface nosql`）が唯一の転送路として使う、自作 HTTP/1.1
//! 最小サブセットのモジュール群。
//!
//! 本モジュールは `AGENTS.md` P1 の「Web API はスコープ外」が指す汎用 Web API
//! フレームワークとは別物である。ここで実装するのは spec で規範化された内部転送
//! プロトコル（HTTP-1〜13）であり、pg wire v3 互換実装（[`crate::framing`] 等）と
//! 並ぶ、もう一方の表層の接続処理層に位置づく。
//!
//! エラー契約は SQL 表層（`wire_code` ＝ [`engine::error_format::ErrorClass`]）と
//! 完全共有し、新規 `wire_code` は追加しない。本モジュール配下は `ErrorClass` を
//! HTTP 上の表現（ステータス・応答本文等）へ写像する各要素、要求側のパース処理、
//! `POST /v1/query` の JSON クエリオブジェクト検証、およびセッション認証の
//! 構成要素を集約する。
//!
//! モジュール構成（現時点）:
//! - [`request`]: 要求行（メソッド・ターゲット・バージョン）の解析（Issue #740・
//!   TASK-173・HTTP-2, HTTP-11）
//! - [`headers`]: ヘッダ部（`name: value` 行群と終端空行）の解析。合計 8 KiB・
//!   32 個の固定上限、`Content-Length` 必須・一意・digits-only、
//!   `Transfer-Encoding` 拒否（Issue #741・TASK-173・HTTP-2, HTTP-11）
//! - [`body`]: 本文を読み取る前の `Content-Type`（`application/json`・
//!   `charset=utf-8` のみ）検証と本文長 1 MiB 上限判定（`08P01`／`54000`）、
//!   および読み取り後の本文バイト列 → UTF-8 文字列昇格（`42601`。Issue #742・
//!   TASK-173・HTTP-3, HTTP-11）
//! - [`status`]: `ErrorClass` → HTTP ステータスの決定的射影（Issue #744・ERR-4）
//! - [`error_body`]: `ErrorClass` → JSON エラー本文（Issue #745・ERR-4・ERR-5）
//! - [`conn`][]: 接続 1 本ぶんの受理後処理。要求行→ヘッダ→本文の読み取りと
//!   パース → `RequestHandler` へのルーティング（production は [`router::
//!   Router`]。Issue #752 より前は全パス `08P01` の `conn::PlaceholderRouter`
//!   固定だった）を [`conn::handle_connection_with`] として実装し、不正
//!   フレーム時も応答を書いてから有界に読み捨ててクローズする（PoC-15）・
//!   panic は `catch_unwind` で多層防御する（Issue #747・TASK-173・
//!   HTTP-12）。同時接続数上限超過時に 503／`53300` を返す
//!   [`conn::reject_too_many_connections`]（Issue #743・TASK-69・WIRE-5,
//!   WIRE-6）も担う
//! - [`listener`]: `--surface nosql` の accept ループ本体
//!   （[`listener::accept_loop_with_router`]。読み取り 30 秒タイムアウト・
//!   同時接続数 64 の共有リミッターを SQL wire と同一契約で適用する。
//!   `accept_loop_with_limiter`〔`conn::PlaceholderRouter` 固定〕は既存
//!   呼び出し元向けの後方互換 API として残置。Issue #735・#743・#752・
//!   TASK-171／HTTP-1・HTTP-9）。`main.rs::run_server` が SQL wire の
//!   [`crate::server::accept_loop_with_engine`] と排他選択で呼ぶ唯一の
//!   呼び出し元
//! - [`query`]: `POST /v1/query` の op 別写像の親モジュール（TASK-175。
//!   `query::schema` が JSON クエリオブジェクトの意味的検証（必須キー欠落・
//!   未知キー・型不一致 → `42601`）を担う（Issue #760）。`query::filter` は
//!   `filter` 配列の `op` 語彙（`eq`／`prefix`）を
//!   `engine::declarative_filter::DeclarativeFilter` へ写像し `bind_all` へ
//!   委譲する（Issue #761・NOSQL-7）。`query::response` は `search`／`scan`／
//!   `aggregate` 成功時の `QueryResult` → JSON 応答本文（`columns`／`rows`／
//!   `row_count`、`crate::result_encoder` と同じ型写像。Issue #762・
//!   NOSQL-11）を担う。`query::op` は `op` 名を閉じた語彙 6 値（`search`／
//!   `scan`／`aggregate`／`insert`／`update`／`delete`）へ分類する許可リストで、
//!   語彙外（DDL・UDF 呼び出し・トランザクション制御を含む）を `0A000` へ
//!   写像する（Issue #759・TASK-179・NOSQL-1・NOSQL-9。`update`／`delete`
//!   の追加は Issue #875・NOSQL-12。束縛・実行結線は Issue #876 の担当）。
//!   `query::insert`
//!   は `insert` op を `engine::sql::exec::execute_insert_batch`／
//!   `EngineCore::execute_bound_insert_in_session` へ写像し、`operation_id` 必須化
//!   （`23502`）・台帳照合による再送判定（`23505`／`22023`）・INDEX-4 上限
//!   （`54000`）を適用する（Issue #771・TASK-178・NOSQL-6。`gate.rs` への結線は
//!   対象外）
//! - [`session`]: HTTP セッション認証の構成要素（トークン生成・エンコード
//!   〔Issue #750・TASK-174・HTTP-4〕、TTL 固定・同時有効数上限付きの
//!   メモリ内セッションストア〔`session::store::SessionStore`。Issue #751・
//!   TASK-174・HTTP-4・HTTP-5〕、`POST /v1/session` の発行パイプライン本体
//!   〔`session::issue::handle`。Issue #752・HTTP-6〕、`Authorization: Bearer`
//!   ヘッダの受信データ経路〔`session::bearer::extract_bearer_token`〕、
//!   `POST /v1/session/close` のワンタイム失効パイプライン本体
//!   〔`session::close::handle`。いずれも Issue #753・HTTP-8〕、`/v1/query`
//!   前段の Bearer ミドルウェア〔`session::middleware::authenticate`。
//!   `SessionPrincipal` の導出・`tenant_id` 相当ヘッダの拒否。Issue #754・
//!   HTTP-5・HTTP-6・HTTP-7〕）。
//! - [`router`]: production 入口のルータ（[`router::Router`]）。`target ==
//!   "/v1/session"` を [`session::issue::handle`] へ、`"/v1/session/close"`
//!   を [`session::close::handle`] へ、`"/v1/query"` を
//!   [`session::middleware::authenticate`] 経由で [`query::gate::handle`] へ
//!   ディスパッチし、それ以外（クエリ文字列付き・末尾スラッシュ違い等）は
//!   `conn::PlaceholderRouter` と同一のバイト列（`08P01`）で拒否する。
//!   `main.rs::run_server` が nosql 選択時に構築する唯一の呼び出し元
//!   （Issue #752・#753・#754・TASK-171／HTTP-1・HTTP-6・HTTP-7・HTTP-8）
//! - [`response`]: ステータスコード＋JSON 本文 → HTTP/1.1 応答バイト列
//!   （ステータス行・`Connection: close`・`Content-Type`／`Content-Length`・
//!   CRLF の組み立て。Issue #746・HTTP-2・HTTP-3・ERR-4・ERR-5）
//! - [`tls_transport`]: `--surface nosql` を TLS opt-in（`--tls-cert`／
//!   `--tls-key`／`--tls-mode`）と組み合わせたときの接続 1 本の TLS 終端
//!   （先頭バイトで TLS／平文を判定 → ハンドシェイク → `conn::
//!   handle_connection_with` への委譲。Issue #968・親 #941・TASK-228。
//!   対象ビヘイビア WIRE-9・HTTP-9・HTTP-10）
//!
//! 後続 Issue で追加予定（本モジュールでは未実装）:
//! - `explain: true` 時の `{"explain":[...]}` 応答（#765）
//! - [`query::gate`] の暫定 `0A000`／501 応答を op ごとの束縛・実行計画への
//!   写像で置き換える（op 許可リスト自体は [`query::op`] として実装済み
//!   〔Issue #759・TASK-179・NOSQL-1・NOSQL-9〕。#763・#766・#768 以降が
//!   本 seam を置き換える）
//!
//! 対応: TASK-173〜TASK-175（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア
//! HTTP-1〜13・NOSQL-1〜NOSQL-10。PoC-15/TASK-182 は private 資産のため
//! ポインタ参照のみで、コード・所見は転記しない）。

pub mod body;
pub(crate) mod conn;
pub mod date;
pub(crate) mod deadline_stream;
pub mod error_body;
pub mod headers;
pub mod listener;
pub mod query;
pub mod request;
pub mod response;
pub mod router;
pub mod session;
pub mod status;
pub(crate) mod tls_transport;
