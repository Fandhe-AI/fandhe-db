//! `POST /v1/query`（`op: "insert"`）の契約全体（`operation_id` 必須化・
//! 台帳照合による再送判定・INDEX-4 処理量上限・TABLE-12 同一テナント内 `id`
//! 衝突・RLS-9 秘匿）を production ルータ経由（生バイトクライアント）で
//! 固定する層 A 結合テスト（Issue #773・TASK-178。対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-6・
//! `docs/spec/04-behavior/sql-surface.md` SQL-10・
//! `docs/spec/04-behavior/indexing.md` INDEX-4・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10・
//! `docs/spec/04-behavior/data-model.md` TABLE-12・
//! `docs/spec/04-behavior/rls.md` RLS-9）。
//!
//! ## 役割分担（重複再検証をしない）
//!
//! - `crates/wire-server/src/http/query/insert.rs` 内の unit tests: `bind_rows`
//!   境界・`execute` レベルの `23502`／`23505`／`22023` を検証済み。本ファイルは
//!   ルータ・HTTP フレーミングを経由した **wire 越し** の観測に徹する
//!   （`wire_insert_operation_id.rs` ⇄ `sql_operation_id.rs` と同じ関係）。
//! - `tests/nosql6_tenant_row_id_scope.rs`（Issue #772）: 成功応答の形・複数行
//!   件数・RLS-9 成功経路のバイト同一性・TABLE-12 同一テナント重複の `23505`
//!   （非漏えい込み）を固定済み。本ファイルは再検証せず、**エラー経路**の
//!   RLS-9 バイト同一性（他テナント seed 有無で `23505` 応答が不変）を補完する。
//! - `tests/wire_insert_operation_id.rs`: SQL wire 版の `23502`／`23505`／
//!   `22023`／`42601`。本ファイルは同じ seed・SQL 文を NoSQL 側の対として流用し、
//!   両表層のパリティ（`wire_code`・`message` 一致）を固定する。
//! - `crates/engine/tests/sql_insert_batch_public_api.rs`・`batch_limits.rs`:
//!   INDEX-4 各上限・判定順序（engine 公開 API）の確定オラクル。本ファイルは
//!   NoSQL 側の `54000` と `wire_code` 一致・副作用なしを固定する。
//!
//! **対象外**: NoSQL 版のレイテンシ分布区別不能性（層 B。
//! `wire_tenant_row_id_scope.rs::judge` 相当）は別 Issue（design doc 申し送り）。
//! `explain` フィールド（NOSQL-10・#765）。
//!
//! ## 役割分担の補足（SQL-16 結線後・Issue #863）
//!
//! SQL 表層へ複数行 `VALUES`（SQL-16・TASK-190・PR #978）が結線された後は、
//! HTTP `rows[]` と SQL 複数行 `VALUES` の両方が `sql::exec::
//! execute_insert_batch_with_schema` を共有する（第 2 の書き込み経路を作らない
//! 設計。`crates/engine/tests/insert_multi_row.rs` モジュール doc 参照）ため、
//! 本ファイルは両表層間の再送判定パリティ（SQL 複数行 → HTTP `rows[]`・
//! その逆方向の `23505`／`22023`）も固定する。INDEX-4 ②③④（1 行あたり・
//! バッチ合計のバイト量・チャンク総量）の SQL 経路固有テストは
//! `crates/engine/tests/insert_multi_row.rs` が担う（本ファイルは NoSQL 側の
//! `54000` と `wire_code` 一致・副作用なしに限定する）。
//!
//! ## RLS 注意（誤コピー防止）
//!
//! 読み戻しオラクル（[`read_back_ids`]）は必ず
//! `PolicyContext::with_visibilities(TENANT_A, [Public, Private])` を使う。
//! HTTP 経由の `insert` は常に `Private` 固定で書き込まれる。wire-server の
//! 認証導出点（`auth.rs::session_policy_context`）は pg wire・HTTP セッション
//! 発行の双方で `Public` ＋ 自テナントの `Private` を許可可視性とする
//! （RLS-11・TASK-195。read-your-writes）ため、production の SQL wire
//! セッションからも同一テナントであれば読み戻せる——本ファイルの
//! `read_back_ids` は wire を経由しない engine API 直呼び出しのオラクルで
//! あり、production の SQL wire 経路とは別の検証経路であることに注意する
//! （production の SQL wire 経由読み戻しは `crates/wire-server/tests/
//! rls11_read_your_writes.rs` が担う）。

use crate::common;
use crate::http_common;

use std::sync::Arc;

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::OperationId;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";

/// `docs(embedding VECTOR(3) NOT NULL, lang TEXT NULL)`。`lang` は
/// `wire_insert_operation_id.rs::insert_sql` が要求する列であり、SQL wire
/// パリティ（A4・B3〜B5・E1）を成立させるために必要。HTTP 側は `lang`
/// nullable のため省略でき、バイトサイズを厳密に制御したい INDEX-4 テスト
/// （C1〜C4）は `lang` を省略して `embedding` 列 12 バイト（`f32` × 3）のみで
/// 行サイズを決定的にする。
fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// `limits` 指定時は `Arc::new` の前に `with_batch_limits` を適用する
/// （INDEX-4 上限系テストは `BatchLimits::default()` の環境変数依存を避け、
/// 各テストが明示した値のみを使う）。
fn new_core(limits: Option<BatchLimits>) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql6-insert-layer-a");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    if let Some(limits) = limits {
        core = core.with_batch_limits(limits);
    }
    (Arc::new(core), guard)
}

/// `tenant-b`（id=100）・`tenant-c`（id=200）へ、HTTP を経由せず
/// `EngineCore::insert_row` で直接 1 行ずつ seed する
/// （`nosql6_tenant_row_id_scope.rs::seed_foreign_tenants` と同型）。
fn seed_foreign_tenants(core: &EngineCore) {
    let b = PolicyContext::new(TENANT_B).expect("valid tenant");
    let c = PolicyContext::new(TENANT_C).expect("valid tenant");
    core.insert_row(
        &b,
        TABLE,
        100,
        &RowInput {
            tenant_id: TENANT_B,
            visibility: Visibility::Public,
            embedding: &[0.0, 1.0, 0.0],
            metadata: b"seed-b",
        },
        Some(&OperationId::parse("seed-op-tenant-b").expect("valid operation_id")),
    )
    .expect("seed tenant-b row id=100");
    core.insert_row(
        &c,
        TABLE,
        200,
        &RowInput {
            tenant_id: TENANT_C,
            visibility: Visibility::Public,
            embedding: &[0.0, 0.0, 1.0],
            metadata: b"seed-c",
        },
        Some(&OperationId::parse("seed-op-tenant-c").expect("valid operation_id")),
    )
    .expect("seed tenant-c row id=200");
}

/// HTTP（`alice`／tenant-a）・SQL wire（同一 core・同一 `alice`）の両方を
/// 同一 `core` 上で起動する（`wire_insert_operation_id.rs::spawn_with_alice`・
/// `nosql6_tenant_row_id_scope.rs::spawn_alice_session` の合成）。
struct Both {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn spawn_both(core: Arc<EngineCore>) -> (Both, std::net::TcpStream) {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
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

    let sql_addr = common::spawn_server_with_engine(&users_path, core);
    let sql_stream = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");

    (Both { http_addr, token }, sql_stream)
}

/// `alice` のトークンで `/v1/query` へ `body` を送る便宜 API。
fn query(both: &Both, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", both.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        both.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// HTTP `insert` 要求本文（1 行）を組み立てる。`op_id` は 3 状態
/// （`Some(値)`／省略／JSON `null`）を [`OpId`] で切り替えられる。`lang` は
/// 省略可能（省略時は `Value::Null` として書き込まれ、行サイズ計算に寄与しない
/// ため INDEX-4 の C1〜C4 で使う）。
enum OpId<'a> {
    Value(&'a str),
    Omitted,
    Null,
    Empty,
}

fn insert_body_with(id: u64, lang: Option<&str>, op_id: OpId<'_>) -> Vec<u8> {
    let mut fields = format!(r#""id":{id},"embedding":[0.1,0.2,0.3]"#);
    if let Some(lang) = lang {
        fields.push_str(&format!(r#","lang":"{lang}""#));
    }
    let op_id_field = match op_id {
        OpId::Value(v) => format!(r#","operation_id":"{v}""#),
        OpId::Omitted => String::new(),
        OpId::Null => r#","operation_id":null"#.to_string(),
        OpId::Empty => r#","operation_id":"""#.to_string(),
    };
    format!(r#"{{"op":"insert","table":"docs","rows":[{{{fields}}}]{op_id_field}}}"#).into_bytes()
}

/// 正規の `operation_id` を持つ 1 行 insert 要求（`lang` 省略）。
fn insert_body(id: u64, op_id: &str) -> Vec<u8> {
    insert_body_with(id, None, OpId::Value(op_id))
}

/// `table` を差し替えた 1 行 insert 要求（D3 用）。
fn insert_body_for_table(table: &str, id: u64, op_id: OpId<'_>) -> Vec<u8> {
    let op_id_field = match op_id {
        OpId::Value(v) => format!(r#","operation_id":"{v}""#),
        OpId::Omitted => String::new(),
        OpId::Null => r#","operation_id":null"#.to_string(),
        OpId::Empty => r#","operation_id":"""#.to_string(),
    };
    format!(
        r#"{{"op":"insert","table":"{table}","rows":[{{"id":{id},"embedding":[0.1,0.2,0.3]}}]{op_id_field}}}"#
    )
    .into_bytes()
}

/// 複数行 insert 要求（`(id, lang)` の列）。
fn insert_body_multi(rows: &[(u64, &str)], op_id: &str) -> Vec<u8> {
    let rows_json: Vec<String> = rows
        .iter()
        .map(|(id, lang)| format!(r#"{{"id":{id},"embedding":[0.1,0.2,0.3],"lang":"{lang}"}}"#))
        .collect();
    format!(
        r#"{{"op":"insert","table":"docs","rows":[{}],"operation_id":"{op_id}"}}"#,
        rows_json.join(",")
    )
    .into_bytes()
}

/// `n` 行（`lang` 省略・`id` は 1 起点連番）を持つ insert 要求。C1〜C4（INDEX-4
/// 処理量上限）専用。行 1 件あたりの byte 寄与は `embedding` 列のみ（12 バイト。
/// `f32` × 3）で決定的。
fn insert_body_n_rows(n: u64, op_id: &str) -> Vec<u8> {
    let rows_json: Vec<String> = (1..=n)
        .map(|id| format!(r#"{{"id":{id},"embedding":[0.1,0.2,0.3]}}"#))
        .collect();
    format!(
        r#"{{"op":"insert","table":"docs","rows":[{}],"operation_id":"{op_id}"}}"#,
        rows_json.join(",")
    )
    .into_bytes()
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') USING OPERATION_ID '{op_id}'"
    )
}

/// SQL 表層の複数行 `VALUES`（SQL-16・TASK-190）文（`(id, lang)` の列）。
/// `insert_body_multi` と同じ行集合表現で SQL↔NoSQL 表層のパリティ検証に使う
/// （Issue #863）。
fn multi_row_sql(rows: &[(u64, &str)], op_id: &str) -> String {
    let values: Vec<String> = rows
        .iter()
        .map(|(id, lang)| format!("({id}, '[0.1,0.2,0.3]', '{lang}')"))
        .collect();
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES {} USING OPERATION_ID '{op_id}'",
        values.join(", ")
    )
}

/// 成功応答の本文を `(inserted, operation_id)` へ解析する
/// （`nosql6_tenant_row_id_scope.rs::parse_insert_success_body` と同型）。
fn parse_insert_success_body(resp: &HttpResponse) -> (u64, String) {
    assert_eq!(
        resp.status,
        200,
        "expected 200, body={:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let text = std::str::from_utf8(&resp.body).expect("body must be utf-8");
    let JsonValue::Object(mut top) = parse_json(text).expect("body must be valid json") else {
        panic!("top level must be an object: {text}");
    };
    let inserted = match top.remove("inserted") {
        Some(JsonValue::Number(n)) => n.as_f64() as u64,
        other => panic!("inserted must be a number, got {other:?}"),
    };
    let operation_id = match top.remove("operation_id") {
        Some(JsonValue::String(s)) => s,
        other => panic!("operation_id must be a string, got {other:?}"),
    };
    (inserted, operation_id)
}

/// `core` を tenant-a（`with_visibilities([Public, Private])`）で読み戻し、
/// 可視な行 `id` の一覧を返す（wire を経由しない engine API 直呼び出しの
/// オラクル。モジュール doc「RLS 注意」参照）。
fn read_back_ids(core: &EngineCore) -> Vec<u64> {
    let ctx = PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant-a ctx");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT id FROM docs LIMIT 100")
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    result.rows.iter().map(|row| row.id).collect()
}

/// `core.operation_recorded` の薄いラッパー（tenant-a 固定）。
fn ledger_state(core: &EngineCore, op_id: &str) -> LedgerLookup {
    let ctx = PolicyContext::new(TENANT_A).expect("valid tenant ctx");
    let parsed = OperationId::parse(op_id).expect("valid operation_id");
    core.operation_recorded(&ctx, TABLE, &parsed)
        .expect("ledger lookup must not error")
}

/// `Date` ヘッダをマスクした応答全体（G1 用。ヘッダ集合・本文の完全一致検証に使う）。
fn masked_response_bytes(resp: &HttpResponse, raw: &[u8]) -> Vec<u8> {
    let date_value = resp
        .header("date")
        .expect("response must carry a Date header")
        .to_string();
    let text = std::str::from_utf8(raw).expect("response must be utf-8");
    text.replace(&date_value, &"X".repeat(date_value.len()))
        .into_bytes()
}

// --- A: `operation_id` 必須化（RECOVER-1・`23502`） -------------------------

#[test]
fn missing_operation_id_is_23502() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core.clone());

    let resp = query(&both, &insert_body_with(1, None, OpId::Omitted));
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23502");
    assert!(
        read_back_ids(&core).is_empty(),
        "must not write on rejection"
    );

    // 副作用なしの非 vacuous 証跡: 同じ id へ正規 operation_id で insert すると
    // 通ること。
    let after = query(&both, &insert_body(1, "nosql-op-after-missing"));
    let (inserted, _) = parse_insert_success_body(&after);
    assert_eq!(inserted, 1);
}

#[test]
fn null_operation_id_is_23502() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core.clone());

    let resp = query(&both, &insert_body_with(1, None, OpId::Null));
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23502");
    assert!(read_back_ids(&core).is_empty());

    let after = query(&both, &insert_body(1, "nosql-op-after-null"));
    let (inserted, _) = parse_insert_success_body(&after);
    assert_eq!(inserted, 1);
}

#[test]
fn empty_operation_id_is_23502() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core.clone());

    let resp = query(&both, &insert_body_with(1, None, OpId::Empty));
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23502");
    assert!(read_back_ids(&core).is_empty());

    let after = query(&both, &insert_body(1, "nosql-op-after-empty"));
    let (inserted, _) = parse_insert_success_body(&after);
    assert_eq!(inserted, 1);
}

/// HTTP 側の `23502` が SQL wire の `MissingOperationId` と同じ `message` を
/// 共有する（`ClassifiedError::client_message` を両表層が共有することの機械
/// 検証）。
#[test]
fn missing_operation_id_matches_sql_wire_23502() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let resp = query(&both, &insert_body_with(1, None, OpId::Omitted));
    let message = http_common::error_message_of(&resp);
    assert_eq!(http_common::wire_code_of(&resp), "23502");

    common::send_simple_query(
        &mut sql,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja')",
    );
    common::expect_error_response_with_sqlstate_and_message(&mut sql, "23502", &message);
    common::read_ready_for_query(&mut sql);
}

// --- B: 台帳照合による再送判定（RECOVER-10・`23505`／`22023`） --------------

#[test]
fn http_resend_same_content_is_23505() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core);

    let first = query(&both, &insert_body(1, "nosql-op-resend-same"));
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 1);

    let second = query(&both, &insert_body(1, "nosql-op-resend-same"));
    assert_eq!(second.status, 409, "body={second:?}");
    assert_eq!(http_common::wire_code_of(&second), "23505");
    http_common::assert_message_does_not_echo(&second, "nosql-op-resend-same");
    http_common::assert_message_does_not_echo(&second, TENANT_A);
}

#[test]
fn http_resend_different_content_is_22023() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core.clone());

    let first = query(
        &both,
        &insert_body_with(1, Some("ja"), OpId::Value("nosql-op-mismatch")),
    );
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 1);

    // id・lang が異なる別内容の文へ同一 operation_id を使い回す。
    let second = query(
        &both,
        &insert_body_with(2, Some("en"), OpId::Value("nosql-op-mismatch")),
    );
    assert_eq!(second.status, 400, "body={second:?}");
    assert_eq!(http_common::wire_code_of(&second), "22023");
    http_common::assert_message_does_not_echo(&second, "nosql-op-mismatch");
    assert!(
        !read_back_ids(&core).contains(&2),
        "content-mismatch resend must not write the second row"
    );
}

#[test]
fn sql_then_http_resend_same_content_is_23505() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    common::send_simple_query(&mut sql, &insert_sql(1, "ja", "nosql-op-sql-then-http"));
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "INSERT 0 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        &insert_body_with(1, Some("ja"), OpId::Value("nosql-op-sql-then-http")),
    );
    assert_eq!(resp.status, 409, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

#[test]
fn http_then_sql_resend_same_content_is_23505() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let first = query(
        &both,
        &insert_body_with(1, Some("ja"), OpId::Value("nosql-op-http-then-sql")),
    );
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 1);

    common::send_simple_query(&mut sql, &insert_sql(1, "ja", "nosql-op-http-then-sql"));
    common::expect_error_response_with_sqlstate(&mut sql, "23505");
    common::read_ready_for_query(&mut sql);
}

#[test]
fn sql_then_http_different_content_is_22023() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    common::send_simple_query(&mut sql, &insert_sql(1, "ja", "nosql-op-sql-http-mismatch"));
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "INSERT 0 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        &insert_body_with(2, Some("en"), OpId::Value("nosql-op-sql-http-mismatch")),
    );
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22023");
}

#[test]
fn http_then_sql_different_content_is_22023() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let first = query(
        &both,
        &insert_body_with(1, Some("ja"), OpId::Value("nosql-op-http-sql-mismatch")),
    );
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 1);

    common::send_simple_query(&mut sql, &insert_sql(2, "en", "nosql-op-http-sql-mismatch"));
    common::expect_error_response_with_sqlstate(&mut sql, "22023");
    common::read_ready_for_query(&mut sql);
}

#[test]
fn multi_row_batch_resend_same_content_is_23505_and_mismatch_is_22023() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core.clone());

    let rows = [(1u64, "ja"), (2u64, "en")];
    let first = query(&both, &insert_body_multi(&rows, "nosql-op-multi-resend"));
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 2);

    // 同一内容の再送 → 23505。
    let resend_same = query(&both, &insert_body_multi(&rows, "nosql-op-multi-resend"));
    assert_eq!(resend_same.status, 409, "body={resend_same:?}");
    assert_eq!(http_common::wire_code_of(&resend_same), "23505");

    // 1 行だけ内容を変えた再送 → 22023。
    let mismatched = [(1u64, "ja"), (2u64, "de")];
    let resend_mismatch = query(
        &both,
        &insert_body_multi(&mismatched, "nosql-op-multi-resend"),
    );
    assert_eq!(resend_mismatch.status, 400, "body={resend_mismatch:?}");
    assert_eq!(http_common::wire_code_of(&resend_mismatch), "22023");

    assert_eq!(read_back_ids(&core).len(), 2, "no extra rows from resends");
}

// --- SQL 複数行 VALUES ⇄ HTTP rows[] の再送判定パリティ（Issue #863。
//     SQL-16 結線後は「共有 Rust 入口への一致」に留まらず、SQL 表層の複数行
//     `VALUES` 文そのものとの一致を主張できる） ----------------------------

#[test]
fn sql_multi_row_then_http_rows_resend_same_content_is_23505() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let rows = [(1u64, "ja"), (2u64, "en")];
    common::send_simple_query(
        &mut sql,
        &multi_row_sql(&rows, "nosql-op-sql-multi-then-http"),
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "INSERT 0 2");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        &insert_body_multi(&rows, "nosql-op-sql-multi-then-http"),
    );
    assert_eq!(resp.status, 409, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
}

#[test]
fn http_rows_then_sql_multi_row_resend_same_content_is_23505() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let rows = [(1u64, "ja"), (2u64, "en")];
    let first = query(
        &both,
        &insert_body_multi(&rows, "nosql-op-http-then-sql-multi"),
    );
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 2);

    common::send_simple_query(
        &mut sql,
        &multi_row_sql(&rows, "nosql-op-http-then-sql-multi"),
    );
    common::expect_error_response_with_sqlstate(&mut sql, "23505");
    common::read_ready_for_query(&mut sql);
}

#[test]
fn sql_multi_row_then_http_rows_different_content_is_22023() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let rows = [(1u64, "ja"), (2u64, "en")];
    common::send_simple_query(
        &mut sql,
        &multi_row_sql(&rows, "nosql-op-sql-multi-http-mismatch"),
    );
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "INSERT 0 2");
    common::read_ready_for_query(&mut sql);

    let mismatched = [(1u64, "ja"), (2u64, "de")];
    let resp = query(
        &both,
        &insert_body_multi(&mismatched, "nosql-op-sql-multi-http-mismatch"),
    );
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22023");
}

#[test]
fn http_rows_then_sql_multi_row_different_content_is_22023() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let rows = [(1u64, "ja"), (2u64, "en")];
    let first = query(
        &both,
        &insert_body_multi(&rows, "nosql-op-http-sql-multi-mismatch"),
    );
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 2);

    let mismatched = [(1u64, "ja"), (2u64, "de")];
    common::send_simple_query(
        &mut sql,
        &multi_row_sql(&mismatched, "nosql-op-http-sql-multi-mismatch"),
    );
    common::expect_error_response_with_sqlstate(&mut sql, "22023");
    common::read_ready_for_query(&mut sql);
}

// --- C: INDEX-4 処理量上限（`54000`） ---------------------------------------

fn limits(
    max_files: usize,
    max_file_body: usize,
    max_total: usize,
    max_chunks: usize,
) -> BatchLimits {
    BatchLimits {
        max_files_per_batch: max_files,
        max_file_body_bytes: max_file_body,
        max_batch_total_bytes: max_total,
        max_batch_chunks: max_chunks,
    }
}

#[test]
fn rows_over_max_files_per_batch_is_54000_without_side_effects() {
    let (core, _guard) = new_core(Some(limits(2, 10_000, 10_000, 10_000)));
    let (both, _sql) = spawn_both(core.clone());

    let resp = query(&both, &insert_body_n_rows(3, "nosql-op-too-many-files"));
    assert_eq!(resp.status, 413, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "54000");
    assert_eq!(
        ledger_state(&core, "nosql-op-too-many-files"),
        LedgerLookup::NotRecorded
    );
    assert!(read_back_ids(&core).is_empty());

    // 境界値（上限ちょうど＝2 行）は成功する。
    let ok = query(&both, &insert_body_n_rows(2, "nosql-op-too-many-files"));
    let (inserted, _) = parse_insert_success_body(&ok);
    assert_eq!(inserted, 2);
}

#[test]
fn rows_over_max_files_per_batch_matches_sql_batch_wire_code() {
    let (core, _guard) = new_core(Some(limits(2, 10_000, 10_000, 10_000)));
    let ctx = PolicyContext::new(TENANT_A).expect("valid tenant ctx");

    // ①（バッチあたり最大ファイル数）はテーブル参照・束縛より前に判定されるため
    // （`core.rs::execute_insert_sql_batch` 冒頭）、"documents" テーブル・埋め込み
    // 未接続でも到達する。文の中身は判定に無関係なプレースホルダでよい。
    let err = core
        .execute_insert_sql_batch(&ctx, &["INSERT 1", "INSERT 2", "INSERT 3"])
        .expect_err("must reject before parsing individual statements");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn rows_over_max_file_body_bytes_is_54000_without_side_effects() {
    // ②（1 行あたり最大本文サイズ）は `execute_bound_insert_in_session` 判定 6 が
    // 行ごとのバイト量（`embedding VECTOR(3)` = 12 バイト/行。`lang` 省略で
    // 0 バイト寄与）を `(0, row_bytes)` として `batch_limits::validate_batch_shape`
    // へ渡すことで行形にも適用される（`core.rs` 判定 6 のドキュメント参照。
    // codex-review 指摘: 他の C1〜C4 テストは max_file_body_bytes を 10_000 と
    // 十分大きく固定しているため②単体では発火しない。本テストは③④を
    // 十分大きくして②のみを発火させ、上限ちょうど（12）は成功・
    // 上限未満（11）は拒否・副作用なしであることを固定する）。
    let (core, _guard) = new_core(Some(limits(10, 11, 10_000, 10_000)));
    let (both, _sql) = spawn_both(core.clone());

    let resp = query(&both, &insert_body_n_rows(1, "nosql-op-file-body-bytes"));
    assert_eq!(resp.status, 413, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "54000");
    assert_eq!(
        ledger_state(&core, "nosql-op-file-body-bytes"),
        LedgerLookup::NotRecorded
    );
    assert!(read_back_ids(&core).is_empty());

    // 境界値（上限ちょうど＝12 バイト）は成功する。
    let (core, _guard) = new_core(Some(limits(10, 12, 10_000, 10_000)));
    let (both, _sql) = spawn_both(core.clone());
    let ok = query(&both, &insert_body_n_rows(1, "nosql-op-file-body-bytes"));
    let (inserted, _) = parse_insert_success_body(&ok);
    assert_eq!(inserted, 1);
}

#[test]
fn rows_over_max_batch_total_bytes_is_54000() {
    // `embedding VECTOR(3)` = 12 バイト/行（`lang` 省略で 0 バイト寄与）。
    // 1 行（12 バイト）は通り、2 行（24 バイト）は max_batch_total_bytes=20 を
    // 超えて拒否される。max_file_body_bytes は十分大きくして③のみを発火させる。
    let (core, _guard) = new_core(Some(limits(10, 10_000, 20, 10_000)));
    let (both, _sql) = spawn_both(core.clone());

    let resp = query(&both, &insert_body_n_rows(2, "nosql-op-total-bytes"));
    assert_eq!(resp.status, 413, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "54000");
    assert_eq!(
        ledger_state(&core, "nosql-op-total-bytes"),
        LedgerLookup::NotRecorded
    );
    assert!(read_back_ids(&core).is_empty());

    let ok = query(&both, &insert_body_n_rows(1, "nosql-op-total-bytes"));
    let (inserted, _) = parse_insert_success_body(&ok);
    assert_eq!(inserted, 1);
}

#[test]
fn rows_over_max_batch_chunks_is_54000() {
    // 「1 行 = 1 チャンク」読み替え。max_batch_chunks=1 で 2 行は超過。
    let (core, _guard) = new_core(Some(limits(10, 10_000, 10_000, 1)));
    let (both, _sql) = spawn_both(core.clone());

    let resp = query(&both, &insert_body_n_rows(2, "nosql-op-chunks"));
    assert_eq!(resp.status, 413, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "54000");
    assert_eq!(
        ledger_state(&core, "nosql-op-chunks"),
        LedgerLookup::NotRecorded
    );
    assert!(read_back_ids(&core).is_empty());

    let ok = query(&both, &insert_body_n_rows(1, "nosql-op-chunks"));
    let (inserted, _) = parse_insert_success_body(&ok);
    assert_eq!(inserted, 1);
}

// --- D: 束縛・カタログ検証（`22000`／`23505`／`42P01`） ---------------------

#[test]
fn empty_rows_is_22000() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core.clone());

    let body = br#"{"op":"insert","table":"docs","rows":[],"operation_id":"nosql-op-empty-rows"}"#;
    let resp = query(&both, body);
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "22000");
    assert_eq!(
        ledger_state(&core, "nosql-op-empty-rows"),
        LedgerLookup::NotRecorded
    );
}

#[test]
fn duplicate_id_within_batch_is_23505() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core.clone());

    let body =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]},{"id":1,"embedding":[0.4,0.5,0.6]}],"operation_id":"nosql-op-dup-in-batch"}"#;
    let resp = query(&both, body);
    assert_eq!(resp.status, 409, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
    assert!(read_back_ids(&core).is_empty());
}

#[test]
fn unknown_table_is_42p01_on_both_surfaces() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    let resp = query(
        &both,
        &insert_body_for_table("nope", 1, OpId::Value("nosql-op-nope")),
    );
    assert_eq!(resp.status, 404, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42P01");

    common::send_simple_query(
        &mut sql,
        "INSERT INTO nope (id, embedding) VALUES (1, '[0.1,0.2,0.3]') USING OPERATION_ID 'nosql-op-nope-sql'",
    );
    common::expect_error_response_with_sqlstate(&mut sql, "42P01");
    common::read_ready_for_query(&mut sql);
}

/// 判定順序: `operation_id` 省略 + 未存在テーブルは `23502` が先
/// （`EngineCore::execute_bound_insert_in_session` 判定 1 が判定 4・5
/// 〔スキーマ取得〕より前）。HTTP 経由でも同じ順序であることを固定する。
#[test]
fn missing_operation_id_precedes_unknown_table_check() {
    let (core, _guard) = new_core(None);
    let (both, _sql) = spawn_both(core);

    let resp = query(&both, &insert_body_for_table("nope", 1, OpId::Omitted));
    assert_eq!(resp.status, 400, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23502");
}

// --- E: TABLE-12 同一テナント内 `id` 衝突（`23505`） -------------------------

#[test]
fn same_tenant_id_conflict_matches_sql_wire_23505_and_message() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    // HTTP → SQL 方向。
    let http_first = query(
        &both,
        &insert_body_with(42, Some("ja"), OpId::Value("nosql-op-e1-http")),
    );
    let (inserted, _) = parse_insert_success_body(&http_first);
    assert_eq!(inserted, 1);

    common::send_simple_query(&mut sql, &insert_sql(42, "en", "nosql-op-e1-sql"));
    common::expect_error_response_with_sqlstate_and_message(
        &mut sql,
        "23505",
        "row id already exists",
    );
    common::read_ready_for_query(&mut sql);
}

#[test]
fn sql_then_http_id_conflict_is_23505() {
    let (core, _guard) = new_core(None);
    let (both, mut sql) = spawn_both(core);

    // SQL → HTTP 方向。
    common::send_simple_query(&mut sql, &insert_sql(43, "ja", "nosql-op-e1-sql-first"));
    let tag = common::read_command_complete(&mut sql);
    assert_eq!(tag, "INSERT 0 1");
    common::read_ready_for_query(&mut sql);

    let resp = query(
        &both,
        &insert_body_with(43, Some("en"), OpId::Value("nosql-op-e1-http-second")),
    );
    assert_eq!(resp.status, 409, "body={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23505");
    assert_eq!(
        http_common::error_message_of(&resp),
        "row id already exists"
    );
}

// --- G: RLS-9 応答バイト列の同一性（重複拒否時） -----------------------------

/// 他テナントが保持する行の有無で、同一テナント内重複（`23505`）応答バイト列
/// （ステータス・ヘッダ集合・本文。`Date` を除く）が完全一致すること。
/// オラクル: `crates/engine/tests/sql_operation_id.rs::
/// error_response_of_same_tenant_conflict_is_identical_regardless_of_other_tenant_rows`。
#[test]
fn rls9_conflict_error_bytes_are_identical_regardless_of_foreign_tenant_rows() {
    let (core_without, _guard_without) = new_core(None);
    let (both_without, _sql_without) = spawn_both(core_without);
    let first = query(
        &both_without,
        &insert_body_with(42, Some("ja"), OpId::Value("nosql-op-g1-first")),
    );
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 1);
    let resp_without = query(
        &both_without,
        &insert_body_with(42, Some("en"), OpId::Value("nosql-op-g1-second")),
    );
    assert_eq!(resp_without.status, 409, "body={resp_without:?}");
    assert_eq!(http_common::wire_code_of(&resp_without), "23505");

    let (core_with, _guard_with) = new_core(None);
    seed_foreign_tenants(&core_with);
    let (both_with, _sql_with) = spawn_both(core_with);
    let first = query(
        &both_with,
        &insert_body_with(42, Some("ja"), OpId::Value("nosql-op-g1-first")),
    );
    let (inserted, _) = parse_insert_success_body(&first);
    assert_eq!(inserted, 1);
    let resp_with = query(
        &both_with,
        &insert_body_with(42, Some("en"), OpId::Value("nosql-op-g1-second")),
    );
    assert_eq!(resp_with.status, 409, "body={resp_with:?}");
    assert_eq!(http_common::wire_code_of(&resp_with), "23505");

    assert_eq!(resp_with.status, resp_without.status);
    let mut headers_with: Vec<(String, String)> = resp_with
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("date"))
        .cloned()
        .collect();
    let mut headers_without: Vec<(String, String)> = resp_without
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("date"))
        .cloned()
        .collect();
    headers_with.sort();
    headers_without.sort();
    assert_eq!(
        headers_with, headers_without,
        "response headers (excluding Date) must match"
    );

    let masked_with = masked_response_bytes(&resp_with, &resp_with.body);
    let masked_without = masked_response_bytes(&resp_without, &resp_without.body);
    assert_eq!(
        masked_with, masked_without,
        "conflict response body bytes must be indistinguishable regardless of whether another \
         tenant holds unrelated rows"
    );

    let body_str = String::from_utf8_lossy(&resp_with.body);
    assert!(
        !body_str.contains(TENANT_B) && !body_str.contains(TENANT_C),
        "error response must not leak a foreign tenant identifier: {body_str:?}"
    );
}

// --- H: セッション健全性 ------------------------------------------------------

/// 一連の拒否（`23502`・`22023`・`54000`・`23505`）を送った後も、同一トークンの
/// 後続 insert が正常に成功すること（`nosql4_5_aggregate_wire_parity.rs::
/// rejections_do_not_poison_session_token` と同型）。
#[test]
fn rejections_do_not_poison_session_token() {
    let (core, _guard) = new_core(Some(limits(2, 10_000, 10_000, 10_000)));
    let (both, _sql) = spawn_both(core);

    let missing = query(&both, &insert_body_with(1, None, OpId::Omitted));
    assert_eq!(http_common::wire_code_of(&missing), "23502");

    let mismatch_first = query(
        &both,
        &insert_body_with(2, Some("ja"), OpId::Value("nosql-op-h1-mismatch")),
    );
    let (inserted, _) = parse_insert_success_body(&mismatch_first);
    assert_eq!(inserted, 1);
    let mismatch = query(
        &both,
        &insert_body_with(3, Some("en"), OpId::Value("nosql-op-h1-mismatch")),
    );
    assert_eq!(http_common::wire_code_of(&mismatch), "22023");

    let too_many = query(&both, &insert_body_n_rows(3, "nosql-op-h1-too-many"));
    assert_eq!(http_common::wire_code_of(&too_many), "54000");

    let dup_first = query(
        &both,
        &insert_body_with(4, None, OpId::Value("nosql-op-h1-dup")),
    );
    let (inserted, _) = parse_insert_success_body(&dup_first);
    assert_eq!(inserted, 1);
    let dup_second = query(
        &both,
        &insert_body_with(4, None, OpId::Value("nosql-op-h1-dup-2")),
    );
    assert_eq!(http_common::wire_code_of(&dup_second), "23505");

    let after = query(&both, &insert_body(5, "nosql-op-h1-after"));
    let (inserted_after, _) = parse_insert_success_body(&after);
    assert_eq!(inserted_after, 1);
}
