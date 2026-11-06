//! `--stopwords-file`／`--synonyms-file`（Issue #2130・SEARCH-16 ポインタ）をバイナリ子プロセスとして
//! 起動し、CLI 引数の受理・拒否（fail-closed）を外形的に検証する結合テスト。
//!
//! - T1: 正しいファイル（空ファイルを含む）は `listening on` に到達する
//! - T2: 不正な行・上限超過・存在しないパス・ディレクトリ・非 UTF-8・値欠落・重複指定は
//!   非 0 終了・stderr にフラグ名を含む
//! - T3: stderr にファイルの中身（目印のダミー文字列）を出さない

use crate::common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const STOP: &str = "--stopwords-file";
const SYN: &str = "--synonyms-file";

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

/// 固定フィクスチャのディレクトリ内に置くファイルのパス（ユーザーストアの隣）。
fn sibling(fixture: &common::TempFixtureDir, name: &str) -> PathBuf {
    Path::new(&fixture.users_path_str()).with_file_name(name)
}

fn run_failing(args: &[String]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(args)
        .output()
        .expect("spawn wire-server");
    assert!(
        !output.status.success(),
        "args={args:?}: expected non-zero exit"
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn starts_listening(args: &[String]) -> bool {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");
    let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    listening
}

/// T1: 正しいファイル・空ファイル・片方だけの指定は起動を妨げない。
#[test]
fn valid_files_start_listening() {
    let fixture = common::TempFixtureDir::new("ta-t1");
    common::write_empty_user_store(&fixture.users_path_str());
    let stop = sibling(&fixture, "stop.txt");
    let syn = sibling(&fixture, "syn.txt");
    std::fs::write(&stop, "The\r\nand\n\n").expect("write stopwords");
    std::fs::write(&syn, "car automobile\n").expect("write synonyms");
    let empty = sibling(&fixture, "empty.txt");
    std::fs::write(&empty, "").expect("write empty");

    let cases: Vec<Vec<String>> = vec![
        vec![STOP.into(), stop.to_str().unwrap().into()],
        vec![SYN.into(), syn.to_str().unwrap().into()],
        vec![
            STOP.into(),
            stop.to_str().unwrap().into(),
            SYN.into(),
            syn.to_str().unwrap().into(),
        ],
        vec![STOP.into(), empty.to_str().unwrap().into()],
    ];
    for extra in cases {
        let mut args = base_args(&fixture);
        args.extend(extra.clone());
        assert!(starts_listening(&args), "args={extra:?}: must listen");
    }
}

/// T2: 不正な指定は非 0 終了で、stderr にフラグ名を含む。
#[test]
fn invalid_files_and_arguments_are_rejected() {
    let fixture = common::TempFixtureDir::new("ta-t2");
    common::write_empty_user_store(&fixture.users_path_str());
    let base = base_args(&fixture);

    let bad_line = sibling(&fixture, "bad_line.txt");
    std::fs::write(&bad_line, "ok\n# comment\n").unwrap();
    let two_words = sibling(&fixture, "two_words.txt");
    std::fs::write(&two_words, "the and\n").unwrap();
    let one_word_syn = sibling(&fixture, "one_word_syn.txt");
    std::fs::write(&one_word_syn, "car\n").unwrap();
    let too_big = sibling(&fixture, "too_big.txt");
    std::fs::write(&too_big, "a\n".repeat(600_000)).unwrap();
    let not_utf8 = sibling(&fixture, "not_utf8.txt");
    std::fs::write(&not_utf8, [0xff, 0xfe, 0x41]).unwrap();
    let missing = sibling(&fixture, "missing.txt");
    let dir = sibling(&fixture, "a_dir");
    std::fs::create_dir(&dir).unwrap();
    let ok = sibling(&fixture, "ok.txt");
    std::fs::write(&ok, "the\n").unwrap();

    let s = |p: &PathBuf| p.to_str().unwrap().to_string();
    let cases: Vec<(Vec<String>, &str, Option<&str>)> = vec![
        (vec![STOP.into(), s(&bad_line)], STOP, None),
        (vec![STOP.into(), s(&two_words)], STOP, None),
        (vec![SYN.into(), s(&one_word_syn)], SYN, None),
        (vec![STOP.into(), s(&too_big)], STOP, None),
        (vec![SYN.into(), s(&too_big)], SYN, None),
        (vec![STOP.into(), s(&not_utf8)], STOP, None),
        (vec![STOP.into(), s(&missing)], STOP, None),
        (vec![SYN.into(), s(&dir)], SYN, None),
        (vec![STOP.into()], STOP, None),
        (vec![SYN.into()], SYN, None),
        (
            vec![STOP.into(), s(&ok), STOP.into(), s(&ok)],
            STOP,
            Some("specified more than once"),
        ),
        (
            vec![SYN.into(), s(&ok), SYN.into(), s(&ok)],
            SYN,
            Some("specified more than once"),
        ),
    ];
    for (extra, flag, msg) in cases {
        let mut args = base.clone();
        args.extend(extra.clone());
        let stderr = run_failing(&args);
        assert!(
            stderr.contains(flag),
            "args={extra:?}: expected {flag} in stderr, got: {stderr}"
        );
        if let Some(msg) = msg {
            assert!(stderr.contains(msg), "args={extra:?}: {stderr}");
        }
    }
}

/// 置き換え先がストップワードに含まれる組み合わせは起動エラーになる。
#[test]
fn synonym_target_in_stopwords_is_rejected() {
    let fixture = common::TempFixtureDir::new("ta-t2b");
    common::write_empty_user_store(&fixture.users_path_str());
    let stop = sibling(&fixture, "stop.txt");
    let syn = sibling(&fixture, "syn.txt");
    std::fs::write(&stop, "automobile\n").unwrap();
    std::fs::write(&syn, "car automobile\n").unwrap();
    let mut args = base_args(&fixture);
    args.extend([
        STOP.to_string(),
        stop.to_str().unwrap().to_string(),
        SYN.to_string(),
        syn.to_str().unwrap().to_string(),
    ]);
    let stderr = run_failing(&args);
    assert!(stderr.contains(SYN), "{stderr}");
}

/// T3: 不正なファイルの中身（目印のダミー文字列）を stderr へ出さない。
#[test]
fn rejected_file_contents_are_not_echoed() {
    let fixture = common::TempFixtureDir::new("ta-t3");
    common::write_empty_user_store(&fixture.users_path_str());
    let marker = sibling(&fixture, "marker.txt");
    std::fs::write(&marker, "dummy_marker_2130 second_word third_word\n").unwrap();
    let mut args = base_args(&fixture);
    args.extend([STOP.to_string(), marker.to_str().unwrap().to_string()]);
    let stderr = run_failing(&args);
    assert!(stderr.contains(STOP), "{stderr}");
    assert!(!stderr.contains("dummy_marker_2130"), "{stderr}");
    assert!(!stderr.contains("second_word"), "{stderr}");
}
