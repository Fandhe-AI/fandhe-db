//! 更新可能ビューへの書き込み（TABLE-43・RLS-10・Issue #1674）の wire 越しのテナント境界検証
//! （層 A。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-43）。
//!
//! 位置づけ: 書き換えと台帳の意味論は `crates/engine/tests/table43_view_write_rls_ledger.rs` が確定
//! オラクルとして検証済みで、`wire_view_insert.rs`・`wire_view_update_delete.rs`・
//! `wire_view_check_option.rs` は単一ユーザー（tenant-a）の基本形を見ている。本ファイルは
//! 2 ユーザー・2 テナントで次を確かめる。
//! (1) 簡易クエリの `CommandComplete` が閲覧テナント自身の行だけを数え、他テナントの行を狙っても
//!     存在しない id を狙った場合と同じ応答になること
//! (2) `44000`／`55000`／`42703` の ErrorResponse がテナント間でバイト単位に同一であること
//! (3) 同じ `operation_id` の再送が `23505`、基表への直接送信が `22023` になること
//! (4) NoSQL（HTTP）表層からビュー名を書き込み対象にすると、存在しない表名と同じ応答になり、
//!     ビューの存在を観測できないこと（現行は `42P01`。ビューへの NoSQL 書き込みの受理は対象外）

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;
use http_common::{temp_db, AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-view-write-tenant");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn sql_as(core: &EngineCore, tenant: &str, ddl: bool, sql: &str) {
    let mut session = SessionState::default();
    if ddl {
        session.allow_ddl();
    }
    let ctx = PolicyContext::new(tenant).expect("valid tenant id");
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// ビュー 3 種（単純・CHECK OPTION 付き・自動更新不可）と、各テナント 1 行の種を用意する。
/// alice（tenant-a）は id 1、bob（tenant-b）は id 2。どちらも `lang = 'ja'`。
fn prepare(core: &EngineCore) {
    sql_as(
        core,
        "tenant-a",
        true,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    sql_as(
        core,
        "tenant-a",
        true,
        "CREATE VIEW cv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    sql_as(
        core,
        "tenant-a",
        true,
        "CREATE VIEW lv AS SELECT * FROM docs LIMIT 5",
    );
    for (tenant, id) in [("tenant-a", 1), ("tenant-b", 2)] {
        sql_as(
            core,
            tenant,
            false,
            &format!(
                "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[1,0,0]', 'ja') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
}

fn users() -> UserStoreFile {
    write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ])
}

fn connect_both(core: Arc<EngineCore>) -> (TcpStream, TcpStream) {
    let users_path = users();
    let addr = spawn_server_with_engine(&users_path, core);
    let alice = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let bob = authenticate_to_ready_for_query(addr, "bob", "pw-bob");
    (alice, bob)
}

/// 書き込み文（`RETURNING` なし）を簡易クエリで流し、`CommandComplete` のタグを返す。
fn exec(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

/// 拒否される文を流し、ErrorResponse 本体のバイト列を返す（続く ReadyForQuery も読む）。
fn exec_error_body(stream: &mut TcpStream, sql: &str) -> Vec<u8> {
    send_simple_query(stream, sql);
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse for {sql}");
    read_ready_for_query(stream);
    body
}

fn ids_visible(stream: &mut TcpStream) -> Vec<String> {
    send_simple_query(stream, "SELECT id FROM docs LIMIT 100");
    let _ = read_row_description(stream);
    let mut ids = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => {
                // DataRow: 列数(2)・長さ(4)・値。
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
                ids.push(String::from_utf8_lossy(&body[6..6 + len]).to_string());
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(stream);
    ids.sort();
    ids
}

/// 簡易クエリ: ビュー経由の UPDATE／DELETE は閲覧テナント自身の行だけを数え、他テナントの行
/// （id 1）を狙っても存在しない id を狙った場合と同じ `CommandComplete` になる。
#[test]
fn simple_query_view_writes_are_scoped_to_the_viewing_tenant() {
    let (core, _g) = new_core();
    prepare(&core);
    let (mut alice, mut bob) = connect_both(core);

    // bob が alice の行（id 1）を狙っても、存在しない id（999）を狙っても 0 行で同一。
    let foreign = exec(&mut bob, "DELETE FROM sv WHERE id = 1");
    let missing = exec(&mut bob, "DELETE FROM sv WHERE id = 999");
    assert_eq!(foreign, "DELETE 0");
    assert_eq!(foreign, missing);
    assert_eq!(
        exec(&mut bob, "UPDATE sv SET lang = 'ja' WHERE id = 1"),
        exec(&mut bob, "UPDATE sv SET lang = 'ja' WHERE id = 999")
    );
    // 述語形は自分の行だけ（1 行）。
    assert_eq!(
        exec(&mut bob, "UPDATE sv SET lang = 'ja' WHERE lang = 'ja'"),
        "UPDATE 1"
    );
    assert!(ids_visible(&mut alice).contains(&"1".to_string()));

    // alice 側も同様に自分の行だけを消す。bob の行（id 2）は残る。
    assert_eq!(
        exec(&mut alice, "DELETE FROM sv WHERE lang = 'ja'"),
        "DELETE 1"
    );
    assert!(ids_visible(&mut bob).contains(&"2".to_string()));
}

/// `44000`・`55000`・`42703` の ErrorResponse は alice と bob でバイト単位に同一で、
/// ビュー名・値・テナントを含まない。
#[test]
fn rejections_are_byte_identical_across_tenants() {
    let (core, _g) = new_core();
    prepare(&core);
    let (mut alice, mut bob) = connect_both(core);

    for (sql, sqlstate) in [
        (
            "INSERT INTO cv (id, embedding, lang) VALUES (50, '[1,0,0]', 'secret')",
            "44000",
        ),
        ("UPDATE cv SET lang = 'secret' WHERE lang = 'ja'", "44000"),
        ("UPDATE lv SET lang = 'x' WHERE id = 1", "55000"),
        ("DELETE FROM sv WHERE hidden_col = 'x'", "42703"),
    ] {
        let a = exec_error_body(&mut alice, sql);
        let b = exec_error_body(&mut bob, sql);
        assert_eq!(a, b, "{sql}");
        let text = String::from_utf8_lossy(&a).to_string();
        assert!(text.contains(sqlstate), "{sql}: {text}");
        for needle in ["secret", "tenant-a", "tenant-b", "alice", "bob"] {
            assert!(!text.contains(needle), "{sql} leaks {needle}: {text}");
        }
    }
}

/// 同じ `operation_id` の再送は `23505`、同じ id の基表への直接送信は `22023`。
#[test]
fn replayed_operation_id_is_rejected_over_the_wire() {
    let (core, _g) = new_core();
    prepare(&core);
    let (mut alice, _bob) = connect_both(core);

    let via_view = "UPDATE sv SET lang = 'ja' WHERE id = 1 USING OPERATION_ID 'wire-vw-1'";
    assert_eq!(exec(&mut alice, via_view), "UPDATE 1");
    send_simple_query(&mut alice, via_view);
    expect_error_response_with_sqlstate(&mut alice, "23505");
    read_ready_for_query(&mut alice);

    send_simple_query(
        &mut alice,
        "UPDATE docs SET lang = 'ja' WHERE id = 1 USING OPERATION_ID 'wire-vw-1'",
    );
    expect_error_response_with_sqlstate(&mut alice, "22023");
    read_ready_for_query(&mut alice);
}

// ---- NoSQL（HTTP）表層 -------------------------------------------------------------------------

fn login(addr: std::net::SocketAddr, user: &str, password: &str) -> String {
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
    match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn post_query(addr: std::net::SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
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

/// NoSQL の `insert`／`update`／`delete` でビュー名を対象にしても、存在しない表名を対象にした
/// 場合と同じ HTTP ステータス・`wire_code` になり、両テナントで同一である（ビューの存在が観測
/// できない）。ビューへの NoSQL 書き込みの受理は本 Issue の対象外で、現行の拒否を固定する。
#[test]
fn nosql_writes_to_a_view_look_like_a_missing_table() {
    let (core, _g) = new_core();
    prepare(&core);
    let users_path = users();
    let addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let tokens = [
        login(addr, "alice", "pw-alice"),
        login(addr, "bob", "pw-bob"),
    ];
    let bodies = |table: &str| -> Vec<Vec<u8>> {
        vec![
            format!(
                r#"{{"op":"insert","table":"{table}","rows":[{{"id":70,"embedding":[0.1,0.2,0.3],"lang":"ja"}}],"operation_id":"nv-ins"}}"#
            )
            .into_bytes(),
            format!(
                r#"{{"op":"update","table":"{table}","set":{{"lang":"ja"}},"where":{{"id":1}},"operation_id":"nv-upd"}}"#
            )
            .into_bytes(),
            format!(
                r#"{{"op":"delete","table":"{table}","where":{{"id":1}},"operation_id":"nv-del"}}"#
            )
            .into_bytes(),
        ]
    };
    let view_bodies = bodies("sv");
    let missing_bodies = bodies("no_such_table");
    for (i, (vb, mb)) in view_bodies.iter().zip(missing_bodies.iter()).enumerate() {
        let mut seen = Vec::new();
        for token in &tokens {
            let on_view = post_query(addr, token, vb);
            let on_missing = post_query(addr, token, mb);
            assert_eq!(on_view.status, on_missing.status, "op #{i}");
            assert_eq!(
                http_common::wire_code_of(&on_view),
                http_common::wire_code_of(&on_missing),
                "op #{i}"
            );
            // ビュー名・テナント名を本文へ出さない。
            let text = String::from_utf8_lossy(&on_view.body).to_string();
            assert!(
                !text.contains("\"sv\"") && !text.contains("tenant-"),
                "{text}"
            );
            // 認証・ルーティングを通過したうえでの拒否であること（空虚な一致の排除）。
            assert_eq!(http_common::wire_code_of(&on_view), "42P01", "op #{i}");
            seen.push((on_view.status, http_common::wire_code_of(&on_view)));
        }
        assert_eq!(seen[0], seen[1], "op #{i}");
    }

    // いずれの経路でも基表の行は変わらない（alice の id 1 が残り、id 70 は作られない）。
    let ctx = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant");
    let mut session = SessionState::default();
    let out = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT id FROM docs LIMIT 100")
        .expect("select");
    match out {
        engine::sql::SqlOutcome::Query(q) => {
            let ids: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
            assert!(ids.contains(&1) && !ids.contains(&70), "{ids:?}");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}
