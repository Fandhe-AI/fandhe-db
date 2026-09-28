//! wire-server の結合テスト（TASK-69・対象ビヘイビア WIRE-5, WIRE-6）。
//!
//! ephemeral port（`127.0.0.1:0`）で `wire_server::server::accept_loop_with_limiter` を起動し、
//! `std::net::TcpStream` で生バイトを送受信する自作クライアントを用いる
//! （`tests/wire_auth.rs` と同じ流儀。結合テスト間でモジュールを共有しないため
//! ヘルパーはこのファイル内に閉じる）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::{argon2id, UserStore};
use wire_server::limits::{ConnectionLimiter, MAX_CONNECTIONS};

/// `UserStore::load_from_file` は Argon2id パラメータが `RECOMMENDED_PARAMS` と
/// 完全一致するレコードのみを受理するため、フィクスチャも本番既定値を使う
/// （`tests/wire_auth.rs` と同じ理由）。
const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

/// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
/// プロセス内単調カウンタ（`wire_auth.rs` と同一クラスの競合対策。Issue #172）。
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_user_store_file(records: &[(&str, &str, &str)]) -> std::path::PathBuf {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-limits-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
        seq
    ));
    // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる
    // （Issue #172）。
    std::fs::create_dir(&dir).expect("create unique fixture dir");
    let path = dir.join("users.txt");

    let mut content = String::new();
    for (username, tenant_id, password) in records {
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(password.as_bytes(), salt, &TEST_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{username}:{tenant_id}:{phc}\n"));
    }
    std::fs::write(&path, content).expect("write user store fixture");
    path
}

/// `wire_server::server::accept_loop_with_limiter` をサーバースレッドで起動し、
/// `(接続先アドレス, リミッターのクローン)` を返す。呼び出し元はリミッターの
/// `active()` を観測して、拒否時に枠が消費されていないことを間接確認できる。
fn spawn_server(
    users_path: &std::path::Path,
    max_connections: usize,
    read_timeout: Duration,
) -> (std::net::SocketAddr, ConnectionLimiter) {
    let store = Arc::new(UserStore::load_from_file(users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);
    let limiter_for_loop = limiter.clone();

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_limiter(
            listener,
            store,
            limiter_for_loop,
            read_timeout,
        );
    });

    (addr, limiter)
}

/// SSLRequest → 'N' → StartupMessage(protocol 3.0, user=...) を送るクライアント側の
/// 共通シーケンス（`tests/wire_auth.rs` と同じ内容）。
fn send_ssl_request_and_startup(stream: &mut TcpStream, username: &str, database: &str) {
    let mut ssl_req = Vec::new();
    ssl_req.extend_from_slice(&8i32.to_be_bytes());
    ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl_req).expect("send SSLRequest");

    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(
        &resp, b"N",
        "server must decline SSL (WIRE-1: cleartext only)"
    );

    let mut params = Vec::new();
    params.extend_from_slice(b"user\0");
    params.extend_from_slice(username.as_bytes());
    params.push(0);
    params.extend_from_slice(b"database\0");
    params.extend_from_slice(database.as_bytes());
    params.push(0);
    params.push(0);

    let total_len = (4 + 4 + params.len()) as i32;
    let mut startup = Vec::new();
    startup.extend_from_slice(&total_len.to_be_bytes());
    startup.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    startup.extend_from_slice(&params);
    stream.write_all(&startup).expect("send StartupMessage");
}

fn read_auth_request_type(stream: &mut TcpStream) -> i32 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    assert_eq!(header[0], b'R', "expected Authentication* message");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf);
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read auth code");
    assert_eq!(len, 8);
    i32::from_be_bytes(code_buf)
}

fn send_password_message(stream: &mut TcpStream, password: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(password.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send PasswordMessage");
}

fn read_message_type_discarding_body(stream: &mut TcpStream) -> u8 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    header[0]
}

/// 認証完了まで進め、`ReadyForQuery` に到達させる（WIRE-6 の「認証済みでも枠は
/// 占有し続ける」ことの検証で使う）。
fn complete_authentication(stream: &mut TcpStream, username: &str, password: &str) {
    send_ssl_request_and_startup(stream, username, "db");
    let _ = read_auth_request_type(stream);
    send_password_message(stream, password);

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read auth ok type");
    assert_eq!(header[0], b'R', "expected AuthenticationOk");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read code");
    assert_eq!(i32::from_be_bytes(code_buf), 0, "AuthenticationOk expected");

    let mut msg_type = read_message_type_discarding_body(stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(stream);
    }
}

/// 接続が受理されて待機している（即座に EOF ではない）ことを、短いプローブ猶予
/// 内の `WouldBlock` で確認する。
fn assert_connection_accepted_and_idle(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set client probe timeout");
    let mut buf = [0u8; 1];
    let err = stream.read(&mut buf).expect_err("no data sent yet");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::WouldBlock,
        "connection must be accepted and left open (not immediately closed)"
    );
}

// ---------------------------------------------------------------------------
// WIRE-5: 読み取りタイムアウト
// ---------------------------------------------------------------------------

/// 接続後何も送らずアイドルすると、応答なし（`'E'` 等のバイトが一切ない）で
/// EOF になること。
#[test]
fn wire5_pre_auth_idle_is_closed_without_response() {
    let read_timeout = Duration::from_millis(300);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server(&users_path, 4, read_timeout);

    let mut stream = TcpStream::connect(addr).expect("connect");
    std::thread::sleep(read_timeout * 3);

    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "idle connection must be closed by the read timeout without any response bytes"
    );
}

/// StartupMessage の length フィールドだけ送って停止した部分フレームも、
/// 応答なしで EOF になること（WIRE-1 の SSL ネゴシエーション後の部分フレーム）。
#[test]
fn wire5_partial_frame_is_closed_without_response() {
    let read_timeout = Duration::from_millis(300);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server(&users_path, 4, read_timeout);

    let mut stream = TcpStream::connect(addr).expect("connect");

    let mut ssl_req = Vec::new();
    ssl_req.extend_from_slice(&8i32.to_be_bytes());
    ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl_req).expect("send SSLRequest");
    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(&resp, b"N");

    // StartupMessage の length(4 バイト)だけ送って停止する（body を送らない）。
    let total_len = 4i32 + 4 + 16; // 適当な妥当長。中身は送らない。
    stream
        .write_all(&total_len.to_be_bytes())
        .expect("send partial length prefix only");

    std::thread::sleep(read_timeout * 3);
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "partial frame must be closed by the read timeout without any response bytes"
    );
}

/// 認証完了後にアイドルしても応答なしで EOF になり、接続枠が解放されて後続接続を
/// 受理できること（有効な資格情報を持つクライアントが接続を張ったまま何も送らない
/// ことで枠を永久占有できないことの回帰確認）。
#[test]
fn wire5_post_auth_idle_is_closed_without_response_and_permit_released() {
    let read_timeout = Duration::from_millis(300);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server(&users_path, 1, read_timeout);

    let mut stream = TcpStream::connect(addr).expect("connect");
    complete_authentication(&mut stream, "alice", "correct-horse");

    std::thread::sleep(read_timeout * 3);
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "authenticated session must be closed by the read timeout without any response"
    );

    // 枠の解放をポーリングで観測してから、後続接続が受理されることを
    // 確認する（Issue #1083。ソケットのクローズより permit の drop が
    // 遅れうるため、固定 `sleep` の待ち時間より遅延すると誤検知しうる）。
    wait_for_active_exact(&limiter, 0, 1, Duration::from_secs(60));

    let mut next = TcpStream::connect(addr).expect("connect after permit release");
    assert_connection_accepted_and_idle(&mut next);
}

// ---------------------------------------------------------------------------
// WIRE-6: 共有接続数リミッター
// ---------------------------------------------------------------------------

/// max=1 で 1 本保持している間、2 本目は `'E'` / `53300` を受けて EOF になること。
/// 保持中の 1 本目は影響を受けず認証を完了でき、1 本目を閉じた後の 3 本目は
/// 受理されること。
#[test]
fn wire6_over_limit_is_rejected_with_53300_in_accept_loop() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server(&users_path, 1, LIMIT_TEST_IDLE_TIMEOUT);

    let mut held = TcpStream::connect(addr).expect("connect first (holds the only slot)");
    wait_for_active_exact(&limiter, 1, 1, Duration::from_secs(60));

    let mut second = TcpStream::connect(addr).expect("connect second");
    read_rejection_blocking(&mut second, Duration::from_secs(60));

    // 保持中の 1 本目は影響を受けず認証を完了できる。
    complete_authentication(&mut held, "alice", "correct-horse");

    drop(held);
    wait_for_active_exact(&limiter, 0, 1, Duration::from_secs(60));

    let mut third = TcpStream::connect(addr).expect("connect third after release");
    wait_for_active_exact(&limiter, 1, 1, Duration::from_secs(60));
    assert_connection_accepted_and_idle(&mut third);
}

/// 認証済み接続が枠を占有していても、未認証の 2 本目は同じく `53300` で拒否される
/// こと（枠の占有は認証状態を問わない）。
#[test]
fn wire6_limit_applies_to_authenticated_and_unauthenticated_alike() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server(&users_path, 1, LIMIT_TEST_IDLE_TIMEOUT);

    let mut authenticated = TcpStream::connect(addr).expect("connect first");
    complete_authentication(&mut authenticated, "alice", "correct-horse");

    let mut second = TcpStream::connect(addr).expect("connect second");
    let mut header = [0u8; 1];
    second.read_exact(&mut header).expect("read message type");
    assert_eq!(
        header[0], b'E',
        "unauthenticated connection must be rejected even though the occupying slot is authenticated"
    );
}

/// max=4 で 16 クライアントを同時 connect すると、受理 4・`53300` 拒否 12 を
/// ちょうど観測し、`limiter.active()` が 4 を超えないこと。
///
/// 分類はクライアントの短い read timeout による「受理か拒否か」の推測
/// ではなく、サーバー側の決定そのもの（`limiter.active()` の遷移と
/// `'E'` バイトの到達）をポーリングで観測して行う（Issue #1083）。
/// 旧実装はクライアント 200ms read timeout の `WouldBlock` を「受理」と
/// 見なしていたが、accept ループの応答書き込みが負荷下で 200ms を超えて
/// 遅延すると、本来拒否される接続が `WouldBlock` を返し誤って「受理」に
/// 分類され得た（accept ループは単一スレッドで listener を順に処理する
/// ため、`active` の増加はその接続が実際に枠を取得したことのみを意味する。
/// idle timeout を [`LIMIT_TEST_IDLE_TIMEOUT`] へ延長し、保持中の接続が
/// テスト実行中に idle 期限で閉じて枠が空く経路も塞ぐ）。
#[test]
fn wire6_concurrent_burst_never_exceeds_max() {
    const MAX: usize = 4;
    const BURST: usize = 16;
    let hang_guard = Duration::from_secs(60);
    let poll_interval = Duration::from_millis(5);

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server(&users_path, MAX, LIMIT_TEST_IDLE_TIMEOUT);

    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut held: Vec<TcpStream> = Vec::new();

    for _ in 0..BURST {
        let mut stream = TcpStream::connect(addr).expect("connect burst client");
        stream
            .set_read_timeout(Some(poll_interval))
            .expect("set probe timeout");

        let deadline = std::time::Instant::now() + hang_guard;
        loop {
            let mut header = [0u8; 1];
            match stream.read(&mut header) {
                // 'E' が読めた: 上限超過で拒否された。
                Ok(1) if header[0] == b'E' => {
                    rejected += 1;
                    break;
                }
                // この接続を数える前に active が 1 つ増えていれば、それは
                // この接続自身が枠を取得したことを意味する（accept ループ
                // は単一スレッドで順に処理するため、他の接続の増分と
                // 取り違えない）。
                Ok(1) => panic!("unexpected non-'E' byte from server: {:?}", header[0]),
                Ok(0) => panic!(
                    "connection closed with EOF before an ErrorResponse byte was read; \
                     this would misclassify a rejection as neither accepted nor rejected"
                ),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if limiter.active() == accepted + 1 {
                        accepted += 1;
                        held.push(stream);
                        break;
                    }
                }
                Err(e) => panic!("unexpected read error: {e:?}"),
                #[allow(unreachable_patterns)]
                Ok(_) => unreachable!("read into a 1-byte buffer cannot return more than 1"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out classifying burst connection as accepted or rejected"
            );
        }
    }

    assert_eq!(accepted, MAX, "exactly max connections must be accepted");
    assert_eq!(
        rejected,
        BURST - MAX,
        "the remaining connections must be rejected with 53300"
    );
    assert!(
        limiter.active() <= MAX,
        "active permits must never exceed max"
    );
}

/// `limiter.active()` が `expected` にちょうど一致するまでポーリングする
/// （Issue #1083。旧 `wait_for_active_permits` は `>=` による早期判定のため、
/// 解放による減少（0 への遷移）を待つ用途に使えなかった）。
///
/// accept ループは単一スレッドで listener を順に処理するため、`connect()`
/// が返った時点では OS の backlog に滞留していて許可枠がまだ付与・解放
/// されていないことがある。固定時間の `sleep` で待つ方式は、負荷下では
/// 実際の遷移より早く／遅く終わり、`READ_TIMEOUT`／アイドル期限に対して
/// 無視できない割合を占めうる（codex-review P2・Cursor Bugbot Medium
/// 指摘、Issue #1083 継続）。ここでは短い間隔（2ms）で条件を再確認する
/// ポーリングに置き換え、各回 `active <= max` の不変条件も検証しながら
/// 許可枠が実際に `expected` へ遷移した事実そのもので合否を判定する
/// （読み取りタイムアウトの経過待ちに依存しない）。`hang_guard` は
/// ハング防止のためだけの上限で、合否の基準ではない。
fn wait_for_active_exact(
    limiter: &ConnectionLimiter,
    expected: usize,
    max: usize,
    hang_guard: Duration,
) {
    let deadline = std::time::Instant::now() + hang_guard;
    loop {
        let active = limiter.active();
        assert!(
            active <= max,
            "active permits must never exceed max, got {active} (max {max})"
        );
        if active == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected} active permits, got {active}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// 上限超過で拒否された接続から `'E'`／`53300` を読み取り、続けて EOF になる
/// ことを確認する（`wire6_over_limit_is_rejected_with_53300_in_accept_loop`・
/// `wire6_production_max_connections_rejects_the_65th_connection` に重複して
/// いた読み取りコードの共通化。Issue #1083）。クライアントの read timeout を
/// `hang_guard` 相当まで長く取り、負荷下で accept ループの応答書き込みが
/// 遅延してもタイムアウトで誤判定しないようにする。
fn read_rejection_blocking(stream: &mut TcpStream, hang_guard: Duration) {
    stream
        .set_read_timeout(Some(hang_guard))
        .expect("set client read timeout for rejection read");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    assert_eq!(header[0], b'E', "expected ErrorResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains(wire_server::limits::SQLSTATE_TOO_MANY_CONNECTIONS),
        "ErrorResponse must carry SQLSTATE 53300, got: {body_str:?}"
    );

    let mut trailing = [0u8; 1];
    let n = stream.read(&mut trailing).unwrap_or(0);
    assert_eq!(n, 0, "rejected connection must be closed after rejection");
}

/// WIRE-6 テスト専用の accept ループ idle timeout（Issue #1083）。負荷下では
/// production の `READ_TIMEOUT`（30 秒）や旧テスト値（5 秒）でも、先頭に
/// 保持している接続がテスト実行中に idle timeout で閉じられ枠が空いてしまう
/// レースがあった。WIRE-6 は「枠の占有・拒否」を検証する観点であり
/// `READ_TIMEOUT` そのもの（WIRE-5）の検証対象ではないため、このテスト用
/// サーバーに限って大きく延長する。production の `crate::limits::READ_TIMEOUT`
/// は変更しない。
const LIMIT_TEST_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// production 定数 `MAX_CONNECTIONS`（64）そのもので accept ループを通し、
/// 65 本目が `'E'`／`53300` で拒否されること（Issue #482。既存の
/// `wire6_concurrent_burst_never_exceeds_max` はパラメータ化した小さい上限
/// （4）でのみ検証しており、production 定数そのものでの回帰は本テストが
/// 初めて固定する）。
///
/// 64 本の接続受理確認は、接続 1 本ごとに固定待ち（50ms）を課す方式では
/// なく `wait_for_active_exact` によるポーリングで行う（codex-review
/// P2・Cursor Bugbot Medium 指摘対応: 逐次固定待ちは累積で数秒かかり、
/// 共有環境の遅延が重なるとアイドル期限に対し先頭接続から閉じられ得る
/// ため）。idle timeout も [`LIMIT_TEST_IDLE_TIMEOUT`] へ延長する
/// （Issue #1083。旧値 5 秒は待機上限 5 秒と同じ長さで、負荷下では先頭の
/// 保持接続が idle timeout で閉じ、64 本受理を待つ間に枠が空いて 65 本目
/// が受理されてしまい得た）。
#[test]
fn wire6_production_max_connections_rejects_the_65th_connection() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server(&users_path, MAX_CONNECTIONS, LIMIT_TEST_IDLE_TIMEOUT);

    let mut held: Vec<TcpStream> = Vec::with_capacity(MAX_CONNECTIONS);
    for _ in 0..MAX_CONNECTIONS {
        let stream = TcpStream::connect(addr).expect("connect within capacity");
        held.push(stream);
    }
    // 許可枠が実際に `MAX_CONNECTIONS` 本付与された事実そのものが受理の
    // 証拠であり（枠を超えて許可されることはない）、各接続を個別に
    // アイドル状態か読み取りタイムアウトで確認する必要はない。
    wait_for_active_exact(
        &limiter,
        MAX_CONNECTIONS,
        MAX_CONNECTIONS,
        Duration::from_secs(60),
    );

    let mut extra = TcpStream::connect(addr).expect("connect the 65th");
    read_rejection_blocking(&mut extra, Duration::from_secs(60));

    assert!(
        limiter.active() <= MAX_CONNECTIONS,
        "active permits must never exceed MAX_CONNECTIONS even after a rejection"
    );

    drop(held);
}
