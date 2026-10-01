//! `--ddl-allowed-users` opt-in（SQL-23・TASK-203、Issue #902）をバイナリ子プロセス
//! として起動し、CLI 引数の受理・拒否（fail-closed）を外形的に検証する結合
//! テスト。`wire_durability_cli.rs`・`wire_search_engine_cli.rs` と同じ流儀
//! （実バイナリを `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、
//! stderr の `listening on` 行または非 0 終了・エラーメッセージを外形的に
//! 確認する）。
//!
//! - R1: 未知ユーザーを指す `--ddl-allowed-users` は非 0 終了・stderr にフラグ名を
//!   含むこと（fail-closed。設定ミスを起動時に検出する）
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含むこと
//! - R3: 重複指定は非 0 終了・stderr に "specified more than once" を含むこと
//! - R4: ユーザーストアに実在するユーザーを指す `--ddl-allowed-users` は
//!   `listening on` に到達すること（起動を妨げない）
//!
//! wire フレーミング越しの実際の権限ゲート挙動（`CommandComplete`／
//! `42501`）は `wire_create_table.rs` の担当（本ファイルは CLI 解析の外形
//! 確認に徹する）。

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[path = "common/mod.rs"]
mod common;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-ddl-permission-cli-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        Self { dir }
    }

    fn users_path_str(&self) -> String {
        self.dir
            .join("users.txt")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    fn db_path_str(&self) -> String {
        self.dir
            .join("db.redb")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        // 削除失敗はテストを失敗させず（Drop 内 panic は二重 panic の恐れがある）、
        // パスとエラーだけを stderr へ出す（Issue #1303）。
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to remove temp entry {}: {e}",
                    self.dir.display()
                );
            }
        }
    }
}

fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// `alice:tenant-a:<phc>` の 1 行を持つユーザーストア
/// （`wire_durability_cli.rs::write_user_store_with_alice` と同型）。
fn write_user_store_with_alice(path: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-password");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"pw-alice\n")
        .expect("write password to stdin");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success(), "hash-password must succeed");
    let phc = String::from_utf8(output.stdout)
        .expect("utf-8 phc")
        .trim()
        .to_string();
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write user store");
}

/// 非 0 終了・stderr にフラグ名を含む拒否系（R1〜R3）を検証する共通ヘルパー。
fn assert_startup_rejected(extra_args: &[&str], expect_substr: &str) {
    let fixture = TempFixtureDir::new("reject");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let mut args = vec![
        "--users".to_string(),
        users_path,
        "--db".to_string(),
        db_path,
        "--bind".to_string(),
        "127.0.0.1:0".to_string(),
    ];
    args.extend(extra_args.iter().map(|s| s.to_string()));

    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(&args)
        .output()
        .expect("spawn wire-server");
    assert!(
        !output.status.success(),
        "expected non-zero exit for args {extra_args:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expect_substr),
        "expected stderr to contain {expect_substr:?}, got: {stderr}"
    );
}

#[test]
fn ddl_allowed_users_rejects_unknown_user() {
    assert_startup_rejected(&["--ddl-allowed-users", "carol"], "--ddl-allowed-users");
}

#[test]
fn ddl_allowed_users_rejects_missing_value() {
    assert_startup_rejected(&["--ddl-allowed-users"], "--ddl-allowed-users");
}

#[test]
fn ddl_allowed_users_rejects_duplicate_flag() {
    assert_startup_rejected(
        &[
            "--ddl-allowed-users",
            "alice",
            "--ddl-allowed-users",
            "alice",
        ],
        "specified more than once",
    );
}

#[test]
fn ddl_allowed_users_rejects_empty_element() {
    assert_startup_rejected(
        &["--ddl-allowed-users", "alice,,bob"],
        "--ddl-allowed-users",
    );
}

#[test]
fn ddl_allowed_users_accepts_known_user_and_reaches_listening() {
    let fixture = TempFixtureDir::new("accept");
    let users_path = fixture.users_path_str();
    write_user_store_with_alice(&users_path);
    let db_path = fixture.db_path_str();

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--ddl-allowed-users",
            "alice",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    assert!(
        common::wait_for_listening(&mut child, Duration::from_secs(10)),
        "expected listening on with valid --ddl-allowed-users"
    );
    let _ = child.kill();
    let _ = child.wait();
}
