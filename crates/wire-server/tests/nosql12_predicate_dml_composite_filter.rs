//! `POST /v1/query`（`op: "update"`／`op: "delete"`）の述語形 `filter` が、`ARRAY`／`JSON`／
//! `JSONB` 列の `eq`／`ne`／`in`（`not`・`or` 経由を含む）を SQL 表層の述語形 DML と同じ
//! 結果集合・同じ台帳照合（`content_hash`。`23505`／`22023`）で受理することを production
//! ルータ経由で固定する層 A 結合テスト（Issue #1410。対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-12・NOSQL-14・NOSQL-17、
//! `docs/spec/04-behavior/sql-surface.md` SQL-19・SQL-24、
//! `docs/spec/04-behavior/recovery.md` RECOVER-10）。
//!
//! 役割分担: `filter.rs` の unit tests は JSON → `WherePredicate` の写像（AST 同一性）を、
//! 本ファイルは SQL の `DELETE` と NoSQL の `delete` の影響行数・台帳照合の一致を wire 越しに
//! 固定する（検索系 3 op の `in` は `nosql14_query_composite_in_filter.rs`。Issue #1429）。
//! SQL 側のリテラルは NoSQL の正規直列化と同じ綴りで書く（台帳照合は綴り一致が前提）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "items";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("text[] type")),
                true,
            ),
            ColumnDef::new(
                "ints",
                ColumnType::Array(ArrayType::new(ArrayElemType::Integer, 4).expect("int[] type")),
                true,
            ),
            ColumnDef::new("doc", ColumnType::Jsonb, true),
            ColumnDef::new("j", ColumnType::Json, true),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql12-predicate-dml-composite");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

struct Both {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn login(http_addr: std::net::SocketAddr, user: &str, password: &str) -> String {
    let login_body = format!(r#"{{"user":"{user}","password":"{password}"}}"#).into_bytes();
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        &login_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

/// HTTP（alice／tenant-a と bob／tenant-b）・SQL wire（alice）を同一 `core` 上で起動する。
fn spawn_both(core: Arc<EngineCore>) -> (Both, Both, std::net::TcpStream) {
    let users_path = common::write_user_store_file(&[
        ("alice", TENANT_A, "pw-alice"),
        ("bob", TENANT_B, "pw-bob"),
    ]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let alice = Both {
        http_addr,
        token: login(http_addr, "alice", "pw-alice"),
    };
    let bob = Both {
        http_addr,
        token: login(http_addr, "bob", "pw-bob"),
    };
    let sql_addr = common::spawn_server_with_engine(&users_path, core);
    let sql_stream = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");
    (alice, bob, sql_stream)
}

fn query(who: &Both, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", who.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        who.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// seed 4 行（id 4 は tags・ints・doc・j が NULL で、述語は UNKNOWN として除外される）。
fn seed(who: &Both, tag: &str) {
    let rows = [
        r#"{"id":1,"embedding":[0.1,0.2,0.3],"lang":"ja","tags":["a"],"ints":[1,2],"doc":{"k":1},"j":{"k":1}}"#,
        r#"{"id":2,"embedding":[0.1,0.2,0.3],"lang":"en","tags":["b","c"],"ints":[3],"doc":[1,2],"j":[1,2]}"#,
        r#"{"id":3,"embedding":[0.1,0.2,0.3],"lang":"fr","tags":["a","b"],"ints":[],"doc":{"k":2},"j":{"k":2}}"#,
        r#"{"id":4,"embedding":[0.1,0.2,0.3],"lang":"de"}"#,
    ];
    for (i, row) in rows.iter().enumerate() {
        let body = format!(
            r#"{{"op":"insert","table":"{TABLE}","rows":[{row}],"operation_id":"seed-{tag}-{i}"}}"#
        );
        let resp = query(who, body.as_bytes());
        assert_eq!(resp.status, 200, "seed {i}: {resp:?}");
    }
}

fn delete_body(filter_json: &str, op_id: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"delete","table":"{TABLE}","filter":{filter_json},"operation_id":"{op_id}"}}"#
    )
    .into_bytes()
}

fn assert_deleted(resp: &HttpResponse, expected: usize, what: &str) {
    assert_eq!(resp.status, 200, "{what}: resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(
        body.contains(&format!(r#""deleted":{expected}"#)),
        "{what}: {body}"
    );
}

/// (SQL の WHERE 句, NoSQL の filter JSON, 一致する行数)。
const PARITY_CASES: [(&str, &str, usize); 16] = [
    (
        r#"tags = '{"a"}'"#,
        r#"[{"column":"tags","op":"eq","value":["a"]}]"#,
        1,
    ),
    (
        r#"NOT tags = '{"a"}'"#,
        r#"[{"column":"tags","op":"ne","value":["a"]}]"#,
        2,
    ),
    (
        r#"NOT tags = '{"a"}'"#,
        r#"[{"not":{"column":"tags","op":"eq","value":["a"]}}]"#,
        2,
    ),
    (
        r#"tags IN ('{"a"}', '{"b","c"}')"#,
        r#"[{"column":"tags","op":"in","value":[["a"],["b","c"]]}]"#,
        2,
    ),
    (
        r#"NOT tags IN ('{"a"}', '{"b","c"}')"#,
        r#"[{"not":{"column":"tags","op":"in","value":[["a"],["b","c"]]}}]"#,
        1,
    ),
    (
        "ints = '{1,2}'",
        r#"[{"column":"ints","op":"eq","value":[1,2]}]"#,
        1,
    ),
    (
        "ints IN ('{1,2}', '{3}')",
        r#"[{"column":"ints","op":"in","value":[[1,2],[3]]}]"#,
        2,
    ),
    (
        r#"doc = '{"k":1}'"#,
        r#"[{"column":"doc","op":"eq","value":{"k":1}}]"#,
        1,
    ),
    (
        r#"NOT doc = '{"k":1}'"#,
        r#"[{"column":"doc","op":"ne","value":{"k":1}}]"#,
        2,
    ),
    (
        r#"doc IN ('{"k":1}', '[1,2]')"#,
        r#"[{"column":"doc","op":"in","value":[{"k":1},[1,2]]}]"#,
        2,
    ),
    (
        r#"NOT doc IN ('{"k":1}', '[1,2]')"#,
        r#"[{"not":{"column":"doc","op":"in","value":[{"k":1},[1,2]]}}]"#,
        1,
    ),
    (
        r#"j = '{"k":2}'"#,
        r#"[{"column":"j","op":"eq","value":{"k":2}}]"#,
        1,
    ),
    (
        r#"tags = '{"a"}' OR doc = '{"k":2}'"#,
        r#"[{"or":[{"column":"tags","op":"eq","value":["a"]},{"column":"doc","op":"eq","value":{"k":2}}]}]"#,
        2,
    ),
    (
        r#"NOT (tags = '{"a"}' OR doc = '[1,2]')"#,
        r#"[{"not":{"or":[{"column":"tags","op":"eq","value":["a"]},{"column":"doc","op":"eq","value":[1,2]}]}}]"#,
        1,
    ),
    (
        r#"tags IN ('{"a"}', '{"a","b"}') AND lang = 'fr'"#,
        r#"[{"column":"tags","op":"in","value":[["a"],["a","b"]]},{"column":"lang","op":"eq","value":"fr"}]"#,
        1,
    ),
    (
        r#"doc = '{"k":1}' OR lang = 'de'"#,
        r#"[{"or":[{"column":"doc","op":"eq","value":{"k":1}},{"column":"lang","op":"eq","value":"de"}]}]"#,
        2,
    ),
];

/// SQL → NoSQL／NoSQL → SQL の双方向で台帳が `23505` になり、影響行数が一致する。
#[test]
fn composite_delete_parity_with_sql_resend_and_result_set() {
    for (i, (where_clause, filter_json, expected)) in PARITY_CASES.iter().enumerate() {
        let (core, _guard) = new_core();
        let (alice, _bob, mut sql) = spawn_both(core);
        seed(&alice, &format!("a{i}"));
        let op_id = format!("n1410-sql-first-{i}");
        common::send_simple_query(
            &mut sql,
            &format!("DELETE FROM {TABLE} WHERE {where_clause} USING OPERATION_ID '{op_id}'"),
        );
        let tag = common::read_command_complete(&mut sql);
        assert_eq!(tag, format!("DELETE {expected}"), "{where_clause}");
        common::read_ready_for_query(&mut sql);
        let resp = query(&alice, &delete_body(filter_json, &op_id));
        assert_eq!(resp.status, 409, "{filter_json}: {resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "23505", "{filter_json}");

        let (core, _guard) = new_core();
        let (alice, _bob, mut sql) = spawn_both(core);
        seed(&alice, &format!("b{i}"));
        let op_id = format!("n1410-nosql-first-{i}");
        let resp = query(&alice, &delete_body(filter_json, &op_id));
        assert_deleted(&resp, *expected, filter_json);
        common::send_simple_query(
            &mut sql,
            &format!("DELETE FROM {TABLE} WHERE {where_clause} USING OPERATION_ID '{op_id}'"),
        );
        common::expect_error_response_with_sqlstate(&mut sql, "23505");
        common::read_ready_for_query(&mut sql);
    }
}

/// 同一 `operation_id` で値が異なる場合は `22023`。
#[test]
fn composite_delete_content_mismatch_is_22023() {
    let (core, _guard) = new_core();
    let (alice, _bob, mut sql) = spawn_both(core);
    seed(&alice, "m");
    common::send_simple_query(
        &mut sql,
        &format!(
            r#"DELETE FROM {TABLE} WHERE tags = '{{"a"}}' USING OPERATION_ID 'n1410-mismatch'"#
        ),
    );
    let _ = common::read_command_complete(&mut sql);
    common::read_ready_for_query(&mut sql);
    let resp = query(
        &alice,
        &delete_body(
            r#"[{"column":"tags","op":"eq","value":["b"]}]"#,
            "n1410-mismatch",
        ),
    );
    assert_eq!(http_common::wire_code_of(&resp), "22023", "resp={resp:?}");
}

/// `update` でも合成列の述語（`or` 内の `in` と `eq`）を受理し、件数が一致する。
#[test]
fn composite_update_accepts_or_of_in_and_eq() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core);
    seed(&alice, "u");
    let resp = query(
        &alice,
        br#"{"op":"update","table":"items","set":{"lang":"zz"},"filter":[{"or":[{"column":"tags","op":"in","value":[["a"],["b","c"]]},{"column":"doc","op":"eq","value":{"k":2}}]}],"operation_id":"n1410-upd"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        String::from_utf8_lossy(&resp.body).contains(r#""updated":3"#),
        "{resp:?}"
    );
}

/// 型不一致は `42601`、上限超過は `54000`。
#[test]
fn composite_in_type_mismatch_is_42601_and_limits_are_54000() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core);
    seed(&alice, "t");
    for (i, filter) in [
        r#"[{"column":"tags","op":"in","value":["{a}"]}]"#,
        r#"[{"column":"doc","op":"in","value":[1]}]"#,
        r#"[{"column":"lang","op":"in","value":[["a"]]}]"#,
    ]
    .iter()
    .enumerate()
    {
        let resp = query(&alice, &delete_body(filter, &format!("n1410-tm-{i}")));
        assert_eq!(http_common::wire_code_of(&resp), "42601", "{filter}");
    }
    let items = vec!["{}"; 257].join(",");
    let resp = query(
        &alice,
        &delete_body(
            &format!(r#"[{{"column":"doc","op":"in","value":[{items}]}}]"#),
            "n1410-limit-in",
        ),
    );
    assert_eq!(http_common::wire_code_of(&resp), "54000", "{resp:?}");
    let resp = query(
        &alice,
        &delete_body(
            r#"[{"column":"tags","op":"eq","value":["a","b","c","d","e"]}]"#,
            "n1410-limit-len",
        ),
    );
    assert_eq!(http_common::wire_code_of(&resp), "54000", "{resp:?}");
}

/// テナント境界: 合成列の `not in`／`ne` でも他テナントの行は削除されない。
#[test]
fn composite_dml_does_not_cross_tenant_boundary() {
    let (core, _guard) = new_core();
    let (alice, bob, _sql) = spawn_both(core.clone());
    seed(&alice, "ta");
    seed(&bob, "tb");
    let resp = query(
        &alice,
        &delete_body(
            r#"[{"not":{"column":"tags","op":"in","value":[["zzz"]]}},{"column":"doc","op":"ne","value":{"k":99}}]"#,
            "n1410-tenant",
        ),
    );
    // 自テナントの tags・doc が非 NULL の 3 行だけ（NULL 行は UNKNOWN で除外）。
    assert_deleted(&resp, 3, "tenant-a composite delete");
    let ctx = PolicyContext::with_visibilities(TENANT_B, [Visibility::Public, Visibility::Private])
        .expect("valid tenant-b ctx");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT lang FROM items LIMIT 100")
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 4, "tenant-b rows must be untouched");
}
