//! `POST /v1/query`（`op: "update"`／`op: "delete"`）の述語形 `filter` が、`or`・
//! 範囲比較・`in`・数値 4 型の `eq`／`ne`／`between` を SQL 表層の述語形 DML と同じ
//! 結果集合・同じ台帳照合（`content_hash`。`23505`／`22023`）で受理することを production
//! ルータ経由で固定する層 A 結合テスト（Issue #1356。対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-12・NOSQL-14・NOSQL-17、
//! `docs/spec/04-behavior/sql-surface.md` SQL-19・SQL-24、
//! `docs/spec/04-behavior/recovery.md` RECOVER-10）。
//!
//! 役割分担: `filter.rs` の unit tests は JSON → `WherePredicate` の写像（AST 同一性）を、
//! 本ファイルは「SQL で実行した操作を同じ `operation_id` で NoSQL から再送すると重複と判定
//! される」ことと、SQL の `DELETE` と NoSQL の `delete` の影響行数が一致することを
//! wire 越しに固定する。既存の `nosql12_update_delete.rs` は TEXT 列の語彙を担う。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
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
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("x", ColumnType::Double, true),
            ColumnDef::new("d", ColumnType::Date, true),
            ColumnDef::new(
                "amount",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql12-predicate-dml-numeric");
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

/// seed 4 行（id 1〜3 は各列に値があり、id 4 は数値・日付・NUMERIC が NULL）。
///
/// | id | lang | qty | d          | amount |
/// | -- | ---- | --- | ---------- | ------ |
/// | 1  | ja   | 1   | 2024-01-01 | 1.5    |
/// | 2  | en   | 2   | 2024-06-01 | 2.5    |
/// | 3  | fr   | 5   | 2025-01-01 | 7      |
/// | 4  | de   | NULL| NULL       | NULL   |
fn seed(who: &Both, tag: &str) {
    let rows = [
        r#"{"id":1,"embedding":[0.1,0.2,0.3],"lang":"ja","qty":1,"big":1,"r":1.5,"x":1.5,"d":"2024-01-01","amount":1.5}"#,
        r#"{"id":2,"embedding":[0.1,0.2,0.3],"lang":"en","qty":2,"big":2,"r":2.5,"x":2.5,"d":"2024-06-01","amount":2.5}"#,
        r#"{"id":3,"embedding":[0.1,0.2,0.3],"lang":"fr","qty":5,"big":5,"r":5.5,"x":5.5,"d":"2025-01-01","amount":7}"#,
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

fn delete_body(filter_json: &str, op_id: &str) -> Vec<u8> {
    format!(
        r#"{{"op":"delete","table":"{TABLE}","filter":{filter_json},"operation_id":"{op_id}"}}"#
    )
    .into_bytes()
}

/// 応答が 200 で `deleted` が `expected` であることを検証する。
fn assert_deleted(resp: &HttpResponse, expected: usize, what: &str) {
    assert_eq!(resp.status, 200, "{what}: resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(
        body.contains(&format!(r#""deleted":{expected}"#)),
        "{what}: {body}"
    );
}

/// (SQL の WHERE 句, NoSQL の filter JSON, 一致する行数)。seed に対する期待件数は
/// SQL の述語形 `DELETE` の影響行数と NoSQL の `deleted` の両方で検証する。
const PARITY_CASES: [(&str, &str, usize); 19] = [
    ("qty = 5", r#"[{"column":"qty","op":"eq","value":5}]"#, 1),
    (
        "NOT qty = 5",
        r#"[{"column":"qty","op":"ne","value":5}]"#,
        2,
    ),
    (
        "qty BETWEEN 1 AND 2",
        r#"[{"column":"qty","op":"between","value":[1,2]}]"#,
        2,
    ),
    (
        "NOT qty BETWEEN 1 AND 2",
        r#"[{"not":{"column":"qty","op":"between","value":[1,2]}}]"#,
        1,
    ),
    (
        "qty IN (1)",
        r#"[{"column":"qty","op":"in","value":[1]}]"#,
        1,
    ),
    (
        "qty IN (1, 2)",
        r#"[{"column":"qty","op":"in","value":[1,2]}]"#,
        2,
    ),
    (
        "NOT qty IN (1, 2)",
        r#"[{"not":{"column":"qty","op":"in","value":[1,2]}}]"#,
        1,
    ),
    ("qty < 5", r#"[{"column":"qty","op":"lt","value":5}]"#, 2),
    ("qty >= 2", r#"[{"column":"qty","op":"gte","value":2}]"#, 2),
    ("big <= 2", r#"[{"column":"big","op":"le","value":2}]"#, 2),
    ("r > 2.5", r#"[{"column":"r","op":"gt","value":2.5}]"#, 1),
    ("x = 1.5", r#"[{"column":"x","op":"eq","value":1.5}]"#, 1),
    (
        "d >= '2024-06-01'",
        r#"[{"column":"d","op":"gte","value":"2024-06-01"}]"#,
        2,
    ),
    (
        "amount < '2.5'",
        r#"[{"column":"amount","op":"lt","value":2.5}]"#,
        1,
    ),
    (
        "NOT (qty = 1 OR lang = 'en')",
        r#"[{"not":{"or":[{"column":"qty","op":"eq","value":1},{"column":"lang","op":"eq","value":"en"}]}}]"#,
        1,
    ),
    (
        "NOT NOT qty = 5",
        r#"[{"not":{"not":{"column":"qty","op":"eq","value":5}}}]"#,
        1,
    ),
    (
        "qty = 1 OR qty = 5",
        r#"[{"or":[{"column":"qty","op":"eq","value":1},{"column":"qty","op":"eq","value":5}]}]"#,
        2,
    ),
    (
        "lang < 'f'",
        r#"[{"column":"lang","op":"lt","value":"f"}]"#,
        2,
    ),
    (
        "qty >= 1 AND qty <= 2 AND lang IN ('ja', 'en')",
        r#"[{"column":"qty","op":"between","value":[1,2]},{"column":"lang","op":"in","value":["ja","en"]}]"#,
        2,
    ),
];

/// SQL で先に実行した述語形 `DELETE` を同じ `operation_id` で NoSQL から再送すると `23505`、
/// 逆方向（NoSQL → SQL）も `23505`、結果集合（影響行数）が SQL と NoSQL で一致する。
#[test]
fn predicate_delete_parity_with_sql_resend_and_result_set() {
    for (i, (where_clause, filter_json, expected)) in PARITY_CASES.iter().enumerate() {
        // SQL → NoSQL。
        let (core, _guard) = new_core();
        let (alice, _bob, mut sql) = spawn_both(core);
        seed(&alice, &format!("a{i}"));
        let op_id = format!("n1356-sql-first-{i}");
        common::send_simple_query(
            &mut sql,
            &format!("DELETE FROM {TABLE} WHERE {where_clause} USING OPERATION_ID '{op_id}'"),
        );
        let tag = common::read_command_complete(&mut sql);
        assert_eq!(tag, format!("DELETE {expected}"), "{where_clause}");
        common::read_ready_for_query(&mut sql);
        let resp = query(&alice, &delete_body(filter_json, &op_id));
        assert_eq!(resp.status, 409, "{filter_json}: {resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "23505", "{filter_json}");

        // NoSQL → SQL（結果集合の一致もここで確認する）。
        let (core, _guard) = new_core();
        let (alice, _bob, mut sql) = spawn_both(core);
        seed(&alice, &format!("b{i}"));
        let op_id = format!("n1356-nosql-first-{i}");
        let resp = query(&alice, &delete_body(filter_json, &op_id));
        assert_deleted(&resp, *expected, filter_json);
        common::send_simple_query(
            &mut sql,
            &format!("DELETE FROM {TABLE} WHERE {where_clause} USING OPERATION_ID '{op_id}'"),
        );
        common::expect_error_response_with_sqlstate(&mut sql, "23505");
        common::read_ready_for_query(&mut sql);
    }
}

/// 同一 `operation_id` で内容（述語）が異なる場合は `22023`（並び順違いの AND 列を含む）。
#[test]
fn predicate_delete_content_mismatch_is_22023() {
    let (core, _guard) = new_core();
    let (alice, _bob, mut sql) = spawn_both(core);
    seed(&alice, "m");
    common::send_simple_query(
        &mut sql,
        &format!("DELETE FROM {TABLE} WHERE qty = 5 USING OPERATION_ID 'n1356-mismatch'"),
    );
    let _ = common::read_command_complete(&mut sql);
    common::read_ready_for_query(&mut sql);
    let resp = query(
        &alice,
        &delete_body(
            r#"[{"column":"qty","op":"eq","value":6}]"#,
            "n1356-mismatch",
        ),
    );
    assert_eq!(http_common::wire_code_of(&resp), "22023", "resp={resp:?}");
}

/// `update` でも同じ語彙を受理し、影響行数が一致する。
#[test]
fn predicate_update_accepts_numeric_and_or_forms() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core);
    seed(&alice, "u");
    let resp = query(
        &alice,
        br#"{"op":"update","table":"items","set":{"lang":"zz"},"filter":[{"or":[{"column":"qty","op":"in","value":[1,2]},{"column":"qty","op":"gt","value":4}]}],"operation_id":"n1356-upd"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        String::from_utf8_lossy(&resp.body).contains(r#""updated":3"#),
        "{resp:?}"
    );
}

#[test]
fn numeric_filter_type_mismatch_is_42601() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core);
    seed(&alice, "t");
    for (i, filter) in [
        r#"[{"column":"qty","op":"eq","value":"5"}]"#,
        r#"[{"column":"qty","op":"in","value":["1","2"]}]"#,
        r#"[{"column":"qty","op":"in","value":[1,"2"]}]"#,
        r#"[{"column":"qty","op":"lt","value":"5"}]"#,
    ]
    .iter()
    .enumerate()
    {
        let resp = query(&alice, &delete_body(filter, &format!("n1356-tm-{i}")));
        assert_eq!(http_common::wire_code_of(&resp), "42601", "{filter}");
    }
}

/// 上限（葉 256・`in` 256 要素）超過は従来どおり `54000`（`or`／`not` の深さ上限は
/// JSON パーサ側のネスト上限が先に効くため `filter.rs` の unit test が担う）。
#[test]
fn predicate_filter_limits_are_unchanged_54000() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core);
    seed(&alice, "l");

    let leaf = r#"{"column":"lang","op":"eq","value":"ja"}"#;
    let leaves = vec![leaf; 257].join(",");
    let resp = query(
        &alice,
        &delete_body(&format!("[{leaves}]"), "n1356-limit-leaves"),
    );
    assert_eq!(http_common::wire_code_of(&resp), "54000", "{resp:?}");

    let items = (0..257)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let resp = query(
        &alice,
        &delete_body(
            &format!(r#"[{{"column":"qty","op":"in","value":[{items}]}}]"#),
            "n1356-limit-in",
        ),
    );
    assert_eq!(http_common::wire_code_of(&resp), "54000", "{resp:?}");
}

/// RLS 述語名は `or`／`not` の内側でも `42601`（クライアントが RLS を解除できる経路を作らない）。
#[test]
fn rls_predicate_name_inside_or_and_not_is_rejected() {
    let (core, _guard) = new_core();
    let (alice, _bob, _sql) = spawn_both(core);
    for (i, filter) in [
        r#"[{"or":[{"column":"visible","op":"eq","value":"x"},{"column":"lang","op":"eq","value":"ja"}]}]"#,
        r#"[{"not":{"column":"visible","op":"is_null"}}]"#,
    ]
    .iter()
    .enumerate()
    {
        let resp = query(&alice, &delete_body(filter, &format!("n1356-rls-{i}")));
        assert_eq!(http_common::wire_code_of(&resp), "42601", "{filter}");
    }
}

/// テナント境界: `or`／`not`／数値 `in` を使っても他テナントの行は更新・削除されず、
/// 件数も自テナント分だけである。
#[test]
fn predicate_dml_does_not_cross_tenant_boundary() {
    let (core, _guard) = new_core();
    let (alice, bob, _sql) = spawn_both(core.clone());
    seed(&alice, "ta");
    seed(&bob, "tb");

    let resp = query(
        &alice,
        &delete_body(
            r#"[{"not":{"or":[{"column":"qty","op":"eq","value":99},{"column":"lang","op":"eq","value":"zzz"}]}}]"#,
            "n1356-tenant-notor",
        ),
    );
    // 自テナントの qty 非 NULL の 3 行だけが対象（NULL 行は UNKNOWN のまま除外）。
    assert_deleted(&resp, 3, "tenant-a not-or delete");
    // alice の削除後も bob の 4 行は残る。
    let ctx = PolicyContext::with_visibilities(TENANT_B, [Visibility::Public, Visibility::Private])
        .expect("valid tenant-b ctx");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT lang FROM items LIMIT 100")
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 4, "tenant-b rows must be untouched");
}
