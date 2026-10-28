//! wire-server の結合テスト（Issue #743・TASK-173／HTTP-11。対象ポインタ:
//! `docs/spec/05-tasks.md` TASK-69・WIRE-5, WIRE-6）。
//!
//! ephemeral port（`127.0.0.1:0`）で
//! `wire_server::http::listener::accept_loop_with_limiter` を起動し、
//! `std::net::TcpStream` で生バイトを送受信する自作クライアントを用いる
//! （`tests/wire_limits.rs` と同じ流儀の HTTP 表層版）。

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use wire_server::limits::{ConnectionLimiter, MAX_CONNECTIONS};

/// `accept_loop_with_limiter` をサーバースレッドで起動し、
/// `(接続先アドレス, リミッターのクローン)` を返す。呼び出し元はリミッターの
/// `active()` を観測して、拒否時に枠が消費されていないことを間接確認できる。
fn spawn_http_server(
    max_connections: usize,
    read_timeout: Duration,
) -> (std::net::SocketAddr, ConnectionLimiter) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);
    let limiter_for_loop = limiter.clone();

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_limiter(
            listener,
            limiter_for_loop,
            read_timeout,
        );
    });

    (addr, limiter)
}

/// `stream.read` が実際に EOF（`Ok(0)`）で終わったことを確認する。
///
/// `read(...).unwrap_or(0)` は、クライアント側の read が `WouldBlock`／
/// `TimedOut` で終わった場合も `Ok(0)`（EOF）と同一視してしまい、
/// 「サーバーの `read_timeout` 超過後に接続が閉じる」という検証対象の
/// 契約が破れていてもテストを通してしまう（codex-review 指摘）。
/// ここでは `Ok(0)` のみを合格とし、それ以外（`WouldBlock`／`TimedOut`
/// を含む）はテスト失敗として明示する。
fn assert_eof(stream: &mut TcpStream) {
    let mut buf = [0u8; 8];
    match stream.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("expected EOF without any response bytes, got {n} bytes"),
        Err(e) => panic!("expected EOF (Ok(0)), got read error: {e:?}"),
    }
}

fn wait_for_active_permits(limiter: &ConnectionLimiter, expected: usize, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let active = limiter.active();
        if active >= expected {
            assert_eq!(active, expected, "active permits must not exceed expected");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected} active permits, got {active}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// production 定数 `MAX_CONNECTIONS`（64）そのもので accept ループを通し、
/// 65 本目が HTTP 503／`wire_code` `53300` の JSON 応答で拒否されること
/// （`tests/wire_limits.rs::wire6_production_max_connections_rejects_the_65th_connection`
/// の HTTP 版）。`read_timeout` はテスト壁時間より十分長い値
/// （5 秒）を使う ―― 短縮値だと 65 本目の接続前に保持中の枠がタイムアウトで
/// 解放され flaky になる。
#[test]
fn http_production_max_connections_rejects_the_65th_connection_with_503() {
    let (addr, limiter) = spawn_http_server(MAX_CONNECTIONS, Duration::from_secs(5));

    let mut held: Vec<TcpStream> = Vec::with_capacity(MAX_CONNECTIONS);
    for _ in 0..MAX_CONNECTIONS {
        let stream = TcpStream::connect(addr).expect("connect within capacity");
        held.push(stream);
    }
    wait_for_active_permits(&limiter, MAX_CONNECTIONS, Duration::from_secs(5));

    let mut extra = TcpStream::connect(addr).expect("connect the 65th");
    extra
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut received = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match extra.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&received);
    assert!(
        text.starts_with("HTTP/1.1 503 "),
        "expected HTTP 503 for the 65th connection, got: {text:?}"
    );
    assert!(
        text.contains(wire_server::limits::SQLSTATE_TOO_MANY_CONNECTIONS),
        "response must carry SQLSTATE 53300, got: {text:?}"
    );

    assert!(
        limiter.active() <= MAX_CONNECTIONS,
        "active permits must never exceed MAX_CONNECTIONS even after a rejection"
    );

    drop(held);
}

/// 短縮 `read_timeout`（150ms）: タイムアウト前の probe は「まだ読み取り中」
/// であること（`WouldBlock` 相当。ここでは接続が受理されデータ到着待ちの
/// ままであることをクライアント側の短い read タイムアウトで確認する）、
/// タイムアウト後は応答なしで EOF になること。
#[test]
fn http_read_timeout_closes_connection_without_response() {
    let short_timeout = Duration::from_millis(150);
    let (addr, _limiter) = spawn_http_server(MAX_CONNECTIONS, short_timeout);

    let mut stream = TcpStream::connect(addr).expect("connect");
    // タイムアウト到達前に短い read を試み、まだ何も送られてこないこと
    // （サーバー側がまだ受理直後の待機状態であること）を確認する。
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("set short client read timeout");
    let mut probe = [0u8; 1];
    let probe_result = stream.read(&mut probe);
    assert!(
        matches!(
            probe_result,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut
        ),
        "expected no data before server-side read_timeout elapses, got: {probe_result:?}"
    );

    // サーバー側の read_timeout 超過後は応答なしで EOF になる。
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set client read timeout");
    assert_eof(&mut stream);
}

/// テストが渡した `ConnectionLimiter` のクローンで `active()` を観測できる
/// こと（ループが独自にインスタンスを作っていないことの証跡。SQL 表層側
/// との「同じ構築箇所・同じ型」という共有契約は `main.rs::run_server` の
/// 配線で満たされるため、本テストはリミッターの観測経路そのものを固定する）。
#[test]
fn http_accept_loop_reflects_shared_limiter_state() {
    let (addr, limiter) = spawn_http_server(2, Duration::from_secs(5));
    assert_eq!(limiter.active(), 0);

    let first = TcpStream::connect(addr).expect("connect first");
    wait_for_active_permits(&limiter, 1, Duration::from_secs(5));

    let second = TcpStream::connect(addr).expect("connect second");
    wait_for_active_permits(&limiter, 2, Duration::from_secs(5));

    drop(first);
    drop(second);
}

/// `tests/http1_surface_select.rs` の子プロセス経由 `--surface nosql` 起動が
/// 本 Issue の変更後も無変更のまま green であることの確認は、当該ファイル
/// 自体（別プロセスの結合テスト）が担う。本ファイルはライブラリ内 API を
/// 直接呼ぶ経路のみを対象とする。
#[test]
fn http_reject_response_does_not_consume_connection_permit() {
    let (addr, limiter) = spawn_http_server(1, Duration::from_secs(5));

    let holder = TcpStream::connect(addr).expect("connect holder");
    wait_for_active_permits(&limiter, 1, Duration::from_secs(5));

    let mut rejected = TcpStream::connect(addr).expect("connect rejected");
    rejected
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut received = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match rejected.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&received);
    assert!(text.starts_with("HTTP/1.1 503 "), "got: {text:?}");

    assert_eq!(
        limiter.active(),
        1,
        "reject path must not consume the shared connection permit"
    );

    drop(holder);
}
