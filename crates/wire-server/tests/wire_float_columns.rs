//! `REAL`／`DOUBLE PRECISION` 列型（Issue #882・TABLE-13・TASK-196）の簡易
//! クエリプロトコル経由（生バイトクライアント）検証。ポインタ:
//! `docs/spec/05-tasks.md` TASK-196・`docs/spec/04-behavior/data-model.md`。
//!
//! `wire_insert_operation_id.rs`・`wire1_simple_query.rs` と同じ流儀（生
//! `TcpStream`・`common::*` ヘルパー）で、INSERT→SELECT の `DataRow` テキスト・
//! `22003`（NumericOutOfRange）の `ErrorResponse` を wire フレーミング越しに
//! 確認する。REAL 列のテキスト表現は Issue #1173（WIRE-13）以降 PostgreSQL の
//! float4 出力形式（最短往復桁。例: `0.1f32` → `"0.1"`、`1e6` → `"1e+06"`）で、
//! DOUBLE PRECISION 列は f64 の最短往復表記のまま。

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
// `temp_db` は `http_common` が `pub mod temp_db;` として再エクスポートする
// ため、ここでは独自に `mod temp_db;` を宣言しない（clippy::duplicate_mod 回避）。
use http_common::temp_db;

fn new_core_with_metrics_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-float-columns-metrics");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "metrics",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("score", ColumnType::Real, false),
                ColumnDef::new("weight", ColumnType::Double, true),
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

/// INSERT→SELECT の `DataRow` テキストが正準表現で返ることを確認する
/// （`1.5` は REAL・DOUBLE のどちらでも `"1.5"`）。
#[test]
fn wire_insert_select_roundtrips_real_and_double_as_text() {
    let (core, _guard) = new_core_with_metrics_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO metrics (id, embedding, score, weight) \
         VALUES (1, '[0.1,0.2]', 1.5, 2.25) USING OPERATION_ID 'wire-op-1'",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT score, weight FROM metrics WHERE id = 1 LIMIT 1",
    );
    let _columns = read_row_description(&mut stream);
    let cells = read_data_row(&mut stream);
    assert_eq!(
        cells,
        vec![Some("1.5".to_string()), Some("2.25".to_string())]
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// nullable な `weight`（DOUBLE PRECISION）を省略した行は NULL（`DataRow` の
/// 負長）として返る。
#[test]
fn wire_select_returns_null_for_omitted_nullable_double_column() {
    let (core, _guard) = new_core_with_metrics_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO metrics (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
         USING OPERATION_ID 'wire-op-1'",
    );
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT weight FROM metrics WHERE id = 1 LIMIT 1",
    );
    read_row_description(&mut stream);
    let cells = read_data_row(&mut stream);
    assert_eq!(cells, vec![None]);
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 範囲外の REAL リテラルは `22003`（NumericOutOfRange）の `ErrorResponse` で
/// 拒否され、接続は維持される。
#[test]
fn wire_insert_rejects_out_of_range_real_literal_with_22003() {
    let (core, _guard) = new_core_with_metrics_table();
    let mut stream = spawn_with_alice(core);

    let huge = "4".to_string() + &"0".repeat(39);
    send_simple_query(
        &mut stream,
        &format!(
            "INSERT INTO metrics (id, embedding, score) VALUES (1, '[0.1,0.2]', {huge}) \
             USING OPERATION_ID 'wire-op-1'"
        ),
    );
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);

    // 接続は維持され、続く正規の INSERT が成功すること。
    send_simple_query(
        &mut stream,
        "INSERT INTO metrics (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
         USING OPERATION_ID 'wire-op-1'",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// NoSQL `insert` op へ float 列の JSON 数値を渡すと成功し、SQL 表層から
/// 読み戻すと同じ値が観測できる（Issue #896・NOSQL-17。`nosql6_insert.rs` の
/// `spawn_both`／`query` と同じ流儀（`http_common::spawn_router_listener_with_engine`
/// 経由）で production ルータを検証する）。
#[test]
fn nosql_insert_accepts_real_column_value_and_round_trips() {
    use engine::json::{parse_json, JsonValue};
    use http_common::AfterWrite;
    use wire_server::http::session::store::SessionStore;

    let path = temp_db::unique_db_path("wire-float-columns-nosql");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "metrics",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("score", ColumnType::Real, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

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
    let login_resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &login_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(login_resp.status, 200, "login must succeed: {login_resp:?}");
    let login_body_text = String::from_utf8_lossy(&login_resp.body).into_owned();
    let token = match parse_json(&login_body_text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let insert_body = br#"{"op":"insert","table":"metrics","operation_id":"nosql-op-1","rows":[{"id":1,"embedding":[0.1,0.2],"score":1.5}]}"#;
    let insert_request = http_common::build_request(
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
        &insert_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "unexpected response: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body);
    assert!(
        text.contains("\"inserted\":1"),
        "expected inserted:1 in success body, got: {text}"
    );

    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "SELECT score FROM metrics WHERE id = 1 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let cells = read_data_row(&mut stream);
    assert_eq!(cells, vec![Some("1.5".to_string())]);
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}
/// REAL 列の値・式・集計は PostgreSQL の float4 出力形式で返る（Issue #1173・
/// WIRE-13）: `0.1` は `0.1`（f64 拡張の `0.10000000149011612` ではない）、
/// `1000000` は `1e+06`。`SUM(REAL)` は real で、`0.1 + 0.2` は `0.3`
/// （f32 へ丸めた結果）になる。DOUBLE PRECISION 列は float8 形式（別テスト参照）。
#[test]
fn wire_real_text_follows_postgresql_float4_output() {
    let (core, _guard) = new_core_with_metrics_table();
    let mut stream = spawn_with_alice(core);

    for (id, score, op) in [(1, "0.1", "wire-op-1"), (2, "0.2", "wire-op-2")] {
        send_simple_query(
            &mut stream,
            &format!(
                "INSERT INTO metrics (id, embedding, score, weight) \
                 VALUES ({id}, '[0.1,0.2]', {score}, 0.1) USING OPERATION_ID '{op}'"
            ),
        );
        read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }
    send_simple_query(
        &mut stream,
        "INSERT INTO metrics (id, embedding, score) VALUES (3, '[0.1,0.2]', 1000000) \
         USING OPERATION_ID 'wire-op-3'",
    );
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT score, weight FROM metrics WHERE id = 1 LIMIT 1",
    );
    read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("0.1".to_string()), Some("0.1".to_string())]
    );
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT score FROM metrics WHERE id = 3 LIMIT 1",
    );
    read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("1e+06".to_string())]);
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT SUM(score) AS s FROM metrics WHERE id < 3",
    );
    let columns = read_row_description_with_oids(&mut stream);
    assert_eq!(columns, vec![("s".to_string(), 700)]);
    assert_eq!(read_data_row(&mut stream), vec![Some("0.3".to_string())]);
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// DOUBLE PRECISION 列のテキスト出力は PostgreSQL の float8 出力形式（Issue #1343・
/// WIRE-13 のポインタ）。大小指数は `1e+20`／`1e-05` 形式、通常域は固定小数。
#[test]
fn wire_double_text_follows_postgresql_float8_output() {
    let (core, _guard) = new_core_with_metrics_table();
    let mut stream = spawn_with_alice(core);

    for (id, weight) in [
        (1, "1e20"),
        (2, "0.00001"),
        (3, "0.1"),
        (4, "123456789012345"),
        (5, "1e15"),
    ] {
        send_simple_query(
            &mut stream,
            &format!(
                "INSERT INTO metrics (id, embedding, score, weight) \
                 VALUES ({id}, '[0.1,0.2]', 1, {weight}) USING OPERATION_ID 'wire-d-{id}'"
            ),
        );
        read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }

    for (id, expected) in [
        (1, "1e+20"),
        (2, "1e-05"),
        (3, "0.1"),
        (4, "123456789012345"),
        (5, "1e+15"),
    ] {
        send_simple_query(
            &mut stream,
            &format!("SELECT weight FROM metrics WHERE id = {id} LIMIT 1"),
        );
        let columns = read_row_description_with_oids(&mut stream);
        assert_eq!(columns, vec![("weight".to_string(), 701)]);
        assert_eq!(
            read_data_row(&mut stream),
            vec![Some(expected.to_string())],
            "id={id}"
        );
        read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }
}
