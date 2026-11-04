//! `--max-sparsevec-nnz`（Issue #2027・TABLE-47 (2)／SQL-46 ポインタ）をバイナリ子プロセスとして
//! 起動し、CLI 引数の受理・拒否（fail-closed）と、設定した上限が pg wire の simple query の
//! INSERT へ効くこと（`54000`）を外形的に検証する結合テスト。`wire_require_where_for_dml_cli.rs`
//! と同じ流儀（実バイナリを `CARGO_BIN_EXE_wire-server` で起動する）。
//!
//! - S1: 範囲内（下限 `1`・ハード上限 `16000`）の値は `listening on` に到達する
//! - S2: 値欠落・重複指定・範囲外・非数値は非 0 終了・stderr にフラグ名を含む
//! - S3: `3` を指定すると非ゼロ数 3 の INSERT は受理され、4 は `54000` で拒否される
//! - S4: 未指定（既定）では同じ 4 要素の INSERT が成功する

use crate::common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const FLAG: &str = "--max-sparsevec-nnz";

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

/// S1: 範囲内の値は起動を妨げない。
#[test]
fn in_range_values_start_listening() {
    for value in ["1", "3", "16000"] {
        let fixture = common::TempFixtureDir::new("s1");
        common::write_empty_user_store(&fixture.users_path_str());
        let mut args = base_args(&fixture);
        args.extend([FLAG.to_string(), value.to_string()]);
        let mut child = spawn(&args);
        let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();
        assert!(listening, "value={value}: must reach listening state");
    }
}

/// S2: 値欠落・重複指定・範囲外・非数値はいずれも非 0 終了・stderr にフラグ名を含む。
#[test]
fn invalid_missing_duplicate_or_out_of_range_values_are_rejected() {
    let fixture = common::TempFixtureDir::new("s2");
    common::write_empty_user_store(&fixture.users_path_str());
    let base = base_args(&fixture);

    let mut cases: Vec<(Vec<&str>, Option<&str>)> = vec![
        (vec![FLAG], None),
        (vec![FLAG, "5", FLAG, "6"], Some("specified more than once")),
    ];
    for bad in ["0", "16001", "abc", "+5", " 5", "5a", "", "１２", "-1"] {
        cases.push((vec![FLAG, bad], None));
    }

    for (extra_args, expected_msg) in cases {
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
            stderr.contains(FLAG),
            "args={extra_args:?}: expected stderr to mention {FLAG}, got: {stderr}"
        );
        if let Some(msg) = expected_msg {
            assert!(
                stderr.contains(msg),
                "args={extra_args:?}: expected stderr to contain {msg:?}, got: {stderr}"
            );
        }
    }
}

/// 次元 100・非ゼロ数 `n` の SPARSEVEC リテラル（引用符込み）。
fn lit(n: usize) -> String {
    let parts: Vec<String> = (1..=n).map(|i| format!("{i}:1")).collect();
    format!("'{{{}}}/100'", parts.join(","))
}

/// S3／S4 共通: サーバーを起動し、表を作って非ゼロ数 3・4 の INSERT の結果を返す。
fn insert_results(extra: &[&str]) -> (String, Option<String>) {
    let fixture = common::TempFixtureDir::new("s3");
    write_user_store_with_alice(&fixture.users_path_str());
    let mut args = base_args(&fixture);
    args.extend(
        ["--ddl-allowed-users", "alice"]
            .iter()
            .map(|s| s.to_string()),
    );
    args.extend(extra.iter().map(|s| s.to_string()));
    let mut child = spawn(&args);
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    let mut stream = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut stream,
        "CREATE TABLE sv (id BIGINT PRIMARY KEY, v SPARSEVEC(100))",
    );
    let _ = common::read_command_complete(&mut stream);
    common::read_ready_for_query(&mut stream);

    common::send_simple_query(
        &mut stream,
        &format!("INSERT INTO sv (id, v) VALUES (1, {})", lit(3)),
    );
    let at_limit = common::read_command_complete(&mut stream);
    common::read_ready_for_query(&mut stream);

    common::send_simple_query(
        &mut stream,
        &format!("INSERT INTO sv (id, v) VALUES (2, {})", lit(4)),
    );
    // 4 要素の結果は成功（既定）か 54000（設定時）のどちらか。呼び出し側が期待を検証する。
    let over = try_read_outcome(&mut stream);
    let _ = child.kill();
    let _ = child.wait();
    (at_limit, over)
}

/// 次のメッセージが CommandComplete なら `None`、ErrorResponse なら SQLSTATE を返し、
/// ReadyForQuery まで読み進める。
fn try_read_outcome(stream: &mut std::net::TcpStream) -> Option<String> {
    use std::io::Read;
    let mut ty = [0u8; 1];
    stream.read_exact(&mut ty).expect("read type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = usize::try_from(i32::from_be_bytes(len_buf)).expect("non-negative len");
    let mut body = vec![0u8; len.saturating_sub(4)];
    stream.read_exact(&mut body).expect("read body");
    let outcome = match ty[0] {
        b'C' => None,
        b'E' => {
            // ErrorResponse は `<field byte><cstring>` の列。SQLSTATE はフィールド `C`。
            let state = body
                .split(|b| *b == 0)
                .find(|f| f.first() == Some(&b'C'))
                .map(|f| String::from_utf8_lossy(&f[1..]).to_string());
            Some(state.expect("SQLSTATE field"))
        }
        other => panic!("unexpected message type {other}"),
    };
    common::read_ready_for_query(stream);
    outcome
}

/// S3: `--max-sparsevec-nnz 3` では 3 要素は受理され、4 要素は `54000`。
#[test]
fn configured_limit_applies_to_simple_query_insert() {
    let (at_limit, over) = insert_results(&[FLAG, "3"]);
    assert_eq!(at_limit, "INSERT 0 1");
    assert_eq!(over.as_deref(), Some("54000"));
}

/// S4: 未指定（既定）では 4 要素の INSERT も成功する。
#[test]
fn default_accepts_four_elements() {
    let (at_limit, over) = insert_results(&[]);
    assert_eq!(at_limit, "INSERT 0 1");
    assert_eq!(over, None);
}
