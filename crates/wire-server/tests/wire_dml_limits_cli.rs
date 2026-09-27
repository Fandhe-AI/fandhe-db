//! `--max-dml-affected-rows`／`--max-insert-rows` opt-in（Issue #997）を
//! バイナリ子プロセスとして起動し、CLI 引数の受理・拒否（fail-closed）を
//! 外形的に検証する結合テスト。`wire_durability_cli.rs`・
//! `wire_ddl_permission_cli.rs` と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを外形的に確認する）。
//!
//! - R1: 範囲内（`1..=1_000_000`）の値はいずれのフラグも `listening on` に
//!   到達すること（起動を妨げない）
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含むこと
//! - R3: 重複指定は非 0 終了・stderr に "specified more than once" を含むこと
//! - R4: 範囲外（`0`・`1_000_001`）・非数値（先頭 `+`・空白・英字混じり）は
//!   非 0 終了・stderr にフラグ名を含むこと
//!
//! 実際に上限値が engine 側の判定（`54000`・副作用ゼロ）へ届くことは
//! `crates/engine/tests/sql_predicate_dml_exec.rs`・
//! `crates/engine/tests/insert_multi_row.rs`（`EngineCore::with_dml_limits`
//! 経由）の担当（本ファイルは CLI 解析の外形確認に徹する）。

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（ユーザーストア・DB ファイルの
/// 置き場）を確保し、`Drop` で確実に削除するガード
/// （`wire_durability_cli.rs::TempFixtureDir` と同型）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-dml-limits-cli-{label}-{}-{}-{}",
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

/// 子プロセスの stderr を専用スレッドで読み、`listening on` を待つ
/// （`wire_durability_cli.rs::wait_for_listening` と同型）。
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
    let cases: [&[&str]; 3] = [
        &["--max-dml-affected-rows", "5"],
        &["--max-insert-rows", "5"],
        &[
            "--max-dml-affected-rows",
            "1000000",
            "--max-insert-rows",
            "1",
        ],
    ];

    for (idx, extra_args) in cases.iter().enumerate() {
        let fixture = TempFixtureDir::new(&format!("r1-{idx}"));
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
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

/// R2〜R4: 値欠落・重複指定・範囲外・非数値はいずれも非 0 終了・
/// stderr にフラグ名を含む。
#[test]
fn invalid_missing_duplicate_or_out_of_range_values_are_rejected() {
    let fixture = TempFixtureDir::new("r2-r4");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let cases: Vec<(&str, Vec<&str>)> = vec![
        // R2: 値欠落。
        ("--max-dml-affected-rows", vec!["--max-dml-affected-rows"]),
        ("--max-insert-rows", vec!["--max-insert-rows"]),
        // R3: 重複指定。
        (
            "--max-dml-affected-rows",
            vec![
                "--max-dml-affected-rows",
                "5",
                "--max-dml-affected-rows",
                "10",
            ],
        ),
        (
            "--max-insert-rows",
            vec!["--max-insert-rows", "5", "--max-insert-rows", "10"],
        ),
        // R4: 範囲外（下限未満・上限超過）。
        (
            "--max-dml-affected-rows",
            vec!["--max-dml-affected-rows", "0"],
        ),
        (
            "--max-dml-affected-rows",
            vec!["--max-dml-affected-rows", "1000001"],
        ),
        ("--max-insert-rows", vec!["--max-insert-rows", "0"]),
        ("--max-insert-rows", vec!["--max-insert-rows", "1000001"]),
        // R4: 非数値（先頭 `+`・空白・英字混じり）。
        (
            "--max-dml-affected-rows",
            vec!["--max-dml-affected-rows", "+5"],
        ),
        (
            "--max-dml-affected-rows",
            vec!["--max-dml-affected-rows", " 5"],
        ),
        (
            "--max-dml-affected-rows",
            vec!["--max-dml-affected-rows", "abc"],
        ),
        ("--max-insert-rows", vec!["--max-insert-rows", "+5"]),
    ];

    for (expected_flag, extra_args) in cases {
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
    }
}
