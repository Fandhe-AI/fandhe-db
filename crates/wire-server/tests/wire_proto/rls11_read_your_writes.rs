//! RLS-11（TASK-195。read-your-writes）の wire・HTTP・表層横断の層 A
//! 結合テスト。
//!
//! Issue #973（PR #977）で wire-server の認証導出点（`auth.rs::
//! session_policy_context`。pg wire・HTTP セッション発行の双方が経由）が
//! 返す `PolicyContext` の許可可視性を「`Public` のみ」から「`Public` ＋
//! 自テナントの `Private`」へ切り替えた。本ファイルは production の
//! 生バイトクライアント（pg wire・HTTP）経由でその契約を固定する:
//!
//! - `rls11_wire_read_your_writes_matrix`: pg wire 経由の 3 テナント ×
//!   {同一接続, 同一テナント別接続（別ユーザー alice2 を含む）, 他テナント}。
//! - `rls11_http_read_your_writes_matrix`: HTTP（`/v1/session`・`/v1/query`）
//!   経由の同型行列。
//! - `rls11_cross_surface_read_your_writes`: 同一 `Arc<EngineCore>` を
//!   pg wire・HTTP の双方のリスナーへ同時結線し、wire で書いた行を HTTP の
//!   `search`／`scan`／`aggregate` が、HTTP で書いた行を wire の `SELECT` が
//!   それぞれ同一テナントから読み戻せることを確認する（`nosql6_insert.rs`・
//!   `nosql6_tenant_row_id_scope.rs` の「HTTP 経由で書いた行は同一 SQL wire
//!   セッションからは読み戻せない」という旧前提はここで置き換わる）。
//!
//! いずれも `LIMIT`／`limit` を可視総数を上回る値にし、越境・非表示が
//! 打ち切りに隠れないようにする。

use crate::common;
use crate::http_common;

use std::collections::BTreeSet;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;
use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// 3 テナントそれぞれに `Public` 行を 1 件ずつ投入した `EngineCore` を返す
/// （id=1..=3。`docs` テーブル）。
fn new_core_with_seed() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("rls11-wire-http");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (idx, tenant) in TENANTS.iter().enumerate() {
        let id = (idx as u64) + 1;
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let op_id = OperationId::parse(&format!("rls11-wire-seed-{id}")).expect("valid op id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![1.0, 0.0]), Value::Text("ja".to_string())],
            &op_id,
        )
        .expect("insert seed row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// wire 経由 `SELECT` を実行し、`id` 列の集合を返す（可視総数を上回る
/// `LIMIT` を使う）。
fn wire_select_ids(stream: &mut std::net::TcpStream) -> BTreeSet<String> {
    send_simple_query(
        stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100",
    );
    let _columns = read_row_description(stream);
    let mut ids = BTreeSet::new();
    // `CommandComplete` の `SELECT n` タグを先に見て件数を確定させる方式
    // ではなく、`DataRow`/`CommandComplete` を型バイトで見分けて読み切る
    // （wire1_simple_query.rs の流儀と異なり件数を事前に知らないため）。
    loop {
        let mut header = [0u8; 1];
        use std::io::Read;
        stream.read_exact(&mut header).expect("read message type");
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len - 4];
        stream.read_exact(&mut body).expect("read body");
        match header[0] {
            b'D' => {
                // DataRow: 1 列（id）。フォーマットは `read_data_row` と同型
                // だが本関数は独自にヘッダを消費済みのためここで手動解析する。
                let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
                assert_eq!(field_count, 1, "expected single id column");
                let val_len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                assert!(val_len >= 0, "id column must not be NULL");
                let val_len = val_len as usize;
                let value = std::str::from_utf8(&body[6..6 + val_len])
                    .expect("utf8 id value")
                    .to_string();
                ids.insert(value);
            }
            b'C' => break,
            other => panic!("unexpected message type {other} while reading SELECT results"),
        }
    }
    read_ready_for_query(stream);
    ids
}

/// pg wire 経由の RLS-11 行列: 3 テナント × {同一接続, 同一テナント別接続
/// （alice2 を含む）, 他テナント}。
#[test]
fn rls11_wire_read_your_writes_matrix() {
    let (core, _guard) = new_core_with_seed();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("alice2", "tenant-a", "pw-alice2"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);

    let users_by_tenant: [(&str, &str, &str); 3] = [
        ("tenant-a", "alice", "pw-alice"),
        ("tenant-b", "bob", "pw-bob"),
        ("tenant-c", "carol", "pw-carol"),
    ];

    for (idx, (tenant, user, pw)) in users_by_tenant.into_iter().enumerate() {
        let seed_id = (idx + 1).to_string();
        let mut s1 = authenticate_to_ready_for_query(addr, user, pw);

        // ウォーム: 自テナントの Public seed 行が見える。
        let before = wire_select_ids(&mut s1);
        assert!(
            before.contains(&seed_id),
            "tenant {tenant} must see its own Public seed row before insert, got {before:?}"
        );

        // INSERT（同一接続 s1）。
        let insert_id = 100 + idx as u64;
        send_simple_query(
            &mut s1,
            &format!(
                "INSERT INTO docs (id, embedding, lang) VALUES \
                 ({insert_id}, '[1.0,0.0]', 'ja') USING OPERATION_ID 'rls11-wire-insert-{tenant}'"
            ),
        );
        let tag = read_command_complete(&mut s1);
        assert_eq!(tag, "INSERT 0 1");
        read_ready_for_query(&mut s1);

        // (1) 同一接続で直ちに読み戻せる。
        let same_conn_ids = wire_select_ids(&mut s1);
        assert!(
            same_conn_ids.contains(&insert_id.to_string()),
            "tenant {tenant}: same wire connection must observe its own just-inserted row, \
             got {same_conn_ids:?}"
        );

        // (2) 同一テナントの新規接続（同じユーザー）でも見える。
        let mut s2 = authenticate_to_ready_for_query(addr, user, pw);
        let s2_ids = wire_select_ids(&mut s2);
        assert_eq!(
            s2_ids, same_conn_ids,
            "tenant {tenant}: fresh same-tenant connection ({user}) must see the same set"
        );

        // 同一テナントの別ユーザー（alice のみ alice2 を持つ）でも見える
        // （可視性がユーザー単位ではなくテナント単位であることの証跡）。
        if tenant == "tenant-a" {
            let mut s3 = authenticate_to_ready_for_query(addr, "alice2", "pw-alice2");
            let s3_ids = wire_select_ids(&mut s3);
            assert_eq!(
                s3_ids, same_conn_ids,
                "tenant-a: fresh connection from a different user of the same tenant \
                 (alice2) must see the same set"
            );
        }

        // (3) 他テナントは一切見えない。
        for (other_tenant, other_user, other_pw) in users_by_tenant {
            if other_tenant == tenant {
                continue;
            }
            let mut other = authenticate_to_ready_for_query(addr, other_user, other_pw);
            let other_ids = wire_select_ids(&mut other);
            assert!(
                !other_ids.contains(&insert_id.to_string()),
                "tenant {other_tenant} must never observe tenant {tenant}'s row (id={insert_id}), \
                 got {other_ids:?}"
            );
        }
    }
}

// --- HTTP 表層 --------------------------------------------------------------

fn http_session(addr: std::net::SocketAddr, user: &str, pw: &str) -> String {
    let login_body = format!(r#"{{"user":"{user}","password":"{pw}"}}"#).into_bytes();
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        &login_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed for {user}: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn http_query(addr: std::net::SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
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

/// `{"columns":...,"rows":[[...],...],"row_count":n}` 応答本文から `rows`
/// 配列を取り出す（`op: scan`／`search`／`aggregate` いずれも共通の応答形状。
/// `wire_server::http::query::response::encode` 参照）。
fn extract_rows(resp: &HttpResponse) -> Vec<JsonValue> {
    let text = std::str::from_utf8(&resp.body).expect("utf-8 body");
    let JsonValue::Object(mut top) = parse_json(text).expect("valid json") else {
        panic!("top level must be an object: {text}");
    };
    let JsonValue::Array(rows) = top.remove("rows").expect("rows field") else {
        panic!("rows must be an array");
    };
    rows
}

/// 単一列（`id`）の `rows` を `id` の値集合へ変換する。
fn rows_to_id_set(rows: Vec<JsonValue>) -> BTreeSet<String> {
    rows.into_iter()
        .map(|row| match row {
            JsonValue::Array(mut cells) => match cells.pop() {
                Some(JsonValue::Number(n)) => (n.as_f64() as u64).to_string(),
                Some(JsonValue::String(s)) => s,
                other => panic!("expected numeric/string id cell, got {other:?}"),
            },
            other => panic!("row must be an array, got {other:?}"),
        })
        .collect()
}

/// `op: scan` 応答を解析し、`id` 列の値集合を返す（`columns: ["id"]` 前提）。
fn http_scan_ids(addr: std::net::SocketAddr, token: &str) -> BTreeSet<String> {
    let resp = http_query(
        addr,
        token,
        br#"{"op":"scan","table":"docs","limit":100,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "scan must succeed: {resp:?}");
    rows_to_id_set(extract_rows(&resp))
}

/// `op: search`（`vector` 経由の dense ORDER BY 相当）応答を解析し、`id` 列
/// の値集合を返す（`columns: ["id"]` 前提。`limit` は可視総数を上回る値）。
fn http_search_ids(addr: std::net::SocketAddr, token: &str) -> BTreeSet<String> {
    let resp = http_query(
        addr,
        token,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":100,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "search must succeed: {resp:?}");
    rows_to_id_set(extract_rows(&resp))
}

/// `op: aggregate`（`COUNT(*)`）応答を解析し、件数を返す。
fn http_aggregate_count(addr: std::net::SocketAddr, token: &str) -> u64 {
    let resp = http_query(
        addr,
        token,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}]}"#,
    );
    assert_eq!(resp.status, 200, "aggregate must succeed: {resp:?}");
    let rows = extract_rows(&resp);
    let row = rows.into_iter().next().expect("aggregate returns 1 row");
    match row {
        JsonValue::Array(cells) => match cells.into_iter().next() {
            Some(JsonValue::Number(n)) => n.as_f64() as u64,
            other => panic!("expected numeric COUNT(*) cell, got {other:?}"),
        },
        other => panic!("row must be an array, got {other:?}"),
    }
}

fn http_insert(addr: std::net::SocketAddr, token: &str, id: u64, op_id: &str) {
    let body = format!(
        r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[1.0,0.0],"lang":"ja"}}],"operation_id":"{op_id}"}}"#
    )
    .into_bytes();
    let resp = http_query(addr, token, &body);
    assert_eq!(resp.status, 200, "insert must succeed: {resp:?}");
}

/// HTTP（`/v1/session` → `/v1/query`）経由の RLS-11 行列: 3 テナント ×
/// {同一トークン, 同一テナント別トークン, 他テナント}。
#[test]
fn rls11_http_read_your_writes_matrix() {
    let (core, _guard) = new_core_with_seed();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);

    let users_by_tenant: [(&str, &str, &str); 3] = [
        ("tenant-a", "alice", "pw-alice"),
        ("tenant-b", "bob", "pw-bob"),
        ("tenant-c", "carol", "pw-carol"),
    ];

    for (idx, (tenant, user, pw)) in users_by_tenant.into_iter().enumerate() {
        let seed_id = (idx + 1).to_string();
        let token1 = http_session(addr, user, pw);

        let before = http_scan_ids(addr, &token1);
        assert!(
            before.contains(&seed_id),
            "tenant {tenant} must see its own Public seed row before insert, got {before:?}"
        );

        let insert_id = 100 + idx as u64;
        http_insert(
            addr,
            &token1,
            insert_id,
            &format!("rls11-http-insert-{tenant}"),
        );

        // (1) 同一トークンで直ちに読み戻せる。
        let same_token_ids = http_scan_ids(addr, &token1);
        assert!(
            same_token_ids.contains(&insert_id.to_string()),
            "tenant {tenant}: same HTTP session must observe its own just-inserted row, \
             got {same_token_ids:?}"
        );

        // (2) 同一テナントの新規トークン（同じユーザーで再ログイン）でも
        //     見える。
        let token2 = http_session(addr, user, pw);
        let token2_ids = http_scan_ids(addr, &token2);
        assert_eq!(
            token2_ids, same_token_ids,
            "tenant {tenant}: fresh same-tenant session must see the same set"
        );

        // (3) 他テナントは一切見えない。
        for (other_tenant, other_user, other_pw) in users_by_tenant {
            if other_tenant == tenant {
                continue;
            }
            let other_token = http_session(addr, other_user, other_pw);
            let other_ids = http_scan_ids(addr, &other_token);
            assert!(
                !other_ids.contains(&insert_id.to_string()),
                "tenant {other_tenant} must never observe tenant {tenant}'s row (id={insert_id}), \
                 got {other_ids:?}"
            );
        }
    }
}

// --- 表層横断 ----------------------------------------------------------------

/// 同一 `Arc<EngineCore>` を pg wire・HTTP の双方へ同時結線し、書いた表層
/// を問わず同一テナントの他方の表層から読み戻せる（RLS-11・TASK-195）こと
/// を確認する（`nosql6_insert.rs`・`nosql6_tenant_row_id_scope.rs` の
/// 旧前提「HTTP 経由で書いた行は同一 SQL wire セッションからは読み戻せない」
/// を production の生バイトクライアント経由で置き換える）。
#[test]
fn rls11_cross_surface_read_your_writes() {
    let (core, _guard) = new_core_with_seed();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);

    let wire_addr = spawn_server_with_engine(&users_path, core.clone());
    let http_addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);

    // wire で書いた行を HTTP の search／scan／aggregate すべてで読み戻せる
    // （alice/tenant-a）。
    let mut wire_stream = authenticate_to_ready_for_query(wire_addr, "alice", "pw-alice");
    send_simple_query(
        &mut wire_stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (201, '[1.0,0.0]', 'ja') \
         USING OPERATION_ID 'rls11-cross-wire-insert'",
    );
    assert_eq!(read_command_complete(&mut wire_stream), "INSERT 0 1");
    read_ready_for_query(&mut wire_stream);

    let alice_token = http_session(http_addr, "alice", "pw-alice");
    let alice_ids_after_wire_insert = http_scan_ids(http_addr, &alice_token);
    assert!(
        alice_ids_after_wire_insert.contains("201"),
        "tenant-a: row inserted via wire must be visible over HTTP scan, \
         got {alice_ids_after_wire_insert:?}"
    );
    let alice_search_ids_after_wire_insert = http_search_ids(http_addr, &alice_token);
    assert!(
        alice_search_ids_after_wire_insert.contains("201"),
        "tenant-a: row inserted via wire must be visible over HTTP search, \
         got {alice_search_ids_after_wire_insert:?}"
    );
    let alice_count_after_wire_insert = http_aggregate_count(http_addr, &alice_token);
    assert_eq!(
        alice_count_after_wire_insert, 4,
        "tenant-a: HTTP aggregate COUNT(*) must include the wire-inserted row \
         (3 shared Public seed rows + 1 own Private row)"
    );

    // HTTP で書いた行を wire の SELECT で読み戻せる（bob/tenant-b）。
    let bob_token = http_session(http_addr, "bob", "pw-bob");
    http_insert(http_addr, &bob_token, 202, "rls11-cross-http-insert");

    let mut bob_wire = authenticate_to_ready_for_query(wire_addr, "bob", "pw-bob");
    let bob_ids_after_http_insert = wire_select_ids(&mut bob_wire);
    assert!(
        bob_ids_after_http_insert.contains("202"),
        "tenant-b: row inserted via HTTP must be visible over wire SELECT, \
         got {bob_ids_after_http_insert:?}"
    );

    // 越境しない: alice(tenant-a) は bob(tenant-b) の HTTP 挿入行 (id=202)
    // を wire からも HTTP（scan／search／aggregate いずれも）からも見えない。
    let mut alice_wire = authenticate_to_ready_for_query(wire_addr, "alice", "pw-alice");
    let alice_wire_ids = wire_select_ids(&mut alice_wire);
    assert!(
        !alice_wire_ids.contains("202"),
        "tenant-a must not observe tenant-b's HTTP-inserted row over wire, \
         got {alice_wire_ids:?}"
    );
    let alice_scan_ids = http_scan_ids(http_addr, &alice_token);
    assert!(
        !alice_scan_ids.contains("202"),
        "tenant-a must not observe tenant-b's HTTP-inserted row over HTTP scan, \
         got {alice_scan_ids:?}"
    );
    let alice_search_ids = http_search_ids(http_addr, &alice_token);
    assert!(
        !alice_search_ids.contains("202"),
        "tenant-a must not observe tenant-b's HTTP-inserted row over HTTP search, \
         got {alice_search_ids:?}"
    );
    let alice_count_after_bob_insert = http_aggregate_count(http_addr, &alice_token);
    assert_eq!(
        alice_count_after_bob_insert, 4,
        "tenant-a: HTTP aggregate COUNT(*) must be unaffected by tenant-b's HTTP-inserted \
         Private row"
    );
}
