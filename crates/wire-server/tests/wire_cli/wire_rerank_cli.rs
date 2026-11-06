//! `--reranker`／`--rerank-window`／`--rerank-mmr-lambda`（Issue #2142・#2143。ビヘイビア
//! SEARCH-15 のポインタ）をバイナリ
//! 子プロセスとして起動し、起動の受理と拒否（fail-closed）を外形的に検証する。
//!
//! - R1: 未指定・`off`・`lexical`・`mmr`（窓幅・λ の境界を含む）は `listening on` に到達する
//! - R2: 語彙外・未対応（`cross_encoder`）・範囲外・非数値・値欠落・重複指定・
//!   `off` のままの `--rerank-window`・`mmr` 以外での λ 指定・λ の範囲外／不正書式は
//!   非 0 終了で、stderr にフラグ名や理由を含む

use crate::common;

use std::process::{Command, Stdio};
use std::time::Duration;

const RERANKER: &str = "--reranker";
const WINDOW: &str = "--rerank-window";
const LAMBDA: &str = "--rerank-mmr-lambda";

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

/// R1: 受理される指定は起動を妨げない。
#[test]
fn valid_rerank_flags_start_listening() {
    let cases: [&[&str]; 11] = [
        &[],
        &[RERANKER, "off"],
        &[RERANKER, "lexical"],
        &[RERANKER, "lexical", WINDOW, "50"],
        &[RERANKER, "lexical", WINDOW, "10000"],
        &[RERANKER, "mmr"],
        &[RERANKER, "mmr", WINDOW, "50"],
        &[RERANKER, "mmr", LAMBDA, "0"],
        &[RERANKER, "mmr", LAMBDA, "1"],
        &[RERANKER, "mmr", LAMBDA, "0.25", WINDOW, "100"],
        &[LAMBDA, "0.5", RERANKER, "mmr"],
    ];
    for (n, extra) in cases.iter().enumerate() {
        let fixture = common::TempFixtureDir::new(&format!("rr-ok-{n}"));
        common::write_empty_user_store(&fixture.users_path_str());
        let mut args = base_args(&fixture);
        args.extend(extra.iter().map(|s| (*s).to_string()));
        assert!(starts_listening(&args), "args={extra:?}: must listen");
    }
}

/// R2: 不正な指定は起動エラー。
#[test]
fn invalid_rerank_flags_are_rejected() {
    let cases: [(&[&str], &str); 27] = [
        (&[RERANKER], RERANKER),
        (
            &[RERANKER, "lexical", RERANKER, "off"],
            "specified more than once",
        ),
        (
            &[RERANKER, "lexical", WINDOW, "5", WINDOW, "6"],
            "specified more than once",
        ),
        (&[RERANKER, "bogus"], RERANKER),
        (&[RERANKER, "Lexical"], RERANKER),
        (&[RERANKER, ""], RERANKER),
        (&[RERANKER, "identity"], RERANKER),
        (&[RERANKER, "cross_encoder"], "not supported"),
        (&[RERANKER, "lexical", WINDOW], WINDOW),
        (&[RERANKER, "lexical", WINDOW, "0"], WINDOW),
        (&[RERANKER, "lexical", WINDOW, "10001"], WINDOW),
        (&[RERANKER, "lexical", WINDOW, "-1"], WINDOW),
        (&[RERANKER, "lexical", WINDOW, "+5"], WINDOW),
        (&[RERANKER, "lexical", WINDOW, "abc"], WINDOW),
        (&[WINDOW, "10"], WINDOW),
        (&[RERANKER, "off", WINDOW, "10"], WINDOW),
        (&[RERANKER, "mmr", LAMBDA], LAMBDA),
        (
            &[RERANKER, "mmr", LAMBDA, "0.5", LAMBDA, "0.6"],
            "specified more than once",
        ),
        (&[RERANKER, "mmr", LAMBDA, "1.5"], LAMBDA),
        (&[RERANKER, "mmr", LAMBDA, "-0.1"], LAMBDA),
        (&[RERANKER, "mmr", LAMBDA, "+0.5"], LAMBDA),
        (&[RERANKER, "mmr", LAMBDA, "nan"], LAMBDA),
        (&[RERANKER, "mmr", LAMBDA, "1e-1"], LAMBDA),
        (&[RERANKER, "mmr", LAMBDA, ".5"], LAMBDA),
        (&[RERANKER, "lexical", LAMBDA, "0.5"], LAMBDA),
        (&[RERANKER, "off", LAMBDA, "0.5"], LAMBDA),
        (&[LAMBDA, "0.5"], LAMBDA),
    ];
    for (n, (extra, expected)) in cases.into_iter().enumerate() {
        let fixture = common::TempFixtureDir::new(&format!("rr-ng-{n}"));
        common::write_empty_user_store(&fixture.users_path_str());
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(base_args(&fixture))
            .args(extra)
            .output()
            .expect("spawn wire-server");
        assert!(!output.status.success(), "args={extra:?}: expected failure");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "args={extra:?}: expected {expected:?}, got: {stderr}"
        );
    }
}
