//! レプリケーション接続の読み書きの抽象（Issue #2177・親 #2171・RECOVER-15 (3)・WIRE-9 ポインタ。方式の記録は
//! `docs/design/replication-streaming.md` の「実装判断（Issue #2177）」）。
//!
//! 責務境界: 送信側（[`crate::replication`]）・受信側（[`crate::replica`]・[`crate::replica_resync`]）が
//! 同じフレーム処理（Hello・Identity・Record・Ack 等）を、平文の `TcpStream` と TLS 1.3 の上で同じ形で
//! 使えるようにする。接続は読み取り半分（[`ConnReader`]）と書き込み半分（[`ConnWriter`]）へ分け、TLS の
//! セッション（[`TlsSession`]）は 2 つの半分で `Arc<Mutex<..>>` を共有する。送信側は 1 接続で「送信ループ」「ack
//! の読み取り」「監視」の 3 スレッドが動くため、**ロックは seal／open の CPU 処理の間だけ保持し、ソケットの
//! 読み書き中は決して保持しない**（受信スレッドが読み取りでブロックしたまま送信を止めるデッドロックを作らない）。
//!
//! タイムアウト・`shutdown` は平文と TLS で同じ OS ソケット（`raw`。`try_clone` で得た同一ソケット）に作用するので、
//! 監視スレッドの `shutdown` によるブロック解除はそのまま効く。TLS のセッションが壊れた（復号失敗・fatal alert・
//! `Mutex` の poison）場合は、読み書きとも以後エラーで返す（fail-closed）。
//!
//! 入力は untrusted: 受信バッファは [`RecordBuffer`] で有界、平文の一時保持は 1 レコード分（16 KiB）まで。
#![cfg(unix)]

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::tls::record::{RecordBuffer, RecordKind};
use crate::tls::server_handshake::{AppEvent, TlsSession};

/// 2 つの半分で共有する TLS セッション。
type SharedSession = Arc<Mutex<Box<TlsSession>>>;

fn lock(session: &SharedSession) -> io::Result<MutexGuard<'_, Box<TlsSession>>> {
    // poison は別スレッドがセッション操作中に panic した印。状態を確信できないので使わない（fail-closed）。
    session
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "TLS session unavailable"))
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// 接続の読み取り半分。
pub struct ConnReader {
    raw: TcpStream,
    tls: Option<SharedSession>,
    rx: RecordBuffer,
    plain: Vec<u8>,
    pos: usize,
    eof: bool,
    failed: bool,
}

/// 接続の書き込み半分。
pub struct ConnWriter {
    raw: TcpStream,
    tls: Option<SharedSession>,
    failed: bool,
}

/// 平文の接続を読み書きの半分へ分ける。
pub fn plain(stream: TcpStream) -> io::Result<(ConnReader, ConnWriter)> {
    let write_half = stream.try_clone()?;
    Ok((
        ConnReader::new(stream, None),
        ConnWriter {
            raw: write_half,
            tls: None,
            failed: false,
        },
    ))
}

/// TLS ハンドシェイク済みの接続を読み書きの半分へ分ける。`session` は application epoch へ
/// 切り替え済みのもの（サーバー側・クライアント側どちらのハンドシェイクの結果でもよい）。
pub fn tls(stream: TcpStream, session: Box<TlsSession>) -> io::Result<(ConnReader, ConnWriter)> {
    let write_half = stream.try_clone()?;
    let shared: SharedSession = Arc::new(Mutex::new(session));
    Ok((
        ConnReader::new(stream, Some(Arc::clone(&shared))),
        ConnWriter {
            raw: write_half,
            tls: Some(shared),
            failed: false,
        },
    ))
}

impl ConnReader {
    fn new(raw: TcpStream, tls: Option<SharedSession>) -> Self {
        Self {
            raw,
            tls,
            rx: RecordBuffer::new(),
            plain: Vec::new(),
            pos: 0,
            eof: false,
            failed: false,
        }
    }

    /// 読み取りタイムアウトを設定する（同じソケット。TLS でも効く）。
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.raw.set_read_timeout(timeout)
    }

    /// 接続を両方向とも閉じるためのハンドル（同一ソケットの複製）。監視スレッドが使う。
    pub fn shutdown_handle(&self) -> io::Result<ShutdownHandle> {
        Ok(ShutdownHandle(self.raw.try_clone()?))
    }

    /// 続きのデータが `wait` 以内に届くか（読み出しは消費しない）。`None` は切断・エラー。
    /// TLS では、復号済みの平文・未処理のレコードを既に抱えていれば待たずに `true`。
    pub fn data_ready(&mut self, wait: Duration) -> Option<bool> {
        if self.failed {
            return None;
        }
        if self.pos < self.plain.len() || self.rx.has_buffered() {
            return Some(true);
        }
        if self.eof {
            return None;
        }
        self.raw.set_read_timeout(Some(wait)).ok()?;
        let mut probe = [0u8; 1];
        match self.raw.peek(&mut probe) {
            Ok(0) => None,
            Ok(_) => Some(true),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Some(false)
            }
            Err(_) => None,
        }
    }

    fn drain(&mut self, buf: &mut [u8]) -> usize {
        let remaining = self.plain.get(self.pos..).unwrap_or(&[]);
        let n = remaining.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), remaining.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.pos = self.pos.saturating_add(n);
        if self.pos >= self.plain.len() {
            self.plain.clear();
            self.pos = 0;
        }
        n
    }

    fn read_tls(&mut self, session: &SharedSession, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.plain.len() {
                return Ok(self.drain(buf));
            }
            if self.eof {
                return Ok(0);
            }
            match self.rx.next_record(RecordKind::Ciphertext) {
                Ok(Some(record)) => {
                    // ロックは復号の間だけ。ソケットの読み出し中は持たない。
                    let event = lock(session)?.open_record(&record);
                    match event {
                        Ok(AppEvent::ApplicationData(data)) => {
                            if data.is_empty() {
                                continue;
                            }
                            self.plain = data;
                            self.pos = 0;
                        }
                        Ok(AppEvent::CloseNotify) => {
                            self.eof = true;
                            return Ok(0);
                        }
                        Ok(AppEvent::UserCanceled | AppEvent::Ignored) => {}
                        Err(_) => {
                            self.failed = true;
                            return Err(invalid("TLS record protection failure"));
                        }
                    }
                }
                Ok(None) => {
                    // 1 チャンク読む（残り容量に絞る。超過分を捨てて次のレコードを壊さない）。
                    let mut chunk = [0u8; 4096];
                    let cap = self.rx.remaining_capacity().min(chunk.len());
                    let dst = chunk.get_mut(..cap).ok_or_else(|| invalid("TLS buffer"))?;
                    let n = self.raw.read(dst)?;
                    if n == 0 {
                        // close_notify なしの切断は、境界上でも切り詰めとして扱う（正常終端と区別する）。
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "TLS peer closed the connection without close_notify",
                        ));
                    }
                    if let Some(got) = chunk.get(..n) {
                        self.rx.feed(got);
                    }
                }
                Err(_) => {
                    self.failed = true;
                    return Err(invalid("TLS record framing failure"));
                }
            }
        }
    }
}

impl Read for ConnReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(invalid("connection already failed"));
        }
        match self.tls.clone() {
            None => self.raw.read(buf),
            Some(session) => self.read_tls(&session, buf),
        }
    }
}

/// 同一ソケットを両方向とも閉じるハンドル。
pub struct ShutdownHandle(TcpStream);

impl ShutdownHandle {
    /// 読み書きとも閉じる（失敗は無視。切断済みでもよい）。
    pub fn shutdown(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

impl ConnWriter {
    /// 書き込みタイムアウトを設定する（同じソケット。TLS でも効く）。
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.raw.set_write_timeout(timeout)
    }

    /// 同一ソケットを両方向とも閉じる（失敗は無視）。
    pub fn shutdown(&self) {
        let _ = self.raw.shutdown(Shutdown::Both);
    }
}

impl Write for ConnWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(invalid("connection already failed"));
        }
        let Some(session) = self.tls.clone() else {
            return self.raw.write(buf);
        };
        if buf.is_empty() {
            return Ok(0);
        }
        // ロックは seal の間だけ。ソケットへの書き込み中は持たない。書き込みは 1 スレッドだけが行う
        // （送信ループ。受信側はフレームを書く単一スレッド）ので、レコードの順序は崩れない。
        let records = lock(&session)?.seal_application_data(buf).map_err(|_| {
            self.failed = true;
            invalid("TLS session cannot seal data")
        })?;
        let mut out = Vec::new();
        for r in &records {
            r.serialize_into(&mut out, RecordKind::Ciphertext)
                .map_err(|_| invalid("failed to serialize record"))?;
        }
        if let Err(e) = self.raw.write_all(&out) {
            // 部分的に送出されたレコードが相手に届いているかもしれない。以後は使わない。
            self.failed = true;
            return Err(e);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // 平文は `TcpStream` が即時に送る。TLS の `write` は seal 済みのレコードを書き切ってから戻る。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let c = TcpStream::connect(addr).expect("connect");
        let (s, _) = l.accept().expect("accept");
        (c, s)
    }

    #[test]
    fn plain_halves_round_trip_and_report_readiness() {
        let (c, s) = pair();
        let (cr, mut cw) = plain(c).expect("split");
        let (mut sr, _sw) = plain(s).expect("split");
        assert_eq!(sr.data_ready(Duration::from_millis(20)), Some(false));
        cw.write_all(b"hello").expect("write");
        assert_eq!(sr.data_ready(Duration::from_secs(2)), Some(true));
        let mut buf = [0u8; 5];
        sr.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"hello");
        // 閉じると `None`（切断）。
        let h = cr.shutdown_handle().expect("handle");
        h.shutdown();
        drop(cw);
        assert_eq!(sr.data_ready(Duration::from_secs(2)), None);
    }
}
