//! `Idempotency-Key` ヘッダを書き込み op（`insert`／`update`／`delete`。単一行形・述語形・
//! 分割実行）の `operation_id` 供給元として結線した契約を、production ルータ経由
//! （生バイトクライアント）で固定する層 A 結合テスト（Issue #2064。親 #2062・TASK-253。
//! 対象ビヘイビア: `docs/spec/04-behavior/http-transport.md` HTTP-14 (1)〜(4)、
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10）。
//!
//! ## 固定する内容
//!
//! - ヘッダのみ／本文のみ／両方一致は成功、両方不一致は `42601` で副作用なし
//! - ヘッダ経路も本文と同じ台帳へ到達する（同内容再送 `23505`・内容違い再送 `22023`）
//! - `--require-operation-id` 相当（`Ledgered`）でもヘッダだけで要件を満たす
//! - 書き込みでない op へのヘッダ付与は `42601`（副作用なし）
//! - 応答はヘッダを返さず、解決した `operation_id` を本文（成功本文・409 の `data`）で返す
//!
//! ヘッダ値の取り出し・検証自体は `idempotency_key.rs` の単体テストが担い、SQL との相互再送と
//! テナント境界は Issue #2065 の範囲である。

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::parser::PartitionedDmlLimits;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TENANT_A: &str = "tenant-a";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs (n BIGINT, u TEXT UNIQUE)` を作り、tenant-a へ `rows` 行 seed する。
fn new_core(rows: usize, mode: LedgerMode) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("http14-idempotency-key");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(mode)
        .with_partitioned_dml_limits(PartitionedDmlLimits {
            chunk_rows: std::num::NonZeroUsize::new(3).expect("non-zero"),
            ..PartitionedDmlLimits::default()
        });
    let mut session = SessionState::default();
    session.allow_ddl();
    let sys = PolicyContext::new("sys").expect("sys ctx");
    core.execute_sql_in_session_legacy(
        &sys,
        &mut session,
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)",
    )
    .expect("create table");
    if rows > 0 {
        let values: Vec<String> = (1..=rows)
            .map(|i| format!("({i}, {}, 'u{i}')", i * 10))
            .collect();
        core.execute_sql_in_session_legacy(
            &ctx(),
            &mut SessionState::default(),
            &format!(
                "INSERT INTO docs (id, n, u) VALUES {} USING OPERATION_ID 'seed-a'",
                values.join(", ")
            ),
        )
        .expect("seed rows");
    }
    (Arc::new(core), guard)
}

struct Client {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn spawn(core: Arc<EngineCore>) -> Client {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let http_addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);
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
    let token = match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected json object, got {other:?}"),
    };
    Client { http_addr, token }
}

/// `header` が `Some` なら `Idempotency-Key` を付けて `POST /v1/query` する。
fn query(c: &Client, body: &str, header: Option<&str>) -> HttpResponse {
    let auth = format!("Bearer {}", c.token);
    let len = body.len().to_string();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", &auth),
        ("Content-Type", "application/json"),
        ("Content-Length", &len),
    ];
    if let Some(h) = header {
        headers.push(("Idempotency-Key", h));
    }
    let request = http_common::build_request("/v1/query", &headers, body.as_bytes());
    http_common::parse_single_response(&http_common::send_raw(
        c.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn count_rows(core: &EngineCore) -> usize {
    use engine::sql::SqlOutcome;
    let outcome = core
        .execute_sql_in_session_legacy(
            &ctx(),
            &mut SessionState::default(),
            "SELECT u FROM docs LIMIT 1000",
        )
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    result.rows.len()
}

fn op_field(op_id: Option<&str>) -> String {
    op_id.map_or(String::new(), |id| format!(r#","operation_id":"{id}""#))
}

fn insert(id: u64, op_id: Option<&str>) -> String {
    format!(
        r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"n":{id},"u":"ins{id}"}}]{}}}"#,
        op_field(op_id)
    )
}

fn update_where(id: u64, value: &str, op_id: Option<&str>) -> String {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"n":{value}}},"where":{{"id":{id}}}{}}}"#,
        op_field(op_id)
    )
}

fn update_filter(u: &str, value: &str, op_id: Option<&str>) -> String {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"n":{value}}},"filter":[{{"column":"u","op":"eq","value":"{u}"}}]{}}}"#,
        op_field(op_id)
    )
}

fn delete_where(id: u64, op_id: Option<&str>) -> String {
    format!(
        r#"{{"op":"delete","table":"docs","where":{{"id":{id}}}{}}}"#,
        op_field(op_id)
    )
}

fn delete_filter(u: &str, op_id: Option<&str>) -> String {
    format!(
        r#"{{"op":"delete","table":"docs","filter":[{{"column":"u","op":"eq","value":"{u}"}}]{}}}"#,
        op_field(op_id)
    )
}

/// 4 パターン（ヘッダのみ／本文のみ／両方一致／両方不一致）を `mk(n, body_op_id)` が作る
/// 要求で確認する。`mk` の `n` は 0 から始まる呼び出し番号（行の選択に使う）。
fn assert_four_patterns(
    core: &EngineCore,
    c: &Client,
    key: &str,
    mk: impl Fn(usize, Option<&str>) -> String,
) {
    // ヘッダのみ: 200 で本文にヘッダ値が operation_id として返る。
    let id = format!("{key}-hdr");
    let resp = query(c, &mk(0, None), Some(&id));
    assert_eq!(resp.status, 200, "header only: {resp:?}");
    assert!(
        body_text(&resp).contains(&format!(r#""operation_id":"{id}""#)),
        "{}",
        body_text(&resp)
    );
    assert!(resp.header("idempotency-key").is_none());

    // 本文のみ: 従来どおり 200。
    let id = format!("{key}-body");
    let resp = query(c, &mk(1, Some(&id)), None);
    assert_eq!(resp.status, 200, "body only: {resp:?}");
    assert!(body_text(&resp).contains(&format!(r#""operation_id":"{id}""#)));

    // 両方一致: 200。
    let id = format!("{key}-both");
    let resp = query(c, &mk(2, Some(&id)), Some(&id));
    assert_eq!(resp.status, 200, "both equal: {resp:?}");

    // 両方不一致: 42601。副作用なし（行数不変）で、台帳にも記録されない。
    let before = count_rows(core);
    let body_marker = format!("marker-body-{key}");
    let resp = query(c, &mk(3, Some(&body_marker)), Some("marker-header-y"));
    assert_eq!(resp.status, 400, "mismatch: {resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
    http_common::assert_message_does_not_echo(&resp, &body_marker);
    http_common::assert_message_does_not_echo(&resp, "marker-header-y");
    assert_eq!(count_rows(core), before, "mismatch must not write");
    // 台帳が未記録であること: 同じ要求を本文の id だけで送ると初回として成功する。
    let resp = query(c, &mk(3, Some(&body_marker)), None);
    assert_eq!(resp.status, 200, "ledger must be untouched: {resp:?}");
}

#[test]
fn insert_four_patterns() {
    let (core, _g) = new_core(0, LedgerMode::Optional);
    let c = spawn(core.clone());
    assert_four_patterns(&core, &c, "ins", |n, op| insert(100 + n as u64, op));
}

#[test]
fn update_where_and_filter_four_patterns() {
    let (core, _g) = new_core(8, LedgerMode::Optional);
    let c = spawn(core.clone());
    assert_four_patterns(&core, &c, "updw", |n, op| {
        update_where(1 + n as u64, "5", op)
    });
    assert_four_patterns(&core, &c, "updf", |n, op| {
        update_filter(&format!("u{}", 5 + n), "6", op)
    });
}

#[test]
fn delete_where_and_filter_four_patterns() {
    let (core, _g) = new_core(8, LedgerMode::Optional);
    let c = spawn(core.clone());
    // 不一致で消えないことを確認するため、不一致の n=3 を後段で本文のみ再送して消す。
    assert_four_patterns(&core, &c, "delw", |n, op| delete_where(1 + n as u64, op));
    assert_four_patterns(&core, &c, "delf", |n, op| {
        delete_filter(&format!("u{}", 5 + n), op)
    });
}

#[test]
fn header_path_reaches_the_same_ledger_as_body() {
    let (core, _g) = new_core(0, LedgerMode::Optional);
    let c = spawn(core.clone());
    let resp = query(&c, &insert(1, None), Some("led-1"));
    assert_eq!(resp.status, 200, "{resp:?}");
    // 同内容の再送は 23505（ヘッダ・本文のどちらの経路でも同一台帳）。
    let resp = query(&c, &insert(1, None), Some("led-1"));
    assert_eq!(http_common::wire_code_of(&resp), "23505", "{resp:?}");
    let resp = query(&c, &insert(1, Some("led-1")), None);
    assert_eq!(http_common::wire_code_of(&resp), "23505", "{resp:?}");
    // 内容違いの再送は 22023。
    let resp = query(&c, &insert(2, None), Some("led-1"));
    assert_eq!(http_common::wire_code_of(&resp), "22023", "{resp:?}");
    assert_eq!(count_rows(&core), 1);
}

#[test]
fn ledgered_mode_is_satisfied_by_header_alone() {
    let (core, _g) = new_core(0, LedgerMode::Ledgered);
    let c = spawn(core.clone());
    let resp = query(&c, &insert(1, None), None);
    assert_eq!(http_common::wire_code_of(&resp), "23502", "{resp:?}");
    let resp = query(&c, &insert(1, None), Some("req-hdr"));
    assert_eq!(resp.status, 200, "{resp:?}");
    let resp = query(&c, &delete_where(1, None), Some("req-hdr-del"));
    assert_eq!(resp.status, 200, "{resp:?}");
}

#[test]
fn non_write_ops_reject_the_header_without_side_effects() {
    let (core, _g) = new_core(2, LedgerMode::Optional);
    let c = spawn(core.clone());
    let bodies = [
        r#"{"op":"scan","table":"docs","limit":1}"#,
        r#"{"op":"scan","table":"docs","limit":1,"explain":true}"#,
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count"}]}"#,
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count"}],"explain":true}"#,
        r#"{"op":"drop_table","table":"docs"}"#,
        r#"{"op":"show_partitioned_dml","table":"docs","operation_id":"x"}"#,
        r#"{"op":"cancel_partitioned_dml","table":"docs","operation_id":"x"}"#,
    ];
    for body in bodies {
        let resp = query(&c, body, Some("marker-hdr-z"));
        assert_eq!(resp.status, 400, "{body}: {resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "{body}");
        http_common::assert_message_does_not_echo(&resp, "marker-hdr-z");
    }
    // `drop_table` が実行されていない（表も行も残る）。
    assert_eq!(count_rows(&core), 2);
}

#[test]
fn invalid_header_values_are_rejected_on_write_ops() {
    let (core, _g) = new_core(0, LedgerMode::Optional);
    let c = spawn(core.clone());
    let resp = query(&c, &insert(1, None), Some(&"x".repeat(257)));
    assert_eq!(http_common::wire_code_of(&resp), "42601", "{resp:?}");
    assert_eq!(count_rows(&core), 0);
}

#[test]
fn partitioned_dml_accepts_header_only_and_reports_it_in_body_and_409() {
    let (core, _g) = new_core(3, LedgerMode::Optional);
    let c = spawn(core.clone());
    let filter = r#""filter":[{"column":"u","op":"not_null"}]"#;
    let ok = format!(
        r#"{{"op":"update","table":"docs","set":{{"n":7}},{filter},"mode":"partitioned","chunk":1}}"#
    );
    let resp = query(&c, &ok, Some("part-ok"));
    assert_eq!(resp.status, 200, "{resp:?}");
    assert!(body_text(&resp).contains(r#""operation_id":"part-ok""#));

    // UNIQUE 列へ同値を chunk=1 で書き込み、2 チャンク目で確定的に部分完了（409）にする。
    let dup = format!(
        r#"{{"op":"update","table":"docs","set":{{"u":"dup"}},{filter},"mode":"partitioned","chunk":1}}"#
    );
    let resp = query(&c, &dup, Some("part-409"));
    assert_eq!(resp.status, 409, "{resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "VD001");
    assert!(
        body_text(&resp).contains(r#""operation_id":"part-409""#),
        "{}",
        body_text(&resp)
    );
}
