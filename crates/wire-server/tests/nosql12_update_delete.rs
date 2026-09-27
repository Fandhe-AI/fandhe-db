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
//!   `nosql1_op_vocabulary.rs`: `filter`（述語形）が `0A000` のまま留まる
//!   こと・`where` 形が実行結線済みで基本的な成功／`23502` 経路を固定済み。
//!   本ファイルはそれらと重複せず、台帳照合（`23505`／`22023`）・
//!   SQL↔NoSQL パリティ・RLS-9 秘匿に集中する。
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

// --- H: filter（述語形）は 0A000 かつ副作用なし -----------------------------

#[test]
fn update_and_delete_filter_only_reject_with_0a000_and_no_side_effect() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core.clone());
    query(&both, &insert_body(1, "ja", "n12-seed-h"));

    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[]}"#,
        br#"{"op":"delete","table":"docs","filter":[]}"#,
    ];
    for body in bodies {
        let resp = query(&both, body);
        assert_eq!(resp.status, 501, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "0A000");
    }
    let rows = read_back_langs(&core);
    assert_eq!(rows, vec![(1, Some("ja".to_string()))]);
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
//     （wire 越し）で固定する（述語形〔`filter`〕はまだ `0A000` で未結線の
//     ため対象外。述語形の表現統一自体は `recovery::content_hash` の単体
//     テストで固定済み）。

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
