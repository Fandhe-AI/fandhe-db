//! `POST /v1/query` の op 許可リスト（`search`／`scan`／`aggregate`／
//! `insert`／`update`／`delete` の閉じた 6 値）を、production ルータ経由
//! （生バイトクライアント）で検証する層 A 結合テスト（Issue #759・
//! TASK-179。`update`／`delete` の追加は Issue #875。対象ビヘイビア
//! NOSQL-1・NOSQL-9・NOSQL-12。ポインタ: `docs/spec/05-tasks.md` TASK-179・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1・NOSQL-9・NOSQL-12）。
//!
//! `wire_server::http::query::op` の単体テストは crate 内部から `Op::parse`
//! を直接叩くが、本ファイルは `nosql1_endpoint_routing.rs` と同じ流儀で
//! production ルータ（[`wire_server::http::router::Router`]）へ実際に
//! `POST /v1/query` を送り、HTTP 応答（ステータス・`wire_code`・
//! エラーコード・メッセージ）を確認する。
//!
//! 許可リストの `search`／`scan`／`aggregate`／`insert`／`update`（`where`
//! 形・`filter` 形の両方）／`delete`（同左）の 6 op はいずれも実行結線済み
//! （TASK-186・NOSQL-2〜6・NOSQL-12・Issue #764・#766・#768・#772・#876・
//! `filter`〔述語形〕は #1062）のため、`engine` 接続済みでは暫定
//! `0A000`／501（[`wire_server::http::query::gate::PLACEHOLDER_MESSAGE`]）を
//! 返さず、存在しないテーブルへの到達を示す `42P01`／404 で「認証 →
//! op 許可リスト → スキーマ検証 → engine 呼び出し」が走ったことを確認する。
//! 一方 `update`／`delete` の `filter: []`（空配列）は SQL 表層の `WHERE`
//! 句省略とのパリティで `42601`（engine を一切呼ばない。Issue #1062）に
//! 留まる（本ファイルでは実行結線 6 op と分けて確認する）。語彙外拒否は
//! HTTP 501・`wire_code` `0A000`・`code` `FEATURE_NOT_SUPPORTED` で status
//! だけでは区別できないため、必ず `error_message_of` で
//! [`wire_server::http::query::gate::UNSUPPORTED_OP_MESSAGE`] を突き合わせる。
//!
//! 役割分担（Issue #774）: seed 済み 2 テナント fixture 上での 4 op **成功
//! 応答**・RLS-5／RLS-7／RLS-8 系の検証は `nosql1_op_vocabulary.rs` が担う。
//! 本ファイルはスローアウェイ core 上の到達判定（本 doc 上記）に閉じる。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::net::SocketAddr;

use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::gate::UNSUPPORTED_OP_MESSAGE;
use wire_server::http::session::store::SessionStore;

/// `POST /v1/session` へログインしてトークン（base64url 表現）を取り出す。
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

/// 正常形の `POST <target>`（`Content-Type: application/json`・宣言済み
/// `Content-Length`）を送って解析済み応答を返す便宜 API。
fn post(addr: SocketAddr, target: &str, auth: Option<&str>, body: &[u8]) -> HttpResponse {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let auth_header;
    if let Some(auth) = auth {
        auth_header = format!("Bearer {auth}");
        headers.push(("Authorization", &auth_header));
    }
    let content_length = body.len().to_string();
    headers.push(("Content-Type", "application/json"));
    headers.push(("Content-Length", &content_length));
    let request = http_common::build_request(target, &headers, body);
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// テナントの Bearer トークンを都度発行し `/v1/query` へ本文を送る便宜 API
/// （セッション枠を使い切らないよう毎回新規ログインする）。
fn query(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, "/v1/query", Some(&token), body)
}

fn spawn() -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener(&users_path, SessionStore::new())
}

// --- 許可リスト 6 op（where 形の update／delete を含む）はいずれも実行結線済み（42P01・非 501） ---

#[test]
fn six_allowlisted_ops_reach_the_engine_and_report_undefined_table() {
    let addr = spawn();

    // `scan`（TASK-186・NOSQL-3・Issue #766）・`aggregate`（Issue #768）・
    // `insert`（Issue #772）・`search`（TASK-186・NOSQL-2・Issue #764）・
    // `update`（`where` 形。Issue #876）・`delete`（`where` 形。Issue #876）
    // はいずれも実行結線済みのため、スローアウェイ `EngineCore`（テーブル
    // 未作成）上では `42P01`／404 が到達の証跡になる（暫定 `0A000`／501
    // へ到達する op は `filter`〔述語形〕指定のみになった）。`search` は
    // `vector`／`plan` いずれも未指定だが、テーブル解決
    // （`EngineCore::execute_bound_search_in_session` の schema 取得）が
    // binder（`vector`／`plan` 排他判定）より先に走るため `42P01` になる
    // （§2.3 の判定順序どおり）。
    let executed_bodies: [&[u8]; 6] = [
        br#"{"op":"scan","table":"docs","limit":1}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"nosql9-op-1"}"#,
        br#"{"op":"search","table":"docs","limit":1}"#,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":"nosql9-op-2"}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":"nosql9-op-3"}"#,
    ];
    for body in executed_bodies {
        let resp = query(addr, body);
        http_common::assert_reached_query_gate(&resp);
    }
}

// --- update／delete: `filter: []`（空配列）は engine を呼ばず 42601 ---

#[test]
fn update_and_delete_empty_filter_rejects_with_42601_without_reaching_engine() {
    // `filter: []`（空配列）は SQL 表層の `WHERE` 句省略とのパリティで
    // `42601`（Issue #1062）。engine を一切呼ばないため `42P01` には到達
    // しない。
    let addr = spawn();
    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[]}"#,
        br#"{"op":"delete","table":"docs","filter":[]}"#,
    ];
    for body in bodies {
        let resp = query(addr, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

#[test]
fn update_and_delete_non_empty_filter_reach_the_engine_and_report_undefined_table() {
    // 述語形（`filter` 非空。TASK-186・NOSQL-12、Issue #1062 で実行結線済み）
    // は語彙・スキーマ検証を通過した後 engine まで到達し、存在しないテーブル
    // への要求は `where` 形と同じ `42P01`／404 になる。
    let addr = spawn();
    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"n9-pred-update-1"}"#,
        br#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"n9-pred-delete-1"}"#,
    ];
    for body in bodies {
        let resp = query(addr, body);
        assert_eq!(resp.status, 404, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42P01");
    }
}

// --- 語彙外 op: DDL・UDF・トランザクション・表記揺れ -----------------------

#[test]
fn vocabulary_outside_six_ops_rejects_with_0a000_and_unsupported_message() {
    let addr = spawn();
    let unsupported_ops = [
        // NOSQL-13 対象外の DDL 相当（`create_table`／`alter_table`／
        // `drop_table` は Issue #910 で語彙へ加わったため対象から外す）
        "create_index",
        "drop_index",
        "create_view",
        "drop_view",
        // UDF 呼び出し相当
        "call",
        "udf",
        // トランザクション制御相当
        "begin",
        "commit",
        "rollback",
        // SQL 系・表記揺れ
        "select",
        "explain",
        "set",
        "SEARCH",
        " search",
        "",
    ];
    for op in unsupported_ops {
        let body = format!(r#"{{"op":"{op}","table":"docs"}}"#);
        let resp = query(addr, body.as_bytes());
        assert_eq!(resp.status, 501, "op={op:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "0A000", "op={op:?}");
        assert_eq!(http_common::error_code_of(&resp), "FEATURE_NOT_SUPPORTED");
        assert_eq!(
            http_common::error_message_of(&resp),
            UNSUPPORTED_OP_MESSAGE,
            "op={op:?}"
        );
        // 語彙外 op 文字列そのものを応答本文へ echo しない。
        if !op.is_empty() {
            http_common::assert_message_does_not_echo(&resp, op);
        }
    }
}

// --- 順序契約: op 判定が schema 検証より先 ---------------------------------

#[test]
fn op_allowlist_check_precedes_schema_validation_over_wire() {
    let addr = spawn();
    // 語彙外 op に加え未知キー（`hint_order`）も同時に付与しても、
    // schema 検証由来の 42601 ではなく op 許可リスト由来の 0A000 が返る
    // （`drop_table` は Issue #910 で語彙へ加わったため、引き続き語彙外の
    // `drop_index` を使う）。
    let body = br#"{"op":"drop_index","table":"docs","hint_order":["path"]}"#;
    let resp = query(addr, body);
    assert_eq!(resp.status, 501, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "0A000");
    assert_eq!(http_common::error_message_of(&resp), UNSUPPORTED_OP_MESSAGE);
}

// --- 未知キー（HINT ORDER・セッション変数・トランザクション/UDF 相当） ----

#[test]
fn unknown_fields_on_valid_ops_reject_with_42601() {
    let addr = spawn();
    let cases: [(&str, &str); 17] = [
        (
            "search",
            r#"{"op":"search","table":"docs","limit":1,"hint_order":["path"]}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"hint_order":["path"]}"#,
        ),
        (
            "aggregate",
            r#"{"op":"aggregate","table":"docs","aggregates":[],"hint_order":["path"]}"#,
        ),
        (
            "insert",
            r#"{"op":"insert","table":"docs","rows":[],"hint_order":["path"]}"#,
        ),
        (
            "search",
            r#"{"op":"search","table":"docs","limit":1,"hint":"x"}"#,
        ),
        (
            "search",
            r#"{"op":"search","table":"docs","limit":1,"search_mode":"precision"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"search_mode":"precision"}"#,
        ),
        (
            "aggregate",
            r#"{"op":"aggregate","table":"docs","aggregates":[],"search_mode":"precision"}"#,
        ),
        (
            "insert",
            r#"{"op":"insert","table":"docs","rows":[],"search_mode":"precision"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"set":"x"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"session":"x"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"mode":"precision"}"#,
        ),
        (
            "aggregate",
            r#"{"op":"aggregate","table":"docs","aggregates":[],"mode":"precision"}"#,
        ),
        (
            "insert",
            r#"{"op":"insert","table":"docs","rows":[],"mode":"precision"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"transaction":"begin"}"#,
        ),
        (
            "update",
            r#"{"op":"update","table":"docs","set":{"lang":"en"},"hint_order":["path"]}"#,
        ),
        (
            "delete",
            r#"{"op":"delete","table":"docs","where":{"id":1},"search_mode":"precision"}"#,
        ),
    ];
    for (label, body) in cases {
        let resp = query(addr, body.as_bytes());
        assert_eq!(resp.status, 400, "label={label} body={body} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "label={label}");
    }
}

// --- op 欠落／非文字列／ルート非オブジェクト（回帰） -----------------------

#[test]
fn missing_or_non_string_op_and_non_object_root_reject_with_42601() {
    let addr = spawn();
    let bodies: [&[u8]; 3] = [br#"{"table":"docs"}"#, br#"{"op":1}"#, br#"[]"#];
    for body in bodies {
        let resp = query(addr, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

// --- 非漏えい: いずれの応答本文もテナント ID・トークンを含まない ----------

#[test]
fn responses_do_not_leak_tenant_or_token() {
    let addr = spawn();
    let resp = query(addr, br#"{"op":"begin","table":"docs"}"#);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(!text.contains("tenant-a"));
    assert!(!text.contains("pw-alice"));
}
