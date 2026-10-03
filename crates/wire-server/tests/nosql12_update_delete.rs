//! `POST /v1/query`（`op: "update"`／`op: "delete"`）の契約全体
//! （`where`／`filter` 排他判定・`operation_id` 必須化・台帳照合による
//! 再送判定・SQL 表層とのパリティ・TABLE-12・RLS-9 秘匿）を production
//! ルータ経由（生バイトクライアント）で固定する層 A 結合テスト（Issue #876・
//! TASK-186。対象ビヘイビア: `docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-6・NOSQL-12・`docs/spec/04-behavior/sql-surface.md` SQL-17・
//! SQL-18・`docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10・
//! `docs/spec/04-behavior/data-model.md` TABLE-12・
//! `docs/spec/04-behavior/rls.md` RLS-9）。
//!
//! ## 役割分担（重複再検証をしない）
//!
//! - `crates/wire-server/src/http/query/update.rs`・`delete.rs`・
//!   `dml_target.rs` 内の unit tests: `map_set_assignments`・
//!   `bind_target_form` の境界を検証済み。本ファイルはルータ・HTTP
//!   フレーミングを経由した **wire 越し** の観測に徹する。
//! - `crates/wire-server/tests/nosql9_op_allowlist.rs`・
//!   `nosql1_op_vocabulary.rs`: `filter: []`（空配列）が `42601` になること・
//!   `where` 形が実行結線済みで基本的な成功／`23502` 経路を固定済み。
//!   本ファイルはそれらと重複せず、述語形（`filter` 非空。Issue #1062）の
//!   台帳照合（`23505`／`22023`）・SQL↔NoSQL パリティ・RLS-9 秘匿に集中する。
//! - `crates/engine/tests/sql_update_delete_session_public_api.rs`:
//!   `EngineCore::execute_bound_update_in_session`／
//!   `execute_bound_delete_in_session` が SQL 表層と同一の実行器・台帳
//!   キー空間へ到達することの確定オラクル。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql12-update-delete-layer-a");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `tenant-b`（id=100）へ、HTTP を経由せず `EngineCore::insert_row` で
/// 直接 1 行 seed する（他テナント所有 id の RLS-9 対照用）。
fn seed_foreign_tenant(core: &EngineCore) {
    let b = PolicyContext::new(TENANT_B).expect("valid tenant");
    core.insert_row(
        &b,
        TABLE,
        100,
        &RowInput {
            tenant_id: TENANT_B,
            visibility: Visibility::Public,
            embedding: &[0.0, 1.0, 0.0],
            metadata: b"seed-b",
        },
        Some(&OperationId::parse("seed-op-tenant-b").expect("valid operation_id")),
    )
    .expect("seed tenant-b row id=100");
}

/// HTTP（`alice`／tenant-a）・SQL wire（同一 core・同一 `alice`）の両方を
/// 同一 `core` 上で起動する（`nosql6_insert.rs::spawn_both` と同型）。
struct Both {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn spawn_both(core: Arc<EngineCore>) -> (Both, std::net::TcpStream) {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
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
    let token = match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let sql_addr = common::spawn_server_with_engine(&users_path, core);
    let sql_stream = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");

    (Both { http_addr, token }, sql_stream)
}

fn query(both: &Both, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", both.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        both.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// 応答から `Date` ヘッダを除いた文字列（時刻に依存する行だけを除外した
/// バイト同一性比較のため。`http4_session.rs::strip_date` と同じ意図）。
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

fn insert_body(id: u64, lang: &str, op_id: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2,0.3],"lang":"{lang}"}}],"operation_id":"{op_id}"}}"#
    )
    .into_bytes()
}

fn update_body(id: u64, lang: &str, op_id: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"lang":"{lang}"}},"where":{{"id":{id}}},"operation_id":"{op_id}"}}"#
    )
    .into_bytes()
}

fn delete_body(id: u64, op_id: &str) -> Vec<u8> {
    format!(r#"{{"op":"delete","table":"docs","where":{{"id":{id}}},"operation_id":"{op_id}"}}"#)
        .into_bytes()
}

fn update_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!("UPDATE docs SET lang = '{lang}' WHERE id = {id} USING OPERATION_ID '{op_id}'")
}

fn delete_sql(id: u64, op_id: &str) -> String {
    format!("DELETE FROM docs WHERE id = {id} USING OPERATION_ID '{op_id}'")
}

/// `core` を tenant-a（`with_visibilities([Public, Private])`）で読み戻し、
/// 可視な行 `(id, lang)` の一覧を返す（wire を経由しない engine API 直
/// 呼び出しのオラクル。`nosql6_insert.rs::read_back_ids` と同じ注意）。
fn read_back_langs(core: &EngineCore) -> Vec<(u64, Option<String>)> {
    let ctx = PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant-a ctx");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT lang FROM docs LIMIT 100")
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    result
        .rows
        .iter()
        .map(|row| {
            let lang = row.cells.first().and_then(|c| match c {
                engine::sql::exec::Cell::Text(s) => Some(s.clone()),
                engine::sql::exec::Cell::Null => None,
                other => panic!("unexpected cell kind: {other:?}"),
            });
            (row.id, lang)
        })
        .collect()
}

// --- A: 成功経路 -----------------------------------------------------------

#[test]
fn update_success_updates_the_row_and_returns_updated_1() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    let resp = query(&both, &insert_body(1, "ja", "n12-seed-1"));
    assert_eq!(resp.status, 200, "resp={resp:?}");

    let resp = query(&both, &update_body(1, "en", "n12-update-1"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(body.contains(r#""updated":1"#), "{body}");

    let rows = read_back_langs(&core);
    assert_eq!(rows, vec![(1, Some("en".to_string()))]);
}

#[test]
fn delete_success_removes_the_row_and_returns_deleted_1() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    let resp = query(&both, &insert_body(1, "ja", "n12-seed-2"));
    assert_eq!(resp.status, 200, "resp={resp:?}");

    let resp = query(&both, &delete_body(1, "n12-delete-1"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(body.contains(r#""deleted":1"#), "{body}");

    let rows = read_back_langs(&core);
    assert!(rows.is_empty(), "{rows:?}");
}

// --- B: operation_id 必須化 --------------------------------------------------

#[test]
fn update_and_delete_reject_missing_null_and_empty_operation_id_with_23502() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-seed-3"));

    let bodies: [&[u8]; 6] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1}}"#,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":null}"#,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":""}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1}}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":null}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":""}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "23502");
    }
    // 副作用なし。
    let rows = read_back_langs(&core);
    assert_eq!(rows, vec![(1, Some("ja".to_string()))]);
}

// --- C: 未存在テーブル -------------------------------------------------------

#[test]
fn update_and_delete_reject_undefined_table_with_42p01() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"missing","set":{"lang":"en"},"where":{"id":1},"operation_id":"n12-c1"}"#,
        br#"{"op":"delete","table":"missing","where":{"id":1},"operation_id":"n12-c2"}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 404, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42P01");
    }
}

// --- D: 42601（where/filter 排他・set 禁止列・set 空・tenant_id 自己申告） --

#[test]
fn update_and_delete_reject_where_and_filter_both_present_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"filter":[]}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"filter":[]}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

#[test]
fn update_and_delete_reject_neither_where_nor_filter_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"}}"#,
        br#"{"op":"delete","table":"docs"}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

#[test]
fn update_rejects_forbidden_set_columns_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let bodies: [&[u8]; 3] = [
        br#"{"op":"update","table":"docs","set":{"id":2},"where":{"id":1},"operation_id":"n12-d1"}"#,
        br#"{"op":"update","table":"docs","set":{"tenant_id":"evil"},"where":{"id":1},"operation_id":"n12-d2"}"#,
        br#"{"op":"update","table":"docs","set":{"visibility":"public"},"where":{"id":1},"operation_id":"n12-d3"}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

#[test]
fn update_rejects_empty_set_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{},"where":{"id":1},"operation_id":"n12-d4"}"#,
    );
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

#[test]
fn update_and_delete_reject_tenant_id_json_self_declaration_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"tenant_id":"evil"}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"tenant_id":"evil"}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

// --- E: 22000（where.id 型不正・set 値の型不一致） --------------------------

#[test]
fn update_rejects_non_integer_where_id_matching_sql_lexer_parity() {
    // SQL 表層の字句解析とのパリティ（`dml_target.rs` モジュール doc 参照）:
    // `1.5`（小数。単一の `Number` トークンとして単一行形に振り分けられた
    // うえで `bind_update` の `u64` パース失敗により `22000`）と `-1`
    // （`-` が独立した `Punct` トークンのため単一行形に一致せず述語形へ
    // 振り分けられ `validate_update` が `42601` で拒否）は異なる `wire_code`
    // になる。
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1.5},"operation_id":"n12-e1"}"#,
    );
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22000");

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":-1},"operation_id":"n12-e2"}"#,
    );
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

#[test]
fn update_rejects_set_value_type_mismatch_with_22000() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-seed-e"));

    let bodies: [&[u8]; 3] = [
        br#"{"op":"update","table":"docs","set":{"lang":1},"where":{"id":1},"operation_id":"n12-e3"}"#,
        br#"{"op":"update","table":"docs","set":{"embedding":"[1,2,3]"},"where":{"id":1},"operation_id":"n12-e4"}"#,
        br#"{"op":"update","table":"docs","set":{"embedding":[1,2]},"where":{"id":1},"operation_id":"n12-e5"}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "22000");
    }
    // 副作用なし。
    let rows = read_back_langs(&core);
    assert_eq!(rows, vec![(1, Some("ja".to_string()))]);
}

// --- F: 台帳照合（表層を跨いだ再送判定のパリティ） ---------------------------

#[test]
fn update_resend_same_operation_id_same_content_is_23505() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-seed-f1"));
    let resp = query(&both, &update_body(1, "en", "n12-f1"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let resp = query(&both, &update_body(1, "en", "n12-f1"));
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

#[test]
fn update_resend_same_operation_id_different_content_is_22023() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-seed-f2"));
    let resp = query(&both, &update_body(1, "en", "n12-f2"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let resp = query(&both, &update_body(1, "fr", "n12-f2"));
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22023");
}

#[test]
fn delete_resend_same_operation_id_is_23505() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-seed-f3"));
    let resp = query(&both, &delete_body(1, "n12-f3"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let resp = query(&both, &delete_body(1, "n12-f3"));
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

#[test]
fn cross_surface_update_resend_same_operation_id_same_content_is_23505_sql_then_nosql() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-seed-f4"));
    common::send_simple_query(&mut sql, &update_sql(1, "en", "n12-f4"));
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(&both, &update_body(1, "en", "n12-f4"));
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

#[test]
fn cross_surface_update_resend_same_operation_id_same_content_is_23505_nosql_then_sql() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-seed-f5"));
    let resp = query(&both, &update_body(1, "en", "n12-f5"));
    assert_eq!(resp.status, 200, "resp={resp:?}");

    common::send_simple_query(&mut sql, &update_sql(1, "en", "n12-f5"));
    common::expect_error_response_with_sqlstate(&mut sql, "23505");
    common::read_ready_for_query(&mut sql);
}

#[test]
fn cross_surface_delete_resend_same_operation_id_is_23505_sql_then_nosql() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-seed-f6"));
    common::send_simple_query(&mut sql, &delete_sql(1, "n12-f6"));
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "DELETE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(&both, &delete_body(1, "n12-f6"));
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

// --- G: RLS-9 応答バイト列の同一性（他テナント所有 id・未存在 id） ----------

#[test]
fn update_zero_row_response_is_identical_for_foreign_tenant_and_missing_id() {
    // 他テナント（tenant-b）所有 id=100 への update と、未存在 id=999 への
    // update は、いずれも `updated:0`・`200` になり応答バイト列（ヘッダ集合・
    // 本文。`Date` を除く）が完全一致する（RLS-9。存在情報の非漏えい）。
    let (core_without, _guard_without) = new_core();
    let (both_without, _sql_without) = spawn_both(core_without);
    let resp_missing = query(&both_without, &update_body(999, "en", "n12-g1-missing"));
    assert_eq!(resp_missing.status, 200, "resp={resp_missing:?}");
    assert!(String::from_utf8_lossy(&resp_missing.body).contains(r#""updated":0"#));

    let (core_with, _guard_with) = new_core();
    seed_foreign_tenant(&core_with);
    let (both_with, _sql_with) = spawn_both(core_with);
    let resp_foreign = query(&both_with, &update_body(100, "en", "n12-g1-missing"));
    assert_eq!(resp_foreign.status, 200, "resp={resp_foreign:?}");
    assert!(String::from_utf8_lossy(&resp_foreign.body).contains(r#""updated":0"#));

    assert_eq!(strip_date(&resp_missing), strip_date(&resp_foreign));

    // 0 行応答後も台帳へは記録済み（同一 operation_id 再送は 23505）。
    let resp = query(&both_without, &update_body(999, "en", "n12-g1-missing"));
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

#[test]
fn delete_zero_row_response_is_identical_for_foreign_tenant_and_missing_id() {
    let (core_without, _guard_without) = new_core();
    let (both_without, _sql_without) = spawn_both(core_without);
    let resp_missing = query(&both_without, &delete_body(999, "n12-g2-missing"));
    assert_eq!(resp_missing.status, 200, "resp={resp_missing:?}");
    assert!(String::from_utf8_lossy(&resp_missing.body).contains(r#""deleted":0"#));

    let (core_with, _guard_with) = new_core();
    seed_foreign_tenant(&core_with);
    let (both_with, _sql_with) = spawn_both(core_with);
    let resp_foreign = query(&both_with, &delete_body(100, "n12-g2-missing"));
    assert_eq!(resp_foreign.status, 200, "resp={resp_foreign:?}");
    assert!(String::from_utf8_lossy(&resp_foreign.body).contains(r#""deleted":0"#));

    assert_eq!(strip_date(&resp_missing), strip_date(&resp_foreign));
}

// --- H: filter: []（空配列）は 42601 かつ副作用なし --------------------------

#[test]
fn update_and_delete_empty_filter_reject_with_42601_and_no_side_effect() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-seed-h"));

    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[]}"#,
        br#"{"op":"delete","table":"docs","filter":[]}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
    let rows = read_back_langs(&core);
    assert_eq!(rows, vec![(1, Some("ja".to_string()))]);
}

// --- H2: filter（述語形。非空。Issue #1062）は SQL 表層の述語形 DML と
//         同一の実行結果・台帳照合を共有する ---------------------------------

fn predicate_update_body(lang_match: &str, new_lang: &str, op_id: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"lang":"{new_lang}"}},"filter":[{{"column":"lang","op":"eq","value":"{lang_match}"}}],"operation_id":"{op_id}"}}"#
    )
    .into_bytes()
}

fn predicate_delete_body(lang_match: &str, op_id: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"delete","table":"docs","filter":[{{"column":"lang","op":"eq","value":"{lang_match}"}}],"operation_id":"{op_id}"}}"#
    )
    .into_bytes()
}

fn predicate_update_sql(lang_match: &str, new_lang: &str, op_id: &str) -> String {
    format!(
        "UPDATE docs SET lang = '{new_lang}' WHERE lang = '{lang_match}' USING OPERATION_ID '{op_id}'"
    )
}

fn predicate_delete_sql(lang_match: &str, op_id: &str) -> String {
    format!("DELETE FROM docs WHERE lang = '{lang_match}' USING OPERATION_ID '{op_id}'")
}

#[test]
fn predicate_update_success_updates_matching_rows() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-seed-1"));
    query(&both, &insert_body(2, "ja", "n12-pred-seed-2"));
    query(&both, &insert_body(3, "en", "n12-pred-seed-3"));

    let resp = query(
        &both,
        &predicate_update_body("ja", "fr", "n12-pred-update-1"),
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(body.contains(r#""updated":2"#), "{body}");

    let mut rows = read_back_langs(&core);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            (1, Some("fr".to_string())),
            (2, Some("fr".to_string())),
            (3, Some("en".to_string())),
        ]
    );
}

#[test]
fn predicate_delete_success_deletes_matching_rows() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-seed-4"));
    query(&both, &insert_body(2, "ja", "n12-pred-seed-5"));
    query(&both, &insert_body(3, "en", "n12-pred-seed-6"));

    let resp = query(&both, &predicate_delete_body("ja", "n12-pred-delete-1"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(body.contains(r#""deleted":2"#), "{body}");

    let rows = read_back_langs(&core);
    assert_eq!(rows, vec![(3, Some("en".to_string()))]);
}

/// SQL 表層の述語形 `UPDATE ... WHERE lang = '<match>'` と NoSQL 表層の
/// `filter:[{"column":"lang","op":"eq","value":"<match>"}]` は同一の
/// `content_hash` を生成するため、同一 `operation_id` への跨表層再送は
/// 内容一致（`23505`）として扱われる（RECOVER-10。第 2 の実行器を作らない
/// ことの直接の検証）。
#[test]
fn cross_surface_predicate_update_resend_with_same_content_is_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-cross-seed-1"));

    common::send_simple_query(
        &mut sql,
        &predicate_update_sql("ja", "en", "n12-pred-cross-1"),
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        &predicate_update_body("ja", "en", "n12-pred-cross-1"),
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

/// 述語形 `UPDATE` の `SET` に `VECTOR` 列を含む場合も、Issue #1061 の正準化
/// （`InsertLiteral::String` を `VECTOR` 列向けにタグ 5・f32 LE 列へ正規化する
/// 経路。`run_predicate_update` 内の `needs_legacy_vector_hash` 分岐）を
/// NoSQL 経路が引き継ぐため、SQL 表層のベクトルリテラル文字列と NoSQL 表層の
/// JSON 配列は同一 `content_hash` になる（`WHERE` 対象列は非 VECTOR 列
/// `lang`。`filter` の `eq` は `VECTOR` 列を対象にできないが、`SET` 側は
/// 対象にできる）。
#[test]
fn cross_surface_predicate_update_with_vector_set_resend_is_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-vec-seed"));

    common::send_simple_query(
        &mut sql,
        "UPDATE docs SET embedding = '[-0,0.5,0.6]' WHERE lang = 'ja' \
         USING OPERATION_ID 'n12-pred-vec-1'",
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"embedding":[-0.0,0.5,0.6]},"filter":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"n12-pred-vec-1"}"#,
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

/// `prefix`（`filter.rs::like_escape` 経由で `WherePredicate::Prefix` へ
/// 写像される。A03 インジェクション境界）が SQL 表層の `LIKE 'j%'` と同一の
/// `content_hash` を生成することを跨表層再送で固定する。
#[test]
fn cross_surface_predicate_delete_with_prefix_filter_resend_is_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-prefix-seed"));

    common::send_simple_query(
        &mut sql,
        "DELETE FROM docs WHERE lang LIKE 'j%' USING OPERATION_ID 'n12-pred-prefix-1'",
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "DELETE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        br#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"prefix","value":"j"}],"operation_id":"n12-pred-prefix-1"}"#,
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

/// 複数要素 `filter`（`AND` 結合。宣言順が `content_hash` の入力順になる）が
/// SQL 表層の複数 `WHERE ... AND ...` 述語と同一の `content_hash` を生成する
/// ことを跨表層再送で固定する。
#[test]
fn cross_surface_predicate_update_with_multiple_filter_elements_resend_is_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-multi-seed"));

    common::send_simple_query(
        &mut sql,
        "UPDATE docs SET lang = 'fr' WHERE lang = 'ja' AND lang LIKE 'j%' \
         USING OPERATION_ID 'n12-pred-multi-1'",
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"lang":"fr"},"filter":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"prefix","value":"j"}],"operation_id":"n12-pred-multi-1"}"#,
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

/// 同一の 2 要素 `filter` でも宣言順が異なれば別ハッシュとして扱われ、
/// 同一 `operation_id` は内容不一致（`22023`）になる（`content_hash` が
/// 宣言順に依存することの直接固定。`sql_predicate_dml_exec.rs::
/// predicate_delete_resend_with_reordered_predicates_is_content_mismatch`
/// の NoSQL 版）。
#[test]
fn predicate_update_resend_with_reordered_filter_elements_is_content_mismatch() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-reorder-seed"));

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"lang":"fr"},"filter":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"prefix","value":"j"}],"operation_id":"n12-pred-reorder-1"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"lang":"fr"},"filter":[{"column":"lang","op":"prefix","value":"j"},{"column":"lang","op":"eq","value":"ja"}],"operation_id":"n12-pred-reorder-1"}"#,
    );
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22023");
}

/// 同一 `operation_id` でも内容（`SET` の新値）が異なれば `22023`（内容不一致）
/// になる（RECOVER-10）。
#[test]
fn cross_surface_predicate_update_resend_with_different_content_is_content_mismatch() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-cross-seed-2"));

    common::send_simple_query(
        &mut sql,
        &predicate_update_sql("ja", "en", "n12-pred-cross-2"),
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        &predicate_update_body("ja", "fr", "n12-pred-cross-2"),
    );
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22023");
}

/// 述語形 `DELETE` も同じ台帳キー空間 `(tenant, table, operation_id)` を
/// 共有し、SQL→NoSQL の同一内容再送は `23505` になる。
#[test]
fn cross_surface_predicate_delete_resend_with_same_content_is_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-pred-cross-seed-3"));

    common::send_simple_query(&mut sql, &predicate_delete_sql("ja", "n12-pred-cross-3"));
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "DELETE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(&both, &predicate_delete_body("ja", "n12-pred-cross-3"));
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

/// RLS-9: tenant-b に述語一致行が「ある場合」と「ない場合」で、tenant-a の
/// 述語形 `update` の応答（ステータス・本文）がバイト単位で一致する
/// （他テナントの存在情報を漏らさない）。
#[test]
fn predicate_update_rls9_response_identity_regardless_of_foreign_tenant_match() {
    let (core_missing, _guard_missing) = new_core();
    let (both_missing, _sql_missing) = spawn_both(core_missing.clone());
    query(&both_missing, &insert_body(1, "ja", "n12-pred-rls9-seed-1"));
    let resp_missing = query(
        &both_missing,
        &predicate_update_body("ja", "en", "n12-pred-rls9-1"),
    );
    assert_eq!(resp_missing.status, 200, "resp={resp_missing:?}");

    let (core_foreign, _guard_foreign) = new_core();
    seed_foreign_tenant(&core_foreign);
    let (both_foreign, _sql_foreign) = spawn_both(core_foreign.clone());
    query(&both_foreign, &insert_body(1, "ja", "n12-pred-rls9-seed-1"));
    let resp_foreign = query(
        &both_foreign,
        &predicate_update_body("ja", "en", "n12-pred-rls9-1"),
    );
    assert_eq!(resp_foreign.status, 200, "resp={resp_foreign:?}");

    assert_eq!(strip_date(&resp_missing), strip_date(&resp_foreign));
}

// --- I: explain: true は未知キーとして 42601 --------------------------------

#[test]
fn update_and_delete_reject_explain_true_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);
    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"explain":true}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"explain":true}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

// --- J: 複数列 SET の宣言順は content_hash に影響しない（Issue #876
//        レビュー指摘の是正。docs/design/nosql-update-delete-mapping.md
//        「複数列 SET の宣言順と content_hash」節参照） -------------------

#[test]
fn cross_surface_multi_column_set_declared_out_of_alphabetical_order_is_treated_as_duplicate() {
    // `set` は JSON パース時点で `BTreeMap`（キーのアルファベット順）へ
    // 正規化されるため、SQL 表層が宣言順（例: `lang, embedding`）で書いた
    // 場合と NoSQL 表層（常にアルファベット順 `embedding, lang`）とで
    // `BoundUpdate::assignments` の順序が食い違いうる。`tenant::
    // update_row_columns_unchecked` が `content_hash::for_update_columns`
    // へ渡す前に列をスキーマ順へ正規化する（宣言順は SET 意味論に一切
    // 影響しない）ため、この食い違いは「内容不一致」（`22023`）へ誤判定
    // されず「同一内容の再送」（`23505`）として正しく扱われる。本テストは
    // SQL・NoSQL 表層を跨いだ同一 `operation_id` 再送の内容一致判定が列の
    // 記述順に依存しないことを固定する。
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-order-seed"));

    // SQL: 宣言順 `lang, embedding`（アルファベット順とは逆）。
    common::send_simple_query(
        &mut sql,
        "UPDATE docs SET lang = 'en', embedding = '[0.4,0.5,0.6]' WHERE id = 1 \
         USING OPERATION_ID 'n12-order-1'",
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    // NoSQL: JSON 内の記述順に関わらず `BTreeMap` により
    // `embedding, lang`（アルファベット順）へ正規化される。列の値・意味は
    // SQL 表層で送った内容と完全に同一のため、同一 `operation_id` の再送は
    // `23505`（重複）として拒否される。
    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"embedding":[0.4,0.5,0.6],"lang":"en"},"where":{"id":1},"operation_id":"n12-order-1"}"#,
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

// --- 単一行 UPDATE の VECTOR 値表層跨ぎ content_hash 統一（Issue #1061） ----
//     単一行形（`WHERE id = n`）は束縛後の `Value::Vector` を
//     `for_update_columns` でハッシュするため、SQL 表層のベクトルリテラル
//     文字列（`sql::parser::parse_vector_literal`）と NoSQL 表層の JSON 配列
//     （`JsonNumber::as_f32`）はいずれも `json::parse_f32_text` の単一丸めを
//     経由し、束縛前から同一表現になる。本節はそのことを本番ルータ経由
//     （wire 越し）で固定する。述語形（`filter`。`WHERE` 対象列は `lang` の
//     ような非 VECTOR 列だが、`SET` 側に VECTOR 列を含めることは単一行形と
//     同様に可能）の VECTOR SET 表層跨ぎは
//     `cross_surface_predicate_update_with_vector_set_resend_is_duplicate`
//     （#H2 節）で固定する。

/// SQL 表層で書いた VECTOR 値を、NoSQL 表層から同一 `operation_id` で
/// 再送すると `23505`（重複）になる。
#[test]
fn cross_surface_vector_value_sql_then_nosql_resend_is_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-vec-seed"));

    common::send_simple_query(
        &mut sql,
        "UPDATE docs SET embedding = '[-0,0.5,0.6]' WHERE id = 1 \
         USING OPERATION_ID 'n12-vec-1'",
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"embedding":[-0.0,0.5,0.6]},"where":{"id":1},"operation_id":"n12-vec-1"}"#,
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

/// SQL 表層の表記ゆれ（`'1.0'`／`'2.0'`／`'3.0'`）は NoSQL 表層の整数表記
/// （`[1,2,3]`）と同一値としてハッシュされ、同一 `operation_id` の再送は
/// `23505`（重複）になる。
#[test]
fn cross_surface_vector_value_spelling_variants_are_treated_as_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-vec-spelling-seed"));

    common::send_simple_query(
        &mut sql,
        "UPDATE docs SET embedding = '[1.0,2.0,3.0]' WHERE id = 1 \
         USING OPERATION_ID 'n12-vec-spelling-1'",
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"embedding":[1,2,3]},"where":{"id":1},"operation_id":"n12-vec-spelling-1"}"#,
    );
    assert_eq!(resp.status, 409, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

/// D2 の `-0.0` 保持契約: SQL 表層の `'[0,0.5,0.6]'`（`+0.0`）に対し、
/// NoSQL 表層から `-0.0` を含む値で同一 `operation_id` を再送すると
/// `22023`（内容不一致）になる（表層を跨いでも符号付きゼロは正規化しない）。
#[test]
fn cross_surface_vector_value_negative_zero_is_distinct_from_positive_zero() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-vec-negzero-seed"));

    common::send_simple_query(
        &mut sql,
        "UPDATE docs SET embedding = '[0,0.5,0.6]' WHERE id = 1 \
         USING OPERATION_ID 'n12-vec-negzero-1'",
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "UPDATE 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"embedding":[-0.0,0.5,0.6]},"where":{"id":1},"operation_id":"n12-vec-negzero-1"}"#,
    );
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22023");
}

/// 逆順（NoSQL 表層で書いた VECTOR 値を SQL 表層から再送）でも `23505`
/// になることを 1 ケース固定する。
#[test]
fn cross_surface_vector_value_nosql_then_sql_resend_is_duplicate() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);
    query(&both, &insert_body(1, "ja", "n12-vec-reverse-seed"));

    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"embedding":[0.4,0.5,0.6]},"where":{"id":1},"operation_id":"n12-vec-reverse-1"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");

    common::send_simple_query(
        &mut sql,
        "UPDATE docs SET embedding = '[0.4,0.5,0.6]' WHERE id = 1 \
         USING OPERATION_ID 'n12-vec-reverse-1'",
    );
    common::expect_error_response_with_sqlstate(&mut sql, "23505");
    common::read_ready_for_query(&mut sql);
}

// --- Issue #1197・NOSQL-14: 述語形 DML の `ne`／`like`／`is_null`／`not_null`／
//         `not` が SQL 表層と同一の実行結果・台帳照合（content_hash）を持つ ------

/// (NoSQL の filter 配列 JSON, 等価な SQL の WHERE 句)。いずれも `lang = 'ja'` の
/// 行だけに一致する（seed は ja・en の 2 行）。
const PREDICATE_DML_PARITY_CASES: [(&str, &str); 10] = [
    (
        r#"[{"column":"lang","op":"ne","value":"en"}]"#,
        "NOT lang = 'en'",
    ),
    (
        r#"[{"column":"lang","op":"like","value":"%a"}]"#,
        "lang LIKE '%a'",
    ),
    (
        r#"[{"column":"lang","op":"not_null"},{"column":"lang","op":"like","value":"j_"}]"#,
        "lang IS NOT NULL AND lang LIKE 'j_'",
    ),
    (
        r#"[{"not":{"column":"lang","op":"prefix","value":"e"}}]"#,
        "NOT lang LIKE 'e%'",
    ),
    (
        r#"[{"not":{"column":"lang","op":"eq","value":"en"}}]"#,
        "NOT lang = 'en'",
    ),
    // Issue #1356: 範囲比較・`in`・`or`・`not` で包んだ `or`／`in`。
    (r#"[{"column":"lang","op":"lt","value":"f"}]"#, "lang < 'f'"),
    (
        r#"[{"column":"lang","op":"in","value":["en","zz"]}]"#,
        "lang IN ('en', 'zz')",
    ),
    (
        r#"[{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"eq","value":"zz"}]}]"#,
        "lang = 'ja' OR lang = 'zz'",
    ),
    (
        r#"[{"not":{"or":[{"column":"lang","op":"eq","value":"en"},{"column":"lang","op":"eq","value":"zz"}]}}]"#,
        "NOT (lang = 'en' OR lang = 'zz')",
    ),
    (
        r#"[{"not":{"column":"lang","op":"in","value":["en"]}}]"#,
        "NOT lang IN ('en')",
    ),
];

#[test]
fn cross_surface_predicate_delete_new_operators_resend_is_duplicate() {
    for (i, (filter_json, where_clause)) in PREDICATE_DML_PARITY_CASES.iter().enumerate() {
        let (core, _guard) = new_core();
        let (both, mut sql) = spawn_both(core.clone());
        query(
            &both,
            &insert_body(1, "ja", &format!("n12-1197-seed-a-{i}")),
        );
        query(
            &both,
            &insert_body(2, "en", &format!("n12-1197-seed-b-{i}")),
        );

        let op_id = format!("n12-1197-del-{i}");
        common::send_simple_query(
            &mut sql,
            &format!("DELETE FROM docs WHERE {where_clause} USING OPERATION_ID '{op_id}'"),
        );
        let tag = common::read_command_complete(&mut sql);
        assert_eq!(tag, "DELETE 1", "{where_clause}");
        common::read_ready_for_query(&mut sql);

        let body = format!(
            r#"{{"op":"delete","table":"docs","filter":{filter_json},"operation_id":"{op_id}"}}"#
        );
        let resp = query(&both, body.as_bytes());
        assert_eq!(resp.status, 409, "{filter_json}: resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "23505", "{filter_json}");
    }
}

#[test]
fn predicate_update_and_delete_with_new_operators_apply_and_reject_forms() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-1197-own-1"));
    query(&both, &insert_body(2, "en", "n12-1197-own-2"));

    // `ne` の更新が自テナントの全一致行へ届く（他テナント境界は既存テストが担う）。
    let resp = query(
        &both,
        br#"{"op":"update","table":"docs","set":{"lang":"xx"},"filter":[{"column":"lang","op":"ne","value":"zzz"}],"operation_id":"n12-1197-upd-ne"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let langs = read_back_langs(&core);
    assert!(
        langs
            .iter()
            .any(|(id, l)| *id == 1 && l.as_deref() == Some("xx"))
            && langs
                .iter()
                .any(|(id, l)| *id == 2 && l.as_deref() == Some("xx")),
        "own rows must be updated: {langs:?}"
    );

    // Issue #1356: `or`・範囲比較は述語形 DML でも受理される（SQL の述語形 DML と同じ
    // 結果集合）。`not{or}` は自テナントの全行（lang = 'xx'）へ届く。
    let resp = query(
        &both,
        br#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"lt","value":"a"}],"operation_id":"n12-1197-del-lt"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        String::from_utf8_lossy(&resp.body).contains(r#""deleted":0"#),
        "{resp:?}"
    );
    let resp = query(
        &both,
        br#"{"op":"delete","table":"docs","filter":[{"not":{"or":[{"column":"lang","op":"eq","value":"a"},{"column":"lang","op":"eq","value":"b"}]}}],"operation_id":"n12-1197-del-notor"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        String::from_utf8_lossy(&resp.body).contains(r#""deleted":2"#),
        "{resp:?}"
    );
}
