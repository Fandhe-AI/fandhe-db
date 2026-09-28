//! 統合テスト（Issue #1081、対象ビヘイビア: RECOVER-8。ポインタ:
//! `docs/design/stderr-log-write-failure.md`）: `wire-server` 実バイナリを子
//! プロセスとして起動し、stderr の読み手（親プロセス側のパイプ読み口）を
//! 閉じた後でも、サーバーが接続エラー等の診断ログを出す経路
//! （`protocol_dispatch::reject_and_close`・`server::accept_loop_*` の
//! `connection error` ログ）を複数回通っても abort しないことを固定する。
//!
//! `#[ignore]` は付けず `make ci` で常時実行する（外部クライアント・DB
//! シードを必要とせず、psql/psycopg/pg が未導入のローカル環境でも走る）。
//!
//! 検証の骨子:
//! 1. stderr を読む別スレッドが listen 行を受け取ったら join し、`ChildStderr`
//!    の読み口が確実に drop された状態を作る（`tests/three_client_e2e.rs::
//!    ServerGuard` とは異なり、ここでは読み口を閉じたままにすることが検証の
//!    核心のため、読み続けない）。
//! 2. 認証済み接続へ FunctionCall（`'F'`）を送る。`protocol_dispatch::
//!    reject_and_close` は ErrorResponse を書く**前**に `engine::log_stderr!`
//!    でログを出す契約（`crates/wire-server/src/protocol_dispatch.rs`
//!    参照）。`0A000` の ErrorResponse が届くこと自体が「閉じたパイプへの
//!    書き込みが起き、スレッドもプロセスも死ななかった」ことの証跡になる。
//! 3. 加えて `server::accept_loop_*` の `connection error` ログ経路
//!    （Issue #943 と同型の RST 切断）も通し、新規接続が ReadyForQuery まで
//!    届くこと・子プロセスが生存し続けることを確認する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use wire_server::auth::argon2id;

/// 子プロセス（`wire-server`）を Drop で必ず kill・wait する（ゾンビ防止）。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `--users <path> --db <path> --bind 127.0.0.1:0` で `wire-server` を起動し、
/// stderr の `listening on <addr>` 行から bind 済みポートを取得する。listen
/// 行の読み取りスレッドは listen 行到達後に join し、`ChildStderr` の読み口
/// （`stderr` フィールドとして返す）を、この関数を抜けた時点で確実に drop
/// できる状態にする（呼び出し元が明示的に drop するまで保持する）。
fn spawn_wire_server_with_owned_stderr(
    users_path: &Path,
    db_path: &Path,
) -> (ChildGuard, u16, std::process::ChildStderr) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("--users")
        .arg(users_path)
        .arg("--db")
        .arg(db_path)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server binary (built by `cargo test`)");

    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let port = loop {
        assert!(
            Instant::now() < deadline,
            "wire-server did not report a listening port within the deadline"
        );
        line.clear();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("wire-server stderr closed before reporting a listening port");
        }
        let trimmed = line.trim();
        if let Some(addr_str) = trimmed.strip_prefix("wire-server: listening on ") {
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                break addr.port();
            }
        }
    };
    stderr = reader.into_inner();

    (ChildGuard(child), port, stderr)
}

/// 単一ユーザー（alice/tenant-a）のみの認証ファイル。テーブル作成は不要
/// （`reject_and_close` は post-auth・pre-dispatch の分類のみで完結する）。
fn write_single_user_file(path: &Path) {
    let salt = b"0123456789abcdef";
    let phc = argon2id::encode_phc(b"correct-horse", salt, &argon2id::RECOMMENDED_PARAMS)
        .expect("valid phc encoding");
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write users file");
}

/// テーブルを 1 つも持たない空 DB を用意する（`reject_and_close` の検証に
/// テーブルは不要。`storage::Storage::open` がファイルを新規作成する）。
fn seed_empty_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("stderr-closed-no-abort");
    let guard = temp_db::CleanupGuard(path.clone());
    // ファイルの存在自体を確定させる（`open` の遅延作成に依存しない）。
    engine::storage::Storage::open(&path).expect("open storage");
    (path, guard)
}

/// 核心の回帰テスト（Issue #1081）: stderr の読み手が閉じた状態で
/// `reject_and_close`（FunctionCall 拒否）と `connection error` ログ
/// （RST 切断）の両経路を複数回通しても、サーバーがプロセスごと abort せず、
/// 新規接続へ ReadyForQuery まで応答し続けることを確認する。
#[test]
fn stderr_closed_reader_does_not_abort_server() {
    let (db_path, _db_guard) = seed_empty_db();
    let users_dir = temp_db::TempDir::new("stderr-closed-no-abort-users");
    let users_path = users_dir.path().join("users.txt");
    write_single_user_file(&users_path);

    let (mut child_guard, port, stderr) =
        spawn_wire_server_with_owned_stderr(&users_path, &db_path);
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("valid addr");

    // 読み口を drop する ―― これが「stderr の読み手が閉じた」状態そのもの。
    // 以降サーバーが stderr へ書く度に読み手不在の書き込み（EPIPE 等）が起きる。
    drop(stderr);

    // 1. reject_and_close 経路: 認証済み接続へ FunctionCall を送り、
    //    ErrorResponse（0A000）が届くことを確認する。
    //    `protocol_dispatch::reject_and_close` は ErrorResponse を書く前に
    //    ログを出すため、応答が届くこと自体が非 vacuous な証跡になる。
    for _ in 0..3 {
        let mut stream = common::authenticate_to_ready_for_query(addr, "alice", "correct-horse");
        common::send_length_prefixed_message(&mut stream, b'F', &[]);
        common::expect_error_response_with_sqlstate(&mut stream, "0A000");
    }

    // 2. connection error ログ経路（Issue #943 と同型）: StartupMessage 送信後、
    //    認証要求が受信バッファへ届いたことを peek で確認してから、読まずに
    //    closeする（未読データを残した close は RST になり、サーバー側の
    //    読み取りが ECONNRESET として stderr へログされる）。
    for _ in 0..3 {
        let mut stream = TcpStream::connect(addr).expect("connect");
        common::send_startup_message(&mut stream, "alice", "irrelevant-db-name");
        let mut peek_buf = [0u8; 1];
        let peek_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match stream.peek(&mut peek_buf) {
                Ok(n) if n > 0 => break,
                _ => {
                    assert!(
                        Instant::now() < peek_deadline,
                        "authentication request did not arrive before the deadline"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        drop(stream);
    }

    // サーバーがまだ生存していること（abort していないこと）を確認する。
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        child_guard.0.try_wait().expect("try_wait").is_none(),
        "wire-server must not have exited (aborted) after stderr writes with a closed reader"
    );

    // 新規接続が引き続き ReadyForQuery まで到達できることを確認する
    // （プロセスは生きていても、内部状態が壊れていないことの追加確認）。
    let mut stream = common::authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    common::send_length_prefixed_message(&mut stream, b'F', &[]);
    common::expect_error_response_with_sqlstate(&mut stream, "0A000");
}
