//! 接続資源上限 3 種（`--max-connections`・`--read-timeout-secs`・
//! `--max-prepared-statements`）の境界・NoSQL 表層への適用・keepalive 非送出の結合テスト
//! （Issue #2455。ポインタ: WIRE-25。対象 WIRE-5・WIRE-6・WIRE-11。親 TASK-338）。
//!
//! #2454 のテスト（`connection_limits_opt` の単体テスト・`wire_connection_limits_cli.rs` の
//! 受理/拒否と配線スモーク）が「値が解決され渡ること」を見るのに対し、本ファイルは
//! 既定値以外の**設定値**で「上限ちょうどは受理・1 つ超えたら拒否」の境界が成り立つこと、
//! 拒否が枠を消費しないこと、NoSQL 本番経路（`accept_loop_with_router`）でも同じ上限が効く
//! ことを固定する。in-process（ライブラリ注入）と子プロセス（CLI 配線）の両方を使う。
//!
//! keepalive 非送出（WIRE-25 の方針）は、アイドル区間で 1 バイトも届かず設定値で EOF に
//! なる振る舞いテストと、`src/**/*.rs` に keepalive 設定の参照が無いことのソース検査で
//! 固定する。ソケットの `SO_KEEPALIVE` を直接読むには libc 等の依存追加か `unsafe` が要り、
//! いずれも承認制・原則禁止のため採らない。
//!
//! 時間依存の窓は 3 秒以内に収め、保持接続の枠が読み取りタイムアウトで解放されて
//! flaky にならないよう、枠を数える試験では読み取りタイムアウトを十分長く（5 秒）取る。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::http::router::Router;
use wire_server::http::session::store::SessionStore;
use wire_server::limits::{ConnectionLimiter, SessionLimits, SQLSTATE_TOO_MANY_CONNECTIONS};

use common::*;

const LONG_TIMEOUT: Duration = Duration::from_secs(5);

fn new_core_with_documents_table(label: &str) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

/// `accept_loop_with_router`（NoSQL 本番経路）を in-process で起動する。
/// 使い捨て core は open 直後に unlink して一時領域を残さない（Issue #1303）。
fn spawn_nosql_router_server(
    users_path: &std::path::Path,
    max_connections: usize,
    read_timeout: Duration,
) -> (SocketAddr, ConnectionLimiter) {
    let store = wire_server::auth::UserStore::load_from_file(users_path).expect("valid store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);
    let limiter_for_loop = limiter.clone();
    let core_path = temp_db::unique_db_path("wire25-nosql-throwaway");
    let core = EngineCore::open(&core_path).expect("open throwaway engine core");
    temp_db::unlink_open_db_file(&core_path);
    let router = Router::with_engine(Arc::new(store), SessionStore::new(), Arc::new(core));

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_router(
            listener,
            limiter_for_loop,
            read_timeout,
            router,
        );
    });

    (addr, limiter)
}

/// 拒否応答を EOF まで読み切る（5 秒でハング防止）。
fn read_until_close(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut received = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&received).into_owned()
}

/// タイムアウト前の probe: 何も届かず（`WouldBlock`／`TimedOut`）接続が開いたままであること。
/// `Ok(n)` は keepalive 等のバイトがアイドル中に送られた証拠として失敗にする。
fn assert_silent_and_open(stream: &mut TcpStream, probe: Duration) {
    stream
        .set_read_timeout(Some(probe))
        .expect("set probe timeout");
    let mut buf = [0u8; 16];
    match stream.read(&mut buf) {
        Err(ref e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        other => panic!("expected silent open connection during the idle window, got {other:?}"),
    }
}

/// `Ok(0)`（EOF）だけを合格にする厳密判定（`Ok(n>0)` は応答/keepalive バイトの混入）。
fn assert_eof(stream: &mut TcpStream, within: Duration) -> Duration {
    stream
        .set_read_timeout(Some(within))
        .expect("set read timeout");
    let started = Instant::now();
    let mut buf = [0u8; 16];
    match stream.read(&mut buf) {
        Ok(0) => started.elapsed(),
        Ok(n) => panic!("expected EOF without any bytes, got {n} bytes"),
        Err(e) => panic!("expected EOF (Ok(0)), got read error: {e:?}"),
    }
}

// ---------------------------------------------------------------------------
// 接続数の境界
// ---------------------------------------------------------------------------

/// B1: SQL 表層。上限 3 ちょうどは受理、4 本目は `53300`。拒否は枠を消費せず、
/// 保持中の 1 本を閉じると枠が設定値どおり回収されて新規接続が受理される。
#[test]
fn sql_max_connections_boundary_at_configured_value() {
    let (core, _guard) = new_core_with_documents_table("wire25-b1");
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server_with_engine_and_limits(
        &users_path,
        core,
        3,
        LONG_TIMEOUT,
        SessionLimits::default(),
    );

    let mut held: Vec<TcpStream> = (0..3)
        .map(|_| TcpStream::connect(addr).expect("connect within capacity"))
        .collect();
    wait_for_active_exact(&limiter, 3, Duration::from_secs(5));

    let mut extra = TcpStream::connect(addr).expect("connect the 4th");
    extra
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    expect_error_response_with_sqlstate(&mut extra, SQLSTATE_TOO_MANY_CONNECTIONS);
    assert!(limiter.active() <= 3, "rejection must not consume a permit");

    // 1 本閉じると枠が 1 つ戻り、認証まで通る接続が受理される。
    drop(held.pop());
    wait_for_active_exact(&limiter, 2, Duration::from_secs(5));
    let _accepted = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    wait_for_active_exact(&limiter, 3, Duration::from_secs(5));
}

/// B2: NoSQL 表層（本番 router 経路）。上限 3 ちょうどは受理、4 本目は HTTP 503 と `53300`。
#[test]
fn nosql_max_connections_boundary_at_configured_value() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let (addr, limiter) = spawn_nosql_router_server(&users_path, 3, LONG_TIMEOUT);

    let held: Vec<TcpStream> = (0..3)
        .map(|_| TcpStream::connect(addr).expect("connect within capacity"))
        .collect();
    wait_for_active_exact(&limiter, 3, Duration::from_secs(5));

    let mut extra = TcpStream::connect(addr).expect("connect the 4th");
    let text = read_until_close(&mut extra);
    assert!(
        text.starts_with("HTTP/1.1 503 "),
        "expected HTTP 503 for the 4th connection, got: {text:?}"
    );
    assert!(
        text.contains(SQLSTATE_TOO_MANY_CONNECTIONS),
        "response must carry SQLSTATE 53300, got: {text:?}"
    );
    assert_eq!(limiter.active(), 3, "rejection must not consume a permit");
    drop(held);
}

// ---------------------------------------------------------------------------
// 読み取りタイムアウト（と keepalive 非送出）
// ---------------------------------------------------------------------------

/// B3 + B7（NoSQL）: 設定値 300ms。受理直後は何も届かず、設定値経過後は応答なしで EOF。
#[test]
fn nosql_read_timeout_closes_idle_connection_at_configured_value_without_bytes() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let (addr, _limiter) = spawn_nosql_router_server(&users_path, 8, Duration::from_millis(300));

    let mut stream = TcpStream::connect(addr).expect("connect");
    assert_silent_and_open(&mut stream, Duration::from_millis(50));
    assert_eof(&mut stream, Duration::from_secs(3));
}

/// B6 + B7（SQL）: 認証後のアイドルが設定値 3 秒で応答なしに切断される。設定値より短い
/// 間隔で通信し続ける接続は、合計で設定値を超えても切断されない。
#[test]
fn sql_read_timeout_closes_idle_session_at_configured_value_and_spares_active_one() {
    let (core, _guard) = new_core_with_documents_table("wire25-b6");
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let (addr, _limiter) = spawn_server_with_engine_and_limits(
        &users_path,
        core,
        8,
        Duration::from_secs(3),
        SessionLimits::default(),
    );

    let mut idle = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut active = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    // 認証（Argon2）完了後に keep-alive を数え始めるため、idle の観測より前に active へ
    // 最初のクエリを送る。待機は設定値 3 秒の 1/3 以下（1 秒）の間隔で、余裕を 1.5 秒以上取る。
    let query_and_drain = |stream: &mut TcpStream| {
        send_simple_query(stream, "SELECT id FROM documents LIMIT 1");
        loop {
            let (kind, _) = read_message(stream);
            if kind == b'Z' {
                break;
            }
        }
    };
    query_and_drain(&mut active);
    assert_silent_and_open(&mut idle, Duration::from_millis(1500));

    // 1 秒間隔で合計 3 秒以上（設定値 3 秒に到達）通信を続けても切断されない。
    for _ in 0..4 {
        query_and_drain(&mut active);
        std::thread::sleep(Duration::from_secs(1));
    }
    query_and_drain(&mut active);

    // 無通信の接続は設定値を過ぎると 0 バイトのまま EOF になる（3 秒以内）。
    let elapsed = assert_eof(&mut idle, Duration::from_secs(3));
    assert!(
        elapsed < Duration::from_secs(3),
        "idle session must be closed at the configured value, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// prepared statement 件数の境界
// ---------------------------------------------------------------------------

fn parse_named(stream: &mut TcpStream, name: &str) {
    send_length_prefixed_message(
        stream,
        b'P',
        &parse_body(name, "SELECT id FROM documents LIMIT 1", 0),
    );
}

fn expect_parse_complete(stream: &mut TcpStream, what: &str) {
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "{what} should be accepted");
}

/// ErrorResponse(54000) → Sync → ReadyForQuery(I) の同期回復を確かめる。
fn expect_54000_then_recovers(stream: &mut TcpStream) {
    expect_error_response_with_sqlstate(stream, "54000");
    send_length_prefixed_message(stream, b'S', b"");
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");
    assert_eq!(body, [b'I']);
}

/// B4: 設定値 3。名前付き 3 件は受理、4 件目は `54000`。上限到達後も無名は受理され、
/// 名前付きを Close すると新しい名前の Parse が再び通る。
#[test]
fn prepared_statement_boundary_at_configured_value() {
    let (core, _guard) = new_core_with_documents_table("wire25-b4");
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server_with_engine_and_limits(
        &users_path,
        core,
        8,
        LONG_TIMEOUT,
        SessionLimits {
            max_prepared_statements: 3,
        },
    );
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for i in 0..3 {
        parse_named(&mut stream, &format!("s{i}"));
        expect_parse_complete(&mut stream, &format!("statement s{i}"));
    }
    parse_named(&mut stream, "s3");
    expect_54000_then_recovers(&mut stream);

    // 無名は数えない契約: 上限到達後も受理される。
    parse_named(&mut stream, "");
    expect_parse_complete(&mut stream, "anonymous statement");

    // Close（statement）で 1 件解放すると、新しい名前が再び通る。
    let mut close_body = vec![b'S'];
    close_body.extend_from_slice(b"s0\0");
    send_length_prefixed_message(&mut stream, b'C', &close_body);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3', "expected CloseComplete");
    parse_named(&mut stream, "s-new");
    expect_parse_complete(&mut stream, "statement after Close");
}

/// B5: ハード上限 1024 を設定値にした場合も、1024 件は受理・1025 件目は `54000`。
/// Parse をまとめて送り、応答をまとめて読む（往復待ちで遅くならない）。
#[test]
fn prepared_statement_boundary_at_hard_upper_bound() {
    const HARD: usize = 1024;
    let (core, _guard) = new_core_with_documents_table("wire25-b5");
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server_with_engine_and_limits(
        &users_path,
        core,
        8,
        LONG_TIMEOUT,
        SessionLimits {
            max_prepared_statements: HARD,
        },
    );
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // 書き込みが応答待ちでデッドロックしないよう、書き側を別スレッドに任せる。
    let mut writer = stream.try_clone().expect("clone stream");
    let sender = std::thread::spawn(move || {
        for i in 0..HARD {
            parse_named(&mut writer, &format!("s{i}"));
        }
    });
    for i in 0..HARD {
        expect_parse_complete(&mut stream, &format!("statement s{i}"));
    }
    sender.join().expect("sender thread");

    parse_named(&mut stream, &format!("s{HARD}"));
    expect_54000_then_recovers(&mut stream);
}

// ---------------------------------------------------------------------------
// keepalive 非送出（ソース検査）
// ---------------------------------------------------------------------------

/// B7（補助）: サーバー実装が TCP keepalive を設定していないこと。振る舞い面は上の
/// アイドル試験が担い、ここは `src/**/*.rs` の参照有無で退行を検出する。
#[test]
fn server_sources_do_not_configure_tcp_keepalive() {
    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect(&src, &mut files);
    assert!(!files.is_empty(), "source scan must find files");
    // 検索語は連結で組み立て、本ファイル自身は src 配下にないため対象外。
    let needles = ["set_", "keepalive"].concat();
    let upper = ["SO_", "KEEPALIVE"].concat();
    let mac = ["TCP_", "KEEPALIVE"].concat();
    for f in files {
        let text = std::fs::read_to_string(&f).expect("read source");
        assert!(
            !text.contains(&needles) && !text.contains(&upper) && !text.contains(&mac),
            "{} must not configure TCP keepalive (WIRE-25)",
            f.display()
        );
    }
}

// ---------------------------------------------------------------------------
// 子プロセス（CLI → 各表層の配線）
// ---------------------------------------------------------------------------

fn write_single_user_store(path: &str) {
    use wire_server::auth::argon2id;
    let phc = argon2id::encode_phc(
        b"correct-horse",
        b"0123456789abcdef",
        &argon2id::RECOMMENDED_PARAMS,
    )
    .expect("valid phc encoding");
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write user store");
}

fn spawn_child(fixture: &TempFixtureDir, extra: &[&str]) -> (SpawnedServer, SocketAddr) {
    let users = fixture.users_path_str();
    let db = fixture.db_path_str();
    let mut args = vec!["--users", &users, "--db", &db, "--bind", "127.0.0.1:0"];
    args.extend_from_slice(extra);
    let mut server = SpawnedServer::spawn(&args);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must report listening address");
    (server, addr.parse().expect("valid socket addr"))
}

/// B8: `--surface nosql --max-connections 2` が NoSQL 表層の接続枠へ届く。
#[test]
fn child_nosql_max_connections_flag_applies_to_http_surface() {
    let fixture = TempFixtureDir::new("wire25-b8-conn");
    write_empty_user_store(&fixture.users_path_str());
    let (_server, addr) = spawn_child(
        &fixture,
        &[
            "--surface",
            "nosql",
            "--max-connections",
            "2",
            "--read-timeout-secs",
            "5",
        ],
    );

    let _first = TcpStream::connect(addr).expect("connect first");
    let _second = TcpStream::connect(addr).expect("connect second");
    // 子プロセスの active() は外から観測できないため、受理を待つ余裕を持たせる。
    std::thread::sleep(Duration::from_millis(500));
    let mut third = TcpStream::connect(addr).expect("connect third");
    let text = read_until_close(&mut third);
    assert!(
        text.starts_with("HTTP/1.1 503 ") && text.contains(SQLSTATE_TOO_MANY_CONNECTIONS),
        "third connection must be rejected with 503/53300, got: {text:?}"
    );
}

/// B8 + B7: `--surface nosql --read-timeout-secs 1` で無通信接続が既定 30 秒よりずっと早く、
/// 0 バイトのまま EOF になる。
#[test]
fn child_nosql_read_timeout_flag_applies_to_http_surface() {
    let fixture = TempFixtureDir::new("wire25-b8-timeout");
    write_empty_user_store(&fixture.users_path_str());
    let (_server, addr) = spawn_child(
        &fixture,
        &["--surface", "nosql", "--read-timeout-secs", "1"],
    );

    let mut stream = TcpStream::connect(addr).expect("connect");
    let elapsed = assert_eof(&mut stream, Duration::from_secs(4));
    assert!(
        elapsed < Duration::from_secs(4),
        "idle HTTP connection must close at the 1s read timeout, took {elapsed:?}"
    );
}

/// B8: `--max-prepared-statements 2` が SQL 表層のセッションへ届く（3 件目の名前付きは `54000`）。
#[test]
fn child_sql_max_prepared_statements_flag_applies_to_session() {
    let fixture = TempFixtureDir::new("wire25-b8-prepared");
    write_single_user_store(&fixture.users_path_str());
    // 子プロセスの `--db` へ先に表を作っておく（Parse は存在するテーブルを要する）。
    {
        let storage = Storage::open(std::path::Path::new(&fixture.db_path_str()))
            .expect("open fixture storage");
        storage
            .create_table(&TableSchema::new(
                "documents",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(3), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table");
    }
    let (_server, addr) = spawn_child(&fixture, &["--max-prepared-statements", "2"]);

    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    for i in 0..2 {
        parse_named(&mut stream, &format!("s{i}"));
        expect_parse_complete(&mut stream, &format!("statement s{i}"));
    }
    parse_named(&mut stream, "s2");
    expect_54000_then_recovers(&mut stream);
}
