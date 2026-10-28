//! `ENUM` 列型（TABLE-14・TASK-198、Issue #890）の wire・NoSQL 結合テスト。
//! ポインタ: `docs/spec/05-tasks.md` TASK-198・`docs/spec/04-behavior/
//! wire-protocol.md` WIRE-13・`docs/spec/04-behavior/nosql.md` NOSQL-17。
//!
//! `crates/engine/tests/enum_column.rs`（engine API 直接）とは独立に、
//! **wire フレーミング越し**の観測に徹する（`wire_bytea_column.rs` と同じ
//! 役割分担）:
//! - SQL 表層（生バイトクライアント）: ラベル文字列の往復一致・語彙外の `22P02`
//! - NoSQL 表層（HTTP）: JSON string 表現での insert/update・SQL とのパリティ・
//!   拒否経路（語彙外は `22P02`、非文字列は `42601`）

use crate::common;
use crate::http_common;

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
const ENUM_TYPE: &str = "mood";

fn schema(storage: &Storage) -> TableSchema {
    let def = storage
        .create_enum_type(
            ENUM_TYPE,
            vec![
                "happy".to_string(),
                "sad".to_string(),
                "neutral".to_string(),
            ],
        )
        .expect("create enum type");
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("mood", ColumnType::Enum(def), true),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-enum-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = schema(&storage);
    storage.create_table(&schema).expect("create table");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

// --- SQL 表層: ラベル文字列の往復・語彙外の拒否 --------------------------------

#[test]
fn wire_text_select_returns_label_and_round_trips() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, mood) VALUES (1, '[0.1,0.2,0.3]', 'happy') \
         USING OPERATION_ID 'op-insert-1'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(&mut alice, "SELECT mood FROM docs WHERE id = 1 LIMIT 1");
    common::read_row_description(&mut alice);
    let row = common::read_data_row(&mut alice);
    assert_eq!(row, vec![Some("happy".to_string())]);
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);
}

#[test]
fn wire_null_and_present_enum_are_distinguishable() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, mood) VALUES (1, '[0.1,0.2,0.3]', 'sad') \
         USING OPERATION_ID 'op-present'",
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

    common::send_simple_query(&mut alice, "SELECT id, mood FROM docs LIMIT 100");
    common::read_row_description(&mut alice);
    let mut rows = Vec::new();
    for _ in 0..2 {
        rows.push(common::read_data_row(&mut alice));
    }
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    let by_id: std::collections::BTreeMap<String, Option<String>> = rows
        .into_iter()
        .map(|r| {
            let id = r[0].clone().expect("id must not be NULL");
            (id, r[1].clone())
        })
        .collect();
    assert_eq!(by_id.get("1"), Some(&Some("sad".to_string())));
    assert_eq!(by_id.get("2"), Some(&None));
}

#[test]
fn wire_insert_rejects_out_of_vocabulary_label_and_keeps_connection_alive() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, mood) VALUES (1, '[0.1,0.2,0.3]', 'furious') \
         USING OPERATION_ID 'op-reject-1'",
    );
    common::expect_error_response_with_sqlstate(&mut alice, "22P02");
    common::read_ready_for_query(&mut alice);

    // 接続は維持され、続けて正しいクエリを送れる。
    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, mood) VALUES (2, '[0.1,0.2,0.3]', 'happy') \
         USING OPERATION_ID 'op-after-reject'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);
}

// --- NoSQL 表層: JSON string 表現・SQL とのパリティ・拒否経路 -------------------

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
fn nosql_insert_label_round_trips_through_scan() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"mood":"happy"}],"operation_id":"op-nosql-insert-1"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "insert must succeed: {resp:?}");

    let scan_body = br#"{"op":"scan","table":"docs","columns":["mood"],"limit":100}"#;
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
    assert_eq!(row0[0], JsonValue::String("happy".to_string()));
}

#[test]
fn nosql_update_set_label_matches_sql_read_back() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"op-seed"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "seed insert must succeed: {resp:?}");

    let update_body = br#"{"op":"update","table":"docs","where":{"id":1},"set":{"mood":"neutral"},"operation_id":"op-nosql-update-1"}"#;
    let resp = query(&both, update_body);
    assert_eq!(resp.status, 200, "update must succeed: {resp:?}");

    common::send_simple_query(&mut sql, "SELECT mood FROM docs WHERE id = 1 LIMIT 1");
    common::read_row_description(&mut sql);
    let row = common::read_data_row(&mut sql);
    assert_eq!(row, vec![Some("neutral".to_string())]);
    common::read_command_complete(&mut sql);
    common::read_ready_for_query(&mut sql);
}

#[test]
fn nosql_insert_rejects_out_of_vocabulary_label_with_22p02() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    let body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"mood":"furious"}],"operation_id":"op-reject"}"#;
    let resp = query(&both, body);
    assert_eq!(http_common::wire_code_of(&resp), "22P02", "resp: {resp:?}");
}

#[test]
fn nosql_insert_rejects_non_string_mood_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    for (label, mood_json) in [("number", "123"), ("array", "[1,2,3]"), ("boolean", "true")] {
        let body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[0.1,0.2,0.3],"mood":{mood_json}}}],"operation_id":"op-reject-{label}"}}"#
        );
        let resp = query(&both, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "case: {label}, resp: {resp:?}"
        );
    }
}

// --- NoSQL 表層: `search` op の `filter`（宣言的フィルタ API）経由の ENUM 等価 ---

#[test]
fn nosql_search_filter_on_enum_column_matches_and_rejects_out_of_vocabulary() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"mood":"happy"},{"id":2,"embedding":[0.4,0.5,0.6],"mood":"sad"}],"operation_id":"op-seed-filter"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "seed insert must succeed: {resp:?}");

    // 有効なラベルでの等価フィルタは、宣言的フィルタ API（`DeclarativeFilter::
    // equals`）を経由して TEXT 列と同じ束縛経路へ流れる。ENUM 型を型分岐で
    // 特別扱いしていないため（`filter.rs` は列型を見ずに `DeclarativeFilter::
    // equals(column, value)` を組み立てるだけ）、この経路が ENUM 列でも動作する
    // ことを固定する。
    let search_body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3],"limit":10,"columns":["id"],"filter":[{"column":"mood","op":"eq","value":"happy"}]}"#;
    let resp = query(&both, search_body);
    assert_eq!(
        resp.status, 200,
        "search with valid filter must succeed: {resp:?}"
    );
    let JsonValue::Object(obj) = json_body(&resp) else {
        panic!("expected object body");
    };
    let JsonValue::Array(rows) = obj.get("rows").expect("rows field").clone() else {
        panic!("expected rows array");
    };
    assert_eq!(rows.len(), 1, "only id=1 has mood='happy': {rows:?}");

    // 語彙外のラベルでの等価フィルタは `22P02` で拒否される
    // （`DeclarativeFilter::bind` の ENUM 分岐。Issue #890 D7）。
    let bad_filter_body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3],"limit":10,"columns":["id"],"filter":[{"column":"mood","op":"eq","value":"furious"}]}"#;
    let resp = query(&both, bad_filter_body);
    assert_eq!(http_common::wire_code_of(&resp), "22P02", "resp: {resp:?}");
}
