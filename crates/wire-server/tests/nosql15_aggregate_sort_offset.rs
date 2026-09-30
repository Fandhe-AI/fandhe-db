//! `POST /v1/query`（`op: aggregate`）の `sort`／`offset` が SQL 表層の
//! `GROUP BY ... ORDER BY ... LIMIT ... OFFSET ...`（SQL-25 (a)(b)）と同一結果に
//! なることを production ルータ（生バイトクライアント）経由で検証する層 A
//! 結合テスト（Issue #1198。対象ビヘイビア NOSQL-15・NOSQL-16 (b)。ポインタ:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-15・NOSQL-16・
//! `docs/spec/04-behavior/sql-surface.md` SQL-25）。
//!
//! オラクルは `nosql16_multi_group_by.rs` と同じ方式で、同じ `Arc<EngineCore>` に
//! 対する `execute_sql_in_session`（SQL テキスト経由）の `QueryResult` を
//! `wire_server::http::query::response::encode` へ通した JSON 本文と
//! **バイト単位で完全一致**することを確認する。SQL 表層は `OFFSET` 単独を
//! 受理しない（`LIMIT` 必須）ため、`offset` のみの要求のオラクルは
//! `LIMIT <MAX_GROUPS> OFFSET m` で組み立てる。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::group_by::MAX_GROUPS;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::response::encode as encode_query_result;
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

/// SQL 表層の `OFFSET` は `LIMIT` を伴う場合のみ受理されるため、`offset` 単独の
/// オラクルに使う `LIMIT` 値。`offset` の上限（`MAX_SEARCH_K`）と同値であり、
/// グループ数上限 `MAX_GROUPS` と一致することを下のテストで固定する。
const ORACLE_LIMIT: usize = 10_000;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("region", ColumnType::Text, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// tenant-a に `lang` が ja x3・en x2・fr x1・NULL x1 の可視行、tenant-b に
/// tenant-a と排他的な `lang = "xx"` の Private 行 5 件（件数順位が最上位に
/// なるため、`offset` が不可視グループを数えると結果が変わる）を投入する。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql15-aggregate-sort-offset");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = ctx_for("tenant-a");
    let rows: [(Option<&str>, &str); 7] = [
        (Some("ja"), "jp"),
        (Some("ja"), "jp"),
        (Some("ja"), "kr"),
        (Some("en"), "us"),
        (Some("en"), "uk"),
        (Some("fr"), "fr"),
        (None, "us"),
    ];
    for (idx, (lang, region)) in rows.iter().enumerate() {
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
            &[
                Value::Vector(vec![id as f32, 0.0]),
                lang_value,
                Value::Text((*region).to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }

    let ctx_b = ctx_for("tenant-b");
    for n in 0..5u64 {
        let id = 101 + n;
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-b-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("xx".to_string()),
                Value::Text("zz".to_string()),
            ],
            &op_id,
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

fn query_as(addr: SocketAddr, user: &str, password: &str, body: &str) -> HttpResponse {
    let token = login(addr, user, password);
    post(addr, &token, body.as_bytes())
}

fn query_as_alice(addr: SocketAddr, body: &str) -> HttpResponse {
    query_as(addr, "alice", "pw-alice", body)
}

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

/// `aggregate` 要求本文（`count(*)` 1 項目）を組み立てる。`extra` は先頭に
/// `,` を含む追加キー列。
fn agg(extra: &str) -> String {
    format!(
        r#"{{"op":"aggregate","table":"docs","aggregates":[{{"fn":"count","column":"*"}}]{extra}}}"#
    )
}

/// 要求と SQL オラクルの本文がバイト一致することを、alice（tenant-a）で確認する。
fn assert_matches_sql(core: &Arc<EngineCore>, addr: SocketAddr, request: &str, sql: &str) {
    let resp = query_as_alice(addr, request);
    assert_eq!(resp.status, 200, "request={request} resp={resp:?}");
    assert_eq!(
        body_utf8(&resp),
        sql_oracle_body(core, "tenant-a", sql),
        "request={request}"
    );
}

#[test]
fn oracle_limit_equals_max_groups() {
    assert_eq!(ORACLE_LIMIT, MAX_GROUPS);
}

#[test]
fn sort_on_group_key_matches_sql() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":"lang","sort":[{"column":"lang","dir":"asc"}]"#),
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY lang",
    );
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":["lang"],"sort":[{"column":"lang","dir":"desc"}]"#),
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY lang DESC",
    );
}

#[test]
fn sort_on_aggregate_value_matches_sql_with_group_key_tie_break() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    // en(2)・fr(1)・NULL(1) の同値はグループキー順で決まる。
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":"lang","sort":[{"column":"count","dir":"desc"}]"#),
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY count DESC",
    );
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":"lang","sort":[{"column":"count","dir":"asc"}]"#),
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY count",
    );
}

#[test]
fn sort_with_offset_matches_sql() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    for m in [0usize, 1, 2, 3] {
        assert_matches_sql(
            &core,
            addr,
            &agg(&format!(
                r#","group_by":"lang","sort":[{{"column":"count","dir":"desc"}}],"offset":{m}"#
            )),
            &format!(
                "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY count DESC \
                 LIMIT {ORACLE_LIMIT} OFFSET {m}"
            ),
        );
    }
}

#[test]
fn offset_only_applies_over_default_group_key_order() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":"lang","offset":1"#),
        &format!("SELECT lang, COUNT(*) FROM docs GROUP BY lang LIMIT {ORACLE_LIMIT} OFFSET 1"),
    );
}

#[test]
fn offset_at_or_over_group_count_returns_empty_result() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    // tenant-a の可視グループは 4 つ（ja・en・fr・NULL）。
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":"lang","offset":4"#),
        &format!("SELECT lang, COUNT(*) FROM docs GROUP BY lang LIMIT {ORACLE_LIMIT} OFFSET 4"),
    );
    let resp = query_as_alice(addr, &agg(r#","group_by":"lang","offset":50"#));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(body_utf8(&resp).contains("\"row_count\":0"), "{resp:?}");
}

#[test]
fn multi_column_group_by_with_multi_key_sort_matches_sql() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":["lang","region"],
            "sort":[{"column":"count","dir":"desc"},{"column":"region","dir":"desc"}],
            "offset":1"#),
        &format!(
            "SELECT lang, region, COUNT(*) FROM docs GROUP BY lang, region \
             ORDER BY count DESC, region DESC LIMIT {ORACLE_LIMIT} OFFSET 1"
        ),
    );
}

#[test]
fn having_and_filter_combine_with_sort_and_offset() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","filter":[{"column":"region","op":"eq","value":"us"}],
            "group_by":"lang",
            "having":[{"fn":"count","column":"*","op":">=","value":1}],
            "sort":[{"column":"lang","dir":"desc"}],
            "offset":1"#),
        &format!(
            "SELECT lang, COUNT(*) FROM docs WHERE region = 'us' GROUP BY lang \
             HAVING count >= 1 ORDER BY lang DESC LIMIT {ORACLE_LIMIT} OFFSET 1"
        ),
    );
}

#[test]
fn string_group_by_equals_single_element_array_and_sql() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    let sql = "SELECT lang, COUNT(*) FROM docs GROUP BY lang";
    assert_matches_sql(&core, addr, &agg(r#","group_by":"lang""#), sql);
    assert_matches_sql(&core, addr, &agg(r#","group_by":["lang"]"#), sql);
    assert_matches_sql(
        &core,
        addr,
        &agg(r#","group_by":"lang","having":[{"fn":"count","column":"*","op":">","value":1}]"#),
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING count > 1",
    );
}

#[test]
fn string_group_by_rejections_keep_wire_codes() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    for (extra, code) in [
        (r#","group_by":"nope""#, "22000"),
        (r#","group_by":"embedding""#, "22000"),
        (r#","group_by":"""#, "42601"),
        (r#","group_by":"do cs""#, "42601"),
        (r#","group_by":1"#, "42601"),
        (r#","group_by":{}"#, "42601"),
        (r#","group_by":null"#, "42601"),
    ] {
        let resp = query_as_alice(addr, &agg(extra));
        assert_eq!(
            http_common::wire_code_of(&resp),
            code,
            "extra={extra} resp={resp:?}"
        );
    }
}

#[test]
fn tenant_boundary_offset_skips_only_visible_groups() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    let request = agg(r#","group_by":"lang","sort":[{"column":"count","dir":"desc"}],"offset":1"#);
    let sql = format!(
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY count DESC \
         LIMIT {ORACLE_LIMIT} OFFSET 1"
    );

    let resp_a = query_as_alice(addr, &request);
    assert_eq!(resp_a.status, 200, "resp={resp_a:?}");
    assert_eq!(body_utf8(&resp_a), sql_oracle_body(&core, "tenant-a", &sql));
    assert!(!body_utf8(&resp_a).contains("xx"), "{}", body_utf8(&resp_a));

    let resp_b = query_as(addr, "bob", "pw-bob", &request);
    assert_eq!(resp_b.status, 200, "resp={resp_b:?}");
    assert_eq!(body_utf8(&resp_b), sql_oracle_body(&core, "tenant-b", &sql));
    // tenant-b からは自テナントの Private グループ（xx）が先頭に見え、offset 1 が
    // それを読み飛ばす。tenant-a とは結果が異なる（可視グループのみが offset の対象）。
    assert_ne!(body_utf8(&resp_a), body_utf8(&resp_b));
}

#[test]
fn malformed_sort_is_rejected_without_echoing_input() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    let key = r#"{"column":"lang","dir":"asc"}"#;
    let nine = [key; 9].join(",");
    let cases: Vec<(String, &str)> = vec![
        (r#","group_by":"lang","sort":[]"#.to_string(), "42601"),
        (
            r#","group_by":"lang","sort":[{"column":"count","dir":"DESC"}]"#.to_string(),
            "42601",
        ),
        (
            r#","group_by":"lang","sort":[{"column":"*","dir":"asc"}]"#.to_string(),
            "42601",
        ),
        (
            r#","group_by":"lang","sort":[{"column":"secret_col","dir":"asc"}]"#.to_string(),
            "22000",
        ),
        (format!(r#","group_by":"lang","sort":[{key},{key}]"#), "200"),
        (format!(r#","group_by":"lang","sort":[{nine}]"#), "54000"),
    ];
    for (extra, code) in cases {
        let resp = query_as_alice(addr, &agg(&extra));
        if code == "200" {
            assert_eq!(resp.status, 200, "extra={extra} resp={resp:?}");
            continue;
        }
        assert_eq!(
            http_common::wire_code_of(&resp),
            code,
            "extra={extra} resp={resp:?}"
        );
        assert!(!body_utf8(&resp).contains("row_count"));
        assert!(!body_utf8(&resp).contains("DESC"));
    }
    // 曖昧な参照: `count(*)` と `count(lang)` を併記した `count` は SQL と同型に 22000。
    let ambiguous = r#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"count","column":"lang"}],
        "group_by":"lang","sort":[{"column":"count","dir":"asc"}]}"#;
    let resp = query_as_alice(addr, ambiguous);
    assert_eq!(http_common::wire_code_of(&resp), "22000", "resp={resp:?}");
}

#[test]
fn sort_and_offset_without_group_by_are_rejected_with_42601() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    for extra in [
        r#","sort":[{"column":"count","dir":"asc"}]"#,
        r#","offset":1"#,
        r#","offset":0"#,
    ] {
        let resp = query_as_alice(addr, &agg(extra));
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "extra={extra} resp={resp:?}"
        );
        assert!(!body_utf8(&resp).contains("row_count"));
    }
}

#[test]
fn offset_shape_and_range_errors() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    for (offset, code) in [("-1", "42601"), ("1.5", "42601"), ("10001", "22000")] {
        let resp = query_as_alice(
            addr,
            &agg(&format!(r#","group_by":"lang","offset":{offset}"#)),
        );
        assert_eq!(
            http_common::wire_code_of(&resp),
            code,
            "offset={offset} resp={resp:?}"
        );
    }
    let ok = query_as_alice(addr, &agg(r#","group_by":"lang","offset":10000"#));
    assert_eq!(ok.status, 200, "resp={ok:?}");
}

#[test]
fn explain_is_accepted_together_with_sort_and_offset() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));
    let resp = query_as_alice(
        addr,
        &agg(
            r#","group_by":"lang","sort":[{"column":"count","dir":"desc"}],"offset":1,
            "explain":true"#,
        ),
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = body_utf8(&resp);
    assert!(body.contains("\"explain\""), "{body}");
    assert!(!body.contains("row_count"), "{body}");
}
