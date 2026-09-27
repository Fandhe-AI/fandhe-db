//! `POST /v1/query`（`op: scan`）の `offset`（Issue #947・NOSQL-15・
//! TASK-224）が SQL 表層の広域取得 `LIMIT n OFFSET m`（SQL-25 (b)）と同一の
//! 実行計画へ写像され、ページ分割・RLS 暗黙適用・エラー分類（`42601`／
//! `22000`）が契約どおりであることを検証する層 A 結合テスト。
//!
//! `nosql3_scan_wire_parity.rs`（`offset` 以外の scan 契約）・
//! `wire_scan_offset.rs`（pg wire 生バイト経路での `OFFSET`）と役割分担する。
//! 本ファイルは HTTP（NoSQL）表層越しの `offset` のみを対象にする。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::response::encode as encode_query_result;
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// wire ログインが導出する `PolicyContext` と同じ可視性（`Public` ＋
/// 自テナント `Private`。RLS-11・TASK-195・read-your-writes）を持つ ctx。
/// SQL オラクルは必ずこれを使う（`nosql3_scan_wire_parity.rs` と同方針）。
fn wire_scoped_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx (Public + own tenant Private, wire 既定)")
}

/// tenant-a に Public 行を id=1..=n として投入する（ページ分割検証用の
/// 十分な件数）。
fn new_core_with_tenant_a_rows(n: u64) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql15-offset-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for id in 1..=n {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql15-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// tenant-a に Public 行 `a_count` 件、tenant-b に Private 行 `b_count` 件を
/// id を交互（tenant-a: 奇数寄せ・tenant-b: 偶数寄せではなく単純に別レンジ）
/// に投入する RLS 検証用 fixture。tenant-a から見える件数は `a_count` の
/// みで、tenant-b の行数を変えても tenant-a のページ境界（空ページになる
/// `offset`）が変化しないことを確認する。
fn new_core_rls_fixture(a_count: u64, b_count: u64) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql15-offset-rls");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    for i in 0..a_count {
        let id = 1000 + i;
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql15-rls-a-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-a row");
    }

    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");
    for i in 0..b_count {
        let id = 2000 + i;
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("xx".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql15-rls-b-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-b row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core)
}

fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn post(addr: SocketAddr, auth: &str, body: &[u8]) -> HttpResponse {
    let auth_header = format!("Bearer {auth}");
    let content_length = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_header),
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn query_as(addr: SocketAddr, user: &str, password: &str, body: &[u8]) -> HttpResponse {
    let token = login(addr, user, password);
    post(addr, &token, body)
}

fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    query_as(addr, "alice", "pw-alice", body)
}

/// `sql` を `tenant` の wire スコープ ctx で SQL テキスト経由実行し、
/// `response::encode` を通した JSON 本文（オラクル）を返す。
fn sql_oracle_body(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let ctx = wire_scoped_ctx(tenant);
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect("oracle SQL should succeed");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected SqlOutcome::Query for {sql:?}");
    };
    encode_query_result(&result).expect("oracle result should encode")
}

/// 応答本文を `(columns, rows, row_count)` へ解析する
/// （`nosql3_scan_wire_parity.rs::parse_success_body` と同型）。
fn parse_success_body(resp: &HttpResponse) -> (Vec<String>, Vec<Vec<JsonValue>>, u64) {
    assert_eq!(
        resp.status,
        200,
        "expected 200, body={:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let text = std::str::from_utf8(&resp.body).expect("body must be utf-8");
    parse_body_str(text)
}

fn parse_body_str(text: &str) -> (Vec<String>, Vec<Vec<JsonValue>>, u64) {
    let JsonValue::Object(mut top) = parse_json(text).expect("body must be valid json") else {
        panic!("top level must be an object: {text}");
    };
    let columns = match top.remove("columns") {
        Some(JsonValue::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                JsonValue::Object(mut col) => match col.remove("name") {
                    Some(JsonValue::String(s)) => s,
                    other => panic!("column name must be a string, got {other:?}"),
                },
                other => panic!("column entry must be an object, got {other:?}"),
            })
            .collect(),
        other => panic!("columns must be an array, got {other:?}"),
    };
    let rows: Vec<Vec<JsonValue>> = match top.remove("rows") {
        Some(JsonValue::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                JsonValue::Array(cells) => cells,
                other => panic!("row must be an array, got {other:?}"),
            })
            .collect(),
        other => panic!("rows must be an array, got {other:?}"),
    };
    let row_count = match top.remove("row_count") {
        Some(JsonValue::Number(n)) => n.as_f64() as u64,
        other => panic!("row_count must be a number, got {other:?}"),
    };
    (columns, rows, row_count)
}

fn ids_of(columns: &[String], rows: &[Vec<JsonValue>]) -> Vec<u64> {
    let id_index = columns.iter().position(|c| c == "id").expect("id column");
    rows.iter()
        .map(|row| match &row[id_index] {
            JsonValue::Number(n) => n.as_f64() as u64,
            other => panic!("id cell must be a number, got {other:?}"),
        })
        .collect()
}

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

// --- (1) ページ分割の一貫性 -------------------------------------------------

/// `limit n` で `offset 0, n, 2n, …` と取ったページは互いに重複せず、
/// 全ページを連結すると `offset` 省略時の全件結果と順序含めて一致する。
#[test]
fn pages_partition_the_full_result_without_overlap() {
    let (core, _guard) = new_core_with_tenant_a_rows(7);
    let addr = spawn(Arc::clone(&core));

    let full_body = br#"{"op":"scan","table":"docs","limit":100,"columns":["id"]}"#;
    let full_resp = query_as_alice(addr, full_body);
    let (full_columns, full_rows, full_count) = parse_success_body(&full_resp);
    assert_eq!(full_count, 7);
    let full_ids = ids_of(&full_columns, &full_rows);

    let page_size = 3u64;
    let mut collected: Vec<u64> = Vec::new();
    let mut offset = 0u64;
    loop {
        let body = format!(
            r#"{{"op":"scan","table":"docs","limit":{page_size},"offset":{offset},"columns":["id"]}}"#
        );
        let resp = query_as_alice(addr, body.as_bytes());
        let (columns, rows, _row_count) = parse_success_body(&resp);
        if rows.is_empty() {
            break;
        }
        collected.extend(ids_of(&columns, &rows));
        offset += page_size;
        assert!(
            offset <= 20,
            "pagination did not terminate: offset={offset}"
        );
    }
    assert_eq!(collected, full_ids);
}

// --- (2) SQL とのパリティ ---------------------------------------------------

/// `scan` の `offset` 付き応答は、同一 core 上で SQL
/// `SELECT id, lang FROM docs LIMIT n OFFSET m` を実行したオラクルとバイト
/// 一致する。
#[test]
fn offset_response_matches_sql_oracle_byte_for_byte() {
    let (core, _guard) = new_core_with_tenant_a_rows(5);
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10,"offset":2,"columns":["id","lang"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let actual = body_utf8(&resp);

    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id, lang FROM docs LIMIT 10 OFFSET 2",
    );
    assert_eq!(actual, oracle);
}

// --- (3) no-op 等価 ----------------------------------------------------------

/// `"offset":0` と省略時の応答本文はバイト一致する。
#[test]
fn offset_zero_is_byte_identical_to_omitted_offset() {
    let (core, _guard) = new_core_with_tenant_a_rows(5);
    let addr = spawn(Arc::clone(&core));

    let with_zero =
        br#"{"op":"scan","table":"docs","limit":10,"offset":0,"columns":["id","lang"]}"#;
    let omitted = br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"]}"#;

    let resp_zero = query_as_alice(addr, with_zero);
    let resp_omitted = query_as_alice(addr, omitted);
    assert_eq!(resp_zero.status, 200);
    assert_eq!(resp_omitted.status, 200);
    assert_eq!(body_utf8(&resp_zero), body_utf8(&resp_omitted));
}

// --- (4) 範囲外の空ページ ----------------------------------------------------

/// 可視件数以上の `offset` は `200`・`rows=[]`・`row_count=0` になる。
#[test]
fn offset_beyond_visible_count_returns_empty_page() {
    let (core, _guard) = new_core_with_tenant_a_rows(3);
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10,"offset":10,"columns":["id"]}"#;
    let resp = query_as_alice(addr, body);
    let (_columns, rows, row_count) = parse_success_body(&resp);
    assert!(rows.is_empty());
    assert_eq!(row_count, 0);
}

/// `offset: 10000`（`MAX_SEARCH_K` 境界。範囲内の上限値）は空ページで
/// 正常応答する。
#[test]
fn offset_at_max_search_k_boundary_returns_empty_page() {
    let (core, _guard) = new_core_with_tenant_a_rows(3);
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10,"offset":10000,"columns":["id"]}"#;
    let resp = query_as_alice(addr, body);
    let (_columns, rows, row_count) = parse_success_body(&resp);
    assert!(rows.is_empty());
    assert_eq!(row_count, 0);
}

// --- (5) RLS ------------------------------------------------------------

/// tenant-a の各ページが tenant-a の可視行集合のスライスと一致し、空ページに
/// なる閾値（`offset` = 可視件数）が tenant-b の行数に依存しないことを、
/// tenant-b の行数だけ変えた 2 通りの fixture で比較する。
#[test]
fn rls_page_boundary_does_not_depend_on_other_tenant_row_count() {
    for b_count in [0u64, 50u64] {
        let (core, _guard) = new_core_rls_fixture(4, b_count);
        let addr = spawn(Arc::clone(&core));

        // tenant-a から可視な 4 件ちょうどで境界になる: offset=4 は空、
        // offset=3 は 1 件。
        let body_at_boundary =
            br#"{"op":"scan","table":"docs","limit":10,"offset":4,"columns":["id"]}"#;
        let resp_at_boundary = query_as_alice(addr, body_at_boundary);
        let (_c, rows_at_boundary, count_at_boundary) = parse_success_body(&resp_at_boundary);
        assert!(
            rows_at_boundary.is_empty(),
            "b_count={b_count} rows={rows_at_boundary:?}"
        );
        assert_eq!(count_at_boundary, 0, "b_count={b_count}");

        let body_before_boundary =
            br#"{"op":"scan","table":"docs","limit":10,"offset":3,"columns":["id"]}"#;
        let resp_before_boundary = query_as_alice(addr, body_before_boundary);
        let (_c2, rows_before_boundary, count_before_boundary) =
            parse_success_body(&resp_before_boundary);
        assert_eq!(rows_before_boundary.len(), 1, "b_count={b_count}");
        assert_eq!(count_before_boundary, 1, "b_count={b_count}");
    }
}

/// bob（tenant-b）が `offset` 付きで scan しても、tenant-a の Private 行
/// （bob からは不可視）の値・テナント ID・資格情報が応答本文に一切出現
/// しない（`nosql3_scan_wire_parity.rs` の非漏えい検査と同型。`offset` が
/// 不可視行を数え上げに使えないことの確認）。
#[test]
fn rls_offset_response_never_leaks_other_tenant_private_row_or_credentials() {
    let path = temp_db::unique_db_path("nosql15-offset-rls-leak");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    // tenant-a の Private 行（bob からは不可視。read-your-writes は alice の
    // みに適用される）。
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-a ctx");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
        1,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("secret-a".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("nosql15-leak-a-1")
            .expect("valid operation_id"),
    )
    .expect("insert tenant-a private row");

    // bob 自身の Public 行（offset のページングが機能する程度の件数）。
    let ctx_b = PolicyContext::with_visibilities("tenant-b", [Visibility::Public])
        .expect("valid tenant-b ctx");
    for i in 0..3u64 {
        let id = 100 + i;
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("pub-b".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql15-leak-b-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-b public row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addr = spawn(Arc::clone(&core));
    drop(guard);

    for offset in [0, 1, 2, 3, 10] {
        let body = format!(
            r#"{{"op":"scan","table":"docs","limit":10,"offset":{offset},"columns":["id","lang"]}}"#
        );
        let resp_bob = query_as(addr, "bob", "pw-bob", body.as_bytes());
        assert_eq!(resp_bob.status, 200, "offset={offset} resp={resp_bob:?}");
        let text_bob = body_utf8(&resp_bob);
        assert!(
            !text_bob.contains("secret-a"),
            "offset={offset} text={text_bob}"
        );
        assert!(!text_bob.contains("[1,"), "offset={offset} text={text_bob}");
        assert!(
            !text_bob.contains("tenant-a"),
            "offset={offset} text={text_bob}"
        );
        assert!(
            !text_bob.contains("pw-alice"),
            "offset={offset} text={text_bob}"
        );
    }
}

// --- (6) `filter` 併用 --------------------------------------------------

/// `offset` は `filter` 適用後の一致行に対して効く（SQL の
/// `WHERE … LIMIT n OFFSET m` とのパリティで確認する）。
#[test]
fn offset_applies_after_filter_matching_sql_oracle() {
    let path = temp_db::unique_db_path("nosql15-offset-filter");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for i in 0..10u64 {
        let id = 1 + i;
        let lang = if i % 2 == 0 { "ja" } else { "en" };
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql15-filter-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addr = spawn(Arc::clone(&core));
    drop(guard);

    let body = br#"{"op":"scan","table":"docs","limit":10,"offset":2,"columns":["id","lang"],
        "filter":[{"column":"lang","op":"eq","value":"ja"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let actual = body_utf8(&resp);

    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10 OFFSET 2",
    );
    assert_eq!(actual, oracle);
}

// --- (7) エラーコード ---------------------------------------------------

/// 形状不正の `offset`（文字列・真偽値・`null`・負数・小数・`u32::MAX` 超過・
/// 配列）は `42601` になる。
#[test]
fn malformed_offset_values_are_rejected_with_42601() {
    let (core, _guard) = new_core_with_tenant_a_rows(1);
    let addr = spawn(Arc::clone(&core));

    let bodies: [&[u8]; 7] = [
        br#"{"op":"scan","table":"docs","limit":10,"offset":"5"}"#,
        br#"{"op":"scan","table":"docs","limit":10,"offset":true}"#,
        br#"{"op":"scan","table":"docs","limit":10,"offset":null}"#,
        br#"{"op":"scan","table":"docs","limit":10,"offset":-1}"#,
        br#"{"op":"scan","table":"docs","limit":10,"offset":1.5}"#,
        br#"{"op":"scan","table":"docs","limit":10,"offset":4294967296}"#,
        br#"{"op":"scan","table":"docs","limit":10,"offset":[1]}"#,
    ];
    for body in bodies {
        let resp = query_as_alice(addr, body);
        http_common::assert_rejected(&resp, 400, "42601");
    }
}

/// 範囲外（`MAX_SEARCH_K` 超過）の `offset` は `22000` になる
/// （形状は正しいが値域外。SQL 表層・`wire_scan_offset.rs` と同じ 2 段構え）。
#[test]
fn out_of_range_offset_values_are_rejected_with_22000() {
    let (core, _guard) = new_core_with_tenant_a_rows(1);
    let addr = spawn(Arc::clone(&core));

    for offset in ["10001", "4294967295"] {
        let body = format!(r#"{{"op":"scan","table":"docs","limit":10,"offset":{offset}}}"#);
        let resp = query_as_alice(addr, body.as_bytes());
        http_common::assert_rejected(&resp, 400, "22000");
    }
}

/// `offset` と `explain: true` の同時指定は `42601`
/// （検証順は `explain` → `table` → `limit` → `offset` → `columns` に固定。
/// `explain` 拒否が最初に確定する）。
#[test]
fn offset_with_explain_true_is_rejected_with_42601() {
    let (core, _guard) = new_core_with_tenant_a_rows(1);
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10,"offset":1,"explain":true}"#;
    let resp = query_as_alice(addr, body);
    http_common::assert_rejected(&resp, 400, "42601");
}

/// `search`／`aggregate` op への `offset` 付与は未知キーとして `42601` になる
/// （`schema.rs::offset_is_unknown_key_for_search_and_aggregate` の HTTP 越し
/// 再確認）。
#[test]
fn offset_on_search_and_aggregate_ops_is_rejected_with_42601() {
    let (core, _guard) = new_core_with_tenant_a_rows(1);
    let addr = spawn(Arc::clone(&core));

    let search_body = br#"{"op":"search","table":"docs","limit":10,"vector":[0.1,0.2],"offset":1}"#;
    let resp_search = query_as_alice(addr, search_body);
    http_common::assert_rejected(&resp_search, 400, "42601");

    let aggregate_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"offset":1}"#;
    let resp_aggregate = query_as_alice(addr, aggregate_body);
    http_common::assert_rejected(&resp_aggregate, 400, "42601");
}

/// 形状不正の応答文言に入力値がそのまま含まれない。
#[test]
fn invalid_offset_message_does_not_echo_input_value() {
    let (core, _guard) = new_core_with_tenant_a_rows(1);
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10,"offset":-999999}"#;
    let resp = query_as_alice(addr, body);
    http_common::assert_message_does_not_echo(&resp, "999999");
}

/// テーブル不在（`42P01`）と `offset` 不正を同時に送った場合、検証順
/// （`table` の識別子検査は形状のみ・スキーマ解決前。`offset` はスキーマ
/// 非依存の検証としてテーブル解決より前に確定する）により `offset` 側が
/// 先に `42601` として確定する。
#[test]
fn invalid_offset_is_detected_before_table_not_found() {
    let (core, _guard) = new_core_with_tenant_a_rows(1);
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"missing_table","limit":10,"offset":-1}"#;
    let resp = query_as_alice(addr, body);
    http_common::assert_rejected(&resp, 400, "42601");
}
