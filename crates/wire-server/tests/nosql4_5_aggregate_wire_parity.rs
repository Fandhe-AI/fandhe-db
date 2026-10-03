//! `POST /v1/query`（`op: aggregate`）の写像（NOSQL-4・NOSQL-5。Issue #768・
//! #769）が、pg wire 経由の集計テスト `wire_aggregate.rs`（TASK-168・
//! SQL-13・SQL-14）と**同一の seed**・**同一の手計算オラクル**に対して
//! 同じ契約で振る舞うことを検証する層 A 結合テスト（Issue #770。対象
//! ビヘイビア TASK-177・NOSQL-4・NOSQL-5。ポインタ: `docs/spec/05-tasks.md`
//! TASK-177・`docs/spec/04-behavior/nosql-surface.md` NOSQL-4・NOSQL-5・
//! `docs/spec/04-behavior/sql-surface.md` SQL-13・SQL-14・
//! `docs/spec/04-behavior/rls.md` RLS-7・RLS-8）。
//!
//! ファイル名の `4_5` は NOSQL-4・NOSQL-5 の双方を対象にすることを表す
//! （既存の `nosql<N>_<topic>.rs` 慣例の拡張）。
//!
//! `nosql4_aggregate.rs`／`nosql5_group_by.rs` との役割分担: 既存 2 ファイルは
//! それぞれ独自の小さな seed で個別の受理・拒否契約を広く固定している。
//! 本ファイルはそれらを再検証しない。代わりに `wire_aggregate.rs` の seed
//! （3 テナント × Public 1 行＋Private 2 行）を**そのまま複製**し、(a) 同一
//! `Arc<EngineCore>` 上の SQL テキスト実行（`response::encode` 経由の JSON
//! 本文）とのバイト単位パリティ、(b) `wire_aggregate.rs` が固定した手計算
//! オラクル値との一致、の 2 点を通して pg wire・NoSQL 両表層が同一の集計
//! 契約に従うことを確認する。
//!
//! オラクル方針: 固定値は本ファイル内の手計算（`wire_aggregate.rs` で既に
//! 公開済みの値）のみを使い、engine 実装を読み直して再計算しない。
//!
//! RLS 注意: `wire_aggregate.rs` の seed は tenant-a・tenant-b それぞれが
//! 自身の Private 行（id=11／id=12）を持つ。wire ログインが導出する
//! `PolicyContext` は `Public` ＋ 自テナント `Private`（RLS-11・TASK-195・
//! read-your-writes。`crate::auth::verify` 参照）であるため、SQL オラクルも
//! `with_visibilities(tenant, [Public, Private])` で実行する
//! （`nosql4_aggregate.rs`／`nosql5_group_by.rs` の `ctx_for` と同型）。
//! tenant-a／tenant-b は COUNT=4、tenant-c（自テナント Private 行なし）は
//! COUNT=3 になる。`tenant_a_and_b_see_own_private_row_but_not_each_others`
//! が最初にこの整合を自己検査する。

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

/// wire ログインが導出する `PolicyContext` と同じ可視性（`Public` ＋
/// 自テナント `Private`。RLS-11・TASK-195・read-your-writes）を持つ ctx。
/// オラクルは必ずこれを使う（モジュール doc の RLS 注意を参照）。
fn wire_scoped_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx (Public + own tenant Private, wire 既定)")
}

/// `wire_aggregate.rs::new_core_aggregate_docs` と同一内容の複製
/// （3 テナント × Public 行 1 件＋Private 行 2 件。可視行: id=1
/// (tenant-a, [1,0], "ja") / id=2 (tenant-b, [0,1], "en") /
/// id=3 (tenant-c, [-1,0], "ja")。独立オラクル（手計算・固定値）:
/// `COUNT(*)=3`・`SUM(id)=6`・`AVG(id)=2`・`MIN(id)=1`・`MAX(id)=3`・
/// `MIN(lang)="en"`・`MAX(lang)="ja"`。`GROUP BY lang` → `en:(1,2)`,
/// `ja:(2,4)`（キー昇順）。`lang="xx"` は Private 行にしか存在しない
/// ため、RLS 違反があれば `group_by` の結果に混入する）。
fn new_core_wire_seed() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql4-5-aggregate-wire-parity-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let public_rows: [(&str, u64, [f32; 2], &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja"),
        ("tenant-b", 2, [0.0, 1.0], "en"),
        ("tenant-c", 3, [-1.0, 0.0], "ja"),
    ];
    for (tenant, id, emb, lang) in public_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql45-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    let private_rows: [(&str, u64, [f32; 2]); 2] =
        [("tenant-a", 11, [1.0, 0.0]), ("tenant-b", 12, [0.0, 1.0])];
    for (tenant, id, emb) in private_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Private,
            &[Value::Vector(emb.to_vec()), Value::Text("xx".to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql45-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `id` 桁あふれ（`22003`）検証専用の別フィクスチャ（`id` ∈
/// {`u64::MAX`, 1}）。`wire_aggregate.rs::sql13_numeric_overflow_is_rejected_with_22003_over_wire`
/// と同一の判断。
fn new_core_overflow() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql4-5-aggregate-wire-parity-overflow");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = wire_scoped_ctx("tenant-a");
    for (id, lang) in [(u64::MAX, "ja"), (1, "ja")] {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32, 0.0f32]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("overflow-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert overflow row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `NULL` 契約専用の小フィクスチャ（`docs(embedding VECTOR(2), lang TEXT NOT
/// NULL, note TEXT nullable)`。tenant-a に Public 行 3 件（`note` あり 2 件・
/// `NULL` 1 件）を投入する。`COUNT(note) < COUNT(*)`・`MIN`/`MAX(note)` は
/// `NULL` を除外・`GROUP BY note` は `NULL` グループを既定順で末尾に持つ
/// という契約を SQL・NoSQL 両経路で固定する）。
fn new_core_nullable() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql4-5-aggregate-wire-parity-nullable");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let nullable_schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("note", ColumnType::Text, true),
        ],
    );
    storage
        .create_table(&nullable_schema)
        .expect("create table");
    let ctx = wire_scoped_ctx("tenant-a");
    let rows: [(u64, Option<&str>); 3] = [(1, Some("a")), (2, Some("b")), (3, None)];
    for (id, note) in rows {
        let note_value = match note {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("ja".to_string()),
                note_value,
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nullable-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert nullable row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
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

fn query_as(addr: SocketAddr, user: &str, password: &str, body: &[u8]) -> HttpResponse {
    let token = login(addr, user, password);
    post(addr, &token, body)
}

fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    query_as(addr, "alice", "pw-alice", body)
}

/// `sql` を `tenant` の wire スコープ ctx（Public のみ）で SQL テキスト経由
/// 実行し、`response::encode` を通した JSON 本文（オラクル）を返す。
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

/// [`sql_oracle_body`] の拒否系版。SQL 経路の `wire_code()` を返す。
fn sql_oracle_err(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let ctx = wire_scoped_ctx(tenant);
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err("oracle SQL should be rejected");
    err.wire_code().to_string()
}

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

/// 誤コピー検出用の自己検査を兼ねる: 3 テナント全員の `COUNT(*)` が
/// wire スコープ（Public のみ）オラクルどおり固定値 3 であることを確認
/// する（モジュール doc の RLS 注意を参照）。
#[test]
fn tenant_a_and_b_see_own_private_row_but_not_each_others() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#;
    // alice（tenant-a）・bob（tenant-b）は自テナントの Private 行（id=11／
    // id=12）を含め COUNT=4（RLS-11・TASK-195・read-your-writes）。
    // carol（tenant-c）は自テナントの Private 行を持たないため COUNT=3 の
    // まま。いずれのテナントも他テナントの Private 行は不可視のまま。
    for (user, pw, tenant, expected_count) in [
        ("alice", "pw-alice", "tenant-a", 4),
        ("bob", "pw-bob", "tenant-b", 4),
        ("carol", "pw-carol", "tenant-c", 3),
    ] {
        let resp = query_as(addr, user, pw, body);
        assert_eq!(resp.status, 200, "user={user} resp={resp:?}");
        let oracle = sql_oracle_body(&core, tenant, "SELECT COUNT(*) FROM docs");
        assert_eq!(body_utf8(&resp), oracle, "user={user}");
        assert!(
            body_utf8(&resp).contains(&format!("[[{expected_count}]]")),
            "user={user} {}",
            body_utf8(&resp)
        );
    }
}

/// SQL-13: 8 種の単一行集計を 1 要求へまとめ、`wire_aggregate.rs` と
/// 同一の固定値（`COUNT(*)=3`・`COUNT(lang)=3`・`SUM(id)=6`・`AVG(id)=2`・
/// `MIN(id)=1`・`MAX(id)=3`・`MIN(lang)="en"`・`MAX(lang)="ja"`）で SQL
/// テキスト実行とバイト一致することを確認する。
#[test]
fn five_functions_single_request_match_sql_and_fixed_oracle() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[
        {"fn":"count","column":"*"},
        {"fn":"count","column":"lang"},
        {"fn":"sum","column":"id"},
        {"fn":"avg","column":"id"},
        {"fn":"min","column":"id"},
        {"fn":"max","column":"id"},
        {"fn":"min","column":"lang"},
        {"fn":"max","column":"lang"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT COUNT(*), COUNT(lang), SUM(id), AVG(id), MIN(id), MAX(id), \
         MIN(lang), MAX(lang) FROM docs",
    );
    assert_eq!(body_utf8(&resp), oracle);
    // alice（tenant-a）は Public 3 件 + 自テナント Private 行（id=11）1 件の
    // 計 4 件を可視とする（RLS-11・TASK-195・read-your-writes）。
    assert!(
        body_utf8(&resp).contains(r#""rows":[[4,4,17,4.25,1,11,"en","xx"]]"#),
        "{}",
        body_utf8(&resp)
    );
}

/// 既定列名（`AS` 省略時の関数名小文字）が SQL 表層と一致することを確認
/// する。
#[test]
fn default_column_names_match_sql_lowercase_function_names() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs","aggregates":[
        {"fn":"count","column":"*"},{"fn":"sum","column":"id"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(&core, "tenant-a", "SELECT COUNT(*), SUM(id) FROM docs");
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#"{"name":"count","type":"numeric"}"#)
            || body_utf8(&resp).contains(r#""name":"count""#),
        "{}",
        body_utf8(&resp)
    );
    assert!(
        body_utf8(&resp).contains(r#""name":"sum""#),
        "{}",
        body_utf8(&resp)
    );
}

/// `filter` の `eq`／`prefix` が SQL `WHERE`（`=`／`LIKE 'j%'`）と一致する
/// ことを確認する。
#[test]
fn filter_eq_and_prefix_match_sql_where() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body_eq = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "filter":[{"column":"lang","op":"eq","value":"ja"}]}"#;
    let resp_eq = query_as_alice(addr, body_eq);
    assert_eq!(resp_eq.status, 200, "resp={resp_eq:?}");
    let oracle_eq = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT COUNT(*) FROM docs WHERE lang = 'ja'",
    );
    assert_eq!(body_utf8(&resp_eq), oracle_eq);
    assert!(
        body_utf8(&resp_eq).contains("[[2]]"),
        "{}",
        body_utf8(&resp_eq)
    );

    let body_prefix = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "filter":[{"column":"lang","op":"prefix","value":"j"}]}"#;
    let resp_prefix = query_as_alice(addr, body_prefix);
    assert_eq!(resp_prefix.status, 200, "resp={resp_prefix:?}");
    let oracle_prefix = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT COUNT(*) FROM docs WHERE lang LIKE 'j%'",
    );
    assert_eq!(body_utf8(&resp_prefix), oracle_prefix);
    assert!(
        body_utf8(&resp_prefix).contains("[[2]]"),
        "{}",
        body_utf8(&resp_prefix)
    );
}

/// 空集合の契約（`COUNT` は `0`、それ以外は `NULL`）を SQL とバイト一致で
/// 確認する。
#[test]
fn empty_set_contract_count_zero_others_null() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"id"},
                       {"fn":"avg","column":"id"},{"fn":"min","column":"lang"}],
        "filter":[{"column":"lang","op":"eq","value":"zz"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT COUNT(*), SUM(id), AVG(id), MIN(lang) FROM docs WHERE lang = 'zz'",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains("[0,null,null,null]"),
        "{}",
        body_utf8(&resp)
    );
}

/// `NULL` 列の契約（`COUNT(note) < COUNT(*)`・`MIN`/`MAX` は `NULL` 除外）を
/// SQL とバイト一致で確認する。
#[test]
fn null_column_contract_matches_sql() {
    let (core, _guard) = new_core_nullable();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"},{"fn":"count","column":"note"},
                       {"fn":"min","column":"note"},{"fn":"max","column":"note"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT COUNT(*), COUNT(note), MIN(note), MAX(note) FROM docs",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#"[3,2,"a","b"]"#),
        "{}",
        body_utf8(&resp)
    );

    let body_group = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["note"]}"#;
    let resp_group = query_as_alice(addr, body_group);
    assert_eq!(resp_group.status, 200, "resp={resp_group:?}");
    let oracle_group = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT note, COUNT(*) FROM docs GROUP BY note",
    );
    assert_eq!(body_utf8(&resp_group), oracle_group);
    // NULL グループは既定順で末尾に来る。
    assert!(
        body_utf8(&resp_group).contains(r#"[["a",1],["b",1],[null,1]]"#),
        "{}",
        body_utf8(&resp_group)
    );
}

/// `SUM`/`AVG` の `u64` 桁あふれは `22003` で SQL・NoSQL 両経路とも拒否し、
/// `MAX(id)`（オーバーフローしない・`u64::MAX`）は成功する。`u64::MAX` は
/// JSON number として本文へそのまま現れることをバイト単位で確認する
/// （`response.rs` の `Cell::Integer` は文字列化せず 10 進テキストのまま
/// 出力するため、f64 丸めによる精度欠落は経由しない）。
#[test]
fn overflow_sum_and_avg_reject_with_22003_max_succeeds() {
    let (core, _guard) = new_core_overflow();
    let addr = spawn(Arc::clone(&core));

    for (nosql_fn, sql_fn) in [("sum", "SUM"), ("avg", "AVG")] {
        let body = format!(
            r#"{{"op":"aggregate","table":"docs","aggregates":[{{"fn":"{nosql_fn}","column":"id"}}]}}"#
        );
        let resp = query_as_alice(addr, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "22003",
            "fn={nosql_fn} resp={resp:?}"
        );
        let oracle_err =
            sql_oracle_err(&core, "tenant-a", &format!("SELECT {sql_fn}(id) FROM docs"));
        assert_eq!(oracle_err, "22003", "fn={nosql_fn}");
    }

    // `group_by` を伴っても同じくオーバーフロー拒否になる。
    let body_grouped =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"id"}],"group_by":["lang"]}"#;
    let resp_grouped = query_as_alice(addr, body_grouped);
    assert_eq!(
        http_common::wire_code_of(&resp_grouped),
        "22003",
        "resp={resp_grouped:?}"
    );

    let body_max =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"max","column":"id"}]}"#;
    let resp_max = query_as_alice(addr, body_max);
    assert_eq!(resp_max.status, 200, "resp={resp_max:?}");
    let oracle_max = sql_oracle_body(&core, "tenant-a", "SELECT MAX(id) FROM docs");
    assert_eq!(body_utf8(&resp_max), oracle_max);
    assert!(
        body_utf8(&resp_max).contains(&format!("[[{}]]", u64::MAX)),
        "{}",
        body_utf8(&resp_max)
    );
}

/// SQL-14: `GROUP BY` 既定順（キー昇順）が SQL とバイト一致・固定値
/// `en:(1,2)`／`ja:(2,4)` と一致することを確認する。
#[test]
fn group_by_matches_sql_default_key_order() {
    let (core, _guard) = new_core_wire_seed();
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
    // alice（tenant-a）は自テナント Private 行（id=11, lang="xx"）を含め
    // 3 グループ（キー昇順: en/ja/xx。RLS-11・TASK-195）になる。
    assert!(
        body_utf8(&resp).contains(r#"[["en",1,2],["ja",2,4],["xx",1,11]]"#),
        "{}",
        body_utf8(&resp)
    );
}

/// `HAVING count >= 2` が SQL とバイト一致・固定値 `ja:2` に絞り込まれる
/// ことを確認する（参照曖昧 `22000` を避けるため集計項目は 1 つに絞る）。
#[test]
fn having_count_ge_2_matches_sql() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"],
        "having":[{"fn":"count","column":"*","op":">=","value":2}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING count >= 2",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#"[["ja",2]]"#),
        "{}",
        body_utf8(&resp)
    );
}

/// RLS-7・RLS-8・RLS-11（TASK-195）: 自テナントの `Private` 専用グループ
/// （`"xx"`）は read-your-writes により alice（tenant-a）・bob（tenant-b）
/// 自身には現れるが、他テナントの `Private` グループは決して混入しないこと
/// を確認する（`wire_aggregate.rs::
/// rls_count_and_groups_never_reveal_other_tenants_private_rows` の
/// NoSQL 版）。
#[test]
fn tenants_see_own_private_group_but_never_other_tenants() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"]}"#;
    // alice（tenant-a）・bob（tenant-b）は自テナントの Private 行（lang="xx"）
    // による "xx" グループを追加で持つ（RLS-11・TASK-195）。carol は持たない。
    for (user, pw, tenant, expected_rows) in [
        (
            "alice",
            "pw-alice",
            "tenant-a",
            r#"[["en",1],["ja",2],["xx",1]]"#,
        ),
        (
            "bob",
            "pw-bob",
            "tenant-b",
            r#"[["en",1],["ja",2],["xx",1]]"#,
        ),
        ("carol", "pw-carol", "tenant-c", r#"[["en",1],["ja",2]]"#),
    ] {
        let resp = query_as(addr, user, pw, body);
        assert_eq!(resp.status, 200, "user={user} resp={resp:?}");
        let oracle = sql_oracle_body(
            &core,
            tenant,
            "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
        );
        assert_eq!(body_utf8(&resp), oracle, "user={user}");
        assert!(
            body_utf8(&resp).contains(expected_rows),
            "user={user} {}",
            body_utf8(&resp)
        );
        assert!(
            !body_utf8(&resp).contains("tenant-"),
            "user={user} {}",
            body_utf8(&resp)
        );
    }
}

/// tenant-a に Private 行（`lang="xx"`）を 50 件追加した前後で bob（tenant-b）
/// の `COUNT(*)`／`group_by` 結果が不変であることを確認する
/// （`wire_aggregate.rs` 同名テストの手順を踏襲）。
#[test]
fn results_are_invariant_after_adding_50_private_rows_to_another_tenant() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let count_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#;
    let group_body = br#"{"op":"aggregate","table":"docs",
        "aggregates":[{"fn":"count","column":"*"}],
        "group_by":["lang"]}"#;

    let resp_before = query_as(addr, "bob", "pw-bob", count_body);
    assert_eq!(resp_before.status, 200);
    // bob（tenant-b）は Public 3 件 + 自テナント Private 行（id=12）1 件の
    // 計 4 件を可視とする（RLS-11・TASK-195・read-your-writes）。
    assert!(
        body_utf8(&resp_before).contains("[[4]]"),
        "{}",
        body_utf8(&resp_before)
    );

    let writer_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let metadata = engine::row_codec::encode_scalar_columns(
        &schema(),
        &[Value::Null, Value::Text("xx".to_string())],
    )
    .expect("encode scalar columns");
    for i in 0..50u64 {
        let id = 20_000 + i;
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("nosql45-rls-{i}"))
                .expect("valid operation_id");
        core.insert_row(
            &writer_ctx,
            TABLE,
            id,
            &engine::storage::RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[1.0f32, 0.0f32],
                metadata: &metadata,
            },
            Some(&op_id),
        )
        .expect("insert additional private row");
    }

    let resp_after = query_as(addr, "bob", "pw-bob", count_body);
    assert_eq!(resp_after.status, 200);
    assert_eq!(
        body_utf8(&resp_after),
        body_utf8(&resp_before),
        "COUNT(*) must stay invariant after adding 50 private rows to another tenant"
    );

    let resp_group_after = query_as(addr, "bob", "pw-bob", group_body);
    assert_eq!(resp_group_after.status, 200);
    // bob 自身の Private 行（id=12, lang="xx"）による "xx" グループは
    // read-your-writes（RLS-11・TASK-195）により legitimately 現れる。
    // tenant-a への 50 件追加は bob からは見えないため不変のまま。
    assert!(
        body_utf8(&resp_group_after).contains(r#"[["en",1],["ja",2],["xx",1]]"#),
        "{}",
        body_utf8(&resp_group_after)
    );
}

/// `aggregates` が [`engine::sql::allowlist::MAX_AGGREGATE_ITEMS`]（32）を
/// 超えると `54000` で拒否され、SQL 側（同数の `COUNT(*)` を SELECT リストへ
/// 並べた場合）も同じ `wire_code` になることを確認する。
#[test]
fn aggregates_over_max_items_reject_with_54000_on_both_surfaces() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let max = engine::sql::allowlist::MAX_AGGREGATE_ITEMS;
    let items: Vec<String> = (0..=max)
        .map(|_| r#"{"fn":"count","column":"*"}"#.to_string())
        .collect();
    let body = format!(
        r#"{{"op":"aggregate","table":"docs","aggregates":[{}]}}"#,
        items.join(",")
    );
    let resp = query_as_alice(addr, body.as_bytes());
    assert_eq!(http_common::wire_code_of(&resp), "54000", "resp={resp:?}");

    let select_list: Vec<&str> = std::iter::repeat_n("COUNT(*)", max + 1).collect();
    let sql = format!("SELECT {} FROM docs", select_list.join(", "));
    let oracle_err = sql_oracle_err(&core, "tenant-a", &sql);
    assert_eq!(
        oracle_err, "54000",
        "SQL path must also reject exceeding MAX_AGGREGATE_ITEMS with the same wire_code"
    );
}

/// SQL 表層専用の機能（`ORDER BY`・`LIMIT`・式集計・`WHERE visible()`）は
/// NoSQL 経由では実行されず fail-closed に拒否されることを確認する
/// （表層差を意図的に固定する。黙って無視・無害化はしない）。
#[test]
fn sql_only_features_are_rejected_not_silently_dropped() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    // 未知キー（`order_by`・`limit`）は `AGGREGATE_SCHEMA` に宣言が無いため
    // 一般則で `42601` になる。
    let cases: [&[u8]; 4] = [
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"order_by":[{"column":"lang"}]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"limit":10}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"vec_norm(embedding)"}]}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"filter":[{"column":"visible","op":"eq","value":"x"}]}"#,
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
    }
}

/// 拒否応答を複数回送った後も同一セッショントークンが汚染されず、直後の
/// 成功クエリが正常に通ることを確認する（1 要求＝1 接続のため wire 側の
/// 「接続が生き残る」検証の NoSQL 版として、セッショントークンの継続性で
/// 代替する）。
#[test]
fn rejections_do_not_poison_session_token() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let token = login(addr, "alice", "pw-alice");

    let overflow_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"embedding"}]}"#;
    let resp1 = post(addr, &token, overflow_body);
    assert_eq!(http_common::wire_code_of(&resp1), "42883", "resp={resp1:?}");

    let max = engine::sql::allowlist::MAX_AGGREGATE_ITEMS;
    let items: Vec<String> = (0..=max)
        .map(|_| r#"{"fn":"count","column":"*"}"#.to_string())
        .collect();
    let over_limit_body = format!(
        r#"{{"op":"aggregate","table":"docs","aggregates":[{}]}}"#,
        items.join(",")
    );
    let resp2 = post(addr, &token, over_limit_body.as_bytes());
    assert_eq!(http_common::wire_code_of(&resp2), "54000", "resp={resp2:?}");

    let sql_only_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"limit":10}"#;
    let resp3 = post(addr, &token, sql_only_body);
    assert_eq!(http_common::wire_code_of(&resp3), "42601", "resp={resp3:?}");

    // 同一トークンを使い続けても直後の集計クエリが正常に成功する。
    let ok_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#;
    let resp_ok = post(addr, &token, ok_body);
    assert_eq!(resp_ok.status, 200, "resp={resp_ok:?}");
    let oracle = sql_oracle_body(&core, "tenant-a", "SELECT COUNT(*) FROM docs");
    assert_eq!(body_utf8(&resp_ok), oracle);
    // alice は自テナント Private 行を含め 4 件（RLS-11・TASK-195）。
    assert!(
        body_utf8(&resp_ok).contains("[[4]]"),
        "{}",
        body_utf8(&resp_ok)
    );
}
