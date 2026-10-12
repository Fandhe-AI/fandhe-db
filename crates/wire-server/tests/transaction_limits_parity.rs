//! `--max-transaction-statements`（Issue #1553。ポインタ: SQL-46・SQL-31）で解決した
//! `TransactionLimits` が、実際に wire 越しの明示トランザクションの `54000` 判定へ届く
//! ことを固定する回帰テスト。
//!
//! 上限値は `wire_server::transaction_limits_opt::resolve`（main.rs が CLI 引数から
//! 呼ぶのと同じ関数）の出力をそのまま `EngineCore::with_transaction_limits` へ渡して作る
//! （`batch_max_files_parity.rs` と同じ流儀。main.rs の引数走査と配線自体は
//! `wire_transaction_limits_cli.rs` が子プロセスで確認する）。
//!
//! - 下げた場合: N 文目までは成功し、N+1 文目で `54000`・`ReadyForQuery` は `'E'`
//! - 上げた場合: 既定（1,000）を超える文数でも上限ちょうどまで成功し、超過で `54000`
//! - 暗黙トランザクション（WIRE-16。BEGIN を含まない複数文メッセージ）にも同じ値が効く

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::transaction_limits_opt::{resolve, TransactionLimitsRawFlags};

use common::*;

fn spawn_with_limits(
    max_statements: &str,
) -> (std::net::TcpStream, temp_db::CleanupGuard, UserStoreFile) {
    let limits = resolve(TransactionLimitsRawFlags {
        max_duration_ms: None,
        max_statements: Some(max_statements),
    })
    .expect("valid --max-transaction-statements");
    let path = temp_db::unique_db_path("transaction-limits-parity");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_transaction_limits(limits);
    let users = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users, Arc::new(core));
    (
        authenticate_to_ready_for_query(addr, "alice", "pw-alice"),
        guard,
        users,
    )
}

/// `SELECT 1` を 1 文送り、成功応答（行 1 件・`CommandComplete`）を読み切って
/// `ReadyForQuery` の状態バイトを返す。
fn select_one_ok(stream: &mut std::net::TcpStream) -> u8 {
    send_simple_query(stream, "SELECT 1");
    let _cols = read_row_description(stream);
    let _row = read_data_row(stream);
    assert_eq!(read_command_complete(stream), "SELECT 1");
    read_ready_for_query_status(stream)
}

fn begin(stream: &mut std::net::TcpStream) {
    send_simple_query(stream, "BEGIN");
    assert_eq!(read_command_complete(stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(stream), b'T');
}

fn assert_boundary(max: u64) {
    let (mut stream, _guard, _users) = spawn_with_limits(&max.to_string());
    begin(&mut stream);
    for _ in 0..max {
        assert_eq!(select_one_ok(&mut stream), b'T');
    }
    send_simple_query(&mut stream, "SELECT 1");
    expect_error_response_with_sqlstate(&mut stream, "54000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
}

#[test]
fn lowered_statement_limit_reaches_wire_transaction() {
    assert_boundary(2);
}

#[test]
fn raised_statement_limit_above_default_reaches_wire_transaction() {
    assert_boundary(1_001);
}

#[test]
fn lowered_statement_limit_also_applies_to_implicit_transaction() {
    let (mut stream, _guard, _users) = spawn_with_limits("2");
    // 書き込みが最後以外にある 3 文のメッセージは暗黙トランザクションになり、3 文目で `54000`。
    let ins = |id: u64| {
        format!(
            "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2]') USING OPERATION_ID 'imp-{id}'"
        )
    };
    send_simple_query(&mut stream, &format!("{}; {}; {}", ins(1), ins(2), ins(3)));
    loop {
        let mut ty = [0u8; 1];
        std::io::Read::read_exact(&mut stream, &mut ty).expect("read type");
        let mut len_buf = [0u8; 4];
        std::io::Read::read_exact(&mut stream, &mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        std::io::Read::read_exact(&mut stream, &mut body).expect("read body");
        if ty[0] == b'E' {
            let text = String::from_utf8_lossy(&body);
            assert!(text.contains("54000"), "unexpected error: {text}");
            break;
        }
        assert_ne!(ty[0], b'Z', "ReadyForQuery before ErrorResponse");
    }
}
