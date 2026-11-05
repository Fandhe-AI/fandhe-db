//! `Idempotency-Key` ヘッダ経由の `operation_id` が SQL 表層と同じ台帳を共有し、テナント境界を
//! 越えないことを、production ルータ（HTTP）と SQL wire の両方を同一 `EngineCore` 上で起動して
//! 固定する層 A 結合テスト（Issue #2065。親 #2062。対象ビヘイビア:
//! `docs/spec/04-behavior/http-transport.md` HTTP-14、`docs/spec/04-behavior/recovery.md`
//! RECOVER-2、RLS-9）。
//!
//! ## 分担
//!
//! - `http14_idempotency_key.rs`（Issue #2064）: HTTP 単独（ヘッダ／本文の 4 パターン・同一台帳）
//! - `nosql12_update_delete.rs`・`nosql12_partitioned_dml.rs`: 本文 `operation_id` の表層横断
//! - 本ファイル: **ヘッダだけ**で再送したときの表層横断（SQL→ヘッダ・ヘッダ→SQL）と、
//!   2 テナントが同じキーを使っても衝突せず他テナントの使用有無が応答に現れないこと
//!
//! 表層間でハッシュが一致するよう、`set` は単一列に限る。テナント境界の検証では UNIQUE 制約の
//! 跨テナント挙動という別要因を混ぜないため UNIQUE 無しの表を使う。

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::parser::PartitionedDmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs (n BIGINT, u TEXT [UNIQUE])` を作る。分割実行のチャンク幅は 3。
fn new_core(unique: bool) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("http14-idempotency-key-cross-surface");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_partitioned_dml_limits(PartitionedDmlLimits {
            chunk_rows: std::num::NonZeroUsize::new(3).expect("non-zero"),
            ..PartitionedDmlLimits::default()
        });
    let mut session = SessionState::default();
    session.allow_ddl();
    let ddl = if unique {
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)"
    } else {
        "CREATE TABLE docs (n BIGINT, u TEXT)"
    };
    core.execute_sql_in_session_legacy(&ctx("sys"), &mut session, ddl)
        .expect("create table");
    (Arc::new(core), guard)
}

/// `tenant` の `rows` 行（id 1..=rows）を `op_id` で seed する。
fn seed(core: &EngineCore, tenant: &str, rows: usize, op_id: &str) {
    let values: Vec<String> = (1..=rows)
        .map(|i| format!("({i}, {}, 'u{i}')", i * 10))
        .collect();
    core.execute_sql_in_session_legacy(
        &ctx(tenant),
        &mut SessionState::default(),
        &format!(
            "INSERT INTO docs (id, n, u) VALUES {} USING OPERATION_ID '{op_id}'",
            values.join(", ")
        ),
    )
    .expect("seed rows");
}

fn count_rows(core: &EngineCore, tenant: &str) -> usize {
    let outcome = core
        .execute_sql_in_session_legacy(
            &ctx(tenant),
            &mut SessionState::default(),
            "SELECT u FROM docs LIMIT 1000",
        )
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    result.rows.len()
}

/// 1 ユーザー分の HTTP セッションと SQL 接続。
struct Peer {
    http_addr: SocketAddr,
    token: String,
    sql: TcpStream,
}

fn login(http_addr: SocketAddr, user: &str, password: &str) -> String {
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
    match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected json object, got {other:?}"),
    }
}

/// alice（tenant-a）・bob（tenant-b）それぞれの HTTP セッションと SQL 接続を起動する。
fn spawn(core: Arc<EngineCore>) -> (Peer, Peer) {
    let users_path = common::write_user_store_file(&[
        ("alice", TENANT_A, "pw-alice"),
        ("bob", TENANT_B, "pw-bob"),
    ]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let sql_addr = common::spawn_server_with_engine(&users_path, core);
    let alice = Peer {
        http_addr,
        token: login(http_addr, "alice", "pw-alice"),
        sql: common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice"),
    };
    let bob = Peer {
        http_addr,
        token: login(http_addr, "bob", "pw-bob"),
        sql: common::authenticate_to_ready_for_query(sql_addr, "bob", "pw-bob"),
    };
    (alice, bob)
}

/// `header` が `Some` なら `Idempotency-Key` を付けて `POST /v1/query` する。
fn query(p: &Peer, body: &str, header: Option<&str>) -> HttpResponse {
    let auth = format!("Bearer {}", p.token);
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
        p.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn strip_date(resp: &HttpResponse) -> String {
    let mut out = format!("{} {}\n", resp.status, resp.reason);
    for (name, value) in &resp.headers {
        if !name.eq_ignore_ascii_case("date") {
            out.push_str(&format!("{name}: {value}\n"));
        }
    }
    out.push('\n');
    out.push_str(&body_text(resp));
    out
}

fn sql_ok(p: &mut Peer, stmt: &str) -> String {
    common::send_simple_query(&mut p.sql, stmt);
    let tag = common::read_command_complete(&mut p.sql);
    common::read_ready_for_query(&mut p.sql);
    tag
}

fn sql_err(p: &mut Peer, stmt: &str, sqlstate: &str) {
    common::send_simple_query(&mut p.sql, stmt);
    common::expect_error_response_with_sqlstate(&mut p.sql, sqlstate);
    common::read_ready_for_query(&mut p.sql);
}

/// `{"error":{"data":{"operation_id":"..."}}}` の `operation_id` を取り出す。
fn error_op_id(resp: &HttpResponse) -> String {
    let text = body_text(resp);
    let JsonValue::Object(top) = parse_json(&text).expect("json body") else {
        panic!("object expected: {text}");
    };
    let Some(JsonValue::Object(err)) = top.get("error") else {
        panic!("error object expected: {text}");
    };
    let Some(JsonValue::Object(data)) = err.get("data") else {
        panic!("error.data expected: {text}");
    };
    match data.get("operation_id") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("operation_id must be a string, got {other:?}"),
    }
}

fn assert_code(resp: &HttpResponse, status: u16, code: &str) {
    assert_eq!(resp.status, status, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(resp), code, "resp={resp:?}");
}

// --- 要求本文（`operation_id` は含めずヘッダで渡す） ------------------------------------

fn insert_body(id: u64, n: u64) -> String {
    format!(r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"n":{n},"u":"ins{id}"}}]}}"#)
}

fn insert_sql(id: u64, n: u64, op: &str) -> String {
    format!("INSERT INTO docs (id, n, u) VALUES ({id}, {n}, 'ins{id}') USING OPERATION_ID '{op}'")
}

fn update_body(id: u64, n: u64) -> String {
    format!(r#"{{"op":"update","table":"docs","set":{{"n":{n}}},"where":{{"id":{id}}}}}"#)
}

fn update_sql(id: u64, n: u64, op: &str) -> String {
    format!("UPDATE docs SET n = {n} WHERE id = {id} USING OPERATION_ID '{op}'")
}

fn delete_body(id: u64) -> String {
    format!(r#"{{"op":"delete","table":"docs","where":{{"id":{id}}}}}"#)
}

fn delete_sql(id: u64, op: &str) -> String {
    format!("DELETE FROM docs WHERE id = {id} USING OPERATION_ID '{op}'")
}

const ALL_FILTER: &str = r#""filter":[{"column":"u","op":"not_null"}]"#;

fn predicate_update_body(n: u64) -> String {
    format!(r#"{{"op":"update","table":"docs","set":{{"n":{n}}},{ALL_FILTER}}}"#)
}

fn predicate_update_sql(n: u64, op: &str) -> String {
    format!("UPDATE docs SET n = {n} WHERE u IS NOT NULL USING OPERATION_ID '{op}'")
}

fn partitioned_u_body(value: &str, chunk: u32) -> String {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"u":"{value}"}},{ALL_FILTER},"mode":"partitioned","chunk":{chunk}}}"#
    )
}

fn partitioned_u_sql(value: &str, op: &str, chunk: u32) -> String {
    format!(
        "UPDATE docs SET u = '{value}' WHERE u IS NOT NULL USING OPERATION_ID '{op}' PARTITIONED CHUNK {chunk}"
    )
}

// --- 相互再送 ----------------------------------------------------------------------

#[test]
fn insert_sql_then_header_resend_is_23505_and_mismatch_is_22023() {
    let (core, _g) = new_core(false);
    let (mut a, _b) = spawn(core.clone());
    assert_eq!(sql_ok(&mut a, &insert_sql(1, 1, "xs-ins")), "INSERT 0 1");

    let resp = query(&a, &insert_body(1, 1), Some("xs-ins"));
    assert_code(&resp, 409, "23505");
    let resp = query(&a, &insert_body(2, 2), Some("xs-ins"));
    assert_code(&resp, 400, "22023");
    assert_eq!(count_rows(&core, TENANT_A), 1);
}

#[test]
fn insert_header_then_sql_resend_is_23505_and_mismatch_is_22023() {
    let (core, _g) = new_core(false);
    let (mut a, _b) = spawn(core.clone());
    let resp = query(&a, &insert_body(1, 1), Some("xs-ins-h"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(body_text(&resp).contains(r#""operation_id":"xs-ins-h""#));

    sql_err(&mut a, &insert_sql(1, 1, "xs-ins-h"), "23505");
    sql_err(&mut a, &insert_sql(2, 2, "xs-ins-h"), "22023");
    assert_eq!(count_rows(&core, TENANT_A), 1);
}

#[test]
fn update_where_cross_surface_resend_via_header_both_directions() {
    let (core, _g) = new_core(false);
    let (mut a, _b) = spawn(core.clone());
    seed(&core, TENANT_A, 3, "seed-a");

    // SQL で書いた id をヘッダだけで再送する。
    assert_eq!(sql_ok(&mut a, &update_sql(1, 5, "xs-upd-1")), "UPDATE 1");
    assert_code(
        &query(&a, &update_body(1, 5), Some("xs-upd-1")),
        409,
        "23505",
    );
    assert_code(
        &query(&a, &update_body(1, 6), Some("xs-upd-1")),
        400,
        "22023",
    );

    // ヘッダで書いた id を SQL で再送する。
    let resp = query(&a, &update_body(2, 5), Some("xs-upd-2"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    sql_err(&mut a, &update_sql(2, 5, "xs-upd-2"), "23505");
    sql_err(&mut a, &update_sql(2, 6, "xs-upd-2"), "22023");
}

#[test]
fn delete_where_cross_surface_resend_via_header_both_directions() {
    let (core, _g) = new_core(false);
    let (mut a, _b) = spawn(core.clone());
    seed(&core, TENANT_A, 4, "seed-a");

    assert_eq!(sql_ok(&mut a, &delete_sql(1, "xs-del-1")), "DELETE 1");
    assert_code(&query(&a, &delete_body(1), Some("xs-del-1")), 409, "23505");
    assert_code(&query(&a, &delete_body(2), Some("xs-del-1")), 400, "22023");

    let resp = query(&a, &delete_body(3), Some("xs-del-2"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    sql_err(&mut a, &delete_sql(3, "xs-del-2"), "23505");
    sql_err(&mut a, &delete_sql(4, "xs-del-2"), "22023");
    // 再送・不一致はいずれも副作用なし（削除は id 1 と 3 の 2 行だけ）。
    assert_eq!(count_rows(&core, TENANT_A), 2);
}

#[test]
fn predicate_update_cross_surface_resend_via_header_both_directions() {
    let (core, _g) = new_core(false);
    let (mut a, _b) = spawn(core.clone());
    seed(&core, TENANT_A, 3, "seed-a");

    assert_eq!(
        sql_ok(&mut a, &predicate_update_sql(5, "xs-pu-1")),
        "UPDATE 3"
    );
    assert_code(
        &query(&a, &predicate_update_body(5), Some("xs-pu-1")),
        409,
        "23505",
    );
    assert_code(
        &query(&a, &predicate_update_body(6), Some("xs-pu-1")),
        400,
        "22023",
    );

    let resp = query(&a, &predicate_update_body(7), Some("xs-pu-2"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    sql_err(&mut a, &predicate_update_sql(7, "xs-pu-2"), "23505");
    sql_err(&mut a, &predicate_update_sql(8, "xs-pu-2"), "22023");
}

#[test]
fn partitioned_job_interrupted_by_sql_resumes_through_header_only() {
    let (core, _g) = new_core(true);
    let (mut a, _b) = spawn(core.clone());
    seed(&core, TENANT_A, 2, "seed-a");

    // SQL で中断 → 原因除去 → ヘッダだけで同内容を再送 → 累計で完了。
    sql_err(&mut a, &partitioned_u_sql("dup", "xs-part", 1), "VD001");
    assert_eq!(
        sql_ok(
            &mut a,
            "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-1'"
        ),
        "UPDATE 1"
    );
    let resp = query(&a, &partitioned_u_body("dup", 1), Some("xs-part"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert_eq!(
        body_text(&resp),
        r#"{"updated":2,"operation_id":"xs-part"}"#
    );
}

#[test]
fn partitioned_job_interrupted_by_header_resumes_through_sql() {
    let (core, _g) = new_core(true);
    let (mut a, _b) = spawn(core.clone());
    seed(&core, TENANT_A, 2, "seed-a");

    let resp = query(&a, &partitioned_u_body("dup", 1), Some("xs-part-h"));
    assert_code(&resp, 409, "VD001");
    assert_eq!(error_op_id(&resp), "xs-part-h");
    sql_ok(
        &mut a,
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-2'",
    );
    assert_eq!(
        sql_ok(&mut a, &partitioned_u_sql("dup", "xs-part-h", 1)),
        "UPDATE 2"
    );
}

#[test]
fn body_and_header_agreeing_still_share_the_sql_ledger_entry() {
    let (core, _g) = new_core(false);
    let (mut a, _b) = spawn(core.clone());
    assert_eq!(sql_ok(&mut a, &insert_sql(1, 1, "xs-both")), "INSERT 0 1");
    let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"n":1,"u":"ins1"}],"operation_id":"xs-both"}"#;
    assert_code(&query(&a, body, Some("xs-both")), 409, "23505");
}

// --- テナント境界（RLS-9） ---------------------------------------------------------

#[test]
fn same_key_in_two_tenants_does_not_collide_on_either_surface() {
    let (core, _g) = new_core(false);
    let (mut a, mut b) = spawn(core.clone());

    // ヘッダ同士: 同じキー・同じ内容でも別テナントなら衝突しない。
    let resp = query(&a, &insert_body(1, 1), Some("shared-k"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let resp = query(&b, &insert_body(1, 1), Some("shared-k"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(body_text(&resp).contains(r#""operation_id":"shared-k""#));
    // それぞれの自己再送は台帳が働いて 23505。
    assert_code(
        &query(&a, &insert_body(1, 1), Some("shared-k")),
        409,
        "23505",
    );
    assert_code(
        &query(&b, &insert_body(1, 1), Some("shared-k")),
        409,
        "23505",
    );

    // alice の SQL → bob のヘッダ。
    assert_eq!(sql_ok(&mut a, &insert_sql(2, 2, "shared-k2")), "INSERT 0 1");
    let resp = query(&b, &insert_body(2, 2), Some("shared-k2"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    // alice のヘッダ → bob の SQL。
    let resp = query(&a, &insert_body(3, 3), Some("shared-k3"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert_eq!(sql_ok(&mut b, &insert_sql(3, 3, "shared-k3")), "INSERT 0 1");

    // 各テナントの行数は自分の書き込み分だけ。
    assert_eq!(count_rows(&core, TENANT_A), 3);
    assert_eq!(count_rows(&core, TENANT_B), 3);
}

#[test]
fn foreign_key_use_is_invisible_in_http_response_bytes() {
    // core1: 他テナントはキーを使っていない。core2: alice が内容違いで使用済み。
    let (core1, _g1) = new_core(false);
    let (_a1, b1) = spawn(core1);
    let (core2, _g2) = new_core(false);
    let (a2, b2) = spawn(core2);
    assert_eq!(
        query(&a2, &insert_body(2, 2), Some("k")).status,
        200,
        "alice must use the key first"
    );

    let r1 = query(&b1, &insert_body(1, 1), Some("k"));
    let r2 = query(&b2, &insert_body(1, 1), Some("k"));
    assert_eq!(r1.status, 200, "resp={r1:?}");
    assert_eq!(strip_date(&r1), strip_date(&r2));

    // bob の自己再送の応答も、他テナントの使用有無に依存しない。
    let s1 = query(&b1, &insert_body(1, 1), Some("k"));
    let s2 = query(&b2, &insert_body(1, 1), Some("k"));
    assert_code(&s1, 409, "23505");
    assert_eq!(strip_date(&s1), strip_date(&s2));
}

#[test]
fn foreign_key_use_is_invisible_for_zero_row_update_response() {
    let (core1, _g1) = new_core(false);
    seed(&core1, TENANT_A, 1, "seed-a");
    let (_a1, b1) = spawn(core1);
    let (core2, _g2) = new_core(false);
    seed(&core2, TENANT_A, 1, "seed-a");
    let (a2, b2) = spawn(core2);
    assert_eq!(query(&a2, &update_body(1, 9), Some("k")).status, 200);

    // bob から見て id=1 は他テナント所有（0 行 update）。応答は core 間で同一。
    let r1 = query(&b1, &update_body(1, 5), Some("k"));
    let r2 = query(&b2, &update_body(1, 5), Some("k"));
    assert_eq!(r1.status, 200, "resp={r1:?}");
    assert!(body_text(&r1).contains(r#""updated":0"#));
    assert_eq!(strip_date(&r1), strip_date(&r2));
}

#[test]
fn foreign_key_use_is_invisible_on_sql_surface() {
    let (core1, _g1) = new_core(false);
    let (_a1, mut b1) = spawn(core1);
    let (core2, _g2) = new_core(false);
    let (a2, mut b2) = spawn(core2);
    assert_eq!(query(&a2, &insert_body(2, 2), Some("k")).status, 200);

    let t1 = sql_ok(&mut b1, &insert_sql(1, 1, "k"));
    let t2 = sql_ok(&mut b2, &insert_sql(1, 1, "k"));
    assert_eq!(t1, "INSERT 0 1");
    assert_eq!(t1, t2);
}
