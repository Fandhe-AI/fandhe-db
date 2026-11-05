//! `POST /v1/session` で絞ったスコープ（op・表・読み取り専用）が `POST /v1/query` の要求ごとに
//! 強制されることを、production ルータ（生バイトクライアント）経由で固定する層 A 結合テスト
//! （Issue #2101。対象ビヘイビア: HTTP-15・ERR-4・NOSQL-1。ポインタ:
//! `docs/spec/04-behavior/http-transport.md` HTTP-15・`docs/spec/04-behavior/error-format.md` ERR-4）。
//!
//! 固定する契約: スコープ外は実行前に `42501`（副作用なし）／応答から表の存在が分からない
//! （スコープ外の表は存在の有無に依らず同一応答、スコープ内の存在しない表はスコープなしと同じ
//! `42P01`）／`read_only` は許可リスト／スコープは権限を狭める方向にだけ働く。

use crate::common;
use crate::http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::auth::UserStore;
use wire_server::http::session::store::SessionStore;
use wire_server::limits::ConnectionLimiter;

const TENANT_A: &str = "tenant-a";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("http15-session-scope");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // 従来方式の表（暗黙の `id` あり）を engine 直で用意する。
    for name in ["docs", "other"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("lang", ColumnType::Text, true)],
            ))
            .expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// alice は DDL 権限あり、bob は DDL 権限なし（どちらも tenant-a）。
fn spawn(core: Arc<EngineCore>) -> std::net::SocketAddr {
    let users_path = common::write_user_store_file(&[
        ("alice", TENANT_A, "pw-alice"),
        ("bob", TENANT_A, "pw-bob"),
    ]);
    let store = UserStore::load_from_file(&users_path)
        .expect("valid user store")
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("alice is known");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let limiter = ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
    let router =
        wire_server::http::router::Router::with_engine(Arc::new(store), SessionStore::new(), core);
    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_router(
            listener,
            limiter,
            wire_server::limits::READ_TIMEOUT,
            router,
        );
    });
    addr
}

fn post(addr: std::net::SocketAddr, target: &str, token: Option<&str>, body: &str) -> HttpResponse {
    let len = body.len().to_string();
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![
        ("Content-Type", "application/json"),
        ("Content-Length", &len),
    ];
    if let Some(a) = auth.as_deref() {
        headers.push(("Authorization", a));
    }
    let req = http_common::build_request(target, &headers, body.as_bytes());
    http_common::parse_single_response(&http_common::send_raw(addr, &req, AfterWrite::HalfClose))
}

/// `scope_json` は `"scope":{...}` 片（空文字ならスコープなし）。
fn login(addr: std::net::SocketAddr, user: &str, scope_json: &str) -> String {
    let extra = if scope_json.is_empty() {
        String::new()
    } else {
        format!(",{scope_json}")
    };
    let body = format!(r#"{{"user":"{user}","password":"pw-{user}"{extra}}}"#);
    let resp = post(addr, "/v1/session", None, &body);
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).expect("login json") {
        JsonValue::Object(mut o) => match o.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected token, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

fn q(addr: std::net::SocketAddr, token: &str, body: &str) -> HttpResponse {
    post(addr, "/v1/query", Some(token), body)
}

fn assert_scope_denied(resp: &HttpResponse) {
    assert_eq!(resp.status, 403, "got: {resp:?}");
    assert_eq!(http_common::wire_code_of(resp), "42501", "got: {resp:?}");
}

fn row_count(addr: std::net::SocketAddr, token: &str, table: &str) -> usize {
    let r = q(
        addr,
        token,
        &format!(r#"{{"op":"scan","table":"{table}","limit":100}}"#),
    );
    assert_eq!(r.status, 200, "got: {r:?}");
    let text = String::from_utf8_lossy(&r.body).into_owned();
    match parse_json(&text).expect("scan json") {
        JsonValue::Object(mut top) => match top.remove("rows") {
            Some(JsonValue::Array(rows)) => rows.len(),
            other => panic!("expected rows array, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

#[test]
fn op_axis_rejects_ops_outside_the_scope_without_echo() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let t = login(addr, "alice", r#""scope":{"ops":["scan"]}"#);
    let ok = q(addr, &t, r#"{"op":"scan","table":"docs","limit":1}"#);
    assert_eq!(ok.status, 200, "got: {ok:?}");
    let denied = q(
        addr,
        &t,
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#,
    );
    assert_scope_denied(&denied);
    let text = String::from_utf8_lossy(&denied.body).into_owned();
    assert!(
        !text.contains("aggregate") && !text.contains("docs"),
        "{text}"
    );
}

#[test]
fn table_axis_hides_existence_of_tables() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let scoped = login(addr, "alice", r#""scope":{"tables":["docs"]}"#);
    let ghost = q(addr, &scoped, r#"{"op":"scan","table":"ghost","limit":1}"#);
    let other = q(addr, &scoped, r#"{"op":"scan","table":"other","limit":1}"#);
    assert_scope_denied(&ghost);
    assert_scope_denied(&other);
    // 存在する表・しない表で応答本文が同一（バイト単位）。
    assert_eq!(ghost.body, other.body);
    let text = String::from_utf8_lossy(&ghost.body).into_owned();
    assert!(!text.contains("ghost") && !text.contains("other"), "{text}");

    // スコープ内の存在しない表は、スコープなしと同じ 42P01。
    let in_scope_ghost = login(addr, "alice", r#""scope":{"tables":["ghost"]}"#);
    let unscoped = login(addr, "alice", "");
    let a = q(
        addr,
        &in_scope_ghost,
        r#"{"op":"scan","table":"ghost","limit":1}"#,
    );
    let b = q(
        addr,
        &unscoped,
        r#"{"op":"scan","table":"ghost","limit":1}"#,
    );
    assert_eq!(a.status, 404, "got: {a:?}");
    assert_eq!(http_common::wire_code_of(&a), "42P01");
    assert_eq!(a.status, b.status);
    assert_eq!(a.body, b.body);
}

#[test]
fn read_only_is_an_allow_list() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let t = login(addr, "alice", r#""scope":{"read_only":true}"#);
    let denied = [
        r#"{"op":"insert","table":"docs","rows":[{"id":1,"lang":"ja"}],"operation_id":"ro-1"}"#,
        r#"{"op":"upsert","table":"docs","rows":[{"id":1,"lang":"ja"}],"on_conflict":{"columns":["id"],"do":"nothing"},"operation_id":"ro-2"}"#,
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":"ro-3"}"#,
        r#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":"ro-4"}"#,
        r#"{"op":"create_table","table":"t1","columns":[{"name":"a","type":"text"}]}"#,
        r#"{"op":"alter_table","table":"docs","add_column":{"name":"c","type":"text"}}"#,
        r#"{"op":"drop_table","table":"docs"}"#,
        r#"{"op":"cancel_partitioned_dml","table":"docs","operation_id":"ro-5"}"#,
    ];
    for body in denied {
        assert_scope_denied(&q(addr, &t, body));
    }
    for body in [
        r#"{"op":"scan","table":"docs","limit":1}"#,
        r#"{"op":"scan","table":"docs","limit":1,"explain":true}"#,
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#,
    ] {
        let r = q(addr, &t, body);
        assert_eq!(r.status, 200, "{body}: {r:?}");
    }
    // show_partitioned_dml はスコープを通過する（未知の operation_id は空の進捗表）。
    let show = q(
        addr,
        &t,
        r#"{"op":"show_partitioned_dml","table":"docs","operation_id":"none"}"#,
    );
    assert_eq!(show.status, 200, "got: {show:?}");
}

#[test]
fn rejected_requests_have_no_side_effects() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let ro = login(addr, "alice", r#""scope":{"read_only":true}"#);
    let full = login(addr, "alice", "");
    let ins =
        r#"{"op":"insert","table":"docs","rows":[{"id":1,"lang":"ja"}],"operation_id":"side-1"}"#;
    assert_scope_denied(&q(addr, &ro, ins));

    // Idempotency-Key 付きの拒否も台帳を消費しない。
    let body = r#"{"op":"insert","table":"docs","rows":[{"id":2,"lang":"ja"}]}"#;
    let len = body.len().to_string();
    let auth = format!("Bearer {ro}");
    let req = http_common::build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &len),
            ("Authorization", &auth),
            ("Idempotency-Key", "side-key-2"),
        ],
        body.as_bytes(),
    );
    let r = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &req,
        AfterWrite::HalfClose,
    ));
    assert_scope_denied(&r);
    assert_eq!(row_count(addr, &full, "docs"), 0);

    // 同じ operation_id が未使用であること（台帳に書かれていない）= 成功し、行が入る。
    let ok = q(addr, &full, ins);
    assert_eq!(ok.status, 200, "got: {ok:?}");
    assert_eq!(row_count(addr, &full, "docs"), 1);
    let ok2 = q(
        addr,
        &full,
        r#"{"op":"insert","table":"docs","rows":[{"id":2,"lang":"ja"}],"operation_id":"side-key-2"}"#,
    );
    assert_eq!(ok2.status, 200, "got: {ok2:?}");
    assert_eq!(row_count(addr, &full, "docs"), 2);
}

#[test]
fn scope_check_precedes_the_allocated_identity_probe() {
    let (core, _g) = new_core();
    // 採番方式の表（TABLE-23）を SQL 表層で作る。
    let mut session = SessionState::default();
    session.allow_ddl();
    let ctx = PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("ctx");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE pk_t (k BIGINT PRIMARY KEY, v TEXT)",
    )
    .expect("create allocated table");
    let addr = spawn(core);
    let scoped = login(addr, "alice", r#""scope":{"tables":["docs"]}"#);
    let unscoped = login(addr, "alice", "");
    // スコープなしでは従来どおり 0A000（採番方式は aggregate 未対応）。
    let base = q(
        addr,
        &unscoped,
        r#"{"op":"aggregate","table":"pk_t","aggregates":[{"fn":"count","column":"*"}]}"#,
    );
    assert_eq!(http_common::wire_code_of(&base), "0A000", "got: {base:?}");
    // スコープ外では存在・方式に依らず 42501（0A000 に分かれない）。
    for body in [
        r#"{"op":"aggregate","table":"pk_t","aggregates":[{"fn":"count","column":"*"}]}"#,
        r#"{"op":"search","table":"pk_t","vector":[0.1],"limit":1}"#,
    ] {
        assert_scope_denied(&q(addr, &scoped, body));
    }
}

#[test]
fn scope_only_narrows_and_never_widens_permissions() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    // bob は DDL 権限なし。スコープで create_table を許しても engine 側の 42501 が返る。
    let t = login(addr, "bob", r#""scope":{"ops":["create_table"]}"#);
    let r = q(
        addr,
        &t,
        r#"{"op":"create_table","table":"t2","columns":[{"name":"a","type":"text"}]}"#,
    );
    assert_eq!(r.status, 403, "got: {r:?}");
    assert_eq!(http_common::wire_code_of(&r), "42501");
    // 権限のある alice なら、同じスコープで通る。
    let ta = login(addr, "alice", r#""scope":{"ops":["create_table"]}"#);
    let ok = q(
        addr,
        &ta,
        r#"{"op":"create_table","table":"t2","columns":[{"name":"a","type":"text"}]}"#,
    );
    assert_eq!(ok.status, 200, "got: {ok:?}");
}

#[test]
fn create_table_foreign_key_target_is_checked_against_tables() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let t = login(addr, "alice", r#""scope":{"tables":["child"]}"#);
    let fk = r#"{"op":"create_table","table":"child","columns":[{"name":"p","type":"integer"}],
        "constraints":[{"kind":"foreign_key","columns":["p"],"references":{"table":"outside","columns":["id"]}}]}"#;
    assert_scope_denied(&q(addr, &t, fk));
    // DDL は作られていない（スコープなしで child を作れる＝既存でない）。
    let full = login(addr, "alice", "");
    let ok = q(
        addr,
        &full,
        r#"{"op":"create_table","table":"child","columns":[{"name":"p","type":"integer"}]}"#,
    );
    assert_eq!(ok.status, 200, "got: {ok:?}");
}
