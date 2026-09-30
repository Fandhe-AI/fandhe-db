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
//! - R5（codex-review P1 指摘・PR #1122 対応）: `--max-insert-rows` を
//!   `batch_limits.max_files_per_batch`（既定 64）超に設定すると起動ログへ
//!   `WARNING` 行が出ること・環境変数 `VECTOR_DB_BATCH_MAX_FILES` で
//!   `max_files_per_batch` を引き上げれば同じ `--max-insert-rows` 値でも
//!   `WARNING` が出ないこと（いずれも `listening on` には到達する。
//!   `wire_server::dml_limits_opt::insert_rows_cap_warning` 参照）
//!
//! 実際に上限値が engine 側の判定（`54000`・副作用ゼロ）へ届くことは
//! `crates/engine/tests/sql_predicate_dml_exec.rs`・
//! `crates/engine/tests/insert_multi_row.rs`（`EngineCore::with_dml_limits`
//! 経由）の担当（本ファイルは CLI 解析の外形確認に徹する）。
//! NoSQL 表層（HTTP）まで上限が届くことは `nosql12_affected_rows_limit.rs` が担当する。

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

/// 子プロセスの stderr を `listening on` に到達するまで全行集めて返す
/// （`wire_durability_cli.rs::wait_for_listening_addr_and_lines` と同じ理由。
/// R5 の `WARNING` 行の有無を判定するため、`listening on` の 1 行だけでなく
/// 途中の全行を保持する）。
fn wait_for_listening_lines(child: &mut Child) -> Vec<String> {
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
    let mut lines = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("did not observe listening state within timeout; lines so far: {lines:?}");
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let trimmed = line.trim_end().to_string();
                let is_listening = line.contains("listening on");
                lines.push(trimmed);
                if is_listening {
                    return lines;
                }
            }
            Err(_) => panic!(
                "stderr channel closed before observing listening state; lines so far: {lines:?}"
            ),
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

/// R5: `--max-insert-rows` を `batch_limits.max_files_per_batch`（既定 64）超に
/// 設定すると起動ログへ `WARNING` 行が出る（`listening on` には到達する）。
/// 環境変数 `VECTOR_DB_BATCH_MAX_FILES` で `max_files_per_batch` を同じ値まで
/// 引き上げれば `WARNING` は出ない（子プロセスの環境変数は
/// `Command::env` 経由で設定するため、他テストとのグローバル環境変数の
/// 競合は起こらない）。
#[test]
fn max_insert_rows_over_batch_limits_default_emits_warning_unless_env_raised() {
    // ケース 1: `VECTOR_DB_BATCH_MAX_FILES` 未設定 → 既定の 64 を超えるため WARNING。
    {
        let fixture = TempFixtureDir::new("r5-warning");
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
                "--max-insert-rows",
                "100",
            ])
            .env_remove("VECTOR_DB_BATCH_MAX_FILES")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let lines = wait_for_listening_lines(&mut child);
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            lines.iter().any(|l| l.contains("WARNING")
                && l.contains("--max-insert-rows")
                && l.contains("VECTOR_DB_BATCH_MAX_FILES")),
            "expected a WARNING line mentioning --max-insert-rows and \
             VECTOR_DB_BATCH_MAX_FILES, got: {lines:?}"
        );
    }

    // ケース 2: `VECTOR_DB_BATCH_MAX_FILES=100` で引き上げ済み → WARNING なし。
    {
        let fixture = TempFixtureDir::new("r5-no-warning");
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
                "--max-insert-rows",
                "100",
            ])
            .env("VECTOR_DB_BATCH_MAX_FILES", "100")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let lines = wait_for_listening_lines(&mut child);
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            !lines.iter().any(|l| l.contains("WARNING")),
            "expected no WARNING line once VECTOR_DB_BATCH_MAX_FILES raises the batch limit, \
             got: {lines:?}"
        );
    }
}

/// `--batch-max-files`（Issue #1166）付きで起動し、`listening on` までの
/// stderr 全行を返す。`env` は子プロセス単位で与える（テストプロセス自身の
/// 環境は変えない）。`VECTOR_DB_BATCH_MAX_CHUNKS` は常に除去して既定に固定する。
fn listening_lines_with(label: &str, extra_args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    let fixture = TempFixtureDir::new(label);
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wire-server"));
    cmd.args([
        "--users",
        &users_path,
        "--db",
        &db_path,
        "--bind",
        "127.0.0.1:0",
    ])
    .args(extra_args)
    .env_remove("VECTOR_DB_BATCH_MAX_FILES")
    .env_remove("VECTOR_DB_BATCH_MAX_CHUNKS");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");
    let lines = wait_for_listening_lines(&mut child);
    let _ = child.kill();
    let _ = child.wait();
    lines
}

/// B1: 範囲内の `--batch-max-files` は起動を妨げない。
#[test]
fn batch_max_files_in_range_starts_listening() {
    for (idx, v) in ["1", "1000000"].iter().enumerate() {
        let lines = listening_lines_with(&format!("b1-{idx}"), &["--batch-max-files", v], &[]);
        assert!(lines.iter().any(|l| l.contains("listening on")));
    }
}

/// B2〜B4: 値欠落・重複・範囲外・非数値は非 0 終了で stderr にフラグ名を含む。
#[test]
fn batch_max_files_invalid_values_are_rejected() {
    let fixture = TempFixtureDir::new("b2-b4");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["--batch-max-files"], "--batch-max-files"),
        (
            vec!["--batch-max-files", "5", "--batch-max-files", "6"],
            "specified more than once",
        ),
        (vec!["--batch-max-files", "0"], "--batch-max-files"),
        (vec!["--batch-max-files", "1000001"], "--batch-max-files"),
        (vec!["--batch-max-files", "abc"], "--batch-max-files"),
        (vec!["--batch-max-files", "+5"], "--batch-max-files"),
        (vec!["--batch-max-files", " 5"], "--batch-max-files"),
        (vec!["--batch-max-files", "5 "], "--batch-max-files"),
    ];
    for (extra_args, expected) in cases {
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
            stderr.contains(expected),
            "args={extra_args:?}: expected stderr to mention {expected}, got: {stderr}"
        );
    }
}

/// B5: `--batch-max-files` で `--max-insert-rows` 以上へ引き上げれば WARNING なし。
#[test]
fn batch_max_files_flag_silences_insert_rows_warning() {
    let lines = listening_lines_with(
        "b5",
        &["--max-insert-rows", "100", "--batch-max-files", "100"],
        &[],
    );
    assert!(
        !lines.iter().any(|l| l.contains("WARNING")),
        "unexpected WARNING: {lines:?}"
    );
}

/// B6: 優先順位は CLI 明示 > 環境変数。環境変数が 100 でも CLI の 10 が効き、
/// WARNING は CLI 側の実効上限 10 を報告する。
#[test]
fn batch_max_files_flag_takes_precedence_over_env() {
    let lines = listening_lines_with(
        "b6",
        &["--max-insert-rows", "50", "--batch-max-files", "10"],
        &[("VECTOR_DB_BATCH_MAX_FILES", "100")],
    );
    assert!(
        lines.iter().any(|l| l.contains("WARNING")
            && l.contains("--max-insert-rows")
            && l.contains("capped at 10 rows")),
        "expected WARNING reporting cap 10, got: {lines:?}"
    );
}

/// B8: files 側が十分大きくても `max_batch_chunks`（既定 4096）が実効上限に
/// なる場合、WARNING は chunks 側の上限と環境変数名を案内する。
#[test]
fn insert_rows_warning_reports_chunks_cap_when_effective() {
    let lines = listening_lines_with(
        "b8",
        &["--max-insert-rows", "5000", "--batch-max-files", "10000"],
        &[],
    );
    assert!(
        lines.iter().any(|l| l.contains("WARNING")
            && l.contains("VECTOR_DB_BATCH_MAX_CHUNKS")
            && l.contains("4096")),
        "expected chunks-side WARNING, got: {lines:?}"
    );
}
