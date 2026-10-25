//! 実行中の文のためのクライアント切断検出（Issue #1798。設計記録:
//! `docs/design/statement-interrupt.md`）。
//!
//! 役割: `handshake::post_auth_loop` が `'Q'`／`'E'` の処理に入るたびに、接続ソケットの複製を
//! probe する poll を engine の中断スコープ（`engine::interrupt`）へ積む。engine の走査ループが
//! 一定間隔で poll を呼び、切断を検出したら `57014` で走査を止める（切断後の空回りで接続スロット
//! と CPU を占有し続けないための資源保護）。
//!
//! 未読バイトがあると `MSG_PEEK` は FIN 到達後も同じバイトを返し続け、切断を判別できない
//! （Execute と Sync を続けて送って FIN で閉じる通常の拡張クエリで起きる）。そこで probe は
//! 読めるバイトを上限つきの side buffer（[`SideBuf`]）へ実際に読み取り、その後の read が 0 を
//! 返したら FIN と判定する。接続の受信経路は [`SideBufTcp`] で、次の読み取りの前にまず
//! side buffer を消費するため、バイトは欠けず順序も崩れない。side buffer が上限に達したら
//! 切断判定はせず生存扱いにする（クライアントがまだ送信している状態。無制限確保の禁止）。
//!
//! PostgreSQL 17 との差: PG は既定（`client_connection_check_interval = 0`）では実行中の切断を
//! 検出しない。本実装は安全側で常に検出する。書き込み側だけ閉じた（半閉じ）クライアントも
//! 切断として扱う。

use std::cell::RefCell;
use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use engine::interrupt::{InterruptGuard, InterruptReason, InterruptScope};

/// ソケット probe の最小間隔。engine 側の poll 頻度（`CHECK_STRIDE` 行ごと）が高くても、
/// システムコールはこの間隔に 1 回へ抑える。スコープ導入後の初回は必ず probe する。
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// side buffer の上限バイト数。untrusted な受信データの先読み量を有界にする。
/// 1 回の probe で読むのもこの残量までで、満杯の間は切断判定をしない（生存扱い）。
pub(crate) const SIDE_BUF_CAP: usize = 64 * 1024;

/// probe が先読みした未消費バイトの退避先。`SideBufTcp::read` が先頭から消費する。
#[derive(Debug, Default)]
pub(crate) struct SideBuf {
    buf: Vec<u8>,
    pos: usize,
}

impl SideBuf {
    /// 未消費バイト数。
    fn pending(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// 消費済み領域を詰める。追記前に必ず呼び、保持量（`buf.len()`・確保量）を未消費分で抑える。
    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.drain(..self.pos.min(self.buf.len()));
            self.pos = 0;
        }
    }

    /// 先頭から最大 `out.len()` バイトを `out` へ移して件数を返す。
    fn drain_into(&mut self, out: &mut [u8]) -> usize {
        let src = self.buf.get(self.pos..).unwrap_or(&[]);
        let n = src.len().min(out.len());
        if let (Some(dst), Some(s)) = (out.get_mut(..n), src.get(..n)) {
            dst.copy_from_slice(s);
        }
        self.pos = self.pos.saturating_add(n);
        if self.pos >= self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        n
    }
}

/// 切断検出用の複製ソケットと、受信経路と共有する side buffer の組。
pub struct LivenessProbe {
    sock: TcpStream,
    side: Rc<RefCell<SideBuf>>,
}

/// 接続の受信経路になる `TcpStream` ラッパー（`handshake` が認証後セッションの直前で包む。
/// TLS 接続では `TlsStream` の下層になるため、side buffer は TLS レコード復号より前の生バイト列
/// で順序が保たれる）。読み取りは side buffer を先に消費し、書き込み等は素通しする。
pub(crate) struct SideBufTcp {
    sock: TcpStream,
    side: Rc<RefCell<SideBuf>>,
}

impl SideBufTcp {
    /// 空の side buffer を持つラッパーを作る。
    pub(crate) fn new(sock: TcpStream) -> Self {
        Self {
            sock,
            side: Rc::new(RefCell::new(SideBuf::default())),
        }
    }

    /// 下層の `TcpStream`（タイムアウト設定・shutdown・複製用）。
    pub(crate) fn socket(&self) -> &TcpStream {
        &self.sock
    }

    /// 切断検出用の probe を作る（ソケット複製が取れなければ `None`）。
    pub(crate) fn probe(&self) -> Option<LivenessProbe> {
        Some(LivenessProbe {
            sock: self.sock.try_clone().ok()?,
            side: Rc::clone(&self.side),
        })
    }
}

impl Read for SideBufTcp {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        {
            let mut side = self.side.borrow_mut();
            if side.pending() > 0 {
                return Ok(side.drain_into(buf));
            }
        }
        self.sock.read(buf)
    }
}

impl Write for SideBufTcp {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sock.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// ソケットが切断されたかを 1 回判定する。読めるバイトは side buffer へ退避する（失わない）。
///
/// 非ブロッキング化は複製と元ソケットで共有されるため、必ず blocking へ戻す。戻せなかった場合は
/// 接続を閉じる側（切断扱い）へ倒す。呼び出しは接続スレッド上で、他の読み書きと並行しない。
pub(crate) fn peer_is_gone(probe: &LivenessProbe) -> bool {
    if probe.sock.set_nonblocking(true).is_err() {
        return true;
    }
    let gone = drain_to_side(probe);
    if probe.sock.set_nonblocking(false).is_err() {
        return true;
    }
    gone
}

/// 非ブロッキングの `probe.sock` から side buffer の残量まで読み取り、FIN／エラーなら真。
fn drain_to_side(probe: &LivenessProbe) -> bool {
    let mut side = probe.side.borrow_mut();
    side.compact();
    let mut chunk = [0u8; 4096];
    loop {
        let room = SIDE_BUF_CAP.saturating_sub(side.pending());
        if room == 0 {
            // 上限に達した: クライアントはまだ送信中とみなし、切断判定はしない。
            return false;
        }
        let want = room.min(chunk.len());
        let Some(dst) = chunk.get_mut(..want) else {
            return true;
        };
        match (&probe.sock).read(dst) {
            // FIN 受信済み（相手が閉じた、または書き込み側を閉じた）。
            Ok(0) => return true,
            Ok(n) => {
                if let Some(got) = chunk.get(..n) {
                    side.buf.extend_from_slice(got);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // ECONNRESET 等。
            Err(_) => return true,
        }
    }
}

/// `sock` の切断検出を現在のスレッドの中断スコープとして導入する。`sock` が `None`
/// （検出手段のない接続）なら何も導入しない。返ったガードの drop で外れる。
pub(crate) fn install(sock: Option<&Rc<LivenessProbe>>) -> Option<InterruptGuard> {
    let sock = Rc::clone(sock?);
    let mut last_probe: Option<Instant> = None;
    Some(InterruptScope::install(Box::new(move || {
        let now = Instant::now();
        if let Some(prev) = last_probe {
            if now.saturating_duration_since(prev) < PROBE_INTERVAL {
                return None;
            }
        }
        last_probe = Some(now);
        peer_is_gone(&sock).then_some(InterruptReason::ClientGone)
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (SideBufTcp, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let c = TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
        let (s, _) = l.accept().expect("accept");
        (SideBufTcp::new(s), c)
    }

    fn probe_of(server: &SideBufTcp) -> LivenessProbe {
        server.probe().expect("probe")
    }

    #[test]
    fn idle_peer_is_alive() {
        let (server, _client) = pair();
        assert!(!peer_is_gone(&probe_of(&server)));
    }

    #[test]
    fn closed_peer_is_gone() {
        let (server, client) = pair();
        drop(client);
        // FIN の到達を待つ。
        std::thread::sleep(Duration::from_millis(50));
        assert!(peer_is_gone(&probe_of(&server)));
    }

    /// 未読バイトが残ったままの FIN を検出する（MSG_PEEK では判別できなかったケース）。
    #[test]
    fn unread_bytes_then_fin_is_gone() {
        let (server, mut client) = pair();
        client.write_all(b"ES").expect("write");
        drop(client);
        std::thread::sleep(Duration::from_millis(50));
        assert!(peer_is_gone(&probe_of(&server)));
    }

    /// FIN 前の未読バイトは生存扱いで、side buffer へ退避されて後続の read で欠けず順に読める。
    #[test]
    fn unread_bytes_are_alive_and_read_back_in_order() {
        let (mut server, mut client) = pair();
        let probe = probe_of(&server);
        client.write_all(b"abc").expect("write");
        std::thread::sleep(Duration::from_millis(50));
        assert!(!peer_is_gone(&probe));
        // 退避後に届いたバイトも順序どおり後ろへ続く。
        client.write_all(b"def").expect("write");
        std::thread::sleep(Duration::from_millis(50));
        assert!(!peer_is_gone(&probe));
        let mut got = [0u8; 6];
        server.read_exact(&mut got).expect("read back");
        assert_eq!(&got, b"abcdef");
        // 退避分を読み切ったあとは通常どおりソケットから読む。
        client.write_all(b"g").expect("write");
        let mut b = [0u8; 1];
        server.read_exact(&mut b).expect("read live");
        assert_eq!(&b, b"g");
    }

    /// 小さい `buf` での読み取りでも side buffer を分割して順に返す。
    #[test]
    fn side_buffer_drains_in_small_reads() {
        let (mut server, mut client) = pair();
        client.write_all(b"wxyz").expect("write");
        std::thread::sleep(Duration::from_millis(50));
        assert!(!peer_is_gone(&probe_of(&server)));
        let mut one = [0u8; 1];
        let mut out = Vec::new();
        for _ in 0..4 {
            server.read_exact(&mut one).expect("read");
            out.extend_from_slice(&one);
        }
        assert_eq!(out, b"wxyz");
    }

    /// 退避後に FIN が来ても、退避済みバイトは切断判定後も読み出せる。
    #[test]
    fn buffered_bytes_survive_fin() {
        let (mut server, mut client) = pair();
        let probe = probe_of(&server);
        client.write_all(b"ES").expect("write");
        drop(client);
        std::thread::sleep(Duration::from_millis(50));
        assert!(peer_is_gone(&probe));
        let mut got = [0u8; 2];
        server.read_exact(&mut got).expect("buffered bytes kept");
        assert_eq!(&got, b"ES");
        assert_eq!(server.read(&mut got).expect("eof"), 0);
    }

    /// side buffer が上限に達したら切断判定はせず生存扱いにし、確保量も上限を超えない。
    #[test]
    fn full_side_buffer_is_alive_and_bounded() {
        let (mut server, mut client) = pair();
        let probe = probe_of(&server);
        let payload = vec![7u8; SIDE_BUF_CAP + 4096];
        let writer = std::thread::spawn(move || {
            client.write_all(&payload).expect("write");
            client
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!peer_is_gone(&probe));
        assert!(probe.side.borrow().pending() <= SIDE_BUF_CAP);
        // 全量が欠けず読める。
        let mut all = vec![0u8; SIDE_BUF_CAP + 4096];
        server.read_exact(&mut all).expect("read all");
        assert!(all.iter().all(|&b| b == 7));
        drop(writer.join().expect("join"));
    }

    /// 未消費データを常に残したまま probe と部分 read を繰り返しても、保持量（len・capacity）が
    /// 上限内に収まる（消費済み領域を詰めずに追記し続けるとメモリが増え続けた）。
    #[test]
    fn repeated_probe_and_partial_read_keeps_memory_bounded() {
        let (mut server, mut client) = pair();
        let probe = probe_of(&server);
        for _ in 0..2000 {
            client.write_all(&[1u8; 100]).expect("write");
            std::thread::sleep(Duration::from_millis(1));
            assert!(!peer_is_gone(&probe));
            // 常に未消費データを残すため、書いた量より少なく読む。
            let mut small = [0u8; 90];
            server.read_exact(&mut small).expect("partial read");
            let side = probe.side.borrow();
            assert!(side.buf.len() <= SIDE_BUF_CAP, "len {}", side.buf.len());
            assert!(
                side.buf.capacity() <= 2 * SIDE_BUF_CAP,
                "cap {}",
                side.buf.capacity()
            );
        }
    }

    #[test]
    fn socket_is_blocking_again_after_probe() {
        let (mut server, mut client) = pair();
        assert!(!peer_is_gone(&probe_of(&server)));
        server
            .socket()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        // blocking に戻っていれば、データ無しの read はタイムアウトまで待機する。
        let t = Instant::now();
        let mut b = [0u8; 1];
        let err = server.read(&mut b).expect_err("no data");
        assert!(matches!(
            err.kind(),
            ErrorKind::WouldBlock | ErrorKind::TimedOut
        ));
        assert!(t.elapsed() >= Duration::from_millis(80), "read must block");
        client.write_all(b"y").expect("write");
        server.read_exact(&mut b).expect("read after probe");
        assert_eq!(&b, b"y");
    }

    #[test]
    fn install_without_socket_is_none() {
        assert!(install(None).is_none());
    }
}
