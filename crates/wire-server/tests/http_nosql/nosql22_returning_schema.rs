//! `insert`／`update`／`delete` の任意キー `returning` のスキーマ検証を、production ルータ経由
//! （生バイトクライアント）で固定する層 A 結合テスト（Issue #2090。親 #2089。
//! 対象ビヘイビア: `docs/spec/04-behavior/nosql-surface.md` NOSQL-22）。
//!
//! ## 固定する内容
//!
//! - `returning` 未指定の成功本文は従来とバイト単位で同一
//! - 不正な値は 3 op すべてで `42601`（HTTP 400）で、本文に入力値を含めない
//! - 妥当な指定は実行結線（Issue #2091）まで固定文言の `0A000`（HTTP 501）で、副作用が無い
//!   （行・値が変わらず、`operation_id` 台帳も消費しない）
//! - 分割実行（`mode`／`chunk`）との併用は `42601`
//! - `Idempotency-Key` ヘッダ付きでも判定順（型 → ヘッダ → 値 → 併用 → 未結線）は変わらない

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TENANT_A: &str = "tenant-a";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs (n BIGINT, u TEXT UNIQUE)` を作り、tenant-a へ 3 行 seed する。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql22-returning-schema");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    let sys = PolicyContext::new("sys").expect("sys ctx");
    core.execute_sql_in_session_legacy(
        &sys,
        &mut session,
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)",
    )
    .expect("create table");
    core.execute_sql_in_session_legacy(
        &ctx(),
        &mut SessionState::default(),
        "INSERT INTO docs (id, n, u) VALUES (1, 10, 'u1'), (2, 20, 'u2'), (3, 30, 'u3') \
         USING OPERATION_ID 'seed-a'",
    )
    .expect("seed rows");
    (Arc::new(core), guard)
}

struct Client {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn spawn(core: Arc<EngineCore>) -> Client {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let http_addr =
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
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let token = match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected json object, got {other:?}"),
    };
    Client { http_addr, token }
}

fn query(c: &Client, body: &str, header: Option<&str>) -> HttpResponse {
    let auth = format!("Bearer {}", c.token);
    let len = body.len().to_string();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", &auth),
        ("Content-Type", "application/json"),
        ("Content-Length", &len),
    ];
    if let Some(h) = header {
        headers.push(("Idempotency-Key", h));
    }
    let request = http_common::build_request("/v1/query", &headers, body.as_bytes());
    http_common::parse_single_response(&http_common::send_raw(
        c.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

/// seed 表の `(id, n)` 一覧（副作用の有無の検査に使う）。
fn snapshot(core: &EngineCore) -> Vec<String> {
    use engine::sql::SqlOutcome;
    let outcome = core
        .execute_sql_in_session_legacy(
            &ctx(),
            &mut SessionState::default(),
            "SELECT n, u FROM docs LIMIT 1000",
        )
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    let mut rows: Vec<String> = result
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect();
    rows.sort();
    rows
}

/// 3 op の本文を `returning` 断片つきで作る（`frag` は `,"returning":...` 形または空）。
fn bodies(frag: &str) -> [String; 3] {
    [
        format!(r#"{{"op":"insert","table":"docs","rows":[{{"id":9,"n":9,"u":"x9"}}]{frag}}}"#),
        format!(r#"{{"op":"update","table":"docs","set":{{"n":99}},"where":{{"id":1}}{frag}}}"#),
        format!(r#"{{"op":"delete","table":"docs","where":{{"id":1}}{frag}}}"#),
    ]
}

#[test]
fn without_returning_success_bodies_are_unchanged() {
    let (core, _g) = new_core();
    let c = spawn(core);
    let [ins, upd, del] = bodies("");
    let resp = query(&c, &ins, None);
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(body_text(&resp), r#"{"inserted":1}"#);
    let resp = query(&c, &upd, None);
    assert_eq!(body_text(&resp), r#"{"updated":1}"#);
    let resp = query(&c, &del, None);
    assert_eq!(body_text(&resp), r#"{"deleted":1}"#);
    let with_id =
        r#"{"op":"update","table":"docs","set":{"n":1},"where":{"id":2},"operation_id":"op-1"}"#;
    let resp = query(&c, with_id, None);
    assert_eq!(body_text(&resp), r#"{"updated":1,"operation_id":"op-1"}"#);
}

#[test]
fn invalid_returning_values_are_42601_without_echo() {
    let (core, _g) = new_core();
    let before = snapshot(&core);
    let c = spawn(core.clone());
    for value in [
        "null",
        "1",
        "{}",
        "true",
        "[]",
        "[1]",
        r#""""#,
        r#""secretcol""#,
        r#"["*"]"#,
        r#"["sec ret"]"#,
        r#"["secret",null]"#,
    ] {
        for body in bodies(&format!(r#","returning":{value}"#)) {
            for header in [None, Some("idem-1")] {
                let resp = query(&c, &body, header);
                assert_eq!(resp.status, 400, "{value} {resp:?}");
                assert_eq!(
                    http_common::wire_code_of(&resp),
                    "42601",
                    "{value} {resp:?}"
                );
                assert!(!body_text(&resp).contains("secret"), "{value} {resp:?}");
            }
        }
    }
    assert_eq!(snapshot(&core), before, "rejections must not write");
}

#[test]
fn valid_returning_is_fixed_0a000_without_side_effects() {
    let (core, _g) = new_core();
    let before = snapshot(&core);
    let c = spawn(core.clone());
    for value in [r#""*""#, r#"["n"]"#, r#"["n","u"]"#] {
        for body in bodies(&format!(r#","returning":{value}"#)) {
            for header in [None, Some("idem-2")] {
                let resp = query(&c, &body, header);
                assert_eq!(resp.status, 501, "{value} {resp:?}");
                assert_eq!(
                    http_common::wire_code_of(&resp),
                    "0A000",
                    "{value} {resp:?}"
                );
                assert!(
                    body_text(&resp).contains("returning is not yet available"),
                    "{resp:?}"
                );
            }
        }
    }
    assert_eq!(snapshot(&core), before, "0A000 must not write");
    // 台帳が消費されていない: 同じ Idempotency-Key を `returning` なしで送ると初回として成功する。
    let [_, upd, _] = bodies("");
    let resp = query(&c, &upd, Some("idem-2"));
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(body_text(&resp), r#"{"updated":1,"operation_id":"idem-2"}"#);
}

#[test]
fn returning_with_partitioned_modifiers_is_42601() {
    let (core, _g) = new_core();
    let c = spawn(core.clone());
    let filter = r#""filter":[{"column":"u","op":"eq","value":"u1"}]"#;
    for body in [
        format!(
            r#"{{"op":"update","table":"docs","set":{{"n":7}},{filter},"mode":"partitioned","returning":"*"}}"#
        ),
        format!(
            r#"{{"op":"delete","table":"docs",{filter},"mode":"partitioned","chunk":1,"returning":["n"]}}"#
        ),
        format!(r#"{{"op":"delete","table":"docs",{filter},"chunk":1,"returning":"*"}}"#),
    ] {
        let resp = query(&c, &body, None);
        assert_eq!(resp.status, 400, "{resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "{resp:?}");
    }
}
