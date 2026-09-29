//! `POST /v1/query`（`op: aggregate`）の複数列 `group_by`（1〜
//! `MAX_GROUP_BY_COLUMNS` 列）が SQL 表層の複数列 `GROUP BY`（SQL-25 (d)）と
//! 同一結果になることを production ルータ（生バイトクライアント）経由で
//! 検証する層 A 結合テスト（Issue #949。対象ビヘイビア TASK-225・
//! NOSQL-16 (b)。ポインタ: `docs/spec/05-tasks.md` TASK-225・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-16 (b)・
//! `docs/spec/04-behavior/sql-surface.md` SQL-25 (d)）。
//!
//! オラクルは `nosql5_group_by.rs` と同じく、同じ `Arc<EngineCore>` に対する
//! `execute_sql_in_session`（SQL テキスト経由）の `QueryResult` を
//! `wire_server::http::query::response::encode` へ通した JSON 本文
//! （wire 応答の本文と**バイト単位で完全一致**することを確認する。
//!
//! 単一列 `group_by`（`["lang"]`）の既存契約・単体テストは
//! `crates/wire-server/src/http/query/aggregate.rs` と `nosql5_group_by.rs`
//! を参照（本ファイルは複数列に固有のケースへ絞る）。

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
            ColumnDef::new("region", ColumnType::Text, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs(embedding VECTOR(2), lang TEXT, region TEXT)` を持つ `EngineCore` を
/// 新設し、tenant-a に `(lang, region)` の組み合わせが重複・`NULL` を含む
/// 可視行、tenant-b に tenant-a とは排他的な組み合わせを持つ Private 行を
/// 投入する（`nosql5_group_by.rs::new_core` と同じ判断。RLS 境界確認用）。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql16-multi-group-by-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = ctx_for("tenant-a");
    // (lang, region): ("ja","jp") x2, ("ja","kr") x1, ("en","us") x1,
    // (NULL,"us") x1。NULL グループ・重複組み合わせの両方を確認する。
    let rows: [(Option<&str>, Option<&str>); 5] = [
        (Some("ja"), Some("jp")),
        (Some("ja"), Some("jp")),
        (Some("ja"), Some("kr")),
        (Some("en"), Some("us")),
        (None, Some("us")),
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
        let region_value = match region {
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
                region_value,
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
            Value::Text("zz".to_string()),
        ],
        &op_id,
    )
    .expect("insert tenant-b row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `MAX_GROUP_BY_COLUMNS`（8）ちょうど・超過（9）を確認するための別フィク
/// スチャ（`wide9(embedding VECTOR(1), c0..c8 TEXT)`。1 行だけ投入すれば
/// 列数上限の検査（`Vec` 確保前）が先に確定するかを固定できる）。
fn new_core_for_column_limit() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql16-multi-group-by-column-limit");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mut columns = vec![ColumnDef::new("embedding", ColumnType::Vector(1), false)];
    for i in 0..9 {
        columns.push(ColumnDef::new(format!("c{i}"), ColumnType::Text, false));
    }
    let wide_schema = TableSchema::new("wide9", columns);
    storage.create_table(&wide_schema).expect("create table");
    let ctx = ctx_for("tenant-a");
    let mut values = vec![Value::Vector(vec![0.0])];
    for i in 0..9 {
        values.push(Value::Text(format!("v{i}")));
    }
    engine::tenant::insert_typed_row(
        &storage,
        "wide9",
        &ctx,
        1,
        Visibility::Public,
        &values,
        &engine::recovery::required_op_id::OperationId::parse("op-1").expect("valid op"),
    )
    .expect("insert row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `MAX_GROUPS`（10,000）超過を 2 キーの直積で確認する別フィクスチャ
/// （`wide2(embedding VECTOR(1), k1 TEXT, k2 TEXT)`。`k1` 101 種 × `k2` 100 種
/// の直積で `MAX_GROUPS` を明らかに超える規模にする。
/// `nosql5_group_by.rs::new_core_over_max_groups` と同じ判断: 境界値ちょうど
/// ではなく「明らかに超過する規模」で `54000` を確認する）。
fn new_core_over_max_groups() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql16-multi-group-by-over-max-groups");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let wide_schema = TableSchema::new(
        "wide2",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(1), false),
            ColumnDef::new("k1", ColumnType::Text, false),
            ColumnDef::new("k2", ColumnType::Text, false),
        ],
    );
    storage.create_table(&wide_schema).expect("create table");
    let ctx = ctx_for("tenant-a");

    let mut id: u64 = 0;
    for i in 0..101u32 {
        for j in 0..100u32 {
            engine::tenant::insert_typed_row(
                &storage,
                "wide2",
                &ctx,
                id,
                Visibility::Public,
                &[
                    Value::Vector(vec![0.0]),
                    Value::Text(format!("k1-{i}")),
                    Value::Text(format!("k2-{j}")),
                ],
                &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                    .expect("valid op"),
            )
            .expect("insert row");
            id += 1;
        }
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
/// 使い切らないよう毎回新規ログインする。`nosql5_group_by.rs` と同じ判断）。
fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

/// `sql` を `tenant` の `PolicyContext` で SQL テキスト経由で実行し、
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
fn multi_column_group_by_matches_sql_byte_for_byte() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"id"}],
        "group_by":["lang","region"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, region, COUNT(*), SUM(id) FROM docs GROUP BY lang, region",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn multi_column_group_by_column_order_matches_sql() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["region","lang"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT region, lang, COUNT(*) FROM docs GROUP BY region, lang",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn single_column_group_by_array_still_matches_sql_as_backward_compat_regression() {
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
}

#[test]
fn having_resolves_correctly_with_multiple_group_by_keys_and_aggregates() {
    // `having[].{fn,column}` は `aggregates` の項目（`fn`＋`column`）のみを
    // 参照する名前空間を持ち、`group_by` のキー列名とは独立に解決される
    // （`aggregate.rs::resolve_having_item_index` のドキュメント参照）。
    // 複数キー（`lang`,`region`）＋複数集計項目＋`having` の組み合わせが
    // SQL テキスト経由と一致することを固定する。
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"id"}],
        "group_by":["lang","region"],
        "having":[{"fn":"sum","column":"id","op":">=","value":0}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, region, COUNT(*), SUM(id) FROM docs GROUP BY lang, region \
         HAVING sum >= 0",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn filter_combines_with_multi_column_group_by() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "filter":[{"column":"lang","op":"eq","value":"ja"}],
        "group_by":["lang","region"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, region, COUNT(*) FROM docs WHERE lang = 'ja' GROUP BY lang, region",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn tenant_a_multi_column_group_by_does_not_reveal_tenant_b_exclusive_group() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang","region"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(!body_utf8(&resp).contains("xx"), "{}", body_utf8(&resp));
    assert!(!body_utf8(&resp).contains("zz"), "{}", body_utf8(&resp));
    assert!(!body_utf8(&resp).contains("tenant-a"));
    assert!(!body_utf8(&resp).contains("tenant-b"));
}

#[test]
fn group_by_column_count_at_limit_succeeds_and_over_limit_rejects_with_54000() {
    let (core, _guard) = new_core_for_column_limit();
    let addr = spawn(Arc::clone(&core));

    let eight_columns: Vec<String> = (0..8).map(|i| format!("\"c{i}\"")).collect();
    let body_ok = format!(
        r#"{{"op":"aggregate","table":"wide9",
           "aggregates":[{{"fn":"count","column":"*"}}],
           "group_by":[{}]}}"#,
        eight_columns.join(",")
    );
    let resp_ok = query_as_alice(addr, body_ok.as_bytes());
    assert_eq!(resp_ok.status, 200, "resp={resp_ok:?}");
    let select_columns = (0..8)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        &format!("SELECT {select_columns}, COUNT(*) FROM wide9 GROUP BY {select_columns}"),
    );
    assert_eq!(body_utf8(&resp_ok), oracle);

    let nine_columns: Vec<String> = (0..9).map(|i| format!("\"c{i}\"")).collect();
    let body_over = format!(
        r#"{{"op":"aggregate","table":"wide9",
           "aggregates":[{{"fn":"count","column":"*"}}],
           "group_by":[{}]}}"#,
        nine_columns.join(",")
    );
    let resp_over = query_as_alice(addr, body_over.as_bytes());
    assert_eq!(
        http_common::wire_code_of(&resp_over),
        "54000",
        "resp={resp_over:?}"
    );
    assert!(
        !body_utf8(&resp_over).contains("row_count"),
        "{}",
        body_utf8(&resp_over)
    );
}

#[test]
fn malformed_multi_column_group_by_shapes_are_rejected_with_42601_without_executing() {
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let cases: [&[u8]; 4] = [
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang","lang"]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":[]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang",123]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang","do cs"]}"#,
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
fn unorderable_or_unknown_second_group_by_column_is_rejected_with_22000() {
    // Issue #1185・SQL-25 (d) で契約改訂: `TEXT` 限定を外したため、拒否されるのは
    // 並べ替え不能な型（`VECTOR`）と未知列だけ（疑似列 `id` は受理側。下の
    // `id_second_group_by_column_is_accepted_and_matches_sql` 参照）。
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let cases: [&[u8]; 2] = [
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang","embedding"]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang","nope"]}"#,
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
fn id_second_group_by_column_is_accepted_and_matches_sql() {
    // NOSQL-16 (b) は SQL-25 (d) の写像のためパリティとして受理する
    // （Issue #1185）。SQL テキスト経由の結果とバイト一致する。
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang","id"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, id, COUNT(*) FROM docs GROUP BY lang, id",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

#[test]
fn group_count_over_max_groups_with_two_keys_rejects_with_54000_and_sql_agrees() {
    let (core, _guard) = new_core_over_max_groups();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"wide2",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["k1","k2"]}"#;
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
            "SELECT k1, k2, COUNT(*) FROM wide2 GROUP BY k1, k2",
        )
        .expect_err("SQL path must also reject exceeding MAX_GROUPS");
    assert_eq!(sql_err.wire_code(), "54000");
}

#[test]
fn explain_true_is_accepted_with_multi_column_group_by_and_returns_fixed_plan() {
    // Issue #948（NOSQL-16・SQL-27・TASK-186）で `aggregate` op の
    // `explain: true` が結線されたため、複数列 `group_by`（Issue #949）でも
    // 同じ束縛（`aggregate.rs::bind`）を経由して受理される（旧: `42601` 拒否）。
    // 検索本体は実行されないため、集計結果（`row_count` 等）は含まれない。
    let (core, _guard) = new_core();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang","region"],
        "explain":true}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body_str = body_utf8(&resp);
    assert!(body_str.contains("\"explain\""), "{body_str}");
    assert!(!body_str.contains("row_count"), "{body_str}");
}
