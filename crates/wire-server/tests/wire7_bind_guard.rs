//! バイナリ `wire-server` を実プロセスとして起動し、非ループバック bind が
//! 起動時に非 0 終了で拒否されること・loopback bind は正常に起動へ進むことを
//! 検証する結合テスト（対応: TASK-70。対象ビヘイビア WIRE-7）。
//!
//! `crates/wire-server/src/bind_guard.rs` の単体テストは `GuardedBindAddrs` の
//! 判定ロジックのみを検証するため、ここでは `main.rs::run_server` が実際に
//! プロセスを非 0 終了させること・stderr に拒否理由を出力することを外形的に
//! 確認する（ユーザーストアの内容には依存しないよう空ファイルで固定する）。

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[path = "common/mod.rs"]
mod common;

/// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
/// プロセス内単調カウンタ（`wire_auth.rs` と同一クラスの競合対策。Issue #172）。
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ユーザーストアディレクトリ／ファイルを保持し、
/// `Drop` でディレクトリごと確実に削除するガード（対応: TASK-70 review 指摘。
/// 空ファイル（ユーザー登録なし）で十分（bind ガードはユーザーストア読込より
/// 前に実行される）。`assert!` の panic 経路でも `Drop` によりクリーンアップが
/// 走るため、ファイル削除のみに頼っていた旧実装のような temp ディレクトリの
/// 残留を起こさない。
struct TempUserStore {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl TempUserStore {
    fn new() -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-wire7-test-{}-{}-{}",
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
        std::fs::write(&path, "").expect("write empty user store");
        Self { dir, path }
    }

    fn path_str(&self) -> &str {
        self.path.to_str().expect("utf-8 path")
    }

    /// `--db` に渡す一時 DB ファイルパス（本テストは bind ガードの外形挙動のみを
    /// 検証するため、ファイル自体は作成せず `EngineCore::open` に新規作成させる。
    /// TASK-73 で `--db` が必須化されたため追加）。
    fn db_path_str(&self) -> String {
        self.dir
            .join("db.redb")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for TempUserStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 非ループバックアドレス（`0.0.0.0` / `[::]`）を指定すると、TLS 未構成
/// （TASK-72/WIRE-9）のため起動が非 0 終了で拒否されること。
#[test]
fn non_loopback_bind_exits_non_zero() {
    let users_store = TempUserStore::new();

    let db_path = users_store.db_path_str();
    for bind_addr in ["0.0.0.0:0", "[::]:0"] {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                users_store.path_str(),
                "--db",
                &db_path,
                "--bind",
                bind_addr,
            ])
            .output()
            .expect("spawn wire-server");

        assert!(
            !output.status.success(),
            "non-loopback bind {bind_addr} must exit non-zero"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("refusing to bind non-loopback") && stderr.contains("TLS"),
            "stderr for {bind_addr} should explain the TLS-related refusal, got: {stderr}"
        );
    }
}

/// loopback アドレス（`127.0.0.1`）は起動拒否されず、accept ループへ進むこと
/// （stderr に `listening on` が出力されるまで待って確認する）。
///
/// listen 待ち受けは `common::wait_for_listening`（Issue #1082）に委譲する。
/// 同ヘルパーは listen 行到達後も子プロセスの stderr を EOF まで読み続ける
/// ため、待ち受け後に受信側を破棄してもパイプの読み口が閉じない（旧実装は
/// 待ち受け用チャネルの送信失敗でループを抜け読み口を閉じており、下記の
/// 回帰テストが再現する「listen 後の診断行の欠落」を招きうる形だった）。
#[test]
fn loopback_bind_starts_listening() {
    let users_store = TempUserStore::new();

    let db_path = users_store.db_path_str();
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_store.path_str(),
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let saw_listening = common::wait_for_listening(&mut child, Duration::from_secs(5));

    // 起動を確認できたら子プロセスを終了させ、ゾンビを残さないよう wait する。
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        saw_listening,
        "loopback bind must not be rejected and must reach the listening state"
    );
}

/// 回帰テスト（Issue #1082。`common::drain_stderr`／`StderrDrain` が listen
/// 行到達後も stderr を EOF まで読み続けることを固定する。
/// `three_client_e2e.rs::
/// server_guard_keeps_draining_stderr_so_logged_connection_errors_do_not_abort_server`
/// と同じ手順を、本ファイルが担当する pg wire 表層の CLI 起動経路で確認する）:
/// 未読データを残したまま接続を閉じて RST を送り、サーバー側に
/// `connection error` を複数回ログさせたうえで、それが `tail` へ届くこと
/// （非 vacuous 化）とサーバーが生存し続けていることを確認する。
///
/// #1081 以前は listen 待ち受け後に受信側チャネルを破棄するとパイプの読み口が
/// 閉じ、以降にサーバーが stderr へ書くと `EPIPE` で `eprintln!` が panic し
/// panic フック（TASK-97・RECOVER-6／TASK-99・RECOVER-8）経由で SIGABRT
/// 終了しうる問題があった（`three_client_e2e.rs::ServerGuard` が先に是正
/// 済み）。#1081 でサーバー側の診断ログが `engine::log_stderr!`（書き込み
/// 失敗を無視する）へ置き換わったため、現在は読み口を閉じてもサーバー側は
/// abort しないが、旧実装のまま（読み口を閉じたまま）だと listen 後の
/// `connection error` 行が読み取れず本テストの `tail` が空のまま失敗する
/// （手元でローカル読み取りループを旧実装へ戻して確認済み。コミットしない）。
#[test]
fn common_listen_helper_keeps_draining_stderr_so_logged_connection_errors_do_not_abort_server() {
    const RESET_CONNECTIONS: usize = 3;

    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let users_store = TempUserStore::new();
    let db_path = users_store.db_path_str();

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_path.to_str().expect("utf-8 path"),
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let mut drain = common::drain_stderr(&mut child);
    let (addr, lines) = drain.wait_for_listening(Duration::from_secs(10));
    let addr = addr
        .unwrap_or_else(|| panic!("did not observe listening address; lines so far: {lines:?}"));

    for _ in 0..RESET_CONNECTIONS {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        common::send_startup_message(&mut stream, "alice", "docs");
        // サーバーの認証要求が受信バッファに届いたことを `peek`（消費しない）
        // で確かめてから読まずに閉じる（未読データを残した close は RST に
        // なり、サーバー側の読み取りが ECONNRESET となって stderr へ 1 行
        // ログされる。固定 sleep だと高負荷下で到着前に閉じ FIN になりうる
        // ため待ち合わせる）。
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set_read_timeout");
        let mut probe = [0u8; 1];
        let peeked = stream
            .peek(&mut probe)
            .expect("peek authentication request");
        assert!(peeked >= 1, "expected pending authentication request bytes");
        drop(stream);
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let logged = drain
            .tail_lines()
            .iter()
            .filter(|l| l.contains("connection error"))
            .count();
        if logged >= 2 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "expected at least 2 logged connection errors, got {logged}; tail={:?}",
                drain.tail_lines()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "server must still be alive after logging connection errors"
    );

    let _ = child.kill();
    let _ = child.wait();
}
