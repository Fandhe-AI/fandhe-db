//! `COPY ... FROM STDIN` で INSERT トリガが発火することを wire 越しに通す E2E（TABLE-45・SQL-68・
//! RECOVER-1・RLS-10。Issue #3281）。`EngineCore` の既定 invoker（本番の PL/pgSQL トリガ実行）を注入なしで
//! 使い、簡易クエリ（psql と同じ末尾 `;` 付きの単独文を含む）・拡張クエリ・明示 Tx・テナント境界を確認する。
//! 表は SCHEMA `s` 配下、監査表は共有 `public`（トリガ本体は SCHEMA 修飾の書き込み先を受けないため）。
//! 結果の確定オラクルは `crates/engine/tests/table_rls/table45_trigger_copy_from.rs`（in-process）。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn serve() -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-trigger-copy");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice", "bob"]);
    (addr, guard)
}

fn connect(addr: std::net::SocketAddr, user: &str, pw: &str) -> TcpStream {
    authenticate_to_ready_for_query(addr, user, pw)
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

/// 共有の監査表（1 回だけ作る）。
fn audit_table(s: &mut TcpStream) {
    exec(s, "CREATE TABLE audit (lv TEXT)");
}

/// SCHEMA `s` に `tt (n)` を作り、`search_path` を `s, public` にする。
fn tenant_tables(s: &mut TcpStream) {
    exec(s, "CREATE SCHEMA s");
    exec(s, "SET search_path TO s, public");
    exec(s, "CREATE TABLE s.tt (n INTEGER)");
}

fn triggers(s: &mut TcpStream) {
    exec(
        s,
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.n := NEW.n + 100; RETURN NEW; END $$",
    );
    exec(
        s,
        "CREATE FUNCTION stmt_audit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (lv) VALUES (TG_LEVEL); RETURN NULL; END $$",
    );
    exec(
        s,
        "CREATE TRIGGER tb BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    exec(
        s,
        "CREATE TRIGGER ts AFTER INSERT ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()",
    );
}

/// CopyInResponse を受け取り、CopyData と CopyDone を送る。
fn copy_in_send(s: &mut TcpStream, payload: &[u8]) {
    let (ty, body) = read_message(s);
    assert_eq!(
        ty,
        b'G',
        "expected CopyInResponse, got {}: {:?}",
        ty as char,
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(s, b'd', payload);
    send_length_prefixed_message(s, b'c', b"");
}

/// 簡易クエリで COPY を送り、CommandComplete のタグを返す。
fn copy_simple(s: &mut TcpStream, sql: &str, payload: &[u8]) -> String {
    send_simple_query(s, sql);
    copy_in_send(s, payload);
    let tag = read_command_complete(s);
    read_ready_for_query(s);
    tag
}

/// 末尾 `;` 付きの単独 `COPY ... FROM STDIN;` で、BEFORE ROW の書き換えと AFTER STATEMENT が発火する。
#[test]
fn wire_simple_query_copy_fires_triggers_with_semicolon() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    audit_table(&mut s);
    tenant_tables(&mut s);
    triggers(&mut s);
    assert_eq!(
        copy_simple(&mut s, "COPY tt (n) FROM STDIN;", b"1\n2\n"),
        "COPY 2"
    );
    assert_eq!(select_col(&mut s, "SELECT n FROM tt"), vec!["101", "102"]);
    assert_eq!(
        select_col(&mut s, "SELECT lv FROM audit"),
        vec!["STATEMENT"]
    );
}

/// 拡張クエリ（Parse/Bind/Execute/Sync）の COPY でも発火する（準備文は SCHEMA 修飾名で書く）。
#[test]
fn wire_extended_query_copy_fires_triggers() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    audit_table(&mut s);
    tenant_tables(&mut s);
    triggers(&mut s);
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("c1", "COPY s.tt (n) FROM STDIN", 0),
    );
    let (ty, body) = read_message(&mut s);
    assert_eq!(ty, b'1', "{}", String::from_utf8_lossy(&body));
    send_length_prefixed_message(&mut s, b'B', &bind_body("p1", "c1"));
    assert_eq!(read_message(&mut s).0, b'2');
    send_length_prefixed_message(&mut s, b'E', &execute_body("p1", 0));
    copy_in_send(&mut s, b"5\n");
    assert_eq!(read_message(&mut s).0, b'C');
    send_length_prefixed_message(&mut s, b'S', &[]);
    assert_eq!(read_message(&mut s).0, b'Z');
    assert_eq!(select_col(&mut s, "SELECT n FROM tt"), vec!["105"]);
    assert_eq!(
        select_col(&mut s, "SELECT lv FROM audit"),
        vec!["STATEMENT"]
    );
}

/// 明示 Tx: COMMIT で結果が残り、ROLLBACK では残らない。
#[test]
fn wire_explicit_transaction_copy_commit_and_rollback() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    audit_table(&mut s);
    tenant_tables(&mut s);
    triggers(&mut s);
    exec(&mut s, "BEGIN");
    assert_eq!(
        copy_simple(&mut s, "COPY tt (n) FROM STDIN", b"1\n"),
        "COPY 1"
    );
    exec(&mut s, "COMMIT");
    exec(&mut s, "BEGIN");
    assert_eq!(
        copy_simple(&mut s, "COPY tt (n) FROM STDIN", b"2\n"),
        "COPY 1"
    );
    exec(&mut s, "ROLLBACK");
    assert_eq!(select_col(&mut s, "SELECT n FROM tt"), vec!["101"]);
    assert_eq!(
        select_col(&mut s, "SELECT lv FROM audit"),
        vec!["STATEMENT"]
    );
}

/// トリガ関数内のエラーで COPY 全体が失敗し、残骸が無く、同じ接続で次の文が実行できる。
#[test]
fn wire_trigger_error_fails_copy_and_connection_survives() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    audit_table(&mut s);
    tenant_tables(&mut s);
    exec(
        &mut s,
        "CREATE FUNCTION boom() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (lv) VALUES ('x'); IF NEW.n = 3 THEN RAISE EXCEPTION 'boom'; END IF; RETURN NEW; END $$",
    );
    exec(
        &mut s,
        "CREATE TRIGGER ta AFTER INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION boom()",
    );
    send_simple_query(&mut s, "COPY tt (n) FROM STDIN;");
    copy_in_send(&mut s, b"1\n3\n");
    let (ty, body) = read_message(&mut s);
    assert_eq!(ty, b'E', "expected ErrorResponse");
    assert!(!String::from_utf8_lossy(&body).contains("C0A000"));
    read_ready_for_query(&mut s);
    assert!(select_col(&mut s, "SELECT n FROM tt").is_empty());
    assert!(select_col(&mut s, "SELECT lv FROM audit").is_empty());
}

/// 別テナント（bob）の COPY では alice のトリガが発火せず、alice の監査行も bob から見えない。
#[test]
fn wire_copy_trigger_does_not_fire_for_other_tenant() {
    let (addr, _g) = serve();
    let mut a = connect(addr, "alice", "pw-alice");
    audit_table(&mut a);
    tenant_tables(&mut a);
    triggers(&mut a);
    let mut b = connect(addr, "bob", "pw-bob");
    tenant_tables(&mut b);
    assert_eq!(
        copy_simple(&mut b, "COPY tt (n) FROM STDIN;", b"1\n"),
        "COPY 1"
    );
    assert_eq!(select_col(&mut b, "SELECT n FROM tt"), vec!["1"]);
    assert!(select_col(&mut b, "SELECT lv FROM audit").is_empty());
    assert!(select_col(&mut a, "SELECT lv FROM audit").is_empty());
}
