//! `POST /v1/query`（`op: "create_table"｜"alter_table"｜"drop_table"`）を
//! production ルータ（生バイトクライアント）経由で固定する層 A 結合テスト
//! （Issue #910・NOSQL-13・TASK-207。ポインタ: `docs/spec/05-tasks.md`
//! TASK-207・`docs/spec/04-behavior/nosql-surface.md` NOSQL-13・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・
//! `docs/spec/04-behavior/error-format.md` ERR-4）。
//!
//! `crates/wire-server/src/http/query/ddl.rs` 内の unit tests がトークン列
//! 写像の境界値を検証済みのため、本ファイルは「HTTP フレーミング越しに
//! SQL 表層の DDL と同一の実行結果（成功・エラー分類）が観測できること」
//! （DDL 実行権限ゲート・カタログ照会・SQL/NoSQL 間のスキーマパリティを含む）
//! に絞る。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::ClassifiedError;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::Storage;

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::auth::UserStore;
use wire_server::http::session::store::SessionStore;
use wire_server::limits::ConnectionLimiter;

const TENANT_A: &str = "tenant-a";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql13-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql13-ddl-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// [`http_common::spawn_router_listener_with_engine`] と同一の production 入口
/// だが、呼び出し元が組み立てた `UserStore`（`--ddl-allowed-users` 適用済み）を
/// 使う（`wire_ddl_add_column.rs::spawn_server_with_engine_and_store` の HTTP 版）。
fn spawn_with_store(store: UserStore, engine: Arc<EngineCore>) -> std::net::SocketAddr {
    let store = Arc::new(store);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
    let router = wire_server::http::router::Router::with_engine(store, SessionStore::new(), engine);

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_router(
            listener,
            limiter,
            wire_server::limits::READ_TIMEOUT,
            router,
        );
    });

    addr
}

struct Session {
    addr: std::net::SocketAddr,
    token: String,
}

fn login(addr: std::net::SocketAddr, user: &str, password: &str) -> Session {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#).into_bytes();
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        &body,
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
    Session { addr, token }
}

fn query(session: &Session, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", session.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        session.addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn ddl_session(core: Arc<EngineCore>) -> Session {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("alice is a known username");
    let addr = spawn_with_store(store, core);
    login(addr, "alice", "pw-alice")
}

fn non_ddl_session(core: Arc<EngineCore>) -> Session {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_with_store(store, core);
    login(addr, "alice", "pw-alice")
}

// --- create_table ---------------------------------------------------------

#[test]
fn create_table_succeeds_and_is_visible_to_insert_and_scan() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let body = br#"{"op":"create_table","table":"docs","columns":[
        {"name":"embedding","type":"vector","dim":3},
        {"name":"lang","type":"text","nullable":true}
    ]}"#;
    let resp = query(&session, body);
    assert_eq!(resp.status, 200, "got: {resp:?}");
    assert_eq!(String::from_utf8_lossy(&resp.body).trim(), r#"{"ok":true}"#);

    let insert_body =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"lang":"ja"}],"operation_id":"op-1"}"#;
    let insert_resp = query(&session, insert_body);
    assert_eq!(insert_resp.status, 200, "got: {insert_resp:?}");

    let scan_body = br#"{"op":"scan","table":"docs","limit":10}"#;
    let scan_resp = query(&session, scan_body);
    assert_eq!(scan_resp.status, 200, "got: {scan_resp:?}");
}

#[test]
fn create_table_with_primary_key_unique_and_foreign_key_succeeds() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let parent = br#"{"op":"create_table","table":"parent","columns":[
        {"name":"code","type":"integer"}
    ],"constraints":[{"kind":"primary_key","columns":["code"]}]}"#;
    assert_eq!(query(&session, parent).status, 200);

    let child = br#"{"op":"create_table","table":"child","columns":[
        {"name":"embedding","type":"vector","dim":2},
        {"name":"parent_code","type":"integer"},
        {"name":"tag","type":"text"}
    ],"constraints":[
        {"kind":"unique","columns":["tag"]},
        {"kind":"foreign_key","columns":["parent_code"],"references":{"table":"parent","columns":["code"]}}
    ]}"#;
    let resp = query(&session, child);
    assert_eq!(resp.status, 200, "got: {resp:?}");
}

/// 成功応答の本文（`scan`）から行数を取り出す（Issue #1148 の副作用ゼロ検証で
/// 使う。`nosql3_scan_mapping.rs::parse_success_body` と同じ応答形状を前提に
/// 行数のみを読む簡略版）。
fn scan_row_count(resp: &HttpResponse) -> usize {
    assert_eq!(resp.status, 200, "expected 200, got: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body);
    match parse_json(&text).expect("scan body must be valid json") {
        JsonValue::Object(mut top) => match top.remove("rows") {
            Some(JsonValue::Array(rows)) => rows.len(),
            other => panic!("expected rows array, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

// --- FOREIGN KEY 参照アクション（Issue #1148・NOSQL-13。SQL 表層 TABLE-17・
// TASK-205 の `ON DELETE`／`ON UPDATE` と同一のカタログ表現・連鎖適用を
// NoSQL `create_table` からも宣言できることを固定する） -------------------

#[test]
fn create_table_with_on_delete_cascade_removes_child_rows_via_nosql_delete() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let parent = br#"{"op":"create_table","table":"parents","columns":[
        {"name":"name","type":"text"}
    ]}"#;
    assert_eq!(query(&session, parent).status, 200);

    let child = br#"{"op":"create_table","table":"children","columns":[
        {"name":"parent_id","type":"integer","nullable":true},
        {"name":"note","type":"text"}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_id"],
         "references":{"table":"parents","columns":["id"],"on_delete":"cascade"}}
    ]}"#;
    assert_eq!(query(&session, child).status, 200);

    let insert_parent = br#"{"op":"insert","table":"parents","rows":[{"id":1,"name":"p1"}],"operation_id":"fkact-cascade-parent"}"#;
    assert_eq!(query(&session, insert_parent).status, 200);
    let insert_child = br#"{"op":"insert","table":"children","rows":[{"id":1,"parent_id":1,"note":"c1"}],"operation_id":"fkact-cascade-child"}"#;
    assert_eq!(query(&session, insert_child).status, 200);

    let delete = br#"{"op":"delete","table":"parents","where":{"id":1},"operation_id":"fkact-cascade-delete"}"#;
    let resp = query(&session, delete);
    assert_eq!(resp.status, 200, "got: {resp:?}");

    let scan = br#"{"op":"scan","table":"children","limit":10}"#;
    assert_eq!(scan_row_count(&query(&session, scan)), 0);
}

#[test]
fn create_table_with_on_update_set_null_clears_child_column_via_nosql_update() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let parent = br#"{"op":"create_table","table":"parents","columns":[
        {"name":"code","type":"integer"}
    ],"constraints":[{"kind":"unique","columns":["code"]}]}"#;
    assert_eq!(query(&session, parent).status, 200);

    let child = br#"{"op":"create_table","table":"children","columns":[
        {"name":"parent_code","type":"integer","nullable":true},
        {"name":"note","type":"text"}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_code"],
         "references":{"table":"parents","columns":["code"],"on_update":"set_null"}}
    ]}"#;
    assert_eq!(query(&session, child).status, 200);

    let insert_parent = br#"{"op":"insert","table":"parents","rows":[{"id":1,"code":100}],"operation_id":"fkact-setnull-parent"}"#;
    assert_eq!(query(&session, insert_parent).status, 200);
    let insert_child = br#"{"op":"insert","table":"children","rows":[{"id":1,"parent_code":100,"note":"c1"}],"operation_id":"fkact-setnull-child"}"#;
    assert_eq!(query(&session, insert_child).status, 200);

    let update = br#"{"op":"update","table":"parents","set":{"code":200},"where":{"id":1},"operation_id":"fkact-setnull-update"}"#;
    let resp = query(&session, update);
    assert_eq!(resp.status, 200, "got: {resp:?}");

    let scan = br#"{"op":"scan","table":"children","limit":10}"#;
    let scan_resp = query(&session, scan);
    assert_eq!(scan_resp.status, 200, "got: {scan_resp:?}");
    let text = String::from_utf8_lossy(&scan_resp.body);
    let JsonValue::Object(mut top) = parse_json(&text).expect("scan body must be valid json")
    else {
        panic!("expected json object body");
    };
    let JsonValue::Array(columns) = top.remove("columns").expect("columns field") else {
        panic!("columns must be an array");
    };
    // `parent_code` の列位置を列名から解決する（列順の前提を置かない）。
    let parent_code_index = columns
        .iter()
        .position(|c| match c {
            JsonValue::Object(col) => {
                matches!(col.get("name"), Some(JsonValue::String(s)) if s == "parent_code")
            }
            _ => false,
        })
        .expect("parent_code column must exist");
    let JsonValue::Array(rows) = top.remove("rows").expect("rows field") else {
        panic!("rows must be an array");
    };
    assert_eq!(rows.len(), 1);
    let JsonValue::Array(cells) = rows.into_iter().next().expect("one row") else {
        panic!("row must be an array of cells");
    };
    assert!(
        matches!(cells.get(parent_code_index), Some(JsonValue::Null)),
        "parent_code must be set to null after ON UPDATE SET NULL: {cells:?}"
    );
}

/// SQL/NoSQL 同一プロセスパリティ（Issue #1148）: 同じ参照アクション宣言を
/// SQL 表層（生 `execute_sql_in_session`）と NoSQL `create_table` それぞれで
/// 別テーブルへ適用したうえで、**同じ NoSQL DML**（`insert`／`update`／
/// `delete`／`scan`）を両テーブルへ適用し、結果（行集合）が一致することを
/// 固定する（宣言面の違いが実行結果に影響しないことの確認。`cascade`・
/// `set_null` を含める）。
#[test]
fn sql_and_nosql_create_table_produce_identical_results_for_referential_actions() {
    let (core, _guard) = new_core();
    let ctx = PolicyContext::with_visibilities(
        TENANT_A,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant ctx");
    let mut sql_session = engine::sql::mode::SessionState::default();
    sql_session.allow_ddl();
    for ddl in [
        "CREATE TABLE parents_sql (code INTEGER, UNIQUE (code))",
        "CREATE TABLE children_sql (parent_code INTEGER REFERENCES parents_sql (code) \
         ON DELETE CASCADE ON UPDATE SET NULL, tag TEXT)",
    ] {
        core.execute_sql_in_session(&ctx, &mut sql_session, ddl)
            .expect("sql fixture DDL must succeed");
    }

    let nosql_session = ddl_session(Arc::clone(&core));
    let parent_nosql = br#"{"op":"create_table","table":"parents_nosql","columns":[
        {"name":"code","type":"integer"}
    ],"constraints":[{"kind":"unique","columns":["code"]}]}"#;
    assert_eq!(query(&nosql_session, parent_nosql).status, 200);
    let child_nosql = br#"{"op":"create_table","table":"children_nosql","columns":[
        {"name":"parent_code","type":"integer","nullable":true},
        {"name":"tag","type":"text"}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_code"],
         "references":{"table":"parents_nosql","columns":["code"],
         "on_delete":"cascade","on_update":"set_null"}}
    ]}"#;
    assert_eq!(query(&nosql_session, child_nosql).status, 200);

    // 以降は同じ NoSQL DML（`insert`／`update`／`delete`／`scan`）を SQL 側・
    // NoSQL 側それぞれの宣言のテーブルへ適用し、結果（行集合）を比較する。
    // 宣言を SQL テキストで書いたか NoSQL JSON で書いたかは、同一実行器へ
    // 到達したあとのカタログ表現・連鎖適用に差を生まないはずである。
    for (parent, child) in [
        ("parents_sql", "children_sql"),
        ("parents_nosql", "children_nosql"),
    ] {
        let insert_parent_1 = format!(
            r#"{{"op":"insert","table":"{parent}","rows":[{{"id":1,"code":100}}],"operation_id":"op-p1-{parent}"}}"#
        );
        assert_eq!(
            query(&nosql_session, insert_parent_1.as_bytes()).status,
            200
        );
        let insert_parent_2 = format!(
            r#"{{"op":"insert","table":"{parent}","rows":[{{"id":2,"code":300}}],"operation_id":"op-p2-{parent}"}}"#
        );
        assert_eq!(
            query(&nosql_session, insert_parent_2.as_bytes()).status,
            200
        );
        let insert_child_1 = format!(
            r#"{{"op":"insert","table":"{child}","rows":[{{"id":1,"parent_code":100,"tag":"c1"}}],"operation_id":"op-c1-{child}"}}"#
        );
        assert_eq!(query(&nosql_session, insert_child_1.as_bytes()).status, 200);
        let insert_child_2 = format!(
            r#"{{"op":"insert","table":"{child}","rows":[{{"id":2,"parent_code":300,"tag":"c2"}}],"operation_id":"op-c2-{child}"}}"#
        );
        assert_eq!(query(&nosql_session, insert_child_2.as_bytes()).status, 200);

        // `ON UPDATE SET NULL`: 親 1 の `code` 変更で子 1 の `parent_code` が
        // `NULL` になる。
        let update_parent_1 = format!(
            r#"{{"op":"update","table":"{parent}","set":{{"code":200}},"where":{{"id":1}},"operation_id":"op-u1-{parent}"}}"#
        );
        assert_eq!(
            query(&nosql_session, update_parent_1.as_bytes()).status,
            200
        );

        // `ON DELETE CASCADE`: 親 2 の削除で子 2 が連鎖削除される。
        let delete_parent_2 = format!(
            r#"{{"op":"delete","table":"{parent}","where":{{"id":2}},"operation_id":"op-d2-{parent}"}}"#
        );
        assert_eq!(
            query(&nosql_session, delete_parent_2.as_bytes()).status,
            200
        );

        let scan_children = format!(r#"{{"op":"scan","table":"{child}","limit":10}}"#);
        assert_eq!(
            scan_row_count(&query(&nosql_session, scan_children.as_bytes())),
            1,
            "table {child}: cascade delete must remove child 2, leaving only child 1"
        );
    }

    // `children_sql`／`children_nosql` の残存行（子 1）の `parent_code` が
    // いずれも `NULL` になっていることを、列名から位置解決して確認する
    // （列順が変わっても壊れないように、`tag` の値ではなく列名で特定する）。
    for child in ["children_sql", "children_nosql"] {
        let scan_body = format!(r#"{{"op":"scan","table":"{child}","limit":10}}"#);
        let resp = query(&nosql_session, scan_body.as_bytes());
        assert_eq!(resp.status, 200, "got: {resp:?}");
        let text = String::from_utf8_lossy(&resp.body);
        let JsonValue::Object(mut top) = parse_json(&text).expect("scan body must be valid json")
        else {
            panic!("expected json object body");
        };
        let JsonValue::Array(columns) = top.remove("columns").expect("columns field") else {
            panic!("columns must be an array");
        };
        let parent_code_index = columns
            .iter()
            .position(|c| match c {
                JsonValue::Object(col) => {
                    matches!(col.get("name"), Some(JsonValue::String(s)) if s == "parent_code")
                }
                _ => false,
            })
            .expect("parent_code column must exist");
        let JsonValue::Array(rows) = top.remove("rows").expect("rows field") else {
            panic!("rows must be an array");
        };
        assert_eq!(rows.len(), 1, "table {child}: exactly one row must remain");
        let JsonValue::Array(cells) = rows.into_iter().next().expect("one row") else {
            panic!("row must be an array of cells");
        };
        assert!(
            matches!(cells.get(parent_code_index), Some(JsonValue::Null)),
            "table {child}: parent_code must be null after ON UPDATE SET NULL: {cells:?}"
        );
    }
}

#[test]
fn create_table_set_null_on_not_null_column_is_42830() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let parent = br#"{"op":"create_table","table":"parents","columns":[
        {"name":"code","type":"integer"}
    ],"constraints":[{"kind":"unique","columns":["code"]}]}"#;
    assert_eq!(query(&session, parent).status, 200);

    let set_null_not_null = br#"{"op":"create_table","table":"c1","columns":[
        {"name":"parent_code","type":"integer","nullable":false}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_code"],
         "references":{"table":"parents","columns":["code"],"on_delete":"set_null"}}
    ]}"#;
    assert_eq!(
        http_common::wire_code_of(&query(&session, set_null_not_null)),
        "42830"
    );

    let set_default_no_default = br#"{"op":"create_table","table":"c2","columns":[
        {"name":"parent_code","type":"integer","nullable":false}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_code"],
         "references":{"table":"parents","columns":["code"],"on_update":"set_default"}}
    ]}"#;
    assert_eq!(
        http_common::wire_code_of(&query(&session, set_default_no_default)),
        "42830"
    );

    let set_default_with_default = br#"{"op":"create_table","table":"c3","columns":[
        {"name":"parent_code","type":"integer","nullable":false,"default":0}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_code"],
         "references":{"table":"parents","columns":["code"],"on_update":"set_default"}}
    ]}"#;
    assert_eq!(query(&session, set_default_with_default).status, 200);
}

#[test]
fn create_table_unknown_referential_action_is_42601() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let parent = br#"{"op":"create_table","table":"parents","columns":[
        {"name":"name","type":"text"}
    ]}"#;
    assert_eq!(query(&session, parent).status, 200);

    for bad in [
        r#"{"op":"create_table","table":"bogus_action","columns":[
            {"name":"parent_id","type":"integer","nullable":true}
        ],"constraints":[
            {"kind":"foreign_key","columns":["parent_id"],
             "references":{"table":"parents","columns":["id"],"on_delete":"bogus"}}
        ]}"#,
        r#"{"op":"create_table","table":"uppercase_action","columns":[
            {"name":"parent_id","type":"integer","nullable":true}
        ],"constraints":[
            {"kind":"foreign_key","columns":["parent_id"],
             "references":{"table":"parents","columns":["id"],"on_delete":"CASCADE"}}
        ]}"#,
        r#"{"op":"create_table","table":"type_mismatch_action","columns":[
            {"name":"parent_id","type":"integer","nullable":true}
        ],"constraints":[
            {"kind":"foreign_key","columns":["parent_id"],
             "references":{"table":"parents","columns":["id"],"on_delete":1}}
        ]}"#,
        // 同一キーの重複（`on_delete` を 2 回）。`engine::json::parse_json`
        // の重複キー拒否（`parse_json_rejects_duplicate_key_in_nested_object`
        // で固定済み）が JSON 構文解析の時点で `referential_action_tokens`
        // の語彙検証より先に働くため `42601` になる（既知の経路差分:
        // wire 側の語彙検証には到達しないが、分類は SQL 表層の構文エラー
        // と同じ `42601` で揃う）。
        r#"{"op":"create_table","table":"duplicate_key_action","columns":[
            {"name":"parent_id","type":"integer","nullable":true}
        ],"constraints":[
            {"kind":"foreign_key","columns":["parent_id"],
             "references":{"table":"parents","columns":["id"],
             "on_delete":"cascade","on_delete":"set_null"}}
        ]}"#,
    ] {
        let resp = query(&session, bad.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "body={bad} got: {resp:?}"
        );
    }

    // 副作用ゼロ: いずれのテーブルも作られていない。
    for table in [
        "bogus_action",
        "uppercase_action",
        "type_mismatch_action",
        "duplicate_key_action",
    ] {
        let scan = format!(r#"{{"op":"scan","table":"{table}","limit":1}}"#);
        assert_eq!(
            http_common::wire_code_of(&query(&session, scan.as_bytes())),
            "42P01",
            "table {table} must not have been created"
        );
    }
}

#[test]
fn cascade_delete_beyond_depth_limit_via_nosql_is_54000_with_zero_side_effects() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let create = br#"{"op":"create_table","table":"nodes","columns":[
        {"name":"parent_id","type":"integer","nullable":true},
        {"name":"name","type":"text"}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_id"],
         "references":{"table":"nodes","columns":["id"],"on_delete":"cascade"}}
    ]}"#;
    assert_eq!(query(&session, create).status, 200);

    let root = br#"{"op":"insert","table":"nodes","rows":[{"id":1,"name":"root"}],"operation_id":"n13-depth-root"}"#;
    assert_eq!(query(&session, root).status, 200);
    // `MAX_REFERENTIAL_ACTION_DEPTH`（engine 既定値 16。
    // `crates/engine/src/constraint.rs`）を超える一本鎖を 20 行作る
    // （`engine::fk_referential_actions.rs::
    // cascade_delete_beyond_depth_limit_is_54000_with_zero_side_effects` と
    // 同じ構成）。
    for id in 2..=20u64 {
        let body = format!(
            r#"{{"op":"insert","table":"nodes","rows":[{{"id":{id},"parent_id":{prev},"name":"n{id}"}}],"operation_id":"n13-depth-{id}"}}"#,
            prev = id - 1
        );
        assert_eq!(query(&session, body.as_bytes()).status, 200);
    }

    let delete =
        br#"{"op":"delete","table":"nodes","where":{"id":1},"operation_id":"n13-depth-del"}"#;
    assert_eq!(http_common::wire_code_of(&query(&session, delete)), "54000");
    let scan = br#"{"op":"scan","table":"nodes","limit":100}"#;
    assert_eq!(scan_row_count(&query(&session, scan)), 20);

    // 台帳未記録（副作用ゼロ）: 同じ `operation_id` の再送も再び `54000`。
    assert_eq!(http_common::wire_code_of(&query(&session, delete)), "54000");
}

#[test]
fn cascade_delete_does_not_cross_tenant_boundary_via_nosql() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[
        ("alice", TENANT_A, "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string(), "bob".to_string()])
        .expect("both usernames known");
    let addr = spawn_with_store(store, core);
    let alice = login(addr, "alice", "pw-alice");
    let bob = login(addr, "bob", "pw-bob");

    let parent = br#"{"op":"create_table","table":"parents","columns":[
        {"name":"name","type":"text"}
    ]}"#;
    assert_eq!(query(&alice, parent).status, 200);
    let child = br#"{"op":"create_table","table":"children","columns":[
        {"name":"parent_id","type":"integer","nullable":true},
        {"name":"note","type":"text"}
    ],"constraints":[
        {"kind":"foreign_key","columns":["parent_id"],
         "references":{"table":"parents","columns":["id"],"on_delete":"cascade"}}
    ]}"#;
    assert_eq!(query(&alice, child).status, 200);
    // テーブルカタログはテナント間で共有される（RLS 相当のテナント境界は
    // 行単位。`tenant_id` によるテナント分離。security.md P0）。したがって
    // bob（tenant-b）に同名テーブルを別途作る必要はなく、同じ `parents`／
    // `children` へ書き込んだ行が `tenant_id` で分離されることを見る。

    // alice と bob で同じ `id`／`parent_id` の値を使う（operation_id のみ
    // テナントごとに変える）。テナントを跨いだ一意性の前提を置くためでは
    // なく、逆に「値が一致していても tenant_id で分離される」ことを見る
    // ため。ここで id／parent_id をテナントごとに変えてしまうと、連鎖削除が
    // 値一致だけで検索し tenant_id フィルタを取り違えてテナント境界を
    // 越えてしまう不具合があっても bob 側の値と一致せず検出できない
    // （codex レビュー指摘。PR #1157）。
    let insert_parent_alice = br#"{"op":"insert","table":"parents","rows":[{"id":1,"name":"p1"}],"operation_id":"n13-tenant-parent-alice"}"#;
    assert_eq!(query(&alice, insert_parent_alice).status, 200);
    let insert_parent_bob = br#"{"op":"insert","table":"parents","rows":[{"id":1,"name":"p1"}],"operation_id":"n13-tenant-parent-bob"}"#;
    assert_eq!(query(&bob, insert_parent_bob).status, 200);
    let insert_child_alice = br#"{"op":"insert","table":"children","rows":[{"id":1,"parent_id":1,"note":"c1"}],"operation_id":"n13-tenant-child-alice"}"#;
    assert_eq!(query(&alice, insert_child_alice).status, 200);
    let insert_child_bob = br#"{"op":"insert","table":"children","rows":[{"id":1,"parent_id":1,"note":"c1"}],"operation_id":"n13-tenant-child-bob"}"#;
    assert_eq!(query(&bob, insert_child_bob).status, 200);

    // alice（tenant-a）の親削除は alice 側の子だけを連鎖削除し、bob
    // （tenant-b）の同名テーブルの行には影響しない（RLS 相当のテナント境界。
    // security.md P0）。
    let delete =
        br#"{"op":"delete","table":"parents","where":{"id":1},"operation_id":"n13-tenant-delete"}"#;
    assert_eq!(query(&alice, delete).status, 200);

    let scan_children = br#"{"op":"scan","table":"children","limit":10}"#;
    assert_eq!(scan_row_count(&query(&alice, scan_children)), 0);
    assert_eq!(
        scan_row_count(&query(&bob, scan_children)),
        1,
        "bob's tenant must be unaffected by alice's cascade delete"
    );

    // alice の削除操作は `parents` の id=1 という値自体も指定しており、
    // bob 側にも同じ値の行が存在する。親行についても bob 側が残存する
    // ことまで確認し、値一致のみで検索し tenant_id フィルタを取り違える
    // 経路が無いことを親・子の両方で担保する（codex レビュー指摘。PR #1157）。
    let scan_parents = br#"{"op":"scan","table":"parents","limit":10}"#;
    assert_eq!(scan_row_count(&query(&alice, scan_parents)), 0);
    assert_eq!(
        scan_row_count(&query(&bob, scan_parents)),
        1,
        "bob's tenant parent row must be unaffected by alice's cascade delete"
    );
}

#[test]
fn create_table_duplicate_table_is_42p07() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text"}]}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P07", "got: {resp:?}");
}

#[test]
fn create_table_schema_parity_matches_sql_surface_reserved_columns() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    for reserved in ["id", "tenant_id", "visibility", "check", "constraint"] {
        let body = format!(
            r#"{{"op":"create_table","table":"docs","columns":[{{"name":"{reserved}","type":"text"}}]}}"#
        );
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "reserved column {reserved:?} got: {resp:?}"
        );
    }
}

#[test]
fn create_table_unknown_type_and_invalid_vector_shape_are_42601() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let cases = [
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"bogus"}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"vector"}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text","dim":3}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"vector","dim":3,"nullable":true}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text","default":true}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text","default":null}]}"#,
    ];
    for body in cases {
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "body={body} got: {resp:?}"
        );
    }
}

// --- CHECK 制約（Issue #1199・NOSQL-13・TABLE-16。SQL 表層と同じ engine の
// 検査点が違反を `23514`／HTTP 409 で拒否することを固定する） -------------

const CHECK_TABLE_BODY: &str = r#"{"op":"create_table","table":"items","columns":[
    {"name":"qty","type":"integer"},
    {"name":"kind","type":"text"}
],"constraints":[
    {"kind":"check","name":"qty_positive","predicate":[{"column":"qty","op":"gt","value":0}]},
    {"kind":"check","predicate":[{"column":"kind","op":"prefix","value":"a"}]}
]}"#;

fn create_check_table(session: &Session) {
    let resp = query(session, CHECK_TABLE_BODY.as_bytes());
    assert_eq!(resp.status, 200, "got: {resp:?}");
    assert_eq!(String::from_utf8_lossy(&resp.body).trim(), r#"{"ok":true}"#);
}

#[test]
fn create_table_check_constraint_rejects_violating_insert_with_23514() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    create_check_table(&session);

    let ok = br#"{"op":"insert","table":"items","rows":[{"id":1,"qty":5,"kind":"apple"}],"operation_id":"chk-ok"}"#;
    assert_eq!(query(&session, ok).status, 200);

    let bad_qty = br#"{"op":"insert","table":"items","rows":[{"id":2,"qty":0,"kind":"apple"}],"operation_id":"chk-bad-qty"}"#;
    let resp = query(&session, bad_qty);
    assert_eq!(resp.status, 409, "got: {resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23514", "got: {resp:?}");

    let bad_kind = br#"{"op":"insert","table":"items","rows":[{"id":3,"qty":1,"kind":"banana"}],"operation_id":"chk-bad-kind"}"#;
    let resp = query(&session, bad_kind);
    assert_eq!(resp.status, 409, "got: {resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23514", "got: {resp:?}");

    // 違反した書き込みは副作用ゼロ（行が増えていない）。
    let scan = br#"{"op":"scan","table":"items","limit":10}"#;
    assert_eq!(scan_row_count(&query(&session, scan)), 1);
}

#[test]
fn create_table_check_constraint_rejects_violating_update_with_23514() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    create_check_table(&session);

    let ok = br#"{"op":"insert","table":"items","rows":[{"id":1,"qty":5,"kind":"apple"}],"operation_id":"chk-upd-ok"}"#;
    assert_eq!(query(&session, ok).status, 200);

    let update = br#"{"op":"update","table":"items","set":{"qty":-1},"where":{"id":1},"operation_id":"chk-upd-bad"}"#;
    let resp = query(&session, update);
    assert_eq!(resp.status, 409, "got: {resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23514", "got: {resp:?}");
}

#[test]
fn create_table_unnamed_checks_get_distinct_default_names() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body = br#"{"op":"create_table","table":"t","columns":[{"name":"a","type":"integer"}],
        "constraints":[
          {"kind":"check","predicate":[{"column":"a","op":"gt","value":0}]},
          {"kind":"check","predicate":[{"column":"a","op":"lt","value":100}]}
        ]}"#;
    let resp = query(&session, body);
    assert_eq!(resp.status, 200, "got: {resp:?}");
}

#[test]
fn create_table_duplicate_explicit_check_name_is_42601() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body = br#"{"op":"create_table","table":"t","columns":[{"name":"a","type":"integer"}],
        "constraints":[
          {"kind":"check","name":"dup","predicate":[{"column":"a","op":"gt","value":0}]},
          {"kind":"check","name":"dup","predicate":[{"column":"a","op":"lt","value":100}]}
        ]}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "got: {resp:?}");
}

#[test]
fn create_table_check_constraint_malformed_forms_are_42601() {
    let cases: &[(&str, &str)] = &[
        ("columns mixed in", r#"{"kind":"check","columns":["a"]}"#),
        (
            "references mixed in",
            r#"{"kind":"check","predicate":[{"column":"a","op":"gt","value":0}],"references":{"table":"x"}}"#,
        ),
        ("missing predicate", r#"{"kind":"check"}"#),
        ("empty predicate", r#"{"kind":"check","predicate":[]}"#),
        (
            "in op",
            r#"{"kind":"check","predicate":[{"column":"a","op":"in","value":[1,2]}]}"#,
        ),
        (
            "or group",
            r#"{"kind":"check","predicate":[{"or":[{"column":"a","op":"gt","value":0}]}]}"#,
        ),
        (
            "unknown op",
            r#"{"kind":"check","predicate":[{"column":"a","op":"GT","value":0}]}"#,
        ),
        (
            "rls predicate name",
            r#"{"kind":"check","predicate":[{"column":"visible","op":"eq","value":true}]}"#,
        ),
        (
            "exponent number",
            r#"{"kind":"check","predicate":[{"column":"a","op":"gt","value":1e3}]}"#,
        ),
        (
            "bool with range op",
            r#"{"kind":"check","predicate":[{"column":"a","op":"gt","value":true}]}"#,
        ),
        (
            "prefix with number",
            r#"{"kind":"check","predicate":[{"column":"a","op":"prefix","value":1}]}"#,
        ),
        (
            "name on unique",
            r#"{"kind":"unique","columns":["a"],"name":"u"}"#,
        ),
    ];
    for (label, constraint) in cases {
        let (core, _guard) = new_core();
        let session = ddl_session(core);
        let body = format!(
            r#"{{"op":"create_table","table":"t","columns":[{{"name":"a","type":"integer"}}],"constraints":[{constraint}]}}"#
        );
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "case {label}: got {resp:?}"
        );
    }
}

/// Issue #1430・SQL-24: SQL 表層が負の数値リテラルを受理するようになったため、
/// 負数の CHECK 述語は（従来の `42601` から）受理される。指数表記は引き続き拒否される
/// （上の malformed ケース）。
#[test]
fn create_table_check_constraint_negative_number_is_accepted() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body = r#"{"op":"create_table","table":"t","columns":[{"name":"a","type":"integer"}],"constraints":[{"kind":"check","predicate":[{"column":"a","op":"gt","value":-1}]}]}"#;
    let resp = query(&session, body.as_bytes());
    assert_eq!(
        resp.body,
        br#"{"ok":true}"#.to_vec(),
        "negative CHECK literal should be accepted: {resp:?}"
    );
}

#[test]
fn create_table_check_constraint_leaf_count_over_limit_is_54000() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let leaves = vec![r#"{"column":"a","op":"gt","value":0}"#; 257].join(",");
    let body = format!(
        r#"{{"op":"create_table","table":"t","columns":[{{"name":"a","type":"integer"}}],"constraints":[{{"kind":"check","predicate":[{leaves}]}}]}}"#
    );
    let resp = query(&session, body.as_bytes());
    assert_eq!(http_common::wire_code_of(&resp), "54000", "got: {resp:?}");
}

#[test]
fn create_table_check_constraint_without_ddl_permission_is_42501() {
    let (core, _guard) = new_core();
    let session = non_ddl_session(core);
    let resp = query(&session, CHECK_TABLE_BODY.as_bytes());
    assert_eq!(http_common::wire_code_of(&resp), "42501", "got: {resp:?}");
}

#[test]
fn create_table_tenant_id_self_declaration_is_42601() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body =
        br#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text"}],"tenant_id":"evil"}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "got: {resp:?}");
}

// --- alter_table -----------------------------------------------------------

#[test]
fn alter_table_add_column_succeeds() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"alter_table","table":"docs","add_column":{"name":"note","type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(resp.status, 200, "got: {resp:?}");
}

#[test]
fn alter_table_add_column_numeric_and_enum_succeed() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);

    let numeric = br#"{"op":"alter_table","table":"docs","add_column":{"name":"price","type":"numeric","precision":10,"scale":2}}"#;
    assert_eq!(query(&session, numeric).status, 200);

    // ENUM 型は事前登録が無いため、実行段（`Storage::get_enum_type`）が
    // 未定義の型名として `42601`（SQL 表層 `ALTER TABLE ADD COLUMN <col>
    // <未知の識別子>` と同一の分類。ENUM 型名の存在確認は engine 側の実行段
    // が担う）を返す。少なくとも `0A000` へ落ちず engine まで到達したことを
    // 固定する（構造検証段〔wire 側〕とカタログ照会段〔engine 側〕のどちらの
    // `42601` かは区別しないが、いずれも SQL 表層とパリティが取れている）。
    let enum_col = br#"{"op":"alter_table","table":"docs","add_column":{"name":"kind","type":"enum","enum_type":"my_enum"}}"#;
    let resp = query(&session, enum_col);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "got: {resp:?}");
}

#[test]
fn alter_table_add_column_undefined_table_is_42p01() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body =
        br#"{"op":"alter_table","table":"nonexistent","add_column":{"name":"note","type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "got: {resp:?}");
}

#[test]
fn alter_table_add_column_duplicate_column_is_42701() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body =
        br#"{"op":"alter_table","table":"docs","add_column":{"name":"embedding","type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42701", "got: {resp:?}");
}

#[test]
fn alter_table_drop_column_succeeds_and_column_disappears() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let add = br#"{"op":"alter_table","table":"docs","add_column":{"name":"note","type":"text"}}"#;
    assert_eq!(query(&session, add).status, 200);
    let drop = br#"{"op":"alter_table","table":"docs","drop_column":{"name":"note"}}"#;
    let resp = query(&session, drop);
    assert_eq!(resp.status, 200, "got: {resp:?}");
    // 削除後は同名で再 ADD できる（墓標方式でも生存列としては存在しない）。
    assert_eq!(query(&session, add).status, 200);
    // 2 回目の DROP 後に存在しない列を DROP すると 42703。
    assert_eq!(query(&session, drop).status, 200);
    let resp = query(&session, drop);
    assert_eq!(http_common::wire_code_of(&resp), "42703", "got: {resp:?}");
}

/// SQL 表層と同一のエラー契約（Issue #1167）。VECTOR 列・予約列は `42601`、
/// 存在しないテーブルは `42P01`。
#[test]
fn alter_table_drop_column_error_contract_matches_sql_surface() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    for (body, code) in [
        (
            r#"{"op":"alter_table","table":"docs","drop_column":{"name":"embedding"}}"#,
            "42601",
        ),
        (
            r#"{"op":"alter_table","table":"docs","drop_column":{"name":"id"}}"#,
            "42601",
        ),
        (
            r#"{"op":"alter_table","table":"docs","drop_column":{"name":"tenant_id"}}"#,
            "42601",
        ),
        (
            r#"{"op":"alter_table","table":"docs","drop_column":{"name":"missing"}}"#,
            "42703",
        ),
        (
            r#"{"op":"alter_table","table":"nope","drop_column":{"name":"a"}}"#,
            "42P01",
        ),
    ] {
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            code,
            "body={body} got: {resp:?}"
        );
    }
}

#[test]
fn alter_table_drop_column_without_ddl_permission_is_42501() {
    let (core, _guard) = new_core_with_docs_table();
    let session = non_ddl_session(core);
    for table in ["docs", "nope"] {
        let body = format!(
            r#"{{"op":"alter_table","table":"{table}","drop_column":{{"name":"embedding"}}}}"#
        );
        let resp = query(&session, body.as_bytes());
        assert_eq!(http_common::wire_code_of(&resp), "42501", "got: {resp:?}");
    }
}

#[test]
fn alter_table_both_and_neither_add_and_drop_column_is_42601() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);

    let both = br#"{"op":"alter_table","table":"docs","add_column":{"name":"a","type":"text"},"drop_column":{"name":"embedding"}}"#;
    assert_eq!(http_common::wire_code_of(&query(&session, both)), "42601");

    let neither = br#"{"op":"alter_table","table":"docs"}"#;
    assert_eq!(
        http_common::wire_code_of(&query(&session, neither)),
        "42601"
    );
}

#[test]
fn alter_table_add_column_reserved_enum_type_keyword_is_42601() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"alter_table","table":"docs","add_column":{"name":"kind","type":"enum","enum_type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "got: {resp:?}");
}

// --- drop_table --------------------------------------------------------

#[test]
fn drop_table_succeeds_and_subsequent_scan_reports_undefined_table() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);

    let resp = query(&session, br#"{"op":"drop_table","table":"docs"}"#);
    assert_eq!(resp.status, 200, "got: {resp:?}");
    assert_eq!(String::from_utf8_lossy(&resp.body).trim(), r#"{"ok":true}"#);

    let scan_resp = query(&session, br#"{"op":"scan","table":"docs","limit":1}"#);
    assert_eq!(http_common::wire_code_of(&scan_resp), "42P01");
}

#[test]
fn drop_table_undefined_table_is_42p01() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let resp = query(&session, br#"{"op":"drop_table","table":"nonexistent"}"#);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "got: {resp:?}");
}

// --- 権限（42501。存在オラクル非公開） -----------------------------------

#[test]
fn all_three_ddl_ops_reject_with_42501_without_ddl_permission() {
    let (core, _guard) = new_core_with_docs_table();
    let session = non_ddl_session(core);

    let cases: [&[u8]; 3] = [
        br#"{"op":"create_table","table":"new_table","columns":[{"name":"a","type":"text"}]}"#,
        br#"{"op":"alter_table","table":"docs","add_column":{"name":"note","type":"text"}}"#,
        br#"{"op":"drop_table","table":"docs"}"#,
    ];
    for body in cases {
        let resp = query(&session, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42501",
            "body={body:?} got: {resp:?}"
        );
    }
}

#[test]
fn permission_denial_is_byte_identical_regardless_of_table_existence() {
    // DDL 実行権限ゲートはカタログ照会より必ず先に判定する——権限の無い
    // 主体には対象テーブルの有無にかかわらず同一の応答を返す（存在オラクル
    // 非公開。security.md P0）。`Date` ヘッダを含む応答全体を比較する前に
    // `wire_code`／status のみ固定し、本文（`message`）も一致することを見る。
    let (core, _guard) = new_core_with_docs_table();
    let session = non_ddl_session(core);

    let existing = query(&session, br#"{"op":"drop_table","table":"docs"}"#);
    let missing = query(&session, br#"{"op":"drop_table","table":"does_not_exist"}"#);
    assert_eq!(existing.status, missing.status);
    assert_eq!(
        http_common::wire_code_of(&existing),
        http_common::wire_code_of(&missing)
    );
    assert_eq!(
        http_common::error_message_of(&existing),
        http_common::error_message_of(&missing)
    );
}

// --- 語彙外（NOSQL-13 対象外の DDL 相当） -------------------------------

#[test]
fn create_index_and_view_ddl_remain_unsupported_via_nosql_surface() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    for body in [
        &br#"{"op":"create_index","table":"docs"}"#[..],
        &br#"{"op":"drop_index","table":"docs"}"#[..],
        &br#"{"op":"create_view","table":"docs"}"#[..],
        &br#"{"op":"drop_view","table":"docs"}"#[..],
    ] {
        let resp = query(&session, body);
        assert_eq!(http_common::wire_code_of(&resp), "0A000", "got: {resp:?}");
    }
}

// --- untrusted 値の非 echo -------------------------------------------------
//
// `engine::sql::allowlist::SqlSurfaceError::undefined_table`（`42P01`）は
// SQL 表層と同じ設計判断で、切り詰め済みのテーブル名（クライアント自身が
// 送った識別子であり、他テナントの情報ではない）を文言に含める契約
// （`error_format.rs`「テーブル名は…エラーへ含める」参照）。そのため
// `create_table`／`alter_table`／`drop_table` の未定義テーブルエラーは
// テーブル名の非 echo 検査の対象外とし、代わりに権限拒否（`42501`。
// [`all_three_ddl_ops_reject_with_42501_without_ddl_permission`]・
// [`permission_denial_is_byte_identical_regardless_of_table_existence`]）が
// 固定文言のみを返すことで security.md の「存在情報を漏らさない」契約を
// 検証する。

#[test]
fn permission_denial_message_does_not_echo_untrusted_table_name() {
    let (core, _guard) = new_core();
    let session = non_ddl_session(core);
    let marker = "zzz_marker_value_zzz";
    let body = format!(r#"{{"op":"drop_table","table":"{marker}"}}"#);
    let resp = query(&session, body.as_bytes());
    assert_eq!(http_common::wire_code_of(&resp), "42501");
    http_common::assert_message_does_not_echo(&resp, marker);
}
// --- alter_table.add_column の NOT NULL／DEFAULT（Issue #1338） ------------

fn core_with_parity_tables() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql13-ddl-add-column-parity");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for name in ["docs_sql", "docs_nosql"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
    }
    storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("create_enum_type");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// SQL の `ALTER TABLE ... ADD COLUMN <decl>` と NoSQL の `add_column` JSON が
/// 同一の成否・`wire_code`・HTTP ステータスになることを固定する（TABLE-16・
/// NOSQL-13・ERR-4。制御文字を含む DEFAULT は意図した差分のため含めない）。
#[test]
fn sql_and_nosql_add_column_not_null_default_produce_identical_results() {
    let (core, _guard) = core_with_parity_tables();
    let ctx = PolicyContext::with_visibilities(
        TENANT_A,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant ctx");
    let mut sql_session = engine::sql::mode::SessionState::default();
    sql_session.allow_ddl();
    let nosql_session = ddl_session(Arc::clone(&core));

    let long = "x".repeat(70_000);
    let cases: Vec<(String, String)> = vec![
        (
            "a TEXT DEFAULT 'hi'".into(),
            r#"{"name":"a","type":"text","default":"hi"}"#.into(),
        ),
        (
            "b INTEGER NOT NULL DEFAULT 7".into(),
            r#"{"name":"b","type":"integer","not_null":true,"default":7}"#.into(),
        ),
        (
            "c BIGINT DEFAULT -5".into(),
            r#"{"name":"c","type":"bigint","default":-5}"#.into(),
        ),
        (
            "d REAL DEFAULT 1.5".into(),
            r#"{"name":"d","type":"real","default":1.5}"#.into(),
        ),
        (
            "e BOOLEAN NOT NULL DEFAULT true".into(),
            r#"{"name":"e","type":"boolean","not_null":true,"default":true}"#.into(),
        ),
        (
            "f DATE DEFAULT '2020-01-02'".into(),
            r#"{"name":"f","type":"date","default":"2020-01-02"}"#.into(),
        ),
        (
            "g TIMESTAMP DEFAULT '2020-01-01 00:00:00'".into(),
            r#"{"name":"g","type":"timestamp","default":"2020-01-01 00:00:00"}"#.into(),
        ),
        (
            "h UUID DEFAULT '00000000-0000-0000-0000-000000000001'".into(),
            r#"{"name":"h","type":"uuid","default":"00000000-0000-0000-0000-000000000001"}"#.into(),
        ),
        (
            "i mood NOT NULL DEFAULT 'happy'".into(),
            r#"{"name":"i","type":"enum","enum_type":"mood","not_null":true,"default":"happy"}"#
                .into(),
        ),
        (
            "j INTEGER DEFAULT 99999999999".into(),
            r#"{"name":"j","type":"integer","default":99999999999}"#.into(),
        ),
        (
            "k DATE DEFAULT 1".into(),
            r#"{"name":"k","type":"date","default":1}"#.into(),
        ),
        (
            "l DATE DEFAULT 'abc'".into(),
            r#"{"name":"l","type":"date","default":"abc"}"#.into(),
        ),
        (
            "m UUID DEFAULT 'abc'".into(),
            r#"{"name":"m","type":"uuid","default":"abc"}"#.into(),
        ),
        (
            "n mood DEFAULT 'angry'".into(),
            r#"{"name":"n","type":"enum","enum_type":"mood","default":"angry"}"#.into(),
        ),
        (
            "o TEXT DEFAULT 1".into(),
            r#"{"name":"o","type":"text","default":1}"#.into(),
        ),
        (
            "p BOOLEAN DEFAULT 'x'".into(),
            r#"{"name":"p","type":"boolean","default":"x"}"#.into(),
        ),
        (
            "q INTEGER NOT NULL".into(),
            r#"{"name":"q","type":"integer","not_null":true}"#.into(),
        ),
        (
            "r2 BYTEA DEFAULT '\\x01'".into(),
            r#"{"name":"r2","type":"bytea","default":"\\x01"}"#.into(),
        ),
        (
            "r BYTEA DEFAULT 'ab'".into(),
            r#"{"name":"r","type":"bytea","default":"ab"}"#.into(),
        ),
        (
            format!("s TEXT DEFAULT '{long}'"),
            format!(r#"{{"name":"s","type":"text","default":"{long}"}}"#),
        ),
        (
            "embedding TEXT NOT NULL DEFAULT 'x'".into(),
            r#"{"name":"embedding","type":"text","not_null":true,"default":"x"}"#.into(),
        ),
    ];

    for (decl, json) in &cases {
        let sql = core.execute_sql_in_session(
            &ctx,
            &mut sql_session,
            &format!("ALTER TABLE docs_sql ADD COLUMN {decl}"),
        );
        let body = format!(r#"{{"op":"alter_table","table":"docs_nosql","add_column":{json}}}"#);
        let resp = query(&nosql_session, body.as_bytes());
        let label: String = decl.chars().take(60).collect();
        match sql {
            Ok(_) => assert_eq!(resp.status, 200, "parity (ok) for {label}"),
            Err(err) => {
                assert_eq!(
                    http_common::wire_code_of(&resp),
                    err.wire_code(),
                    "parity (wire_code) for {label}"
                );
                assert_eq!(
                    resp.status,
                    wire_server::http::status::http_status(err.error_class()),
                    "parity (http status) for {label}"
                );
            }
        }
    }

    // 存在しないテーブル（NOT NULL＋DEFAULT 付き）も SQL と同じ 42P01。
    let resp = query(
        &nosql_session,
        br#"{"op":"alter_table","table":"nope","add_column":{"name":"z","type":"integer","not_null":true,"default":1}}"#,
    );
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "got: {resp:?}");
}

/// NoSQL 固有の形状エラー（`default` の null／配列／オブジェクト、`not_null` の
/// 型違い、`nullable` 等の未知キー、制御文字入り DEFAULT）は `42601`。
#[test]
fn alter_table_add_column_malformed_not_null_default_is_42601() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    for add in [
        r#"{"name":"a","type":"text","default":null}"#,
        r#"{"name":"a","type":"text","default":[1]}"#,
        r#"{"name":"a","type":"text","default":{"k":1}}"#,
        r#"{"name":"a","type":"text","not_null":"x"}"#,
        r#"{"name":"a","type":"text","not_null":null}"#,
        r#"{"name":"a","type":"text","nullable":true}"#,
        r#"{"name":"a","type":"text","default":"a\nb"}"#,
    ] {
        let body = format!(r#"{{"op":"alter_table","table":"docs","add_column":{add}}}"#);
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "for {add}: {resp:?}"
        );
        assert_eq!(resp.status, 400, "for {add}: {resp:?}");
    }
}

/// DDL 権限の無いセッションは `not_null`／`default` やテーブルの有無に関わらず
/// `42501`（権限ゲートは engine の単一実装・カタログ照会より前）。
#[test]
fn alter_table_add_column_not_null_default_without_ddl_permission_is_42501() {
    let (core, _guard) = new_core_with_docs_table();
    let session = non_ddl_session(core);
    let existing = query(
        &session,
        br#"{"op":"alter_table","table":"docs","add_column":{"name":"n","type":"integer","not_null":true,"default":1}}"#,
    );
    let missing = query(
        &session,
        br#"{"op":"alter_table","table":"nope","add_column":{"name":"n","type":"integer","not_null":true,"default":1}}"#,
    );
    assert_eq!(http_common::wire_code_of(&existing), "42501");
    assert_eq!(existing.body, missing.body);
}

/// DEFAULT のない NOT NULL は行の有無に依存しない構造判定で `42601`
/// （他テナントの行の存在オラクルにならない。テナント境界 P0）。
#[test]
fn alter_table_add_column_not_null_without_default_is_42601_regardless_of_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"alter_table","table":"docs","add_column":{"name":"n","type":"integer","not_null":true}}"#;
    let empty = query(&session, body);
    assert_eq!(http_common::wire_code_of(&empty), "42601", "got: {empty:?}");
    let ins = query(
        &session,
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2]}],"operation_id":"op-1338-a"}"#,
    );
    assert_eq!(ins.status, 200, "got: {ins:?}");
    let with_rows = query(&session, body);
    assert_eq!(with_rows.body, empty.body);
}

/// 追加後の観測: 既存行に DEFAULT が見え、NOT NULL 列への明示 null は 23502、
/// 列省略の insert には DEFAULT が適用される。
#[test]
fn alter_table_add_column_not_null_default_applies_to_rows_and_enforces_not_null() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let ins = query(
        &session,
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2]}],"operation_id":"op-1338-b"}"#,
    );
    assert_eq!(ins.status, 200, "got: {ins:?}");
    let alter = query(
        &session,
        br#"{"op":"alter_table","table":"docs","add_column":{"name":"n","type":"integer","not_null":true,"default":7}}"#,
    );
    assert_eq!(alter.status, 200, "got: {alter:?}");

    let scan = query(&session, br#"{"op":"scan","table":"docs","limit":10}"#);
    assert_eq!(scan.status, 200, "got: {scan:?}");
    let text = String::from_utf8_lossy(&scan.body).into_owned();
    assert!(
        text.contains(r#"[1,[0.1,0.2],7]"#),
        "existing row must show DEFAULT: {text}"
    );

    let null_ins = query(
        &session,
        br#"{"op":"insert","table":"docs","rows":[{"id":2,"embedding":[0.3,0.4],"n":null}],"operation_id":"op-1338-c"}"#,
    );
    assert_eq!(
        http_common::wire_code_of(&null_ins),
        "23502",
        "got: {null_ins:?}"
    );

    let omit_ins = query(
        &session,
        br#"{"op":"insert","table":"docs","rows":[{"id":3,"embedding":[0.5,0.6]}],"operation_id":"op-1338-d"}"#,
    );
    assert_eq!(omit_ins.status, 200, "got: {omit_ins:?}");
}
// --- 列型の SQL パリティ（Issue #1409・NOSQL-13・TABLE-6/13/14。create_table／
// add_column が SQL 表層と同じ型集合・同じカタログ表現になることを固定する） --

/// 同一宣言を SQL と NoSQL JSON の両方で書いた (列名, SQL 型, NoSQL 列 JSON 断片)。
/// 全スカラー型・`numeric`・ENUM（`mood`。事前に `CREATE TYPE` 済み）・配列。
const TYPE_PARITY_COLUMNS: &[(&str, &str, &str)] = &[
    ("c_text", "TEXT", r#""type":"text""#),
    ("c_int", "INTEGER", r#""type":"integer""#),
    ("c_big", "BIGINT", r#""type":"bigint""#),
    ("c_real", "REAL", r#""type":"real""#),
    ("c_dbl", "DOUBLE PRECISION", r#""type":"double""#),
    ("c_bool", "BOOLEAN", r#""type":"boolean""#),
    ("c_date", "DATE", r#""type":"date""#),
    ("c_ts", "TIMESTAMP", r#""type":"timestamp""#),
    ("c_bytea", "BYTEA", r#""type":"bytea""#),
    ("c_json", "JSON", r#""type":"json""#),
    ("c_jsonb", "JSONB", r#""type":"jsonb""#),
    ("c_uuid", "UUID", r#""type":"uuid""#),
    (
        "c_num",
        "NUMERIC(10,2)",
        r#""type":"numeric","precision":10,"scale":2"#,
    ),
    ("c_enum", "mood", r#""type":"enum","enum_type":"mood""#),
    (
        "a_text",
        "TEXT[]",
        r#""type":"array","element_type":"text""#,
    ),
    (
        "a_int",
        "INTEGER[4]",
        r#""type":"array","element_type":"integer","max_len":4"#,
    ),
    (
        "a_num",
        "NUMERIC(5,2)[]",
        r#""type":"array","element_type":"numeric","precision":5,"scale":2"#,
    ),
    (
        "a_enum",
        "mood[3]",
        r#""type":"array","element_type":"enum","enum_type":"mood","max_len":3"#,
    ),
    (
        "a_bytea",
        "BYTEA[]",
        r#""type":"array","element_type":"bytea""#,
    ),
    (
        "a_jsonb",
        "JSONB[2]",
        r#""type":"array","element_type":"jsonb","max_len":2"#,
    ),
];

fn parity_ctx() -> PolicyContext {
    PolicyContext::with_visibilities(
        TENANT_A,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant ctx")
}

/// `SELECT <列...> FROM <table> LIMIT 1` の列メタ（列ごとの `ColumnType`）を返す。
/// カタログに格納された型（解決済み ENUM 定義・配列の `max_len`・`Numeric{p,s}` を含む）
/// を SQL 表層の結果メタ越しに比較するための入口。
fn column_types_of(
    core: &EngineCore,
    session: &mut engine::sql::mode::SessionState,
    table: &str,
    columns: &[&str],
) -> Vec<(String, ColumnType)> {
    use engine::sql::exec::ColumnMeta;
    use engine::sql::SqlOutcome;
    let sql = format!("SELECT {} FROM {table} LIMIT 1", columns.join(", "));
    let outcome = core
        .execute_sql_in_session(&parity_ctx(), session, &sql)
        .unwrap_or_else(|e| panic!("select on {table} must succeed: {e:?}"));
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected SqlOutcome::Query for {sql:?}");
    };
    result
        .columns
        .into_iter()
        .map(|c| match c {
            ColumnMeta::Scalar { name, ty } => (name, ty),
            other => panic!("expected scalar column meta, got {other:?}"),
        })
        .collect()
}

fn parity_core_with_enum() -> (
    Arc<EngineCore>,
    temp_db::CleanupGuard,
    engine::sql::mode::SessionState,
) {
    let (core, guard) = new_core();
    let mut sql_session = engine::sql::mode::SessionState::default();
    sql_session.allow_ddl();
    core.execute_sql_in_session(
        &parity_ctx(),
        &mut sql_session,
        "CREATE TYPE mood AS ENUM ('happy', 'sad')",
    )
    .expect("CREATE TYPE must succeed");
    (core, guard, sql_session)
}

#[test]
fn nosql_and_sql_create_table_produce_identical_column_types() {
    let (core, _guard, mut sql_session) = parity_core_with_enum();
    let sql_cols: Vec<String> = TYPE_PARITY_COLUMNS
        .iter()
        .map(|(n, t, _)| format!("{n} {t}"))
        .collect();
    core.execute_sql_in_session(
        &parity_ctx(),
        &mut sql_session,
        &format!("CREATE TABLE t_sql ({})", sql_cols.join(", ")),
    )
    .expect("sql CREATE TABLE must succeed");

    let nosql_cols: Vec<String> = TYPE_PARITY_COLUMNS
        .iter()
        .map(|(n, _, j)| format!(r#"{{"name":"{n}",{j}}}"#))
        .collect();
    let body = format!(
        r#"{{"op":"create_table","table":"t_nosql","columns":[{}]}}"#,
        nosql_cols.join(",")
    );
    let session = ddl_session(Arc::clone(&core));
    let resp = query(&session, body.as_bytes());
    assert_eq!(resp.status, 200, "got: {resp:?}");

    let names: Vec<&str> = TYPE_PARITY_COLUMNS.iter().map(|(n, _, _)| *n).collect();
    let sql_types = column_types_of(&core, &mut sql_session, "t_sql", &names);
    let nosql_types = column_types_of(&core, &mut sql_session, "t_nosql", &names);
    assert_eq!(sql_types.len(), names.len());
    assert_eq!(sql_types, nosql_types, "catalog column types must match");
}

#[test]
fn nosql_and_sql_add_column_produce_identical_column_types() {
    let (core, _guard, mut sql_session) = parity_core_with_enum();
    for t in ["b_sql", "b_nosql"] {
        core.execute_sql_in_session(
            &parity_ctx(),
            &mut sql_session,
            &format!("CREATE TABLE {t} (base TEXT)"),
        )
        .expect("fixture CREATE TABLE must succeed");
    }
    let session = ddl_session(Arc::clone(&core));
    for (name, sql_ty, json) in TYPE_PARITY_COLUMNS {
        core.execute_sql_in_session(
            &parity_ctx(),
            &mut sql_session,
            &format!("ALTER TABLE b_sql ADD COLUMN {name} {sql_ty}"),
        )
        .unwrap_or_else(|e| panic!("sql ADD COLUMN {name} must succeed: {e:?}"));
        let body = format!(
            r#"{{"op":"alter_table","table":"b_nosql","add_column":{{"name":"{name}",{json}}}}}"#
        );
        let resp = query(&session, body.as_bytes());
        assert_eq!(resp.status, 200, "add_column {name} got: {resp:?}");
    }
    let names: Vec<&str> = TYPE_PARITY_COLUMNS.iter().map(|(n, _, _)| *n).collect();
    let sql_types = column_types_of(&core, &mut sql_session, "b_sql", &names);
    let nosql_types = column_types_of(&core, &mut sql_session, "b_nosql", &names);
    assert_eq!(sql_types, nosql_types, "catalog column types must match");
}

#[test]
fn create_table_array_max_len_is_enforced_on_insert_like_sql() {
    let (core, _guard, mut sql_session) = parity_core_with_enum();
    core.execute_sql_in_session(
        &parity_ctx(),
        &mut sql_session,
        "CREATE TABLE lim_sql (tags TEXT[2])",
    )
    .expect("sql CREATE TABLE must succeed");
    let session = ddl_session(Arc::clone(&core));
    let create = br#"{"op":"create_table","table":"lim_nosql","columns":[
        {"name":"tags","type":"array","element_type":"text","max_len":2}]}"#;
    assert_eq!(query(&session, create).status, 200);

    for table in ["lim_sql", "lim_nosql"] {
        let ok = format!(
            r#"{{"op":"insert","table":"{table}","rows":[{{"id":1,"tags":["a","b"]}}],"operation_id":"ok-{table}"}}"#
        );
        let resp = query(&session, ok.as_bytes());
        assert_eq!(resp.status, 200, "{table} at limit: {resp:?}");
        let over = format!(
            r#"{{"op":"insert","table":"{table}","rows":[{{"id":2,"tags":["a","b","c"]}}],"operation_id":"over-{table}"}}"#
        );
        let resp = query(&session, over.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "54000",
            "{table} over limit: {resp:?}"
        );
        assert_eq!(resp.status, 413, "{table} over limit: {resp:?}");
    }
}

#[test]
fn create_table_new_types_error_classification_matches_sql() {
    let (core, _guard) = new_core();
    let session = ddl_session(Arc::clone(&core));
    let mut sql_session = engine::sql::mode::SessionState::default();
    sql_session.allow_ddl();
    // (SQL 列宣言, NoSQL 列 JSON)。いずれも engine 側で拒否され同じ `wire_code` になる。
    let cases = [
        (
            "c TEXT[0]",
            r#"{"name":"c","type":"array","element_type":"text","max_len":0}"#,
        ),
        (
            "c TEXT[1025]",
            r#"{"name":"c","type":"array","element_type":"text","max_len":1025}"#,
        ),
        (
            "c VECTOR(3)[]",
            r#"{"name":"c","type":"array","element_type":"vector","dim":3}"#,
        ),
        // 未登録の ENUM 型名
        (
            "c nosuch",
            r#"{"name":"c","type":"enum","enum_type":"nosuch"}"#,
        ),
        (
            "c nosuch[]",
            r#"{"name":"c","type":"array","element_type":"enum","enum_type":"nosuch"}"#,
        ),
    ];
    for (i, (sql_decl, json)) in cases.iter().enumerate() {
        let sql_err = core
            .execute_sql_in_session(
                &parity_ctx(),
                &mut sql_session,
                &format!("CREATE TABLE e_sql_{i} ({sql_decl})"),
            )
            .expect_err("sql must reject");
        let body = format!(r#"{{"op":"create_table","table":"e_nosql_{i}","columns":[{json}]}}"#);
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            ClassifiedError::wire_code(&sql_err),
            "nosql {json}: {resp:?}"
        );
        assert_eq!(
            ClassifiedError::wire_code(&sql_err),
            "42601",
            "sql {sql_decl}"
        );
    }
}

#[test]
fn create_table_new_types_without_ddl_permission_is_42501() {
    let (core, _guard) = new_core();
    let session = non_ddl_session(core);
    for body in [
        r#"{"op":"create_table","table":"x","columns":[{"name":"c","type":"array","element_type":"text","max_len":2}]}"#,
        r#"{"op":"create_table","table":"x","columns":[{"name":"c","type":"boolean","default":true}]}"#,
        r#"{"op":"alter_table","table":"x","add_column":{"name":"c","type":"array","element_type":"integer"}}"#,
    ] {
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42501",
            "body={body} got: {resp:?}"
        );
    }
}
