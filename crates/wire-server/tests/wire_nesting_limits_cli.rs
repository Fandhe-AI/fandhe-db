//! `--max-subquery-depth`／`--max-subquery-executions`／`--max-cte-definitions`／
//! `--max-cte-nesting-depth` opt-in（Issue #1552。ポインタ: SQL-46）をバイナリ子プロセスとして起動し、CLI 引数の
//! 受理・拒否（fail-closed）を外形的に検証する結合テスト。`wire_schema_limits_cli.rs`
//! と同じ流儀（実バイナリを `CARGO_BIN_EXE_wire-server` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを確認する）。
//!
//! - R1: 範囲内（下限 `1`・各ハード上限ちょうど）の値は `listening on` に到達する
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含む
//! - R3: 重複指定は非 0 終了・stderr に "specified more than once" を含む
//! - R4: 範囲外（`0`・上限+1）・非数値（先頭 `+`・空白・英字混じり・空文字・
//!   全角数字）は非 0 終了・stderr にフラグ名を含む
//!
//! 設定値がサブクエリ・CTE の判定（`54000`）へ届くことは
//! `crates/engine/tests/nesting_limits.rs` の担当（本ファイルは CLI 解析の外形確認）。

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
            "wire-server-nesting-limits-cli-{label}-{}-{}-{}",
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

/// R1: 範囲内の値はいずれのフラグも起動を妨げない。
#[test]
fn in_range_values_start_listening() {
    let cases: [&[&str]; 9] = [
        &["--max-subquery-depth", "1"],
        &["--max-subquery-depth", "4"],
        &["--max-subquery-executions", "1"],
        &["--max-subquery-executions", "16"],
        &["--max-cte-definitions", "1"],
        &["--max-cte-definitions", "16"],
        &["--max-cte-nesting-depth", "1"],
        &["--max-cte-nesting-depth", "4"],
        &[
            "--max-subquery-depth",
            "2",
            "--max-subquery-executions",
            "3",
            "--max-cte-definitions",
            "2",
            "--max-cte-nesting-depth",
            "1",
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

    // (フラグ名, 上限+1)
    let flags = [
        ("--max-subquery-depth", "5"),
        ("--max-subquery-executions", "17"),
        ("--max-cte-definitions", "17"),
        ("--max-cte-nesting-depth", "5"),
    ];
    let mut cases: Vec<(&str, Vec<&str>, Option<&str>)> = Vec::new();
    for (flag, over) in flags {
        // R2: 値欠落。
        cases.push((flag, vec![flag], None));
        // R3: 重複指定。
        cases.push((
            flag,
            vec![flag, "5", flag, "6"],
            Some("specified more than once"),
        ));
        // R4: 範囲外・非数値。
        for bad in ["0", over, "+5", " 5", "5a", "", "１２", "-1"] {
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
