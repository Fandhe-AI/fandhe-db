//! `POST /v1/query` の `insert`・`update`・`upsert`（述語形・`RETURNING` 付きを含む）が生成列
//! `GENERATED ALWAYS AS (<式>) STORED` を計算して保存し、生成列への値の指定を `428C9` で拒否することを
//! production ルータ越しに固定する層 A 結合テスト（Issue #3329。ポインタ: `docs/spec/04-behavior/` の
//! TABLE-50・NOSQL-6・NOSQL-12・NOSQL-21・RECOVER-11、設計記録: `docs/design/generated-column.md`）。
//!
//! NoSQL 表層は SQL 表層と同じ束縛関数・同じ実行器を通る（第 2 の経路を作らない）ため、結果は同じ状態に
//! 対する SQL の結果と一致する。HTTP 層（`wire_server::http::query`）に生成列の特別扱いは無い。

use crate::common;
use crate::http_common;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TENANT_A: &str = "tenant-a";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `gdocs (k BIGINT PRIMARY KEY, n BIGINT, d BIGINT GENERATED ALWAYS AS (n * 2) STORED)` を持つ `EngineCore` を作る。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql50-generated");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx(TENANT_A),
        &mut session,
        "CREATE TABLE gdocs (k BIGINT PRIMARY KEY, n BIGINT, d BIGINT GENERATED ALWAYS AS (n * 2) STORED)",
    )
    .expect("create table");
    (Arc::new(core), guard)
}

/// tenant-a の `(k, n, d)` を k 順に `"k:n:d"` 形の文字列の列にして返す。
fn state(core: &EngineCore) -> Vec<String> {
    let outcome = core
        .execute_sql_in_session(
            &ctx(TENANT_A),
            &mut SessionState::default(),
            "SELECT k, n, d FROM gdocs ORDER BY k LIMIT 1000",
        )
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    let show = |c: &Cell| match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        other => format!("{other:?}"),
    };
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(show).collect::<Vec<_>>().join(":"))
        .collect()
}

fn login_alice(core: Arc<EngineCore>) -> (SocketAddr, String) {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);
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
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => (addr, s),
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected json object, got {other:?}"),
    }
}

fn query(addr: SocketAddr, token: &str, body: &str) -> HttpResponse {
    let length = body.len().to_string();
    let auth = format!("Bearer {token}");
    let headers: Vec<(&str, &str)> = vec![
        ("Authorization", &auth),
        ("Content-Type", "application/json"),
        ("Content-Length", &length),
    ];
    let request = http_common::build_request("/v1/query", &headers, body.as_bytes());
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

#[test]
fn insert_and_update_compute_the_generated_column() {
    let (core, _g) = new_core();
    let (addr, token) = login_alice(core.clone());
    let resp = query(
        addr,
        &token,
        r#"{"op":"insert","table":"gdocs","rows":[{"k":1,"n":5},{"k":2,"n":6}]}"#,
    );
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(state(&core), ["1:5:10", "2:6:12"]);
    // 単一行の update（`where` 形）。
    let resp = query(
        addr,
        &token,
        r#"{"op":"update","table":"gdocs","set":{"n":7},"where":{"k":1}}"#,
    );
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(state(&core), ["1:7:14", "2:6:12"]);
    // 述語形（`filter`）の update。
    let resp = query(
        addr,
        &token,
        r#"{"op":"update","table":"gdocs","set":{"n":9},"filter":[{"column":"n","op":"not_null"}]}"#,
    );
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(state(&core), ["1:9:18", "2:9:18"]);
}

#[test]
fn upsert_computes_on_insert_and_on_do_update() {
    let (core, _g) = new_core();
    let (addr, token) = login_alice(core.clone());
    let body = r#"{"op":"upsert","table":"gdocs","rows":[{"k":1,"n":4}],"on_conflict":{"columns":["k"],"do":"update"}}"#;
    assert_eq!(query(addr, &token, body).status, 200);
    assert_eq!(state(&core), ["1:4:8"]);
    let body = r#"{"op":"upsert","table":"gdocs","rows":[{"k":1,"n":10}],"on_conflict":{"columns":["k"],"do":"update"}}"#;
    assert_eq!(query(addr, &token, body).status, 200);
    assert_eq!(state(&core), ["1:10:20"]);
}

#[test]
fn generated_column_values_are_rejected_with_428c9_and_http_400() {
    let (core, _g) = new_core();
    let (addr, token) = login_alice(core.clone());
    assert_eq!(
        query(
            addr,
            &token,
            r#"{"op":"insert","table":"gdocs","rows":[{"k":1,"n":5}]}"#
        )
        .status,
        200
    );
    let before = state(&core);
    for body in [
        r#"{"op":"insert","table":"gdocs","rows":[{"k":2,"n":5,"d":1}]}"#,
        r#"{"op":"update","table":"gdocs","set":{"d":1},"where":{"k":1}}"#,
        r#"{"op":"update","table":"gdocs","set":{"d":1},"filter":[{"column":"n","op":"not_null"}]}"#,
        r#"{"op":"upsert","table":"gdocs","rows":[{"k":3,"n":5,"d":1}],"on_conflict":{"columns":["k"],"do":"update"}}"#,
    ] {
        let resp = query(addr, &token, body);
        assert_eq!(resp.status, 400, "{body}: {resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "428C9", "{body}");
        let text = body_text(&resp);
        assert!(!text.contains(TENANT_A), "{text}");
    }
    assert_eq!(state(&core), before, "rejected writes must leave no trace");
}
