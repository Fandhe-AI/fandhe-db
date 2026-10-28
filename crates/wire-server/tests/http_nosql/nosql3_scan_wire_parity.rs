//! `POST /v1/query`（`op: scan`。NOSQL-3・SQL-15。Issue #766）が、pg wire
//! 経由の広域取得テスト `wire_scan.rs`（Issue #454・SQL-15）と**同一の
//! seed**に対して同じ**行集合**（順序は比較しない）を返し、`limit` に
//! よる早期終了・RLS 暗黙適用が NoSQL 表層越しにも成立することを検証する
//! 層 A 結合テスト（Issue #767。対象ビヘイビア TASK-176・NOSQL-3・SQL-15。
//! ポインタ: `docs/spec/05-tasks.md` TASK-176・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-3・
//! `docs/spec/04-behavior/sql-surface.md` SQL-15）。
//!
//! `nosql3_scan_mapping.rs` との役割分担: 同ファイルは独自の小さな seed で
//! 受理・拒否契約（`42601` 系・`explain`・identifier 検査・`limit` 境界）を
//! 広く固定している。本ファイルはそれらを再検証せず、代わりに
//! `wire_scan.rs::new_core_scan_docs` の seed を**そのまま複製**し、
//! (a) 同一 `Arc<EngineCore>` 上の SQL テキスト実行（`response::encode`
//! 経由の JSON 本文）との行集合パリティ、(b) 早期終了（`limit < 可視行数`
//! で `rows.len() == row_count == limit`）、(c) RLS 暗黙適用（他テナントの
//! Private 行が 0 件）の 3 点を確認する。
//!
//! 早期終了の観測境界: wire 越しには走査打ち切りそのもの（走査件数カウンタ
//! 等）を直接観測する手段が無いため、本ファイルは `limit < 可視行数` の
//! とき `rows.len() == row_count == limit` となることで契約を間接観測する。
//! 「走査が実際に途中で止まる」ことの確定オラクルは
//! `crates/engine/src/sql/scan.rs::early_termination_stops_scanning_once_limit_rows_are_collected`
//! （engine 側単体テスト）が担う。順序の決定性（同一要求を繰り返しても
//! 同一順序になること）は `crates/engine/tests/sql_scan.rs::
//! scan_result_order_is_deterministic_across_repeated_calls` が担うため、
//! 本ファイルでは主張しない（SQL-15 は順序保証を持たない契約のため）。
//!
//! RLS 注意: `wire_scan.rs` の seed は tenant-a **自身**が Private 行
//! （id=11, lang="xx"）を持つ。wire ログインが導出する `PolicyContext` は
//! `Public` ＋ 自テナント `Private`（RLS-11・TASK-195・read-your-writes。
//! `crate::auth::verify` 参照）であるため、SQL オラクルも
//! `with_visibilities(tenant, [Public, Private])` で実行する
//! （`PolicyContext::new`（Public のみ）を使うと tenant-a のオラクルだけ
//! id=11 を欠き NoSQL 応答〔Private 行を含む〕と食い違う）。
//! `self_check_tenants_see_wire_scoped_row_set_via_wire_and_oracle`
//! が最初にこの整合を自己検査する。

use crate::common;
use crate::http_common;
// `temp_db` は `http_common` が `pub mod temp_db;` として再エクスポートする
// ため、ここでは独自に `mod temp_db;` を宣言しない
// （`clippy::duplicate_mod` 回避。`http_common/mod.rs` のコメント参照）。
use http_common::temp_db;

use std::collections::BTreeSet;
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
/// オラクルは必ずこれを使う（モジュール doc の RLS 注意を参照）。
fn wire_scoped_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx (Public + own tenant Private, wire 既定)")
}

/// `wire_scan.rs::new_core_scan_docs` と同一内容の複製。可視行（wire 認証
/// 経路では Public のみ）: id=1 (tenant-a, [1,0], "ja") /
/// id=2 (tenant-b, [0,1], "en") / id=3 (tenant-c, [-1,0], "ja")。
/// `lang="xx"` の Private 行（id=11, tenant-a）は wire 越しには不可視で、
/// RLS 非漏えいの対照に使う。
fn new_core_wire_seed() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql3-scan-wire-parity-docs");
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
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql3-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        11,
        Visibility::Private,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("xx".to_string())],
        &engine::recovery::required_op_id::OperationId::parse("nosql3-op-11")
            .expect("valid operation_id"),
    )
    .expect("insert private row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `filter` 併用の早期終了を観測するための補助 fixture: tenant-a に
/// `lang="ja"` の Public 行 10 件・`lang="en"` の Public 行 10 件、tenant-b
/// に `lang="xx"` の Private 行 3 件を投入する（`sql_scan.rs::
/// seed_single_tenant_corpus` と同型のテナント境界フィクスチャ）。
fn new_core_larger() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql3-scan-wire-parity-larger");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    for i in 0..20u64 {
        let id = 100 + i;
        let lang = if i % 2 == 0 { "ja" } else { "en" };
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql3-larger-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert tenant-a row");
    }

    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");
    for id in 200..=202u64 {
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
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql3-larger-{id}"))
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

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

/// 応答本文を `(columns, rows, row_count)` へ解析する（`columns` は列名の
/// みを取り出す。`nosql3_scan_mapping.rs::parse_success_body` と同型）。
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

/// [`sql_oracle_body`] が返す JSON 本文文字列を `(columns, rows,
/// row_count)` へ解析する（`parse_success_body` の HTTP 応答非依存版。
/// オラクル側は `HttpResponse` を経由しないためステータス検査は行わない）。
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

/// `id`／`lang` 列の値から順序非依存の行集合を作る（`id` は
/// `JsonValue::Number` → `u64`、`lang` は `JsonValue::String`）。
fn id_lang_set(columns: &[String], rows: &[Vec<JsonValue>]) -> BTreeSet<(u64, String)> {
    let id_index = columns.iter().position(|c| c == "id").expect("id column");
    let lang_index = columns
        .iter()
        .position(|c| c == "lang")
        .expect("lang column");
    rows.iter()
        .map(|row| {
            let id = match &row[id_index] {
                JsonValue::Number(n) => n.as_f64() as u64,
                other => panic!("id cell must be a number, got {other:?}"),
            };
            let lang = match &row[lang_index] {
                JsonValue::String(s) => s.clone(),
                other => panic!("lang cell must be a string, got {other:?}"),
            };
            (id, lang)
        })
        .collect()
}

/// 行（セル列の `Debug` 文字列表現）の順序非依存 multiset を作る（`Vec`
/// をソートした文字列表現）。`embedding`（`JsonValue::Array`）セルを含む
/// 投影の比較に使う。件数比較のため `BTreeSet` ではなく重複を保持した
/// `Vec` をソートして返す（`BTreeSet` だと同一行の重複が握りつぶされ、
/// HTTP 側が同じ行を余分に返しても `rows.len()`／`row_count` が一致する
/// 限り検出できないため）。
fn row_multiset(rows: &[Vec<JsonValue>]) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
    v.sort();
    v
}

/// wire スコープ ctx とオラクルの整合性を確認する自己検査: bob/carol は
/// Public のみの固定行集合 `{(1,"ja"),(2,"en"),(3,"ja")}`、alice（自テナント
/// Private 行 id=11 を持つ）はそれに自テナント Private 行を加えた集合を
/// wire・SQL オラクルの双方で観測することを確認する（RLS-11・TASK-195・
/// read-your-writes。モジュール doc の RLS 注意を参照）。
#[test]
fn self_check_tenants_see_wire_scoped_row_set_via_wire_and_oracle() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let public_only: BTreeSet<(u64, String)> = BTreeSet::from([
        (1, "ja".to_string()),
        (2, "en".to_string()),
        (3, "ja".to_string()),
    ]);
    // alice（tenant-a）は自テナントの Private 行（id=11, lang="xx"）も可視
    // （RLS-11・TASK-195・read-your-writes）。bob／carol は自テナントの
    // Private 行を持たないため Public のみ。
    let mut alice_expected = public_only.clone();
    alice_expected.insert((11, "xx".to_string()));

    for (user, pw, tenant, expected, expected_count) in [
        ("alice", "pw-alice", "tenant-a", &alice_expected, 4u64),
        ("bob", "pw-bob", "tenant-b", &public_only, 3u64),
        ("carol", "pw-carol", "tenant-c", &public_only, 3u64),
    ] {
        let body = br#"{"op":"scan","table":"docs","limit":10000,"columns":["id","lang"]}"#;
        let resp = query_as(addr, user, pw, body);
        let (columns, rows, row_count) = parse_success_body(&resp);
        assert_eq!(row_count, expected_count, "user={user}");
        assert_eq!(&id_lang_set(&columns, &rows), expected, "user={user}");

        let oracle = sql_oracle_body(&core, tenant, "SELECT id, lang FROM docs LIMIT 10000");
        let (oracle_columns, oracle_rows, oracle_row_count) = parse_body_str(&oracle);
        assert_eq!(oracle_row_count, expected_count, "user={user}");
        assert_eq!(
            &id_lang_set(&oracle_columns, &oracle_rows),
            expected,
            "user={user}"
        );
    }
}

/// `columns` 指定時、行集合（順序は比較しない）が SQL オラクルと一致する
/// ことを確認する。
#[test]
fn explicit_columns_row_set_matches_sql_oracle_ignoring_order() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"]}"#;
    let resp = query_as_alice(addr, body);
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(columns, vec!["id", "lang"]);
    assert_eq!(row_count, rows.len() as u64);

    let oracle_body = sql_oracle_body(&core, "tenant-a", "SELECT id, lang FROM docs LIMIT 10");
    let (oracle_columns, oracle_rows, oracle_row_count) = parse_body_str(&oracle_body);
    assert_eq!(columns, oracle_columns);
    // `id_lang_set` は `BTreeSet` へ変換するため重複行を握りつぶす。行数を
    // 先に照合し、HTTP 側が同じ行を余分に返す重複混入を検出できるように
    // する（件数が一致して初めて集合比較の等値性に意味がある）。
    assert_eq!(rows.len() as u64, oracle_row_count);
    assert_eq!(
        id_lang_set(&columns, &rows),
        id_lang_set(&oracle_columns, &oracle_rows)
    );
}

/// `columns` 省略時（既定投影 `id`+全実列）の行 multiset（`embedding`
/// セル込み）が `SELECT * FROM docs LIMIT n` オラクルと一致し、`score` 列が
/// 現れないことを確認する。
#[test]
fn default_projection_row_set_matches_sql_star_oracle() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10}"#;
    let resp = query_as_alice(addr, body);
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(columns, vec!["id", "embedding", "lang"]);
    assert!(!columns.iter().any(|c| c == "score"), "columns={columns:?}");
    assert_eq!(row_count, rows.len() as u64);

    let oracle_body = sql_oracle_body(&core, "tenant-a", "SELECT * FROM docs LIMIT 10");
    let (oracle_columns, oracle_rows, _oracle_row_count) = parse_body_str(&oracle_body);
    assert_eq!(columns, oracle_columns);
    assert_eq!(row_multiset(&rows), row_multiset(&oracle_rows));
}

/// `filter`（`eq`）が SQL `WHERE` と一致する行集合を返すことを確認する。
#[test]
fn filter_eq_row_set_matches_sql_where_oracle() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body =
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#;
    let resp = query_as_alice(addr, body);
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, rows.len() as u64);

    let oracle_body = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10",
    );
    let (oracle_columns, oracle_rows, oracle_row_count) = parse_body_str(&oracle_body);
    // `id_lang_set` は `BTreeSet` へ変換するため重複行を握りつぶす。行数を
    // 先に照合し、HTTP 側が同じ行を余分に返す重複混入を検出できるように
    // する（件数が一致して初めて集合比較の等値性に意味がある）。
    assert_eq!(rows.len() as u64, oracle_row_count);
    assert_eq!(
        id_lang_set(&columns, &rows),
        id_lang_set(&oracle_columns, &oracle_rows)
    );
    // tenant-a から可視な `lang="ja"` 公開行は id=1・id=3 のみ。
    assert_eq!(
        id_lang_set(&columns, &rows),
        BTreeSet::from([(1, "ja".to_string()), (3, "ja".to_string())])
    );
}

/// `limit` が可視行数（3 件）未満のとき、`rows.len() == row_count ==
/// limit` になる（早期終了の件数パリティ。どの 2 件が返るかは順序保証なし
/// 契約のため主張しない）。SQL オラクル側の件数とも一致する。
#[test]
fn limit_below_visible_count_returns_exactly_limit_rows() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":2,"columns":["id","lang"]}"#;
    let resp = query_as_alice(addr, body);
    let (_columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 2);
    assert_eq!(rows.len(), 2);

    let oracle_body = sql_oracle_body(&core, "tenant-a", "SELECT id, lang FROM docs LIMIT 2");
    let (_oracle_columns, oracle_rows, oracle_row_count) = parse_body_str(&oracle_body);
    assert_eq!(oracle_row_count, 2);
    assert_eq!(oracle_rows.len(), 2);
}

/// `limit` が可視行数以上のとき、可視行（3 件）がちょうど 1 回ずつ返る
/// （早期終了が発火しない側の境界）。
#[test]
fn limit_at_or_above_visible_count_returns_every_visible_row_once() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"scan","table":"docs","limit":10000,"columns":["id","lang"]}"#;
    let resp = query_as_alice(addr, body);
    let (columns, rows, row_count) = parse_success_body(&resp);
    // alice（tenant-a）は Public 3 件 + 自テナント Private 行（id=11, lang="xx"）
    // 1 件の計 4 件を見る（RLS-11・TASK-195・read-your-writes）。
    assert_eq!(row_count, 4);
    assert_eq!(
        id_lang_set(&columns, &rows),
        BTreeSet::from([
            (1, "ja".to_string()),
            (2, "en".to_string()),
            (3, "ja".to_string()),
            (11, "xx".to_string()),
        ])
    );
}

/// `filter` を併用した大規模コーパス（tenant-a 可視 20 件）での早期終了:
/// `limit:5` はちょうど 5 件（全行 `lang="ja"`）、`limit:10000` は 10 件
/// ちょうど（`lang="ja"` 全件）を返す。
#[test]
fn filter_with_limit_terminates_early_on_larger_corpus() {
    let (core, _guard) = new_core_larger();
    let addr = spawn(Arc::clone(&core));

    let small_body =
        br#"{"op":"scan","table":"docs","limit":5,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#;
    let resp = query_as_alice(addr, small_body);
    let (columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 5);
    assert_eq!(rows.len(), 5);
    let id_index = columns.iter().position(|c| c == "id").expect("id column");
    let lang_index = columns
        .iter()
        .position(|c| c == "lang")
        .expect("lang column");
    let mut seen_ids = BTreeSet::new();
    for row in &rows {
        match &row[lang_index] {
            JsonValue::String(s) => assert_eq!(s, "ja", "row={row:?}"),
            other => panic!("lang cell must be a string, got {other:?}"),
        }
        match &row[id_index] {
            JsonValue::Number(n) => {
                let id = n.as_f64() as u64;
                assert!((100..120).contains(&id), "id out of tenant-a range: {id}");
                seen_ids.insert(id);
            }
            other => panic!("id cell must be a number, got {other:?}"),
        }
    }
    assert_eq!(seen_ids.len(), 5, "ids must be distinct");

    let all_body =
        br#"{"op":"scan","table":"docs","limit":10000,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#;
    let resp_all = query_as_alice(addr, all_body);
    let (_columns_all, rows_all, row_count_all) = parse_success_body(&resp_all);
    assert_eq!(row_count_all, 10);
    assert_eq!(rows_all.len(), 10);
}

/// bob（他テナント）が `lang="xx"` を検索すると `200`・`rows:[]`・
/// `row_count:0` になる（エラーではなく空集合。存在情報を漏らさない。SQL
/// オラクルも 0 件で一致する）。alice（自テナントの Private 行 id=11 を持つ）
/// は read-your-writes（RLS-11・TASK-195）により同じフィルタで id=11 が
/// 1 件ヒットする（漏えいではない）。
#[test]
fn private_rows_are_invisible_to_other_tenants_and_yield_empty_result_not_error() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let body =
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"xx"}]}"#;

    // bob（tenant-b）は id=11（tenant-a の Private 行）を含め自テナントに
    // `Private` 行を持たないため、フィルタは空集合のまま（他テナントの
    // Private 行は不可視。RLS-6/RLS-7 の既存契約）。
    let resp = query_as(addr, "bob", "pw-bob", body);
    assert_eq!(resp.status, 200, "user=bob resp={resp:?}");
    let (_columns, rows, row_count) = parse_success_body(&resp);
    assert_eq!(row_count, 0, "user=bob");
    assert!(rows.is_empty(), "user=bob rows={rows:?}");

    let oracle_body = sql_oracle_body(
        &core,
        "tenant-b",
        "SELECT id, lang FROM docs WHERE lang = 'xx' LIMIT 10",
    );
    let (_oracle_columns, oracle_rows, oracle_row_count) = parse_body_str(&oracle_body);
    assert_eq!(oracle_row_count, 0, "user=bob");
    assert!(oracle_rows.is_empty(), "user=bob");

    // alice（tenant-a）は自テナントの Private 行（id=11, lang="xx"）を
    // read-your-writes により可視のまま filter にマッチする（RLS-11・
    // TASK-195。漏えいではない）。
    let resp_alice = query_as(addr, "alice", "pw-alice", body);
    assert_eq!(resp_alice.status, 200, "user=alice resp={resp_alice:?}");
    let (alice_columns, alice_rows, alice_row_count) = parse_success_body(&resp_alice);
    assert_eq!(alice_row_count, 1, "user=alice");
    assert_eq!(
        id_lang_set(&alice_columns, &alice_rows),
        BTreeSet::from([(11, "xx".to_string())]),
        "user=alice"
    );

    let oracle_body_alice = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id, lang FROM docs WHERE lang = 'xx' LIMIT 10",
    );
    let (oracle_columns_alice, oracle_rows_alice, oracle_row_count_alice) =
        parse_body_str(&oracle_body_alice);
    assert_eq!(oracle_row_count_alice, 1, "user=alice");
    assert_eq!(
        id_lang_set(&oracle_columns_alice, &oracle_rows_alice),
        BTreeSet::from([(11, "xx".to_string())]),
        "user=alice"
    );
}

/// bob（他テナント）の応答本文には Private 行の値（`"xx"`）・id=11 が
/// 一切現れないことを固定する（`nosql3_scan_mapping.rs` (13) と同型）。
/// id=11 の非漏えい検査は `"[11,"`（本モジュール doc に記す `response.rs`
/// の空白なし・キー順固定の出力不変条件どおり、行は `[id,...]` の JSON
/// 配列で始まり、本テストの全 body で `id` が投影の先頭列になる）を探す
/// ——`",11,"` では id が先頭列である本文の形状に一致せず検出漏れになる
/// ため使わない。alice（自テナントの Private 行を持つ）は read-your-writes
/// （RLS-11・TASK-195）によりこれらの値を legitimately 観測しうるため、
/// alice についてはテナント ID・パスワード・Bearer トークン文字列の
/// 非漏えいのみを固定する。
#[test]
fn responses_never_leak_private_rows_tenant_ids_credentials_or_token() {
    let (core, _guard) = new_core_wire_seed();
    let addr = spawn(Arc::clone(&core));

    let token_alice = login(addr, "alice", "pw-alice");
    let token_bob = login(addr, "bob", "pw-bob");

    let bodies: [&[u8]; 4] = [
        br#"{"op":"scan","table":"docs","limit":10000}"#,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"xx"}]}"#,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        br#"{"op":"scan","table":"docs","limit":2}"#,
    ];
    for body in bodies {
        // bob（他テナント）はどのクエリでも tenant-a の Private 行（id=11,
        // lang="xx"）を一切観測できないため、値そのものの非出現まで厳密に
        // 固定する。
        let resp_bob = post(addr, &token_bob, body);
        let text_bob = body_utf8(&resp_bob);
        assert!(
            !text_bob.contains("\"xx\""),
            "body={body:?} text={text_bob}"
        );
        assert!(!text_bob.contains("[11,"), "body={body:?} text={text_bob}");
        assert!(
            !text_bob.contains("tenant-b"),
            "body={body:?} text={text_bob}"
        );
        assert!(
            !text_bob.contains("pw-bob"),
            "body={body:?} text={text_bob}"
        );
        assert!(
            !text_bob.contains(token_bob.as_str()),
            "body={body:?} text={text_bob}"
        );

        // alice（tenant-a）は自テナントの Private 行（id=11, lang="xx"）を
        // read-your-writes（RLS-11・TASK-195）により観測しうるため、値の
        // 非出現は求めない。テナント ID・パスワード・Bearer トークン文字列の
        // 非漏えいのみ固定する。
        let resp_alice = post(addr, &token_alice, body);
        let text_alice = body_utf8(&resp_alice);
        assert!(
            !text_alice.contains("tenant-a"),
            "body={body:?} text={text_alice}"
        );
        assert!(
            !text_alice.contains("pw-alice"),
            "body={body:?} text={text_alice}"
        );
        assert!(
            !text_alice.contains(token_alice.as_str()),
            "body={body:?} text={text_alice}"
        );
    }
}
