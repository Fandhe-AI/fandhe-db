//! 分割実行 DML（`UPDATE`／`DELETE ... USING OPERATION_ID '<id>' PARTITIONED [CHUNK n]`）が対象表の行トリガ
//! （BEFORE／AFTER ROW）を発火することを wire 越しに通す E2E（TABLE-45・SQL-19。Issue #3283）。
//! 簡易クエリの末尾 `;` 付き単独送信（psql と同じ形）と拡張クエリの両方で、セッション入口が張る発火元の
//! 主体の写しの内側で分割実行 form が呼ばれていること（外れていれば固定文言の `0A000` になる）を確認する。
//! 文トリガのある表は `0A000`。確定オラクルは `crates/engine/tests/table_rls/table45_trigger_partitioned.rs`。
//! 設計記録は `docs/design/trigger-partitioned-dml.md`。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn serve() -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-trigger-partitioned");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    (addr, guard)
}

fn exec(s: &mut TcpStream, sql: &str) -> String {
    send_simple_query(s, sql);
    let tag = read_command_complete(s);
    read_ready_for_query(s);
    tag
}

fn select_col(s: &mut TcpStream, sql: &str) -> Vec<String> {
    send_simple_query(s, sql);
    let _ = read_row_description(s);
    let mut out = Vec::new();
    loop {
        let (kind, body) = read_message(s);
        match kind {
            b'D' => {
                let len = body
                    .get(2..6)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(i32::from_be_bytes)
                    .expect("column length");
                let n = usize::try_from(len).expect("non-null column");
                let v = body.get(6..6 + n).expect("column value");
                out.push(String::from_utf8_lossy(v).to_string());
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(s);
    out.sort();
    out
}

/// 対象表は SCHEMA `s` の `s.tt`、監査表 `audit` は public（INSERT トリガの wire E2E と同じ構成）。
fn setup(s: &mut TcpStream) {
    exec(s, "CREATE SCHEMA s");
    exec(s, "CREATE TABLE s.tt (n INTEGER)");
    exec(s, "CREATE TABLE audit (n INTEGER)");
    exec(
        s,
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.n := NEW.n + 100; RETURN NEW; END $$",
    );
    exec(
        s,
        "CREATE FUNCTION audit_new() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NULL; END $$",
    );
    exec(
        s,
        "CREATE TRIGGER tb BEFORE UPDATE ON s.tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    exec(
        s,
        "CREATE TRIGGER ta AFTER UPDATE ON s.tt FOR EACH ROW EXECUTE FUNCTION audit_new()",
    );
    exec(s, "INSERT INTO s.tt (n) VALUES (1), (2), (3)");
}

/// 簡易クエリの末尾 `;` 付き単独送信で、分割実行が行トリガを発火する（`PARTITIONED;` が分割実行として解釈される）。
#[test]
fn wire_simple_query_partitioned_update_fires_row_triggers() {
    let (addr, _g) = serve();
    let mut s = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    setup(&mut s);
    assert_eq!(
        exec(
            &mut s,
            "UPDATE s.tt SET n = n WHERE n > 0 USING OPERATION_ID 'u1' PARTITIONED CHUNK 1;"
        ),
        "UPDATE 3"
    );
    assert_eq!(
        select_col(&mut s, "SELECT n FROM s.tt"),
        vec!["101", "102", "103"]
    );
    assert_eq!(
        select_col(&mut s, "SELECT n FROM audit"),
        vec!["101", "102", "103"]
    );
}

/// 拡張クエリ（Parse/Bind/Execute/Sync）でも同じ分割実行が通り、行トリガが発火する。
#[test]
fn wire_extended_query_partitioned_update_fires_row_triggers() {
    let (addr, _g) = serve();
    let mut s = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    setup(&mut s);
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body(
            "s1",
            "UPDATE s.tt SET n = n WHERE n > 0 USING OPERATION_ID 'u2' PARTITIONED CHUNK 2",
            0,
        ),
    );
    assert_eq!(read_message(&mut s).0, b'1');
    send_length_prefixed_message(&mut s, b'B', &bind_body("p1", "s1"));
    assert_eq!(read_message(&mut s).0, b'2');
    send_length_prefixed_message(&mut s, b'E', &execute_body("p1", 0));
    assert_eq!(read_message(&mut s).0, b'C');
    send_length_prefixed_message(&mut s, b'S', &[]);
    assert_eq!(read_message(&mut s).0, b'Z');
    assert_eq!(
        select_col(&mut s, "SELECT n FROM s.tt"),
        vec!["101", "102", "103"]
    );
}

/// 文トリガのある表の分割実行は `0A000` の ErrorResponse と ReadyForQuery で返り、接続は続けて使える。
/// intentional: docs/design/trigger-partitioned-dml.md
/// 根拠は同書 D1（文トリガは非原子実行で保証できない）。
#[test]
fn wire_partitioned_with_statement_trigger_is_0a000_and_connection_survives() {
    let (addr, _g) = serve();
    let mut s = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    setup(&mut s);
    exec(
        &mut s,
        "CREATE TRIGGER ts AFTER UPDATE ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION audit_new()",
    );
    send_simple_query(
        &mut s,
        "UPDATE s.tt SET n = n WHERE n > 0 USING OPERATION_ID 'u3' PARTITIONED CHUNK 1;",
    );
    expect_error_response_with_sqlstate(&mut s, "0A000");
    read_ready_for_query(&mut s);
    assert_eq!(
        select_col(&mut s, "SELECT n FROM s.tt"),
        vec!["1", "2", "3"]
    );
    assert!(select_col(&mut s, "SELECT n FROM audit").is_empty());
}
