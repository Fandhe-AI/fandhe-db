//! `POST /v1/query`（`op: "insert"`）の成功応答（`{"inserted","operation_id"}`）
//! と、行 `id` のテナント内スコープ契約（TABLE-12・RLS-9）を production ルータ
//! 経由（生バイトクライアント）で検証する層 A 結合テスト（Issue #772・
//! TASK-178・対象ビヘイビア NOSQL-6・TABLE-12・RLS-9。ポインタ:
//! `docs/spec/05-tasks.md` TASK-178・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-6・`docs/spec/04-behavior/data-model.md` TABLE-12・
//! `docs/spec/04-behavior/rls.md` RLS-9）。
//!
//! 意味論の確定オラクルは `crates/engine/tests/row_id_tenant_scope.rs`
//! （`engine::tenant` 直呼び出し）・`crates/wire-server/tests/
//! wire_tenant_row_id_scope.rs`（SQL wire 版）であり、本ファイルは同じ規則
//! が NoSQL 表層（`op` 許可リスト → スキーマ検証 → `insert::bind_rows`／
//! `execute` → `EngineCore::execute_bound_insert_in_session`）越しに成立
//! することを wire フレーミング込みで再確認する（`nosql3_scan_mapping.rs`
//! と対になる構成）。

use crate::common;
use crate::http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
    )
}

/// `docs(embedding VECTOR(3))` を持つ `EngineCore` を新設する
/// （`wire_tenant_row_id_scope.rs::new_core_with_docs_table` と同型）。
fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql6-tenant-row-id-scope");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `tenant-b`（id=100）・`tenant-c`（id=200）へ、HTTP を経由せず
/// `EngineCore::insert_row` で直接 1 行ずつ seed する（`wire_tenant_row_id_scope.rs::
/// seed_foreign_tenants` と同じ「wire を経由しない直接 API 呼び出し」の流儀。
/// 3 テナント構成にする理由も同一——「自テナント（alice/tenant-a）から見て
/// 他テナントが `id` を保持しているか否か」だけを変数にするため）。
fn seed_foreign_tenants(core: &EngineCore) {
    let b = PolicyContext::new(TENANT_B).expect("valid tenant");
    let c = PolicyContext::new(TENANT_C).expect("valid tenant");
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
    core.insert_row(
        &c,
        TABLE,
        200,
        &RowInput {
            tenant_id: TENANT_C,
            visibility: Visibility::Public,
            embedding: &[0.0, 0.0, 1.0],
            metadata: b"seed-c",
        },
        Some(&OperationId::parse("seed-op-tenant-c").expect("valid operation_id")),
    )
    .expect("seed tenant-c row id=200");
}

/// テスト起動: seed 済み `core` で production ルータを起動し、`alice`
/// （tenant-a）でログイン済みのトークンとアドレスを返す
/// （`nosql3_scan_mapping.rs::spawn_alice_session` と同型）。
fn spawn_alice_session(core: Arc<EngineCore>) -> (std::net::SocketAddr, String) {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr =
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
        addr,
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
    (addr, token)
}

/// `alice` のトークンで `/v1/query` へ `body` を送る便宜 API
/// （`nosql3_scan_mapping.rs::query` と同型）。
fn query(addr: std::net::SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn insert_body(id: u64, op_id: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2,0.3]}}],"operation_id":"{op_id}"}}"#
    )
    .into_bytes()
}

/// 成功応答（`200`）の本文を `(inserted, operation_id)` へ解析する。
/// トップレベルがちょうど 2 キーであることも固定する（[`InsertSuccess`] の
/// 出力形が `{"inserted","operation_id"}` の 2 フィールドのみであること。
/// `wire_server::http::query::insert::InsertSuccess` 参照）。
fn parse_insert_success_body(resp: &HttpResponse) -> (u64, String) {
    assert_eq!(
        resp.status,
        200,
        "expected 200, body={:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let text = std::str::from_utf8(&resp.body).expect("body must be utf-8");
    let JsonValue::Object(mut top) = parse_json(text).expect("body must be valid json") else {
        panic!("top level must be an object: {text}");
    };
    let inserted = match top.remove("inserted") {
        Some(JsonValue::Number(n)) => n.as_f64() as u64,
        other => panic!("inserted must be a number, got {other:?}"),
    };
    let operation_id = match top.remove("operation_id") {
        Some(JsonValue::String(s)) => s,
        other => panic!("operation_id must be a string, got {other:?}"),
    };
    assert!(
        top.is_empty(),
        "success body must contain exactly {{inserted, operation_id}}, extra keys: {top:?}"
    );
    (inserted, operation_id)
}

// --- 成功応答の形（NOSQL-6） -------------------------------------------------

#[test]
fn insert_success_body_has_exact_shape_and_is_persisted() {
    let (core, _guard) = new_core_with_docs_table();
    let (addr, token) = spawn_alice_session(core.clone());

    let resp = query(addr, &token, &insert_body(1, "nosql-op-shape"));
    let text = std::str::from_utf8(&resp.body).expect("utf-8").to_string();
    assert_eq!(
        text, r#"{"inserted":1,"operation_id":"nosql-op-shape"}"#,
        "unexpected success body: {text}"
    );
    let (inserted, operation_id) = parse_insert_success_body(&resp);
    assert_eq!(inserted, 1);
    assert_eq!(operation_id, "nosql-op-shape");

    // 非 vacuous 化: 保持している `Arc<EngineCore>` から直接 `SELECT`
    // （SQL-15 広域取得）で実際に永続化されたことを確認する（RLS-11・
    // TASK-195 導入後は production の SQL wire セッションからも同一テナント
    // であれば読み戻せるが、その経路自体の検証は
    // `crates/wire-server/tests/rls11_read_your_writes.rs` に委ねる）。
    // 200 応答が「ハンドラが走った」以上の証跡になるようにする。
    let ctx = PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant-a ctx");
    let mut session = engine::sql::mode::SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT id FROM docs LIMIT 10")
        .expect("select ok");
    let engine::sql::SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    assert!(
        result.rows.iter().any(|row| row.id == 1),
        "inserted row id=1 must be persisted and visible to tenant-a: {:?}",
        result.rows
    );
}

#[test]
fn insert_success_body_reports_multi_row_batch_count() {
    let (core, _guard) = new_core_with_docs_table();
    let (addr, token) = spawn_alice_session(core);

    let body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]},{"id":2,"embedding":[0.4,0.5,0.6]}],"operation_id":"nosql-op-multi"}"#;
    let resp = query(addr, &token, body);
    let (inserted, operation_id) = parse_insert_success_body(&resp);
    assert_eq!(inserted, 2);
    assert_eq!(operation_id, "nosql-op-multi");
}

// --- RLS-9: 応答バイト列の同一性 ---------------------------------------------

/// `Date` ヘッダと `operation_id` を固定長マスクへ置換した応答バイト列。
/// 2 腕の `operation_id` は同一バイト長で送る前提（呼び出し元が保証する）。
fn masked_response_bytes(resp: &HttpResponse, raw: &[u8], operation_id: &str) -> Vec<u8> {
    let masked_op = "X".repeat(operation_id.len());
    let text = std::str::from_utf8(raw).expect("response must be utf-8");
    let text = text.replace(operation_id, &masked_op);
    let date_value = resp
        .header("date")
        .expect("response must carry a Date header")
        .to_string();
    text.replace(&date_value, &"X".repeat(date_value.len()))
        .into_bytes()
}

/// 対象ビヘイビア: RLS-9。他テナント（tenant-b）が保持する `id=100` への
/// 自テナント名義 `insert` と、どのテナントも保持しない `id=777` への
/// `insert` とで、応答（ステータス・ヘッダ集合・本文。`operation_id` と
/// `Date` を除く）が完全に一致すること。物理キーが `(tenant_id, id)` で
/// 名前空間化されているため、他テナントの行有無を参照する分岐が構造的に
/// 存在しないことをフレーミング越しに固定する（`wire_tenant_row_id_scope.rs::
/// rls9_wire_insert_response_bytes_are_identical_for_foreign_held_id_and_absent_id`
/// の NoSQL 版）。
#[test]
fn rls9_insert_response_bytes_are_identical_for_foreign_held_id_and_absent_id() {
    let (core, _guard) = new_core_with_docs_table();
    seed_foreign_tenants(&core);
    let (addr, token) = spawn_alice_session(core);

    // 2 腕の operation_id は同一バイト長にする（masked_response_bytes の
    // 前提）。
    const OP_FOREIGN: &str = "nosql-op-foreign-b";
    const OP_ABSENT: &str = "nosql-op-absent-00";
    assert_eq!(OP_FOREIGN.len(), OP_ABSENT.len());

    // (b) 他テナント（tenant-b）が保持する id=100 への自テナント名義 insert。
    let resp_foreign = query(addr, &token, &insert_body(100, OP_FOREIGN));
    assert_eq!(resp_foreign.status, 200, "body={resp_foreign:?}");
    let (inserted_foreign, op_foreign) = parse_insert_success_body(&resp_foreign);
    assert_eq!(inserted_foreign, 1);
    assert_eq!(op_foreign, OP_FOREIGN);

    // (c) どのテナントも保持しない id=777 への insert。
    let resp_absent = query(addr, &token, &insert_body(777, OP_ABSENT));
    assert_eq!(resp_absent.status, 200, "body={resp_absent:?}");
    let (inserted_absent, op_absent) = parse_insert_success_body(&resp_absent);
    assert_eq!(inserted_absent, 1);
    assert_eq!(op_absent, OP_ABSENT);

    // ステータス・ヘッダ集合の個別一致。
    assert_eq!(resp_foreign.status, resp_absent.status);
    let mut headers_foreign: Vec<(String, String)> = resp_foreign
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("date"))
        .cloned()
        .collect();
    let mut headers_absent: Vec<(String, String)> = resp_absent
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("date"))
        .cloned()
        .collect();
    headers_foreign.sort();
    headers_absent.sort();
    assert_eq!(
        headers_foreign, headers_absent,
        "response headers (excluding Date) must match"
    );

    // 本文（`operation_id` をマスクした後）の完全一致。
    let masked_foreign = masked_response_bytes(&resp_foreign, &resp_foreign.body, OP_FOREIGN);
    let masked_absent = masked_response_bytes(&resp_absent, &resp_absent.body, OP_ABSENT);
    assert_eq!(
        masked_foreign, masked_absent,
        "response body bytes (operation_id masked) must be indistinguishable regardless of \
         whether another tenant holds the id"
    );
}

// --- TABLE-12: 同一テナント内重複 --------------------------------------------

/// 対象ビヘイビア: TABLE-12。他テナント（tenant-b・tenant-c）を事前に seed
/// した状態でも、同一テナント（tenant-a）内での重複 `id` への `insert` は
/// `23505` で拒否され、応答本文に他テナント名・行 `id`（重複対象自身の
/// `id`＝42 を含む）を含む識別子が漏えいしないこと（`wire_tenant_row_id_scope.rs::
/// table12_wire_insert_duplicate_within_own_tenant_is_rejected_with_23505_
/// without_leaking_row_identifiers` の NoSQL 版）。重複対象の id は
/// `100`/`200`（他テナント seed 行の id）と数字列として衝突しない `42` を使う。
///
/// 同一 id・別 `operation_id`（台帳照合と行キー衝突を混同しないための注意点
/// は `wire_insert_operation_id.rs`・`row_id_tenant_scope.rs` と同じ）。
#[test]
fn table12_duplicate_within_own_tenant_is_rejected_with_23505_without_leaking_row_identifiers() {
    let (core, _guard) = new_core_with_docs_table();
    seed_foreign_tenants(&core);
    let (addr, token) = spawn_alice_session(core);

    let first = query(addr, &token, &insert_body(42, "nosql-op-dup-first"));
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 1);

    // 同一 id・別 operation_id での再送。
    let second = query(addr, &token, &insert_body(42, "nosql-op-dup-second"));
    assert_eq!(second.status, 409, "body={second:?}");
    assert_eq!(http_common::wire_code_of(&second), "23505");

    let body_str = String::from_utf8_lossy(&second.body);
    assert!(
        !body_str.contains(TENANT_A)
            && !body_str.contains(TENANT_B)
            && !body_str.contains(TENANT_C),
        "error response must not leak a tenant identifier: {body_str:?}"
    );
    assert!(
        !body_str.contains("100") && !body_str.contains("200"),
        "error response must not leak another tenant's row id: {body_str:?}"
    );
    assert!(
        !body_str.contains("42"),
        "error response must not leak the duplicate row's own id: {body_str:?}"
    );

    // 接続が維持されていることを確認する（後続の正規クエリが通ること。
    // 1 要求 1 接続の接続モデルのため、サーバー自体が継続稼働していることの
    // 確認）。
    let after = query(addr, &token, &insert_body(2, "nosql-op-after-dup"));
    let (inserted_after, _) = parse_insert_success_body(&after);
    assert_eq!(inserted_after, 1);
}
