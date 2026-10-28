//! NoSQL（HTTP）表層の行アドレスが PRIMARY KEY 基準で動くことの結合テスト
//! （Issue #2201・ポインタ: `docs/spec` TABLE-23・NOSQL-23・NOSQL-12・NOSQL-6・RLS-9）。
//!
//! 表は engine の SQL 表層（DDL 権限付きセッション）の `CREATE TABLE` で作る（暗黙の `id` を
//! 持たない採番方式の表）。`update`／`delete` の `where` は PK 列（複合可）で指定でき、SQL 表層の
//! 同等の文と同じ行・同じ台帳キー空間に作用すること、PK なし表への単一行形が fail-closed で
//! あること、`insert`／`scan` が `id` なしで使えること、従来方式の表の `where: {"id": n}` が
//! 互換で動くことを固定する。

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;
use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("table23-nosql-pk");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

/// 採番方式の表を SQL 表層で作る（`tenant` の DDL 権限付きセッション）。
fn sql(core: &EngineCore, tenant: &str, stmt: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, stmt)
        .unwrap_or_else(|e| panic!("sql failed: {stmt}: {e:?}"));
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

impl Server {
    fn a(&self, body: &str) -> HttpResponse {
        post(self.addr, &self.token_a, body)
    }
    fn b(&self, body: &str) -> HttpResponse {
        post(self.addr, &self.token_b, body)
    }
}

fn text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn ok(resp: &HttpResponse) -> String {
    assert_eq!(resp.status, 200, "{resp:?} {}", text(resp));
    text(resp)
}

fn assert_code(resp: &HttpResponse, status: u16, code: &str) {
    assert_eq!(resp.status, status, "{resp:?} {}", text(resp));
    assert_eq!(http_common::wire_code_of(resp), code, "{}", text(resp));
}

const COMPOSITE: &str = "CREATE TABLE t (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (a, b))";

fn seed_composite(core: &EngineCore, srv: &Server) {
    sql(core, "tenant-a", COMPOSITE);
    ok(&srv.a(
        r#"{"op":"insert","table":"t","rows":[{"a":"k","b":1,"v":"one"},{"a":"k","b":2,"v":"two"},{"a":"m","b":1,"v":"three"}],"operation_id":"seed"}"#,
    ));
}

fn scan_all(srv: &Server) -> String {
    ok(&srv.a(r#"{"op":"scan","table":"t","limit":100,"sort":[{"column":"a","dir":"asc"},{"column":"b","dir":"asc"}]}"#))
}

// --- 1. パリティ -------------------------------------------------------------

#[test]
fn composite_pk_update_and_sql_update_touch_the_same_row() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);

    let body = ok(&srv
        .a(r#"{"op":"update","table":"t","set":{"v":"nosql"},"where":{"b":2,"a":"k"},"operation_id":"u1"}"#));
    assert!(body.contains(r#""updated":1"#), "{body}");
    let after_nosql = scan_all(&srv);
    assert!(after_nosql.contains("nosql"), "{after_nosql}");
    assert!(
        after_nosql.contains("one") && after_nosql.contains("three"),
        "{after_nosql}"
    );
    assert!(!after_nosql.contains("two"), "{after_nosql}");

    sql(
        &core,
        "tenant-a",
        "UPDATE t SET v = 'viasql' WHERE a = 'k' AND b = 1 USING OPERATION_ID 'u2'",
    );
    let after_sql = scan_all(&srv);
    assert!(after_sql.contains("viasql"), "{after_sql}");
    assert!(!after_sql.contains(r#""one""#), "{after_sql}");
    assert!(after_sql.contains("nosql") && after_sql.contains("three"));
}

#[test]
fn composite_pk_delete_removes_only_the_addressed_row() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);

    let body =
        ok(&srv.a(r#"{"op":"delete","table":"t","where":{"a":"k","b":1},"operation_id":"d1"}"#));
    assert!(body.contains(r#""deleted":1"#), "{body}");
    let rows = scan_all(&srv);
    assert!(!rows.contains(r#""one""#), "{rows}");
    assert!(rows.contains("two") && rows.contains("three"), "{rows}");
}

#[test]
fn sql_then_nosql_same_operation_id_same_content_converges_to_23505() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);

    sql(
        &core,
        "tenant-a",
        "UPDATE t SET v = 'x' WHERE a = 'k' AND b = 1 USING OPERATION_ID 'par-1'",
    );
    let resp = srv.a(
        r#"{"op":"update","table":"t","set":{"v":"x"},"where":{"a":"k","b":1},"operation_id":"par-1"}"#,
    );
    assert_code(&resp, 409, "23505");

    // 内容が違えば `22023`。
    let resp = srv.a(
        r#"{"op":"update","table":"t","set":{"v":"y"},"where":{"a":"k","b":1},"operation_id":"par-1"}"#,
    );
    assert_code(&resp, 400, "22023");
}

// --- 2/3. PK なし表・キー集合の不一致 ------------------------------------------

#[test]
fn table_without_primary_key_rejects_single_row_form_but_allows_filter() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    sql(&core, "tenant-a", "CREATE TABLE nopk (a TEXT, v TEXT)");
    ok(&srv.a(
        r#"{"op":"insert","table":"nopk","rows":[{"a":"k","v":"one"},{"a":"k","v":"two"}],"operation_id":"s"}"#,
    ));
    for body in [
        r#"{"op":"update","table":"nopk","set":{"v":"z"},"where":{"a":"k"},"operation_id":"n1"}"#,
        r#"{"op":"delete","table":"nopk","where":{"a":"k"},"operation_id":"n2"}"#,
        r#"{"op":"delete","table":"nopk","where":{"id":1},"operation_id":"n3"}"#,
    ] {
        let resp = srv.a(body);
        assert_code(&resp, 400, "42601");
        let t = text(&resp);
        assert!(!t.contains("nopk") && !t.contains("tenant-a"), "{t}");
    }
    let rows = ok(&srv.a(r#"{"op":"scan","table":"nopk","limit":10}"#));
    assert!(rows.contains("one") && rows.contains("two"), "{rows}");
    assert!(!rows.contains(r#""z""#), "{rows}");

    let body = ok(&srv.a(
        r#"{"op":"update","table":"nopk","set":{"v":"z"},"filter":[{"column":"a","op":"eq","value":"k"}],"operation_id":"n4"}"#,
    ));
    assert!(body.contains(r#""updated":2"#), "{body}");
}

#[test]
fn mismatched_address_keys_are_rejected_without_side_effects() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);
    let before = scan_all(&srv);
    for where_json in [
        r#"{"a":"k"}"#,
        r#"{"a":"k","b":1,"v":"one"}"#,
        r#"{"a":"k","zz":1}"#,
        r#"{"a":"k","b":null}"#,
        r#"{}"#,
        r#"{"a b":1}"#,
    ] {
        for op in ["update", "delete"] {
            let body = if op == "update" {
                format!(
                    r#"{{"op":"update","table":"t","set":{{"v":"x"}},"where":{where_json},"operation_id":"mm-{op}"}}"#
                )
            } else {
                format!(
                    r#"{{"op":"delete","table":"t","where":{where_json},"operation_id":"mm-{op}"}}"#
                )
            };
            assert_code(&srv.a(&body), 400, "42601");
        }
    }
    assert_eq!(scan_all(&srv), before);

    // 分割実行修飾と `where` の併用は拒否。
    let resp = srv.a(
        r#"{"op":"delete","table":"t","where":{"a":"k","b":1},"mode":"partitioned","operation_id":"pp"}"#,
    );
    assert_code(&resp, 400, "42601");
    assert_eq!(scan_all(&srv), before);
}

// --- 4. 型 ---------------------------------------------------------------------

#[test]
fn declared_id_column_is_an_ordinary_column_on_allocated_tables() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    sql(
        &core,
        "tenant-a",
        "CREATE TABLE named (id TEXT PRIMARY KEY, v TEXT)",
    );
    ok(&srv.a(
        r#"{"op":"insert","table":"named","rows":[{"id":"x","v":"one"},{"id":"y","v":"two"}],"operation_id":"s"}"#,
    ));
    let body = ok(&srv.a(
        r#"{"op":"update","table":"named","set":{"v":"upd"},"where":{"id":"x"},"operation_id":"u"}"#,
    ));
    assert!(body.contains(r#""updated":1"#), "{body}");
    let rows = ok(&srv.a(r#"{"op":"scan","table":"named","limit":10}"#));
    assert!(rows.contains("upd") && rows.contains("two"), "{rows}");
    assert!(!rows.contains(r#""one""#), "{rows}");
}

#[test]
fn pk_value_type_mismatch_is_rejected_like_filter_eq() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);
    let via_where = srv.a(
        r#"{"op":"delete","table":"t","where":{"a":"k","b":"not-a-number"},"operation_id":"ty1"}"#,
    );
    let via_filter = srv.a(
        r#"{"op":"delete","table":"t","filter":[{"column":"a","op":"eq","value":"k"},{"column":"b","op":"eq","value":"not-a-number"}],"operation_id":"ty2"}"#,
    );
    assert_ne!(via_where.status, 200, "{}", text(&via_where));
    assert_eq!(via_where.status, via_filter.status);
    assert_eq!(
        http_common::wire_code_of(&via_where),
        http_common::wire_code_of(&via_filter)
    );
}

// --- 5. テナント境界 ------------------------------------------------------------

#[test]
fn address_does_not_cross_tenants() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);
    ok(&srv.b(
        r#"{"op":"insert","table":"t","rows":[{"a":"only-b","b":9,"v":"bee"},{"a":"k","b":1,"v":"b-copy"}],"operation_id":"seed-b"}"#,
    ));

    // A の更新は A の行だけ。B の同じ PK の行は不変。
    ok(&srv.a(
        r#"{"op":"update","table":"t","set":{"v":"a-new"},"where":{"a":"k","b":1},"operation_id":"ua"}"#,
    ));
    let b_rows = ok(&srv.b(r#"{"op":"scan","table":"t","limit":10}"#));
    assert!(
        b_rows.contains("b-copy") && !b_rows.contains("a-new"),
        "{b_rows}"
    );

    // B だけが持つ PK 値への A の delete は 0 件・200（存在有無が応答から区別できない）。
    let foreign =
        srv.a(r#"{"op":"delete","table":"t","where":{"a":"only-b","b":9},"operation_id":"da1"}"#);
    let missing =
        srv.a(r#"{"op":"delete","table":"t","where":{"a":"nobody","b":9},"operation_id":"da2"}"#);
    assert!(ok(&foreign).contains(r#""deleted":0"#));
    assert_eq!(
        ok(&foreign).replace("da1", "x"),
        ok(&missing).replace("da2", "x")
    );
    let b_rows = ok(&srv.b(r#"{"op":"scan","table":"t","limit":10}"#));
    assert!(b_rows.contains("bee"), "{b_rows}");
}

// --- 6. insert -----------------------------------------------------------------

#[test]
fn insert_needs_no_id_and_never_exposes_a_row_id() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    sql(&core, "tenant-a", "CREATE TABLE nopk (a TEXT, v TEXT)");
    let body = ok(&srv
        .a(r#"{"op":"insert","table":"nopk","rows":[{"a":"p"},{"a":"q"}],"operation_id":"i1"}"#));
    assert_eq!(body, r#"{"inserted":2,"operation_id":"i1"}"#);
    let rows = ok(&srv.a(r#"{"op":"scan","table":"nopk","limit":10}"#));
    assert!(!rows.contains(r#""id""#), "{rows}");

    // 宣言していない `id` キーは未知列（`22000`）。
    let resp =
        srv.a(r#"{"op":"insert","table":"nopk","rows":[{"id":1,"a":"r"}],"operation_id":"i2"}"#);
    assert_code(&resp, 400, "22000");
}

#[test]
fn insert_pk_violations_follow_the_engine_contract() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);
    // PK 列の省略（DEFAULT なし）は NOT NULL 違反。
    assert_code(
        &srv.a(r#"{"op":"insert","table":"t","rows":[{"a":"z","v":"x"}],"operation_id":"i1"}"#),
        400,
        "23502",
    );
    // PK 重複。
    assert_code(
        &srv.a(
            r#"{"op":"insert","table":"t","rows":[{"a":"k","b":1,"v":"dup"}],"operation_id":"i2"}"#,
        ),
        409,
        "23505",
    );
}

// --- 7. scan -------------------------------------------------------------------

#[test]
fn scan_returns_declared_columns_only_and_supports_explain() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);
    let rows = ok(&srv.a(
        r#"{"op":"scan","table":"t","limit":2,"offset":1,"sort":[{"column":"a","dir":"asc"},{"column":"b","dir":"asc"}],"filter":[{"column":"a","op":"eq","value":"k"}]}"#,
    ));
    assert!(rows.contains("two") && !rows.contains("three"), "{rows}");
    assert!(!rows.contains(r#""id""#), "{rows}");

    // 宣言していない `id` の投影・並べ替えは未定義列（`42703`）。
    assert_code(
        &srv.a(r#"{"op":"scan","table":"t","limit":2,"columns":["id"]}"#),
        400,
        "42703",
    );
    assert_code(
        &srv.a(r#"{"op":"scan","table":"t","limit":2,"sort":[{"column":"id","dir":"asc"}]}"#),
        400,
        "42703",
    );
    ok(&srv.a(r#"{"op":"scan","table":"t","limit":2,"explain":true}"#));
}

// --- 8. 互換（従来方式の表） --------------------------------------------------------

#[test]
fn legacy_table_keeps_where_id_form() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx("tenant-a"), &mut session, "CREATE TABLE lg (v TEXT)")
        .expect("create legacy table");
    ok(&srv.a(
        r#"{"op":"insert","table":"lg","rows":[{"id":1,"v":"one"},{"id":2,"v":"two"}],"operation_id":"s"}"#,
    ));
    let body = ok(&srv.a(
        r#"{"op":"update","table":"lg","set":{"v":"upd"},"where":{"id":1},"operation_id":"u"}"#,
    ));
    assert!(body.contains(r#""updated":1"#), "{body}");
    assert_code(
        &srv.a(r#"{"op":"delete","table":"lg","where":{"id":1.5},"operation_id":"d1"}"#),
        400,
        "22000",
    );
    assert_code(
        &srv.a(r#"{"op":"delete","table":"lg","where":{"id":-1},"operation_id":"d2"}"#),
        400,
        "42601",
    );
    // 疑似列 `id` 以外の列を単一行形で指定する表（PK なし）は fail-closed。
    assert_code(
        &srv.a(r#"{"op":"delete","table":"lg","where":{"v":"two"},"operation_id":"d3"}"#),
        400,
        "42601",
    );
    let body = ok(&srv.a(r#"{"op":"delete","table":"lg","where":{"id":2},"operation_id":"d4"}"#));
    assert!(body.contains(r#""deleted":1"#), "{body}");
}

// --- 9. ゲート -------------------------------------------------------------------

#[test]
fn aggregate_and_search_stay_closed_on_allocated_tables() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);
    for body in [
        r#"{"op":"aggregate","table":"t","aggregates":[{"fn":"count","column":"a"}]}"#,
        r#"{"op":"search","table":"t","limit":1,"vector":[1,0,0]}"#,
    ] {
        let resp = srv.a(body);
        assert_code(&resp, 501, "0A000");
        let t = text(&resp);
        assert!(t.contains("row identity mode is not supported yet"), "{t}");
    }
}
