//! `--scram-mock-key-file`（Issue #940 PR #1006 P1 是正）のファイルサイズ
//! 上限チェックをバイナリ子プロセスとして起動し、外形的に検証する結合
//! テスト（`tests/wire_durability_cli.rs` と同じ流儀。
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、非 0 終了・
//! stderr のメッセージまたは `listening on` 到達を確認する）。
//!
//! - R1: `SCRAM_MOCK_KEY_FILE_MAX_LEN` を超えるファイルは起動前に
//!   fail-closed で拒否される（非 0 終了・stderr に上限超過の説明）。
//!   本テストでは実際に上限バイト数のファイルを書き出さず、`/dev/zero`
//!   （終端しない特殊ファイル）を指定して検証する——`std::fs::read` に
//!   よる無制限読み込みだった旧実装ではこの入力に対して listen 前に
//!   メモリを無制限に確保しようとし続けるが、`Read::take` による本修正後
//!   は固定上限までしか読まず速やかに拒否されることを確認する。
//! - R2: 上限以内の妥当なファイル（`SCRAM_MOCK_KEY_FILE_MIN_LEN` バイトの
//!   ゼロ埋め）では引き続き `listening on` に到達すること（回帰確認）。

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[path = "common/mod.rs"]
mod common;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ
/// （`wire_durability_cli.rs::TempFixtureDir` と同型）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-scram-mock-key-size-{label}-{}-{}-{}",
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
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// R1: `/dev/zero`（終端しない特殊ファイル）指定は listen 前に fail-closed
/// で拒否される（非 0 終了・「通常ファイルでない」ことを示す stderr）。
/// メタデータでの事前検査（本修正で追加）が最初の防御層として働き、
/// キャラクタデバイスである `/dev/zero` はサイズ上限チェックへ進む前に
/// 弾かれる（`Read::take` によるサイズ上限は、メタデータの `len()` が
/// 信用できない特殊ファイルに対する 2 段目の防御として別途機能する）。
#[test]
fn oversized_mock_key_file_is_rejected_before_listening() {
    if !std::path::Path::new("/dev/zero").exists() {
        // `/dev/zero` が無い環境（非 Unix 系）ではこの再現手段が使えない
        // ため、このテストの前提が成立しない環境として skip する。
        eprintln!("skipping: /dev/zero not present on this platform");
        return;
    }

    let fixture = TempFixtureDir::new("r1");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--auth-method",
            "scram-sha-256",
            "--scram-mock-key-file",
            "/dev/zero",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn wire-server");

    assert!(
        !output.status.success(),
        "expected non-zero exit for /dev/zero mock key file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--scram-mock-key-file") && stderr.contains("not a regular file"),
        "expected stderr to reject the non-regular file, got: {stderr}"
    );
}

/// R1b: 通常ファイルであっても `SCRAM_MOCK_KEY_FILE_MAX_LEN` を超える
/// サイズは `Read::take` による読み込み上限で fail-closed に拒否される
/// （非 0 終了・上限超過を示す stderr）。
#[test]
fn oversized_regular_mock_key_file_is_rejected() {
    let fixture = TempFixtureDir::new("r1b");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();
    let key_path = fixture.dir.join("mock.key");
    // `wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_MAX_LEN`
    // （1 MiB）を 1 バイト超える通常ファイル。
    std::fs::write(&key_path, vec![0u8; 1024 * 1024 + 1]).expect("write oversized mock key file");

    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--auth-method",
            "scram-sha-256",
            "--scram-mock-key-file",
            key_path.to_str().expect("utf-8 path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn wire-server");

    assert!(
        !output.status.success(),
        "expected non-zero exit for oversized regular mock key file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--scram-mock-key-file") && stderr.contains("exceeds"),
        "expected stderr to report the size limit, got: {stderr}"
    );
}

/// R2: 上限以内の妥当なファイル（ゼロ埋め）は従来どおり `listening on` に
/// 到達する（回帰確認）。
#[test]
fn valid_sized_mock_key_file_still_starts_listening() {
    let fixture = TempFixtureDir::new("r2");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();
    let key_path = fixture.dir.join("mock.key");
    // `SCRAM_MOCK_KEY_FILE_MIN_LEN`（32 バイト）ちょうどのゼロ埋めファイル。
    std::fs::write(&key_path, vec![0u8; 32]).expect("write mock key file");

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--auth-method",
            "scram-sha-256",
            "--scram-mock-key-file",
            key_path.to_str().expect("utf-8 path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        listening,
        "expected to reach listening state with a valid-sized mock key file"
    );
}
