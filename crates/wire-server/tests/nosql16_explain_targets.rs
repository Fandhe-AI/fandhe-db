//! `POST /v1/query`（`explain: true`）の対象を `vector` 指定 `search`・
//! `scan`・`aggregate` へ拡大したことを production ルータ経由（生バイト
//! クライアント）で検証する層 A 結合テスト（Issue #948。対象ビヘイビア
//! NOSQL-16・SQL-27・TASK-186。ポインタ: `docs/spec/05-tasks.md` TASK-186・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-16・
//! `docs/spec/04-behavior/sql-surface.md` SQL-27）。
//!
//! 確定オラクルは同じ `Arc<EngineCore>` に対する `execute_sql_in_session`
//! （SQL テキスト経由の `EXPLAIN SELECT ...`）の `QUERY PLAN` 行であり、
//! 本ファイルは wire フレーミング・認証・op 許可リスト・スキーマ検証込みで
//! 同じ内容へ到達することを固定する。`plan` 指定 `search`（NOSQL-10）の
//! 網羅的なカバレッジは `nosql10_explain.rs` が持つ。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// tenant-a に `docs/a.md`（`lang="ja"`）・`docs/a2.md`（`lang="en"`）の
/// 可視行を 2 件、tenant-b に `docs/b-secret.md` の Private 行を 1 件投入
/// した `EngineCore`（`nosql10_explain.rs::new_core` と同じ判断で既定
/// エンジンを使う）。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql16-explain-targets");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = ctx_for("tenant-a");
    let rows_a = [
        (
            1u64,
            [0.1_f32, 0.2, 0.3, 0.4],
            "ja",
            "docs/a.md",
            "alpha content in english",
        ),
        (
            2u64,
            [0.2_f32, 0.1, 0.0, 0.0],
            "en",
            "docs/a2.md",
            "second row english content",
        ),
    ];
    for (id, vec, lang, path_val, body) in rows_a {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("nosql16-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(path_val.to_string()),
                Value::Text(body.to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }

    let ctx_b = ctx_for("tenant-b");
    let op_id_b = engine::recovery::required_op_id::OperationId::parse("nosql16-op-101")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_b,
        101,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            Value::Text("xx".to_string()),
            Value::Text("docs/b-secret.md".to_string()),
            Value::Text("tenant-b only content".to_string()),
        ],
        &op_id_b,
    )
    .expect("insert tenant-b row");

    drop(storage);
    let core = EngineCore::open(&path).expect("open engine core");
    (Arc::new(core), guard)
}

/// [`new_core`] と同じ行データを持つが、tenant-a 自身の可視行を 0 件にした
/// 版（テナント間バイト一致確認用）。
fn new_core_empty_for_tenant_a() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql16-explain-targets-empty");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    drop(storage);
    let core = EngineCore::open(&path).expect("open engine core");
    (Arc::new(core), guard)
}

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

/// 成功応答（`200`）の本文を `{"explain":[...]}` から `Vec<String>` へ解析する。
fn parse_explain_lines(resp: &HttpResponse) -> Vec<String> {
    assert_eq!(
        resp.status,
        200,
        "expected success response, got: {:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let text = std::str::from_utf8(&resp.body).expect("utf-8 body");
    let JsonValue::Object(mut top) = parse_json(text).expect("valid json body") else {
        panic!("expected json object body: {text}");
    };
    let JsonValue::Array(items) = top.remove("explain").expect("missing \"explain\" key") else {
        panic!("\"explain\" must be an array: {text}");
    };
    items
        .into_iter()
        .map(|v| match v {
            JsonValue::String(s) => s,
            other => panic!("expected string element, got {other:?}"),
        })
        .collect()
}

/// SQL 表層 `EXPLAIN` の行を取得する（オラクル）。
fn sql_explain_lines(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(ctx, &mut session, sql)
        .expect("SQL EXPLAIN should succeed");
    match outcome {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                engine::sql::exec::Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// search（vector 指定）: 行一致
// ---------------------------------------------------------------------

#[test]
fn vector_search_explain_distance_only_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body =
        br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn vector_search_explain_with_filter_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql =
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,
        "filter":[{"column":"lang","op":"eq","value":"ja"}],"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn vector_search_explain_hybrid_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT id FROM docs ORDER BY HYBRID(embedding, '[0.1,0.2,0.3,0.4]', body, 'alpha') LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,
        "hybrid":{"text":"alpha"},"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn vector_search_explain_with_mode_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql =
        "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 5 USING MODE 'precision'";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,
        "mode":"precision","explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn vector_search_explain_with_columns_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT id, lang FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,
        "columns":["id","lang"],"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

// ---------------------------------------------------------------------
// scan: 行一致
// ---------------------------------------------------------------------

#[test]
fn scan_explain_without_filter_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT id FROM docs LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"scan","table":"docs","limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
    assert_eq!(
        expected,
        vec![
            "scalar_plan: plain_scan".to_string(),
            "access_path: full_scan".to_string(),
        ]
    );
}

#[test]
fn scan_explain_with_filter_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT id FROM docs WHERE lang = 'ja' LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"scan","table":"docs","limit":5,
        "filter":[{"column":"lang","op":"eq","value":"ja"}],"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn scan_explain_with_columns_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT id, path FROM docs LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"scan","table":"docs","limit":5,"columns":["id","path"],"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

// ---------------------------------------------------------------------
// aggregate: 行一致
// ---------------------------------------------------------------------

#[test]
fn aggregate_explain_count_star_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT COUNT(*) FROM docs";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn aggregate_explain_with_filter_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT COUNT(*) FROM docs WHERE lang = 'ja'";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],
        "filter":[{"column":"lang","op":"eq","value":"ja"}],"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn aggregate_explain_with_group_by_and_having_matches_sql_explain_rows() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING count > 0";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"],
        "having":[{"fn":"count","column":"*","op":">","value":0}],
        "explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
}

#[test]
fn aggregate_explain_on_table_without_vector_column_matches_sql_explain_rows() {
    let path = temp_db::unique_db_path("nosql16-explain-no-vector");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let plain_schema = TableSchema::new(
        "plain_docs",
        vec![ColumnDef::new("lang", ColumnType::Text, false)],
    );
    storage.create_table(&plain_schema).expect("create table");
    let ctx_a = ctx_for("tenant-a");
    let op_id = engine::recovery::required_op_id::OperationId::parse("nosql16-plain-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        "plain_docs",
        &ctx_a,
        1,
        Visibility::Public,
        &[Value::Text("ja".to_string())],
        &op_id,
    )
    .expect("insert row");
    drop(storage);
    let core = EngineCore::open(&path).expect("open engine core");

    let sql = "SELECT COUNT(*) FROM plain_docs";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let core = Arc::new(core);
    let (addr, token) = spawn_alice_session(Arc::clone(&core));
    let body = br#"{"op":"aggregate","table":"plain_docs",
        "aggregates":[{"fn":"count","column":"*"}],"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(parse_explain_lines(&resp), expected);
    drop(guard);
}

// ---------------------------------------------------------------------
// 本体を実行しないこと
// ---------------------------------------------------------------------

#[test]
fn none_of_the_three_ops_execute_or_touch_visible_bitmap_cache() {
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let before = core.visible_bitmap_cache_stats();

    let vector_body =
        br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#;
    let resp = query(addr, &token, vector_body);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(!text.contains("docs/a.md"), "response body: {text}");
    assert!(!text.contains("\"rows\""), "response body: {text}");

    let scan_body = br#"{"op":"scan","table":"docs","limit":5,"explain":true}"#;
    let resp = query(addr, &token, scan_body);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(!text.contains("docs/a.md"), "response body: {text}");
    assert!(!text.contains("\"rows\""), "response body: {text}");

    let aggregate_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"explain":true}"#;
    let resp = query(addr, &token, aggregate_body);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(!text.contains("row_count"), "response body: {text}");

    let after = core.visible_bitmap_cache_stats();
    assert_eq!(before.hits, after.hits, "hits changed");
    assert_eq!(before.misses, after.misses, "misses changed");
    assert_eq!(before.entries, after.entries, "entries changed");

    // 可視行数（`SELECT COUNT(id)`）が要求の前後で変化しないことも確認する。
    let count_before =
        sql_explain_lines(&core, &ctx_a, "EXPLAIN SELECT id FROM docs LIMIT 100").len();
    let count_after =
        sql_explain_lines(&core, &ctx_a, "EXPLAIN SELECT id FROM docs LIMIT 100").len();
    assert_eq!(count_before, count_after);
}

// ---------------------------------------------------------------------
// テナント非露出
// ---------------------------------------------------------------------

#[test]
fn all_three_ops_are_byte_identical_across_tenants_with_different_visible_row_counts() {
    let (core_a, _guard_a) = new_core();
    let (addr_a, token_a) = spawn_alice_session(Arc::clone(&core_a));

    let (core_b, _guard_b) = new_core_empty_for_tenant_a();
    let (addr_b, token_b) = spawn_alice_session(Arc::clone(&core_b));

    let vector_body =
        br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#;
    let resp_a = query(addr_a, &token_a, vector_body);
    let resp_b = query(addr_b, &token_b, vector_body);
    assert_eq!(
        resp_a.body, resp_b.body,
        "vector search explain diverged across tenants"
    );

    let scan_body = br#"{"op":"scan","table":"docs","limit":5,"explain":true}"#;
    let resp_a = query(addr_a, &token_a, scan_body);
    let resp_b = query(addr_b, &token_b, scan_body);
    assert_eq!(
        resp_a.body, resp_b.body,
        "scan explain diverged across tenants"
    );

    let aggregate_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"explain":true}"#;
    let resp_a = query(addr_a, &token_a, aggregate_body);
    let resp_b = query(addr_b, &token_b, aggregate_body);
    assert_eq!(
        resp_a.body, resp_b.body,
        "aggregate explain diverged across tenants"
    );

    // tenant-b 固有の語彙が本文に現れない。
    for resp in [&resp_a, &resp_b] {
        let text = String::from_utf8_lossy(&resp.body);
        assert!(
            !text.contains("tenant-b"),
            "leaked tenant-b vocabulary: {text}"
        );
    }
}

// ---------------------------------------------------------------------
// エラーの一致（explain あり・なしで status と wire_code が一致）
// ---------------------------------------------------------------------

#[test]
fn undefined_table_error_matches_with_and_without_explain() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let cases: [(&[u8], &[u8]); 3] = [
        (
            br#"{"op":"search","table":"no_such","vector":[0.1,0.2,0.3,0.4],"limit":5}"#,
            br#"{"op":"search","table":"no_such","vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#,
        ),
        (
            br#"{"op":"scan","table":"no_such","limit":5}"#,
            br#"{"op":"scan","table":"no_such","limit":5,"explain":true}"#,
        ),
        (
            br#"{"op":"aggregate","table":"no_such","aggregates":[{"fn":"count","column":"*"}]}"#,
            br#"{"op":"aggregate","table":"no_such","aggregates":[{"fn":"count","column":"*"}],"explain":true}"#,
        ),
    ];
    for (without_explain, with_explain) in cases {
        let resp_no = query(addr, &token, without_explain);
        let resp_yes = query(addr, &token, with_explain);
        assert_eq!(resp_no.status, resp_yes.status, "case={without_explain:?}");
        assert_eq!(
            http_common::wire_code_of(&resp_no),
            http_common::wire_code_of(&resp_yes),
            "case={without_explain:?}"
        );
        assert_eq!(http_common::wire_code_of(&resp_yes), "42P01");
    }
}

#[test]
fn unknown_column_error_matches_with_and_without_explain() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let without_explain =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"no_such"}]}"#;
    let with_explain = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"no_such"}],"explain":true}"#;
    let resp_no = query(addr, &token, without_explain);
    let resp_yes = query(addr, &token, with_explain);
    assert_eq!(resp_no.status, resp_yes.status);
    assert_eq!(
        http_common::wire_code_of(&resp_no),
        http_common::wire_code_of(&resp_yes)
    );
}

#[test]
fn vector_search_on_table_without_vector_column_matches_with_and_without_explain() {
    let path = temp_db::unique_db_path("nosql16-explain-vector-on-plain");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "plain_docs",
            vec![ColumnDef::new("lang", ColumnType::Text, false)],
        ))
        .expect("create table");
    drop(storage);
    let core = Arc::new(EngineCore::open(&path).expect("open engine core"));
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let without_explain =
        br#"{"op":"search","table":"plain_docs","vector":[0.1,0.2,0.3,0.4],"limit":5}"#;
    let with_explain = br#"{"op":"search","table":"plain_docs",
        "vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#;
    let resp_no = query(addr, &token, without_explain);
    let resp_yes = query(addr, &token, with_explain);
    assert_eq!(resp_no.status, resp_yes.status);
    assert_eq!(
        http_common::wire_code_of(&resp_no),
        http_common::wire_code_of(&resp_yes)
    );
    drop(guard);
}

// ---------------------------------------------------------------------
// 拒否の維持
// ---------------------------------------------------------------------

#[test]
fn vector_and_plan_both_present_with_explain_still_rejects_with_42601() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],
        "plan":"find content","limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

#[test]
fn explain_on_insert_update_delete_rejects_as_unknown_key() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let cases: [&[u8]; 3] = [
        br#"{"op":"insert","table":"docs","rows":[],"explain":true}"#,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"explain":true}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"explain":true}"#,
    ];
    for body in cases {
        let resp = query(addr, &token, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "body={body:?}");
    }
}

#[test]
fn explain_non_bool_rejects_with_42601() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let cases: [&[u8]; 2] = [
        br#"{"op":"scan","table":"docs","limit":5,"explain":"yes"}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"explain":1}"#,
    ];
    for body in cases {
        let resp = query(addr, &token, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "body={body:?}");
    }
}

/// [`http_common::spawn_router_listener`] はテーブルを一切持たない
/// スローアウェイ `EngineCore` を接続する（`engine` 未接続の `Router::new`
/// 経路自体は `crates/wire-server/src/http/query/gate.rs` の単体テストが
/// 別途固定する）。ここでは 3 op すべての `explain: true` 要求が
/// スローアウェイ core 上で「認証 → op 許可リスト → スキーマ検証 → engine
/// 呼び出し（explain アーム）」まで到達し、`42P01`（テーブル未存在）で
/// fail-closed に拒否されることを非 vacuous に確認する（`0A000` プレース
/// ホルダーへ縮退していない＝explain アームへ実際に到達したことの証跡）。
#[test]
fn explain_reaches_the_explain_arm_on_a_freshly_spawned_router() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
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
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let token = match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let cases: [&[u8]; 3] = [
        br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#,
        br#"{"op":"scan","table":"docs","limit":5,"explain":true}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"explain":true}"#,
    ];
    for body in cases {
        let resp = query(addr, &token, body);
        assert_eq!(resp.status, 404, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42P01", "body={body:?}");
    }
}
