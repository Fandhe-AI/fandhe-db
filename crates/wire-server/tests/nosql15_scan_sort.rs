//! `POST /v1/query`（`op: "scan"`）への `sort` 指定（Issue #946・NOSQL-15・
//! SQL-25 (a)・TASK-224）を production ルータ経由（生バイトクライアント）で
//! 検証する層 A 結合テスト。
//!
//! 実行意味論（並び順の決定性・NULL 位置・型ごとの並べ替え不能列の拒否）の
//! 確定オラクルは `crates/engine/tests/sql25_scalar_order_by.rs`・
//! `crates/engine/tests/bound_plan_public_api.rs`（`BoundScan::with_order_by`
//! の等価性）であり、本ファイルは同じ規則が NoSQL 表層（`sort[].dir` の
//! 語彙検査〔`super::scan::build_sort`〕→ `BoundScan::with_order_by`）越しに
//! 成立することと、SQL 表層のスカラー `ORDER BY` との応答本文パリティ
//! （行の順序込み）を wire フレーミング込みで確認する。
//! `nosql3_scan_mapping.rs`（`sort` 省略時の受理・拒否契約）とは役割分担し、
//! 本ファイルは `sort` 固有の契約に絞る。

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
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// wire ログインが導出する `PolicyContext` と同じ可視性（`Public` ＋
/// 自テナント `Private`。RLS-11・TASK-195・read-your-writes）。
fn wire_scoped_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// tenant-a に `docs(embedding VECTOR(2) NOT NULL, lang TEXT NULL)` の
/// Public 行 5 件（`lang` の重複・`NULL` を含み、同点解決〔id 昇順〕・
/// NULL 位置の両方を観測できるようにする）、tenant-b に `lang="a"`
/// （tenant-a の可視範囲の値と重なる）の Private 行 1 件を投入する
/// （他テナント行がソート順・境界・件数へ影響しないことの RLS 対照）。
fn new_core_scan_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql15-scan-sort-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    // id: 1="b", 2="a", 3=NULL, 4="a"（2 と同点）, 5="c"
    let rows: [(u64, Option<&str>); 5] = [
        (1, Some("b")),
        (2, Some("a")),
        (3, None),
        (4, Some("a")),
        (5, Some("c")),
    ];
    for (id, lang) in rows {
        let lang_value = lang
            .map(|s| Value::Text(s.to_string()))
            .unwrap_or(Value::Null);
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(vec![id as f32, 0.0]), lang_value],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql15-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-a row");
    }

    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_b,
        101,
        Visibility::Private,
        &[
            Value::Vector(vec![101.0, 0.0]),
            Value::Text("a".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("nosql15-op-101")
            .expect("valid operation_id"),
    )
    .expect("insert tenant-b private row");

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

fn post(addr: SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
    let auth_header = format!("Bearer {token}");
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_header),
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

fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

/// `sql` を `tenant` の wire スコープ ctx（Public + 自テナント Private）で
/// SQL テキスト経由実行し、`response::encode` を通した JSON 本文
/// （オラクル）を返す（`nosql3_scan_wire_parity.rs::sql_oracle_body` と同型）。
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

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

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

fn ids_from(columns: &[String], rows: &[Vec<JsonValue>]) -> Vec<u64> {
    let id_index = columns.iter().position(|c| c == "id").expect("id column");
    rows.iter()
        .map(|row| match &row[id_index] {
            JsonValue::Number(n) => n.as_f64() as u64,
            other => panic!("id cell must be a number, got {other:?}"),
        })
        .collect()
}

// --- 受理: 昇順・降順・NULL 位置・同点解決（id 昇順） ------------------------

#[test]
fn sort_ascending_orders_by_lang_with_null_last_and_id_tie_break() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 5);
    // lang: 2="a", 4="a"（同点→id 昇順）, 1="b", 5="c", 3=NULL（ASC は末尾）。
    assert_eq!(ids_from(&columns, &rows), vec![2, 4, 1, 5, 3]);
}

#[test]
fn sort_descending_orders_by_lang_with_null_first() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,
             "sort":[{"column":"lang","dir":"desc"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 5);
    // DESC は NULL が先頭。以降 c, b, a(2,4 同点→id 昇順)。
    assert_eq!(ids_from(&columns, &rows), vec![3, 5, 1, 2, 4]);
}

#[test]
fn sort_multiple_keys_apply_in_declared_order() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    // `lang DESC, id DESC`: 同点（"a"）のタイブレークを id 降順へ明示的に
    // 上書きする（既定の暗黙 id 昇順タイブレークと異なる結果になることで
    // 複数キーが宣言順どおり適用されることを固定する）。
    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,
             "sort":[{"column":"lang","dir":"desc"},{"column":"id","dir":"desc"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 5);
    assert_eq!(ids_from(&columns, &rows), vec![3, 5, 1, 4, 2]);
}

#[test]
fn sort_limit_truncates_to_prefix_of_sorted_order() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":3,
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 3);
    assert_eq!(ids_from(&columns, &rows), vec![2, 4, 1]);
}

#[test]
fn sort_by_column_not_in_projection_is_accepted() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"],
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(columns, vec!["id"]);
    assert_eq!(row_count, 5);
    assert_eq!(ids_from(&columns, &rows), vec![2, 4, 1, 5, 3]);
}

#[test]
fn sort_combined_with_filter_orders_the_filtered_subset() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,
             "filter":[{"column":"lang","op":"eq","value":"a"}],
             "sort":[{"column":"id","dir":"desc"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 2);
    assert_eq!(ids_from(&columns, &rows), vec![4, 2]);
}

/// 応答に `score` 列が含まれないこと（スカラー `ORDER BY` 経由でも合成
/// スコア列は構造上存在しない）。
#[test]
fn sort_response_never_includes_a_score_column() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    let (columns, _rows, _row_count) = parse_success_body(&resp);
    assert!(!columns.iter().any(|c| c == "score"), "columns={columns:?}");
}

// --- SQL 表層とのパリティ（順序込みバイト一致） ------------------------------

#[test]
fn sort_matches_sql_scalar_order_by_byte_for_byte() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core.clone());

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"],
             "sort":[{"column":"lang","dir":"asc"},{"column":"id","dir":"asc"}]}"#,
    );
    let sql_body = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id, lang FROM docs ORDER BY lang ASC, id ASC LIMIT 10",
    );
    assert_eq!(body_utf8(&resp), sql_body);
}

// --- 上限（8 要素は受理・9 要素は 54000） ------------------------------------

#[test]
fn sort_at_max_scalar_order_keys_is_accepted_and_over_limit_is_54000() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let items_8: String = (0..8)
        .map(|_| r#"{"column":"lang","dir":"asc"}"#)
        .collect::<Vec<_>>()
        .join(",");
    let ok_body = format!(r#"{{"op":"scan","table":"docs","limit":10,"sort":[{items_8}]}}"#);
    let ok_resp = query_as_alice(addr, ok_body.as_bytes());
    assert_eq!(ok_resp.status, 200, "body={ok_resp:?}");

    let items_9: String = (0..9)
        .map(|_| r#"{"column":"lang","dir":"asc"}"#)
        .collect::<Vec<_>>()
        .join(",");
    let over_body = format!(r#"{{"op":"scan","table":"docs","limit":10,"sort":[{items_9}]}}"#);
    let over_resp = query_as_alice(addr, over_body.as_bytes());
    // `54000`（payload_too_large）は HTTP 射影上 `413 Content Too Large`
    // （`nosql-api.md` の射影表どおり。`42601`／`22000` の `400` とは異なる）。
    assert_eq!(over_resp.status, 413, "body={over_resp:?}");
    assert_eq!(http_common::wire_code_of(&over_resp), "54000");
}

// --- 形の不正（42601） -------------------------------------------------------

#[test]
fn sort_shape_violations_reject_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let cases: [&[u8]; 9] = [
        br#"{"op":"scan","table":"docs","limit":10,"sort":[]}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":{}}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":[1]}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":null}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang","dir":"asc","extra":1}]}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":[{"dir":"asc"}]}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang"}]}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang","dir":"ASC"}]}"#,
        br#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang","dir":"up"}]}"#,
    ];
    for body in cases {
        let resp = query_as_alice(addr, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "body={body:?}");
    }
}

#[test]
fn sort_invalid_identifier_column_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,
             "sort":[{"column":"1bad","dir":"asc"}]}"#,
    );
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

// --- 意味エラー（22000）: 未知列・VECTOR 列 ----------------------------------

#[test]
fn sort_unknown_and_vector_columns_reject_with_22000() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    for column in ["does_not_exist", "embedding"] {
        let body = format!(
            r#"{{"op":"scan","table":"docs","limit":10,"sort":[{{"column":"{column}","dir":"asc"}}]}}"#
        );
        let resp = query_as_alice(addr, body.as_bytes());
        assert_eq!(resp.status, 400, "column={column} body={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "22000", "column={column}");
    }
}

// --- 排他: search／aggregate／explain との併用は 42601 -----------------------

#[test]
fn sort_on_search_op_rejects_with_42601_unknown_key() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","limit":5,"vector":[1.0,0.0],
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

#[test]
fn sort_on_aggregate_op_rejects_with_42601_unknown_key() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs",
             "aggregates":[{"fn":"count","column":"id"}],
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

#[test]
fn sort_combined_with_explain_true_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"explain":true,
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

// --- RLS: 他テナントの Private 行が並び順・境界・件数に影響しない ------------

#[test]
fn sort_never_leaks_other_tenant_private_rows_and_ordering_is_unaffected() {
    let (core, _guard) = new_core_scan_docs();
    let addr = spawn(core);

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,
             "sort":[{"column":"lang","dir":"asc"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    // tenant-b の Private 行（id=101・lang="a"）が可視範囲の値と重なって
    // いても、tenant-a の応答には一切現れない（件数・順序ともに不変）。
    assert_eq!(row_count, 5);
    assert_eq!(ids_from(&columns, &rows), vec![2, 4, 1, 5, 3]);
    let body_str = String::from_utf8_lossy(&resp.body);
    assert!(!body_str.contains("101"));
    assert!(!body_str.contains("tenant-b"));
}
