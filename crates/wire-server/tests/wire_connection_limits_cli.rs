//! `--read-timeout-secs`・`--max-connections`・`--max-prepared-statements` opt-in（Issue #2454。
//! ポインタ: WIRE-25。対象 WIRE-5・WIRE-6・WIRE-11）をバイナリ子プロセスとして起動し、
//! CLI 引数の受理・拒否（fail-closed）と注入経路の配線を外形的に検証する結合テスト。
//! `wire_write_lock_wait_cli.rs` と同じ流儀。
//!
//! - R1: 範囲内（下限・既定・上限ちょうど）の値は `listening on` に到達する（NoSQL 表層も 1 ケース）
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含む
//! - R3: 重複指定（同値でも）は非 0 終了・stderr に "specified more than once" を含む
//! - R4: 範囲外・非数値は非 0 終了・stderr にフラグ名を含む
//! - R5: 配線スモーク（`--max-connections 1` で 2 本目が `53300`、`--read-timeout-secs` で
//!   無通信接続が切断される）。境界の網羅は別 Issue（#2455）の担当。

use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const READ_TIMEOUT_FLAG: &str = "--read-timeout-secs";
const MAX_CONNECTIONS_FLAG: &str = "--max-connections";
const MAX_PREPARED_STATEMENTS_FLAG: &str = "--max-prepared-statements";

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリを確保し `Drop` で削除するガード。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-connection-limits-cli-{label}-{}-{}-{}",
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

/// `listening on <addr>` 行を待ち、`<addr>` を返す（10 秒で諦める）。
fn wait_for_listening(child: &mut Child) -> Option<String> {
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
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains("listening on") => {
                return line
                    .split("listening on")
                    .nth(1)
                    .map(|a| a.trim().to_string());
            }
            Ok(_) => continue,
            Err(_) => return None,
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

fn spawn(fixture: &TempFixtureDir, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(base_args(fixture))
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server")
}

/// R1: 範囲内の値は起動を妨げない（SQL 表層。NoSQL 表層は別ケース）。
#[test]
fn in_range_values_start_listening() {
    let cases: [(&str, [&str; 3]); 3] = [
        (READ_TIMEOUT_FLAG, ["1", "30", "600"]),
        (MAX_CONNECTIONS_FLAG, ["1", "64", "256"]),
        (MAX_PREPARED_STATEMENTS_FLAG, ["1", "64", "1024"]),
    ];
    for (flag, values) in cases {
        for (idx, v) in values.iter().enumerate() {
            let fixture = TempFixtureDir::new(&format!("r1-{idx}"));
            std::fs::write(fixture.users_path_str(), "").expect("write empty user store");
            let mut child = spawn(&fixture, &[flag, v]);
            let listening = wait_for_listening(&mut child).is_some();
            let _ = child.kill();
            let _ = child.wait();
            assert!(listening, "{flag}={v}: expected to reach listening state");
        }
    }
}

/// R1（NoSQL 表層）: 3 フラグ同時指定でも `--surface nosql` が起動する。
#[test]
fn nosql_surface_accepts_all_flags() {
    let fixture = TempFixtureDir::new("r1-nosql");
    std::fs::write(fixture.users_path_str(), "").expect("write empty user store");
    let mut child = spawn(
        &fixture,
        &[
            "--surface",
            "nosql",
            READ_TIMEOUT_FLAG,
            "5",
            MAX_CONNECTIONS_FLAG,
            "8",
            MAX_PREPARED_STATEMENTS_FLAG,
            "8",
        ],
    );
    let listening = wait_for_listening(&mut child).is_some();
    let _ = child.kill();
    let _ = child.wait();
    assert!(listening, "expected nosql surface to reach listening state");
}

/// R2〜R4: 値欠落・重複指定・範囲外・非数値はいずれも非 0 終了・stderr にフラグ名を含む。
#[test]
fn invalid_missing_duplicate_or_out_of_range_values_are_rejected() {
    let fixture = TempFixtureDir::new("r2-r4");
    std::fs::write(fixture.users_path_str(), "").expect("write empty user store");

    let mut cases: Vec<(&str, Vec<&str>, Option<&str>)> = Vec::new();
    let out_of_range: [(&str, [&str; 2]); 3] = [
        (READ_TIMEOUT_FLAG, ["601", "9999"]),
        (MAX_CONNECTIONS_FLAG, ["0", "257"]),
        (MAX_PREPARED_STATEMENTS_FLAG, ["0", "1025"]),
    ];
    for (flag, oor) in out_of_range {
        cases.push((flag, vec![flag], None));
        cases.push((
            flag,
            vec![flag, "10", flag, "10"],
            Some("specified more than once"),
        ));
        cases.push((
            flag,
            vec![flag, "10", flag, "20"],
            Some("specified more than once"),
        ));
        for bad in oor {
            cases.push((flag, vec![flag, bad], None));
        }
        for bad in ["0", "+5", " 5", "abc", "", "１２", "-1", "5a"] {
            cases.push((flag, vec![flag, bad], None));
        }
    }

    for (flag, extra_args, expected_msg) in cases {
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
            stderr.contains(flag),
            "args={extra_args:?}: expected stderr to mention {flag}, got: {stderr}"
        );
        if let Some(msg) = expected_msg {
            assert!(
                stderr.contains(msg),
                "args={extra_args:?}: expected stderr to contain {msg:?}, got: {stderr}"
            );
        }
    }
}

/// R5: `--max-connections 1` が接続枠へ届く（1 本目が枠を保持している間、2 本目は `53300`）。
#[test]
fn max_connections_flag_reaches_the_limiter() {
    let fixture = TempFixtureDir::new("r5-conn");
    std::fs::write(fixture.users_path_str(), "").expect("write empty user store");
    let mut child = spawn(&fixture, &[MAX_CONNECTIONS_FLAG, "1"]);
    let addr = wait_for_listening(&mut child);
    let result = addr.map(|a| {
        let first = TcpStream::connect(&a).expect("first connect");
        // 1 本目が accept ループに受理され枠を確保するまで少し待つ。
        std::thread::sleep(Duration::from_millis(300));
        let mut second = TcpStream::connect(&a).expect("second connect");
        second
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut buf = Vec::new();
        let _ = second.read_to_end(&mut buf);
        drop(first);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let _ = child.kill();
    let _ = child.wait();
    let body = result.expect("server should reach listening state");
    assert!(body.contains("53300"), "expected 53300, got: {body:?}");
}

/// R5: `--read-timeout-secs` が接続へ届く（無通信の接続が既定 30 秒より十分早く切断される）。
#[test]
fn read_timeout_flag_reaches_the_connection() {
    let fixture = TempFixtureDir::new("r5-timeout");
    std::fs::write(fixture.users_path_str(), "").expect("write empty user store");
    let mut child = spawn(&fixture, &[READ_TIMEOUT_FLAG, "1"]);
    let addr = wait_for_listening(&mut child);
    let elapsed = addr.map(|a| {
        let mut conn = TcpStream::connect(&a).expect("connect");
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");
        let started = Instant::now();
        let mut buf = [0u8; 64];
        // サーバー側のタイムアウトで EOF（または応答後クローズ）になるまで読む。
        while matches!(conn.read(&mut buf), Ok(n) if n > 0) {}
        started.elapsed()
    });
    let _ = child.kill();
    let _ = child.wait();
    let elapsed = elapsed.expect("server should reach listening state");
    assert!(
        elapsed < Duration::from_secs(8),
        "idle connection should be closed by the 1s read timeout, took {elapsed:?}"
    );
}
