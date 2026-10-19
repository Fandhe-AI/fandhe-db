//! `CREATE TEMP TABLE`・一時表の `DROP TABLE`・`ON COMMIT`（TABLE-44・Issue #1683）の
//! pg wire v3 越し（生バイトクライアント）検証。簡易クエリと拡張クエリ（Parse/Bind/Execute）の
//! 両方で、DDL 権限を持たない接続でも一時表を作成・削除できること、一時表を作った後も
//! 2 個目の `CREATE TEMP TABLE` と一時表の `DROP` が拡張クエリで通ること（暫定ゲートの
//! 例外）、`ON COMMIT DROP` の結果、別接続からは一時表が見えないことを確認する。
//!
//! 一時表の意味論（名前解決・上限・fail-closed な拒否）は
//! `crates/engine/tests/table44_temp_table_ddl.rs` が確定オラクルで、本ファイルは wire の
//! フレーミング越しに CommandComplete タグと接続維持が正しいことに徹する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-temp-table");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn send_sync(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn assert_ready_for_query(stream: &mut TcpStream) {
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
}

/// 拡張クエリで DDL を 1 文実行し、CommandComplete のタグを返す。
fn extended_command(stream: &mut TcpStream, name: &str, sql: &str) -> String {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'1',
        "expected ParseComplete for {sql}: {}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(stream, b'B', &bind_body(name, name));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(stream, b'E', &execute_body(name, 0));
    let (kind, tag) = read_message(stream);
    assert_eq!(kind, b'C', "expected CommandComplete for {sql}");
    let tag = String::from_utf8_lossy(&tag[..tag.len() - 1]).into_owned();
    send_sync(stream);
    assert_ready_for_query(stream);
    tag
}

/// 簡易クエリ: DDL 権限の無い接続でも作成・削除でき、重複は `42P07`、
/// 永続表の DDL は `42501` のまま。接続は維持される。
#[test]
fn simple_query_creates_and_drops_temp_tables_without_ddl_permission() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE TEMP TABLE t (a TEXT)");
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE TEMP TABLE t (a TEXT)");
    expect_error_response_with_sqlstate(&mut stream, "42P07");
    read_ready_for_query(&mut stream);

    // 一時表を持つ接続でも、2 個目の CREATE TEMP と一時表の DROP が通る。
    send_simple_query(
        &mut stream,
        "CREATE GLOBAL TEMPORARY TABLE u (a TEXT) ON COMMIT PRESERVE ROWS",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TABLE t, u");
    assert_eq!(read_command_complete(&mut stream), "DROP TABLE");
    read_ready_for_query(&mut stream);

    // 一時表でなくなった名前は永続側の経路になり、DDL 権限が無ければ 42501。
    send_simple_query(&mut stream, "DROP TABLE t");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "CREATE TABLE p (a TEXT)");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);

    // ON COMMIT DROP は自動コミットの文の終わりで消えるため、再作成できる。
    for _ in 0..2 {
        send_simple_query(&mut stream, "CREATE TEMP TABLE d (a TEXT) ON COMMIT DROP");
        assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
        read_ready_for_query(&mut stream);
    }
}

/// 拡張クエリ: 一時表を作った後も、粗いゲートの例外により 2 個目の CREATE TEMP・
/// 一時表の DROP・ON COMMIT DROP の再作成が通る。
#[test]
fn extended_query_creates_and_drops_temp_tables() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(
        extended_command(&mut stream, "s1", "CREATE TEMP TABLE t (a TEXT)"),
        "CREATE TABLE"
    );
    assert_eq!(
        extended_command(&mut stream, "s2", "CREATE TEMP TABLE u (a TEXT)"),
        "CREATE TABLE"
    );
    assert_eq!(
        extended_command(
            &mut stream,
            "s3",
            "CREATE TEMPORARY TABLE d (a TEXT) ON COMMIT DROP"
        ),
        "CREATE TABLE"
    );
    // d は消えているので、同名を作り直せる。
    assert_eq!(
        extended_command(
            &mut stream,
            "s4",
            "CREATE TEMPORARY TABLE d (a TEXT) ON COMMIT DROP"
        ),
        "CREATE TABLE"
    );
    assert_eq!(
        extended_command(&mut stream, "s5", "DROP TABLE t"),
        "DROP TABLE"
    );
    assert_eq!(
        extended_command(&mut stream, "s6", "CREATE TEMP TABLE t (a TEXT)"),
        "CREATE TABLE"
    );
}

/// 別の接続からは一時表が見えない（同名を作れ、永続側の DROP は権限エラー）。
/// 接続を切って張り直した接続にも引き継がれない。
#[test]
fn temp_tables_are_private_to_a_connection() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);

    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut a, "CREATE TEMP TABLE only_a (x TEXT)");
    assert_eq!(read_command_complete(&mut a), "CREATE TABLE");
    read_ready_for_query(&mut a);

    let mut b = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut b, "DROP TABLE only_a");
    expect_error_response_with_sqlstate(&mut b, "42501");
    read_ready_for_query(&mut b);
    send_simple_query(&mut b, "CREATE TEMP TABLE only_a (x TEXT)");
    assert_eq!(read_command_complete(&mut b), "CREATE TABLE");
    read_ready_for_query(&mut b);

    drop(a);
    let mut c = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut c, "DROP TABLE only_a");
    expect_error_response_with_sqlstate(&mut c, "42501");
    read_ready_for_query(&mut c);
}

// ---------------------------------------------------------------------------------------------
// 一時表への DML・SELECT・COPY（TABLE-44・Issue #1684）
// ---------------------------------------------------------------------------------------------

/// 永続表 `perm(a TEXT)` を持つコア（一時表を持つ接続でも永続表の文が通ることの確認用）。
fn new_core_with_perm() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    use engine::catalog::{ColumnDef, ColumnType, TableSchema};
    let path = temp_db::unique_db_path("wire-temp-table-dml");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "perm",
            vec![ColumnDef::new("a", ColumnType::Text, true)],
        ))
        .expect("create perm");
    // 永続表の `operation_id` 必須化（`--require-operation-id` 相当）を明示する。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    (Arc::new(core), guard)
}

fn simple_ok(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

fn simple_err(stream: &mut TcpStream, sql: &str, sqlstate: &str) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, sqlstate);
    read_ready_for_query(stream);
}

fn simple_rows(stream: &mut TcpStream, sql: &str) -> (Vec<String>, Vec<Vec<Option<String>>>) {
    send_simple_query(stream, sql);
    let cols = read_row_description(stream);
    let mut rows = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => rows.push(parse_data_row(&body)),
            b'C' => break,
            other => panic!("unexpected message {}: {sql}", other as char),
        }
    }
    read_ready_for_query(stream);
    (cols, rows)
}

fn parse_data_row(body: &[u8]) -> Vec<Option<String>> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut cells = Vec::new();
    for _ in 0..n {
        let len = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        if len < 0 {
            cells.push(None);
        } else {
            let len = len as usize;
            cells.push(Some(
                String::from_utf8_lossy(&body[pos..pos + len]).into_owned(),
            ));
            pos += len;
        }
    }
    cells
}

/// 拡張クエリの 1 往復の結果。
#[derive(Debug, PartialEq)]
enum Ext {
    Ok {
        rows: Vec<Vec<Option<String>>>,
        tag: String,
    },
    Err(String),
}

fn bind_with_params(portal: &str, statement: &str, params: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for p in params {
        body.extend_from_slice(&(p.len() as i32).to_be_bytes());
        body.extend_from_slice(p.as_bytes());
    }
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    body
}

/// Sync まで読み、結果を畳む。
fn drain_to_sync(stream: &mut TcpStream) -> Ext {
    let mut rows = Vec::new();
    let mut tag = String::new();
    let mut err = None;
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => rows.push(parse_data_row(&body)),
            b'C' => tag = String::from_utf8_lossy(&body[..body.len() - 1]).into_owned(),
            b'E' => {
                if err.is_none() {
                    // フィールド 'C' の値（SQLSTATE）を取り出す。
                    let mut pos = 0usize;
                    while pos < body.len() && body[pos] != 0 {
                        let field = body[pos];
                        pos += 1;
                        let end = pos + body[pos..].iter().position(|&b| b == 0).unwrap_or(0);
                        if field == b'C' {
                            err = Some(String::from_utf8_lossy(&body[pos..end]).into_owned());
                        }
                        pos = end + 1;
                    }
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    match err {
        Some(c) => Ext::Err(c),
        None => Ext::Ok { rows, tag },
    }
}

/// Parse → Bind → Execute → Sync を一度に送って結果を読む。
fn ext_run(stream: &mut TcpStream, name: &str, sql: &str, params: &[&str]) -> Ext {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, 0));
    send_length_prefixed_message(stream, b'B', &bind_with_params(name, name, params));
    send_length_prefixed_message(stream, b'E', &execute_body(name, 0));
    send_sync(stream);
    drain_to_sync(stream)
}

/// 既に Parse 済みのステートメントを Bind → Execute → Sync する。
fn ext_bind_exec(stream: &mut TcpStream, name: &str, params: &[&str]) -> Ext {
    send_length_prefixed_message(stream, b'B', &bind_with_params("p", name, params));
    send_length_prefixed_message(stream, b'E', &execute_body("p", 0));
    send_sync(stream);
    drain_to_sync(stream)
}

fn cells(values: &[&str]) -> Vec<Option<String>> {
    values.iter().map(|v| Some((*v).to_string())).collect()
}

/// 簡易クエリ: 一時表への INSERT／SELECT／UPDATE／DELETE／TRUNCATE とトランザクション、
/// 別接続から見えないこと、`USING OPERATION_ID` の拒否。
#[test]
fn simple_query_dml_and_select_on_temp_tables() {
    let (core, _guard) = new_core_with_perm();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut s = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(
        simple_ok(&mut s, "CREATE TEMP TABLE t (a TEXT NOT NULL)"),
        "CREATE TABLE"
    );
    // `--require-operation-id` 構成（台帳あり・id 必須）でも operation_id は要らない。
    assert_eq!(
        simple_ok(&mut s, "INSERT INTO t (id, a) VALUES (1, 'x'), (2, 'y')"),
        "INSERT 0 2"
    );
    let (cols, rows) = simple_rows(&mut s, "SELECT id, a FROM t ORDER BY id LIMIT 10");
    assert_eq!(cols, ["id", "a"]);
    assert_eq!(rows, [cells(&["1", "x"]), cells(&["2", "y"])]);
    assert_eq!(
        simple_ok(&mut s, "UPDATE t SET a = 'z' WHERE id = 1"),
        "UPDATE 1"
    );
    assert_eq!(simple_ok(&mut s, "DELETE FROM t WHERE id = 2"), "DELETE 1");
    simple_err(&mut s, "INSERT INTO t (id, a) VALUES (1, 'dup')", "23505");
    simple_err(&mut s, "INSERT INTO t (id) VALUES (5)", "23502");
    simple_err(
        &mut s,
        "INSERT INTO t (id, a) VALUES (6, 'q') USING OPERATION_ID 'op-1'",
        "0A000",
    );
    // 永続表の文は一時表を持つ接続でも従来どおり（`--require-operation-id` 構成では operation_id が要る）。
    simple_err(&mut s, "INSERT INTO perm (id, a) VALUES (1, 'p')", "23502");
    assert_eq!(
        simple_ok(
            &mut s,
            "INSERT INTO perm (id, a) VALUES (1, 'p') USING OPERATION_ID 'perm-1'"
        ),
        "INSERT 0 1"
    );

    // トランザクション: ROLLBACK で消え、COMMIT で残る。
    assert_eq!(simple_ok(&mut s, "BEGIN"), "BEGIN");
    assert_eq!(
        simple_ok(&mut s, "INSERT INTO t (id, a) VALUES (7, 'tx')"),
        "INSERT 0 1"
    );
    assert_eq!(simple_rows(&mut s, "SELECT id FROM t LIMIT 10").1.len(), 2);
    assert_eq!(simple_ok(&mut s, "ROLLBACK"), "ROLLBACK");
    assert_eq!(simple_rows(&mut s, "SELECT id FROM t LIMIT 10").1.len(), 1);
    assert_eq!(simple_ok(&mut s, "BEGIN"), "BEGIN");
    assert_eq!(
        simple_ok(&mut s, "INSERT INTO t (id, a) VALUES (7, 'tx')"),
        "INSERT 0 1"
    );
    assert_eq!(simple_ok(&mut s, "COMMIT"), "COMMIT");
    assert_eq!(simple_rows(&mut s, "SELECT id FROM t LIMIT 10").1.len(), 2);

    // 別接続からは見えない。
    let mut other = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    simple_err(&mut other, "SELECT id FROM t LIMIT 10", "42P01");
    simple_err(
        &mut other,
        "INSERT INTO t (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-other'",
        "42P01",
    );

    assert_eq!(simple_ok(&mut s, "TRUNCATE t"), "TRUNCATE TABLE");
    assert_eq!(simple_rows(&mut s, "SELECT id FROM t LIMIT 10").1.len(), 0);
}

/// 拡張クエリ: パラメータ付き INSERT／SELECT・永続表の文・Describe・陳腐化した準備済み文。
#[test]
fn extended_query_dml_and_select_on_temp_tables() {
    let (core, _guard) = new_core_with_perm();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut s = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(
        simple_ok(&mut s, "CREATE TEMP TABLE t (a TEXT NOT NULL)"),
        "CREATE TABLE"
    );
    assert_eq!(
        ext_run(
            &mut s,
            "i1",
            "INSERT INTO t (id, a) VALUES ($1, $2)",
            &["1", "x"]
        ),
        Ext::Ok {
            rows: vec![],
            tag: "INSERT 0 1".to_string()
        }
    );
    assert_eq!(
        ext_run(&mut s, "i2", "INSERT INTO t (id, a) VALUES (2, 'y')", &[]),
        Ext::Ok {
            rows: vec![],
            tag: "INSERT 0 1".to_string()
        }
    );
    assert_eq!(
        ext_run(
            &mut s,
            "q1",
            "SELECT id, a FROM t ORDER BY id LIMIT 10",
            &[]
        ),
        Ext::Ok {
            rows: vec![cells(&["1", "x"]), cells(&["2", "y"])],
            tag: "SELECT 2".to_string()
        }
    );
    assert_eq!(
        ext_run(
            &mut s,
            "q2",
            "SELECT a FROM t WHERE id = $1 LIMIT 5",
            &["2"]
        ),
        Ext::Ok {
            rows: vec![cells(&["y"])],
            tag: "SELECT 1".to_string()
        }
    );
    assert_eq!(
        ext_run(&mut s, "d1", "DELETE FROM t WHERE id = $1", &["1"]),
        Ext::Ok {
            rows: vec![],
            tag: "DELETE 1".to_string()
        }
    );
    // 一時表を持つ接続でも、永続表の文は通る（一時表の名前を含まない文）。
    assert_eq!(
        ext_run(&mut s, "p1", "SELECT id FROM perm LIMIT 5", &[]),
        Ext::Ok {
            rows: vec![],
            tag: "SELECT 0".to_string()
        }
    );
    // 一時表を含まず `$n` も無い準備済み文は、一時表を作った後でも実行できる。
    assert_eq!(
        ext_run(&mut s, "p2", "SELECT id FROM perm LIMIT 3", &[]),
        Ext::Ok {
            rows: vec![],
            tag: "SELECT 0".to_string()
        }
    );
    // 混在する文・未対応の文種は 0A000。
    assert_eq!(
        ext_run(&mut s, "m1", "TRUNCATE t, perm", &[]),
        Ext::Err("0A000".to_string())
    );
    assert_eq!(
        ext_run(&mut s, "m2", "ALTER TABLE t ADD COLUMN z TEXT", &[]),
        Ext::Err("0A000".to_string())
    );

    // Describe(S): 一時表の SELECT の結果列。
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("ds", "SELECT id, a FROM t LIMIT 1", 0),
    );
    let mut describe = vec![b'S'];
    describe.extend_from_slice(b"ds\0");
    send_length_prefixed_message(&mut s, b'D', &describe);
    send_sync(&mut s);
    let mut kinds = Vec::new();
    loop {
        let (kind, _) = read_message(&mut s);
        kinds.push(kind);
        if kind == b'Z' {
            break;
        }
    }
    assert!(kinds.contains(&b'T'), "RowDescription expected: {kinds:?}");

    // 陳腐化: 準備済み文は、一時表が作り直された後に Bind すると 0A000（再 Parse すれば通る）。
    assert_eq!(
        ext_run(&mut s, "stale", "SELECT id FROM t LIMIT 10", &[]),
        Ext::Ok {
            rows: vec![cells(&["2"])],
            tag: "SELECT 1".to_string()
        }
    );
    assert_eq!(simple_ok(&mut s, "DROP TABLE t"), "DROP TABLE");
    assert_eq!(
        simple_ok(&mut s, "CREATE TEMP TABLE t (a TEXT NOT NULL)"),
        "CREATE TABLE"
    );
    assert_eq!(
        ext_bind_exec(&mut s, "stale", &[]),
        Ext::Err("0A000".to_string())
    );
    assert_eq!(
        ext_run(&mut s, "stale2", "SELECT id FROM t LIMIT 10", &[]),
        Ext::Ok {
            rows: vec![],
            tag: "SELECT 0".to_string()
        }
    );
}

/// `COPY ... FROM STDIN`／`COPY (...) TO STDOUT` を一時表に対して実行できる。
#[test]
fn copy_from_and_to_work_on_temp_tables() {
    let (core, _guard) = new_core_with_perm();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut s = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(
        simple_ok(&mut s, "CREATE TEMP TABLE t (a TEXT)"),
        "CREATE TABLE"
    );
    assert_eq!(
        simple_ok(&mut s, "CREATE TEMP TABLE d (a TEXT) ON COMMIT DELETE ROWS"),
        "CREATE TABLE"
    );

    // COPY FROM STDIN（operation_id は不要）。
    send_simple_query(&mut s, "COPY t (id, a) FROM STDIN");
    let (kind, _) = read_message(&mut s);
    assert_eq!(kind, b'G', "expected CopyInResponse");
    send_length_prefixed_message(&mut s, b'd', b"1\talpha\n2\tbeta\n");
    send_length_prefixed_message(&mut s, b'c', b"");
    assert_eq!(read_command_complete(&mut s), "COPY 2");
    read_ready_for_query(&mut s);
    assert_eq!(simple_rows(&mut s, "SELECT id FROM t LIMIT 10").1.len(), 2);

    // COPY TO STDOUT。
    send_simple_query(
        &mut s,
        "COPY (SELECT id, a FROM t ORDER BY id LIMIT 10) TO STDOUT",
    );
    let (kind, _) = read_message(&mut s);
    assert_eq!(kind, b'H', "expected CopyOutResponse");
    let mut out = Vec::new();
    loop {
        let (kind, body) = read_message(&mut s);
        match kind {
            b'd' => out.extend_from_slice(&body),
            b'c' => break,
            other => panic!("unexpected {}", other as char),
        }
    }
    assert_eq!(String::from_utf8_lossy(&out), "1\talpha\n2\tbeta\n");
    assert_eq!(read_command_complete(&mut s), "COPY 2");
    read_ready_for_query(&mut s);

    // 自動コミットの COPY の後に ON COMMIT DELETE ROWS が働く。
    send_simple_query(&mut s, "COPY d (id, a) FROM STDIN");
    let (kind, _) = read_message(&mut s);
    assert_eq!(kind, b'G');
    send_length_prefixed_message(&mut s, b'd', b"1\tx\n");
    send_length_prefixed_message(&mut s, b'c', b"");
    assert_eq!(read_command_complete(&mut s), "COPY 1");
    read_ready_for_query(&mut s);
    assert_eq!(simple_rows(&mut s, "SELECT id FROM d LIMIT 10").1.len(), 0);

    // 永続表と混在する COPY は拒否される。
    simple_err(
        &mut s,
        "COPY t (id, a) FROM STDIN USING OPERATION_ID 'x'",
        "0A000",
    );
}
