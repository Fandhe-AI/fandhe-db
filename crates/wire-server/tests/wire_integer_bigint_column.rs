//! `INTEGER` / `BIGINT` 列型（Issue #881、対象ビヘイビア: TABLE-13・TABLE-6・
//! TASK-196）の wire プロトコル経由・簡易クエリ検証（層 A）。ポインタ:
//! `docs/spec/05-tasks.md` TASK-196。
//!
//! `crates/engine/tests/column_type_integer.rs` が engine API レベルの契約
//! （境界値・範囲外 `22003`・型不一致 `22000`・RLS・台帳照合）を確定オラクルとして
//! 検証済みのため、本ファイルは simple query プロトコル越しに観測できることの
//! 確認に絞る: `DataRow` のテキスト表現・範囲外リテラルの `ErrorResponse`
//! （`22003`）・NoSQL 表層 `insert` の JSON 数値束縛（Issue #896・NOSQL-17。
//! `engine::sql::parser::bind_insert` へ写像し SQL 表層と同一の値を書き込む）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;
use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

fn new_core_with_integer_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-integer-bigint-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// `DataRow` は `INTEGER`／`BIGINT` 列を 10 進テキストとしてそのまま送出し、
/// `RowDescription` はそれぞれ `int4`（OID 23）／`int8`（OID 20）として公告する
/// （Issue #903 レビュー指摘: 一律 `text`（OID 25）で公告すると psql・ドライバ・
/// ORM が整数列を文字列として扱ってしまうため是正。`result_encoder.rs::
/// column_wire_type` が単一情報源）。
#[test]
fn wire_select_returns_integer_and_bigint_as_decimal_text() {
    let (core, _guard) = new_core_with_integer_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        &format!(
            "INSERT INTO docs (id, embedding, n, b) VALUES (1, '[0.1,0.2]', {}, {}) \
             USING OPERATION_ID 'op-1'",
            i32::MIN,
            i64::MAX
        ),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT n, b FROM docs WHERE id = 1 LIMIT 10");
    let columns = read_row_description_with_oids(&mut stream);
    assert_eq!(
        columns,
        vec![("n".to_string(), 23), ("b".to_string(), 20)],
        "INTEGER must announce OID 23 (int4), BIGINT must announce OID 20 (int8)"
    );
    let row = read_data_row(&mut stream);
    assert_eq!(
        row,
        vec![Some(i32::MIN.to_string()), Some(i64::MAX.to_string()),]
    );
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 範囲外の `INTEGER` リテラルは wire 越しに `22003`（`ErrorResponse`）として
/// 観測できる。
#[test]
fn wire_insert_with_out_of_range_integer_literal_returns_22003() {
    let (core, _guard) = new_core_with_integer_table();
    let mut stream = spawn_with_alice(core);

    let over_i32 = i64::from(i32::MAX) + 1;
    send_simple_query(
        &mut stream,
        &format!(
            "INSERT INTO docs (id, embedding, n, b) VALUES (1, '[0.1,0.2]', {over_i32}, 0) \
             USING OPERATION_ID 'op-1'"
        ),
    );
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);
}

/// 小数リテラルを `INTEGER` 列へ渡した場合は `22P02`（Issue #1187）。
#[test]
fn wire_insert_with_non_integer_literal_returns_22p02() {
    let (core, _guard) = new_core_with_integer_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1.5, 0) \
         USING OPERATION_ID 'op-1'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22P02");
    read_ready_for_query(&mut stream);
}

/// NoSQL 表層 `POST /v1/query`（`op: insert`）で `INTEGER`／`BIGINT` 列へ
/// JSON 数値を渡すと成功し、SQL 表層から読み戻すと同じ値が観測できる
/// （Issue #896・NOSQL-17。JSON 数値は `f64` を経由せず `InsertLiteral::Number`
/// の生テキストとして `engine::sql::parser::bind_insert` へ渡る）。
#[test]
fn nosql_insert_with_json_number_for_integer_column_succeeds_and_round_trips() {
    let (core, _guard) = new_core_with_integer_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );

    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let login_resp: HttpResponse = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &login_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(login_resp.status, 200, "login must succeed: {login_resp:?}");
    let login_text = String::from_utf8_lossy(&login_resp.body).into_owned();
    let token = match engine::json::parse_json(&login_text).expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let insert_body =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"n":5,"b":1}],"operation_id":"op-1"}"#;
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &insert_body.len().to_string()),
        ],
        insert_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "unexpected response: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body);
    assert!(
        text.contains("\"inserted\":1"),
        "expected inserted:1 in success body, got: {text}"
    );

    // SQL 表層から同一 core を読み戻し、NoSQL 表層の JSON 数値束縛
    // （`InsertLiteral::Number` の生テキスト経由）が SQL 表層の `bind_insert`
    // と同じ値を書き込んだことを固定する。
    let mut stream = spawn_with_alice(core);
    send_simple_query(&mut stream, "SELECT n, b FROM docs WHERE id = 1 LIMIT 10");
    let _ = read_row_description_with_oids(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("5".to_string()), Some("1".to_string())]);
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// NoSQL 表層 `POST /v1/query`（`op: insert`）の `BIGINT` 列へ `u64`/`i64` の
/// いずれにも収まらない整数リテラル（`18446744073709551616` = `u64::MAX + 1`）
/// を渡すと `22003`（範囲外）として拒否される（Issue #896・NOSQL-17 codex
/// レビュー指摘対応）。`engine::json::parse_number` はこの値を小数点・指数部
/// を含まない `JsonNumber::Float` へフォールバックさせるが、
/// `typed_json::map_json_to_literal` はこれを整数リテラルとして
/// `engine::sql::parser::bind_insert` へ委譲し、範囲判定は SQL 表層と同じ
/// `bind_integer_literal` に一本化する（型不一致 `42601` にはしない）。
#[test]
fn nosql_insert_with_integer_literal_overflowing_u64_returns_22003() {
    let (core, _guard) = new_core_with_integer_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );

    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let login_resp: HttpResponse = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &login_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(login_resp.status, 200, "login must succeed: {login_resp:?}");
    let login_text = String::from_utf8_lossy(&login_resp.body).into_owned();
    let token = match engine::json::parse_json(&login_text).expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"n":5,"b":18446744073709551616}],"operation_id":"op-1"}"#;
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &insert_body.len().to_string()),
        ],
        insert_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(
        http_common::wire_code_of(&resp),
        "22003",
        "unexpected response: {resp:?}"
    );
}
/// NoSQL の INTEGER／BIGINT 列へ小数・指数表記の JSON 数値を渡すと、SQL 表層の
/// 小数リテラルと同じ `22P02`（HTTP 400）で拒否される（Issue #1187・NOSQL-17。
/// 判定点は engine の `bind_integer_literal` に一本化）。
#[test]
fn nosql_insert_with_fractional_or_exponent_number_for_integer_returns_22p02() {
    let (core, _guard) = new_core_with_integer_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let login_resp: HttpResponse = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &login_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(login_resp.status, 200, "login must succeed: {login_resp:?}");
    let login_text = String::from_utf8_lossy(&login_resp.body).into_owned();
    let token = match engine::json::parse_json(&login_text).expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    for (i, (n, b)) in [("1.5", "1"), ("5", "1e3")].iter().enumerate() {
        let insert_body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[0.1,0.2],"n":{n},"b":{b}}}],"operation_id":"op-frac-{i}"}}"#
        );
        let request = http_common::build_request(
            "/v1/query",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Content-Type", "application/json"),
                ("Content-Length", &insert_body.len().to_string()),
            ],
            insert_body.as_bytes(),
        );
        let resp = http_common::parse_single_response(&http_common::send_raw(
            http_addr,
            &request,
            AfterWrite::HalfClose,
        ));
        assert_eq!(resp.status, 400, "case {i}: {resp:?}");
        assert_eq!(
            http_common::wire_code_of(&resp),
            "22P02",
            "case {i}: unexpected response: {resp:?}"
        );
    }
}
