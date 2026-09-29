//! `BYTEA` 列型（TABLE-13・TASK-197、Issue #886）の wire・NoSQL 結合テスト。
//! ポインタ: `docs/spec/05-tasks.md` TASK-197・`docs/spec/04-behavior/
//! wire-protocol.md` WIRE-13・`docs/spec/04-behavior/nosql.md` NOSQL-17。
//!
//! `crates/engine/tests/bytea_column.rs`（engine API 直接）とは独立に、
//! **wire フレーミング越し**の観測に徹する（`wire_update_single_row.rs`・
//! `nosql6_insert.rs` と同じ役割分担）:
//! - SQL 表層（生バイトクライアント）: `\x` 16 進テキスト表現の往復一致
//! - NoSQL 表層（HTTP）: base64 の往復一致・SQL/NoSQL 間パリティ・拒否経路

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("blob", ColumnType::Bytea, true),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-bytea-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

// --- SQL 表層: `\x` 16 進テキスト表現の往復 ------------------------------------

#[test]
fn wire_text_select_returns_lowercase_hex_and_round_trips() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, blob) VALUES (1, '[0.1,0.2,0.3]', '\\xDEADBEEF') \
         USING OPERATION_ID 'op-insert-1'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(&mut alice, "SELECT blob FROM docs WHERE id = 1 LIMIT 1");
    common::read_row_description(&mut alice);
    let row = common::read_data_row(&mut alice);
    assert_eq!(row, vec![Some("\\xdeadbeef".to_string())]);
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    // 読み出したテキストをそのまま再度 INSERT し、同じバイト列になることを
    // 確認する（wire テキスト表現の往復一致。B5）。
    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, blob) VALUES (2, '[0.1,0.2,0.3]', '\\xdeadbeef') \
         USING OPERATION_ID 'op-insert-2'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(&mut alice, "SELECT blob FROM docs WHERE id = 2 LIMIT 1");
    common::read_row_description(&mut alice);
    let row2 = common::read_data_row(&mut alice);
    assert_eq!(row2, row);
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);
}

#[test]
fn wire_text_null_and_empty_bytea_are_distinguishable() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, blob) VALUES (1, '[0.1,0.2,0.3]', '\\x') \
         USING OPERATION_ID 'op-empty'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding) VALUES (2, '[0.1,0.2,0.3]') \
         USING OPERATION_ID 'op-null'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(&mut alice, "SELECT id, blob FROM docs LIMIT 100");
    common::read_row_description(&mut alice);
    let mut rows = Vec::new();
    for _ in 0..2 {
        rows.push(common::read_data_row(&mut alice));
    }
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    // DataRow 長は空バイト列で `\x`（非 NULL）、NULL 行は `None`（-1 長）。
    let by_id: std::collections::BTreeMap<String, Option<String>> = rows
        .into_iter()
        .map(|r| {
            let id = r[0].clone().expect("id must not be NULL");
            (id, r[1].clone())
        })
        .collect();
    assert_eq!(by_id.get("1"), Some(&Some("\\x".to_string())));
    assert_eq!(by_id.get("2"), Some(&None));
}

// --- NoSQL 表層: base64 の往復・SQL とのパリティ・拒否経路 ---------------------

struct Both {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn spawn_both(core: Arc<EngineCore>) -> (Both, std::net::TcpStream) {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let token = match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let sql_addr = common::spawn_server_with_engine(&users_path, core);
    let sql_stream = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");

    (Both { http_addr, token }, sql_stream)
}

fn query(both: &Both, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", both.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        both.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn json_body(resp: &HttpResponse) -> JsonValue {
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    parse_json(&text).expect("response body must be valid json")
}

#[test]
fn nosql_insert_base64_round_trips_through_scan() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    // "3q2+7w==" は [0xde, 0xad, 0xbe, 0xef] の標準 base64 表現。
    let insert_body =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"blob":"3q2+7w=="}],"operation_id":"op-nosql-insert-1"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "insert must succeed: {resp:?}");

    let scan_body = br#"{"op":"scan","table":"docs","columns":["blob"],"limit":100}"#;
    let resp = query(&both, scan_body);
    assert_eq!(resp.status, 200, "scan must succeed: {resp:?}");
    let JsonValue::Object(obj) = json_body(&resp) else {
        panic!("expected object body");
    };
    let JsonValue::Array(rows) = obj.get("rows").expect("rows field").clone() else {
        panic!("expected rows array");
    };
    assert_eq!(rows.len(), 1);
    let JsonValue::Array(row0) = &rows[0] else {
        panic!("expected row array");
    };
    assert_eq!(row0[0], JsonValue::String("3q2+7w==".to_string()));
}

#[test]
fn nosql_update_set_base64_matches_sql_read_back() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"op-seed"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "seed insert must succeed: {resp:?}");

    // "3q0=" は [0xde, 0xad] の標準 base64 表現。
    let update_body = br#"{"op":"update","table":"docs","where":{"id":1},"set":{"blob":"3q0="},"operation_id":"op-nosql-update-1"}"#;
    let resp = query(&both, update_body);
    assert_eq!(resp.status, 200, "update must succeed: {resp:?}");

    common::send_simple_query(&mut sql, "SELECT blob FROM docs WHERE id = 1 LIMIT 1");
    common::read_row_description(&mut sql);
    let row = common::read_data_row(&mut sql);
    assert_eq!(row, vec![Some("\\xdead".to_string())]);
    common::read_command_complete(&mut sql);
    common::read_ready_for_query(&mut sql);
}

#[test]
fn nosql_insert_rejects_non_string_with_42601_and_malformed_base64_with_22p02() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    for (label, blob_json, expected) in [
        ("number", "123", "42601"),
        ("array", "[1,2,3]", "42601"),
        ("boolean", "true", "42601"),
        // 不正な base64 は形式不正として `22P02`（Issue #1187）。
        ("malformed base64 (bad padding)", "\"3q2+7w=a\"", "22P02"),
        ("malformed base64 (alphabet)", "\"!!!!\"", "22P02"),
    ] {
        let body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[0.1,0.2,0.3],"blob":{blob_json}}}],"operation_id":"op-reject-{label}"}}"#
        );
        let resp = query(&both, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            expected,
            "case: {label}, resp: {resp:?}"
        );
    }
}
