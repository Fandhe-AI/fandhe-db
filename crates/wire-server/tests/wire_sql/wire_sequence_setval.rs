//! `setval`／`lastval`（TABLE-24・Issue #1691）の wire 越し検証（層 A。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-24、`docs/spec/04-behavior/error-format.md` ERR-6）。
//!
//! 採番・永続化・トランザクションの意味論は `crates/engine/tests/table24_sequence_setval.rs` が
//! 確定オラクルとして検証済みのため、本ファイルは (1) 簡易クエリで結果が bigint（OID 20）の
//! 1 列 1 行として返ること (2) 範囲外・未定義の SQLSTATE（`22003`・`55000`）
//! (3) 拡張クエリ（Parse／Bind／Execute）でも同じ経路を通り、`$n` 付きは `0A000` で拒否される
//! こと、を確認する。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

fn setup() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sequence-setval");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut session = SessionState::default();
    session.allow_ddl();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");
    for sql in [
        "CREATE SEQUENCE s",
        "CREATE SEQUENCE capped MAXVALUE 10",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT)",
    ] {
        core.execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    (core, guard)
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// 簡易クエリ: 結果は列名 `setval`（別名があれば別名）・bigint の 1 行。範囲外は `22003`。
#[test]
fn simple_query_setval_and_lastval() {
    let (core, _guard) = setup();
    let mut stream = connect(Arc::clone(&core));

    send_simple_query(&mut stream, "SELECT setval('s', 5)");
    assert_eq!(
        read_row_description_with_oids(&mut stream),
        vec![("setval".to_string(), 20)]
    );
    assert_eq!(read_data_row(&mut stream), vec![Some("5".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT setval('s', 9, false) AS v");
    assert_eq!(
        read_row_description_with_oids(&mut stream),
        vec![("v".to_string(), 20)]
    );
    assert_eq!(read_data_row(&mut stream), vec![Some("9".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 範囲外は 22003。
    send_simple_query(&mut stream, "SELECT setval('capped', 11)");
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);

    // nextval 前の lastval は 55000、nextval 後は直近の値。
    send_simple_query(&mut stream, "SELECT lastval()");
    expect_error_response_with_sqlstate(&mut stream, "55000");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, n) VALUES (1, nextval('s')) RETURNING n \
         USING OPERATION_ID 'wire-setval-1'",
    );
    let _ = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("9".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT lastval()");
    assert_eq!(
        read_row_description_with_oids(&mut stream),
        vec![("lastval".to_string(), 20)]
    );
    assert_eq!(read_data_row(&mut stream), vec![Some("9".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ: Parse／Bind／Execute でも実行でき、`$n` 付きは Parse で `0A000`。
#[test]
fn extended_protocol_setval_and_bind_parameters_are_rejected() {
    let (core, _guard) = setup();
    let mut stream = connect(Arc::clone(&core));

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT setval('s', 41)", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("", ""));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
    let (kind, row) = read_message(&mut stream);
    assert_eq!(kind, b'D', "expected DataRow");
    assert!(String::from_utf8_lossy(&row).ends_with("41"));
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 1");
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");

    // `$n` は Parse で 0A000。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT setval('s', $1)", 0),
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");

    // 副作用なし: 次の nextval は 42。
    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, n) VALUES (1, nextval('s')) RETURNING n \
         USING OPERATION_ID 'wire-setval-2'",
    );
    let _ = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("42".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}
