//! `--max-transaction-duration-ms`／`--max-transaction-statements` opt-in（Issue #1553。
//! ポインタ: SQL-46・SQL-31）をバイナリ子プロセスとして起動し、CLI 引数の受理・拒否
//! （fail-closed）を外形的に検証する結合テスト。`tests/common/mod.rs` の子プロセス起動
//! ヘルパー（`SpawnedServer`・`run_wire_server_to_exit`）を使う。
//!
//! - R1: 範囲内（下限・ハード上限ちょうど）の値は `listening on` に到達する
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含む
//! - R3: 重複指定は非 0 終了・stderr に "specified more than once" を含む
//! - R4: 範囲外（下限-1・上限+1）・非数値・`u32` 溢れは非 0 終了・stderr にフラグ名を含む
//! - R5: 明示指定した持続時間が 30 秒を超えるときだけ `WARNING` 行が出る
//!
//! 設定値が `54000` 判定へ届くことは `transaction_limits_parity.rs`（wire 越し）と
//! `crates/engine/tests/transaction_limits.rs`（engine）の担当。

#[path = "common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use common::*;

const DURATION_FLAG: &str = "--max-transaction-duration-ms";
const STATEMENTS_FLAG: &str = "--max-transaction-statements";

/// 起動に成功するか確認し、stop 後の stderr 全行を返す。
fn start_and_collect(label: &str, extra: &[&str]) -> Vec<String> {
    let fixture = TempFixtureDir::new(label);
    let users = fixture.users_path_str();
    write_empty_user_store(&users);
    let db = fixture.db_path_str();
    let mut args = vec!["--users", &users, "--db", &db, "--bind", "127.0.0.1:0"];
    args.extend_from_slice(extra);
    let mut server = SpawnedServer::spawn(&args);
    let addr = server.wait_for_listening(Instant::now() + Duration::from_secs(15));
    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(2));
    assert!(
        addr.is_some(),
        "did not reach listening: {extra:?}: {lines:?}"
    );
    lines
}

/// 起動前に拒否されることを確認し、stderr 全体を返す。
fn run_rejected(label: &str, extra: &[&str]) -> String {
    let fixture = TempFixtureDir::new(label);
    let users = fixture.users_path_str();
    write_empty_user_store(&users);
    let db = fixture.db_path_str();
    let mut args = vec!["--users", &users, "--db", &db, "--bind", "127.0.0.1:0"];
    args.extend_from_slice(extra);
    let out = run_wire_server_to_exit(&args);
    assert!(!out.status.success(), "expected failure: {extra:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn in_range_values_start_listening() {
    let cases: [&[&str]; 5] = [
        &[DURATION_FLAG, "100"],
        &[DURATION_FLAG, "60000"],
        &[STATEMENTS_FLAG, "1"],
        &[STATEMENTS_FLAG, "10000"],
        &[DURATION_FLAG, "20000", STATEMENTS_FLAG, "1000"],
    ];
    for (idx, extra) in cases.iter().enumerate() {
        // 警告の有無は `warning_only_for_explicit_duration_over_write_lock_wait` が担当。
        start_and_collect(&format!("r1-{idx}"), extra);
    }
}

#[test]
fn missing_value_is_rejected() {
    for flag in [DURATION_FLAG, STATEMENTS_FLAG] {
        let err = run_rejected("r2", &[flag]);
        assert!(err.contains(flag), "{flag}: {err}");
    }
}

#[test]
fn duplicate_flag_is_rejected() {
    for (flag, v) in [(DURATION_FLAG, "500"), (STATEMENTS_FLAG, "5")] {
        let err = run_rejected("r3", &[flag, v, flag, v]);
        assert!(err.contains("specified more than once"), "{flag}: {err}");
        assert!(err.contains(flag), "{flag}: {err}");
    }
}

#[test]
fn out_of_range_and_malformed_values_are_rejected() {
    let bad_common = ["+5", " 5", "abc", "", "１２", "4294967296"];
    for bad in bad_common.iter().chain(["0", "99", "60001"].iter()) {
        let err = run_rejected("r4-d", &[DURATION_FLAG, bad]);
        assert!(err.contains(DURATION_FLAG), "{bad}: {err}");
    }
    for bad in bad_common.iter().chain(["0", "10001"].iter()) {
        let err = run_rejected("r4-s", &[STATEMENTS_FLAG, bad]);
        assert!(err.contains(STATEMENTS_FLAG), "{bad}: {err}");
    }
}

#[test]
fn warning_only_for_explicit_duration_over_write_lock_wait() {
    let over = start_and_collect("r5-over", &[DURATION_FLAG, "45000"]);
    assert!(
        over.iter()
            .any(|l| l.contains("WARNING") && l.contains(DURATION_FLAG)),
        "expected warning: {over:?}"
    );
    let at = start_and_collect("r5-at", &[DURATION_FLAG, "30000"]);
    assert!(
        !at.iter()
            .any(|l| l.contains("WARNING") && l.contains(DURATION_FLAG)),
        "unexpected warning: {at:?}"
    );
    let none = start_and_collect("r5-none", &[]);
    assert!(
        !none
            .iter()
            .any(|l| l.contains("WARNING") && l.contains(DURATION_FLAG)),
        "unexpected warning: {none:?}"
    );
}
