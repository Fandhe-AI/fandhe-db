//! `POST /v1/query`（`op: "create_index"｜"drop_index"`）を production ルータ（生バイト
//! クライアント）経由で固定する層 A 結合テスト（Issue #2108・NOSQL-19。ポインタ:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-19・NOSQL-1・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・INDEX-7・INDEX-8・HTTP-15）。
//!
//! 観点は (1) SQL 表層の `CREATE INDEX`／`DROP INDEX` と成功時の観測結果・失敗時の
//! `wire_code` が一致すること、(2) DDL 権限のない主体は表・索引の有無に関わらず同一の
//! `42501` になること、(3) セッションスコープ（HTTP-15）の fail-closed、(4) スキーマ検証。
//! トークン列写像の境界値は `src/http/query/ddl.rs` の単体テストが担う。

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::sync::Arc;

use engine::core::EngineCore;
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

/// docs（lang・emb VECTOR(2)・meta JSONB）・dh（hv HALFVEC(2)）・view v・既存索引 ix_exists を持つ core。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql19-index-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let (ctx, mut sql) = sql_ddl_session();
    for ddl in [
        "CREATE TABLE docs (lang TEXT, emb VECTOR(2), meta JSONB)",
        "CREATE TABLE dh (hv HALFVEC(2))",
        "CREATE VIEW v AS SELECT lang FROM docs",
        "CREATE INDEX ix_exists ON docs (lang)",
    ] {
        core.execute_sql_in_session_legacy(&ctx, &mut sql, ddl)
            .expect("fixture DDL must succeed");
    }
    (Arc::new(core), guard)
}

fn sql_ddl_session() -> (PolicyContext, engine::sql::mode::SessionState) {
    let ctx = PolicyContext::with_visibilities(
        TENANT_A,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant ctx");
    let mut st = engine::sql::mode::SessionState::default();
    st.allow_ddl();
    (ctx, st)
}

fn spawn_with_store(store: UserStore, engine: Arc<EngineCore>) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
    let router = wire_server::http::router::Router::with_engine(
        Arc::new(store),
        SessionStore::new(),
        engine,
    );
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

/// `scope_json` は `"scope":{...}` 片（空文字ならスコープなし）。
fn login(addr: std::net::SocketAddr, user: &str, scope_json: &str) -> Session {
    let extra = if scope_json.is_empty() {
        String::new()
    } else {
        format!(",{scope_json}")
    };
    let body = format!(r#"{{"user":"{user}","password":"pw-{user}"{extra}}}"#).into_bytes();
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

fn query_with(session: &Session, body: &[u8], extra_headers: &[(&str, &str)]) -> HttpResponse {
    let auth = format!("Bearer {}", session.token);
    let len = body.len().to_string();
    let mut headers = vec![
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
        ("Content-Length", len.as_str()),
    ];
    headers.extend_from_slice(extra_headers);
    let request = http_common::build_request("/v1/query", &headers, body);
    http_common::parse_single_response(&http_common::send_raw(
        session.addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn query(session: &Session, body: &str) -> HttpResponse {
    query_with(session, body.as_bytes(), &[])
}

/// alice（tenant-a・DDL 許可）・bob（tenant-b・DDL 不許可）・carol（tenant-a・DDL 不許可）。
fn spawn_server(core: Arc<EngineCore>) -> std::net::SocketAddr {
    let users_path = common::write_user_store_file(&[
        ("alice", TENANT_A, "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", TENANT_A, "pw-carol"),
    ]);
    let store = UserStore::load_from_file(&users_path)
        .expect("valid user store")
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("alice is a known username");
    spawn_with_store(store, core)
}

fn assert_ok(resp: &HttpResponse) {
    assert_eq!(resp.status, 200, "got: {resp:?}");
    assert_eq!(String::from_utf8_lossy(&resp.body).trim(), r#"{"ok":true}"#);
}

fn sql(core: &EngineCore, stmt: &str) -> Result<(), String> {
    let (ctx, mut st) = sql_ddl_session();
    core.execute_sql_in_session_legacy(&ctx, &mut st, stmt)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
}

// --- SQL とのパリティ（成功） -------------------------------------------

#[test]
fn created_index_is_visible_to_sql_surface_for_every_shape() {
    // (NoSQL 本文, 同名で SQL から作り直したとき 42P07 になること, DROP INDEX は SQL から成功する)
    let shapes = [
        r#"{"op":"create_index","name":"n_btree","table":"docs","columns":["lang"]}"#,
        r#"{"op":"create_index","name":"n_hnsw","table":"docs","columns":["emb"],"method":"hnsw","opclass":"vector_cosine_ops","with":{"m":16,"ef_construction":64}}"#,
        r#"{"op":"create_index","name":"n_gin","table":"docs","columns":["meta"],"method":"gin","opclass":"jsonb_path_ops"}"#,
    ];
    for body in shapes {
        let (core, _guard) = new_core();
        let addr = spawn_server(Arc::clone(&core));
        let alice = login(addr, "alice", "");
        assert_ok(&query(&alice, body));
        let name = body
            .split("\"name\":\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("name");
        assert_eq!(
            sql(&core, &format!("CREATE INDEX {name} ON docs (lang)")),
            Err("42P07".to_string()),
            "{body}"
        );
        assert_eq!(sql(&core, &format!("DROP INDEX {name}")), Ok(()), "{body}");
    }
}

#[test]
fn index_created_by_sql_can_be_dropped_via_nosql_and_vice_versa() {
    let (core, _guard) = new_core();
    let addr = spawn_server(Arc::clone(&core));
    let alice = login(addr, "alice", "");
    assert_eq!(sql(&core, "CREATE INDEX from_sql ON docs (lang)"), Ok(()));
    assert_ok(&query(&alice, r#"{"op":"drop_index","name":"from_sql"}"#));
    // 二重削除は SQL と同じ 42704
    let again = query(&alice, r#"{"op":"drop_index","name":"from_sql"}"#);
    assert_eq!(http_common::wire_code_of(&again), "42704");
    assert_eq!(sql(&core, "DROP INDEX from_sql"), Err("42704".to_string()));

    assert_ok(&query(
        &alice,
        r#"{"op":"create_index","name":"from_nosql","table":"docs","columns":["lang"]}"#,
    ));
    assert_eq!(sql(&core, "DROP INDEX from_nosql"), Ok(()));
    // SQL で消した名前は NoSQL から再作成できる
    assert_ok(&query(
        &alice,
        r#"{"op":"create_index","name":"from_nosql","table":"docs","columns":["lang"]}"#,
    ));
}

// --- SQL とのパリティ（失敗の wire_code） ---------------------------------

#[test]
fn failure_wire_codes_match_sql_surface() {
    // (ラベル, NoSQL 本文, 同等の SQL, 期待 wire_code)
    let cases = [
        (
            "missing table",
            r#"{"op":"create_index","name":"n","table":"nope","columns":["lang"]}"#,
            "CREATE INDEX n ON nope (lang)",
            "42P01",
        ),
        (
            "missing column",
            r#"{"op":"create_index","name":"n","table":"docs","columns":["nope"]}"#,
            "CREATE INDEX n ON docs (nope)",
            "42703",
        ),
        (
            "name collision",
            r#"{"op":"create_index","name":"ix_exists","table":"docs","columns":["lang"]}"#,
            "CREATE INDEX ix_exists ON docs (lang)",
            "42P07",
        ),
        (
            "target is a view",
            r#"{"op":"create_index","name":"n","table":"v","columns":["lang"]}"#,
            "CREATE INDEX n ON v (lang)",
            "42809",
        ),
        (
            "drop missing",
            r#"{"op":"drop_index","name":"nope"}"#,
            "DROP INDEX nope",
            "42704",
        ),
        (
            "opclass does not fit method",
            r#"{"op":"create_index","name":"n","table":"docs","columns":["emb"],"method":"hnsw","opclass":"jsonb_ops"}"#,
            "CREATE INDEX n ON docs USING hnsw (emb jsonb_ops)",
            "42704",
        ),
        (
            "hnsw with two columns",
            r#"{"op":"create_index","name":"n","table":"docs","columns":["emb","lang"],"method":"hnsw"}"#,
            "CREATE INDEX n ON docs USING hnsw (emb, lang)",
            "0A000",
        ),
        (
            "with on non-hnsw",
            r#"{"op":"create_index","name":"n","table":"docs","columns":["meta"],"method":"gin","with":{"m":16}}"#,
            "CREATE INDEX n ON docs USING gin (meta) WITH (m = 16)",
            "0A000",
        ),
        (
            "m out of range",
            r#"{"op":"create_index","name":"n","table":"docs","columns":["emb"],"method":"hnsw","with":{"m":1000000}}"#,
            "CREATE INDEX n ON docs USING hnsw (emb) WITH (m = 1000000)",
            "22023",
        ),
        (
            "ef_construction below 2*m",
            r#"{"op":"create_index","name":"n","table":"docs","columns":["emb"],"method":"hnsw","with":{"m":16,"ef_construction":8}}"#,
            "CREATE INDEX n ON docs USING hnsw (emb) WITH (m = 16, ef_construction = 8)",
            "22023",
        ),
        (
            "vector opclass on halfvec column",
            r#"{"op":"create_index","name":"n","table":"dh","columns":["hv"],"method":"hnsw","opclass":"vector_cosine_ops"}"#,
            "CREATE INDEX n ON dh USING hnsw (hv vector_cosine_ops)",
            "42804",
        ),
    ];
    let (core, _guard) = new_core();
    let addr = spawn_server(Arc::clone(&core));
    let alice = login(addr, "alice", "");
    for (label, body, stmt, expected) in cases {
        let resp = query(&alice, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            expected,
            "case {label}: nosql {resp:?}"
        );
        http_common::assert_message_does_not_echo(&resp, TENANT_A);
        assert_eq!(
            sql(&core, stmt),
            Err(expected.to_string()),
            "case {label}: sql parity"
        );
    }
    // 失敗は副作用を残さない（同名で作り直せる）。
    assert_ok(&query(
        &alice,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"]}"#,
    ));
}

#[test]
fn wire_only_rejections_are_42601_without_side_effects() {
    let (core, _guard) = new_core();
    let addr = spawn_server(Arc::clone(&core));
    let alice = login(addr, "alice", "");
    for body in [
        // opclass の語彙外（SQL 表層では 42704。wire で固定語彙に絞るため 42601）
        r#"{"op":"create_index","name":"n","table":"docs","columns":["emb"],"method":"hnsw","opclass":"VECTOR_COSINE_OPS"}"#,
        r#"{"op":"create_index","name":"n","table":"dh","columns":["hv"],"method":"hnsw","opclass":"halfvec_cosine_ops"}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["emb"],"method":"ivfflat"}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":[]}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang","emb"],"opclass":"jsonb_ops"}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["emb"],"method":"hnsw","with":{}}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["emb"],"method":"hnsw","with":{"m":-1}}"#,
        r#"{"op":"create_index","name":"bad-name","table":"docs","columns":["lang"]}"#,
    ] {
        let resp = query(&alice, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "{body}: {resp:?}"
        );
        http_common::assert_message_does_not_echo(&resp, TENANT_A);
    }
    assert_ok(&query(
        &alice,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"]}"#,
    ));
}

// --- 権限 ---------------------------------------------------------------

#[test]
fn permission_denial_is_42501_and_identical_regardless_of_existence() {
    let (core, _guard) = new_core();
    let addr = spawn_server(Arc::clone(&core));
    let carol = login(addr, "carol", "");
    let pairs = [
        (
            r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"]}"#,
            r#"{"op":"create_index","name":"n","table":"nope","columns":["lang"]}"#,
        ),
        (
            r#"{"op":"create_index","name":"ix_exists","table":"docs","columns":["lang"]}"#,
            r#"{"op":"create_index","name":"ix_none","table":"docs","columns":["nope"]}"#,
        ),
        (
            r#"{"op":"drop_index","name":"ix_exists"}"#,
            r#"{"op":"drop_index","name":"ix_none"}"#,
        ),
    ];
    for (a, b) in pairs {
        let ra = query(&carol, a);
        let rb = query(&carol, b);
        assert_eq!(ra.status, 403, "{a}: {ra:?}");
        assert_eq!(rb.status, 403, "{b}: {rb:?}");
        assert_eq!(http_common::wire_code_of(&ra), "42501");
        assert_eq!(http_common::wire_code_of(&rb), "42501");
        assert_eq!(
            http_common::error_message_of(&ra),
            http_common::error_message_of(&rb)
        );
    }
    // 拒否は副作用を持たない（既存の索引は残る）。
    assert_eq!(
        sql(&core, "CREATE INDEX ix_exists ON docs (lang)"),
        Err("42P07".to_string())
    );
    // SQL 表層で DDL 権限のない主体も同じ 42501。
    let ctx = PolicyContext::with_visibilities(TENANT_A, [engine::storage::Visibility::Public])
        .expect("ctx");
    let mut no_ddl = engine::sql::mode::SessionState::default();
    let err = core
        .execute_sql_in_session_legacy(&ctx, &mut no_ddl, "DROP INDEX ix_exists")
        .expect_err("sql must deny");
    assert_eq!(err.wire_code(), "42501");
}

// --- スコープ（HTTP-15） --------------------------------------------------

#[test]
fn read_only_scope_denies_both_ops() {
    let (core, _guard) = new_core();
    let addr = spawn_server(core);
    let ro = login(addr, "alice", r#""scope":{"read_only":true}"#);
    for body in [
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"]}"#,
        r#"{"op":"drop_index","name":"ix_exists"}"#,
    ] {
        let r = query(&ro, body);
        assert_eq!(r.status, 403, "{body}: {r:?}");
        assert_eq!(http_common::wire_code_of(&r), "42501");
    }
}

#[test]
fn ops_only_scope_allows_drop_index() {
    let (core, _guard) = new_core();
    let addr = spawn_server(core);
    let s = login(addr, "alice", r#""scope":{"ops":["drop_index"]}"#);
    assert_ok(&query(&s, r#"{"op":"drop_index","name":"ix_exists"}"#));
    // ops 外の create_index は拒否
    let r = query(
        &s,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"]}"#,
    );
    assert_eq!(http_common::wire_code_of(&r), "42501");
}

#[test]
fn tables_scope_denies_drop_index_always_and_checks_create_index_table() {
    let (core, _guard) = new_core();
    let addr = spawn_server(core);
    let s = login(addr, "alice", r#""scope":{"tables":["docs"]}"#);
    // 索引の有無に関わらず同一の 42501（対象表を特定できないため fail-closed）。
    let existing = query(&s, r#"{"op":"drop_index","name":"ix_exists"}"#);
    let missing = query(&s, r#"{"op":"drop_index","name":"ix_none"}"#);
    assert_eq!(existing.status, 403, "{existing:?}");
    assert_eq!(http_common::wire_code_of(&existing), "42501");
    assert_eq!(
        http_common::error_message_of(&existing),
        http_common::error_message_of(&missing)
    );
    assert_eq!(http_common::wire_code_of(&missing), "42501");
    // 範囲内の表への create_index は通り、範囲外は 42501。
    assert_ok(&query(
        &s,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"]}"#,
    ));
    let out = query(
        &s,
        r#"{"op":"create_index","name":"n2","table":"v","columns":["lang"]}"#,
    );
    assert_eq!(http_common::wire_code_of(&out), "42501");
}

// --- テナント境界 -----------------------------------------------------------

#[test]
fn index_ddl_does_not_expose_other_tenant_rows() {
    let (core, _guard) = new_core();
    let addr = spawn_server(Arc::clone(&core));
    let alice = login(addr, "alice", "");
    let bob = login(addr, "bob", "");
    let inserted = query(
        &alice,
        r#"{"op":"insert","table":"docs","rows":[{"id":1,"lang":"ja"}],"operation_id":"t19-1"}"#,
    );
    assert_eq!(inserted.status, 200, "{inserted:?}");
    assert_ok(&query(
        &alice,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"]}"#,
    ));
    let scan = r#"{"op":"scan","table":"docs","limit":10}"#;
    let rows_of = |resp: &HttpResponse| -> usize {
        assert_eq!(resp.status, 200, "{resp:?}");
        match parse_json(&String::from_utf8_lossy(&resp.body)).expect("json") {
            JsonValue::Object(mut o) => match o.remove("rows") {
                Some(JsonValue::Array(r)) => r.len(),
                other => panic!("rows: {other:?}"),
            },
            other => panic!("object: {other:?}"),
        }
    };
    assert_eq!(rows_of(&query(&alice, scan)), 1);
    assert_eq!(rows_of(&query(&bob, scan)), 0);
    assert_ok(&query(&alice, r#"{"op":"drop_index","name":"n"}"#));
    assert_eq!(rows_of(&query(&bob, scan)), 0);
}

// --- スキーマ ---------------------------------------------------------------

#[test]
fn unknown_keys_type_mismatches_and_idempotency_key_are_42601() {
    let (core, _guard) = new_core();
    let addr = spawn_server(Arc::clone(&core));
    let alice = login(addr, "alice", "");
    for body in [
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"],"unique":true}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"],"if_not_exists":true}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"],"where":[]}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"],"tenant_id":"x"}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang",1]}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"],"with":{"m":1.5}}"#,
        r#"{"op":"create_index","name":"n","table":"docs","columns":["lang"],"with":{"bogus":1}}"#,
        r#"{"op":"create_index","name":"n","table":"docs"}"#,
        r#"{"op":"drop_index","name":"ix_exists","table":"docs"}"#,
        r#"{"op":"drop_index","name":"ix_exists","if_exists":true}"#,
        r#"{"op":"drop_index"}"#,
    ] {
        let resp = query(&alice, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "{body}: {resp:?}"
        );
    }
    let with_key = query_with(
        &alice,
        br#"{"op":"drop_index","name":"ix_exists"}"#,
        &[("Idempotency-Key", "k-1")],
    );
    assert_eq!(
        http_common::wire_code_of(&with_key),
        "42601",
        "{with_key:?}"
    );
    // 拒否された要求は索引を消していない。
    assert_eq!(
        sql(&core, "CREATE INDEX ix_exists ON docs (lang)"),
        Err("42P07".to_string())
    );
}
