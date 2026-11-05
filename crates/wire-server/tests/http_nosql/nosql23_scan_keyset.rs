//! `POST /v1/query`（`op: "scan"`）の不透明カーソル（`after`／`next_cursor`）を production
//! ルータ経由で検証する結合テスト（Issue #2094・ポインタ: `docs/spec` NOSQL-23・TASK-253）。
//!
//! 往復（全ページ連結 = 一括取得）、最終ページの `null`、`after` の型・併用・復号失敗の分類
//! （型と併用は `42601`、改ざん・別主体・別テナント・別 `sort`・別表・別サーバー鍵は `22000`）、
//! 非キーセット要求の応答形の不変、`explain` との併用、カーソルの不透明性を固定する。
//! RLS・並行変更・SQL とのパリティの網羅は Issue #2095 の担当で、ここは代表ケースに留める。

use crate::common;
use crate::http_common;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};
use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::base64_std::{decode_base64_std, encode_base64_std};
use wire_server::http::session::store::SessionStore;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql23-scan-keyset");
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

fn sql(core: &EngineCore, tenant: &str, stmt: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, stmt)
        .unwrap_or_else(|e| panic!("sql failed: {stmt}: {e:?}"));
}

fn login_token(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let req = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &req,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected token, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

/// 同一テナントの 2 ユーザー（alice・carol）と別テナントの 1 ユーザー（bob）。
struct Server {
    addr: SocketAddr,
    alice: String,
    carol: String,
    bob: String,
}

fn spawn(core: Arc<EngineCore>) -> Server {
    let users = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("carol", "tenant-a", "pw-carol"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = http_common::spawn_router_listener_with_engine(&users, SessionStore::new(), core);
    Server {
        addr,
        alice: login_token(addr, "alice", "pw-alice"),
        carol: login_token(addr, "carol", "pw-carol"),
        bob: login_token(addr, "bob", "pw-bob"),
    }
}

fn post(addr: SocketAddr, token: &str, body: &str) -> HttpResponse {
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

fn ok(resp: &HttpResponse) -> String {
    assert_eq!(resp.status, 200, "{resp:?} {}", text(resp));
    text(resp)
}

fn assert_code(resp: &HttpResponse, status: u16, code: &str) {
    assert_eq!(resp.status, status, "{resp:?} {}", text(resp));
    assert_eq!(http_common::wire_code_of(resp), code, "{}", text(resp));
}

/// 1 ページ分（行を文字列化したものと `next_cursor`）。
struct Page {
    rows: Vec<String>,
    next: Option<String>,
}

fn parse_page(body: &str) -> Page {
    let JsonValue::Object(mut top) = parse_json(body).expect("body json") else {
        panic!("object expected: {body}");
    };
    let rows = match top.remove("rows") {
        Some(JsonValue::Array(items)) => items.iter().map(|r| format!("{r:?}")).collect(),
        other => panic!("rows must be an array: {other:?}"),
    };
    let next = match top.remove("next_cursor") {
        Some(JsonValue::String(s)) => Some(s),
        Some(JsonValue::Null) => None,
        other => panic!("next_cursor must be a string or null: {other:?}"),
    };
    Page { rows, next }
}

/// `scan` 要求本文を作る（`after` は `None` なら `null`、`Some` なら文字列）。
fn scan_body(table: &str, limit: usize, sort: &str, after: Option<&str>) -> String {
    let after_json = match after {
        Some(c) => format!("\"{c}\""),
        None => "null".to_string(),
    };
    let sort_part = if sort.is_empty() {
        String::new()
    } else {
        format!(r#","sort":{sort}"#)
    };
    format!(r#"{{"op":"scan","table":"{table}","limit":{limit}{sort_part},"after":{after_json}}}"#)
}

/// `after: null` から `next_cursor` を辿って全ページを連結する。
fn collect_all(
    addr: SocketAddr,
    token: &str,
    table: &str,
    limit: usize,
    sort: &str,
) -> Vec<String> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100 {
        let body = scan_body(table, limit, sort, cursor.as_deref());
        let page = parse_page(&ok(&post(addr, token, &body)));
        assert!(page.rows.len() <= limit);
        all.extend(page.rows);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return all,
        }
    }
    panic!("cursor did not terminate");
}

const PK_TABLE: &str = "CREATE TABLE pkt (k INTEGER PRIMARY KEY, g TEXT, v TEXT)";
const PK_TABLE2: &str = "CREATE TABLE pkt2 (k INTEGER PRIMARY KEY, g TEXT, v TEXT)";
const NOPK_TABLE: &str = "CREATE TABLE nopk (g TEXT, v TEXT)";

/// 7 行（`g` に重複と NULL を含む）。
fn seed(srv: &Server, table: &str, with_k: bool) {
    let gs = [
        Some("b"),
        Some("a"),
        None,
        Some("a"),
        Some("c"),
        Some("b"),
        Some("a"),
    ];
    let mut rows = Vec::new();
    for (i, g) in gs.iter().enumerate() {
        let g_json = g.map_or("null".to_string(), |s| format!("\"{s}\""));
        let k_part = if with_k {
            format!("\"k\":{},", i + 1)
        } else {
            String::new()
        };
        rows.push(format!(r#"{{{k_part}"g":{g_json},"v":"v{i}"}}"#));
    }
    let body = format!(
        r#"{{"op":"insert","table":"{table}","rows":[{}],"operation_id":"seed-{table}"}}"#,
        rows.join(",")
    );
    ok(&post(srv.addr, &srv.alice, &body));
}

fn setup() -> (Arc<EngineCore>, Server, temp_db::CleanupGuard) {
    let (core, guard) = new_core();
    sql(&core, "tenant-a", PK_TABLE);
    sql(&core, "tenant-a", PK_TABLE2);
    sql(&core, "tenant-a", NOPK_TABLE);
    let srv = spawn(core.clone());
    seed(&srv, "pkt", true);
    seed(&srv, "pkt2", true);
    seed(&srv, "nopk", false);
    (core, srv, guard)
}

const SORT_G_ASC: &str = r#"[{"column":"g","dir":"asc"}]"#;
const SORT_G_DESC: &str = r#"[{"column":"g","dir":"desc"}]"#;

#[test]
fn round_trip_equals_single_page_for_every_table_shape_and_limit() {
    let (_core, srv, _g) = setup();
    for table in ["pkt", "nopk"] {
        for sort in ["", SORT_G_ASC, SORT_G_DESC] {
            // 一括（全行が 1 ページに収まる）を基準にする。
            let whole = parse_page(&ok(&post(
                srv.addr,
                &srv.alice,
                &scan_body(table, 50, sort, None),
            )));
            assert_eq!(whole.rows.len(), 7, "{table} {sort}");
            assert!(whole.next.is_none(), "single full page must be last");
            for limit in [1, 2, 3, 7] {
                let paged = collect_all(srv.addr, &srv.alice, table, limit, sort);
                assert_eq!(paged, whole.rows, "{table} sort={sort} limit={limit}");
            }
        }
    }
}

#[test]
fn pk_table_without_sort_is_ascending_by_primary_key() {
    let (_core, srv, _g) = setup();
    let rows = collect_all(srv.addr, &srv.alice, "pkt", 2, "");
    assert_eq!(rows.len(), 7);
    let ks: Vec<usize> = (1..=7).collect();
    for (row, k) in rows.iter().zip(ks) {
        assert!(row.contains(&format!("{k}")), "row {row} should hold k={k}");
    }
}

#[test]
fn last_page_has_null_cursor_without_an_extra_empty_page() {
    let (_core, srv, _g) = setup();
    // 7 行・limit 7: 満杯の最終ページで null（空ページを返さない）。
    let page = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 7, "", None),
    )));
    assert_eq!(page.rows.len(), 7);
    assert!(page.next.is_none());
    // 7 行・limit 4: 4 + 3。最後の端数ページも null。
    let p1 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 4, "", None),
    )));
    assert_eq!(p1.rows.len(), 4);
    let cursor = p1.next.expect("more rows follow");
    let p2 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 4, "", Some(&cursor)),
    )));
    assert_eq!(p2.rows.len(), 3);
    assert!(p2.next.is_none());
}

#[test]
fn after_with_wrong_type_is_42601() {
    let (_core, srv, _g) = setup();
    for bad in ["1", "true", "[]", "{}"] {
        let body = format!(r#"{{"op":"scan","table":"pkt","limit":2,"after":{bad}}}"#);
        assert_code(&post(srv.addr, &srv.alice, &body), 400, "42601");
    }
}

#[test]
fn after_combined_with_offset_is_42601() {
    let (_core, srv, _g) = setup();
    let p1 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 2, "", None),
    )));
    let cursor = p1.next.expect("cursor");
    for after in ["null".to_string(), format!("\"{cursor}\"")] {
        for offset in [0, 5] {
            let body = format!(
                r#"{{"op":"scan","table":"pkt","limit":2,"offset":{offset},"after":{after}}}"#
            );
            assert_code(&post(srv.addr, &srv.alice, &body), 400, "42601");
        }
    }
}

fn flip_one_byte(cursor: &str) -> String {
    let mut raw = decode_base64_std(cursor, 1 << 20).expect("valid base64");
    let mid = raw.len() / 2;
    raw[mid] ^= 0x01;
    encode_base64_std(&raw)
}

#[test]
fn tampered_or_malformed_cursors_are_rejected_with_one_identical_22000() {
    let (_core, srv, _g) = setup();
    let p1 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 2, SORT_G_ASC, None),
    )));
    let good = p1.next.expect("cursor");
    let truncated = {
        let raw = decode_base64_std(&good, 1 << 20).expect("valid base64");
        encode_base64_std(&raw[..raw.len() - 1])
    };
    let candidates = [
        flip_one_byte(&good),
        truncated,
        "!!not-base64!!".to_string(),
        String::new(),
        "A".repeat(200_000),
    ];
    let mut messages = Vec::new();
    for bad in &candidates {
        let resp = post(
            srv.addr,
            &srv.alice,
            &scan_body("pkt", 2, SORT_G_ASC, Some(bad)),
        );
        assert_code(&resp, 400, "22000");
        messages.push(http_common::error_message_of(&resp));
    }
    assert!(messages.windows(2).all(|w| w[0] == w[1]), "{messages:?}");
    assert_eq!(messages[0], "invalid keyset position");
}

#[test]
fn cursor_is_bound_to_principal_tenant_sort_and_table() {
    let (_core, srv, _g) = setup();
    let p1 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 2, SORT_G_ASC, None),
    )));
    let cursor = p1.next.expect("cursor");
    // 正規の提示は通る（対照）。
    ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 2, SORT_G_ASC, Some(&cursor)),
    ));
    // 同一テナントの別ユーザー。
    assert_code(
        &post(
            srv.addr,
            &srv.carol,
            &scan_body("pkt", 2, SORT_G_ASC, Some(&cursor)),
        ),
        400,
        "22000",
    );
    // 別テナント。
    assert_code(
        &post(
            srv.addr,
            &srv.bob,
            &scan_body("pkt", 2, SORT_G_ASC, Some(&cursor)),
        ),
        400,
        "22000",
    );
    // 別 sort（方向違い・列違い・有無違い）。
    for sort in [SORT_G_DESC, r#"[{"column":"v","dir":"asc"}]"#, ""] {
        assert_code(
            &post(
                srv.addr,
                &srv.alice,
                &scan_body("pkt", 2, sort, Some(&cursor)),
            ),
            400,
            "22000",
        );
    }
    // 同じ形の別表。
    assert_code(
        &post(
            srv.addr,
            &srv.alice,
            &scan_body("pkt2", 2, SORT_G_ASC, Some(&cursor)),
        ),
        400,
        "22000",
    );
}

#[test]
fn cursor_from_another_server_key_is_rejected() {
    let (core, srv, _g) = setup();
    let p1 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 2, "", None),
    )));
    let cursor = p1.next.expect("cursor");
    // 同じ EngineCore に 2 つ目のルータ（別の封緘鍵）を立てる = 再起動の代替。
    let srv2 = spawn(core);
    assert_code(
        &post(
            srv2.addr,
            &srv2.alice,
            &scan_body("pkt", 2, "", Some(&cursor)),
        ),
        400,
        "22000",
    );
}

#[test]
fn non_keyset_requests_keep_the_original_response_shape() {
    let (_core, srv, _g) = setup();
    for body in [
        r#"{"op":"scan","table":"pkt","limit":3}"#,
        r#"{"op":"scan","table":"pkt","limit":3,"sort":[{"column":"g","dir":"asc"}]}"#,
        r#"{"op":"scan","table":"pkt","limit":3,"offset":1}"#,
    ] {
        let out = ok(&post(srv.addr, &srv.alice, body));
        assert!(!out.contains("next_cursor"), "{out}");
    }
    let keyset = ok(&post(srv.addr, &srv.alice, &scan_body("pkt", 3, "", None)));
    assert!(
        keyset.find("\"row_count\"") < keyset.find("\"next_cursor\""),
        "{keyset}"
    );
}

#[test]
fn explain_with_after_validates_the_cursor_and_omits_next_cursor() {
    let (_core, srv, _g) = setup();
    let p1 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 2, "", None),
    )));
    let cursor = p1.next.expect("cursor");
    let good =
        format!(r#"{{"op":"scan","table":"pkt","limit":2,"explain":true,"after":"{cursor}"}}"#);
    let out = ok(&post(srv.addr, &srv.alice, &good));
    assert!(!out.contains("next_cursor"), "{out}");
    let bad = r#"{"op":"scan","table":"pkt","limit":2,"explain":true,"after":"AAAA"}"#;
    assert_code(&post(srv.addr, &srv.alice, bad), 400, "22000");
}

#[test]
fn cursor_does_not_expose_plaintext_values() {
    let (_core, srv, _g) = setup();
    let p1 = parse_page(&ok(&post(
        srv.addr,
        &srv.alice,
        &scan_body("pkt", 3, SORT_G_ASC, None),
    )));
    let cursor = p1.next.expect("cursor");
    let raw = decode_base64_std(&cursor, 1 << 20).expect("valid base64");
    let haystack = String::from_utf8_lossy(&raw);
    assert!(!haystack.contains("tenant-a"), "tenant leaked");
    assert!(!cursor.contains("tenant-a"));
}
