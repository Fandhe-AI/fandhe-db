//! `POST /v1/query`（`op: "search"`／`"scan"`／`"aggregate"`）の `filter` が、`ARRAY`／`JSON`／
//! `JSONB` 列の `in`（`not`・`or` 経由を含む）を SQL 表層の `SELECT ... WHERE` と同じ結果集合で
//! 受理し、型不一致は `42601`・上限超過は `54000` で拒否することを production ルータ経由で
//! 固定する層 A 結合テスト（Issue #1429。対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-14・NOSQL-17、
//! `docs/spec/04-behavior/sql-surface.md` SQL-24）。
//!
//! 役割分担: 述語形 `update`／`delete` 側は `nosql12_predicate_dml_composite_filter.rs`
//! （Issue #1410）が担い、本ファイルは同じ写像（`filter.rs` の `declare_in`）を共有する
//! 検索系 3 op の結果集合・テナント境界・エラー分類を固定する。
#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "items";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("text[] type")),
                true,
            ),
            ColumnDef::new(
                "ints",
                ColumnType::Array(ArrayType::new(ArrayElemType::Integer, 4).expect("int[] type")),
                true,
            ),
            ColumnDef::new("doc", ColumnType::Jsonb, true),
            ColumnDef::new("j", ColumnType::Json, true),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql14-query-composite-in");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

struct Both {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn login(http_addr: std::net::SocketAddr, user: &str, password: &str) -> String {
    let login_body = format!(r#"{{"user":"{user}","password":"{password}"}}"#).into_bytes();
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        &login_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

/// HTTP（alice／tenant-a と bob／tenant-b）・SQL wire（alice）を同一 `core` 上で起動する。
fn spawn_both(core: Arc<EngineCore>) -> (Both, Both, std::net::TcpStream) {
    let users_path = common::write_user_store_file(&[
        ("alice", TENANT_A, "pw-alice"),
        ("bob", TENANT_B, "pw-bob"),
    ]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let alice = Both {
        http_addr,
        token: login(http_addr, "alice", "pw-alice"),
    };
    let bob = Both {
        http_addr,
        token: login(http_addr, "bob", "pw-bob"),
    };
    let sql_addr = common::spawn_server_with_engine(&users_path, core);
    let sql_stream = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");
    (alice, bob, sql_stream)
}

fn query(who: &Both, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", who.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        who.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// seed 4 行（id 4 は tags・ints・doc・j が NULL で、述語は UNKNOWN として除外される）。
fn seed(who: &Both, tag: &str) {
    let rows = [
        r#"{"id":1,"embedding":[0.1,0.2,0.3],"lang":"ja","tags":["a"],"ints":[1,2],"doc":{"k":1},"j":{"k":1}}"#,
        r#"{"id":2,"embedding":[0.1,0.2,0.3],"lang":"en","tags":["b","c"],"ints":[3],"doc":[1,2],"j":[1,2]}"#,
        r#"{"id":3,"embedding":[0.1,0.2,0.3],"lang":"fr","tags":["a","b"],"ints":[],"doc":{"k":2},"j":{"k":2}}"#,
        r#"{"id":4,"embedding":[0.1,0.2,0.3],"lang":"de"}"#,
    ];
    for (i, row) in rows.iter().enumerate() {
        let body = format!(
            r#"{{"op":"insert","table":"{TABLE}","rows":[{row}],"operation_id":"seed-{tag}-{i}"}}"#
        );
        let resp = query(who, body.as_bytes());
        assert_eq!(resp.status, 200, "seed {i}: {resp:?}");
    }
}

const LANGS: [&str; 4] = ["ja", "en", "fr", "de"];

/// (SQL の WHERE 句, NoSQL の filter JSON)。
const CASES: [(&str, &str); 9] = [
    (
        r#"tags IN ('{"a"}', '{"b","c"}')"#,
        r#"[{"column":"tags","op":"in","value":[["a"],["b","c"]]}]"#,
    ),
    (
        r#"NOT tags IN ('{"a"}', '{"b","c"}')"#,
        r#"[{"not":{"column":"tags","op":"in","value":[["a"],["b","c"]]}}]"#,
    ),
    (
        "ints IN ('{1,2}', '{3}')",
        r#"[{"column":"ints","op":"in","value":[[1,2],[3]]}]"#,
    ),
    (
        r#"doc IN ('{"k":1}', '[1,2]')"#,
        r#"[{"column":"doc","op":"in","value":[{"k":1},[1,2]]}]"#,
    ),
    (
        r#"NOT doc IN ('{"k":1}', '[1,2]')"#,
        r#"[{"not":{"column":"doc","op":"in","value":[{"k":1},[1,2]]}}]"#,
    ),
    (
        r#"j IN ('{"k":2}', '[1,2]')"#,
        r#"[{"column":"j","op":"in","value":[{"k":2},[1,2]]}]"#,
    ),
    (
        r#"tags IN ('{"a"}') OR doc IN ('{"k":2}')"#,
        r#"[{"or":[{"column":"tags","op":"in","value":[["a"]]},{"column":"doc","op":"in","value":[{"k":2}]}]}]"#,
    ),
    (
        r#"tags IN ('{"a"}', '{"a","b"}') AND lang = 'fr'"#,
        r#"[{"column":"tags","op":"in","value":[["a"],["a","b"]]},{"column":"lang","op":"eq","value":"fr"}]"#,
    ),
    (
        r#"tags IN ('{"zzz"}')"#,
        r#"[{"column":"tags","op":"in","value":[["zzz"]]}]"#,
    ),
];

fn langs_in(text: &str, pattern: impl Fn(&str) -> String) -> Vec<&'static str> {
    LANGS
        .iter()
        .copied()
        .filter(|l| text.contains(&pattern(l)))
        .collect()
}

/// SQL 表層（`tenant` の ctx）で `SELECT lang ... WHERE` を実行し、含まれる lang を返す。
fn sql_langs(core: &EngineCore, tenant: &str, where_clause: &str) -> Vec<&'static str> {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid ctx");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("SELECT lang FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        )
        .expect("sql ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    let text = format!(
        "{:?}",
        result.rows.iter().map(|r| &r.cells).collect::<Vec<_>>()
    );
    langs_in(&text, |l| format!("\"{l}\""))
}

fn scan_body(filter: &str) -> Vec<u8> {
    format!(r#"{{"op":"scan","table":"{TABLE}","columns":["lang"],"filter":{filter},"limit":100}}"#)
        .into_bytes()
}

fn search_body(filter: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"search","table":"{TABLE}","vector":[0.1,0.2,0.3],"columns":["lang"],"filter":{filter},"limit":100}}"#
    )
    .into_bytes()
}

fn aggregate_body(filter: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"aggregate","table":"{TABLE}","aggregates":[{{"fn":"count","column":"*"}}],"filter":{filter}}}"#
    )
    .into_bytes()
}

fn http_langs(resp: &HttpResponse, what: &str) -> Vec<&'static str> {
    assert_eq!(resp.status, 200, "{what}: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    langs_in(&text, |l| format!("[\"{l}\"]"))
}

/// 3 op（scan／search／aggregate）の結果集合が SQL の同じ述語と一致する。
#[test]
fn composite_in_parity_with_sql_on_scan_search_aggregate() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core.clone());
    seed(&alice, "p");
    for (where_clause, filter) in CASES.iter() {
        let expected = sql_langs(&core, TENANT_A, where_clause);
        let scan = http_langs(&query(&alice, &scan_body(filter)), "scan");
        assert_eq!(scan, expected, "scan: {where_clause}");
        let search = http_langs(&query(&alice, &search_body(filter)), "search");
        assert_eq!(search, expected, "search: {where_clause}");
        let agg = query(&alice, &aggregate_body(filter));
        assert_eq!(agg.status, 200, "aggregate: {where_clause}: {agg:?}");
        let text = String::from_utf8_lossy(&agg.body).into_owned();
        assert!(
            text.contains(&format!("[[{}]]", expected.len())),
            "aggregate count {} for {where_clause}: {text}",
            expected.len()
        );
    }
}

/// テナント境界: `not in` を含めても自テナントの行だけが返り・数えられる。
#[test]
fn composite_in_does_not_cross_tenant_boundary() {
    let (core, _guard) = new_core();
    let (alice, bob, _sql) = spawn_both(core);
    seed(&alice, "ta");
    seed(&bob, "tb");
    let filter = r#"[{"not":{"column":"tags","op":"in","value":[["zzz"]]}}]"#;
    // tags が非 NULL の 3 行だけ（NULL 行は UNKNOWN で除外）。他テナント分が混ざれば 6 になる。
    for body in [scan_body(filter), search_body(filter)] {
        let text = String::from_utf8_lossy(&query(&alice, &body).body).into_owned();
        assert!(text.contains("\"row_count\":3"), "{text}");
    }
    let agg = query(&alice, &aggregate_body(filter));
    let text = String::from_utf8_lossy(&agg.body).into_owned();
    assert!(text.contains("[[3]]"), "{text}");
}

/// 型不一致は `42601`、上限超過は `54000`（scan・search 双方）。
#[test]
fn composite_in_type_mismatch_is_42601_and_limits_are_54000() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core);
    seed(&alice, "e");
    let many = format!(
        r#"[{{"column":"doc","op":"in","value":[{}]}}]"#,
        vec!["{}"; 257].join(",")
    );
    let cases: [(&str, &str); 6] = [
        (r#"[{"column":"tags","op":"in","value":["{a}"]}]"#, "42601"),
        (r#"[{"column":"doc","op":"in","value":[1]}]"#, "42601"),
        (r#"[{"column":"lang","op":"in","value":[["a"]]}]"#, "42601"),
        (r#"[{"column":"tags","op":"in","value":[null]}]"#, "42601"),
        (
            r#"[{"column":"tags","op":"in","value":[["a","b","c","d","e"]]}]"#,
            "54000",
        ),
        (&many, "54000"),
    ];
    for (filter, code) in cases.iter() {
        for body in [scan_body(filter), search_body(filter)] {
            let resp = query(&alice, &body);
            assert_eq!(http_common::wire_code_of(&resp), *code, "{filter}");
        }
    }
}
