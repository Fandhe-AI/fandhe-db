//! PL/pgSQL 実行上限の opt-in（`--max-plpgsql-statements`・`--max-plpgsql-loop-iterations`・
//! `--max-plpgsql-nesting-depth`・`--max-plpgsql-body-bytes`・`--max-function-call-depth`。
//! Issue #2225・SQL-68）をバイナリ子プロセスとして起動し、CLI 引数の受理・拒否（fail-closed）を
//! 外形的に検証する結合テスト。`wire_recursive_cte_limits_cli.rs` と同じ流儀。
//!
//! - R1: 範囲内（下限・各ハード上限ちょうど・5 フラグ同時指定）の値は `listening on` に到達する
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含む
//! - R3: 重複指定は非 0 終了・stderr に "specified more than once" を含む
//! - R4: 範囲外（下限-1・上限+1）・非数値（先頭 `+`・空白・英字混じり・空文字・全角数字）は
//!   非 0 終了・stderr にフラグ名を含む
//!
//! 設定値が `54000`／`54001` の判定へ届くことは `plpgsql::interp_tests`（engine）の担当
//! （本ファイルは CLI 解析の外形確認）。

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリを確保し `Drop` で削除するガード。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-plpgsql-limits-cli-{label}-{}-{}-{}",
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

/// R1: 範囲内の値は起動を妨げない。
#[test]
fn in_range_values_start_listening() {
    let cases: [&[&str]; 11] = [
        &["--max-plpgsql-statements", "1"],
        &["--max-plpgsql-statements", "100000"],
        &["--max-plpgsql-loop-iterations", "1"],
        &["--max-plpgsql-loop-iterations", "1000000"],
        &["--max-plpgsql-nesting-depth", "1"],
        &["--max-plpgsql-nesting-depth", "32"],
        &["--max-plpgsql-body-bytes", "1"],
        &["--max-plpgsql-body-bytes", "65536"],
        &["--max-function-call-depth", "1"],
        &["--max-function-call-depth", "32"],
        &[
            "--max-plpgsql-statements",
            "10",
            "--max-plpgsql-loop-iterations",
            "20",
            "--max-plpgsql-nesting-depth",
            "3",
            "--max-plpgsql-body-bytes",
            "4096",
            "--max-function-call-depth",
            "4",
        ],
    ];
    for (idx, extra_args) in cases.iter().enumerate() {
        let fixture = TempFixtureDir::new(&format!("r1-{idx}"));
        let users_path = fixture.users_path_str();
        std::fs::write(&users_path, "").expect("write empty user store");
        let db_path = fixture.db_path_str();

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(*extra_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");
        let listening = wait_for_listening(&mut child);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            listening,
            "args={extra_args:?}: expected to reach listening state"
        );
    }
}

/// R2〜R4: 値欠落・重複指定・範囲外・非数値はいずれも非 0 終了・stderr にフラグ名を含む。
#[test]
fn invalid_missing_duplicate_or_out_of_range_values_are_rejected() {
    let fixture = TempFixtureDir::new("r2-r4");
    let users_path = fixture.users_path_str();
    std::fs::write(&users_path, "").expect("write empty user store");
    let db_path = fixture.db_path_str();

    // (フラグ名, 下限-1, 上限+1)
    let flags = [
        ("--max-plpgsql-statements", "0", "100001"),
        ("--max-plpgsql-loop-iterations", "0", "1000001"),
        ("--max-plpgsql-nesting-depth", "0", "33"),
        ("--max-plpgsql-body-bytes", "0", "65537"),
        ("--max-function-call-depth", "0", "33"),
    ];
    let mut cases: Vec<(&str, Vec<&str>, Option<&str>)> = Vec::new();
    for (flag, under, over) in flags {
        // R2: 値欠落。
        cases.push((flag, vec![flag], None));
        // R3: 重複指定。
        cases.push((
            flag,
            vec![flag, "5", flag, "6"],
            Some("specified more than once"),
        ));
        // R4: 範囲外・非数値。
        for bad in [under, over, "+5", " 5", "5a", "", "１２", "-1"] {
            cases.push((flag, vec![flag, bad], None));
        }
    }

    for (expected_flag, extra_args, expected_msg) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(&extra_args)
            .output()
            .expect("spawn wire-server");
        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected_flag),
            "args={extra_args:?}: expected stderr to mention {expected_flag}, got: {stderr}"
        );
        if let Some(msg) = expected_msg {
            assert!(
                stderr.contains(msg),
                "args={extra_args:?}: expected stderr to contain {msg:?}, got: {stderr}"
            );
        }
    }
}
