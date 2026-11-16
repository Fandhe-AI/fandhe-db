//! `--max-dml-affected-rows`／`--max-insert-rows` opt-in（Issue #997）を
//! バイナリ子プロセスとして起動し、CLI 引数の受理・拒否（fail-closed）を
//! 外形的に検証する結合テスト。`wire_durability_cli.rs`・
//! `wire_ddl_permission_cli.rs` と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを外形的に確認する）。
//!
//! - R1: 範囲内（Issue #1128 で分割実行 DML の 3 フラグ、Issue #2073 でチャンク間隔・時間上限の 2 フラグも同じ R1〜R4 に含める）（`1..=1_000_000`）の値はいずれのフラグも `listening on` に
//!   到達すること（起動を妨げない）
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含むこと
//! - R3: 重複指定は非 0 終了・stderr に "specified more than once" を含むこと
//! - R4: 範囲外（`0`・`1_000_001`）・非数値（先頭 `+`・空白・英字混じり）は
//!   非 0 終了・stderr にフラグ名を含むこと
//! - R5（codex-review P1 指摘・PR #1122 対応。Issue #3310 で ④ 基準へ改訂）:
//!   `--max-insert-rows` を `batch_limits.max_batch_chunks`（④。既定 4096）超に設定
//!   すると起動ログへ `WARNING` 行が出ること・環境変数 `FANDHE_DB_BATCH_MAX_CHUNKS` で
//!   ④ を引き上げれば同じ `--max-insert-rows` 値でも `WARNING` が出ないこと・
//!   ① `max_files_per_batch`（既定 64）は行の形に効かないため警告に影響しないこと
//!   （いずれも `listening on` には到達する。
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
    let cases: [&[&str]; 9] = [
        &["--max-dml-affected-rows", "5"],
        &["--max-insert-rows", "5"],
        // Issue #1128: 分割実行 DML の 3 フラグ（範囲の両端を含む）。
        &[
            "--partitioned-dml-chunk-rows",
            "1",
            "--partitioned-dml-scan-budget",
            "1000000",
            "--partitioned-dml-max-hold-ms",
            "100",
        ],
        &["--partitioned-dml-max-hold-ms", "5000"],
        // Issue #2073: チャンク間隔・1 回の実行の時間上限（0 は「待たない」「無効」として受理。
        // 範囲の両端）。
        &[
            "--partitioned-dml-chunk-interval-ms",
            "0",
            "--partitioned-dml-max-run-ms",
            "0",
        ],
        &[
            "--partitioned-dml-chunk-interval-ms",
            "60000",
            "--partitioned-dml-max-run-ms",
            "86400000",
        ],
        // Issue #1129: 同時実行数・中断記録数の 3 フラグ（範囲の両端と、テナント単位 = 全体）。
        &[
            "--partitioned-dml-max-jobs-per-tenant",
            "64",
            "--partitioned-dml-max-jobs",
            "64",
            "--partitioned-dml-max-interrupted-records",
            "1000000",
        ],
        &[
            "--partitioned-dml-max-jobs-per-tenant",
            "1",
            "--partitioned-dml-max-jobs",
            "1",
            "--partitioned-dml-max-interrupted-records",
            "1",
        ],
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
        // Issue #1128: 分割実行 DML の 3 フラグ（値欠落・重複・範囲外・非数値）。
        (
            "--partitioned-dml-chunk-rows",
            vec!["--partitioned-dml-chunk-rows"],
        ),
        (
            "--partitioned-dml-scan-budget",
            vec!["--partitioned-dml-scan-budget"],
        ),
        (
            "--partitioned-dml-max-hold-ms",
            vec!["--partitioned-dml-max-hold-ms"],
        ),
        (
            "--partitioned-dml-chunk-rows",
            vec![
                "--partitioned-dml-chunk-rows",
                "5",
                "--partitioned-dml-chunk-rows",
                "10",
            ],
        ),
        (
            "--partitioned-dml-scan-budget",
            vec![
                "--partitioned-dml-scan-budget",
                "5",
                "--partitioned-dml-scan-budget",
                "10",
            ],
        ),
        (
            "--partitioned-dml-max-hold-ms",
            vec![
                "--partitioned-dml-max-hold-ms",
                "200",
                "--partitioned-dml-max-hold-ms",
                "300",
            ],
        ),
        (
            "--partitioned-dml-chunk-rows",
            vec!["--partitioned-dml-chunk-rows", "0"],
        ),
        (
            "--partitioned-dml-chunk-rows",
            vec!["--partitioned-dml-chunk-rows", "1000001"],
        ),
        (
            "--partitioned-dml-scan-budget",
            vec!["--partitioned-dml-scan-budget", "0"],
        ),
        (
            "--partitioned-dml-scan-budget",
            vec!["--partitioned-dml-scan-budget", "1000001"],
        ),
        (
            "--partitioned-dml-max-hold-ms",
            vec!["--partitioned-dml-max-hold-ms", "99"],
        ),
        (
            "--partitioned-dml-max-hold-ms",
            vec!["--partitioned-dml-max-hold-ms", "5001"],
        ),
        (
            "--partitioned-dml-chunk-rows",
            vec!["--partitioned-dml-chunk-rows", "+5"],
        ),
        (
            "--partitioned-dml-max-hold-ms",
            vec!["--partitioned-dml-max-hold-ms", "abc"],
        ),
        // Issue #2073: チャンク間隔・時間上限（値欠落・重複・範囲外・非数値）。
        (
            "--partitioned-dml-chunk-interval-ms",
            vec!["--partitioned-dml-chunk-interval-ms"],
        ),
        (
            "--partitioned-dml-chunk-interval-ms",
            vec![
                "--partitioned-dml-chunk-interval-ms",
                "10",
                "--partitioned-dml-chunk-interval-ms",
                "20",
            ],
        ),
        (
            "--partitioned-dml-max-run-ms",
            vec!["--partitioned-dml-max-run-ms"],
        ),
        (
            "--partitioned-dml-max-run-ms",
            vec![
                "--partitioned-dml-max-run-ms",
                "10",
                "--partitioned-dml-max-run-ms",
                "20",
            ],
        ),
        (
            "--partitioned-dml-chunk-interval-ms",
            vec!["--partitioned-dml-chunk-interval-ms", "60001"],
        ),
        (
            "--partitioned-dml-chunk-interval-ms",
            vec!["--partitioned-dml-chunk-interval-ms", "+5"],
        ),
        (
            "--partitioned-dml-chunk-interval-ms",
            vec!["--partitioned-dml-chunk-interval-ms", "abc"],
        ),
        (
            "--partitioned-dml-max-run-ms",
            vec!["--partitioned-dml-max-run-ms", "86400001"],
        ),
        (
            "--partitioned-dml-max-run-ms",
            vec!["--partitioned-dml-max-run-ms", "+5"],
        ),
        (
            "--partitioned-dml-max-run-ms",
            vec!["--partitioned-dml-max-run-ms", "abc"],
        ),
        // Issue #1129: 同時実行数・中断記録数の 3 フラグ（値欠落・重複・範囲外・非数値）。
        (
            "--partitioned-dml-max-jobs-per-tenant",
            vec!["--partitioned-dml-max-jobs-per-tenant"],
        ),
        (
            "--partitioned-dml-max-jobs",
            vec!["--partitioned-dml-max-jobs"],
        ),
        (
            "--partitioned-dml-max-interrupted-records",
            vec!["--partitioned-dml-max-interrupted-records"],
        ),
        (
            "--partitioned-dml-max-jobs-per-tenant",
            vec![
                "--partitioned-dml-max-jobs-per-tenant",
                "1",
                "--partitioned-dml-max-jobs-per-tenant",
                "2",
            ],
        ),
        (
            "--partitioned-dml-max-jobs",
            vec![
                "--partitioned-dml-max-jobs",
                "2",
                "--partitioned-dml-max-jobs",
                "3",
            ],
        ),
        (
            "--partitioned-dml-max-interrupted-records",
            vec![
                "--partitioned-dml-max-interrupted-records",
                "2",
                "--partitioned-dml-max-interrupted-records",
                "3",
            ],
        ),
        (
            "--partitioned-dml-max-jobs-per-tenant",
            vec!["--partitioned-dml-max-jobs-per-tenant", "0"],
        ),
        (
            "--partitioned-dml-max-jobs-per-tenant",
            vec!["--partitioned-dml-max-jobs-per-tenant", "65"],
        ),
        (
            "--partitioned-dml-max-jobs",
            vec!["--partitioned-dml-max-jobs", "0"],
        ),
        (
            "--partitioned-dml-max-jobs",
            vec!["--partitioned-dml-max-jobs", "65"],
        ),
        (
            "--partitioned-dml-max-interrupted-records",
            vec!["--partitioned-dml-max-interrupted-records", "0"],
        ),
        (
            "--partitioned-dml-max-interrupted-records",
            vec!["--partitioned-dml-max-interrupted-records", "1000001"],
        ),
        (
            "--partitioned-dml-max-jobs",
            vec!["--partitioned-dml-max-jobs", "+2"],
        ),
        // 組合せ制約: テナント単位（5）が既定の全体上限（4）を超える構成は起動拒否。
        (
            "partitioned DML",
            vec!["--partitioned-dml-max-jobs-per-tenant", "5"],
        ),
        // 全体だけを 1 へ下げても、既定のテナント単位（1）と一致するので受理される。
        // （ここでは拒否ケースのみ列挙）テナント単位 3 > 全体 2 は起動拒否。
        (
            "partitioned DML",
            vec![
                "--partitioned-dml-max-jobs-per-tenant",
                "3",
                "--partitioned-dml-max-jobs",
                "2",
            ],
        ),
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

/// R5: `--max-insert-rows` を `batch_limits.max_batch_chunks`（④。既定 4096）超に
/// 設定すると起動ログへ `WARNING` 行が出る（`listening on` には到達する）。警告は
/// ④ の環境変数名だけを案内し、① の `FANDHE_DB_BATCH_MAX_FILES` は案内しない。
/// 環境変数 `FANDHE_DB_BATCH_MAX_CHUNKS` で ④ を同じ値まで引き上げれば `WARNING` は
/// 出ない。`--max-insert-rows 100`（① の既定 64 超・④ 以下）は警告しない（Issue #3310。
/// ① は行の形に効かない）。子プロセスの環境変数は `Command::env` 経由で設定するため、
/// 他テストとのグローバル環境変数の競合は起こらない。
#[test]
fn max_insert_rows_over_chunks_cap_emits_warning_unless_env_raised() {
    let run = |label: &str, rows: &str, chunks_env: Option<&str>| -> Vec<String> {
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
            "--max-insert-rows",
            rows,
        ])
        .env_remove("FANDHE_DB_BATCH_MAX_FILES")
        .env_remove("FANDHE_DB_BATCH_MAX_TOTAL_BYTES")
        .env_remove("FANDHE_DB_BATCH_MAX_CHUNKS")
        .env_remove("VECTOR_DB_BATCH_MAX_FILES")
        .env_remove("VECTOR_DB_BATCH_MAX_TOTAL_BYTES")
        .env_remove("VECTOR_DB_BATCH_MAX_CHUNKS")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        if let Some(v) = chunks_env {
            cmd.env("FANDHE_DB_BATCH_MAX_CHUNKS", v);
        }
        let mut child = cmd.spawn().expect("spawn wire-server");
        let lines = wait_for_listening_lines(&mut child);
        let _ = child.kill();
        let _ = child.wait();
        lines
    };

    // ケース 1: ④ 既定 4096 を超える → WARNING（④ だけを案内する）。
    let lines = run("r5-warning", "5000", None);
    assert!(
        lines.iter().any(|l| l.contains("WARNING")
            && l.contains("--max-insert-rows")
            && l.contains("FANDHE_DB_BATCH_MAX_CHUNKS")
            && l.contains("4096")
            && !l.contains("FANDHE_DB_BATCH_MAX_FILES")),
        "expected a chunks-only WARNING, got: {lines:?}"
    );

    // ケース 2: ④ を 5000 へ引き上げ済み → WARNING なし。
    let lines = run("r5-no-warning", "5000", Some("5000"));
    assert!(
        !lines.iter().any(|l| l.contains("WARNING")),
        "expected no WARNING once FANDHE_DB_BATCH_MAX_CHUNKS raises the cap, got: {lines:?}"
    );

    // ケース 3（回帰）: ① の既定 64 超でも ④ 以下なら警告しない。
    let lines = run("r5-files-cap-ignored", "100", None);
    assert!(
        !lines.iter().any(|l| l.contains("WARNING")),
        "max_files_per_batch must not trigger the warning, got: {lines:?}"
    );
}

/// `--batch-max-files`（Issue #1166）付きで起動し、`listening on` までの
/// stderr 全行を返す。`env` は子プロセス単位で与える（テストプロセス自身の
/// 環境は変えない）。`FANDHE_DB_BATCH_MAX_CHUNKS` は常に除去して既定に固定する。
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
    .env_remove("FANDHE_DB_BATCH_MAX_FILES")
    .env_remove("FANDHE_DB_BATCH_MAX_TOTAL_BYTES")
    .env_remove("FANDHE_DB_BATCH_MAX_CHUNKS")
    .env_remove("VECTOR_DB_BATCH_MAX_FILES")
    .env_remove("VECTOR_DB_BATCH_MAX_TOTAL_BYTES")
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

/// B5: `--batch-max-files`（①）を引き上げても ④ 超の警告は消えない（① は行の形に効かない。
/// Issue #3310）。
#[test]
fn batch_max_files_flag_does_not_silence_insert_rows_warning() {
    let lines = listening_lines_with(
        "b5",
        &["--max-insert-rows", "5000", "--batch-max-files", "10000"],
        &[],
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("WARNING") && l.contains("capped at 4096 rows")),
        "expected WARNING, got: {lines:?}"
    );
}

/// B6: `--batch-max-files`／環境変数 `FANDHE_DB_BATCH_MAX_FILES` は行の形の警告に
/// 影響しない。両方を指定しても起動し、④ 以下の `--max-insert-rows` では WARNING なし。
/// CLI > 環境変数の優先順位自体は単体テスト（`resolve_batch_limits_*`）が担当する。
#[test]
fn batch_max_files_flag_and_env_do_not_affect_insert_rows_warning() {
    let lines = listening_lines_with(
        "b6",
        &["--max-insert-rows", "50", "--batch-max-files", "10"],
        &[("FANDHE_DB_BATCH_MAX_FILES", "100")],
    );
    assert!(
        !lines.iter().any(|l| l.contains("WARNING")),
        "unexpected WARNING: {lines:?}"
    );
}

/// B9: 旧名 `VECTOR_DB_BATCH_MAX_CHUNKS` だけを設定しても値は適用されず（既定 4096 の
/// ままなので 5000 行の `--max-insert-rows` は WARNING になる）、旧名ごとに値を含まない
/// 移行警告が 1 行出る。起動は止まらない。
#[test]
fn legacy_batch_env_is_ignored_with_warning() {
    let lines = listening_lines_with(
        "b9",
        &["--max-insert-rows", "5000"],
        &[
            ("VECTOR_DB_BATCH_MAX_FILES", "100"),
            ("VECTOR_DB_BATCH_MAX_CHUNKS", "9999"),
        ],
    );
    for (old, new) in [
        ("VECTOR_DB_BATCH_MAX_FILES", "FANDHE_DB_BATCH_MAX_FILES"),
        ("VECTOR_DB_BATCH_MAX_CHUNKS", "FANDHE_DB_BATCH_MAX_CHUNKS"),
    ] {
        let expected = format!("warning: {old} is no longer read; use {new}");
        let hits: Vec<_> = lines.iter().filter(|l| l.contains(&expected)).collect();
        assert_eq!(
            hits.len(),
            1,
            "expected one migration warning for {old}: {lines:?}"
        );
        assert!(!hits[0].contains("9999"), "warning must not echo the value");
    }
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("VECTOR_DB_BATCH_MAX_TOTAL_BYTES")),
        "unset legacy var must not warn: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("WARNING") && l.contains("capped at 4096 rows")),
        "legacy value must not raise the limit, got: {lines:?}"
    );
}

/// B8: WARNING は chunks 側（④。既定 4096）の上限と環境変数名を案内する。
#[test]
fn insert_rows_warning_reports_chunks_cap_when_effective() {
    let lines = listening_lines_with(
        "b8",
        &["--max-insert-rows", "5000", "--batch-max-files", "10000"],
        &[],
    );
    assert!(
        lines.iter().any(|l| l.contains("WARNING")
            && l.contains("FANDHE_DB_BATCH_MAX_CHUNKS")
            && l.contains("4096")),
        "expected chunks-side WARNING, got: {lines:?}"
    );
}

/// 分割実行 DML の同時実行数の上限値（engine 側 `MAX_PARTITIONED_JOBS`）は、最大接続数
/// （`limits::MAX_CONNECTIONS`）と同じ値に揃える（Issue #1129。engine から wire-server へ
/// 依存させないため、一致をここで固定する）。
#[test]
fn partitioned_max_jobs_ceiling_matches_max_connections() {
    assert_eq!(
        engine::sql::parser::MAX_PARTITIONED_JOBS,
        wire_server::limits::MAX_CONNECTIONS
    );
}
