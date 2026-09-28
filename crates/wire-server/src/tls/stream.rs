//! TLS 1.3 ハンドシェイク完了後の pg wire バイトストリーム（Issue #966）。
//!
//! `handshake::handle_connection_inner` が `SSLRequest` への `'S'` 応答・
//! [`super::server_handshake::perform_server_handshake`] 完了後に、生の
//! `TcpStream` を [`TlsStream::new`] でラップして返す。以後の
//! `negotiate_startup`・認証・簡易/拡張クエリ・COPY・拒否応答は、
//! 平文接続と同じロジックのまま `crate::wire_stream::WireStream` の
//! オブジェクトとしてこのストリームを扱う（型を広げるだけで分岐・応答内容は
//! 一切変えない。`handshake` モジュールドキュメント参照）。
//!
//! # 読み取り方式（push 型。受入基準 4 の要）
//!
//! [`super::record::read_record`]（`read_exact` を使う pull 型）ではなく、
//! [`super::record::RecordBuffer::feed`]／[`next_record`] を使う push 型で
//! 実装する。`handshake::read_next_frame_header` は明示トランザクションが
//! `Active` の間、`WouldBlock`/`TimedOut` を受けても同じストリームで読み
//! 直す契約（SQL-31・TASK-221）を持つため、pull 型でレコード途中の
//! タイムアウトに遭遇すると既に読み込んだ部分バイト列が失われ TLS
//! ストリームの復号状態が壊れる。push 型なら `inner.read` が返した生
//! バイト列を [`RecordBuffer`] へ蓄積するだけなので、途中のタイムアウトで
//! 呼び出しをまたいでも取りこぼしがない。
//!
//! # 入力終端（EOF）の契約
//!
//! `read` が `Ok(0)` を返すのは相手の `close_notify` を受信した後に限る。
//! `close_notify` なしで生ソケットが EOF になった場合は、部分レコードが
//! 残っていれば `InvalidData`（truncation）、レコード境界なら
//! `UnexpectedEof` を返し、いずれもストリームを失敗状態に固定する
//! （以後の `read`／`write` は常にエラー。fail-closed）。
//!
//! # 緊急応答（RECOVER-6）との関係（Issue #1080）
//!
//! `WireStream::emergency_channel` は `None` を返す（平文の緊急応答
//! バイト列が生ソケットへ直接書かれて TLS レコードへ混入するのを防ぐ
//! ため）。緊急応答の登録は [`WireStream::emergency_response_channel`]
//! の上書き実装（[`TlsStream`] の [`WireStream`] 実装）が担う: 呼び出し
//! 時点の [`TlsSession::seal_application_data_detached`]（[`super::
//! record_protection::Sealer::seal_detached`] へ委譲。シーケンス番号を
//! 消費しない「detached seal」）で緊急応答を 1 個の TLS レコードへ
//! 暗号化し、直列化した暗号文を平文の代わりに [`WireStream::
//! emergency_channel`]（`self.inner`。生 `TcpStream`）へ渡す。
//!
//! ## nonce 再利用の防止（不変条件）
//!
//! detached seal した緊急応答レコードと、panic が起きなかった場合の
//! 次の通常応答レコードは同じ nonce（同じ送信 seq）を使う。安全なのは
//! 「両者のうち高々一方しか実際には送出されない」ことが保証されるため:
//! 登録ブロックの間 `stream` は `&mut` で排他借用されており通常送信は
//! 起きない。緊急応答を書く条件を満たした場合（commit-pending 世代が、
//! スタック上の armed な `ResponseBoundaryGuard` の世代と一致するとき）、
//! その guard の `Drop` は `should_abort` により必ず `std::process::
//! abort()` する（`engine::recovery::commit_boundary`・RECOVER-5。panic
//! フックの外側で常に効く安全弁で、フックの登録有無に依存しない）ため、
//! 以後の通常送信は起きない。`wire-server` バイナリではこれに加えて
//! `engine::recovery::fail_fast`（RECOVER-8）がプロセス全体の panic を
//! 網羅的に fail-fast させる。**将来この区間の panic を abort せず捕捉
//! して処理を継続する変更を入れると、この不変条件が壊れて nonce 再利用
//! になる**（[`super::record_protection::Sealer::seal_detached`] の
//! doc も参照）。
//!
//! seal・直列化・`try_clone` のいずれかが失敗した場合、または `failed`
//! （既にこの接続が破損済み）の場合は `None`（登録しない＝接続断のみへ
//! 縮退。fail-closed）。詳細は `docs/design/tls-wire-connection.md`
//! 「緊急応答（RECOVER-6）との関係」節。

use std::io::{self, Read, Write};

use super::record::{self, Record, RecordBuffer, RecordKind, MAX_PLAINTEXT_LEN};
use super::server_handshake::{AppEvent, TlsSession, TlsSessionError};
use crate::wire_stream::WireStream;

/// 復号済みアプリケーションデータの read-ahead 上限（1 レコード分。
/// [`MAX_PLAINTEXT_LEN`] を超えて確保しない）。
const READ_AHEAD_CAP: usize = MAX_PLAINTEXT_LEN;

/// ハンドシェイク完了後の TLS 1.3 接続 1 本を表す。`S`（実運用では
/// `std::net::TcpStream`）の上に [`TlsSession`] のレコード保護を重ねる。
pub struct TlsStream<S> {
    inner: S,
    session: Box<TlsSession>,
    /// 受信生バイト列の未消費分（`RecordBuffer::feed` が容量超過分を
    /// 取り込めなかった場合の呼び出し元側の保持先。[`super::record::
    /// MAX_RECORD_WIRE_LEN`] で有界）。
    rx: RecordBuffer,
    /// 復号済み平文の read-ahead（`AppEvent::ApplicationData` を
    /// `Read::read` の呼び出し粒度に合わせて切り出すためのバッファ）。
    plaintext: Vec<u8>,
    plaintext_pos: usize,
    /// 相手の `close_notify` を受信済み（以後 `read` は `Ok(0)` を返す）。
    eof: bool,
    /// この接続が致命的に失敗した（以後 read/write を即座にエラーにする。
    /// fail-closed。書き込み失敗・復号失敗等で真になる）。
    failed: bool,
}

impl<S: WireStream> TlsStream<S> {
    /// ハンドシェイク完了直後の生ストリームとセッションからラップする。
    pub fn new(inner: S, session: Box<TlsSession>) -> Self {
        Self {
            inner,
            session,
            rx: RecordBuffer::new(),
            plaintext: Vec::new(),
            plaintext_pos: 0,
            eof: false,
            failed: false,
        }
    }

    /// read-ahead に残っている平文があれば `buf` へコピーして返す。
    fn drain_plaintext(&mut self, buf: &mut [u8]) -> usize {
        let remaining = &self.plaintext[self.plaintext_pos..];
        let n = remaining.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), remaining.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.plaintext_pos += n;
        if self.plaintext_pos >= self.plaintext.len() {
            self.plaintext.clear();
            self.plaintext_pos = 0;
        }
        n
    }

    /// 復号失敗・alert 解析失敗等を検出した際、可能なら fatal alert を
    /// 1 回だけ送出してから接続を `failed` にする（best-effort。送出自体の
    /// 失敗は無視する。RFC 8446 §5.2 の bad_record_mac 終了に対応する）。
    fn fail_with_alert(&mut self, desc: Option<record::AlertDescription>) {
        self.failed = true;
        let Some(desc) = desc else {
            return;
        };
        if let Ok(records) = self.session.seal_fatal_alert(desc) {
            let _ = write_records(&mut self.inner, &records);
        }
    }

    /// 内部の生ストリームから 1 チャンク読み、[`RecordBuffer`] へ取り込む。
    /// `inner.read` の `WouldBlock`/`TimedOut`/`Interrupted` は種別を変えず
    /// そのまま呼び出し元へ返す（`handshake::read_next_frame_header` が
    /// 同じストリームで読み直せるよう、内部状態は変更しない）。
    fn fill_from_inner(&mut self) -> io::Result<usize> {
        // 読み取り量を `rx` の残り容量（`RecordBuffer::remaining_capacity`）
        // に絞る。固定 4096 バイトのまま読むと、相手が 1 レコードぶんの
        // 本文を境界をまたいで連続送信してきた場合に `feed` が取り込み
        // 切れない超過分（次のレコードの先頭バイト列）が生まれ、それを
        // 捨てると次のレコードが破損する（レビュー指摘・#966）。
        // `fill_from_inner` は `next_record` が `Ok(None)`（レコード未完成）
        // の場合にのみ呼ばれ、その時点で `rx` の使用量は必ず
        // `MAX_RECORD_WIRE_LEN` 未満のため、残り容量は常に 1 以上になる。
        let mut buf = [0u8; 4096];
        let capacity = self.rx.remaining_capacity().min(buf.len());
        let n = self.inner.read(&mut buf[..capacity])?;
        if n > 0 {
            let taken = self.rx.feed(buf.get(..n).unwrap_or(&[]));
            debug_assert_eq!(
                taken, n,
                "read was bounded by remaining_capacity, feed must take all of it"
            );
        }
        Ok(n)
    }
}

/// レコード列を 1 回の `write_all` にまとめて送出する（`TlsSession::
/// seal_application_data`／`close_notify`／`seal_fatal_alert` いずれも
/// 複数レコードに分割されうるため、書き込み対象を都度連結してから送る）。
fn write_records<W: Write>(w: &mut W, records: &[Record]) -> io::Result<()> {
    let mut out = Vec::new();
    for record in records {
        record
            .serialize_into(&mut out, RecordKind::Ciphertext)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "failed to serialize record")
            })?;
    }
    w.write_all(&out)
}

impl<S: WireStream> Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS connection already failed",
            ));
        }
        loop {
            if self.plaintext_pos < self.plaintext.len() {
                return Ok(self.drain_plaintext(buf));
            }
            if self.eof {
                return Ok(0);
            }

            match self.rx.next_record(RecordKind::Ciphertext) {
                Ok(Some(record)) => match self.session.open_record(&record) {
                    Ok(AppEvent::ApplicationData(data)) => {
                        if data.is_empty() {
                            continue;
                        }
                        // `READ_AHEAD_CAP`（= 1 レコード分の平文上限）を超える
                        // ことはない（`TlsSession::open_record` が 1 レコード
                        // ぶんの `TLSInnerPlaintext` しか返さないため）。
                        self.plaintext = data;
                        self.plaintext_pos = 0;
                        debug_assert!(self.plaintext.len() <= READ_AHEAD_CAP);
                        return Ok(self.drain_plaintext(buf));
                    }
                    Ok(AppEvent::CloseNotify) => {
                        self.eof = true;
                        return Ok(0);
                    }
                    Ok(AppEvent::UserCanceled) | Ok(AppEvent::Ignored) => {
                        continue;
                    }
                    Err(TlsSessionError::Protection(e)) => {
                        let desc = e.alert_description();
                        self.fail_with_alert(desc);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "TLS record protection failure",
                        ));
                    }
                    Err(TlsSessionError::AlertDecode(_))
                    | Err(TlsSessionError::ReceivedFatalAlert(_)) => {
                        // 相手からの alert（受信失敗を含む）に対してはこちらから
                        // alert を送り返さない（driver・RFC 8446 と同じ方針）。
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "TLS session received a fatal alert",
                        ));
                    }
                    Err(TlsSessionError::Poisoned) => {
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "TLS session already failed or closed",
                        ));
                    }
                },
                Ok(None) => {
                    // レコードがそろっていない: 生バイト列を追加で読む。
                    let n = self.fill_from_inner()?;
                    if n == 0 {
                        // 相手が close_notify なしで切断した。未消費の部分
                        // レコードが残っていれば truncation（暗号文の途中
                        // 切断）であり、正常な EOF（`Ok(0)`）として扱わず
                        // 破損状態へ倒す（fail-closed）。相手は既に送信側を
                        // 閉じているため alert は送り返さない。
                        if self.rx.finish().is_err() {
                            self.failed = true;
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "TLS record truncated by peer EOF",
                            ));
                        }
                        // レコード境界での EOF でも close_notify を受信して
                        // いない以上、正常終端とは区別できない（切り詰め
                        // 攻撃・応答末尾の欠落を正常終了と取り違えない。
                        // RFC 8446 §6.1）。`UnexpectedEof` で返しストリームを
                        // 失敗状態に固定する。正常終端（`Ok(0)`）は
                        // `AppEvent::CloseNotify` 受信後に限る。上位の pg
                        // wire 層（`framing::read_typed_frame_header`）は
                        // メッセージ境界での `UnexpectedEof` を平文 TCP の
                        // 切断と同じ「接続終了」として静かに扱い、メッセージ
                        // 途中なら `FrameError::Truncated`（応答なし）となる。
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "TLS peer closed the connection without close_notify",
                        ));
                    }
                }
                Err(e) => {
                    let desc = e.alert_description();
                    self.fail_with_alert(desc);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "TLS record framing failure",
                    ));
                }
            }
        }
    }
}

impl<S: WireStream> Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS connection already failed",
            ));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let records = self.session.seal_application_data(buf).map_err(|_| {
            self.failed = true;
            io::Error::new(io::ErrorKind::InvalidData, "TLS session cannot seal data")
        })?;
        if let Err(e) = write_records(&mut self.inner, &records) {
            // 書き込み失敗（タイムアウトを含む）はこの TLS ストリームを
            // 破損状態にする（fail-closed。部分的に送出済みのレコードが
            // 相手に届いているかもしれず、以後の送受信を安全に続けられない）。
            self.failed = true;
            return Err(e);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<S: WireStream> WireStream for TlsStream<S> {
    fn read_timeout(&self) -> io::Result<Option<std::time::Duration>> {
        self.inner.read_timeout()
    }

    fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    fn shutdown_both(&mut self) -> io::Result<()> {
        self.inner.shutdown_both()
    }

    fn shutdown_write(&mut self) -> io::Result<()> {
        // best-effort で `close_notify` を送ってから下層を閉じる
        // （RFC 8446 §6.1: 書き込み側を閉じる前に close_notify を送る）。
        if !self.failed {
            if let Ok(records) = self.session.close_notify() {
                let _ = write_records(&mut self.inner, &records);
            }
        }
        self.inner.shutdown_write()
    }

    fn emergency_channel(&self) -> Option<std::net::TcpStream> {
        // `engine::recovery::panic_hook::EmergencyResponseRegistration` は
        // 生の `TcpStream` へバイト列をそのまま書く契約のため、この生
        // 複製を平文のまま登録に使うと TLS レコードへ混入する。登録は
        // `emergency_response_channel`（下記の上書き）が暗号化してから
        // 行うため、この経路は常に `None`（モジュールドキュメント参照）。
        None
    }

    fn emergency_response_channel(
        &self,
        response: &[u8],
    ) -> Option<(Vec<u8>, std::net::TcpStream)> {
        // fail-closed: 既に破損済みの接続へは登録しない（モジュール
        // ドキュメント「緊急応答（RECOVER-6）との関係」節）。
        if self.failed {
            return None;
        }
        let record = self.session.seal_application_data_detached(response).ok()?;
        let mut ciphertext = Vec::new();
        record
            .serialize_into(&mut ciphertext, RecordKind::Ciphertext)
            .ok()?;
        // `self.inner`（生 `TcpStream`）の既定実装（`TcpStream::
        // try_clone`）を借りて、平文の代わりに暗号文を積んだ組を返す。
        self.inner.emergency_response_channel(&ciphertext)
    }

    fn graceful_close(&mut self) {
        if self.failed {
            return;
        }
        if let Ok(records) = self.session.close_notify() {
            let _ = write_records(&mut self.inner, &records);
        }
    }

    fn tls_server_end_point(&self) -> Option<&[u8]> {
        self.session.tls_server_end_point()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use super::super::key_schedule::EarlySecret;
    use super::super::record_protection::{Opener, Sealer};
    use super::super::x25519::EphemeralSecret;

    /// 実ハンドシェイクを経由せず、鍵スケジュール（[`EarlySecret`]・
    /// [`super::super::x25519::EphemeralSecret`]）を 1 回通してレコード
    /// 保護層以上のロジックだけを検証するための `TlsSession` 対を組み立てる
    /// （`record_protection.rs::tests::dummy_keys` と同じ手法）。client/server
    /// 方向の traffic secret は同一の handshake secret から HKDF ラベルで
    /// 分離されるため、1 回の鍵交換から双方向ぶんの鍵を得られる。
    fn test_session_pair() -> (Box<TlsSession>, Box<TlsSession>) {
        let early = EarlySecret::new_without_psk();
        let server_priv = [7u8; 32];
        let client_pub_raw = [9u8; 32];
        let secret = EphemeralSecret::from_bytes(server_priv);
        let shared = secret
            .diffie_hellman(&client_pub_raw)
            .expect("non-zero shared secret for distinct seed bytes");
        let hs = early.into_handshake(&shared).expect("valid HKDF params");
        let th = [0u8; super::super::hkdf::HASH_LEN];
        // `Sealer::seal`（`Epoch::Handshake`）は 0-RTT 非対応の送信契約により
        // `ApplicationData` の送信を拒否する（`record_protection.rs::Sealer::
        // seal` ドキュメント参照）ため、`TlsStream` の平文往復を検証するには
        // `Application` epoch まで鍵スケジュールを進める必要がある。
        // `install_application_keys` は直前に `install_handshake_keys` を
        // 要求するため、両方とも導出してから順番にインストールする。
        let hs_traffic = hs.traffic_secrets(&th).expect("valid HKDF params");
        let hs_server_keys = hs_traffic.server.traffic_keys().expect("valid HKDF params");
        let hs_client_keys = hs_traffic.client.traffic_keys().expect("valid HKDF params");
        let master = hs.into_master().expect("valid HKDF params");
        let app_traffic = master
            .application_traffic_secrets(&th)
            .expect("valid HKDF params");
        let server_keys = app_traffic
            .server
            .traffic_keys()
            .expect("valid HKDF params");
        let client_keys = app_traffic
            .client
            .traffic_keys()
            .expect("valid HKDF params");

        let mut server_sealer = Sealer::new();
        server_sealer
            .install_handshake_keys(&hs_server_keys)
            .expect("install handshake");
        server_sealer
            .install_application_keys(&server_keys)
            .expect("install application");
        let mut server_opener = Opener::new();
        server_opener
            .install_handshake_keys(&hs_client_keys)
            .expect("install handshake");
        server_opener
            .install_application_keys(&client_keys)
            .expect("install application");

        let mut client_sealer = Sealer::new();
        client_sealer
            .install_handshake_keys(&hs_client_keys)
            .expect("install handshake");
        client_sealer
            .install_application_keys(&client_keys)
            .expect("install application");
        let mut client_opener = Opener::new();
        client_opener
            .install_handshake_keys(&hs_server_keys)
            .expect("install handshake");
        client_opener
            .install_application_keys(&server_keys)
            .expect("install application");

        (
            Box::new(TlsSession::new_for_tests(server_sealer, server_opener)),
            Box::new(TlsSession::new_for_tests(client_sealer, client_opener)),
        )
    }

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (server, client)
    }

    #[test]
    fn round_trip_writes_and_reads_plaintext() {
        let (server_sock, client_sock) = loopback_pair();
        let (server_session, client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);
        let mut client = TlsStream::new(client_sock, client_session);

        server.write_all(b"hello over tls").expect("write");
        let mut buf = [0u8; 64];
        let n = client.read(&mut buf).expect("read");
        assert_eq!(&buf[..n], b"hello over tls");
    }

    #[test]
    fn write_of_empty_buffer_is_a_no_op() {
        let (server_sock, _client_sock) = loopback_pair();
        let (server_session, _client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);
        assert_eq!(server.write(&[]).expect("empty write"), 0);
    }

    /// 受入基準 4 の核心: レコード本文が複数回の生 `read` に分かれて届いた
    /// 場合でも、`WouldBlock`（読み取りタイムアウト）を挟んで読み直しても
    /// 平文が正しく続くこと（push 型読み取り方式の存在意義。設計は
    /// `docs/design/tls-wire-connection.md` 参照）。
    #[test]
    fn read_recovers_after_would_block_mid_record() {
        let (mut server_sock, client_sock) = loopback_pair();
        let (mut server_session, client_session) = test_session_pair();
        let mut client = TlsStream::new(client_sock, client_session);

        // レコードの生バイト列を書き手側で組み立て、意図的に前半・後半へ
        // 分割して間に sleep を挟む（`TlsStream::write` を使うと 1 回の
        // `write_all` で送られてしまい、受け手側が必ず 1 回の `read` で
        // レコード全体を受け取れてしまうため、`WouldBlock` を意図的に
        // 起こせない＝レビュー指摘: 元のテストは vacuous だった）。
        let records = server_session
            .seal_application_data(b"across two reads")
            .expect("seal");
        let mut wire = Vec::new();
        for record in &records {
            record
                .serialize_into(&mut wire, RecordKind::Ciphertext)
                .expect("serialize");
        }
        assert!(
            wire.len() > 4,
            "need at least a few bytes to split meaningfully"
        );
        let split_at = wire.len() / 2;

        let writer = std::thread::spawn(move || {
            server_sock
                .write_all(&wire[..split_at])
                .expect("write first half");
            std::thread::sleep(Duration::from_millis(50));
            server_sock
                .write_all(&wire[split_at..])
                .expect("write second half");
        });

        // クライアント側の生ソケットに極短い読み取りタイムアウトを設定する。
        // 前半しか届いていない間の `read` は必ず `WouldBlock`/`TimedOut` に
        // なることを検証してから、後半到着後に読み直して完成させる。
        client
            .set_read_timeout(Some(Duration::from_millis(1)))
            .expect("set timeout");
        let mut buf = [0u8; 64];
        let first = client.read(&mut buf);
        assert!(
            matches!(
                &first,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut
            ),
            "expected the first read (before the second half arrives) to time out, got {first:?}"
        );

        let mut last_err_was_timeout = true;
        for _ in 0..5000 {
            match client.read(&mut buf) {
                Ok(n) => {
                    assert_eq!(&buf[..n], b"across two reads");
                    writer.join().expect("writer thread must not panic");
                    return;
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    last_err_was_timeout = true;
                    continue;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(last_err_was_timeout, "expected at least one timeout");
        panic!("read never completed despite repeated retries");
    }

    /// レビュー指摘の回帰確認（Issue #966）: 1 レコードぶんの本文が複数回の
    /// `read` にまたがって届き、かつその区切りが `RecordBuffer` の残り容量
    /// ちょうどに来る場合でも、次のレコードの先頭バイト列を取りこぼさない
    /// こと（`fill_from_inner` を固定長 4096 バイトのまま読んでいた旧実装
    /// では、1 回の `read` が複数レコードにまたがった際に `feed` が
    /// 取り込み切れなかった超過分を捨てており、次のレコードが破損した）。
    #[test]
    fn read_does_not_drop_bytes_when_two_records_arrive_in_one_chunk_at_capacity_boundary() {
        let (mut server_sock, client_sock) = loopback_pair();
        let (mut server_session, client_session) = test_session_pair();
        let mut client = TlsStream::new(client_sock, client_session);

        // 1 レコードの平文上限（`MAX_PLAINTEXT_LEN` = 16384 バイト）ぴったりの
        // メッセージに続けて短いメッセージを送る。`seal_application_data` は
        // `MAX_PLAINTEXT_LEN` ごとに分割するため、大きい方は複数レコードに
        // 分かれず 1 レコードで済む。
        let big_payload = vec![0xABu8; MAX_PLAINTEXT_LEN];
        let small_payload = b"tail".to_vec();

        let mut wire = Vec::new();
        for record in server_session
            .seal_application_data(&big_payload)
            .expect("seal big")
        {
            record
                .serialize_into(&mut wire, RecordKind::Ciphertext)
                .expect("serialize big");
        }
        for record in server_session
            .seal_application_data(&small_payload)
            .expect("seal small")
        {
            record
                .serialize_into(&mut wire, RecordKind::Ciphertext)
                .expect("serialize small");
        }

        let writer = std::thread::spawn(move || {
            server_sock.write_all(&wire).expect("write combined wire");
        });

        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        while received.len() < big_payload.len() + small_payload.len() {
            let n = client.read(&mut buf).expect("read");
            assert!(n > 0, "must not observe premature EOF");
            received.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&received[..big_payload.len()], big_payload.as_slice());
        assert_eq!(&received[big_payload.len()..], small_payload.as_slice());

        writer.join().expect("writer thread must not panic");
    }

    #[test]
    fn write_after_failed_write_keeps_failing_read_and_write() {
        // 書き込み先を即座にクローズしたソケットへ書くと `write_all` が
        // 失敗する。以後の read/write が fail-closed になることを固定する。
        let (server_sock, client_sock) = loopback_pair();
        drop(client_sock);
        let (server_session, _client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);

        // 相手が閉じた直後の 1 回目の書き込みは OS 側の事情で成功することが
        // あるため、複数回書いて確実に失敗させる。
        let mut failed = false;
        for _ in 0..64 {
            if server.write_all(b"x").is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "expected write to eventually fail");
        assert!(server.write(b"y").is_err(), "must stay fail-closed");
        let mut buf = [0u8; 1];
        assert!(server.read(&mut buf).is_err(), "must stay fail-closed");
    }

    /// `fail_with_alert` の経路（`TlsSessionError::Protection` を受けた際
    /// fatal alert を best-effort 送出してから `failed` に固定する）を、
    /// 改ざんした暗号文を実際に読ませて固定する。読み手は `InvalidData` を
    /// 返し、送り手側の生ソケットには fatal alert（レコードヘッダの先頭
    /// バイトは `ContentType::Alert` = 0x15）が届く。
    /// 部分レコードを送った直後に相手が close_notify なしで切断した場合、
    /// 正常な EOF（`Ok(0)`）ではなくエラーとして扱い、以後の読み取りも
    /// 失敗し続けること（truncation の fail-closed。PR #1056 レビュー指摘）。
    #[test]
    fn truncated_record_at_eof_fails_closed_instead_of_clean_eof() {
        let (mut server_sock, client_sock) = loopback_pair();
        let (mut server_session, client_session) = test_session_pair();
        let mut client = TlsStream::new(client_sock, client_session);

        let records = server_session
            .seal_application_data(b"cut in the middle")
            .expect("seal");
        let mut wire = Vec::new();
        for record in &records {
            record
                .serialize_into(&mut wire, RecordKind::Ciphertext)
                .expect("serialize");
        }
        let cut = wire.len() / 2;
        server_sock
            .write_all(wire.get(..cut).expect("prefix"))
            .expect("write partial record");
        server_sock
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown write");

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set timeout");
        let mut buf = [0u8; 64];
        let err = client
            .read(&mut buf)
            .expect_err("truncated record must not be a clean EOF");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let again = client
            .read(&mut buf)
            .expect_err("stream must stay failed after truncation");
        assert_eq!(again.kind(), io::ErrorKind::InvalidData);
    }

    /// レコード境界であっても close_notify を受信せずに EOF になった場合は
    /// 正常終端（`Ok(0)`）ではなく `UnexpectedEof` を返し、以後の読み書きも
    /// 失敗し続けること（切断と正常終了を取り違えない。PR #1056 レビュー
    /// 指摘）。
    #[test]
    fn eof_on_record_boundary_without_close_notify_fails_closed() {
        let (server_sock, client_sock) = loopback_pair();
        let (_server_session, client_session) = test_session_pair();
        let mut client = TlsStream::new(client_sock, client_session);
        server_sock
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown write");

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set timeout");
        let mut buf = [0u8; 64];
        let err = client
            .read(&mut buf)
            .expect_err("EOF without close_notify must not be a clean EOF");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            client.read(&mut buf).is_err(),
            "stream must stay failed after EOF without close_notify"
        );
        assert!(
            client.write(b"x").is_err(),
            "writes must also fail after EOF without close_notify"
        );
    }

    /// 平文を受け取った後に close_notify を受信し、その後に生ソケットが
    /// EOF になった場合は正常終端（`Ok(0)`）として扱い、繰り返し読んでも
    /// `Ok(0)` のままであること。
    #[test]
    fn eof_after_close_notify_is_clean_eof() {
        let (mut server_sock, client_sock) = loopback_pair();
        let (mut server_session, client_session) = test_session_pair();
        let mut client = TlsStream::new(client_sock, client_session);

        let mut records = server_session
            .seal_application_data(b"last words")
            .expect("seal");
        records.extend(server_session.close_notify().expect("close_notify"));
        let mut wire = Vec::new();
        for record in &records {
            record
                .serialize_into(&mut wire, RecordKind::Ciphertext)
                .expect("serialize");
        }
        server_sock.write_all(&wire).expect("write records");
        server_sock
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown write");

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set timeout");
        let mut out = Vec::new();
        client
            .read_to_end(&mut out)
            .expect("clean EOF after close_notify");
        assert_eq!(out, b"last words");
        let mut buf = [0u8; 8];
        assert_eq!(client.read(&mut buf).expect("still clean eof"), 0);
    }

    #[test]
    fn corrupted_ciphertext_sends_fatal_alert_and_fails_closed() {
        let (mut server_sock, client_sock) = loopback_pair();
        let (server_session, client_session) = test_session_pair();
        let mut client = TlsStream::new(client_sock, client_session);

        let mut server_session = server_session;
        let records = server_session
            .seal_application_data(b"tampered")
            .expect("seal");
        let mut wire = Vec::new();
        for record in &records {
            record
                .serialize_into(&mut wire, RecordKind::Ciphertext)
                .expect("serialize");
        }
        // レコードヘッダ（5 バイト）より後ろの暗号文本体を 1 バイト反転し、
        // AEAD タグ検証が必ず失敗するようにする。
        if let Some(byte) = wire.get_mut(5) {
            *byte ^= 0xFF;
        }
        server_sock.write_all(&wire).expect("write tampered record");

        let mut buf = [0u8; 64];
        let err = client
            .read(&mut buf)
            .expect_err("must reject bad_record_mac");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // 送り手側の生ソケットに fatal alert レコードが届くことを確認する。
        // TLS 1.3 のハンドシェイク完了後は全レコードの外側 content type が
        // `ApplicationData`（RFC 8446 §5.2）に固定されるため、実際の種別
        // （alert かどうか）は復号しないと分からない。`server_session`
        // （送信側と鍵材料が対応する opener）で開き、fatal alert の受信
        // として分類されることを確認する。
        server_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let alert_record = record::read_record(&mut server_sock, RecordKind::Ciphertext)
            .expect("read alert record")
            .expect("alert record present");
        let open_err = server_session
            .open_record(&alert_record)
            .expect_err("must be a fatal alert, not application data");
        assert!(
            matches!(open_err, TlsSessionError::ReceivedFatalAlert(_)),
            "expected ReceivedFatalAlert, got {open_err:?}"
        );
    }

    // ---- emergency_response_channel（RECOVER-6・Issue #1080） ----

    /// 健全な application epoch の接続では、緊急応答（平文の
    /// ErrorResponse バイト列）を暗号化済み TLS レコードとして登録できる。
    /// 相手側の `Opener`（`client_session`）で正しく復号でき、元の平文と
    /// 完全一致すること（`WireStream::emergency_channel` 経由で平文の
    /// ままだった旧経路との違いの核心）。detached seal がシーケンス番号を
    /// 消費しないことは `record_protection::tests::
    /// seal_detached_does_not_advance_sequence_number` で固定済みのため、
    /// ここでは統合経路（`TlsStream` を通した暗号文の組み立て・直列化）が
    /// 正しく機能することを確認する。
    #[test]
    fn emergency_response_channel_produces_ciphertext_the_peer_can_decrypt() {
        let (server_sock, mut client_sock) = loopback_pair();
        let (server_session, mut client_session) = test_session_pair();
        let server = TlsStream::new(server_sock, server_session);

        let response = b"S=ERROR\0C=XX000\0D=state=may_be_committed\0\0".to_vec();
        let (bytes, mut raw) = server
            .emergency_response_channel(&response)
            .expect("must register on a healthy application-epoch connection");
        raw.write_all(&bytes)
            .expect("write ciphertext via raw clone");

        client_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set timeout");
        let record = record::read_record(&mut client_sock, RecordKind::Ciphertext)
            .expect("read record")
            .expect("record present");
        let event = client_session
            .open_record(&record)
            .expect("open detached-sealed record");
        assert_eq!(event, AppEvent::ApplicationData(response));
    }

    /// 既に破損状態（`failed`）の接続では登録しない（fail-closed。接続断
    /// のみへ縮退）。
    #[test]
    fn emergency_response_channel_returns_none_when_already_failed() {
        let (server_sock, client_sock) = loopback_pair();
        drop(client_sock);
        let (server_session, _client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);

        let mut failed = false;
        for _ in 0..64 {
            if server.write_all(b"x").is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "expected write to eventually fail");
        assert!(
            server.emergency_response_channel(b"unused").is_none(),
            "must not register on an already-failed connection"
        );
    }

    /// `close_notify` を送出済み（送信方向が終了済み）の接続では、
    /// [`TlsSession::seal_application_data_detached`] が `Poisoned` を返す
    /// ため登録しない。
    #[test]
    fn emergency_response_channel_returns_none_after_close_notify() {
        let (server_sock, _client_sock) = loopback_pair();
        let (server_session, _client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);

        server.graceful_close();
        assert!(
            server.emergency_response_channel(b"unused").is_none(),
            "must not register after this connection has sent close_notify"
        );
    }
}
