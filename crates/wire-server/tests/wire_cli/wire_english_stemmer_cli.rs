//! `--english-stemmer`（Issue #2129・#3050・SEARCH-16 ポインタ）の起動時挙動を
//! バイナリ子プロセスで外形検証する。engine の `english-stemmer` feature 無効のビルドは常に
//! 非 0 終了し、stderr は固定文言のみ（引数値を echo しない）。有効のビルドではスイッチとして
//! 受理され（後続の未知引数で初めて失敗する）、重複指定は拒否される。

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

fn feature_on() -> bool {
    engine::sparse::analysis::english_stemmer_available()
}

const REJECT: &str = "--english-stemmer is not supported in this build";

#[test]
fn search16_english_stemmer_flag_follows_build_feature() {
    let (code, err) = run(&["--english-stemmer", "--no-such-flag-3050"]);
    assert_ne!(code, Some(0));
    if feature_on() {
        // フラグは受理され、後続の未知引数で失敗する。
        assert!(
            err.contains("unknown argument: --no-such-flag-3050"),
            "{err}"
        );
    } else {
        assert!(err.contains(REJECT), "{err}");
        assert!(!err.contains("unknown argument"), "{err}");
    }
}

#[test]
fn search16_english_stemmer_duplicate_is_rejected() {
    let (code, err) = run(&["--english-stemmer", "--english-stemmer"]);
    assert_ne!(code, Some(0));
    if feature_on() {
        assert!(err.contains("specified more than once"), "{err}");
    } else {
        assert!(err.contains(REJECT), "{err}");
    }
}

#[test]
fn search16_english_stemmer_flag_with_value_never_echoes_it_when_unavailable() {
    let (code, err) = run(&["--english-stemmer", "secret-value-xyz"]);
    assert_ne!(code, Some(0));
    if !feature_on() {
        assert!(err.contains(REJECT), "{err}");
        assert!(!err.contains("secret-value-xyz"), "{err}");
    }
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
