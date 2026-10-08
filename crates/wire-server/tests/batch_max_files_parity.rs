//! `--batch-max-files`（Issue #1166）で解決した `BatchLimits::max_files_per_batch`
//! が、実際に engine の一括投入経路へ届くことを wire 越しに固定する回帰テスト。
//!
//! 上限値は `wire_server::dml_limits_opt::resolve_batch_limits`（main.rs が CLI
//! 引数から呼ぶのと同じ関数）の出力をそのまま `EngineCore::with_batch_limits`
//! へ渡して作るため、環境変数 `FANDHE_DB_BATCH_MAX_FILES` には依存しない
//! （ただし `base` に `BatchLimits::default()` を使うので `max_batch_chunks` 等は
//! 既定に従う）。
//!
//! - SQL 複数行 `VALUES`: 上限ちょうどは成功・上限 + 1 は `54000`・副作用ゼロ
//! - COPY FROM STDIN: 同じ値を共有し、上限 + 1 行目で `54000`・副作用ゼロ
//! - 引き上げ方向: 既定 64 より大きい値なら 65 行以上でも成功する
//!
//! NoSQL `op=insert rows[]` の上限超過（`54000`）は `nosql6_insert.rs`、ファイル形
//! バッチ（engine ローカル API・wire 経路なし）は `crates/engine/tests/
//! batch_limits.rs` が同じ `max_files_per_batch` を明示値で固定済みのため、本
//! ファイルは CLI 解決値が wire 経路へ届く点に集中する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::dml_limits_opt::resolve_batch_limits;

use common::*;

/// CLI 解決関数を通した `max_files_per_batch = raw` の `BatchLimits`。
fn limits_from_cli(raw: &str) -> BatchLimits {
    resolve_batch_limits(BatchLimits::default(), Some(raw)).expect("valid --batch-max-files")
}

fn new_core(limits: BatchLimits) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("batch-max-files-parity");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// `CopyInResponse`（'G'）を読み捨てる（`wire17_copy.rs` と同型の最小ヘルパー）。
fn read_copy_in_response(stream: &mut std::net::TcpStream) {
    use std::io::Read;
    let mut ty = [0u8; 1];
    stream.read_exact(&mut ty).expect("read type");
    assert_eq!(ty[0], b'G', "expected CopyInResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    stream.read_exact(&mut body).expect("read body");
}

fn send_copy_data(stream: &mut std::net::TcpStream, chunk: &[u8]) {
    send_length_prefixed_message(stream, b'd', chunk);
}

fn send_copy_done(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'c', b"");
}

fn multi_row_insert_sql(count: u64, op_id: &str) -> String {
    let rows: Vec<String> = (1..=count)
        .map(|id| format!("({id}, '[0.1,0.2]', 'ja')"))
        .collect();
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES {} USING OPERATION_ID '{op_id}'",
        rows.join(", ")
    )
}

/// `docs` の行数を `expected` 件として読み戻す（全行を DataRow で消費する）。
fn assert_row_count(stream: &mut std::net::TcpStream, expected: usize) {
    send_simple_query(stream, "SELECT id FROM docs LIMIT 1000");
    let _cols = read_row_description(stream);
    for _ in 0..expected {
        let _ = read_data_row(stream);
    }
    assert_eq!(read_command_complete(stream), format!("SELECT {expected}"));
    read_ready_for_query(stream);
}

#[test]
fn sql_multi_row_values_respects_cli_resolved_limit() {
    let (core, _guard) = new_core(limits_from_cli("3"));
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, &multi_row_insert_sql(4, "parity-sql-over"));
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);
    assert_row_count(&mut stream, 0);

    send_simple_query(&mut stream, &multi_row_insert_sql(3, "parity-sql-at"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 3");
    read_ready_for_query(&mut stream);
    assert_row_count(&mut stream, 3);
}

#[test]
fn sql_multi_row_values_can_exceed_default_when_raised() {
    let (core, _guard) = new_core(limits_from_cli("80"));
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, &multi_row_insert_sql(80, "parity-sql-raised"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 80");
    read_ready_for_query(&mut stream);
    assert_row_count(&mut stream, 80);
}

#[test]
fn copy_from_stdin_respects_cli_resolved_limit() {
    let (core, _guard) = new_core(limits_from_cli("3"));
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'parity-copy-over'",
    );
    read_copy_in_response(&mut stream);
    send_copy_data(
        &mut stream,
        b"1\t[1.0,0.0]\tja\n2\t[1.0,0.0]\tja\n3\t[1.0,0.0]\tja\n4\t[1.0,0.0]\tja\n",
    );
    send_copy_done(&mut stream);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);
    assert_row_count(&mut stream, 0);
}
