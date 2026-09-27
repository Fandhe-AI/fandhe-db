//! `POST /v1/query`（`op: aggregate`）の `group_by`／`having` が SQL 表層の
//! `GROUP BY` 集計実行計画（TASK-167・SQL-14）と同一結果になることを
//! production ルータ（生バイトクライアント）経由で検証する層 A 結合テスト
//! （Issue #769。対象ビヘイビア TASK-177・NOSQL-5。ポインタ:
//! `docs/spec/05-tasks.md` TASK-177・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-5・
//! `docs/spec/04-behavior/sql-surface.md` SQL-14）。
//!
//! オラクルは `nosql4_aggregate.rs` と同じく、同じ `Arc<EngineCore>` に対する
//! `execute_sql_in_session`（SQL テキスト経由）の `QueryResult` を
//! `wire_server::http::query::response::encode` へ通した JSON 本文
//! （wire 応答の本文と**バイト単位で完全一致**することを確認する。
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
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs(embedding VECTOR(2), lang TEXT)` を持つ `EngineCore` を新設し、
/// tenant-a に 6 件の可視行（`lang` = "ja" 3 件・"en" 2 件・`NULL` 1 件）、
/// tenant-b に 1 件の Private 行（`lang` = "xx"。wire 認証経路では常に
/// 不可視）を投入する（`nosql4_aggregate.rs::new_core` と同じ判断。RLS
/// 境界確認用に `NULL` グループも 1 件混ぜる）。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql5-group-by-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = ctx_for("tenant-a");
    let langs: [Option<&str>; 6] = [
        Some("ja"),
        Some("ja"),
        Some("ja"),
        Some("en"),
        Some("en"),
        None,
    ];
    for (idx, lang) in langs.iter().enumerate() {
        let id = idx as u64 + 1;
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-a-op-{id}"))
                .expect("valid operation_id");
        let lang_value = match lang {
            Some(s) => Value::Text((*s).to_string()),
            None => Value::Null,
        };
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(vec![id as f32, 0.0]), lang_value],
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

/// `MAX_GROUPS`（10,000）超過専用の別フィクスチャ（`wide(embedding
/// VECTOR(1), k TEXT)`。`crates/engine/tests/sql_group_by.rs::
/// group_count_over_max_groups_is_rejected_as_payload_too_large` と同じ
/// 判断: 境界値ちょうどではなく「明らかに超過する規模」で `54000` を確認する）。
fn new_core_over_max_groups() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql5-group-by-over-max-groups");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let wide_schema = TableSchema::new(
        "wide",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(1), false),
            ColumnDef::new("k", ColumnType::Text, false),
        ],
    );
    storage.create_table(&wide_schema).expect("create table");
    let ctx = ctx_for("tenant-a");

    const OVER: u64 = engine::sql::group_by::MAX_GROUPS as u64 + 1;
    for i in 0..OVER {
        engine::tenant::insert_typed_row(
            &storage,
            "wide",
            &ctx,
            i,
            Visibility::Public,
            &[Value::Vector(vec![0.0]), Value::Text(format!("k{i}"))],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{i}"))
                .expect("valid op"),
        )
        .expect("insert row");
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
/// 使い切らないよう毎回新規ログインする。`nosql4_aggregate.rs` と同じ判断）。
fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

/// `tenant-b`（`bob`）のトークンで `body` を送る便宜 API。
fn query_as_bob(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "bob", "pw-bob");
    post(addr, &token, body)
}

/// `sql` を tenant-a の `PolicyContext` で SQL テキスト経由で実行し、
/// `response::encode` を通した JSON 本文（オラクル）を返す。
fn sql_oracle_body(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let ctx = ctx_for(tenant);
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
fn group_by_result_matches_sql_text_execution_byte_for_byte() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"id"}],
        "group_by":["lang"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, COUNT(*), SUM(id) FROM docs GROUP BY lang",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn having_matches_sql_text_execution_for_all_five_operators() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let cases: [(&str, f64, &str); 5] = [
        (">=", 2.0, "count >= 2"),
        (">", 2.0, "count > 2"),
        ("<=", 2.0, "count <= 2"),
        ("<", 3.0, "count < 3"),
        ("=", 2.0, "count = 2"),
    ];
    for (op, value, sql_having) in cases {
        let body = format!(
            r#"{{"op":"aggregate","table":"docs",
               "aggregates":[{{"fn":"count","column":"*"}}],
               "group_by":["lang"],
               "having":[{{"fn":"count","column":"*","op":"{op}","value":{value}}}]}}"#
        );
        let resp = query_as_alice(addr, body.as_bytes());
        assert_eq!(resp.status, 200, "op={op} resp={resp:?}");
        let oracle = sql_oracle_body(
            &core,
            "tenant-a",
            &format!("SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING {sql_having}"),
        );
        assert_eq!(body_utf8(&resp), oracle, "op={op}");
    }
}

#[test]
fn having_supports_negative_literal_and_multiple_predicates_with_and() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"id"}],
        "group_by":["lang"],
        "having":[{"fn":"count","column":"*","op":">","value":-1},
                  {"fn":"sum","column":"id","op":"<","value":100}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, COUNT(*), SUM(id) FROM docs GROUP BY lang \
         HAVING count > -1 AND sum < 100",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn filter_combines_with_group_by() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "filter":[{"column":"lang","op":"eq","value":"ja"}],
        "group_by":["lang"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, COUNT(*) FROM docs WHERE lang = 'ja' GROUP BY lang",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn null_group_matches_sql_text_execution() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
    );
    assert_eq!(body_utf8(&resp), oracle);
    // NULL グループ（既定順で末尾）を含むことを確認する。
    assert!(body_utf8(&resp).contains("null"), "{}", body_utf8(&resp));
}

#[test]
fn tenant_a_group_by_does_not_reveal_tenant_b_exclusive_group() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(!body_utf8(&resp).contains("xx"), "{}", body_utf8(&resp));
    assert!(!body_utf8(&resp).contains("tenant-a"));
    assert!(!body_utf8(&resp).contains("tenant-b"));
}

#[test]
fn tenant_b_group_by_sees_cross_tenant_public_rows_and_own_private_row() {
    // `PolicyContext::is_visible`（`engine::policy`）の契約は「`Public` 行は
    // テナントを問わず可視・`Private` 行は所有テナントかつ明示許可
    // （`with_visibilities` に `Private` を含む場合のみ）可視」。wire の
    // ログインセッションが導出する `PolicyContext`（`auth::verify`）は
    // `Public` ＋ 自テナント `Private` を許可可視性集合とするため（RLS-11・
    // TASK-195・read-your-writes。`auth.rs` 参照）、tenant-b（bob）から見て
    // tenant-a の `Public` 行のグループはそのまま見え、かつ tenant-b 自身の
    // `Visibility::Private` 行も可視化される——「他テナントの `Public` 行は
    // 見える」「自テナントの `Private` 行は read-your-writes により見える」
    // の両方を固定する。
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"]}"#;
    let resp = query_as_bob(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let wire_scoped_ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx (Public + own tenant Private, wire 既定)");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &wire_scoped_ctx_b,
            &mut session,
            "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
        )
        .expect("oracle SQL should succeed");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected SqlOutcome::Query");
    };
    let oracle = encode_query_result(&result).expect("oracle result should encode");
    assert_eq!(body_utf8(&resp), oracle);
    // tenant-a の Public 行（"en"・"ja"・NULL の 3 グループ）に加え、
    // tenant-b 自身の Private 行（lang="xx"）のグループも可視（RLS-11・
    // TASK-195・read-your-writes）。
    assert!(body_utf8(&resp).contains("xx"), "{}", body_utf8(&resp));
    assert_eq!(result.rows.len(), 4);
}

#[test]
fn group_count_over_max_groups_rejects_with_54000_and_sql_agrees() {
    let (core, _guard) = new_core_over_max_groups();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"wide",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["k"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "54000", "resp={resp:?}");
    assert!(
        !body_utf8(&resp).contains("row_count"),
        "{}",
        body_utf8(&resp)
    );

    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    let sql_err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT k, COUNT(*) FROM wide GROUP BY k",
        )
        .expect_err("SQL path must also reject exceeding MAX_GROUPS");
    assert_eq!(sql_err.wire_code(), "54000");
}

#[test]
fn malformed_shapes_are_rejected_with_42601_without_executing() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let cases: [&[u8]; 9] = [
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":[]}"#,
        // 複数列 `group_by`（Issue #949）で受理されるようになったため、この
        // ケースが `42601` になる理由は「要素数 ≠ 1」ではなく「重複列名」
        // （engine 側 `BoundAggregate::new_grouped_by_columns` の検査）。
        // 複数列固有のケースは `nosql16_multi_group_by.rs` を参照。
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang","lang"]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"having":[{"fn":"count","column":"*","op":">=","value":1}]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"having":[]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"],"having":[{"fn":"count","column":"*","op":"gt","value":1}]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"COUNT","column":"*"}],"group_by":["lang"],"having":[{"fn":"count","column":"*","op":">=","value":1}]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"],"having":[{"fn":"count","column":"*","op":" >=","value":1}]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["do cs"]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"],"having":[{"fn":"COUNT","column":"*","op":">=","value":1}]}"#,
    ];
    for body in cases {
        let resp = query_as_alice(addr, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "body={} resp={resp:?}",
            String::from_utf8_lossy(body)
        );
        assert!(
            !body_utf8(&resp).contains("row_count"),
            "{}",
            body_utf8(&resp)
        );
        assert!(!body_utf8(&resp).contains("do cs"), "{}", body_utf8(&resp));
    }
}

#[test]
fn type_mismatches_are_rejected_with_22000() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let cases: [&[u8]; 5] = [
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["embedding"]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["id"]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["nope"]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"},{"fn":"min","column":"lang"}],"group_by":["lang"],"having":[{"fn":"min","column":"lang","op":">=","value":1}]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"],"having":[{"fn":"sum","column":"id","op":">=","value":1}]}"#,
    ];
    for body in cases {
        let resp = query_as_alice(addr, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            "22000",
            "body={} resp={resp:?}",
            String::from_utf8_lossy(body)
        );
    }
}

#[test]
fn ambiguous_having_reference_is_rejected_with_22000() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"count","column":"*"}],
        "group_by":["lang"],
        "having":[{"fn":"count","column":"*","op":">=","value":1}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "22000", "resp={resp:?}");
}

#[test]
fn having_predicate_count_over_limit_rejects_with_54000() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let max = engine::sql::allowlist::MAX_AGGREGATE_ITEMS;
    let having_items: Vec<String> = (0..=max)
        .map(|_| r#"{"fn":"count","column":"*","op":">=","value":0}"#.to_string())
        .collect();
    let body = format!(
        r#"{{"op":"aggregate","table":"docs",
           "aggregates":[{{"fn":"count","column":"*"}}],
           "group_by":["lang"],
           "having":[{}]}}"#,
        having_items.join(",")
    );
    let resp = query_as_alice(addr, body.as_bytes());
    assert_eq!(http_common::wire_code_of(&resp), "54000", "resp={resp:?}");
}

#[test]
fn explain_true_is_still_rejected_with_42601_even_with_group_by() {
    // SQL-6 の「`EXPLAIN` は `USING PLAN` 付き検索 `SELECT` 専用」契約の写像
    // として `42601` へ拒否する（NOSQL-10・Issue #765）。
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"],
        "explain":true}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
    assert!(
        !body_utf8(&resp).contains("row_count"),
        "{}",
        body_utf8(&resp)
    );
}

#[test]
fn overflow_still_rejects_with_22003_when_grouped() {
    let path = temp_db::unique_db_path("nosql5-group-by-overflow");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx_a = ctx_for("tenant-a");
    for (id, lang) in [(u64::MAX, "ja"), (1, "ja")] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("overflow-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(vec![1.0, 0.0]), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert overflow row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"sum","column":"id"}],
        "group_by":["lang"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "22003", "resp={resp:?}");
}
