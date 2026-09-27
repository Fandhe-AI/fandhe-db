//! `POST /v1/query`（`op: "scan"`）の写像・実行を production ルータ経由
//! （生バイトクライアント）で検証する層 A 結合テスト（Issue #766・
//! TASK-176・対象ビヘイビア NOSQL-3。ポインタ: `docs/spec/05-tasks.md`
//! TASK-176・`docs/spec/04-behavior/nosql-surface.md` NOSQL-3・
//! `docs/spec/04-behavior/sql-surface.md` SQL-15）。
//!
//! 実行意味論そのもの（早期終了・投影規則・`LIMIT` 範囲・第 2 の実行器を
//! 作らないこと）の確定オラクルは `crates/engine/tests/sql_scan_public_api.rs`・
//! `crates/engine/tests/bound_plan_public_api.rs`（in-process）であり、本
//! ファイルは同じ規則が NoSQL 表層（`op` 許可リスト → スキーマ検証 →
//! `scan::bind_request`／`execute` → `EngineCore::
//! execute_bound_scan_in_session`）越しに成立することを wire フレーミング
//! 込みで再確認する（`wire_scan.rs` の SQL wire 版と対になる）。
//!
//! `wire_scan.rs` の seed に対する pg wire ↔ NoSQL 2 表層の行集合パリティ
//! （順序保証なし・早期終了・RLS 暗黙適用）は `nosql3_scan_wire_parity.rs`
//! （Issue #767）が別途固定する。本ファイルはそれを再検証しない。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::collections::BTreeSet;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

/// `docs(embedding VECTOR(2), lang TEXT)` を持つ `EngineCore` を新設する。
/// tenant-a に `lang="ja"` の Public 行 3 件・`lang="en"` の Public 行 2 件、
/// tenant-b に `lang="xx"` の Private 行 3 件（id 101..=103）を投入する
/// （`sql_scan_public_api.rs::seed_two_tenants` と同型のテナント境界
/// フィクスチャ）。
fn new_core_scan_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql3-scan-mapping");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ja_rows: [(u64, [f32; 2]); 3] = [(1, [1.0, 0.0]), (2, [0.0, 1.0]), (3, [-1.0, 0.0])];
    for (id, emb) in ja_rows {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text("ja".to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql3-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-a ja row");
    }
    let en_rows: [(u64, [f32; 2]); 2] = [(4, [1.0, 1.0]), (5, [-1.0, -1.0])];
    for (id, emb) in en_rows {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text("en".to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql3-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-a en row");
    }

    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");
    for id in 101..=103u64 {
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
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql3-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-b row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// テスト起動: seed 済み `core` で production ルータを起動し、`alice`
/// （tenant-a）でログイン済みのトークンとアドレスを返す。
fn spawn_alice_session(core: Arc<EngineCore>) -> (std::net::SocketAddr, String) {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
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

/// `alice` のトークンで `/v1/query` へ `body` を送る便宜 API。
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

/// 成功応答（`200`）の本文を `(columns, rows, row_count)` へ解析する。
fn parse_success_body(resp: &HttpResponse) -> (Vec<String>, Vec<Vec<JsonValue>>, u64) {
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

// --- (1)〜(5) vector／plan／mode／hybrid 付与・limit 欠落は 42601 ----------

#[test]
fn limit_missing_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);
    let resp = query(addr, &token, br#"{"op":"scan","table":"docs"}"#);
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

#[test]
fn vector_plan_mode_hybrid_fields_reject_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let cases: [&[u8]; 4] = [
        br#"{"op":"scan","table":"docs","limit":1,"vector":[1.0,0.0]}"#,
        br#"{"op":"scan","table":"docs","limit":1,"plan":"find docs"}"#,
        br#"{"op":"scan","table":"docs","limit":1,"mode":"precision"}"#,
        br#"{"op":"scan","table":"docs","limit":1,"hybrid":{"text":"x"}}"#,
    ];
    for body in cases {
        let resp = query(addr, &token, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "body={body:?}");
    }
}

// --- (6) explain ------------------------------------------------------------

/// Issue #948（NOSQL-16・SQL-27）で `scan` op への `explain: true` を受理
/// するよう拡大した（旧来は `42601` で拒否）。`explain: true` は `QUERY
/// PLAN` 応答（行データを含まない）を返し、`explain: false` は通常どおり
/// 実行される（網羅的なカバレッジ・行一致は `nosql16_explain_targets.rs`・
/// `nosql10_explain.rs` が持つ）。
#[test]
fn explain_true_returns_query_plan_and_false_executes() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let true_resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":1,"explain":true}"#,
    );
    assert_eq!(true_resp.status, 200, "body={true_resp:?}");
    let text = String::from_utf8_lossy(&true_resp.body);
    assert!(text.contains("\"explain\""), "body={true_resp:?}");
    assert!(!text.contains("\"rows\""), "body={true_resp:?}");

    let false_resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":1,"explain":false}"#,
    );
    assert_eq!(false_resp.status, 200, "body={false_resp:?}");
}

// --- (7) 正常形（columns 省略） ----------------------------------------------

#[test]
fn default_projection_omits_score_and_respects_limit() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let resp = query(addr, &token, br#"{"op":"scan","table":"docs","limit":100}"#);
    let (columns, rows, row_count) = parse_success_body(&resp);
    // `columns` 省略時の既定投影（`Projection::All`）は `id` 疑似列 + スキーマ
    // 宣言順の実列（`embedding`・`lang`）をすべて列挙する（`bind_projection`
    // の既存契約）。`score` に相当する合成列は構造上存在しない。
    assert_eq!(columns, vec!["id", "embedding", "lang"]);
    assert!(!columns.iter().any(|c| c == "score"), "columns={columns:?}");
    assert_eq!(row_count, rows.len() as u64);
    assert!(
        row_count <= 5,
        "tenant-a has only 5 visible rows: {row_count}"
    );
}

// --- (8) columns 指定 ---------------------------------------------------------

#[test]
fn explicit_columns_selection_and_validation() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let ok_resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"]}"#,
    );
    let (columns, _rows, _row_count) = parse_success_body(&ok_resp);
    assert_eq!(columns, vec!["id", "lang"]);

    let score_resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["score"]}"#,
    );
    assert_eq!(score_resp.status, 400, "body={score_resp:?}");
    assert_eq!(http_common::wire_code_of(&score_resp), "22000");

    let empty_resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":[]}"#,
    );
    assert_eq!(empty_resp.status, 400, "body={empty_resp:?}");
    assert_eq!(http_common::wire_code_of(&empty_resp), "42601");

    let blank_resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":[""]}"#,
    );
    assert_eq!(blank_resp.status, 400, "body={blank_resp:?}");
    assert_eq!(http_common::wire_code_of(&blank_resp), "42601");
}

// --- (9) filter 併用 ----------------------------------------------------------

#[test]
fn filter_matches_sql_where_equivalent_id_set() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":100,"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
    );
    let (columns, rows, row_count) = parse_success_body(&resp);
    let id_index = columns.iter().position(|c| c == "id").expect("id column");
    let lang_index = columns
        .iter()
        .position(|c| c == "lang")
        .expect("lang column");
    assert_eq!(row_count, rows.len() as u64);

    let mut ids = BTreeSet::new();
    for row in &rows {
        match &row[lang_index] {
            JsonValue::String(s) => assert_eq!(s, "ja", "row={row:?}"),
            other => panic!("lang cell must be a string, got {other:?}"),
        }
        match &row[id_index] {
            JsonValue::Number(n) => {
                ids.insert(n.as_f64() as u64);
            }
            other => panic!("id cell must be a number, got {other:?}"),
        }
    }
    // tenant-a の `lang="ja"` 公開行は id 1..=3（seed 順）のみ。
    assert_eq!(ids, BTreeSet::from([1, 2, 3]));
}

// --- (10) limit 境界 -----------------------------------------------------------

#[test]
fn limit_boundaries_are_enforced() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let zero_resp = query(addr, &token, br#"{"op":"scan","table":"docs","limit":0}"#);
    assert_eq!(zero_resp.status, 400, "body={zero_resp:?}");
    assert_eq!(http_common::wire_code_of(&zero_resp), "22000");

    let too_large_resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10001}"#,
    );
    assert_eq!(too_large_resp.status, 400, "body={too_large_resp:?}");
    assert_eq!(http_common::wire_code_of(&too_large_resp), "22000");

    let fractional_resp = query(addr, &token, br#"{"op":"scan","table":"docs","limit":1.5}"#);
    assert_eq!(fractional_resp.status, 400, "body={fractional_resp:?}");
    assert_eq!(http_common::wire_code_of(&fractional_resp), "42601");

    let negative_resp = query(addr, &token, br#"{"op":"scan","table":"docs","limit":-1}"#);
    assert_eq!(negative_resp.status, 400, "body={negative_resp:?}");
    assert_eq!(http_common::wire_code_of(&negative_resp), "42601");

    let one_resp = query(addr, &token, br#"{"op":"scan","table":"docs","limit":1}"#);
    let (_columns, rows, row_count) = parse_success_body(&one_resp);
    assert_eq!(row_count, rows.len() as u64);
    assert!(row_count <= 1, "row_count={row_count}");
}

// --- (11) RLS 非漏えい ----------------------------------------------------------

#[test]
fn tenant_b_private_rows_never_appear_for_tenant_a() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let resp = query(addr, &token, br#"{"op":"scan","table":"docs","limit":100}"#);
    let (columns, rows, _row_count) = parse_success_body(&resp);
    let id_index = columns.iter().position(|c| c == "id").expect("id column");
    for row in &rows {
        match &row[id_index] {
            JsonValue::Number(n) => {
                let n = n.as_f64();
                assert!(!(101.0..=103.0).contains(&n), "tenant-b row id leaked: {n}")
            }
            other => panic!("id cell must be a number, got {other:?}"),
        }
    }
    let body_str = String::from_utf8_lossy(&resp.body);
    assert!(!body_str.contains("tenant-b"));
    assert!(!body_str.contains("\"xx\""));
}

// --- (12) 存在しないテーブル ------------------------------------------------

#[test]
fn undefined_table_rejects_with_42p01() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let resp = query(
        addr,
        &token,
        br#"{"op":"scan","table":"does_not_exist","limit":1}"#,
    );
    assert_eq!(resp.status, 404, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42P01");
}

// --- (12b) `table`／`columns` の識別子形状検査（cursor[bot] 指摘） ----------

/// `table` が SQL レキサーの識別子形状（先頭 ASCII 英字／`_`、以降 ASCII
/// 英数字／`_`、63 文字以下）を満たさない場合、engine のスキーマ解決
/// （`42P01`／`22000`）ではなく `ident::check_identifier` による `42601` に
/// なることを固定する（`search`／`aggregate` op と同じ分類）。
#[test]
fn table_with_invalid_shape_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    for table in ["1abc", "doc s", "doc;s", "doc.s", "-abc"] {
        let body = format!(r#"{{"op":"scan","table":"{table}","limit":1}}"#);
        let resp = query(addr, &token, body.as_bytes());
        assert_eq!(resp.status, 400, "table={table:?} body={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "table={table:?}");
    }
}

/// `table` が 64 文字（`MAX_IDENTIFIER_LEN` 超過）の場合、schema 走査より
/// 前に `42601` で拒否される（DoS 対策としての事前フィルタ。長大文字列は
/// JSON パーサ自体〔`engine::json::MAX_JSON_STRING_CHARS`〕は受理しうる）。
#[test]
fn table_over_length_limit_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let oversized = "a".repeat(64);
    let body = format!(r#"{{"op":"scan","table":"{oversized}","limit":1}}"#);
    let resp = query(addr, &token, body.as_bytes());
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

/// `columns` の要素が識別子形状を満たさない場合も `42601` で拒否される
/// （空文字列・制御文字のみを弾いていた既存判定〔Issue #766〕を、`search`／
/// `aggregate` op と同じ字句解析相当の検査へ揃える。cursor[bot] 指摘）。
#[test]
fn columns_with_invalid_shape_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    for column in ["1abc", "la ng", "la;ng", "la.ng"] {
        let body = format!(r#"{{"op":"scan","table":"docs","limit":1,"columns":["{column}"]}}"#);
        let resp = query(addr, &token, body.as_bytes());
        assert_eq!(resp.status, 400, "column={column:?} body={resp:?}");
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "column={column:?}"
        );
    }
}

/// `columns` 要素が 64 文字（`MAX_IDENTIFIER_LEN` 超過）の場合も `42601`。
#[test]
fn columns_element_over_length_limit_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let oversized = "a".repeat(64);
    let body = format!(r#"{{"op":"scan","table":"docs","limit":1,"columns":["{oversized}"]}}"#);
    let resp = query(addr, &token, body.as_bytes());
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

// --- (13) 応答本文の非漏えい -------------------------------------------------

#[test]
fn responses_never_leak_tenant_id_password_or_token() {
    let (core, _guard) = new_core_scan_docs();
    let (addr, token) = spawn_alice_session(core);

    let bodies: [&[u8]; 3] = [
        br#"{"op":"scan","table":"docs","limit":1}"#,
        br#"{"op":"scan","table":"docs","limit":1,"columns":["score"]}"#,
        br#"{"op":"scan","table":"does_not_exist","limit":1}"#,
    ];
    for body in bodies {
        let resp = query(addr, &token, body);
        let text = String::from_utf8_lossy(&resp.body);
        assert!(!text.contains("tenant-a"), "body={body:?} text={text}");
        assert!(!text.contains("pw-alice"), "body={body:?} text={text}");
        assert!(!text.contains(&token), "body={body:?} text={text}");
    }
}
