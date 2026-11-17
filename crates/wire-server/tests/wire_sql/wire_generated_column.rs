//! 生成列 `GENERATED ALWAYS AS (<式>) STORED` の宣言（`CREATE TABLE`）の wire 越し検証（層 A。
//! Issue #3327。ポインタ: `docs/spec/04-behavior/` の TABLE-50、ERR-6）。
//!
//! 宣言の意味論・SQLSTATE の表は engine の `table50_generated_column_create` が確定オラクルとして
//! 検証済みのため、本ファイルは (1) psql 形（末尾 `;` を付けた単独の simple query）で生成列つきの
//! `CREATE TABLE` が `CommandComplete` で通ること、(2) 拒否形が `ErrorResponse` の SQLSTATE で返ること、
//! (3) INSERT・UPDATE・UPSERT の計算（Issue #3328）と生成列への値の指定の `428C9`、を pg wire 越しに確認する。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-generated-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// 許可主体つき（`--ddl-allowed-users` 相当）のサーバーを起動する（`wire_create_table` と同型）。
fn spawn_with_ddl_user(users_path: &std::path::Path, engine: Arc<EngineCore>) -> SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("ddl principals must reference known users");
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });
    addr
}

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let (core, guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_user(&users_path, core);
    (
        authenticate_to_ready_for_query(addr, "alice", "pw-alice"),
        guard,
    )
}

/// psql 形（末尾 `;`）の単独 simple query で生成列つきの表を作れる。
#[test]
fn psql_shaped_create_table_with_generated_column_succeeds() {
    let (mut stream, _guard) = connect();
    send_simple_query(
        &mut stream,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);
}

/// 拒否形は決めた SQLSTATE の `ErrorResponse` で返り、接続は使い続けられる。
#[test]
fn rejected_forms_return_sqlstate() {
    let (mut stream, _guard) = connect();
    for (sql, sqlstate) in [
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) VIRTUAL);",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b TIMESTAMPTZ GENERATED ALWAYS AS (now()) STORED);",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (zzz) STORED);",
            "42703",
        ),
        (
            "CREATE TABLE r (a INTEGER, b TEXT GENERATED ALWAYS AS (a + 1) STORED);",
            "42804",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
    // 拒否の後も表は残らず、同名で作れる。
    send_simple_query(&mut stream, "CREATE TABLE r (a INTEGER);");
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);
}

/// psql 形（末尾 `;` 付きの単独送信）の INSERT・UPDATE・UPSERT で生成列が計算され、SELECT が計算後の値を返す
/// （Issue #3328）。
#[test]
fn insert_update_upsert_compute_the_generated_column() {
    let (mut stream, _guard) = connect();
    send_simple_query(
        &mut stream,
        "CREATE TABLE g (k INTEGER PRIMARY KEY, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO g (k, a) VALUES (1, 1) USING OPERATION_ID 'wire-gen-1';",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "UPDATE g SET a = 5 WHERE k = 1 USING OPERATION_ID 'wire-gen-2';",
    );
    assert_eq!(read_command_complete(&mut stream), "UPDATE 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO g (k, a) VALUES (1, 7) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'wire-gen-3';",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT a, b FROM g;");
    let _ = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("7".to_string()), Some("14".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 生成列への値の指定は `428C9`。拒否の後も接続は使える。
#[test]
fn explicit_value_for_generated_column_is_rejected_with_428c9() {
    let (mut stream, _guard) = connect();
    send_simple_query(
        &mut stream,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    for sql in [
        "INSERT INTO g (a, b) VALUES (1, 2) USING OPERATION_ID 'wire-gen-4';",
        "UPDATE g SET b = 1 WHERE a = 1 USING OPERATION_ID 'wire-gen-5';",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "428C9");
        read_ready_for_query(&mut stream);
    }
}

/// 参照列の変更・削除・改名の依存検査（Issue #3331。TABLE-50 (3)）を psql 形（末尾 `;` の単独 simple query）で
/// 流す。受理する形は `CommandComplete "ALTER TABLE"`、拒否する形は `ErrorResponse` の SQLSTATE で返り、
/// どちらの後も `ReadyForQuery` へ戻る。意味論の確定オラクルは engine の `table50_generated_column_alter`。
#[test]
fn psql_shaped_alter_on_generated_columns() {
    let (mut stream, _guard) = connect();
    for sql in [
        "CREATE TABLE g (a INTEGER, other INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
        "CREATE TABLE h (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED, keep INTEGER);",
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
        read_ready_for_query(&mut stream);
    }
    for (sql, code) in [
        ("ALTER TABLE g DROP COLUMN a;", "2BP01"),
        ("ALTER TABLE g DROP COLUMN a RESTRICT;", "2BP01"),
        ("ALTER TABLE g ALTER COLUMN a TYPE BIGINT;", "0A000"),
        (
            "ALTER TABLE g ALTER COLUMN b SET EXPRESSION AS (a);",
            "0A000",
        ),
        ("ALTER TABLE g ALTER COLUMN other DROP EXPRESSION;", "55000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
    for sql in [
        "ALTER TABLE g RENAME COLUMN a TO a2;",
        "ALTER TABLE g ALTER COLUMN other DROP EXPRESSION IF EXISTS;",
        "ALTER TABLE g ALTER COLUMN b DROP EXPRESSION;",
        "ALTER TABLE h DROP COLUMN a CASCADE;",
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), "ALTER TABLE", "{sql}");
        read_ready_for_query(&mut stream);
    }
}
/// 簡易クエリで文を送り、CommandComplete のタグを返す（末尾 `;` 付きの単独送信）。
fn exec(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

/// SELECT の結果（`n` 行）を読む。
fn select_rows(stream: &mut TcpStream, sql: &str, n: usize) -> Vec<Vec<Option<String>>> {
    send_simple_query(stream, sql);
    let _ = read_row_description(stream);
    let rows: Vec<Vec<Option<String>>> = (0..n).map(|_| read_data_row(stream)).collect();
    assert_eq!(read_command_complete(stream), format!("SELECT {n}"));
    read_ready_for_query(stream);
    rows
}

fn cells(rows: &[&[&str]]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|r| r.iter().map(|c| Some(c.to_string())).collect())
        .collect()
}

/// CopyInResponse を受け取り、CopyData と CopyDone を送る。
fn copy_in_send(stream: &mut TcpStream, payload: &[u8]) {
    let (ty, body) = read_message(stream);
    assert_eq!(
        ty,
        b'G',
        "expected CopyInResponse, got {}: {:?}",
        ty as char,
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(stream, b'd', payload);
    send_length_prefixed_message(stream, b'c', b"");
}

/// psql 形（末尾 `;` 付きの単独 `COPY ... FROM STDIN;`）の COPY が生成列を計算する（Issue #3329）。列リスト省略は
/// 生成列を除いた並び、列リストの生成列は CopyInResponse の前に `428C9`。
#[test]
fn psql_shaped_copy_from_stdin_computes_the_generated_column() {
    let (mut stream, _guard) = connect();
    exec(
        &mut stream,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, c INTEGER);",
    );
    send_simple_query(&mut stream, "COPY g FROM STDIN;");
    copy_in_send(&mut stream, b"1\t7\n2\t8\n");
    assert_eq!(read_command_complete(&mut stream), "COPY 2");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "COPY g (a) FROM STDIN WITH (FORMAT csv, HEADER true);",
    );
    copy_in_send(&mut stream, b"a\n3\n");
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    read_ready_for_query(&mut stream);
    assert_eq!(
        select_rows(&mut stream, "SELECT a, b, c FROM g ORDER BY a;", 3),
        vec![
            vec![Some("1".into()), Some("2".into()), Some("7".into())],
            vec![Some("2".into()), Some("4".into()), Some("8".into())],
            vec![Some("3".into()), Some("6".into()), None],
        ]
    );
    // 列リストの生成列は、CopyInResponse を返す前に拒否される。
    send_simple_query(&mut stream, "COPY g (a, b) FROM STDIN;");
    expect_error_response_with_sqlstate(&mut stream, "428C9");
    read_ready_for_query(&mut stream);
}

/// 自動更新可能ビュー経由の INSERT・UPDATE が基表の生成列を計算する（psql 形）。
#[test]
fn psql_shaped_view_writes_compute_the_generated_column() {
    let (mut stream, _guard) = connect();
    exec(
        &mut stream,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    exec(&mut stream, "CREATE VIEW gv AS SELECT a, b FROM g;");
    assert_eq!(
        exec(&mut stream, "INSERT INTO gv (a) VALUES (3);"),
        "INSERT 0 1"
    );
    assert_eq!(exec(&mut stream, "UPDATE gv SET a = 4;"), "UPDATE 1");
    assert_eq!(
        select_rows(&mut stream, "SELECT a, b FROM g;", 1),
        cells(&[&["4", "8"]])
    );
    send_simple_query(&mut stream, "INSERT INTO gv (a, b) VALUES (1, 1);");
    expect_error_response_with_sqlstate(&mut stream, "428C9");
    read_ready_for_query(&mut stream);
}

/// BEFORE ROW トリガ付きの表への INSERT・UPDATE は、トリガの書き換えの後に生成列を計算する。BEFORE トリガの
/// WHEN が NEW の生成列を参照する定義は `42P17`（psql 形）。
#[test]
fn psql_shaped_before_trigger_runs_before_the_generated_computation() {
    let (mut stream, _guard) = connect();
    exec(&mut stream, "CREATE SCHEMA s;");
    exec(&mut stream, "SET search_path TO s, public;");
    exec(
        &mut stream,
        "CREATE TABLE s.g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    exec(
        &mut stream,
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.a := NEW.a + 10; RETURN NEW; END $$;",
    );
    exec(
        &mut stream,
        "CREATE TRIGGER tb BEFORE INSERT OR UPDATE ON s.g FOR EACH ROW EXECUTE FUNCTION bump();",
    );
    assert_eq!(
        exec(&mut stream, "INSERT INTO g (a) VALUES (1);"),
        "INSERT 0 1"
    );
    assert_eq!(
        select_rows(&mut stream, "SELECT a, b FROM g;", 1),
        cells(&[&["11", "22"]])
    );
    assert_eq!(exec(&mut stream, "UPDATE g SET a = 2;"), "UPDATE 1");
    assert_eq!(
        select_rows(&mut stream, "SELECT a, b FROM g;", 1),
        cells(&[&["12", "24"]])
    );
    send_simple_query(
        &mut stream,
        "CREATE TRIGGER tw BEFORE INSERT ON s.g FOR EACH ROW WHEN (NEW.b > 1) EXECUTE FUNCTION bump();",
    );
    expect_error_response_with_sqlstate(&mut stream, "42P17");
    read_ready_for_query(&mut stream);
}

/// 生成列の追加（Issue #3330。TABLE-50）を psql 形（末尾 `;` の単独 simple query）で流す。既存行は全行が
/// 計算され、SELECT が計算済みの値を返す。評価エラーは `22012` で返り、その後も表は変わらず接続は使える。
/// 意味論の確定オラクルは engine の `table50_generated_column_add_column`。
#[test]
fn psql_shaped_add_generated_column_backfills_existing_rows() {
    let (mut stream, _guard) = connect();
    for (sql, tag) in [
        ("CREATE TABLE g (a INTEGER, d INTEGER);", "CREATE TABLE"),
        ("INSERT INTO g (a, d) VALUES (1, 5), (2, 0);", "INSERT 0 2"),
        (
            "ALTER TABLE g ADD COLUMN b INTEGER GENERATED ALWAYS AS (a * 2) STORED;",
            "ALTER TABLE",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), tag, "{sql}");
        read_ready_for_query(&mut stream);
    }
    send_simple_query(&mut stream, "SELECT a, b FROM g ORDER BY a;");
    let _ = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("1".to_string()), Some("2".to_string())]
    );
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("2".to_string()), Some("4".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);

    // 評価エラー（`d = 0` の行）は `22012`。表は変わらず、同名の通常の列を追加できる。
    send_simple_query(
        &mut stream,
        "ALTER TABLE g ADD COLUMN q INTEGER GENERATED ALWAYS AS (10 / d) STORED;",
    );
    expect_error_response_with_sqlstate(&mut stream, "22012");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "SELECT q FROM g;");
    expect_error_response_with_sqlstate(&mut stream, "42703");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "ALTER TABLE g ADD COLUMN q INTEGER;");
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);
}
