//! `POST /v1/query` の分割実行 DML 語彙（`update`／`delete` の `mode: "partitioned"`・`chunk`、
//! `show_partitioned_dml`／`cancel_partitioned_dml` op、部分完了 `VD001`・取り消し `VD002` の
//! 409 本文）の契約全体を production ルータ経由（生バイトクライアント）で固定する層 A
//! 結合テスト（Issue #1130。対象ビヘイビア: NOSQL-12・SQL-19・RECOVER-11・RLS-9・ERR-4。
//! ポインタ: ADR `docs/design/partitioned-dml.md` 10 節）。
//!
//! 部分完了は「UNIQUE 列へ同じ値を `chunk: 1` で書き込む」ことで 2 チャンク目に確定的な
//! `23505` を起こして作る（時間に依存しない）。SQL 接続と HTTP の両方を同一 `EngineCore` 上で
//! 起動し、表層を跨いだ再送（ハッシュドメイン共有）も固定する。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::collections::BTreeMap;
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

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

/// `docs (n BIGINT, u TEXT UNIQUE)` を作り、tenant-a へ 3 行 seed する。
fn new_core(rows: usize) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql12-partitioned-dml");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_partitioned_dml_limits(PartitionedDmlLimits {
            chunk_rows: std::num::NonZeroUsize::new(3).expect("non-zero"),
            ..PartitionedDmlLimits::default()
        });
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)",
    )
    .expect("create table");
    let values: Vec<String> = (1..=rows)
        .map(|i| format!("({i}, {}, 'u{i}')", i * 10))
        .collect();
    exec(
        &core,
        TENANT_A,
        &format!(
            "INSERT INTO docs (id, n, u) VALUES {} USING OPERATION_ID 'seed-a'",
            values.join(", ")
        ),
    );
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
    match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected json object, got {other:?}"),
    }
}

/// alice（tenant-a）・bob（tenant-b）の HTTP セッションと、alice の SQL 接続を起動する。
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
    let sql = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");
    (alice, bob, sql)
}

fn query(who: &Both, body: &str) -> HttpResponse {
    let body = body.as_bytes();
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

fn strip_date(resp: &HttpResponse) -> String {
    let mut out = format!("{} {}\n", resp.status, resp.reason);
    for (name, value) in &resp.headers {
        if !name.eq_ignore_ascii_case("date") {
            out.push_str(&format!("{name}: {value}\n"));
        }
    }
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&resp.body));
    out
}

fn top_object(resp: &HttpResponse) -> BTreeMap<String, JsonValue> {
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).unwrap_or_else(|_| panic!("valid json body: {text:?}")) {
        JsonValue::Object(o) => o,
        other => panic!("expected object body, got {other:?}"),
    }
}

/// `{"error":{"data":{"committed":n,"operation_id":"..."}}}` を取り出す。
fn error_data(resp: &HttpResponse) -> (i64, String) {
    let top = top_object(resp);
    let Some(JsonValue::Object(err)) = top.get("error") else {
        panic!("error object expected: {top:?}");
    };
    let Some(JsonValue::Object(data)) = err.get("data") else {
        panic!("error.data expected: {err:?}");
    };
    let committed = match data.get("committed") {
        Some(JsonValue::Number(n)) => n.as_f64() as i64,
        other => panic!("committed must be a number, got {other:?}"),
    };
    let op_id = match data.get("operation_id") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("operation_id must be a string, got {other:?}"),
    };
    (committed, op_id)
}

/// `show`／`cancel` 応答の `rows` を `(status, rows)` の列として返す。
fn status_rows(resp: &HttpResponse) -> Vec<(String, i64)> {
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let top = top_object(resp);
    let Some(JsonValue::Array(rows)) = top.get("rows") else {
        panic!("rows expected: {top:?}");
    };
    rows.iter()
        .map(|r| {
            let JsonValue::Array(cells) = r else {
                panic!("row must be an array: {r:?}");
            };
            match (cells.first(), cells.get(1)) {
                (Some(JsonValue::String(s)), Some(JsonValue::Number(n))) => {
                    (s.clone(), n.as_f64() as i64)
                }
                other => panic!("unexpected cells {other:?}"),
            }
        })
        .collect()
}

fn count_rows(core: &EngineCore) -> usize {
    match exec(core, TENANT_A, "SELECT id FROM docs LIMIT 1000") {
        SqlOutcome::Query(q) => q.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn count_u_eq(core: &EngineCore, value: &str) -> usize {
    match exec(
        core,
        TENANT_A,
        &format!("SELECT id FROM docs WHERE u = '{value}' LIMIT 1000"),
    ) {
        SqlOutcome::Query(q) => q.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

const ALL_FILTER: &str = r#""filter":[{"column":"u","op":"not_null"}]"#;

fn upd(set_n: &str, extra: &str, op_id: &str) -> String {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"n":{set_n}}},{ALL_FILTER},"operation_id":"{op_id}"{extra}}}"#
    )
}

fn upd_u(value: &str, extra: &str, op_id: &str) -> String {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"u":"{value}"}},{ALL_FILTER},"operation_id":"{op_id}"{extra}}}"#
    )
}

fn del(extra: &str, op_id: &str) -> String {
    format!(r#"{{"op":"delete","table":"docs",{ALL_FILTER},"operation_id":"{op_id}"{extra}}}"#)
}

fn sql_ok(sql: &mut std::net::TcpStream, stmt: &str) -> String {
    common::send_simple_query(sql, stmt);
    let tag = common::read_command_complete(sql);
    common::read_ready_for_query(sql);
    tag
}

fn sql_err(sql: &mut std::net::TcpStream, stmt: &str, sqlstate: &str) {
    common::send_simple_query(sql, stmt);
    common::expect_error_response_with_sqlstate(sql, sqlstate);
    common::read_ready_for_query(sql);
}

// --- 成功 -------------------------------------------------------------------------

#[test]
fn partitioned_update_and_delete_succeed_with_the_atomic_body_shape() {
    let (core, _g) = new_core(3);
    let (a, _b, _sql) = spawn_both(core.clone());

    let resp = query(
        &a,
        &upd("77", r#","mode":"partitioned","chunk":2"#, "p-upd-1"),
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert_eq!(
        String::from_utf8_lossy(&resp.body),
        r#"{"updated":3,"operation_id":"p-upd-1"}"#
    );
    match exec(&core, TENANT_A, "SELECT n FROM docs WHERE u = 'u1' LIMIT 1") {
        SqlOutcome::Query(q) => assert_eq!(q.rows.len(), 1),
        other => panic!("{other:?}"),
    }

    let resp = query(&a, &del(r#","mode":"partitioned""#, "p-del-1"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert_eq!(
        String::from_utf8_lossy(&resp.body),
        r#"{"deleted":3,"operation_id":"p-del-1"}"#
    );
    assert_eq!(count_rows(&core), 0);
}

// --- 拒否（副作用ゼロ） -----------------------------------------------------------

#[test]
fn malformed_modifiers_are_rejected_without_side_effects() {
    let (core, _g) = new_core(3);
    let (a, _b, _sql) = spawn_both(core.clone());

    let cases: Vec<(String, &str)> = vec![
        // mode + where（単一行形）。
        (
            r#"{"op":"delete","table":"docs","where":{"id":1},"mode":"partitioned","operation_id":"r1"}"#
                .to_string(),
            "42601",
        ),
        (
            r#"{"op":"update","table":"docs","set":{"n":1},"where":{"id":1},"mode":"partitioned","operation_id":"r2"}"#
                .to_string(),
            "42601",
        ),
        // chunk のみ。
        (del(r#","chunk":2"#, "r3"), "42601"),
        // mode の値。
        (del(r#","mode":"atomic""#, "r4"), "42601"),
        (del(r#","mode":"PARTITIONED""#, "r5"), "42601"),
        (del(r#","mode":"""#, "r6"), "42601"),
        (del(r#","mode":1"#, "r7"), "42601"),
        (del(r#","mode":null"#, "r8"), "42601"),
        // chunk の値。
        (del(r#","mode":"partitioned","chunk":0"#, "r9"), "22000"),
        (del(r#","mode":"partitioned","chunk":1.5"#, "r10"), "22000"),
        (
            del(r#","mode":"partitioned","chunk":18446744073709551616"#, "r11"),
            "22000",
        ),
        (del(r#","mode":"partitioned","chunk":-1"#, "r12"), "42601"),
        (del(r#","mode":"partitioned","chunk":"2""#, "r13"), "42601"),
        // サーバー幅（3）超過。
        (del(r#","mode":"partitioned","chunk":4"#, "r14"), "22000"),
        // 述語形の不備。
        (
            r#"{"op":"delete","table":"docs","filter":[],"mode":"partitioned","operation_id":"r15"}"#
                .to_string(),
            "42601",
        ),
        (
            r#"{"op":"delete","table":"docs","filter":[{"column":"u","op":"lt","value":"a"}],"mode":"partitioned","operation_id":"r16"}"#
                .to_string(),
            "42601",
        ),
        // explain は未対応のまま未知キー。
        (del(r#","mode":"partitioned","explain":true"#, "r17"), "42601"),
    ];
    for (body, expected) in &cases {
        let resp = query(&a, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            *expected,
            "body={body} resp={resp:?}"
        );
    }
    // operation_id 欠落は 23502。
    let resp = query(
        &a,
        &format!(r#"{{"op":"delete","table":"docs",{ALL_FILTER},"mode":"partitioned"}}"#),
    );
    assert_eq!(http_common::wire_code_of(&resp), "23502", "resp={resp:?}");
    assert_eq!(count_rows(&core), 3);
}

// --- 部分完了・取り消し -------------------------------------------------------------

#[test]
fn partial_completion_is_409_vd001_with_data() {
    let (core, _g) = new_core(2);
    let (a, _b, _sql) = spawn_both(core.clone());

    let resp = query(
        &a,
        &upd_u("dup", r#","mode":"partitioned","chunk":1"#, "pc-1"),
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "VD001");
    assert_eq!(http_common::error_code_of(&resp), "PARTIAL_COMPLETION");
    let (committed, op_id) = error_data(&resp);
    assert_eq!(committed, 1);
    assert_eq!(op_id, "pc-1");
    http_common::assert_message_does_not_echo(&resp, TENANT_A);
    assert_eq!(count_u_eq(&core, "dup"), 1);
}

#[test]
fn cancel_then_resend_is_409_vd002_with_data_and_show_reports_states() {
    let (core, _g) = new_core(2);
    let (a, _b, _sql) = spawn_both(core.clone());
    let body = upd_u("dup", r#","mode":"partitioned","chunk":1"#, "cx-1");

    assert_eq!(http_common::wire_code_of(&query(&a, &body)), "VD001");
    let show = |op: &str| {
        query(
            &a,
            &format!(r#"{{"op":"show_partitioned_dml","table":"docs","operation_id":"{op}"}}"#),
        )
    };
    let cancel = |op: &str| {
        query(
            &a,
            &format!(r#"{{"op":"cancel_partitioned_dml","table":"docs","operation_id":"{op}"}}"#),
        )
    };
    assert_eq!(
        status_rows(&show("cx-1")),
        vec![("interrupted".to_string(), 1)]
    );
    assert_eq!(
        status_rows(&cancel("cx-1")),
        vec![("cancelled".to_string(), 1)]
    );
    assert_eq!(
        status_rows(&show("cx-1")),
        vec![("cancelled".to_string(), 1)]
    );

    let resp = query(&a, &body);
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "VD002");
    assert_eq!(error_data(&resp), (1, "cx-1".to_string()));

    // 完了済みジョブへの cancel は completed のまま。
    let resp = query(&a, &del(r#","mode":"partitioned""#, "cx-2"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert_eq!(
        status_rows(&cancel("cx-2")),
        vec![("completed".to_string(), 2)]
    );
    assert_eq!(
        status_rows(&show("cx-2")),
        vec![("completed".to_string(), 2)]
    );
}

// --- 表層横断（ハッシュドメイン共有） ------------------------------------------------

#[test]
fn sql_interrupted_job_resumes_through_nosql_and_back() {
    let (core, _g) = new_core(2);
    let (a, _b, mut sql) = spawn_both(core.clone());

    // SQL で中断 → 原因除去 → NoSQL で同内容を再送 → 累計で完了。
    sql_err(
        &mut sql,
        "UPDATE docs SET u = 'dup' WHERE u IS NOT NULL USING OPERATION_ID 'xs-1' PARTITIONED CHUNK 1",
        "VD001",
    );
    assert_eq!(
        sql_ok(
            &mut sql,
            "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-1'"
        ),
        "UPDATE 1"
    );
    let resp = query(
        &a,
        &upd_u("dup", r#","mode":"partitioned","chunk":1"#, "xs-1"),
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert_eq!(
        String::from_utf8_lossy(&resp.body),
        r#"{"updated":2,"operation_id":"xs-1"}"#
    );

    // NoSQL で中断 → 原因除去 → SQL で再送 → 累計で完了。
    let (core2, _g2) = new_core(2);
    let (a2, _b2, mut sql2) = spawn_both(core2.clone());
    let resp = query(
        &a2,
        &upd_u("dup", r#","mode":"partitioned","chunk":1"#, "xs-2"),
    );
    assert_eq!(http_common::wire_code_of(&resp), "VD001", "resp={resp:?}");
    sql_ok(
        &mut sql2,
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-2'",
    );
    assert_eq!(
        sql_ok(
            &mut sql2,
            "UPDATE docs SET u = 'dup' WHERE u IS NOT NULL USING OPERATION_ID 'xs-2' PARTITIONED CHUNK 1"
        ),
        "UPDATE 2"
    );
}

#[test]
fn completed_job_resend_from_the_other_surface_is_23505_or_22023() {
    let (core, _g) = new_core(3);
    let (a, _b, mut sql) = spawn_both(core.clone());

    // NoSQL で完了 → SQL 再送は 23505。
    let resp = query(&a, &upd("5", r#","mode":"partitioned""#, "cd-1"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    sql_err(
        &mut sql,
        "UPDATE docs SET n = 5 WHERE u IS NOT NULL USING OPERATION_ID 'cd-1' PARTITIONED",
        "23505",
    );
    // SQL で完了 → NoSQL 再送は 23505。内容が違えば 22023。
    sql_ok(
        &mut sql,
        "UPDATE docs SET n = 6 WHERE u IS NOT NULL USING OPERATION_ID 'cd-2' PARTITIONED",
    );
    let resp = query(&a, &upd("6", r#","mode":"partitioned""#, "cd-2"));
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
    let resp = query(&a, &upd("8", r#","mode":"partitioned""#, "cd-2"));
    assert_eq!(http_common::wire_code_of(&resp), "22023", "resp={resp:?}");
}

#[test]
fn atomic_and_partitioned_nosql_do_not_share_a_hash_in_either_direction() {
    let (core, _g) = new_core(3);
    let (a, _b, _sql) = spawn_both(core.clone());

    assert_eq!(query(&a, &upd("5", "", "at-1")).status, 200);
    let resp = query(&a, &upd("5", r#","mode":"partitioned""#, "at-1"));
    assert_eq!(http_common::wire_code_of(&resp), "22023", "resp={resp:?}");

    assert_eq!(
        query(&a, &upd("6", r#","mode":"partitioned""#, "at-2")).status,
        200
    );
    let resp = query(&a, &upd("6", "", "at-2"));
    assert_eq!(http_common::wire_code_of(&resp), "22023", "resp={resp:?}");
}

// --- 照会・取り消し（テナント境界・同一応答） ------------------------------------------

#[test]
fn show_and_cancel_not_found_cases_are_byte_identical_and_tenant_isolated() {
    let (core, _g) = new_core(2);
    // tenant-b にもテーブルは見えるが、ジョブは持たない。
    let (a, b, _sql) = spawn_both(core.clone());
    let resp = query(
        &a,
        &upd_u("dup", r#","mode":"partitioned","chunk":1"#, "nf-1"),
    );
    assert_eq!(http_common::wire_code_of(&resp), "VD001");

    for op in ["show_partitioned_dml", "cancel_partitioned_dml"] {
        let q = |who: &Both, table: &str, id: &str| {
            query(
                who,
                &format!(r#"{{"op":"{op}","table":"{table}","operation_id":"{id}"}}"#),
            )
        };
        let no_job = q(&a, "docs", "no-such");
        assert_eq!(no_job.status, 200);
        assert!(status_rows(&no_job).is_empty());
        let no_table = q(&a, "no_such_table", "nf-1");
        let other_tenant = q(&b, "docs", "nf-1");
        assert_eq!(strip_date(&no_job), strip_date(&no_table), "{op}");
        assert_eq!(strip_date(&no_job), strip_date(&other_tenant), "{op}");
    }
    // 他テナントの cancel は alice のジョブを変えない。
    let resp = query(
        &a,
        r#"{"op":"show_partitioned_dml","table":"docs","operation_id":"nf-1"}"#,
    );
    assert_eq!(status_rows(&resp), vec![("interrupted".to_string(), 1)]);
}

#[test]
fn job_ops_reject_malformed_requests() {
    let (core, _g) = new_core(1);
    let (a, _b, _sql) = spawn_both(core);
    for op in ["show_partitioned_dml", "cancel_partitioned_dml"] {
        let cases = [
            // 不正な table。
            (
                format!(r#"{{"op":"{op}","table":"a b","operation_id":"x"}}"#),
                "42601",
            ),
            // operation_id の欠落・null・空文字。
            (format!(r#"{{"op":"{op}","table":"docs"}}"#), "23502"),
            (
                format!(r#"{{"op":"{op}","table":"docs","operation_id":null}}"#),
                "23502",
            ),
            (
                format!(r#"{{"op":"{op}","table":"docs","operation_id":""}}"#),
                "23502",
            ),
            // 未知キー（tenant_id 自己申告を含む）。
            (
                format!(r#"{{"op":"{op}","table":"docs","operation_id":"x","tenant_id":"t"}}"#),
                "42601",
            ),
            (
                format!(r#"{{"op":"{op}","table":"docs","operation_id":"x","extra":1}}"#),
                "42601",
            ),
            // table 欠落。
            (format!(r#"{{"op":"{op}","operation_id":"x"}}"#), "42601"),
        ];
        for (body, code) in &cases {
            let resp = query(&a, body);
            assert_eq!(http_common::wire_code_of(&resp), *code, "body={body}");
        }
    }
}

// --- 語彙 -----------------------------------------------------------------------

#[test]
fn op_name_variants_are_unsupported() {
    let (core, _g) = new_core(1);
    let (a, _b, _sql) = spawn_both(core);
    for op in [
        "SHOW_PARTITIONED_DML",
        " show_partitioned_dml",
        "show_partitioned_dml ",
        "Cancel_Partitioned_Dml",
        "show_partitioned",
    ] {
        let body = format!(r#"{{"op":"{op}","table":"docs","operation_id":"x"}}"#);
        let resp = query(&a, &body);
        assert_eq!(resp.status, 501, "op={op:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "0A000");
    }
}
