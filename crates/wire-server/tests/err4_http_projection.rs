//! ERR-4 分類境界 (a)〜(f) の全類型について、`wire_code` と HTTP ステータス
//! の射影（[`wire_server::http::status::http_status`]）が実要求から誘発した
//! 応答と一致することを 1 類型 1 テストで網羅検証する層 A 結合テスト
//! （Issue #775・TASK-180。対象ビヘイビア ERR-4。ポインタ:
//! `docs/spec/05-tasks.md` TASK-180・`docs/spec/04-behavior/error-format.md`
//! ERR-4）。関連: HTTP-2/3/11/12（TASK-173）・HTTP-4〜8（TASK-174）・
//! NOSQL-1〜9（TASK-175〜179）・ERR-5（TASK-153）。
//!
//! 個々の類型はこれまで各 Issue の層 A テスト（`http2_framing.rs`・
//! `http11_limits.rs`・`http4_session*.rs`・`nosql2_search.rs` 等）に散在し、
//! 一部は `wire_code` のみを検証し HTTP ステータスまでは検証していなかった。
//! 本ファイルは「実要求で誘発 → `wire_code` と HTTP ステータスの組が射影表
//! （`status.rs::EXPECTED` と同値の [`EXPECTED_STATUS`]）と一致」を 1 箇所に
//! 集約し、あわせて「新規 `wire_code` が増えていない」ことを
//! [`err4_projection_table_is_closed_over_all_error_classes`] で機械的に
//! 固定する。production コードは変更しない（テスト専任）。
//!
//! 到達不能類型（`42501`・`P0002`・`34000`・`42701`）の扱い: NoSQL 表層は
//! テナントをセッション（`SessionPrincipal::policy_context()`）からのみ
//! 導出し、クライアント自己申告の `tenant_id` 相当値は JSON／ヘッダ／パス
//! いずれの位置でも `42601` で先に拒否する（`gate.rs`・
//! `session/middleware.rs`・`router.rs`）ため、`ForbiddenTenantMismatch`
//! （`42501`）を実要求から誘発する経路が構造的に存在しない。`RowNotFound`
//! （`P0002`）に対応する op（更新・削除系）も NoSQL 表層の許可リストに無い。
//! `InvalidCursorName`（`34000`。WIRE-15・TASK-218）はカーソル
//! （`DECLARE`／`FETCH`／`CLOSE`）専用の分類で、NoSQL 表層の `op` 許可
//! リストにカーソル操作が無いため実要求からは到達しない。`DuplicateColumn`
//! （`42701`。`ALTER TABLE ADD COLUMN` の列名重複。TASK-202・SQL-23・
//! Issue #900）に対応する `op` も NoSQL 表層の許可リストに無い（DDL は
//! NoSQL 表層の対象外）。これらはテナント境界の検査を緩める・バイパスする
//! production 経路を新設せず（`.claude/rules/security.md` P0）、production
//! の応答エンコーダ（`http::response::encode_error`。ルータ・各 op ハンドラ
//! が実際に使う関数）を通したバイト列を実応答と同じパーサ
//! （`http_common::parse_single_response`）で解析し、射影のみを検証する
//! （[`err4_f_unreachable_classes_project_via_production_encoder`]）。
//!
//! `data` キー付き `XX000`（緊急応答。`RECOVER-5` (3)・ERR-5 ポインタ）も
//! 同様に実要求からは到達不能: `http::response::encode_error_may_be_committed`
//! を呼び出す production 経路が `http/` 配下に存在せず、commit 後 panic は
//! RECOVER-5 の既存ガードが abort に倒すため（`http_insert_response_boundary.rs`
//! で検証済み）。通常応答が `data` キーを含まないことは
//! [`assert_projected`] が全テストで機械的に固定する。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::ErrorClass;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;
use wire_server::http::status::http_status;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            // `path`／`body` は `EngineCore::dictionary_snapshot`
            // （TASK-109・PLAN-5。`USING PLAN`／`plan` 検索が内部で要求する
            // 辞書スナップショット抽出）の non-nullable text 列要件を満たす
            // ために必要（`err4_f_internal_error_projects_xx000_to_500` が
            // `plan` 検索を誘発するため）。
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// 空の `docs` テーブルを持つスローアウェイ `EngineCore`。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("err4-http-projection");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `id` 桁あふれ（`22003`）専用フィクスチャ（`nosql4_aggregate.rs::
/// new_core_overflow` と同型）。
fn new_core_overflow() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("err4-http-projection-overflow");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant ctx");
    for id in [u64::MAX, 1] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("overflow-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text("ja".to_string()),
                Value::Text(format!("docs/{id}.md")),
                Value::Text("overflow content".to_string()),
            ],
            &op_id,
        )
        .expect("insert overflow row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `insert` の件数上限（INDEX-4）を小さく固定したスローアウェイ `EngineCore`
/// （環境変数の実値に依存せず非 vacuous に誘発するため）。
fn new_core_with_small_batch_limit(max_files: usize) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("err4-http-projection-batch-limit");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(
        BatchLimits {
            max_files_per_batch: max_files,
            ..Default::default()
        },
    );
    (Arc::new(core), guard)
}

/// `FOREIGN KEY` 付きの親子テーブル（TABLE-17・TASK-205、Issue #907）を持つ
/// スローアウェイ `EngineCore`。`FOREIGN KEY` の宣言面は SQL 表層の `CREATE TABLE`
/// のみ（NoSQL `op` 語彙に DDL は無い）のため、DDL 実行権限を付与したセッションで
/// production の SQL 経路からテーブルを作る。宣言済みテーブルへの NoSQL
/// `insert`／`update`／`delete` は engine 内の単一検査点を通るため `23503` が
/// 実要求から到達可能になる。
fn new_core_with_foreign_key() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("err4-http-projection-fk");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant ctx");
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE fk_parent (embedding VECTOR(2))",
        "CREATE TABLE fk_child (embedding VECTOR(2), parent_id BIGINT REFERENCES fk_parent)",
    ] {
        core.execute_sql_in_session(&ctx, &mut session, ddl)
            .expect("fk fixture DDL must succeed");
    }
    (Arc::new(core), guard)
}

fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
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

fn post(addr: SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
    let auth = format!("Bearer {token}");
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth),
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

/// `alice`（tenant-a）で都度ログインしてから要求を送る便宜 API。
fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

/// `status.rs::EXPECTED`（`#[cfg(test)]` 内で外部から参照不可）と同値の
/// 期待表。両者の乖離は [`err4_projection_table_is_closed_over_all_error_classes`]
/// が `http_status` 経由で検出する。
const EXPECTED_STATUS: [(&str, u16); 34] = [
    ("22000", 400),
    ("28P01", 401),
    ("28000", 401),
    ("42501", 403),
    ("34000", 404),
    ("42P01", 404),
    ("P0002", 404),
    ("23505", 409),
    ("23502", 400),
    ("54000", 413),
    ("53300", 503),
    ("0A000", 501),
    ("42601", 400),
    ("08P01", 400),
    ("XX000", 500),
    ("22003", 400),
    ("22023", 400),
    ("22008", 400),
    ("22P02", 400),
    ("55P03", 503),
    ("25000", 400),
    ("25001", 400),
    ("25P01", 400),
    ("25P02", 400),
    // `DuplicateTable`（`42P07`。SQL-23・TASK-85、Issue #899）は
    // `UniqueViolation` と同じ「対象が既に存在する」意味論のため同じ 409。
    // `CREATE VIEW`（TABLE-18・SQL-23・TASK-205、Issue #909）の名前衝突も
    // 同じ分類・同じステータスを共有する。
    ("42P07", 409),
    ("42701", 400),
    // TABLE-18・SQL-23・TASK-205（Issue #909）: `CREATE VIEW`／`DROP VIEW` が
    // 新設する残り 2 分類。SQL 表層専用の DDL であり NoSQL `op` 許可リストには
    // 含めない（本ファイル冒頭 doc の「到達不能類型」と同じ扱い。
    // production の応答エンコーダ経由で射影のみ検証する。下記
    // `err4_f_unreachable_classes_project_via_production_encoder` 参照）。
    ("2BP01", 400),
    ("42809", 400),
    // TASK-206・INDEX-7・SQL-23（Issue #908）: `CREATE INDEX`／`DROP INDEX` が
    // 新設する 2 分類（索引不在・参照列不在）。索引名の衝突は `42P07` を共有する。
    // SQL 表層専用の DDL であり NoSQL `op` 許可リストには含めないため、
    // production の応答エンコーダ経由で射影のみ検証する。
    ("42704", 400),
    ("42703", 400),
    // `CheckViolation`（`23514`。TABLE-16・TASK-204、Issue #906）は
    // `UniqueViolation` と同じ「対象の状態と矛盾する」意味論のため同じ 409。
    ("23514", 409),
    // `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）: 参照整合性違反（`23503`）は
    // `UniqueViolation`／`CheckViolation` と同じ 409、宣言の不正（`42830`）は
    // SQL 表層専用の DDL の分類で 400。
    ("23503", 409),
    ("42830", 400),
    // `AmbiguousColumn`（`42702`。SQL-28・RLS-10、Issue #924）: 複数テーブル
    // 参照スコープでの非修飾列の曖昧解決。本 Issue では SQL 表層が JOIN・複数
    // FROM を受理しないため到達不能で、`err4_f_unreachable_classes_project_via_production_encoder`
    // が射影のみを検証する。
    ("42702", 400),
];

/// (a)〜(f) 全類型の共通アサーション: `wire_code` が逆引き可能・射影ステータス
/// と実応答ステータスが一致・`EXPECTED_STATUS`（射影関数とは独立の期待表）
/// とも一致・`code` ラベルが分類と一致・`message` が非空・トップレベル
/// `error` オブジェクトに `data` キーが無いことを固定する。
fn assert_projected(resp: &HttpResponse, expected_wire_code: &str) {
    let wire_code = http_common::wire_code_of(resp);
    assert_eq!(
        wire_code, expected_wire_code,
        "unexpected wire_code (resp={resp:?})"
    );
    let class = ErrorClass::from_wire_code(&wire_code)
        .unwrap_or_else(|| panic!("wire_code {wire_code:?} must be a known ErrorClass"));
    assert_eq!(
        resp.status,
        http_status(class),
        "response status must match the http_status projection for {wire_code:?}"
    );
    let (_, expected_status) = EXPECTED_STATUS
        .iter()
        .find(|(code, _)| *code == expected_wire_code)
        .unwrap_or_else(|| panic!("{expected_wire_code:?} missing from EXPECTED_STATUS"));
    assert_eq!(
        resp.status, *expected_status,
        "response status must match the independent EXPECTED_STATUS table for {wire_code:?}"
    );
    assert_eq!(
        http_common::error_code_of(resp),
        class.label(),
        "error.code must match the ErrorClass label"
    );
    assert!(
        !http_common::error_message_of(resp).is_empty(),
        "error.message must not be empty"
    );
    let body_str = std::str::from_utf8(&resp.body).expect("body must be utf-8");
    let parsed = engine::json::parse_json(body_str).expect("body must be valid json");
    let engine::json::JsonValue::Object(mut top) = parsed else {
        panic!("top level must be an object: {body_str}");
    };
    let error_value = top.remove("error").expect("missing error key");
    let engine::json::JsonValue::Object(error_obj) = error_value else {
        panic!("error value must be an object: {body_str}");
    };
    assert!(
        !error_obj.contains_key("data"),
        "normal error response must not carry a data key: {body_str}"
    );
    assert_eq!(
        resp.header("content-type"),
        Some(wire_server::http::response::CONTENT_TYPE_JSON_UTF8)
    );
    assert_eq!(resp.header("connection"), Some("close"));
}

// --- R7: 射影表が ErrorClass::ALL 全体を閉じて覆うことの機械検証 -----------

const _: () = assert!(ErrorClass::ALL.len() == 35);

/// `23502` を共有する分類（ERR-6・TABLE-16・TASK-204、Issue #904）。
/// [`err4_projection_table_is_closed_over_all_error_classes`] がこの組にだけ
/// 「`wire_code` からの厳密往復」を免除する（`EXPECTED_STATUS` は wire_code
/// 単位の一意テーブルのままで、共有側はどちらも同じステータス 400 のため
/// テーブル自体は増やさない）。
const SHARED_23502_CLASSES: [ErrorClass; 2] =
    [ErrorClass::MissingOperationId, ErrorClass::NotNullViolation];

#[test]
fn err4_projection_table_is_closed_over_all_error_classes() {
    // `EXPECTED_STATUS` は一意な `wire_code` 単位の期待表。`23502` は 2 分類が
    // 共有するため、一意な `wire_code` の数は `ErrorClass::ALL` より 1 小さい。
    let unique_wire_codes: std::collections::HashSet<&str> =
        ErrorClass::ALL.iter().map(|c| c.wire_code()).collect();
    assert_eq!(EXPECTED_STATUS.len(), unique_wire_codes.len());
    for class in ErrorClass::ALL {
        let wire_code = class.wire_code();
        let (_, expected_status) = EXPECTED_STATUS
            .iter()
            .find(|(code, _)| *code == wire_code)
            .unwrap_or_else(|| panic!("{wire_code:?} missing from EXPECTED_STATUS"));
        assert_eq!(
            http_status(class),
            *expected_status,
            "http_status(class) must match EXPECTED_STATUS for {wire_code:?}"
        );
        let round_tripped = ErrorClass::from_wire_code(wire_code)
            .unwrap_or_else(|| panic!("{wire_code:?} must round-trip via from_wire_code"));
        if SHARED_23502_CLASSES.contains(&class) {
            // 共有 wire_code の逆引きは宣言順で最初の分類（MissingOperationId）
            // へ固定的に戻る契約（`ErrorClass::from_wire_code` の doc 参照）。
            assert_eq!(round_tripped, ErrorClass::MissingOperationId);
        } else {
            assert_eq!(round_tripped, class);
        }
    }
}

// --- (a) HTTP フレーミング不正 → 08P01／400 --------------------------------

#[test]
fn err4_a_malformed_framing_projects_08p01_to_400() {
    let (addr, _limiter) = http_common::spawn_http_listener(4, http_common::SERVER_READ_TIMEOUT);
    let request =
        b"GET /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n";
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        request,
        AfterWrite::HalfClose,
    ));
    http_common::assert_rejected(&resp, 400, "08P01");
    assert_projected(&resp, "08P01");
}

#[test]
fn err4_a_unknown_target_projects_08p01_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let token = login(addr, "alice", "pw-alice");
    let request = http_common::build_request(
        "/v1/unknown",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(
        http_common::error_message_of(&resp),
        http_common::ROUTER_PLACEHOLDER_MESSAGE,
        "resp={resp:?}"
    );
    assert_projected(&resp, "08P01");
}

// --- (b) JSON 構文不正・スキーマ違反 → 42601／400 --------------------------

#[test]
fn err4_b_json_syntax_error_projects_42601_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(addr, br#"{"op":"#);
    assert_projected(&resp, "42601");
}

#[test]
fn err4_b_schema_violation_projects_42601_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    // 未知キー。
    let resp = query_as_alice(addr, br#"{"op":"scan","table":"docs","limit":1,"extra":1}"#);
    assert_projected(&resp, "42601");

    // `tenant_id` キー付き（クライアント自己申告のテナント値は先頭で拒否）。
    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"tenant-a"}"#,
    );
    assert_projected(&resp, "42601");
}

// --- (c) op 語彙外 → 0A000／501 ---------------------------------------------
//
// 「語彙内だが未対応」の従来サブケース（aggregate の explain: true）は
// NOSQL-10（Issue #765）の実装完了により `42601` へ写像されるようになり
// 本クラスの例として成立しなくなったため撤去した（base ブランチ取り込みに
// 伴う意味論変化。PR #832）。同じ理由で `delete` op も本クラスの fixture
// から外した（Issue #875・NOSQL-12 で語彙へ加わったため、この fixture は
// もう語彙外の例にならない。`engine` 未接続時の `update`／`delete` を含む
// 全 op プレースホルダー応答は `crates/wire-server/src/http/query/gate.rs`
// の単体テスト（`valid_scan_reaches_placeholder_response_when_engine_is_not_connected`
// 等）で、`update`／`delete` の `engine` 接続済みでも placeholder に留まる
// 契約は `valid_update_and_delete_reach_placeholder_even_when_engine_is_connected`
// で別途固定済み）。

#[test]
fn err4_c_unsupported_op_projects_0a000_to_501() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    // 語彙外 op（`drop_table` は Issue #910 で語彙に加わったため、
    // NOSQL-13 対象外の index 系 DDL 相当を使う）。
    let resp = query_as_alice(addr, br#"{"op":"drop_index","table":"docs"}"#);
    assert_projected(&resp, "0A000");
}

// --- (d) 構造は受理されたが値不正 → 22000／22003／22023 ---------------------

#[test]
fn err4_d_invalid_value_projects_22000_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"mode":"fuzzy"}"#,
    );
    assert_projected(&resp, "22000");
}

#[test]
fn err4_d_numeric_overflow_projects_22003_to_400() {
    let (core, _guard) = new_core_overflow();
    let addr = spawn(core);
    let resp = query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"id"}]}"#,
    );
    assert_projected(&resp, "22003");
}

#[test]
fn err4_d_operation_id_content_mismatch_projects_22023_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let first = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"}],"operation_id":"err4-op-d"}"#;
    let resp = query_as_alice(addr, first);
    assert_eq!(resp.status, 200, "first insert must succeed: {resp:?}");

    // 同一 operation_id・異なる内容での再送。
    let second = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.9,0.9],"lang":"ja","path":"docs/1.md","body":"changed content"}],"operation_id":"err4-op-d"}"#;
    let resp = query_as_alice(addr, second);
    assert_projected(&resp, "22023");
}

// --- (e) 上限超過 → 54000／413 ----------------------------------------------

#[test]
fn err4_e_body_over_limit_projects_54000_to_413() {
    let (addr, _limiter) = http_common::spawn_http_listener(4, http_common::SERVER_READ_TIMEOUT);
    let over_limit = wire_server::http::body::MAX_BODY_LEN + 1;
    let request = format!(
        "POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {over_limit}\r\n\r\n"
    )
    .into_bytes();
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::KeepOpen,
    ));
    http_common::assert_rejected(&resp, 413, "54000");
    assert_projected(&resp, "54000");
}

#[test]
fn err4_e_filter_count_over_limit_projects_54000_to_413() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let mut items_json = String::from("[");
    for i in 0..=engine::declarative_filter::MAX_METADATA_FILTERS {
        if i > 0 {
            items_json.push(',');
        }
        items_json.push_str(&format!(r#"{{"column":"lang","op":"eq","value":"v{i}"}}"#));
    }
    items_json.push(']');
    let body = format!(r#"{{"op":"scan","table":"docs","limit":10,"filter":{items_json}}}"#);
    let resp = query_as_alice(addr, body.as_bytes());
    assert_projected(&resp, "54000");
}

#[test]
fn err4_e_insert_batch_over_limit_projects_54000_to_413() {
    let (core, _guard) = new_core_with_small_batch_limit(2);
    let addr = spawn(core);

    let body = br#"{"op":"insert","table":"docs","rows":[
        {"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"},
        {"id":2,"embedding":[0.2,0.3],"lang":"ja","path":"docs/2.md","body":"beta content"},
        {"id":3,"embedding":[0.3,0.4],"lang":"ja","path":"docs/3.md","body":"gamma content"}
        ],"operation_id":"err4-op-e-batch"}"#;
    let resp = query_as_alice(addr, body);
    assert_projected(&resp, "54000");
}

// --- (f) その他固定分類 ------------------------------------------------------

#[test]
fn err4_f_auth_required_projects_28000_to_401() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_projected(&resp, "28000");
}

#[test]
fn err4_f_auth_invalid_projects_28p01_to_401() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let body = br#"{"user":"alice","password":"wrong-password"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_projected(&resp, "28P01");
    // 誤資格の応答が実パスワードを反映しないこと（`.claude/rules/security.md`
    // の非漏えい方針）。
    http_common::assert_message_does_not_echo(&resp, "wrong-password");
}

#[test]
fn err4_f_undefined_table_projects_42p01_to_404() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(addr, br#"{"op":"scan","table":"missing","limit":1}"#);
    assert_projected(&resp, "42P01");
}

#[test]
fn err4_f_duplicate_operation_id_projects_23505_to_409() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"}],"operation_id":"err4-op-f-dup"}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "first insert must succeed: {resp:?}");

    // 同一 operation_id・同一内容での再送。
    let resp = query_as_alice(addr, body);
    assert_projected(&resp, "23505");
    http_common::assert_message_does_not_echo(&resp, "tenant-a");
}

#[test]
fn err4_f_missing_operation_id_projects_23502_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let missing =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"}]}"#;
    let resp = query_as_alice(addr, missing);
    assert_projected(&resp, "23502");

    let null_id = br#"{"op":"insert","table":"docs","rows":[{"id":2,"embedding":[0.1,0.2],"lang":"ja","path":"docs/2.md","body":"beta content"}],"operation_id":null}"#;
    let resp = query_as_alice(addr, null_id);
    assert_projected(&resp, "23502");

    let empty_id = br#"{"op":"insert","table":"docs","rows":[{"id":3,"embedding":[0.1,0.2],"lang":"ja","path":"docs/3.md","body":"gamma content"}],"operation_id":""}"#;
    let resp = query_as_alice(addr, empty_id);
    assert_projected(&resp, "23502");
}

#[test]
fn err4_f_connection_limit_projects_53300_to_503() {
    let (addr, limiter) = http_common::spawn_http_listener(1, Duration::from_secs(5));

    let held = std::net::TcpStream::connect(addr).expect("connect within capacity");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while limiter.active() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the held connection to be accepted"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // 上限超過接続は要求を読まずに 503 を返して即座にクローズする
    // （`http/listener.rs::accept_loop_with_handler` の拒否経路）。要求を
    // 書き込むと、既に応答してクローズ済みのソケットへの書き込みが
    // カーネル RST を誘発し、実装によっては未読の応答バイト列が失われて
    // `send_raw` が panic しうる（`tests/http_limits.rs` と同じ理由で
    // 「接続するだけで書き込まない」形にする）。
    let mut extra =
        std::net::TcpStream::connect(addr).expect("connect the connection-over-capacity socket");
    extra
        .set_read_timeout(Some(http_common::CLIENT_READ_TIMEOUT))
        .expect("set client read timeout");
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match std::io::Read::read(&mut extra, &mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) => panic!("unexpected read error while waiting for 503 response: {e:?}"),
        }
    }
    let resp = http_common::parse_single_response(&received);
    assert_projected(&resp, "53300");
    drop(held);
}

#[test]
fn err4_f_session_limit_projects_53300_to_503() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(
        &users_path,
        SessionStore::with_limits(1, Duration::from_secs(3600)),
    );

    let first = login(addr, "alice", "pw-alice");
    assert!(!first.is_empty());

    let body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_projected(&resp, "53300");
}

#[test]
fn err4_f_internal_error_projects_xx000_to_500() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","plan":"find content","limit":10}"#,
    );
    assert_projected(&resp, "XX000");
}

/// `42501`（テナント越境）・`P0002`（行不在）・`34000`（カーソル不在。
/// WIRE-15・TASK-218）は NoSQL 表層の実要求からは構造的に到達不能
/// （本ファイル冒頭 doc 参照）。`42P07`（`DuplicateTable`）・
/// `42701`（`DuplicateColumn`。SQL-23・TASK-85、Issue #899）は SQL 表層専用の
/// `CREATE TABLE` 分類であり、`2BP01`／`42809`
/// （TABLE-18・SQL-23・TASK-205、Issue #909）も同様——`CREATE VIEW`／
/// `DROP VIEW` は SQL 表層専用の DDL で、いずれも NoSQL `op` 許可リストに
/// 含まれない（`gate.rs`・`http/query/op.rs`）。明示トランザクション制御
/// （SQL-31・TASK-221。`25000`/`25001`/`25P01`/`25P02`）も NoSQL 表層の `op`
/// 許可リストにトランザクション制御が無いため同様に到達不能。`42704`／`42703`
/// （TASK-206・INDEX-7・SQL-23、Issue #908。`CREATE INDEX`／`DROP INDEX`）も
/// SQL 表層専用の DDL の分類で同様に到達不能。要求駆動ではなく、
/// production の応答エンコーダ（[`wire_server::http::response::encode_error`]。
/// ルータ・各 op ハンドラが実際に使う関数）を通したバイト列を実応答と同じパーサで
/// 解析し、射影のみを検証する。
#[test]
fn err4_f_unreachable_classes_project_via_production_encoder() {
    // `2BP01`／`42809`（TABLE-18・SQL-23・TASK-205、Issue #909）は
    // `CREATE VIEW`／`DROP VIEW` が SQL 表層専用の DDL（NoSQL-13 の対象外。
    // Issue #910 で `op` 許可リストへ加わったのは `create_table`／
    // `alter_table`／`drop_table` の 3 op のみ）であるため到達不能。
    // 明示トランザクション（SQL-31・TASK-221）の状態エラーも、NoSQL 表層の
    // `op` 語彙にトランザクション制御が無いため同様に到達不能。
    // `DuplicateTable`（`42P07`）・`DuplicateColumn`（`42701`）・
    // `ForbiddenTenantMismatch`（`42501`。DDL 実行権限不足）は Issue #910 で
    // `create_table`／`alter_table`／`drop_table` から実要求経由で到達可能に
    // なったため、本リストから外した（NoSQL の実要求経由の固定は
    // `tests/nosql13_ddl.rs` の `create_table_duplicate_table_is_42p07`・
    // `alter_table_add_column_duplicate_column_is_42701`・
    // `all_three_ddl_ops_reject_with_42501_without_ddl_permission` が担う）。
    // `RowNotFound` は本 Issue の対象外のため引き続き到達不能のまま。
    for class in [
        ErrorClass::RowNotFound,
        ErrorClass::InvalidCursorName,
        ErrorClass::DependentObjectsStillExist,
        ErrorClass::WrongObjectType,
        ErrorClass::InvalidTransactionState,
        ErrorClass::ActiveSqlTransaction,
        ErrorClass::NoActiveSqlTransaction,
        ErrorClass::InFailedSqlTransaction,
        // TASK-206・INDEX-7・SQL-23（Issue #908）: 索引 DDL は SQL 表層専用。
        ErrorClass::UndefinedObject,
        ErrorClass::UndefinedColumn,
        // `InvalidForeignKey`（`42830`。TABLE-17・TASK-205、Issue #907）は
        // `FOREIGN KEY` 宣言（SQL 表層専用の `CREATE TABLE`）の分類で到達不能。
        // `ForeignKeyViolation`（`23503`）は宣言済みテーブルへの書き込み op から
        // 到達可能なため含めない（`err4_f_foreign_key_violation_reachable_via_*`）。
        ErrorClass::InvalidForeignKey,
        // `AmbiguousColumn`（`42702`。SQL-28・RLS-10、Issue #924）: 複数テーブル
        // 参照スコープの基盤導入のみで、許可リストは JOIN・複数 FROM を引き続き
        // `42601` で拒否するため NoSQL 表層からは到達不能。
        ErrorClass::AmbiguousColumn,
    ] {
        let raw =
            wire_server::http::response::encode_error(class, "test message", SystemTime::now());
        let resp = http_common::parse_single_response(&raw);
        assert_projected(&resp, class.wire_code());
    }
}

/// `ForeignKeyViolation`（`23503`。TABLE-17・TASK-205、Issue #907）は NoSQL 表層の
/// 実要求から到達可能: 参照先が不在の値を持つ行の `insert` は engine の単一検査点
/// （`constraint::enforce_row_constraints_in_txn`）で拒否される。応答に参照先の値・
/// テナントを含めない。
#[test]
fn err4_f_foreign_key_violation_reachable_via_nosql_insert() {
    let (core, _guard) = new_core_with_foreign_key();
    let addr = spawn(core);

    let body = br#"{"op":"insert","table":"fk_child","rows":[{"id":1,"embedding":[0.1,0.2],"parent_id":999}],"operation_id":"err4-fk-insert"}"#;
    let resp = query_as_alice(addr, body);
    assert_projected(&resp, "23503");
    http_common::assert_message_does_not_echo(&resp, "tenant-a");
    assert!(
        !http_common::error_message_of(&resp).contains("999"),
        "FK violation message must not echo the referenced value"
    );
}

/// `ForeignKeyViolation`（`23503`）は NoSQL `update` op（参照元列を参照先に存在
/// しない値へ更新）からも到達可能。
#[test]
fn err4_f_foreign_key_violation_reachable_via_nosql_update() {
    let (core, _guard) = new_core_with_foreign_key();
    let addr = spawn(core);

    let insert_parent = br#"{"op":"insert","table":"fk_parent","rows":[{"id":1,"embedding":[0.1,0.2]}],"operation_id":"err4-fk-update-parent"}"#;
    assert_eq!(query_as_alice(addr, insert_parent).status, 200);
    let insert_child = br#"{"op":"insert","table":"fk_child","rows":[{"id":1,"embedding":[0.1,0.2],"parent_id":1}],"operation_id":"err4-fk-update-child"}"#;
    assert_eq!(query_as_alice(addr, insert_child).status, 200);

    let update = br#"{"op":"update","table":"fk_child","set":{"parent_id":999},"where":{"id":1},"operation_id":"err4-fk-update"}"#;
    let resp = query_as_alice(addr, update);
    assert_projected(&resp, "23503");
    http_common::assert_message_does_not_echo(&resp, "tenant-a");
}

/// `ForeignKeyViolation`（`23503`）は NoSQL `delete` op（参照元行が残る参照先行の
/// 削除。参照先側の検査）からも到達可能。
#[test]
fn err4_f_foreign_key_violation_reachable_via_nosql_delete() {
    let (core, _guard) = new_core_with_foreign_key();
    let addr = spawn(core);

    let insert_parent = br#"{"op":"insert","table":"fk_parent","rows":[{"id":1,"embedding":[0.1,0.2]}],"operation_id":"err4-fk-delete-parent"}"#;
    assert_eq!(query_as_alice(addr, insert_parent).status, 200);
    let insert_child = br#"{"op":"insert","table":"fk_child","rows":[{"id":1,"embedding":[0.1,0.2],"parent_id":1}],"operation_id":"err4-fk-delete-child"}"#;
    assert_eq!(query_as_alice(addr, insert_child).status, 200);

    let delete_parent =
        br#"{"op":"delete","table":"fk_parent","where":{"id":1},"operation_id":"err4-fk-delete"}"#;
    let resp = query_as_alice(addr, delete_parent);
    assert_projected(&resp, "23503");
    http_common::assert_message_does_not_echo(&resp, "tenant-a");
}

/// `55P03`（書き込みゲートの待機上限超過。SQL-31・TASK-221）は到達不能では
/// ない: SQL 表層の明示トランザクションが単一ライタを保持している間に NoSQL
/// 表層の書き込み op（`insert`／`update`／`delete`）が待機上限を超えると
/// `TenantWriteError::WriteLockTimeout` → `LOCK_NOT_AVAILABLE` として返る。
/// 再現には待機上限の経過を要するため、射影（503）は production の応答
/// エンコーダ経由で固定する。
#[test]
fn err4_lock_not_available_projects_to_service_unavailable() {
    let raw = wire_server::http::response::encode_error(
        ErrorClass::LockNotAvailable,
        "test message",
        SystemTime::now(),
    );
    let resp = http_common::parse_single_response(&raw);
    assert_projected(&resp, "55P03");
}
