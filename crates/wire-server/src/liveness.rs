//! 実行中の文のためのクライアント切断検出（Issue #1798。設計記録:
//! `docs/design/statement-interrupt.md`）。
//!
//! 役割: `handshake::post_auth_loop` が `'Q'`／`'E'` の処理に入るたびに、接続ソケットの複製を
//! peek する poll を engine の中断スコープ（`engine::interrupt`）へ積む。engine の走査ループが
//! 一定間隔で poll を呼び、切断を検出したら `57014` で走査を止める（切断後の空回りで接続スロット
//! と CPU を占有し続けないための資源保護）。受信データは消費せず、パイプライン送信された後続
//! メッセージには影響しない。
//!
//! PostgreSQL 17 との差: PG は既定（`client_connection_check_interval = 0`）では実行中の切断を
//! 検出しない。本実装は安全側で常に検出する。書き込み側だけ閉じた（半閉じ）クライアントも
//! 切断として扱う。

use std::io::ErrorKind;
use std::net::TcpStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use engine::interrupt::{InterruptGuard, InterruptReason, InterruptScope};

/// ソケット probe の最小間隔。engine 側の poll 頻度（`CHECK_STRIDE` 行ごと）が高くても、
/// システムコールはこの間隔に 1 回へ抑える。スコープ導入後の初回は必ず probe する。
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// ソケットが切断されたかを 1 回判定する。peek のため受信バッファは消費しない。
///
/// 非ブロッキング化は複製と元ソケットで共有されるため、必ず blocking へ戻す。戻せなかった場合は
/// 接続を閉じる側（切断扱い）へ倒す。呼び出しは接続スレッド上で、他の読み書きと並行しない。
pub(crate) fn peer_is_gone(sock: &TcpStream) -> bool {
    if sock.set_nonblocking(true).is_err() {
        return true;
    }
    let mut buf = [0u8; 1];
    let gone = match sock.peek(&mut buf) {
        // FIN 受信済み（相手が閉じた、または書き込み側を閉じた）。
        Ok(0) => true,
        // 未読バイトあり（パイプライン送信の後続）。生存と見なす。
        Ok(_) => false,
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => false,
        // ECONNRESET 等。
        Err(_) => true,
    };
    if sock.set_nonblocking(false).is_err() {
        return true;
    }
    gone
}

/// `sock` の切断検出を現在のスレッドの中断スコープとして導入する。`sock` が `None`
/// （検出手段のない接続）なら何も導入しない。返ったガードの drop で外れる。
pub(crate) fn install(sock: Option<&Rc<TcpStream>>) -> Option<InterruptGuard> {
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
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let c = TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
        let (s, _) = l.accept().expect("accept");
        (s, c)
    }

    #[test]
    fn idle_peer_is_alive() {
        let (server, _client) = pair();
        assert!(!peer_is_gone(&server));
    }

    #[test]
    fn closed_peer_is_gone() {
        let (server, client) = pair();
        drop(client);
        // FIN の到達を待つ。
        std::thread::sleep(Duration::from_millis(50));
        assert!(peer_is_gone(&server));
    }

    #[test]
    fn unread_bytes_are_alive_and_not_consumed() {
        let (mut server, mut client) = pair();
        client.write_all(b"x").expect("write");
        std::thread::sleep(Duration::from_millis(50));
        assert!(!peer_is_gone(&server));
        let mut b = [0u8; 1];
        server.read_exact(&mut b).expect("still readable");
        assert_eq!(&b, b"x");
    }

    #[test]
    fn socket_is_blocking_again_after_probe() {
        let (mut server, mut client) = pair();
        assert!(!peer_is_gone(&server));
        server
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
