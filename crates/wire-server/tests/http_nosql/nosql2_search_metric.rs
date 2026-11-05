//! `POST /v1/query`（`op: search`）の距離種別 `metric`（Issue #2243）が、SQL 表層の距離演算子
//! （`<=>`／`<->`／`<#>`／`<+>`）と同じ距離・同じ順序・同じ分類で振る舞うことを検証する層 A
//! 結合テスト（ビヘイビア ID のポインタ: NOSQL-2・SQL-33 (7)・INDEX-7。関連: SQL-75）。
//!
//! 方式は `nosql2_search.rs` と同じ。同一 `Arc<EngineCore>` 上の SQL テキスト実行
//! （`execute_sql_in_session` → `response::encode`）をオラクルとして応答本文をバイト単位で比較する。
//! L2／L1 の実行は SQL 表層でも未接続（`0A000`）のため、現時点では両表層とも同じ `0A000` になることを
//! 固定する（結線されたらこの期待値を結果一致へ更新する）。
//!
//! fixture は cosine と内積で順位が変わるノルム非 1 のベクトルを使い、距離の取り違えを検出する。
//! tenant-b の `Private` 行（内積でも cosine でも上位になりうる）は、どの `metric` でも
//! tenant-a の結果に現れない（RLS-7）。

use crate::common;
use crate::http_common;
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, IndexDef, IndexKind, TableSchema, VectorOpClass};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::search_engine::{self, HnswScope};
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

fn wire_scoped_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant ctx (Public only, wire 既定)")
}

/// クエリ `[1,1]` に対し cosine は id3 < id2 < id1、内積は id1 < id2 < id3 の順になる fixture。
/// tenant-b の `Private` 行 id=12（`[100,100]`）は両方で上位になりうるため RLS の対照になる。
fn new_core(
    scope: Option<HnswScope>,
    decls: &[VectorOpClass],
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql2-search-metric");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (i, opclass) in decls.iter().enumerate() {
        storage
            .create_index(&IndexDef::new(
                format!("idx_metric_{i}"),
                TABLE.to_string(),
                IndexKind::Hnsw(*opclass),
                vec!["embedding".to_string()],
            ))
            .expect("declare hnsw index");
    }
    let rows: [(&str, u64, [f32; 2], Visibility); 4] = [
        ("tenant-a", 1, [10.0, 0.0], Visibility::Public),
        ("tenant-a", 2, [1.0, 0.9], Visibility::Public),
        ("tenant-a", 3, [0.1, 0.1], Visibility::Public),
        ("tenant-b", 12, [100.0, 100.0], Visibility::Private),
    ];
    for (tenant, id, emb, visibility) in rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            visibility,
            &[Value::Vector(emb.to_vec()), Value::Text("ja".to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql2-metric-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = match scope {
        None => EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        Some(scope) => {
            let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid hnsw params");
            EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(scope)
        }
    };
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
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn query_as_alice(addr: SocketAddr, body: &str) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");
    let len = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
            ("Content-Length", &len),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

/// SQL 表層の結果。成功は `response::encode` 済みの本文、失敗は `wire_code`。
fn sql_oracle(core: &EngineCore, sql: &str) -> Result<String, String> {
    let ctx = wire_scoped_ctx("tenant-a");
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx, &mut session, sql) {
        Ok(SqlOutcome::Query(result)) => {
            Ok(encode_query_result(&result).expect("oracle result should encode"))
        }
        Ok(other) => panic!("expected SqlOutcome::Query, got {other:?}"),
        Err(err) => Err(err.wire_code().to_string()),
    }
}

/// NoSQL 表層の結果を [`sql_oracle`] と同じ形（成功は本文、失敗は `wire_code`）へ写す。
fn nosql_result(resp: &HttpResponse) -> Result<String, String> {
    if resp.status == 200 {
        Ok(body_utf8(resp))
    } else {
        Err(http_common::wire_code_of(resp))
    }
}

fn search(metric: Option<&str>, extra: &str) -> String {
    let metric = metric
        .map(|m| format!(r#","metric":{m}"#))
        .unwrap_or_default();
    format!(
        r#"{{"op":"search","table":"docs","vector":[1.0,1.0],"limit":3,"columns":["id"]{metric}{extra}}}"#
    )
}

fn sql_knn(op: &str) -> String {
    format!("SELECT id FROM docs ORDER BY embedding {op} '[1.0,1.0]' LIMIT 3")
}

/// 結果の id 列順（`[[1],[2],[3]]` 形）を抜き出す。順位が実際に違うことの確認に使う。
fn ids_of(body: &str) -> String {
    let start = body.find("\"rows\"").unwrap_or(0);
    body[start..].to_string()
}

#[test]
fn cosine_explicit_equals_omitted_equals_sql_cosine() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(Arc::clone(&core));
    let oracle = sql_oracle(&core, &sql_knn("<=>"));
    assert!(oracle.is_ok(), "{oracle:?}");
    assert_eq!(
        nosql_result(&query_as_alice(addr, &search(Some(r#""cosine""#), ""))),
        oracle
    );
    assert_eq!(
        nosql_result(&query_as_alice(addr, &search(None, ""))),
        oracle
    );
}

#[test]
fn ip_matches_sql_inner_product_and_differs_from_cosine() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(Arc::clone(&core));
    let oracle = sql_oracle(&core, &sql_knn("<#>"));
    let cosine = sql_oracle(&core, &sql_knn("<=>"));
    assert!(oracle.is_ok(), "{oracle:?}");
    assert_ne!(
        ids_of(oracle.as_ref().unwrap()),
        ids_of(cosine.as_ref().unwrap())
    );
    let resp = query_as_alice(addr, &search(Some(r#""ip""#), ""));
    assert_eq!(nosql_result(&resp), oracle, "resp={resp:?}");
}

#[test]
fn l2_and_l1_share_the_sql_classification() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(Arc::clone(&core));
    for (metric, op) in [("l2", "<->"), ("l1", "<+>")] {
        let oracle = sql_oracle(&core, &sql_knn(op));
        let resp = query_as_alice(addr, &search(Some(&format!("\"{metric}\"")), ""));
        assert_eq!(nosql_result(&resp), oracle, "{metric}: resp={resp:?}");
    }
}

#[test]
fn invalid_metric_values_are_22023_without_echoing_input() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(core);
    for value in [r#""COSINE""#, r#""euclidean""#, r#""""#] {
        let resp = query_as_alice(addr, &search(Some(value), ""));
        assert_eq!(resp.status, 400, "{value}: resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "22023", "{value}");
        assert!(
            !body_utf8(&resp).contains("euclidean"),
            "{}",
            body_utf8(&resp)
        );
    }
}

#[test]
fn metric_type_mismatch_is_42601() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(core);
    for value in ["1", "null", r#"["ip"]"#] {
        let resp = query_as_alice(addr, &search(Some(value), ""));
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "{value}: resp={resp:?}"
        );
    }
}

#[test]
fn metric_with_plan_or_hybrid_is_42601() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(core);
    let with_hybrid = r#"{"op":"search","table":"docs","vector":[1.0,1.0],"limit":3,"hybrid":{"text":"x"},"metric":"cosine"}"#;
    let resp = query_as_alice(addr, with_hybrid);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
    let with_plan = r#"{"op":"search","table":"docs","plan":"find x","limit":3,"metric":"ip"}"#;
    let resp = query_as_alice(addr, with_plan);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
}

#[test]
fn ip_with_precision_matches_sql_0a000() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(Arc::clone(&core));
    let oracle = sql_oracle(
        &core,
        "SELECT id FROM docs ORDER BY embedding <#> '[1.0,1.0]' LIMIT 3 USING MODE 'precision'",
    );
    assert_eq!(oracle, Err("0A000".to_string()));
    let resp = query_as_alice(addr, &search(Some(r#""ip""#), r#","mode":"precision""#));
    assert_eq!(nosql_result(&resp), oracle, "resp={resp:?}");
}

#[test]
fn unknown_table_takes_precedence_over_invalid_metric() {
    let (core, _guard) = new_core(None, &[]);
    let addr = spawn(core);
    let body = r#"{"op":"search","table":"nope","vector":[1.0,1.0],"limit":3,"metric":"bogus"}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "resp={resp:?}");
}

#[test]
fn omitted_metric_follows_declared_index_opclass() {
    use VectorOpClass::{Cosine, Ip, L2};
    let declared = Some(HnswScope::Declared);
    // (宣言, 省略時に一致すべき SQL 演算子)
    let cases: [(&[VectorOpClass], &str); 3] =
        [(&[Ip], "<#>"), (&[L2], "<->"), (&[Cosine, L2], "<=>")];
    for (decls, op) in cases {
        let (core, _guard) = new_core(declared, decls);
        let addr = spawn(Arc::clone(&core));
        let oracle = sql_oracle(&core, &sql_knn(op));
        let resp = query_as_alice(addr, &search(None, ""));
        assert_eq!(nosql_result(&resp), oracle, "{decls:?}: resp={resp:?}");
    }
}

#[test]
fn opclass_mismatch_degrades_to_exact_scan_matching_sql() {
    let (core, _guard) = new_core(Some(HnswScope::Declared), &[VectorOpClass::Cosine]);
    let addr = spawn(Arc::clone(&core));
    let oracle = sql_oracle(&core, &sql_knn("<#>"));
    assert!(oracle.is_ok(), "{oracle:?}");
    let resp = query_as_alice(addr, &search(Some(r#""ip""#), ""));
    assert_eq!(nosql_result(&resp), oracle, "resp={resp:?}");
}

#[test]
fn other_tenant_private_row_never_appears_for_any_metric() {
    let (core, _guard) = new_core(Some(HnswScope::Declared), &[VectorOpClass::Cosine]);
    let addr = spawn(core);
    for metric in [r#""cosine""#, r#""ip""#] {
        let resp = query_as_alice(addr, &search(Some(metric), ""));
        assert_eq!(resp.status, 200, "{metric}: resp={resp:?}");
        let body = body_utf8(&resp);
        assert!(!body.contains("[12]"), "{metric}: {body}");
        assert!(body.contains(r#""row_count":3"#), "{metric}: {body}");
    }
}

fn explain_lines(body: &str) -> Vec<String> {
    let JsonValue::Object(mut top) = parse_json(body).expect("valid json body") else {
        panic!("expected json object body: {body}");
    };
    let Some(JsonValue::Array(items)) = top.remove("explain") else {
        panic!("missing explain array: {body}");
    };
    items
        .into_iter()
        .map(|v| match v {
            JsonValue::String(s) => s,
            other => panic!("expected string element, got {other:?}"),
        })
        .collect()
}

fn sql_explain(core: &EngineCore, sql: &str) -> Result<Vec<String>, String> {
    let ctx = wire_scoped_ctx("tenant-a");
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx, &mut session, sql) {
        Ok(SqlOutcome::Explain(result)) => Ok(result
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                engine::sql::exec::Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect()),
        Ok(other) => panic!("expected SqlOutcome::Explain, got {other:?}"),
        Err(err) => Err(err.wire_code().to_string()),
    }
}

#[test]
fn explain_matches_sql_explain_for_each_metric() {
    let (core, _guard) = new_core(Some(HnswScope::Declared), &[VectorOpClass::Cosine]);
    let addr = spawn(Arc::clone(&core));
    for (metric, op) in [
        ("cosine", "<=>"),
        ("ip", "<#>"),
        ("l2", "<->"),
        ("l1", "<+>"),
    ] {
        let expected = sql_explain(&core, &format!("EXPLAIN {}", sql_knn(op)));
        let resp = query_as_alice(
            addr,
            &search(Some(&format!("\"{metric}\"")), r#","explain":true"#),
        );
        let got = if resp.status == 200 {
            Ok(explain_lines(&body_utf8(&resp)))
        } else {
            Err(http_common::wire_code_of(&resp))
        };
        assert_eq!(got, expected, "{metric}: resp={resp:?}");
    }
}
