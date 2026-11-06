//! `--max-ts-headline-bytes`／`--max-ts-headline-fragments`（Issue #2125・SEARCH-14 ポインタ）を
//! バイナリ子プロセスとして起動し、CLI 引数の受理・拒否（fail-closed）と、設定した上限が pg wire の
//! simple query の `ts_headline` へ効くこと（`54000`）を外形的に検証する結合テスト。
//! `wire_sparsevec_limits_cli.rs` と同じ流儀（実バイナリを `CARGO_BIN_EXE_wire-server` で起動する）。
//!
//! - H1: 範囲内（下限 `1`・ハード上限）の値は `listening on` に到達する
//! - H2: 値欠落・重複指定・範囲外・非数値は非 0 終了・stderr にフラグ名を含む
//! - H3: 出力バイト長を 12 に下げると `a <b>fox</b>`（12 バイト）は成功し、
//!   11 に下げると `54000`（切り詰めない）
//! - H4: `MaxFragments` の上限を 1 に下げると `MaxFragments=2` が `54000`
//! - H5: 未指定（既定）では同じ呼び出しが成功する

use crate::common;

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

const BYTES_FLAG: &str = "--max-ts-headline-bytes";
const FRAGS_FLAG: &str = "--max-ts-headline-fragments";

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

fn spawn(args: &[String]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server")
}

/// `alice:tenant-a:<phc>` のユーザーストアを書く（ハッシュは `hash-password` サブコマンドで生成する）。
fn write_user_store_with_alice(path: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-password");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"pw-alice\n")
        .expect("write password");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success(), "hash-password must succeed");
    let phc = String::from_utf8(output.stdout).expect("utf-8 phc");
    std::fs::write(path, format!("alice:tenant-a:{}\n", phc.trim())).expect("write user store");
}

/// H1: 範囲内の値は起動を妨げない。
#[test]
fn in_range_values_start_listening() {
    for (flag, value) in [
        (BYTES_FLAG, "1"),
        (BYTES_FLAG, "4194304"),
        (FRAGS_FLAG, "1"),
        (FRAGS_FLAG, "1024"),
    ] {
        let fixture = common::TempFixtureDir::new("h1");
        common::write_empty_user_store(&fixture.users_path_str());
        let mut args = base_args(&fixture);
        args.extend([flag.to_string(), value.to_string()]);
        let mut child = spawn(&args);
        let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();
        assert!(listening, "{flag}={value}: must reach listening state");
    }
}

/// H2: 値欠落・重複指定・範囲外・非数値はいずれも非 0 終了・stderr にフラグ名を含む。
#[test]
fn invalid_missing_duplicate_or_out_of_range_values_are_rejected() {
    let fixture = common::TempFixtureDir::new("h2");
    common::write_empty_user_store(&fixture.users_path_str());
    let base = base_args(&fixture);

    let mut cases: Vec<(&str, Vec<&str>, Option<&str>)> = vec![
        (BYTES_FLAG, vec![BYTES_FLAG], None),
        (FRAGS_FLAG, vec![FRAGS_FLAG], None),
        (
            BYTES_FLAG,
            vec![BYTES_FLAG, "5", BYTES_FLAG, "6"],
            Some("specified more than once"),
        ),
        (
            FRAGS_FLAG,
            vec![FRAGS_FLAG, "5", FRAGS_FLAG, "6"],
            Some("specified more than once"),
        ),
    ];
    for bad in ["0", "4194305", "abc", "+5", " 5", "5a", "", "１２", "-1"] {
        cases.push((BYTES_FLAG, vec![BYTES_FLAG, bad], None));
    }
    for bad in ["0", "1025", "abc", "+5", ""] {
        cases.push((FRAGS_FLAG, vec![FRAGS_FLAG, bad], None));
    }

    for (flag, extra_args, expected_msg) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(&base)
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

/// 次のメッセージが DataRow なら結果テキスト（`Ok`）、ErrorResponse なら SQLSTATE（`Err`）を返し、
/// ReadyForQuery まで読み進める。RowDescription は読み飛ばす。
fn read_outcome(stream: &mut std::net::TcpStream) -> Result<String, String> {
    let mut result: Option<Result<String, String>> = None;
    loop {
        let mut ty = [0u8; 1];
        stream.read_exact(&mut ty).expect("read type");
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = usize::try_from(i32::from_be_bytes(len_buf)).expect("non-negative len");
        let mut body = vec![0u8; len.saturating_sub(4)];
        stream.read_exact(&mut body).expect("read body");
        match ty[0] {
            b'T' | b'C' => {}
            b'D' => {
                // DataRow: i16 列数, i32 長さ, 本体（1 列のみ）。
                let n = usize::try_from(i32::from_be_bytes(body[2..6].try_into().expect("len")))
                    .expect("non-negative column length");
                result = Some(Ok(String::from_utf8_lossy(&body[6..6 + n]).to_string()));
            }
            b'E' => {
                let state = body
                    .split(|b| *b == 0)
                    .find(|f| f.first() == Some(&b'C'))
                    .map(|f| String::from_utf8_lossy(&f[1..]).to_string());
                result = Some(Err(state.expect("SQLSTATE field")));
            }
            b'Z' => return result.expect("a result before ReadyForQuery"),
            other => panic!("unexpected message type {other}"),
        }
    }
}

/// サーバーを起動し、`queries` を順に simple query で実行した結果を返す。
fn run_queries(extra: &[&str], queries: &[&str]) -> Vec<Result<String, String>> {
    let fixture = common::TempFixtureDir::new("h3");
    write_user_store_with_alice(&fixture.users_path_str());
    let mut args = base_args(&fixture);
    args.extend(extra.iter().map(|s| s.to_string()));
    let mut child = spawn(&args);
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    let mut stream = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let out = queries
        .iter()
        .map(|q| {
            common::send_simple_query(&mut stream, q);
            read_outcome(&mut stream)
        })
        .collect();
    let _ = child.kill();
    let _ = child.wait();
    out
}

const SQL: &str = "SELECT ts_headline('a fox', 'fox')";
const SQL_FRAGS: &str =
    "SELECT ts_headline('a fox b fox', 'fox', 'MaxFragments=2, MaxWords=3, MinWords=1')";

/// H3: 出力バイト長の上限は、ちょうどなら成功し、1 バイト足りなければ切り詰めず `54000`。
#[test]
fn configured_output_limit_applies_to_simple_query() {
    let exact = run_queries(&[BYTES_FLAG, "12"], &[SQL]);
    assert_eq!(exact, vec![Ok("a <b>fox</b>".to_string())]);
    let short = run_queries(&[BYTES_FLAG, "11"], &[SQL]);
    assert_eq!(short, vec![Err("54000".to_string())]);
}

/// H4: `MaxFragments` の上限を下げると、超える指定は `54000`。
#[test]
fn configured_fragment_limit_applies_to_simple_query() {
    let r = run_queries(&[FRAGS_FLAG, "1"], &[SQL_FRAGS, SQL]);
    assert_eq!(r.first(), Some(&Err("54000".to_string())));
    assert_eq!(r.get(1), Some(&Ok("a <b>fox</b>".to_string())));
}

/// H5: 未指定（既定）では同じ呼び出しが成功する。
#[test]
fn default_limits_accept_the_same_calls() {
    let r = run_queries(&[], &[SQL, SQL_FRAGS]);
    assert_eq!(r.first(), Some(&Ok("a <b>fox</b>".to_string())));
    assert!(matches!(r.get(1), Some(Ok(_))), "{r:?}");
}
