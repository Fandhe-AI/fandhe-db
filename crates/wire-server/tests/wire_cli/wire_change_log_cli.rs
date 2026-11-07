//! `--change-log`（Issue #3067・親 #2166・RECOVER-14 ポインタ）の起動時挙動をバイナリ子プロセスで
//! 外形検証する。書き込み点の移行（#3068）と有効化（#3069）が済むまでは、指定した起動は
//! 固定文言で非 0 終了し（fail-closed）、引数値を echo せず、DB ファイルも作らない。
//! 未指定の起動は従来どおり `listening on` に到達する。

use crate::common;

use std::process::{Command, Stdio};
use std::time::Duration;

const FLAG: &str = "--change-log";
const REJECT: &str = "--change-log is not available yet";

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

/// 指定した起動は固定文言で拒否される（単体・他フラグとの併用・重複・値つきのいずれも）。
#[test]
fn recover14_change_log_flag_is_refused_until_it_is_wired() {
    for extra in [
        vec![FLAG],
        vec![FLAG, FLAG],
        vec![FLAG, "--surface", "nosql"],
        vec!["--require-operation-id", FLAG],
    ] {
        let fixture = common::TempFixtureDir::new("change-log-refused");
        common::write_empty_user_store(&fixture.users_path_str());
        let mut args = base_args(&fixture);
        args.extend(extra.iter().map(|s| s.to_string()));
        let (code, err) = run(&args);
        assert_ne!(code, Some(0), "args={extra:?}: must exit non-zero");
        assert!(err.contains(REJECT), "args={extra:?}: {err}");
        assert!(!err.contains("listening on"), "args={extra:?}: {err}");
        assert!(
            !std::path::Path::new(&fixture.db_path_str()).exists(),
            "args={extra:?}: the refusal must happen before the DB file is created"
        );
    }
}

/// 値つき・`=` 連結形も echo せずに拒否する。
#[test]
fn recover14_change_log_flag_never_echoes_a_value() {
    let fixture = common::TempFixtureDir::new("change-log-value");
    common::write_empty_user_store(&fixture.users_path_str());
    let mut args = base_args(&fixture);
    args.extend([FLAG.to_string(), "secret-value-xyz".to_string()]);
    let (code, err) = run(&args);
    assert_ne!(code, Some(0));
    assert!(err.contains(REJECT), "{err}");
    assert!(!err.contains("secret-value-xyz"), "{err}");

    let (code, err) = run(&["--change-log=true".to_string()]);
    assert_ne!(code, Some(0));
    assert!(err.contains("unknown argument"), "{err}");
}

/// 未指定の起動は従来どおり待ち受けに到達する（既存の挙動が不変）。
#[test]
fn recover14_without_the_flag_the_server_starts_as_before() {
    let fixture = common::TempFixtureDir::new("change-log-absent");
    common::write_empty_user_store(&fixture.users_path_str());
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(base_args(&fixture))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");
    let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    assert!(listening, "must reach listening state");
}
