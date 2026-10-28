//! NoSQL（HTTP）表層 `insert` op の移行済み表での `id` キー省略の結合テスト
//! （Issue #1700・ポインタ: `docs/spec` TABLE-23・TABLE-24・NOSQL-6・RECOVER-10・RLS-9）。
//!
//! 表は従来方式（暗黙の `id` あり）で作って閉じて開き直し、engine の移行で移行済み表にする
//! （設計記録 `docs/design/migrated-id-identity.md` D8・D9）。検証項目:
//!
//! - `id` キーを省略した複数行 `insert` が行順に採番する（テナントごとに独立）
//! - operation_id 付きの再送は `409`/`23505` で値を消費せず、内容違いは `22023`
//! - operation_id 省略の送信は別の値を採番する（台帳に残らない）
//! - 省略行と明示行の混在は `0A000`、明示 `"id": null` は `22000`、従来どおり明示 `id` も使える

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Visibility;

use common::*;
use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 従来方式の表 `items (n BIGINT)` を作って閉じ、開き直して移行済みにしたコア。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("http-insert-migrated-id");
    let guard = temp_db::CleanupGuard(path.clone());
    {
        let core = EngineCore::open(&path).expect("open");
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session_legacy(
            &ctx("tenant-a"),
            &mut session,
            "CREATE TABLE items (n BIGINT)",
        )
        .expect("create");
        core.execute_sql_in_session_legacy(
            &ctx("tenant-a"),
            &mut SessionState::default(),
            "INSERT INTO items (id, n) VALUES (10, 0)",
        )
        .expect("seed");
    }
    (
        Arc::new(EngineCore::open(&path).expect("reopen migrates")),
        guard,
    )
}

fn login_token(addr: std::net::SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let req = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp: HttpResponse = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &req,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match engine::json::parse_json(&text).expect("login body json") {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected token, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

struct Server {
    addr: std::net::SocketAddr,
    token_a: String,
    token_b: String,
}

fn spawn(core: Arc<EngineCore>) -> Server {
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = http_common::spawn_router_listener_with_engine(&users, SessionStore::new(), core);
    Server {
        addr,
        token_a: login_token(addr, "alice", "pw-alice"),
        token_b: login_token(addr, "bob", "pw-bob"),
    }
}

fn post(addr: std::net::SocketAddr, token: &str, body: &str) -> HttpResponse {
    let req = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(addr, &req, AfterWrite::HalfClose))
}

fn text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn ok(resp: &HttpResponse) {
    assert_eq!(resp.status, 200, "{resp:?} {}", text(resp));
}

fn code(resp: &HttpResponse) -> String {
    assert_ne!(resp.status, 200, "must fail: {}", text(resp));
    http_common::wire_code_of(resp)
}

/// 自テナントの `id` を昇順で返す（engine の SQL 表層で読む）。
fn ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let mut v: Vec<u64> = core
        .execute_sql(&ctx(tenant), "SELECT id FROM items LIMIT 1000")
        .expect("select")
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn omitted_id_rows_are_assigned_in_row_order_per_tenant() {
    let (core, _g) = new_core();
    let srv = spawn(Arc::clone(&core));
    ok(&post(
        srv.addr,
        &srv.token_a,
        r#"{"op":"insert","table":"items","rows":[{"n":1},{"n":2},{"n":3}]}"#,
    ));
    assert_eq!(ids(&core, "tenant-a"), vec![10, 11, 12, 13]);
    ok(&post(
        srv.addr,
        &srv.token_b,
        r#"{"op":"insert","table":"items","rows":[{"n":1},{"n":2}]}"#,
    ));
    assert_eq!(ids(&core, "tenant-b"), vec![1, 2]);
    // 単一行形も採番する。
    ok(&post(
        srv.addr,
        &srv.token_a,
        r#"{"op":"insert","table":"items","rows":[{"n":4}]}"#,
    ));
    assert_eq!(ids(&core, "tenant-a"), vec![10, 11, 12, 13, 14]);
}

#[test]
fn resend_with_operation_id_is_23505_without_consuming_values() {
    let (core, _g) = new_core();
    let srv = spawn(Arc::clone(&core));
    let body = r#"{"op":"insert","table":"items","rows":[{"n":1},{"n":2}],"operation_id":"h-1"}"#;
    ok(&post(srv.addr, &srv.token_a, body));
    assert_eq!(code(&post(srv.addr, &srv.token_a, body)), "23505");
    assert_eq!(
        code(&post(
            srv.addr,
            &srv.token_a,
            r#"{"op":"insert","table":"items","rows":[{"n":1},{"n":3}],"operation_id":"h-1"}"#
        )),
        "22023"
    );
    assert_eq!(ids(&core, "tenant-a"), vec![10, 11, 12]);
    ok(&post(
        srv.addr,
        &srv.token_a,
        r#"{"op":"insert","table":"items","rows":[{"n":9}]}"#,
    ));
    assert_eq!(ids(&core, "tenant-a"), vec![10, 11, 12, 13]);
}

#[test]
fn without_operation_id_each_send_assigns_new_values() {
    let (core, _g) = new_core();
    let srv = spawn(Arc::clone(&core));
    let body = r#"{"op":"insert","table":"items","rows":[{"n":1},{"n":2}]}"#;
    ok(&post(srv.addr, &srv.token_a, body));
    ok(&post(srv.addr, &srv.token_a, body));
    assert_eq!(ids(&core, "tenant-a"), vec![10, 11, 12, 13, 14]);
}

#[test]
fn mixed_batch_and_invalid_id_forms_are_rejected() {
    let (core, _g) = new_core();
    let srv = spawn(Arc::clone(&core));
    // 省略行と明示行の混在は 0A000（行は書かれない）。
    assert_eq!(
        code(&post(
            srv.addr,
            &srv.token_a,
            r#"{"op":"insert","table":"items","rows":[{"n":1},{"id":50,"n":2}]}"#
        )),
        "0A000"
    );
    // 明示 `"id": null`・非整数は従来どおり 22000。
    for row in [r#"{"id":null,"n":1}"#, r#"{"id":"7","n":1}"#] {
        assert_eq!(
            code(&post(
                srv.addr,
                &srv.token_a,
                &format!(r#"{{"op":"insert","table":"items","rows":[{row}]}}"#)
            )),
            "22000",
            "{row}"
        );
    }
    assert_eq!(ids(&core, "tenant-a"), vec![10]);
    // 明示 id のバッチは従来どおり。
    ok(&post(
        srv.addr,
        &srv.token_a,
        r#"{"op":"insert","table":"items","rows":[{"id":50,"n":1},{"id":51,"n":2}]}"#,
    ));
    assert_eq!(ids(&core, "tenant-a"), vec![10, 50, 51]);
}
