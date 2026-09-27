//! 認証後の pg wire 接続ハンドラ（`handshake`・`simple_query`・
//! `extended_query`・`copy`・`protocol_dispatch`）が読み書きするストリームの
//! 抽象。平文 `TcpStream` と TLS 上のストリーム（[`crate::tls::stream::
//! TlsStream`]。Issue #966）のどちらでも同じハンドラロジックを走らせるために
//! 導入する（Issue #966）。
//!
//! ハンドラ側の関数は本 trait のオブジェクト（`&mut dyn WireStream`）を
//! 受け取る形へ一般化し、分岐やメッセージ組み立てのロジック自体は変更しない
//! （型を広げるだけ）。`Read + Write` はオブジェクトセーフなので、個々の
//! 関数を generic 化せずに単一の trait object で受け渡しできる。

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 認証後接続ハンドラが要求するストリーム操作の最小集合。
///
/// `TcpStream`（平文）と `TlsStream`（TLS。Issue #966）の双方に実装する。
/// ハンドラ側のロジック（分岐・応答内容・順序）はこの trait を介しても
/// 一切変えない契約とする。
pub trait WireStream: Read + Write {
    /// 現在設定されている読み取りタイムアウトを取得する
    /// （`post_auth_loop` が接続全体の基準値として最初に読む）。
    fn read_timeout(&self) -> io::Result<Option<Duration>>;

    /// 読み取りタイムアウトを設定する。TLS 上でも同じ OS ソケットの
    /// タイムアウトがそのまま効く（`TlsStream` は内部の `TcpStream` へ委譲）。
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;

    /// 書き込みタイムアウトを設定する。[`Self::set_read_timeout`] と対。
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;

    /// 読み書き両方向を閉じる（`protocol_dispatch::drain_and_close` 等の
    /// fail-closed な即時切断が使う）。
    fn shutdown_both(&mut self) -> io::Result<()>;

    /// 書き込み方向だけを閉じる（`protocol_dispatch::drain_and_close` の
    /// lingering close が FIN を送ってから読み捨てるために使う）。
    fn shutdown_write(&mut self) -> io::Result<()>;

    /// 緊急応答（RECOVER-6・panic フック経由の応答送出）用の生ソケット複製。
    /// `engine::recovery::panic_hook::EmergencyResponseRegistration` が
    /// `TcpStream` 固定の API のため、平文接続でのみ `Some` を返す。TLS
    /// 接続はこの生複製へ平文を直接書けない（TLS レコードへ混入する）ため
    /// 常に `None`（[`Self::emergency_response_channel`] の既定実装が使う
    /// 経路とは別に、旧経路として直接呼ばれても平文混入させないための
    /// fail-closed）。
    fn emergency_channel(&self) -> Option<TcpStream>;

    /// 緊急応答（RECOVER-6・Issue #966／#1080）の登録に使う
    /// 「送出済み形式のバイト列」と「書き込み先の生ソケット複製」を返す。
    /// `engine::recovery::panic_hook::EmergencyResponseRegistration::
    /// register` はこの組を受け取り、panic フック内で `response_bytes` を
    /// 生ソケットへそのまま書く契約（[`Self::emergency_channel`] と同じ
    /// クローン）。`response` は wire プロトコルの平文 ErrorResponse
    /// バイト列（呼び出し元は `simple_query`／`copy` の登録ヘルパー）。
    /// `None` は登録しない（fail-closed。接続断のみへ縮退）。
    ///
    /// 既定実装は [`Self::emergency_channel`] をそのまま使い、`response`
    /// を素通しする（平文 `TcpStream` はこれで従来と**ビット同一**）。
    /// TLS 接続（[`crate::tls::stream::TlsStream`]）はこれを上書きし、
    /// `response` を TLS レコードとして暗号化してから返す（詳細は
    /// `docs/design/tls-wire-connection.md`「緊急応答（RECOVER-6）との
    /// 関係」節）。
    fn emergency_response_channel(&self, response: &[u8]) -> Option<(Vec<u8>, TcpStream)> {
        let clone = self.emergency_channel()?;
        Some((response.to_vec(), clone))
    }

    /// 接続終了時の best-effort な後始末（TLS では `close_notify` の送出）。
    /// 平文では no-op（既存挙動とビット同一）。
    fn graceful_close(&mut self);

    /// SCRAM-SHA-256-PLUS（[`crate::auth::scram`]・WIRE-18）がチャネル
    /// バインディングとして使う `tls-server-end-point`（RFC 5929 §4。
    /// Issue #970）。平文接続では構造的に `None`（`TcpStream` 実装は常に
    /// `None` を返す）。TLS 接続では [`crate::tls::server_handshake::
    /// TlsSession::tls_server_end_point`] へ委譲し、非対応の署名
    /// アルゴリズム・提示無効化設定の場合も `None` になる。
    ///
    /// 既定実装は `None`（Issue #970 追加時点で下流の既存 `WireStream`
    /// 実装を破壊しないための互換維持。codex-review PR #1087 P1 是正）。
    /// `None` を返す実装は `auth::scram::authenticate_scram` の呼び出し元
    /// （`handshake.rs`）が SCRAM-SHA-256-PLUS を機構リストへ提示せず、
    /// クライアントが `gs2-cbind-flag` に `p=<cb-name>` を送っても
    /// `negotiate_channel_binding` が拒否する（fail-closed。`tests/
    /// wire_scram_plus_tls.rs`・`tests/wire_scram_auth.rs` で固定）。
    fn tls_server_end_point(&self) -> Option<&[u8]> {
        None
    }
}

impl WireStream for TcpStream {
    fn read_timeout(&self) -> io::Result<Option<Duration>> {
        TcpStream::read_timeout(self)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }

    fn shutdown_both(&mut self) -> io::Result<()> {
        TcpStream::shutdown(self, std::net::Shutdown::Both)
    }

    fn shutdown_write(&mut self) -> io::Result<()> {
        TcpStream::shutdown(self, std::net::Shutdown::Write)
    }

    fn emergency_channel(&self) -> Option<TcpStream> {
        self.try_clone().ok()
    }

    fn graceful_close(&mut self) {
        // 平文接続には送るべき終端メッセージが無い。既存挙動（drop のみ）と
        // ビット同一に保つため何もしない。
    }

    fn tls_server_end_point(&self) -> Option<&[u8]> {
        // 平文接続は TLS を経由しないため、構造的にチャネルバインディング
        // 値を持たない。
        None
    }
}
