//! `POST /v1/query`（`op: aggregate`）が SQL 表層の集計実行計画（TASK-166・
//! SQL-13）と同一結果になることを production ルータ（生バイトクライアント）
//! 経由で検証する層 A 結合テスト（Issue #768。対象ビヘイビア TASK-177・
//! NOSQL-4。ポインタ: `docs/spec/05-tasks.md` TASK-177・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-4・
//! `docs/spec/04-behavior/sql-surface.md` SQL-13）。
//!
//! オラクルは同じ `Arc<EngineCore>` に対する `execute_sql_in_session`
//! （SQL テキスト経由）の `QueryResult` を
//! `wire_server::http::query::response::encode` へ通した JSON 本文
//! （wire 応答の本文と**バイト単位で完全一致**することを確認する。
//! `wire_aggregate.rs`（pg wire 経由）と同じ「値そのものは engine 側テストが
//! 確定オラクル」という方針を踏襲し、本ファイルは NoSQL 表層への写像が
//! それを壊していないことに徹する。
//!
//! `wire_aggregate.rs` と同一 seed による pg wire ↔ NoSQL 2 表層パリティ検証は
//! `nosql4_5_aggregate_wire_parity.rs` を参照（Issue #770）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
// `temp_db` は `http_common` が `pub mod temp_db;` として再エクスポートする
// ため、ここでは独自に `mod temp_db;` を宣言しない
// （`clippy::duplicate_mod` 回避。`http_common/mod.rs` のコメント参照）。
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
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

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs(embedding VECTOR(2), lang TEXT)` を持つ `EngineCore` を新設し、
/// tenant-a に 5 件の可視行（`lang` = "ja" 3 件・"en" 2 件）、tenant-b に
/// 1 件の Private 行（`lang` = "xx"。wire 認証経路では常に不可視）を投入
/// する。`wire_aggregate.rs::new_core_aggregate_docs` と同じ判断
/// （RLS 境界確認用）。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql4-aggregate-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = ctx_for("tenant-a");
    let langs = ["ja", "ja", "ja", "en", "en"];
    for (idx, lang) in langs.iter().enumerate() {
        let id = idx as u64 + 1;
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-a-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text((*lang).to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }

    let ctx_b = ctx_for("tenant-b");
    let op_id = engine::recovery::required_op_id::OperationId::parse("tenant-b-op-101")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_b,
        101,
        Visibility::Private,
        &[
            Value::Vector(vec![101.0, 0.0]),
            Value::Text("xx".to_string()),
        ],
        &op_id,
    )
    .expect("insert tenant-b row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `id` 桁あふれ（`22003`）検証専用の別フィクスチャ（`id` ∈
/// {`u64::MAX`, 1}）。`new_core` と混在させない（`sum(id)` が意図せず
/// オーバーフローしないよう通常フィクスチャは小さい `id` のみを使う）。
fn new_core_overflow() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql4-aggregate-overflow");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx_a = ctx_for("tenant-a");
    for id in [u64::MAX, 1] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("overflow-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(vec![1.0, 0.0]), Value::Text("ja".to_string())],
            &op_id,
        )
        .expect("insert overflow row");
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
    match engine::json::parse_json(&String::from_utf8_lossy(&resp.body))
        .expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
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

/// `tenant-a`（`alice`）のトークンで `body` を送る便宜 API（セッション枠を
/// 使い切らないよう毎回新規ログインする。`nosql9_op_allowlist.rs` と同じ
/// 判断）。
fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

/// `sql` を tenant-a の `PolicyContext` で SQL テキスト経由で実行し、
/// `response::encode` を通した JSON 本文（オラクル）を返す。同じ
/// `Arc<EngineCore>` に対して呼ぶため、wire 越しの応答本文と
/// バイト単位で一致するはずという契約を検証できる。
fn sql_oracle_body(core: &EngineCore, sql: &str) -> String {
    let ctx = ctx_for("tenant-a");
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

#[test]
fn basic_aggregates_match_sql_text_execution_byte_for_byte() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let cases: [(&str, &str); 5] = [
        (
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#,
            "SELECT COUNT(*) FROM docs",
        ),
        (
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"id"}]}"#,
            "SELECT SUM(id) FROM docs",
        ),
        (
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"avg","column":"id"}]}"#,
            "SELECT AVG(id) FROM docs",
        ),
        (
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"min","column":"lang"}]}"#,
            "SELECT MIN(lang) FROM docs",
        ),
        (
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"embedding"}]}"#,
            "SELECT COUNT(embedding) FROM docs",
        ),
    ];

    for (nosql_body, sql) in cases {
        let resp = query_as_alice(addr, nosql_body.as_bytes());
        assert_eq!(resp.status, 200, "nosql_body={nosql_body} resp={resp:?}");
        let oracle = sql_oracle_body(&core, sql);
        assert_eq!(body_utf8(&resp), oracle, "nosql_body={nosql_body}");
    }
}

#[test]
fn filter_matches_sql_where_clause() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "filter":[{"column":"lang","op":"eq","value":"ja"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(&core, "SELECT COUNT(*) FROM docs WHERE lang = 'ja'");
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn empty_visible_set_follows_null_contract() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"id"}],
        "filter":[{"column":"lang","op":"eq","value":"never-matches"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "SELECT COUNT(*), SUM(id) FROM docs WHERE lang = 'never-matches'",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains("[0,null]"),
        "{}",
        body_utf8(&resp)
    );
}

#[test]
fn sum_id_overflow_rejects_with_22003() {
    let (core, _guard) = new_core_overflow();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"id"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "22003", "resp={resp:?}");

    // MAX(id) はオーバーフローしないため成功する。
    let body_max =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"max","column":"id"}]}"#;
    let resp_max = query_as_alice(addr, body_max);
    assert_eq!(resp_max.status, 200, "resp={resp_max:?}");
}

#[test]
fn vector_column_rejects_sum_avg_min_max_with_22000() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    for func in ["sum", "avg", "min", "max"] {
        let body = format!(
            r#"{{"op":"aggregate","table":"docs","aggregates":[{{"fn":"{func}","column":"embedding"}}]}}"#
        );
        let resp = query_as_alice(addr, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42883",
            "func={func} resp={resp:?}"
        );
    }
}

#[test]
fn uppercase_function_name_is_rejected_with_42601() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"COUNT","column":"id"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
}

#[test]
fn star_with_non_count_function_is_rejected_with_42601() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"*"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
}

#[test]
fn unknown_column_is_rejected_with_22000() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"nope"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "22000", "resp={resp:?}");
}

#[test]
fn undefined_table_is_rejected_with_42p01() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"ghost","aggregates":[{"fn":"count","column":"*"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "resp={resp:?}");
}

#[test]
fn malformed_identifier_shape_is_rejected_without_leaking_input() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"do cs","aggregates":[{"fn":"count","column":"*"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
    assert!(!body_utf8(&resp).contains("do cs"), "{}", body_utf8(&resp));
}

/// Issue #948（NOSQL-16・SQL-27）で `aggregate` op への `explain: true` を
/// 受理するよう拡大した（旧来は `42601` で拒否）。`explain: true` は
/// `QUERY PLAN` 応答を返し、集計本体（`row_count`・行データ）を含まない
/// （網羅的なカバレッジ・行一致は `nosql16_explain_targets.rs`・
/// `nosql10_explain.rs` が持つ）。
#[test]
fn explain_true_returns_query_plan_and_does_not_execute() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"explain":true}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let text = body_utf8(&resp);
    assert!(text.contains("\"explain\""), "{text}");
    // 実行していない（`row_count` が本文に一切現れない）ことを確認する。
    assert!(!text.contains("row_count"), "{text}");
}

#[test]
fn malformed_group_by_having_shapes_reject_with_42601_and_do_not_execute() {
    // `group_by`／`having` は Issue #769（NOSQL-5）で `bind` が直接処理する
    // ようになったため、形の逸脱（`group_by` 空配列・`having` の単独指定）は
    // `0A000`（未実装扱い）ではなく `42601`（構文層の拒否と同分類）になる。
    // `group_by`／`having` の詳細な受理・拒否契約は
    // `crates/wire-server/tests/nosql5_group_by.rs` が別途固定する。
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let cases: [&[u8]; 2] = [
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":[]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"having":[]}"#,
    ];
    for body in cases {
        let resp = query_as_alice(addr, body);
        assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
        // 実行していない（`row_count` が本文に一切現れない）ことを確認する。
        assert!(
            !body_utf8(&resp).contains("row_count"),
            "{}",
            body_utf8(&resp)
        );
    }
}

#[test]
fn tenant_a_count_does_not_include_tenant_b_private_row() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    // tenant-a の可視行は 5 件（tenant-b の Private 行 1 件は含まれない）。
    let oracle = sql_oracle_body(&core, "SELECT COUNT(*) FROM docs");
    assert_eq!(body_utf8(&resp), oracle);
    assert!(body_utf8(&resp).contains("[[5]]"), "{}", body_utf8(&resp));
    assert!(!body_utf8(&resp).contains("tenant-a"));
    assert!(!body_utf8(&resp).contains("tenant-b"));
}
