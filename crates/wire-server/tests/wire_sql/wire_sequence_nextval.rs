//! `nextval`／`currval`（TABLE-24・Issue #1690）の wire 越し検証（層 A。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-24、`docs/spec/04-behavior/error-format.md` ERR-6）。
//!
//! 採番・台帳・ROLLBACK の意味論は `crates/engine/tests/table24_sequence_nextval.rs` が確定
//! オラクルとして検証済みのため、本ファイルは (1) 簡易クエリで `RETURNING` が払い出した値を
//! bigint（OID 20）の列として返し、接続（セッション）内で `currval` が直近値を返すこと
//! (2) 未呼び出しの `currval` が `55000`、上限到達が `2200H` の SQLSTATE で返ること
//! (3) 拡張クエリプロトコルの Execute でも同じ採番経路を通ること、を確認する。

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

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sequence-nextval");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

/// 管理用セッションで DDL を実行する（wire 経由の DDL 権限とは独立）。
fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn setup() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let (core, guard) = new_core();
    ddl(&core, "CREATE SEQUENCE s");
    ddl(&core, "CREATE SEQUENCE capped MAXVALUE 2");
    ddl(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT)");
    (core, guard)
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// 簡易クエリ: `INSERT ... RETURNING` は払い出した値を bigint 列として返し、同じ接続の
/// `currval` は直近の値を返す。
#[test]
fn simple_query_returning_reports_assigned_values_and_currval() {
    let (core, _guard) = setup();
    let mut stream = connect(Arc::clone(&core));

    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, n) VALUES (1, nextval('s')), (2, nextval('s')) RETURNING n \
         USING OPERATION_ID 'wire-seq-1'",
    );
    let columns = read_row_description_with_oids(&mut stream);
    assert_eq!(columns, vec![("n".to_string(), 20)]);
    assert_eq!(read_data_row(&mut stream), vec![Some("1".to_string())]);
    assert_eq!(read_data_row(&mut stream), vec![Some("2".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 2");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, n) VALUES (3, currval('s')) RETURNING n USING OPERATION_ID 'wire-seq-2'",
    );
    let _ = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("2".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// 未呼び出しの `currval` は `55000`、NO CYCLE の上限到達は `2200H`。
#[test]
fn error_sqlstates_for_currval_before_nextval_and_limit() {
    let (core, _guard) = setup();
    let mut stream = connect(Arc::clone(&core));

    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, n) VALUES (1, currval('s')) USING OPERATION_ID 'wire-seq-3'",
    );
    expect_error_response_with_sqlstate(&mut stream, "55000");
    read_ready_for_query(&mut stream);

    for (id, op) in [(1, "wire-seq-4"), (2, "wire-seq-5")] {
        send_simple_query(
            &mut stream,
            &format!(
                "INSERT INTO t (id, n) VALUES ({id}, nextval('capped')) USING OPERATION_ID '{op}'"
            ),
        );
        assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
        read_ready_for_query(&mut stream);
    }
    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, n) VALUES (3, nextval('capped')) USING OPERATION_ID 'wire-seq-6'",
    );
    expect_error_response_with_sqlstate(&mut stream, "2200H");
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ: Parse／Bind／Execute でも同じ採番経路を通り、値が連続する。
#[test]
fn extended_protocol_execute_assigns_values() {
    let (core, _guard) = setup();
    let mut stream = connect(Arc::clone(&core));

    for (i, expected) in ["1", "2"].iter().enumerate() {
        let sql = format!(
            "INSERT INTO t (id, n) VALUES ({}, nextval('s')) RETURNING n \
             USING OPERATION_ID 'wire-seq-ext-{i}'",
            10 + i
        );
        send_length_prefixed_message(&mut stream, b'P', &parse_body("", &sql, 0));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'1', "expected ParseComplete");
        send_length_prefixed_message(&mut stream, b'B', &bind_body("", ""));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'2', "expected BindComplete");
        send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
        let (kind, row) = read_message(&mut stream);
        assert_eq!(kind, b'D', "expected DataRow");
        let text = String::from_utf8_lossy(&row);
        assert!(
            text.ends_with(expected),
            "row {text:?} must end with {expected}"
        );
        let (kind, tag) = read_message(&mut stream);
        assert_eq!(kind, b'C');
        assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "INSERT 0 1");
        send_length_prefixed_message(&mut stream, b'S', b"");
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'Z', "expected ReadyForQuery");
    }
}
