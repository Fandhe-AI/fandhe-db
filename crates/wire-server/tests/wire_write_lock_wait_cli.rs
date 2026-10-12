//! `--write-lock-wait-ms` opt-in（Issue #1554。ポインタ: SQL-46）をバイナリ子プロセスとして
//! 起動し、CLI 引数の受理・拒否（fail-closed）を外形的に検証する結合テスト。
//! `wire_join_limits_cli.rs` と同じ流儀。
//!
//! - R1: 範囲内（下限・既定・上限ちょうど）の値は `listening on` に到達する
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含む
//! - R3: 重複指定（同値でも）は非 0 終了・stderr に "specified more than once" を含む
//! - R4: 範囲外・非数値は非 0 終了・stderr にフラグ名を含む
//!
//! 設定値が `55P03` へ届くことは `crates/engine/tests/write_lock_wait.rs` と
//! `write_lock_wait_parity.rs` の担当。

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FLAG: &str = "--write-lock-wait-ms";

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリを確保し `Drop` で削除するガード。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-write-lock-wait-cli-{label}-{}-{}-{}",
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

fn wait_for_listening(child: &mut Child) -> bool {
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 || tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains("listening on") => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

fn base_args(fixture: &TempFixtureDir) -> Vec<String> {
    vec![
        "--users".into(),
        fixture.users_path_str(),
        "--db".into(),
        fixture.db_path_str(),
        "--bind".into(),
        "127.0.0.1:0".into(),
    ]
}

/// R1: 範囲内の値は起動を妨げない。
#[test]
fn in_range_values_start_listening() {
    for (idx, v) in ["100", "30000", "60000"].iter().enumerate() {
        let fixture = TempFixtureDir::new(&format!("r1-{idx}"));
        std::fs::write(fixture.users_path_str(), "").expect("write empty user store");
        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(base_args(&fixture))
            .args([FLAG, v])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");
        let listening = wait_for_listening(&mut child);
        let _ = child.kill();
        let _ = child.wait();
        assert!(listening, "value={v}: expected to reach listening state");
    }
}

/// R2〜R4: 値欠落・重複指定・範囲外・非数値はいずれも非 0 終了・stderr にフラグ名を含む。
#[test]
fn invalid_missing_duplicate_or_out_of_range_values_are_rejected() {
    let fixture = TempFixtureDir::new("r2-r4");
    std::fs::write(fixture.users_path_str(), "").expect("write empty user store");

    let mut cases: Vec<(Vec<&str>, Option<&str>)> = vec![
        (vec![FLAG], None),
        (
            vec![FLAG, "100", FLAG, "100"],
            Some("specified more than once"),
        ),
        (
            vec![FLAG, "100", FLAG, "200"],
            Some("specified more than once"),
        ),
    ];
    for bad in [
        "0", "99", "60001", "+5", " 5", "abc", "", "１２", "-1", "5a",
    ] {
        cases.push((vec![FLAG, bad], None));
    }

    for (extra_args, expected_msg) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(base_args(&fixture))
            .args(&extra_args)
            .output()
            .expect("spawn wire-server");
        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(FLAG),
            "args={extra_args:?}: expected stderr to mention {FLAG}, got: {stderr}"
        );
        if let Some(msg) = expected_msg {
            assert!(
                stderr.contains(msg),
                "args={extra_args:?}: expected stderr to contain {msg:?}, got: {stderr}"
            );
        }
    }
}
