//! `POST /v1/query`（`op: "insert"`）で commit 成功直後に panic した場合、
//! HTTP 応答境界ガード（RECOVER-5 (3)。`crate::http::conn::build_outcome` の
//! `engine::recovery::commit_boundary::ResponseBoundaryGuard`。codex-review
//! P1 指摘対応・PR #829）が、`catch_unwind` による通常の `500` への縮退を
//! 構造的に防ぎ、必ずプロセス終了へ倒すことを検証する層 A 結合テスト。
//!
//! 注入点は `crate::fault_injection::maybe_panic_after_http_insert_commit`
//! （`crate::http::query::insert::execute` の commit 成功直後から呼ばれる。
//! `tests/wire_fault_injection_cli.rs`（SQL wire 版。Issue #705）と対になる
//! 構成）。
//!
//! ## 2 系統のテストが分担する観測境界（重要）
//!
//! - [`post_commit_panic_during_http_insert_aborts_instead_of_downgrading_to_500`]
//!   は**実バイナリ**（`CARGO_BIN_EXE_wire-server`）を `--surface nosql
//!   --fault-inject post-commit-panic` で子プロセス起動する。しかし `main.rs::
//!   run_server` は起動時に無条件で `engine::recovery::fail_fast::install`
//!   （TASK-99・RECOVER-8）を導入しており、その panic hook は unwind **前**に
//!   `std::process::abort()` する。したがってこの経路では
//!   `ResponseBoundaryGuard`・`build_outcome` の `catch_unwind` のいずれにも
//!   実際には到達せず（`fail_fast` が先に介入する）、本テストは「実バイナリの
//!   end-to-end 挙動として commit 後 panic が確実にプロセス終了へ倒れる」
//!   ことの回帰は検出するが、**`ResponseBoundaryGuard` 自体の効果は判別しない**
//!   （`fail_fast` を外しても・`ResponseBoundaryGuard` を外しても同じ結果になる。
//!   実際に本 Issue 実装時に手動で確認済み）。
//! - [`subprocess_post_commit_panic_during_http_insert_aborts_without_fail_fast_installed`]
//!   が `ResponseBoundaryGuard` 自体を判別する側。`fail_fast::install`／
//!   `engine::recovery::panic_hook::install_panic_hook` のいずれも呼ばない
//!   （Rust 既定の panic hook のまま）状態で、production ルータ（`wire_server::
//!   http::router::Router`）を **同一テストバイナリ内**（`std::env::current_exe()`
//!   を `--exact <このテスト名> --test-threads=1` で自己再実行する子プロセス）
//!   で起動する。これは codex-review P1 指摘が対象とする「`fail_fast` を
//!   導入しないライブラリ利用」を直接再現し、`crates/engine/src/recovery/
//!   commit_boundary.rs` の `subprocess_panic_after_commit_and_finish_returns_
//!   still_aborts_within_response_boundary` 等と同型の手法（libtest 自身の
//!   `catch_unwind` すら `ResponseBoundaryGuard` の unwind 中 Drop が先回りする
//!   ことの実証）を HTTP 接続ハンドラの `catch_unwind`（`crate::http::conn::
//!   handle_connection_with`）に対して適用する。
//!
//! HTTP 表層は RECOVER-6（緊急応答の同期送出）をまだ実装していないため
//! （`docs/design/nosql-insert-mapping.md`「対象外」節参照）、いずれのテストも
//! 検証するのは RECOVER-5 の安全性側（commit 後 panic が「通常応答へ縮退して
//! 処理を継続する」経路を通らずプロセス終了へ倒れること）のみである。
//! クライアント側は完全な応答を受け取らずに接続断のみを観測する。
//!
//! `--fault-inject`・`fault_injection::arm` はいずれも feature `fault-injection`
//! 限定（`crates/wire-server/Cargo.toml`）。`make lint`／`make test` は
//! `--all-features` のため CI でも常にコンパイル・実行される（既定ビルド側の
//! `--fault-inject` 拒否契約は既存の `tests/wire_fault_injection_cli.rs` が
//! 検査済みであり、本ファイルは feature 有効時のみ意味を持つため二重化しない）。

#![cfg(feature = "fault-injection")]

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::AfterWrite;

const TABLE: &str = "docs";
const TENANT: &str = "tenant-a";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
    )
}

/// `docs(embedding VECTOR(3))` の空テーブルを用意する（子プロセス起動前に
/// `Storage` を drop すること。redb のファイルロック回避。
/// `wire_fault_injection_cli.rs::create_empty_docs_table` と同型）。
fn create_empty_docs_table(db_path: &str) {
    let storage = Storage::open(db_path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    drop(storage);
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("subprocess did not terminate within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn assert_aborted(status: std::process::ExitStatus) {
    use std::os::unix::process::ExitStatusExt as _;
    assert!(
        !status.success(),
        "child must not exit successfully; status={status:?}"
    );
    assert_eq!(
        status.signal(),
        Some(6),
        "child must be terminated by SIGABRT (std::process::abort); status={status:?}"
    );
}

fn spawn_armed(users_path: &str, db_path: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_path,
            "--db",
            db_path,
            "--bind",
            "127.0.0.1:0",
            "--surface",
            "nosql",
            "--fault-inject",
            "post-commit-panic",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server")
}

/// `POST /v1/session` でログインし、応答本文の `token` フィールドを返す。
fn login(addr: SocketAddr) -> String {
    let body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

/// `POST /v1/query`（`insert`）を送り、完全な応答フレームが返らない（＝プロセス
/// abort による接続断のみが観測される）ことを確認する。
///
/// `http_common::parse_single_response` は完全な応答フレームを前提とするため
/// ここでは使わない（不完全な応答は解析失敗＝panic になり、意図した「応答が
/// 完成しないまま接続が切れる」ことをそのまま検証できなくなる）。
fn send_insert_and_assert_no_complete_500(addr: SocketAddr, token: &str, body: &[u8]) {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream.write_all(&request).expect("write request");
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            // プロセス abort による接続断は `ConnectionReset`／`TimedOut` 等
            // 複数の形で観測されうる。いずれも「完全な応答は来なかった」
            // 証跡として扱う。
            Err(_) => break,
        }
    }

    // ガードが機能せず `catch_unwind` が通常経路へ縮退させていれば、ここに
    // 完成した `HTTP/1.1 500 ...` 応答が観測されるはず（RECOVER-5 違反の
    // 再現形）。ガードが正しく作用していれば、応答は不完全（0 バイト、または
    // ヘッダ／本文が完成しない途中経過）のまま接続が切れる。
    let text = String::from_utf8_lossy(&received);
    assert!(
        !text.starts_with("HTTP/1.1 500"),
        "commit-success panic must not be downgraded to a complete HTTP 500 response \
         (RECOVER-5 violation); got: {text:?}"
    );
}

/// 実バイナリ end-to-end: arm 済み・commit 成功 `insert` の直後に panic すると、
/// クライアントは完成した応答を受け取らず、プロセスは `SIGABRT` で終了する
/// （`catch_unwind` による通常応答への縮退が起きない）。commit 自体は成功
/// しているため、再オープン後も行は可視のまま。
///
/// **注意**（モジュール doc「2 系統のテストが分担する観測境界」節参照）:
/// 実バイナリは `fail_fast::install` を無条件に導入するため、本テストは
/// `ResponseBoundaryGuard` 自体を判別しない（`fail_fast` が先に abort する）。
/// `ResponseBoundaryGuard` 固有の回帰検出は
/// [`subprocess_post_commit_panic_during_http_insert_aborts_without_fail_fast_installed`]
/// が担う。
#[test]
fn post_commit_panic_during_http_insert_aborts_instead_of_downgrading_to_500() {
    let users_path = common::write_user_store_file(&[("alice", TENANT, "pw-alice")]);
    let db_path = temp_db::unique_db_path("http-insert-response-boundary-armed");
    let _guard = temp_db::CleanupGuard(db_path.clone());
    create_empty_docs_table(db_path.to_str().expect("utf-8 path"));

    let mut child = spawn_armed(
        users_path.to_str().expect("utf-8 path"),
        db_path.to_str().expect("utf-8 path"),
    );
    let (addr, lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    assert!(
        lines.iter().any(|l| l.contains("fault injection armed")),
        "expected 'fault injection armed' line before 'listening on'; lines={lines:?}"
    );

    let token = login(addr);
    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"http-fi-armed-op-1"}"#;
    send_insert_and_assert_no_complete_500(addr, &token, insert_body);

    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert_aborted(status);

    // commit 自体は成功しているため、再オープン後も行が可視であること
    // （`wire_fault_injection_cli.rs::armed_post_commit_panic_sends_emergency_
    // response_then_aborts` と同じ確認方法）。
    let storage = Storage::open(&db_path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let read_ctx =
        PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut session = engine::sql::mode::SessionState::default();
    let outcome = core
        .execute_sql_in_session(&read_ctx, &mut session, "SELECT id FROM docs LIMIT 10")
        .expect("select should succeed");
    let engine::sql::SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    assert!(
        result.rows.iter().any(|row| row.id == 1),
        "the committed row must remain visible after the abort path: {:?}",
        result.rows
    );
}

/// 対照テスト: feature ビルドでもフラグ無しなら不活性で、insert は通常どおり
/// `200` の成功応答を返す（`wire_fault_injection_cli.rs::
/// feature_build_without_flag_inserts_normally` の HTTP 版。ガードの存在が
/// 通常経路の挙動を変えないことを固定する）。
#[test]
fn feature_build_without_flag_inserts_normally_over_http() {
    let users_path = common::write_user_store_file(&[("alice", TENANT, "pw-alice")]);
    let db_path = temp_db::unique_db_path("http-insert-response-boundary-unarmed");
    let _guard = temp_db::CleanupGuard(db_path.clone());
    create_empty_docs_table(db_path.to_str().expect("utf-8 path"));

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_path.to_str().expect("utf-8 path"),
            "--db",
            db_path.to_str().expect("utf-8 path"),
            "--bind",
            "127.0.0.1:0",
            "--surface",
            "nosql",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let (addr, lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    assert!(
        !lines.iter().any(|l| l.contains("fault injection armed")),
        "unarmed process must not announce fault injection; lines={lines:?}"
    );

    let token = login(addr);
    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"http-fi-unarmed-op-1"}"#;
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &insert_body.len().to_string()),
        ],
        insert_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "body={resp:?}");

    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// `ResponseBoundaryGuard` 自体を判別する self re-exec テスト
// （モジュール doc「2 系統のテストが分担する観測境界」節参照）。
// ---------------------------------------------------------------------------

/// 子プロセスモードの判定に使う環境変数（DB パスを渡す）。
const CHILD_DB_ENV: &str = "WIRE_SERVER_HTTP_INSERT_RESPONSE_BOUNDARY_CHILD_DB";

/// ガードが機能せず `catch_unwind` が通常経路へ縮退してしまった場合にのみ
/// 子プロセスが辿り着く明示的な終了コード（abort であれば決して到達しない）。
/// `0`（成功）や `101`（Rust panic の既定終了コード）と衝突しない値を選ぶ。
const CHILD_GUARD_NOT_TRIGGERED_EXIT_CODE: i32 = 42;

/// `ResponseBoundaryGuard`（RECOVER-5 (3)）固有の回帰検出テスト。
///
/// 子プロセス側は `engine::recovery::fail_fast::install`／`engine::recovery::
/// panic_hook::install_panic_hook` のいずれも呼ばない（Rust 既定の panic hook
/// のまま）状態で production ルータ（`wire_server::http::router::Router`）を
/// in-process 起動し、`fault_injection::arm` → HTTP insert 経由で commit 成功
/// 直後の panic を発生させる。`crate::http::conn::handle_connection_with` の
/// `catch_unwind` はこの子プロセスでも実在するため、`ResponseBoundaryGuard`
/// が無ければ panic は捕捉されて通常の `500` 応答へ縮退し、接続ハンドラの
/// スレッドは（プロセスを終了させずに）処理を継続してしまう。ここで
/// [`CHILD_GUARD_NOT_TRIGGERED_EXIT_CODE`] を明示的な終了コードとして使い、
/// 「ガードが効かなかった」ことを親プロセスが判別可能にする（native な
/// タイムアウト以外の失敗証跡を残す）。
#[test]
fn subprocess_post_commit_panic_during_http_insert_aborts_without_fail_fast_installed() {
    if let Ok(db_path) = std::env::var(CHILD_DB_ENV) {
        // --- 子プロセス側 ---
        let storage = Storage::open(&db_path).expect("child: open storage");
        storage
            .create_table(&schema())
            .expect("child: create table");
        let core = std::sync::Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        ));

        // ログインを経由せず `SessionStore::issue`（pub API）で直接トークンを
        // 発行する（本テストの関心は認証フローではなく応答境界ガードの
        // 効果のみのため。`users` は `Router::with_engine` の構築に必要な
        // だけの空ストア）。
        let users_path = temp_db::unique_db_path("http-insert-response-boundary-child-users");
        std::fs::write(&users_path, "").expect("child: write empty user store");
        let users = std::sync::Arc::new(
            wire_server::auth::UserStore::load_from_file(&users_path)
                .expect("child: load empty user store"),
        );
        // 子プロセスは意図的に abort するため Drop ガードが走らない。読み込み済みの
        // ファイルをここで削除して残置を防ぐ（Issue #1303）。
        if let Err(e) = std::fs::remove_file(&users_path) {
            eprintln!("warning: failed to remove {}: {e}", users_path.display());
        }

        let sessions = wire_server::http::session::store::SessionStore::new();
        let ctx = PolicyContext::new(TENANT).expect("child: valid tenant");
        let token = sessions
            .issue(ctx, Instant::now())
            .expect("child: issue session token");

        let router = wire_server::http::router::Router::with_engine(users, sessions, core);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("child: bind listener");
        let addr = listener.local_addr().expect("child: local addr");
        let limiter =
            wire_server::limits::ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
        std::thread::spawn(move || {
            wire_server::http::listener::accept_loop_with_router(
                listener,
                limiter,
                wire_server::limits::READ_TIMEOUT,
                router,
            );
        });

        wire_server::fault_injection::arm(wire_server::fault_injection::FaultKind::PostCommitPanic);

        let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"http-rb-op-1"}"#;
        let request = format!(
            "POST /v1/query HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            token.encoded(),
            insert_body.len()
        );

        let mut stream = TcpStream::connect(addr).expect("child: connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("child: set read timeout");
        stream
            .write_all(request.as_bytes())
            .expect("child: write request head");
        stream
            .write_all(insert_body)
            .expect("child: write request body");
        let _ = stream.shutdown(std::net::Shutdown::Write);

        // 応答（あれば）を読み切る。ガードが機能していれば、読み取りの途中
        // または直後にプロセスが abort され、以降のコードは実行されない。
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }

        // ここへ到達した＝ガードが機能せず panic が通常経路へ縮退した
        // （RECOVER-5 違反）ことを親プロセスへ伝える固有の終了コード。
        std::process::exit(CHILD_GUARD_NOT_TRIGGERED_EXIT_CODE);
    }

    // --- 親プロセス側 ---
    let db_path = temp_db::unique_db_path("http-insert-response-boundary-guard-subprocess");
    let _cleanup = temp_db::CleanupGuard(db_path.clone());
    drop(Storage::open(&db_path).expect("parent: create storage"));

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("subprocess_post_commit_panic_during_http_insert_aborts_without_fail_fast_installed")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_DB_ENV, &db_path)
        .stdout(Stdio::piped())
        // 既定の panic hook がバックトレースを吐きうる（fail_fast を導入しない
        // ため）。stderr は pipe せず破棄する（パイプバッファ詰まり回避。
        // `crates/engine/src/recovery/commit_boundary.rs` の同型 subprocess
        // テストと同じ方針）。
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child process");

    let status = wait_for_exit(&mut child, Duration::from_secs(30));

    let mut stdout_buf = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout_buf);
    }

    assert_ne!(
        status.code(),
        Some(CHILD_GUARD_NOT_TRIGGERED_EXIT_CODE),
        "ResponseBoundaryGuard did not fire: the panic was caught by catch_unwind and \
         downgraded to a normal response instead of aborting the process (RECOVER-5 \
         violation); status={status:?} stdout={stdout_buf}"
    );
    assert!(
        !status.success(),
        "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            status.signal(),
            Some(6),
            "child must be terminated by SIGABRT (std::process::abort) via \
             ResponseBoundaryGuard even without fail_fast::install being called; \
             status={status:?} stdout={stdout_buf}"
        );
    }

    // commit 自体は成功しているため、再オープン後も行が可視であること。
    let storage = Storage::open(&db_path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let read_ctx =
        PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut session = engine::sql::mode::SessionState::default();
    let outcome = core
        .execute_sql_in_session(&read_ctx, &mut session, "SELECT id FROM docs LIMIT 10")
        .expect("select should succeed");
    let engine::sql::SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    assert!(
        result.rows.iter().any(|row| row.id == 1),
        "the committed row must remain visible after the abort path: {:?}",
        result.rows
    );
}
