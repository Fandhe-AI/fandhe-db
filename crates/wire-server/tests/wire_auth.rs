//! wire-server の結合テスト（TASK-67・対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）。
//!
//! ephemeral port（`127.0.0.1:0`）でサーバースレッドを起動し、`std::net::TcpStream`
//! で生バイトを送受信する自作クライアントを用いる。実 `psql`／`psycopg`／`pg`
//! による無改造クライアント検証は `tests/three_client_e2e.rs`（`#[ignore]`。
//! TASK-73・WIRE-1）が別途担う。

#[path = "common/fixture_guard.rs"]
mod fixture_guard;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wire_server::auth::{argon2id, UserStore};

/// `UserStore::load_from_file` は P0 review 是正により Argon2id パラメータが
/// `argon2id::RECOMMENDED_PARAMS` と完全一致するレコードのみを受理する（既知・
/// 未知ユーザーの KDF コストを常に一致させ、タイミング側チャネルで存在情報が
/// 漏れないようにするため）。したがって結合テストの user store フィクスチャも
/// 軽量パラメータではなく本番既定値をそのまま使う必要がある。
const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

/// フィクスチャ用一時ディレクトリ名の一意性を pid・時刻の組だけに委ねないための
/// プロセス内単調カウンタ（Issue #172）。同一テストバイナリ内の複数テストは
/// libtest により同一 pid で並行実行されるため、クロック分解能が粗い環境
/// （VM 等）では pid+nanos だけでは同時刻ヒットが構造的にあり得た。`Relaxed` で
/// 十分な理由: カウンタの値そのものに他スレッドとの happens-before 関係は不要で、
/// 「同一プロセス内で値が重複して払い出されない」ことだけが要件のため。
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_user_store_file(records: &[(&str, &str, &str)]) -> fixture_guard::UserStoreFile {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-auth-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
        seq
    ));
    // `create_dir_all` は既存ディレクトリを黙って再利用してしまい、名前が
    // 万一衝突した場合に他テストの truncate 直後の空ファイルを読みうる
    // （Issue #172 の H1 仮説）。カウンタ付与で衝突を構造的に不可能にした上で、
    // `create_dir`（既存なら `Err`）にして衝突が起きたら panic で顕在化させる。
    std::fs::create_dir(&dir).expect("create unique fixture dir");
    let path = dir.join("users.txt");

    let mut content = String::new();
    for (username, tenant_id, password) in records {
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(password.as_bytes(), salt, &TEST_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{username}:{tenant_id}:{phc}\n"));
    }
    std::fs::write(&path, &content).expect("write user store fixture");
    // 書き込み直後に読み戻して内容一致を確認する。他プロセス・他テストによる
    // 割り込みがあれば、ここで即座に検出できる（Issue #172: フィクスチャ破損の
    // 早期発見）。
    let readback = std::fs::read_to_string(&path).expect("read back user store fixture");
    assert_eq!(
        readback, content,
        "fixture file content must match what was just written (possible fixture race)"
    );
    fixture_guard::UserStoreFile::new(dir, path)
}

/// サーバースレッドを起動し、`(接続先アドレス, 停止用ハンドル)` を返す。
/// 1 接続だけ受理してスレッドを終了する（テストのシーケンス制御を単純にするため）。
fn spawn_server_accepting_one(users_path: &std::path::Path) -> std::net::SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    // フィクスチャが空ファイルとして読まれた場合（Issue #172 の H1 仮説: 一時
    // ディレクトリ名の衝突により他テストの truncate 中身を読んでしまう競合）、
    // 空ストアは「常に 28P01」という誤テスト失敗を招く。ここで即座に検出し
    // 原因を切り分け可能にする（`write_user_store_file` は空ストアを意図して
    // 呼ばれることがないため、空は常に fixture race を示す）。
    assert!(
        !store.is_empty(),
        "fixture race: user store loaded from {users_path:?} is empty; \
         this indicates the fixture temp dir name collided with another test \
         (Issue #172 H1)"
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = wire_server::handshake::handle_connection_bounded(stream, &store);
        }
    });

    addr
}

/// 失敗時診断ヘルパー（Issue #172）: `wire1`/`wire2` で期待外の応答（`R` 以外）を
/// 受け取った際、追加の再現実験なしに H1〜H3 のどれに該当するかを 1 回のログで
/// 切り分けられるようにする。
///
/// 出力する情報:
/// - 受信したメッセージ型・SQLSTATE を含む本文（フレーミング齟齬 H4 か認証不一致か）
/// - フィクスチャファイルの再読込結果（バイト長・`alice:` 行の有無。H1: フィクスチャ
///   衝突で空/破損ファイルを読んでいないか）
/// - フィクスチャの PHC に対する `verify_phc` の呼び出しテストスレッドでの再計算
///   （`Ok(true)` ならサーバー側の一過性事象 H2/H3、`Ok(false)`/`Err` ならフィクスチャ
///   /KDF 側 H1 寄り）
///
/// パニックメッセージにのみ出力し、平文パスワード・PHC 全体は含めない
/// （診断はテスト実行者のみが見るログだが、コード規約上の一般原則として抑制する）。
fn diagnose_unexpected_auth_response(
    users_path: &std::path::Path,
    type_byte: u8,
    body: &str,
) -> String {
    let refetch = std::fs::read_to_string(users_path);
    let fixture_summary = match &refetch {
        Ok(s) => format!(
            "len={} bytes, has_alice_line={}",
            s.len(),
            s.lines().any(|l| l.starts_with("alice:"))
        ),
        Err(e) => format!("<unreadable: {e:?}>"),
    };

    let recompute_summary = match &refetch {
        Ok(s) => match s.lines().find(|l| l.starts_with("alice:")) {
            Some(line) => match line.splitn(3, ':').nth(2) {
                Some(phc) => match argon2id::verify_phc(phc, b"correct-horse") {
                    Ok(true) => {
                        "Ok(true) -- KDF matches; likely a transient server-side event (H2/H3)"
                            .to_string()
                    }
                    Ok(false) => {
                        "Ok(false) -- KDF does not match fixture password (H1 fixture mismatch)"
                            .to_string()
                    }
                    Err(e) => format!("Err({e:?}) -- KDF execution error (H2)"),
                },
                None => "<no phc field on alice line>".to_string(),
            },
            None => "<no alice line in fixture; H1 fixture race>".to_string(),
        },
        Err(_) => "<fixture unreadable, skipped>".to_string(),
    };

    format!(
        "authentication must succeed regardless of the self-reported database parameter; \
         got message type {:?}, body: {body:?}; \
         fixture reread: {fixture_summary}; \
         testthread KDF recompute: {recompute_summary}",
        type_byte as char
    )
}

// `wire_server::server::accept_loop_with_limiter` 経由（同時接続数上限・読み取りタイムアウトを
// 実際に適用した状態でのハンドシェイク検証）は WIRE-5/WIRE-6 固有の関心事のため
// `tests/wire_limits.rs` へ集約した（D1: 本ファイルは認証フロー自体の検証に専念する）。

/// SSLRequest → 'N' → StartupMessage(protocol 3.0, user=...) を送るクライアント側の
/// 共通シーケンス。
fn send_ssl_request_and_startup(stream: &mut TcpStream, username: &str, database: &str) {
    // SSLRequest: length=8, code=80877103
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

    // StartupMessage: length + protocol(3.0) + "user\0<username>\0" +
    // "database\0<database>\0" + terminator 0x00
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
    // AuthenticationCleartextPassword はペイロードがコードのみ（length=8）。
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

/// 次のメッセージの型バイトを返す（本文は破棄）。`ReadyForQuery` 到達確認・
/// `ErrorResponse` 検出の両方に使う。
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

/// ポインタ: TASK-67・WIRE-1。正常系の認証シーケンスが `ReadyForQuery` まで
/// 到達すること。
#[test]
fn wire1_successful_cleartext_auth_reaches_ready_for_query() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db-name");
    let auth_code = read_auth_request_type(&mut stream);
    assert_eq!(
        auth_code, 3,
        "AuthenticationCleartextPassword code must be 3"
    );

    send_password_message(&mut stream, "correct-horse");

    // AuthenticationOk ('R', code=0)
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read auth ok type");
    if header[0] != b'R' {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        stream.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        panic!(
            "{}",
            diagnose_unexpected_auth_response(&users_path, header[0], &body_str)
        );
    }
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read code");
    assert_eq!(
        i32::from_be_bytes(code_buf),
        0,
        "AuthenticationOk code must be 0"
    );

    // 後続メッセージ（BackendKeyData・ParameterStatus*）を読み飛ばし、
    // 最終的に ReadyForQuery('Z') へ到達することを確認する。
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(&mut stream);
    }
}

/// ポインタ: TASK-67・WIRE-3。誤パスワードの応答・接続クローズ・下限遅延を確認する
/// （上限はフレーク回避のため検証しない）。
#[test]
fn wire3_wrong_password_returns_28p01_without_ready_for_query() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_auth_request_type(&mut stream);

    let start = Instant::now();
    send_password_message(&mut stream, "wrong-password");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    let elapsed = start.elapsed();
    assert_eq!(header[0], b'E', "expected ErrorResponse on wrong password");
    assert!(
        elapsed >= Duration::from_millis(200),
        "auth failure must incur the fixed delay (WIRE-3)"
    );

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("28P01"),
        "ErrorResponse must carry SQLSTATE 28P01, got: {body_str:?}"
    );

    // 接続は切断される（ReadyForQuery を送らない）: 追加の読み取りは EOF になる。
    let mut extra = [0u8; 1];
    let n = stream.read(&mut extra).unwrap_or(0);
    assert_eq!(
        n, 0,
        "connection must be closed after auth failure, not kept open"
    );
}

/// Cursor Bugbot 指摘の再発防止（Issue #940 PR #1006）: cleartext フローの
/// 不正な PasswordMessage（終端 NUL 無し）は、cleartext/SCRAM 共通の
/// `authenticate()` へ統合される前と同じ `"invalid password message"`
/// （`HandshakeError::Protocol` の fallback_message）を返し続けること。
/// 統合直後は `respond_and_close` の呼び出し元が認証方式を区別しない単一の
/// `"invalid message frame"` を渡していたため、既定 cleartext 経路の
/// ErrorResponse がビット同一でなくなっていた。
#[test]
fn wire3_malformed_password_message_returns_original_fallback_message() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_auth_request_type(&mut stream);

    // 終端 NUL の無い不正な PasswordMessage（`read_password_message` が
    // `HandshakeError::Protocol("password message not null-terminated")` を
    // 返す形状）。
    let body = b"not-null-terminated";
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(body);
    stream
        .write_all(&msg)
        .expect("send malformed PasswordMessage");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    assert_eq!(
        header[0], b'E',
        "expected ErrorResponse on malformed PasswordMessage"
    );

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut resp_body = vec![0u8; len - 4];
    stream.read_exact(&mut resp_body).expect("read body");
    let body_str = String::from_utf8_lossy(&resp_body);
    assert!(
        body_str.contains("invalid password message"),
        "cleartext malformed PasswordMessage must keep the pre-SCRAM fallback \
         message, got: {body_str:?}"
    );
}

/// ポインタ: TASK-67・WIRE-3。未知ユーザーが既知ユーザーの誤パスワードと外形上
/// 区別できないこと（列挙対策）。
#[test]
fn wire3_unknown_user_returns_same_error_and_delay() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "no-such-user", "db");
    let _ = read_auth_request_type(&mut stream);

    let start = Instant::now();
    send_password_message(&mut stream, "anything");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    let elapsed = start.elapsed();
    assert_eq!(header[0], b'E');
    assert!(elapsed >= Duration::from_millis(200));
}

/// ポインタ: TASK-67・WIRE-2。クライアント自己申告値が認証結果に影響しないことを
/// 外形的に確認する（テナント導出値そのものの検証は `auth::verify` の単体テストで
/// 行う）。
#[test]
fn wire2_database_param_does_not_affect_authentication_outcome() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "tenant-b-spoofed");
    let _ = read_auth_request_type(&mut stream);
    send_password_message(&mut stream, "correct-horse");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    if header[0] != b'R' {
        // 失敗時は SQLSTATE を含めて診断する（`28P01` なら認証そのものの不一致、
        // `08P01` ならテストクライアント側のフレーミング齟齬で、原因が全く異なる。
        // 後続の flake 調査を「もう一度落として見る」から「1 回のログで切り分ける」
        // に変えるための最小限の追加読み取り）。
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        stream.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        panic!(
            "{}",
            diagnose_unexpected_auth_response(&users_path, header[0], &body_str)
        );
    }
}

/// フィクスチャ一時ディレクトリ名が連続生成でも重複しないことの軽量回帰確認
/// （Issue #172: `FIXTURE_SEQ` カウンタ導入の直接検証）。
#[test]
fn write_user_store_file_generates_unique_fixture_dirs() {
    let path_a = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let path_b = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    assert_ne!(
        path_a.parent(),
        path_b.parent(),
        "consecutive fixture generations must not reuse the same temp dir"
    );
}

/// opt-in の負荷再現テスト（Issue #172）。CI の既定実行には含めない
/// （`#[ignore]`）。高負荷下でのみ非再現だったフレークを、多スレッド×多反復の
/// 完全な認証シーケンスで再現しやすくする。規模は環境変数で調整できる:
/// `WIRE_AUTH_STRESS_THREADS`（既定 4）・`WIRE_AUTH_STRESS_ITERATIONS`（既定 10）。
///
/// debug ビルドでは Argon2id が遅く既定パラメータでは重いため、`--release` 推奨:
/// `cargo test --release -p fandhe-db-wire-server --test wire_auth -- --ignored --nocapture`
#[test]
#[ignore = "opt-in stress harness for Issue #172; run explicitly with --release --ignored"]
fn wire_auth_stress_parallel_successful_auth() {
    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    let threads = env_usize("WIRE_AUTH_STRESS_THREADS", 4);
    let iterations = env_usize("WIRE_AUTH_STRESS_ITERATIONS", 10);

    let handles: Vec<_> = (0..threads)
        .map(|_| {
            std::thread::spawn(move || {
                for _ in 0..iterations {
                    let users_path =
                        write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
                    let addr = spawn_server_accepting_one(&users_path);
                    let mut stream = TcpStream::connect(addr).expect("connect");

                    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db-name");
                    let auth_code = read_auth_request_type(&mut stream);
                    assert_eq!(auth_code, 3);

                    send_password_message(&mut stream, "correct-horse");

                    let mut header = [0u8; 1];
                    stream.read_exact(&mut header).expect("read auth response");
                    if header[0] != b'R' {
                        let mut len_buf = [0u8; 4];
                        stream.read_exact(&mut len_buf).expect("read len");
                        let len = i32::from_be_bytes(len_buf) as usize;
                        let mut body = vec![0u8; len.saturating_sub(4)];
                        stream.read_exact(&mut body).expect("read body");
                        let body_str = String::from_utf8_lossy(&body);
                        panic!(
                            "{}",
                            diagnose_unexpected_auth_response(&users_path, header[0], &body_str)
                        );
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("stress worker thread must not panic");
    }
}

// D1（TASK-69）: 認証前後で二段のタイムアウトを持つ前提のテスト
// （wire_pre_auth_timeout_still_applies_but_post_auth_session_survives_idle /
// wire_post_auth_session_is_closed_and_slot_released_after_idle_timeout）は、
// 読み取りタイムアウトを接続全体へ単一値で適用する契約（WIRE-5）への変更に伴い
// 前提が消えたため撤去した。置換先は `tests/wire_limits.rs` の
// `wire5_pre_auth_idle_is_closed_without_response` /
// `wire5_post_auth_idle_is_closed_without_response_and_permit_released`。
