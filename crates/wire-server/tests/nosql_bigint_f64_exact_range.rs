//! NoSQL（HTTP）表層で、`f64` で正確に表せない `BIGINT` 値（`|v| > 2^53`）が
//! CHECK 評価で拒否されるとき、HTTP 400 かつ `code` が `22003` になることの
//! 結合テスト（Issue #1336・ポインタ: `docs/spec` TABLE-16・ERR-2・ERR-4）。
//!
//! NoSQL の DDL は CHECK を宣言できないため、HTTP リスナー起動前に engine の
//! SQL 表層（DDL 権限付きセッション）で CHECK 付きテーブルを作る。分類の是正
//! 本体は engine の `sql::udf_call` にあり、本ファイルは HTTP 射影（`status.rs` の
//! `NumericOutOfRange` → 400）までが一貫することを固定する。CHECK の無い
//! BIGINT 列は `i64` 全域を引き続き受理する（回帰防止）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

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

const EXACT: i64 = 1 << 53;
const OK_VALUES: [i64; 2] = [EXACT, -EXACT];
const BAD_VALUES: [i64; 4] = [EXACT + 1, -EXACT - 1, i64::MAX, i64::MIN];

/// `checked` が真なら `b BIGINT CHECK (b = b)` を持つテーブル `t` を作る。
fn new_core_with_table(checked: bool) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql-bigint-f64-exact");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    let ddl = if checked {
        "CREATE TABLE t (b BIGINT CHECK (b = b))"
    } else {
        "CREATE TABLE t (b BIGINT)"
    };
    core.execute_sql_in_session(&ctx, &mut session, ddl)
        .expect("create table");
    (Arc::new(core), guard)
}

fn login_token(http_addr: std::net::SocketAddr) -> String {
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let resp: HttpResponse = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &login_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match engine::json::parse_json(&text).expect("login body must be valid json") {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn post_query(http_addr: std::net::SocketAddr, token: &str, body: &str) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn spawn(core: Arc<EngineCore>) -> (std::net::SocketAddr, String) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let http_addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);
    let token = login_token(http_addr);
    (http_addr, token)
}

#[test]
fn nosql_insert_beyond_exact_range_into_checked_bigint_returns_400_22003() {
    let (core, _guard) = new_core_with_table(true);
    let (addr, token) = spawn(core);
    for (i, v) in OK_VALUES.iter().enumerate() {
        let body = format!(
            r#"{{"op":"insert","table":"t","rows":[{{"id":{},"b":{v}}}],"operation_id":"ok-{i}"}}"#,
            i + 1
        );
        let resp = post_query(addr, &token, &body);
        assert_eq!(resp.status, 200, "v={v}: {resp:?}");
    }
    for (i, v) in BAD_VALUES.iter().enumerate() {
        let body = format!(
            r#"{{"op":"insert","table":"t","rows":[{{"id":{},"b":{v}}}],"operation_id":"bad-{i}"}}"#,
            100 + i
        );
        let resp = post_query(addr, &token, &body);
        assert_eq!(resp.status, 400, "v={v}: {resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "22003", "v={v}");
    }
}

#[test]
fn nosql_update_beyond_exact_range_on_checked_bigint_returns_400_22003() {
    let (core, _guard) = new_core_with_table(true);
    let (addr, token) = spawn(core);
    let seed = post_query(
        addr,
        &token,
        r#"{"op":"insert","table":"t","rows":[{"id":1,"b":1}],"operation_id":"seed"}"#,
    );
    assert_eq!(seed.status, 200, "{seed:?}");
    for (i, v) in OK_VALUES.iter().enumerate() {
        let body = format!(
            r#"{{"op":"update","table":"t","set":{{"b":{v}}},"where":{{"id":1}},"operation_id":"u-ok-{i}"}}"#
        );
        let resp = post_query(addr, &token, &body);
        assert_eq!(resp.status, 200, "v={v}: {resp:?}");
    }
    for (i, v) in BAD_VALUES.iter().enumerate() {
        let body = format!(
            r#"{{"op":"update","table":"t","set":{{"b":{v}}},"where":{{"id":1}},"operation_id":"u-bad-{i}"}}"#
        );
        let resp = post_query(addr, &token, &body);
        assert_eq!(resp.status, 400, "v={v}: {resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "22003", "v={v}");
    }
}

#[test]
fn nosql_insert_full_i64_into_unchecked_bigint_still_succeeds() {
    let (core, _guard) = new_core_with_table(false);
    let (addr, token) = spawn(core);
    for (i, v) in OK_VALUES.iter().chain(BAD_VALUES.iter()).enumerate() {
        let body = format!(
            r#"{{"op":"insert","table":"t","rows":[{{"id":{},"b":{v}}}],"operation_id":"any-{i}"}}"#,
            i + 1
        );
        let resp = post_query(addr, &token, &body);
        assert_eq!(resp.status, 200, "v={v}: {resp:?}");
    }
}
