//! `%`（剰余）演算子（SQL-42 のポインタのみ。Issue #3273、親 #3272）を、psql 同様に末尾 `;` 付きで
//! 単独送信する簡易クエリ（'Q'）と拡張クエリ（Parse／Describe）で固定する層 A 結合テスト。
//!
//! engine 側が決めた結果列型（整数の剰余は int4／int8、PostgreSQL 17 準拠）が RowDescription の
//! 型 OID・DataRow の text 表現として観測できること、0 除算（22012）・float 型被演算子（42883）が
//! ErrorResponse の SQLSTATE として届き、その後も接続が使えること（fail-closed）を確認する。
//! wire 層の本番コードは汎用経路のまま（変更なし）。

use crate::common;

use crate::temp_db;

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql42-remainder");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("r", ColumnType::Real, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, embedding, n, b, r) VALUES (1, '[0.1,0.1]', -7, 10, 2.5) \
         USING OPERATION_ID 'seed-1'",
    );
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
    (stream, guard)
}

fn expect_one_value(s: &mut TcpStream, sql: &str, oid: i32, len: i16, text: &str) {
    send_simple_query(s, sql);
    assert_eq!(
        read_row_description_with_types(s),
        vec![("?column?".to_string(), oid, len)],
        "{sql}"
    );
    assert_eq!(read_data_row(s), vec![Some(text.to_string())], "{sql}");
    assert_eq!(read_command_complete(s), "SELECT 1", "{sql}");
    assert_eq!(read_ready_for_query_status(s), b'I');
}

#[test]
fn simple_query_with_trailing_semicolon_returns_integer_remainder() {
    let (mut s, _g) = connect();
    // 符号は被除数に従う（PostgreSQL 17: -7 % 3 = -1）。
    expect_one_value(&mut s, "SELECT 7 % 3;", 23, 4, "1");
    expect_one_value(&mut s, "SELECT n % 3 FROM t;", 23, 4, "-1");
    expect_one_value(&mut s, "SELECT b % 4 FROM t;", 20, 8, "2");
    expect_one_value(&mut s, "SELECT n % b FROM t;", 20, 8, "-7");
}

#[test]
fn division_by_zero_and_float_operands_fail_closed_and_keep_connection() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT 1 % 0;");
    expect_error_response_with_sqlstate(&mut s, "22012");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "SELECT r % 2 FROM t;");
    expect_error_response_with_sqlstate(&mut s, "42883");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "SELECT n % FROM t;");
    expect_error_response_with_sqlstate(&mut s, "42601");
    read_ready_for_query(&mut s);

    // エラー後も同じ接続で通常どおり実行できる。
    expect_one_value(&mut s, "SELECT 10 % 4;", 23, 4, "2");
}

#[test]
fn extended_describe_advertises_integer_remainder_type() {
    let (mut s, _g) = connect();
    send_length_prefixed_message(&mut s, b'P', &parse_body("s0", "SELECT n % 3 FROM t", 0));
    let mut describe = vec![b'S'];
    describe.extend_from_slice(b"s0");
    describe.push(0);
    send_length_prefixed_message(&mut s, b'D', &describe);
    send_length_prefixed_message(&mut s, b'S', b"");
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b'1', "ParseComplete");
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b't', "ParameterDescription");
    assert_eq!(body, [0, 0], "no parameters");
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'T', "RowDescription");
    // 列数 1、列名 `?column?` の後ろの型 OID は 23（int4）。
    let name_end = body[2..].iter().position(|b| *b == 0).expect("nul") + 2;
    assert_eq!(&body[2..name_end], b"?column?");
    let pos = name_end + 1;
    let oid = i32::from_be_bytes(body[pos + 6..pos + 10].try_into().expect("oid"));
    assert_eq!(oid, 23);
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'Z');
    assert_eq!(body, [b'I']);
}
/// 二テナント構成（alice=tenant-a に 5 行、bob=tenant-b に `n = 0` の 1 行）の接続を返す。
/// Issue #3274: 各式文脈の `%` を、他テナントの行が結果へ混ざらないことと併せて固定する。
/// alice は DDL 許可主体（CHECK 付き表の作成に使う）。
fn connect_two_tenants() -> (TcpStream, TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql42-remainder-ctx");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let store = UserStore::load_from_file(&users_path)
        .expect("valid user store")
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("known principal");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let store = Arc::new(store);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            core,
            ConnectionLimiter::new(16),
            Duration::from_secs(5),
        );
    });
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    for (id, n) in [(1, 1), (2, 7), (3, -7), (4, 4), (5, 6)] {
        send_simple_query(
            &mut alice,
            &format!(
                "INSERT INTO t (id, embedding, n) VALUES ({id}, '[0.{id},0.1]', {n}) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
        read_command_complete(&mut alice);
        read_ready_for_query(&mut alice);
    }
    send_simple_query(
        &mut bob,
        "INSERT INTO t (id, embedding, n) VALUES (9, '[0.9,0.1]', 0) USING OPERATION_ID 'seed-b'",
    );
    read_command_complete(&mut bob);
    read_ready_for_query(&mut bob);
    (alice, bob, guard)
}

/// DataRow 本体（列数 i16 の後に列ごと i32 長＋バイト列）を text 列へ復号する。
fn decode_data_row(body: &[u8]) -> Vec<String> {
    let ncols = i16::from_be_bytes(body[0..2].try_into().expect("ncols")) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..ncols {
        let len = i32::from_be_bytes(body[pos..pos + 4].try_into().expect("len")) as usize;
        pos += 4;
        out.push(String::from_utf8(body[pos..pos + len].to_vec()).expect("utf8"));
        pos += len;
    }
    out
}

/// RowDescription の後の全 DataRow を集め、CommandComplete・ReadyForQuery('I') まで消費する。
fn read_rows(s: &mut TcpStream, sql: &str, cols: usize) -> Vec<Vec<String>> {
    send_simple_query(s, sql);
    assert_eq!(read_row_description(s).len(), cols, "{sql}");
    let mut rows = Vec::new();
    loop {
        let (k, body) = read_message(s);
        match k {
            b'D' => rows.push(decode_data_row(&body)),
            b'C' => break,
            other => panic!("unexpected message {other:?} for {sql}"),
        }
    }
    assert_eq!(read_ready_for_query_status(s), b'I', "{sql}");
    rows
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|c| c.to_string()).collect())
        .collect()
}

#[test]
fn simple_query_remainder_in_each_expression_context() {
    let (mut a, mut b, _g) = connect_two_tenants();
    // WHERE（他テナントの n = 0 は偶数でも現れない）。
    let rows = read_rows(&mut a, "SELECT id FROM t WHERE n % 2 = 0 ORDER BY id;", 1);
    assert_eq!(rows, strs(&[&["4"], &["5"]]));
    // GROUP BY（-7 % 3 = -1。他テナントの 0 は群に混ざらない）。
    let rows = read_rows(
        &mut a,
        "SELECT n % 3, count(*) FROM t GROUP BY n % 3 ORDER BY 1;",
        2,
    );
    assert_eq!(rows, strs(&[&["-1", "1"], &["0", "1"], &["1", "3"]]));
    // ORDER BY（式キー）。
    let rows = read_rows(&mut a, "SELECT id FROM t ORDER BY abs(n) % 3, id;", 1);
    assert_eq!(rows, strs(&[&["5"], &["1"], &["2"], &["3"], &["4"]]));
    // UPDATE SET。
    send_simple_query(&mut a, "UPDATE t SET n = n % 3 WHERE id = 1;");
    assert_eq!(read_command_complete(&mut a), "UPDATE 1");
    read_ready_for_query(&mut a);
    // 別テナントの結果は自分の 1 行だけ。
    let rows = read_rows(&mut b, "SELECT n % 2 FROM t;", 1);
    assert_eq!(rows, strs(&[&["0"]]));
}

/// 素の走査の ORDER BY が算術で始まるキーを拒否する既存制限は `%` にも `* /` と同じく適用する。
/// intentional: docs/design/order-by-having-expressions.md
/// 根拠の節: 算術で始まる ORDER BY キーは 42601（同記録の対象外。経緯は Issue #3274）
#[test]
fn plain_scan_order_by_remainder_key_is_rejected_and_keeps_connection() {
    let (mut a, _b, _g) = connect_two_tenants();
    send_simple_query(&mut a, "SELECT id FROM t ORDER BY n % 2;");
    expect_error_response_with_sqlstate(&mut a, "42601");
    read_ready_for_query(&mut a);
    let rows = read_rows(&mut a, "SELECT id FROM t WHERE n % 2 = 1 ORDER BY id;", 1);
    // -7 % 2 = -1（符号は被除数に従う）ため id 3 は含まれない。
    assert_eq!(rows, strs(&[&["1"], &["2"]]));
}

#[test]
fn check_constraint_remainder_violation_is_23514_and_keeps_connection() {
    let (mut a, _b, _g) = connect_two_tenants();
    for (sql, tag) in [
        (
            "CREATE TABLE c (n INTEGER, CHECK (n % 2 = 0));",
            "CREATE TABLE",
        ),
        ("INSERT INTO c (n) VALUES (4);", "INSERT 0 1"),
    ] {
        send_simple_query(&mut a, sql);
        assert_eq!(read_command_complete(&mut a), tag, "{sql}");
        read_ready_for_query(&mut a);
    }
    send_simple_query(&mut a, "INSERT INTO c (n) VALUES (5);");
    expect_error_response_with_sqlstate(&mut a, "23514");
    read_ready_for_query(&mut a);
    send_simple_query(&mut a, "INSERT INTO c (n) VALUES (6);");
    assert_eq!(read_command_complete(&mut a), "INSERT 0 1");
    read_ready_for_query(&mut a);
}
