//! `--english-stemmer`（Issue #2129 Phase A・SEARCH-16 ポインタ）の起動拒否を
//! バイナリ子プロセスで外形検証する。承認と feature 有効化の前は常に非 0 終了し、
//! stderr は固定文言のみ（引数値を echo しない）。

use std::process::{Command, Stdio};

fn run(args: &[&str]) -> (Option<i32>, String) {
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

#[test]
fn search16_english_stemmer_flag_alone_is_rejected() {
    let (code, err) = run(&["--english-stemmer"]);
    assert_ne!(code, Some(0));
    assert!(
        err.contains("--english-stemmer is not supported in this build"),
        "{err}"
    );
    assert!(!err.contains("unknown argument"), "{err}");
}

#[test]
fn search16_english_stemmer_flag_with_value_is_rejected_without_echo() {
    let (code, err) = run(&["--english-stemmer", "secret-value-xyz"]);
    assert_ne!(code, Some(0));
    assert!(
        err.contains("--english-stemmer is not supported in this build"),
        "{err}"
    );
    assert!(!err.contains("secret-value-xyz"), "{err}");
}

#[test]
fn search16_other_unknown_arguments_keep_unknown_argument_error() {
    let (code, err) = run(&["--no-such-flag-2129"]);
    assert_ne!(code, Some(0));
    assert!(
        err.contains("unknown argument: --no-such-flag-2129"),
        "{err}"
    );
}
