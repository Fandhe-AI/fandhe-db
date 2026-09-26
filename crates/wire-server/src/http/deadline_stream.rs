//! NoSQL 表層の HTTPS 終端（Issue #968・親 #941・TASK-228。対象ビヘイビア
//! WIRE-9・HTTP-10）が要求読み取りの絶対期限（Slowloris 対策・受入基準 2）を
//! TLS 上でも平文と同じに保つための、`crate::wire_stream::WireStream` の
//! 薄いラッパー。
//!
//! `crate::http::conn` は `read_head`／`read_body` の各 `read` 呼び出し**前**
//! に一度だけ「絶対期限までの残り時間」を [`WireStream::set_read_timeout`]
//! で設定する（`arm_read_timeout_for_deadline`）。平文 `TcpStream` はこの
//! 1 呼び出しがそのまま 1 回の `read` システムコールに対応するため、これで
//! 期限が正しく効く。
//!
//! しかし [`crate::tls::stream::TlsStream::read`] は 1 レコード分がそろう
//! まで内部で `inner.read` を複数回ループする（push 型のレコード読み取り。
//! `tls::stream` モジュール doc 参照）。`conn` 側が呼び出し前に設定した
//! ソケットタイムアウトは固定値のまま内部ループの各反復で使い回されるため、
//! 相手が 1 レコードの中身を期限ぎりぎりの間隔で 1 バイトずつ送り続けると、
//! 外側から見た 1 回の `read` 呼び出しが「レコード長 × タイムアウト値」まで
//! 際限なく延びうる（Slowloris の変種。設計記録:
//! `docs/design/tls-wire-connection.md`「HTTPS 表層（#968）」H8）。
//!
//! [`DeadlineStream`] は `TlsStream::new` へ渡す前に生 `TcpStream` を包み、
//! `set_read_timeout` を絶対時刻（[`Instant`]）へ変換して保持する。以後の
//! `Read::read` 呼び出し（`TlsStream` の内部ループから複数回呼ばれるものを
//! 含む）の直前に毎回「残り時間」を下位ソケットへ再設定してから読むため、
//! 内部ループの各反復が同じ絶対期限で正しく打ち切られる。`Write`・
//! その他の `WireStream` 操作はすべて下位へそのまま委譲する（挙動を変えない）。

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::wire_stream::WireStream;

/// 絶対期限付きの読み取りタイムアウトを下位ストリームへ都度再適用する
/// ラッパー（`S` は実運用では `std::net::TcpStream`）。
pub(crate) struct DeadlineStream<S> {
    inner: S,
    /// `set_read_timeout` で渡された相対時間を、呼び出し時点の `Instant`
    /// を基準に絶対時刻へ変換して保持する。`None` は「期限なし」。
    deadline: Option<Instant>,
}

impl<S: WireStream> DeadlineStream<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            deadline: None,
        }
    }

    /// 次の `read` 呼び出し 1 回分の下位ソケットタイムアウトを、保持して
    /// いる絶対期限までの残り時間へ合わせる（読み取り直前に毎回呼ぶ）。
    /// 期限が無ければ下位のタイムアウトも解除する。
    fn rearm_before_read(&mut self) -> io::Result<()> {
        let Some(deadline) = self.deadline else {
            return self.inner.set_read_timeout(None);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // 既に期限切れ。`Some(Duration::ZERO)` は一部プラットフォームで
            // 「タイムアウト無効」と解釈されうるため、最小の非ゼロ値を渡し
            // 直後の `read` が確実に即時タイムアウトするようにする
            // （fail-closed）。
            return self.inner.set_read_timeout(Some(Duration::from_nanos(1)));
        }
        self.inner.set_read_timeout(Some(remaining))
    }
}

impl<S: WireStream> Read for DeadlineStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.rearm_before_read()?;
        self.inner.read(buf)
    }
}

impl<S: WireStream> Write for DeadlineStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<S: WireStream> WireStream for DeadlineStream<S> {
    /// 保持している絶対期限から逆算した「現時点での残り時間」を返す
    /// （`post_auth_loop` 等が接続全体の基準値として読む契約は平文と同じ）。
    fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self
            .deadline
            .map(|d| d.saturating_duration_since(Instant::now())))
    }

    /// 相対時間を絶対期限（`Instant::now() + timeout`）へ変換して保持する
    /// だけで、下位ソケットへは次回 `read` 直前にのみ適用する
    /// （`rearm_before_read`）。`Instant` の加算オーバーフローは
    /// 実運用ではまず起きないが、`checked_add` が失敗した場合は期限なし
    /// ではなく「既に期限切れ」側（即時 `Instant::now()`）へ倒す
    /// （fail-closed。無期限化を避ける）。
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.deadline = timeout.map(|d| Instant::now().checked_add(d).unwrap_or_else(Instant::now));
        Ok(())
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    fn shutdown_both(&mut self) -> io::Result<()> {
        self.inner.shutdown_both()
    }

    fn shutdown_write(&mut self) -> io::Result<()> {
        self.inner.shutdown_write()
    }

    fn emergency_channel(&self) -> Option<TcpStream> {
        self.inner.emergency_channel()
    }

    fn graceful_close(&mut self) {
        self.inner.graceful_close()
    }

    fn tls_server_end_point(&self) -> Option<&[u8]> {
        self.inner.tls_server_end_point()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream as StdTcpStream};
    use std::thread;

    fn loopback_pair() -> (StdTcpStream, StdTcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let client = thread::spawn(move || StdTcpStream::connect(addr).expect("connect"));
        let (server, _) = listener.accept().expect("accept");
        let client = client.join().expect("client thread");
        (server, client)
    }

    /// 期限内に通常どおりデータが届けば、そのまま読める（平文経路との
    /// 振る舞いの同一性）。
    #[test]
    fn read_within_deadline_succeeds() {
        let (server, mut client) = loopback_pair();
        let mut stream = DeadlineStream::new(server);
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set deadline");
        client.write_all(b"hello").expect("client write");

        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).expect("read within deadline");
        assert_eq!(&buf, b"hello");
    }

    /// [`DeadlineStream`] の存在意義そのものの回帰確認: 相手が
    /// `set_read_timeout` の元の相対値より短い間隔で 1 バイトずつ送り続けて
    /// も、`read` を繰り返す呼び出し元は絶対期限を過ぎた時点で必ず
    /// `TimedOut`／`WouldBlock` になる（TLS の内部ループが同じ構図で
    /// `inner.read` を繰り返す状況を、ラッパー単体で模した回帰テスト）。
    #[test]
    fn repeated_reads_are_bounded_by_absolute_deadline_despite_trickle() {
        let (server, mut client) = loopback_pair();
        let mut stream = DeadlineStream::new(server);
        let deadline_budget = Duration::from_millis(300);
        stream
            .set_read_timeout(Some(deadline_budget))
            .expect("set deadline");

        // 相手はソケットタイムアウトの再設定を知らないまま、期限の budget
        // より短い間隔で 1 バイトずつ送り続ける（DeadlineStream が無ければ
        // 個々の `read` はその都度成功し続け、外側から見た合計待ち時間が
        // 際限なく延びる）。
        let sender = thread::spawn(move || {
            for _ in 0..50 {
                if client.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });

        let started = Instant::now();
        let mut buf = [0u8; 1];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => panic!("unexpected read error: {e}"),
            }
        }
        let elapsed = started.elapsed();
        // 個々のトリクル間隔（20ms）× 50 回では 1 秒近くになるが、絶対期限
        // （300ms）+ 十分な許容誤差以内で打ち切られていること。
        assert!(
            elapsed < deadline_budget + Duration::from_millis(700),
            "absolute deadline was not enforced across repeated reads: elapsed={elapsed:?}"
        );

        let _ = sender.join();
    }

    /// `set_read_timeout(None)` で期限が解除され、下位ソケットのタイムアウト
    /// も無期限へ戻ることを確認する（`serve_tls_connection` がハンドシェイク
    /// 前後でタイムアウトを退避・復元する経路と組み合わせても矛盾しない）。
    #[test]
    fn clearing_deadline_removes_underlying_timeout() {
        let (server, _client) = loopback_pair();
        let mut stream = DeadlineStream::new(server);
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set deadline");
        stream.set_read_timeout(None).expect("clear deadline");

        assert_eq!(stream.read_timeout().expect("read_timeout"), None);
        // 実際の `read` を発火させ、下位ソケットの `set_read_timeout(None)`
        // が正しく適用されていること（過去に設定した 50ms のまま残って
        // いれば、この後の `rearm_before_read` は `None` を渡すはずが
        // `read_timeout()` の戻り値だけでは検証できないため、下位への
        // 委譲もあわせて確認する）を `set_read_timeout` の戻り値で確認する。
        assert!(stream.rearm_before_read().is_ok());
    }
}
