//! NoSQL 表層の述語形 `update`／`delete`（`filter` 非空）が `--max-dml-affected-rows`
//! （Issue #997）の影響行数上限を超えたとき、`54000`／HTTP 413・副作用ゼロ・台帳
//! 未記録で fail-closed に終わることを固定する結合テスト（Issue #1202。ポインタ:
//! NOSQL-12・RLS-10 (a)・RLS-9・RECOVER-10・ERR-2）。
//!
//! ## 役割分担（重複再検証をしない）
//!
//! - `crates/engine/tests/sql_predicate_dml_exec.rs`: engine 側の上限判定
//!   （`EngineCore::with_dml_limits` 経由の `54000`・副作用ゼロ）の担当
//! - `crates/wire-server/tests/wire_dml_limits_cli.rs`: CLI フラグ解析の外形確認の担当
//! - `crates/wire-server/tests/nosql12_update_delete.rs`: NoSQL update／delete の
//!   基本契約（上限なし）の担当
//! - 本ファイル: 上限が NoSQL 表層（HTTP ルータ経由）まで届くこと（層 A:
//!   インプロセス）と、実バイナリの CLI フラグからも届くこと（層 B）を固定する。
//!   加えて RLS-10 (a) との交差（他テナントの一致行が上限判定に算入されず、応答が
//!   他テナント行の有無で変化しないこと）を、応答バイト列の同一性で固定する。
//!
//! 層 A（A1〜A6）: A1 超過は 413／`54000`、A2 副作用ゼロ・再送しても `54000`
//! （台帳未記録なら 409／`23505` にならない）、A3 ちょうど上限は成功、A4 単一行形
//! （`where.id`）は上限の対象外、A5 他テナントの行が判定に算入されない、A6 SQL 表層
//! とのパリティ。層 B は実バイナリを `--max-dml-affected-rows` 付きで起動する。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::parser::DmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";
const LIMIT: usize = 2;
const USERS: [(&str, &str, &str); 3] = [
    ("alice", "tenant-a", "pw-alice"),
    ("bob", "tenant-b", "pw-bob"),
    ("carol", "tenant-c", "pw-carol"),
];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// seed 1 行分の定義: `(tenant, id, lang)`。全行 Private（各テナントの読み戻しが自テナント行だけになる）・同一 embedding。
type SeedRow = (&'static str, u64, &'static str);

fn seed_storage(storage: &Storage, rows: &[SeedRow]) {
    storage.create_table(&schema()).expect("create table");
    for (tenant, id, lang) in rows {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let op = OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx,
            *id,
            Visibility::Private,
            &[
                Value::Vector(vec![0.1, 0.2, 0.3]),
                Value::Text((*lang).to_string()),
            ],
            &op,
        )
        .expect("seed row");
    }
}

/// 上限つき（または上限なし）の seed 済み core を作る。
fn new_core(rows: &[SeedRow], limit: Option<usize>) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql12-affected-rows-limit");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_storage(&storage, rows);
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(n) = limit {
        core = core.with_dml_limits(DmlLimits {
            max_affected_rows: NonZeroUsize::new(n),
            ..DmlLimits::default()
        });
    }
    (Arc::new(core), guard)
}

/// HTTP ルータ（3 ユーザー）を `core` の上に起動し、各ユーザーのトークンを保持する。
struct Http {
    addr: std::net::SocketAddr,
    tokens: Vec<(&'static str, String)>,
}

fn spawn_http(core: Arc<EngineCore>) -> Http {
    let users_path = common::write_user_store_file(&USERS);
    let addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);
    let tokens = USERS
        .iter()
        .map(|(user, _, pw)| (*user, login(addr, user, pw)))
        .collect();
    Http { addr, tokens }
}

fn login(addr: std::net::SocketAddr, user: &str, pw: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{pw}"}}"#).into_bytes();
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
    match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected json object, got {other:?}"),
    }
}

fn query(addr: std::net::SocketAddr, token: &str, body: &str) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

impl Http {
    fn token(&self, user: &str) -> &str {
        &self
            .tokens
            .iter()
            .find(|(u, _)| *u == user)
            .expect("known user")
            .1
    }

    fn q(&self, user: &str, body: &str) -> HttpResponse {
        query(self.addr, self.token(user), body)
    }
}

/// `Date` ヘッダを除いた応答全体（バイト同一性比較用。`nosql12_update_delete.rs::
/// strip_date` と同じ意図）。
fn strip_date(resp: &HttpResponse) -> String {
    let mut out = format!("{} {}\n", resp.status, resp.reason);
    for (name, value) in &resp.headers {
        if !name.eq_ignore_ascii_case("date") {
            out.push_str(&format!("{name}: {value}\n"));
        }
    }
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&resp.body));
    out
}

fn pred_update(lang: &str, new_lang: &str, op: &str) -> String {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"lang":"{new_lang}"}},"filter":[{{"column":"lang","op":"eq","value":"{lang}"}}],"operation_id":"{op}"}}"#
    )
}

fn pred_delete(lang: &str, op: &str) -> String {
    format!(
        r#"{{"op":"delete","table":"docs","filter":[{{"column":"lang","op":"eq","value":"{lang}"}}],"operation_id":"{op}"}}"#
    )
}

/// 指定テナントの可視行 `(id, lang)` を engine API 直呼びで読み戻す（HTTP を経由しない
/// オラクル。Private 込みで見える自テナント行のみを対象に `id` 昇順で返す）。
fn read_rows(core: &EngineCore, tenant: &str) -> Vec<(u64, Option<String>)> {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT lang FROM docs LIMIT 1000")
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    let mut rows: Vec<(u64, Option<String>)> = result
        .rows
        .iter()
        .map(|row| {
            let lang = row.cells.first().and_then(|c| match c {
                engine::sql::exec::Cell::Text(s) => Some(s.clone()),
                engine::sql::exec::Cell::Null => None,
                other => panic!("unexpected cell kind: {other:?}"),
            });
            (row.id, lang)
        })
        .collect();
    rows.sort();
    rows
}

fn rows_of(
    tenant: &'static str,
    ids: std::ops::RangeInclusive<u64>,
    lang: &'static str,
) -> Vec<SeedRow> {
    ids.map(|id| (tenant, id, lang)).collect()
}

fn assert_limit_rejection(resp: &HttpResponse) {
    assert_eq!(resp.status, 413, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(resp), "54000", "resp={resp:?}");
}

// --- A1・A2: 超過は 413／54000・副作用ゼロ・台帳未記録 -----------------------

#[test]
fn a1_a2_predicate_update_and_delete_over_limit_are_413_with_no_side_effect() {
    // tenant-a は上限 + 1 行の一致行を持つ。
    let rows = rows_of("tenant-a", 1..=(LIMIT as u64 + 1), "ja");
    let (core, _guard) = new_core(&rows, Some(LIMIT));
    let http = spawn_http(core.clone());
    let before = read_rows(&core, "tenant-a");
    assert_eq!(before.len(), LIMIT + 1);

    let resp = http.q("alice", &pred_update("ja", "en", "a1-upd"));
    assert_limit_rejection(&resp);
    let resp = http.q("alice", &pred_delete("ja", "a1-del"));
    assert_limit_rejection(&resp);
    assert_eq!(read_rows(&core, "tenant-a"), before, "no side effect");

    // 台帳未記録: 同一 operation_id の再送でも 409／23505 にならず、同じ 413／54000。
    let resp = http.q("alice", &pred_update("ja", "en", "a1-upd"));
    assert_limit_rejection(&resp);
    let resp = http.q("alice", &pred_delete("ja", "a1-del"));
    assert_limit_rejection(&resp);
    assert_eq!(read_rows(&core, "tenant-a"), before, "still no side effect");
}

// --- A3: ちょうど上限は成功（境界値） ----------------------------------------

#[test]
fn a3_exactly_at_limit_succeeds() {
    let rows = rows_of("tenant-a", 1..=(LIMIT as u64), "ja");
    let (core, _guard) = new_core(&rows, Some(LIMIT));
    let http = spawn_http(core.clone());

    let resp = http.q("alice", &pred_update("ja", "en", "a3-upd"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(body.contains(&format!(r#""updated":{LIMIT}"#)), "{body}");

    let resp = http.q("alice", &pred_delete("en", "a3-del"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(body.contains(&format!(r#""deleted":{LIMIT}"#)), "{body}");
    assert!(read_rows(&core, "tenant-a").is_empty());
}

// --- A4: 単一行形（where.id）は上限の対象外 ---------------------------------

/// 単一行形（`where.id`）は、設定できる最も厳しい上限でも拒否されない。
///
/// 上限は `NonZeroUsize`（`0` は「上限なし」の `None` と同義で設定できない）のため最小値は
/// `1` で、影響行数が高々 1 の単一行形は上限を**超える状態を作れない**。そこで、上限 `1`
/// の同じ core で述語形（一致 3 行）が `413`／`54000` になること（リミッタが働いている
/// ことの陽性対照）を確かめたうえで、単一行形の更新・削除が成功し、指定した行だけが
/// 変わることを固定する（影響行数ではなくテーブル行数や候補数で上限を判定する回帰を
/// 検出する）。
#[test]
fn a4_single_row_form_is_not_subject_to_the_limit() {
    const MIN_LIMIT: usize = 1;
    let rows = rows_of("tenant-a", 1..=3, "ja");
    let (core, _guard) = new_core(&rows, Some(MIN_LIMIT));
    let http = spawn_http(core.clone());

    // 陽性対照: 同じ上限で述語形（一致 3 行 > 1）は拒否され、副作用も無い。
    let before = read_rows(&core, "tenant-a");
    let resp = http.q("alice", &pred_update("ja", "en", "a4-pred-upd"));
    assert_limit_rejection(&resp);
    let resp = http.q("alice", &pred_delete("ja", "a4-pred-del"));
    assert_limit_rejection(&resp);
    assert_eq!(read_rows(&core, "tenant-a"), before, "no side effect");

    // 単一行形は上限 1 でも成功し、指定した行だけが変わる。
    let resp = http.q(
        "alice",
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":"a4-upd"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let resp = http.q(
        "alice",
        r#"{"op":"delete","table":"docs","where":{"id":2},"operation_id":"a4-del"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert_eq!(
        read_rows(&core, "tenant-a"),
        vec![(1, Some("en".to_string())), (3, Some("ja".to_string()))]
    );
}

// --- A5: RLS-10 (a) × NOSQL-12 の交差 ---------------------------------------

/// 他テナント（tenant-b／c）が上限を超える一致行を持っていても、tenant-a の判定には
/// 算入されない。応答（`Date` を除くバイト列）は他テナント行が無い core と完全一致し、
/// 他テナントの行は不変である。
#[test]
fn a5_foreign_matching_rows_are_not_counted_and_do_not_change_responses() {
    let own_ok = rows_of("tenant-a", 1..=(LIMIT as u64), "ja");
    let own_over = rows_of("tenant-a", 1..=(LIMIT as u64 + 1), "ja");
    let mut flood = rows_of("tenant-b", 1..=(LIMIT as u64 + 3), "ja");
    flood.extend(rows_of("tenant-c", 1..=(LIMIT as u64 + 3), "ja"));

    for (own, expect_ok) in [(&own_ok, true), (&own_over, false)] {
        for is_update in [true, false] {
            let mut flooded_rows = own.clone();
            flooded_rows.extend(flood.iter().copied());
            let (flooded, _g1) = new_core(&flooded_rows, Some(LIMIT));
            let (isolated, _g2) = new_core(own, Some(LIMIT));
            let http_f = spawn_http(flooded.clone());
            let http_i = spawn_http(isolated.clone());
            let foreign_before: Vec<_> = ["tenant-b", "tenant-c"]
                .iter()
                .map(|t| read_rows(&flooded, t))
                .collect();

            let body = if is_update {
                pred_update("ja", "en", "a5-op")
            } else {
                pred_delete("ja", "a5-op")
            };
            let resp_f = http_f.q("alice", &body);
            let resp_i = http_i.q("alice", &body);
            if expect_ok {
                assert_eq!(resp_f.status, 200, "resp={resp_f:?}");
            } else {
                assert_limit_rejection(&resp_f);
            }
            assert_eq!(
                strip_date(&resp_f),
                strip_date(&resp_i),
                "response must not depend on foreign rows (own={}, update={is_update})",
                own.len()
            );
            let foreign_after: Vec<_> = ["tenant-b", "tenant-c"]
                .iter()
                .map(|t| read_rows(&flooded, t))
                .collect();
            assert_eq!(
                foreign_before, foreign_after,
                "foreign rows must be untouched"
            );
            assert!(foreign_before.iter().all(|r| r.len() == LIMIT + 3));
        }
    }
}

// --- A6: SQL 表層とのパリティ ----------------------------------------------

#[test]
fn a6_sql_surface_rejects_the_same_statements_with_54000() {
    let rows = rows_of("tenant-a", 1..=(LIMIT as u64 + 1), "ja");
    let (core, _guard) = new_core(&rows, Some(LIMIT));
    let users_path = common::write_user_store_file(&USERS);
    let sql_addr = common::spawn_server_with_engine(&users_path, core.clone());
    let mut sql = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");
    let before = read_rows(&core, "tenant-a");

    for stmt in [
        "UPDATE docs SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'a6-upd'",
        "DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID 'a6-del'",
    ] {
        common::send_simple_query(&mut sql, stmt);
        common::expect_error_response_with_sqlstate(&mut sql, "54000");
        common::read_ready_for_query(&mut sql);
    }
    assert_eq!(read_rows(&core, "tenant-a"), before);
}

// --- 層 B: 実バイナリ + --max-dml-affected-rows ------------------------------

#[test]
fn layer_b_spawned_binary_applies_the_cli_limit_to_nosql_predicate_dml() {
    let fixture = common::TempFixtureDir::new("nosql12-affected-rows-limit");
    let users_path = common::write_user_store_file(&USERS);
    let users_str = users_path.to_str().expect("utf-8 users path").to_string();
    let db_path = fixture.db_path_str();

    // seed 後に Storage を drop してから子プロセスを起動する（redb は単一ライター）。
    {
        let mut rows = rows_of("tenant-a", 1..=3, "ja");
        rows.extend(rows_of("tenant-a", 4..=5, "en"));
        rows.extend(rows_of("tenant-b", 1..=5, "ja"));
        rows.extend(rows_of("tenant-c", 1..=5, "ja"));
        let storage = Storage::open(std::path::Path::new(&db_path)).expect("open storage");
        seed_storage(&storage, &rows);
    }

    let limit = LIMIT.to_string();
    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &users_str,
        "--db",
        &db_path,
        "--bind",
        "127.0.0.1:0",
        "--surface",
        "nosql",
        "--max-dml-affected-rows",
        &limit,
    ]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let addr_str = server
        .wait_for_listening(deadline)
        .expect("server must report listening address");
    let addr: std::net::SocketAddr = addr_str.parse().expect("valid socket addr");
    let token = login(addr, "alice", "pw-alice");

    // 3 行一致（> LIMIT）は 413／54000。update・delete とも。
    assert_limit_rejection(&query(addr, &token, &pred_update("ja", "fr", "b-upd-over")));
    assert_limit_rejection(&query(addr, &token, &pred_delete("ja", "b-del-over")));
    // 副作用ゼロ・台帳未記録: 同一 operation_id の再送も同じ 413／54000。
    assert_limit_rejection(&query(addr, &token, &pred_update("ja", "fr", "b-upd-over")));
    // ちょうど LIMIT 行（lang = 'en' は 2 行）は成功する。
    let resp = query(addr, &token, &pred_update("en", "fr", "b-upd-ok"));
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(body.contains(r#""updated":2"#), "{body}");

    let seen = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let joined = seen.join("");
    for needle in ["tenant-a", "tenant-b", "tenant-c", "alice", "bob", "carol"] {
        assert!(!joined.contains(needle), "stderr must not leak {needle}");
    }
}
