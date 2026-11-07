//! `--change-log`（Issue #3067・#3069・親 #2166・RECOVER-14 ポインタ）の起動時挙動をバイナリ子プロセスで
//! 外形検証する。指定した起動は変更ログを有効にして待ち受けに到達し、重複指定は固定文言で拒否する。
//! 一度有効にした DB をフラグなしで起動すると LSN の欠落を作らないよう固定文言で非 0 終了し
//! （fail-closed）、フラグ付きなら再び起動できる。未指定の起動（有効にしたことのない DB）は従来どおり
//! `listening on` に到達する。

use crate::common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const FLAG: &str = "--change-log";
const DUPLICATE: &str = "--change-log specified more than once";
const REQUIRED: &str = "this database has a change log: restart with --change-log";
const ENABLED: &str = "change log enabled";

fn base_args(fixture: &common::TempFixtureDir) -> Vec<String> {
    vec![
        "--users".to_string(),
        fixture.users_path_str(),
        "--db".to_string(),
        fixture.db_path_str(),
        "--bind".to_string(),
        "127.0.0.1:0".to_string(),
    ]
}

fn run(args: &[String]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run wire-server");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn spawn(args: &[String]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server")
}

/// 待ち受けに到達するかを確かめて子プロセスを止める。
fn reaches_listening(args: &[String]) -> bool {
    let mut child = spawn(args);
    let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    listening
}

fn with_flag(fixture: &common::TempFixtureDir) -> Vec<String> {
    let mut args = base_args(fixture);
    args.push(FLAG.to_string());
    args
}

/// 重複指定は固定文言で拒否される（DB は作られない）。
#[test]
fn recover14_change_log_flag_rejects_duplicates_before_opening_the_database() {
    let fixture = common::TempFixtureDir::new("change-log-dup");
    common::write_empty_user_store(&fixture.users_path_str());
    let mut args = base_args(&fixture);
    args.extend([FLAG.to_string(), FLAG.to_string()]);
    let (code, err) = run(&args);
    assert_ne!(code, Some(0));
    assert!(err.contains(DUPLICATE), "{err}");
    assert!(!err.contains("listening on"), "{err}");
    assert!(
        !std::path::Path::new(&fixture.db_path_str()).exists(),
        "the refusal must happen before the DB file is created"
    );
}

/// `=` 連結形は値つきのスイッチとして受理されない。
#[test]
fn recover14_change_log_flag_is_a_bare_switch() {
    let (code, err) = run(&["--change-log=true".to_string()]);
    assert_ne!(code, Some(0));
    assert!(err.contains("unknown argument"), "{err}");
}

/// 指定した起動は変更ログを有効にして待ち受けに到達し、有効化を固定文言で知らせる。
#[test]
fn recover14_change_log_flag_enables_the_log_and_listens() {
    let fixture = common::TempFixtureDir::new("change-log-enabled");
    common::write_empty_user_store(&fixture.users_path_str());
    let mut child = spawn(&with_flag(&fixture));
    let (_addr, lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        lines.iter().any(|l| l.contains(ENABLED)),
        "the enabling must be reported: {lines:?}"
    );
}

/// 一度有効にした DB をフラグなしで起動すると拒否され、フラグ付きなら再び起動できる。
#[test]
fn recover14_a_database_with_a_change_log_cannot_start_without_the_flag() {
    let fixture = common::TempFixtureDir::new("change-log-sticky");
    common::write_empty_user_store(&fixture.users_path_str());
    assert!(reaches_listening(&with_flag(&fixture)), "first enable");

    let (code, err) = run(&base_args(&fixture));
    assert_ne!(code, Some(0), "{err}");
    assert!(err.contains(REQUIRED), "{err}");
    assert!(!err.contains("listening on"), "{err}");

    assert!(reaches_listening(&with_flag(&fixture)), "re-enable");
}

/// 有効にした DB でも、`backup` サブコマンドの読み取り（一貫スナップショットの書き出し）はフラグなしで通る
/// （起動の拒否は書き込みを伴う起動だけ）。
#[test]
fn recover14_backup_of_a_database_with_a_change_log_needs_no_flag() {
    let fixture = common::TempFixtureDir::new("change-log-backup");
    common::write_empty_user_store(&fixture.users_path_str());
    assert!(reaches_listening(&with_flag(&fixture)), "enable");
    let out = format!("{}.snapshot", fixture.db_path_str());
    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(["backup", "--db", &fixture.db_path_str(), "--out", &out])
        .output()
        .expect("run backup");
    assert!(
        output.status.success(),
        "backup must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(std::path::Path::new(&out).exists(), "snapshot is written");
    let _ = std::fs::remove_file(&out);
}

/// 有効にしたことのない既存の DB に後からフラグを付けて有効にできる（フラグなしの起動も従来どおり）。
#[test]
fn recover14_the_flag_can_be_added_to_an_existing_database() {
    let fixture = common::TempFixtureDir::new("change-log-late");
    common::write_empty_user_store(&fixture.users_path_str());
    assert!(reaches_listening(&base_args(&fixture)), "plain start");
    assert!(reaches_listening(&with_flag(&fixture)), "enable late");
}

/// 未指定の起動は従来どおり待ち受けに到達する（既存の挙動が不変）。
#[test]
fn recover14_without_the_flag_the_server_starts_as_before() {
    let fixture = common::TempFixtureDir::new("change-log-absent");
    common::write_empty_user_store(&fixture.users_path_str());
    assert!(reaches_listening(&base_args(&fixture)));
}
