//! `POST /v1/query`（`op: "upsert"`）が SQL 表層の `INSERT ... ON CONFLICT` と同じ束縛関数・
//! 同じ実行器を通ることを production ルータ越しに固定する層 A 結合テスト（Issue #2087。
//! 親 #2085。対象ビヘイビア: NOSQL-21・SQL-20・SQL-49・RLS-9・RECOVER-11・HTTP-14。
//! ポインタ: `docs/spec/04-behavior/nosql-surface.md` NOSQL-21、`sql-surface.md` SQL-20／SQL-49、
//! `rls.md` RLS-9）。
//!
//! 構成: 2 つの独立した `EngineCore`（同一 seed）のうち、片方へ HTTP の upsert、もう片方へ同等の
//! SQL 文を流し、件数（`INSERT 0 n` の `n` に相当する `rows_affected`）・最終状態・エラーコードの
//! 一致を確かめる。テナント境界は 3 テナント（a・b・c）で、他テナントの行と id・UNIQUE キーが
//! 衝突しても自テナントの応答が衝突なしの場合と同一であることを確かめる。

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::net::SocketAddr;
use std::sync::Arc;

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

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs (n BIGINT, u TEXT UNIQUE)` を持つ `EngineCore` を作る。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql21-upsert");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)",
    )
    .expect("create table");
    (Arc::new(core), guard)
}

fn sql(core: &EngineCore, tenant: &str, text: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut SessionState::default(), text)
        .map_err(|e| e.wire_code().to_string())
}

/// tenant の行を `(id, n, u)` の順で文字列化して返す。
fn state(core: &EngineCore, tenant: &str) -> String {
    let outcome =
        sql(core, tenant, "SELECT n, u FROM docs ORDER BY id LIMIT 1000").expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    format!("{:?}", result.rows)
}

/// tenant-a に id=1 (n=10, 'u1')・id=2 (n=20, 'u2') を seed する。
fn seed_a(core: &EngineCore) {
    sql(
        core,
        TENANT_A,
        "INSERT INTO docs (id, n, u) VALUES (1, 10, 'u1'), (2, 20, 'u2')",
    )
    .expect("seed a");
}

fn login_alice(core: Arc<EngineCore>) -> (SocketAddr, String) {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr =
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
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => (addr, s),
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected json object, got {other:?}"),
    }
}

fn query_with(addr: SocketAddr, token: &str, body: &str, extra: &[(&str, &str)]) -> HttpResponse {
    let length = body.len().to_string();
    let auth = format!("Bearer {token}");
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", &auth),
        ("Content-Type", "application/json"),
        ("Content-Length", &length),
    ];
    headers.extend_from_slice(extra);
    let request = http_common::build_request("/v1/query", &headers, body.as_bytes());
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn query(addr: SocketAddr, token: &str, body: &str) -> HttpResponse {
    query_with(addr, token, body, &[])
}

/// 成功応答から `upserted` を取り出す（キーはこれだけであることも固定する）。
fn upserted_of(resp: &HttpResponse) -> u64 {
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let text = std::str::from_utf8(&resp.body).expect("utf-8");
    let JsonValue::Object(mut top) = parse_json(text).expect("json") else {
        panic!("object expected: {text}");
    };
    let n = match top.remove("upserted") {
        Some(JsonValue::Number(n)) => n.as_f64() as u64,
        other => panic!("upserted must be a number, got {other:?}"),
    };
    top.remove("operation_id");
    assert!(top.is_empty(), "unexpected keys: {top:?}");
    n
}

fn sql_rows_affected(outcome: Result<SqlOutcome, String>) -> u64 {
    match outcome.expect("sql ok") {
        SqlOutcome::Insert(o) => o.rows_affected,
        other => panic!("expected Insert outcome, got {other:?}"),
    }
}

/// NoSQL（HTTP）と SQL を別 core の同一 seed に流し、件数と最終状態の一致を確かめる。
fn assert_parity(http_body: &str, sql_text: &str, expected: u64) {
    let (core_http, _g1) = new_core();
    let (core_sql, _g2) = new_core();
    seed_a(&core_http);
    seed_a(&core_sql);
    let (addr, token) = login_alice(core_http.clone());

    let resp = query(addr, &token, http_body);
    let http_count = upserted_of(&resp);
    let sql_count = sql_rows_affected(sql(&core_sql, TENANT_A, sql_text));
    assert_eq!(http_count, expected, "http count for {http_body}");
    assert_eq!(http_count, sql_count, "sql parity for {sql_text}");
    assert_eq!(
        state(&core_http, TENANT_A),
        state(&core_sql, TENANT_A),
        "final state parity"
    );
}

#[test]
fn parity_new_rows_are_inserted() {
    assert_parity(
        r#"{"op":"upsert","table":"docs","rows":[{"id":10,"n":1,"u":"x10"},{"id":11,"n":2,"u":"x11"}],"on_conflict":{"columns":["id"],"do":"update"}}"#,
        "INSERT INTO docs (id, n, u) VALUES (10, 1, 'x10'), (11, 2, 'x11') \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n, u = EXCLUDED.u",
        2,
    );
}

#[test]
fn parity_do_nothing_on_conflict_counts_zero() {
    assert_parity(
        r#"{"op":"upsert","table":"docs","rows":[{"id":1,"n":99,"u":"z1"}],"on_conflict":{"columns":["id"],"do":"nothing"}}"#,
        "INSERT INTO docs (id, n, u) VALUES (1, 99, 'z1') ON CONFLICT (id) DO NOTHING",
        0,
    );
}

#[test]
fn parity_do_update_with_default_set_updates_conflicting_row() {
    assert_parity(
        r#"{"op":"upsert","table":"docs","rows":[{"id":1,"n":99,"u":"z1"}],"on_conflict":{"columns":["id"],"do":"update"}}"#,
        "INSERT INTO docs (id, n, u) VALUES (1, 99, 'z1') \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n, u = EXCLUDED.u",
        1,
    );
}

#[test]
fn parity_do_update_with_explicit_set_only_touches_listed_columns() {
    assert_parity(
        r#"{"op":"upsert","table":"docs","rows":[{"id":1,"n":99,"u":"z1"}],"on_conflict":{"columns":["id"],"do":"update","set":["n"]}}"#,
        "INSERT INTO docs (id, n, u) VALUES (1, 99, 'z1') \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n",
        1,
    );
}

#[test]
fn parity_mixed_new_and_conflicting_rows() {
    assert_parity(
        r#"{"op":"upsert","table":"docs","rows":[{"id":2,"n":77,"u":"z2"},{"id":20,"n":5,"u":"z20"}],"on_conflict":{"columns":["id"],"do":"update"}}"#,
        "INSERT INTO docs (id, n, u) VALUES (2, 77, 'z2'), (20, 5, 'z20') \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n, u = EXCLUDED.u",
        2,
    );
    assert_parity(
        r#"{"op":"upsert","table":"docs","rows":[{"id":2,"n":77,"u":"z2"},{"id":20,"n":5,"u":"z20"}],"on_conflict":{"columns":["id"],"do":"nothing"}}"#,
        "INSERT INTO docs (id, n, u) VALUES (2, 77, 'z2'), (20, 5, 'z20') \
         ON CONFLICT (id) DO NOTHING",
        1,
    );
}

#[test]
fn parity_unique_constraint_target() {
    // `u` は UNIQUE。id=30 の新規行が id=1 の `u1` と衝突し、id=1 の `n` だけが更新される。
    assert_parity(
        r#"{"op":"upsert","table":"docs","rows":[{"id":30,"n":55,"u":"u1"}],"on_conflict":{"columns":["u"],"do":"update"}}"#,
        "INSERT INTO docs (id, n, u) VALUES (30, 55, 'u1') \
         ON CONFLICT (u) DO UPDATE SET n = EXCLUDED.n",
        1,
    );
}

#[test]
fn conflict_target_not_matching_a_constraint_is_42p10_like_sql() {
    let (core, _g) = new_core();
    seed_a(&core);
    let (addr, token) = login_alice(core.clone());
    let resp = query(
        addr,
        &token,
        r#"{"op":"upsert","table":"docs","rows":[{"id":5,"n":1,"u":"q"}],"on_conflict":{"columns":["n"],"do":"nothing"}}"#,
    );
    assert_eq!(http_common::wire_code_of(&resp), "42P10");
    let sql_err = sql(
        &core,
        TENANT_A,
        "INSERT INTO docs (id, n, u) VALUES (5, 1, 'q') ON CONFLICT (n) DO NOTHING",
    )
    .expect_err("sql must reject");
    assert_eq!(sql_err, "42P10");
}

#[test]
fn shape_and_value_rejections_have_no_side_effect() {
    let (core, _g) = new_core();
    seed_a(&core);
    let before = state(&core, TENANT_A);
    let (addr, token) = login_alice(core.clone());
    let cases: [(&str, &str); 8] = [
        // do の語彙外。
        (
            r#"{"op":"upsert","table":"docs","rows":[{"id":5,"n":1,"u":"q"}],"on_conflict":{"columns":["id"],"do":"replace"}}"#,
            "42601",
        ),
        // nothing に set。
        (
            r#"{"op":"upsert","table":"docs","rows":[{"id":5,"n":1,"u":"q"}],"on_conflict":{"columns":["id"],"do":"nothing","set":["n"]}}"#,
            "42601",
        ),
        // 空の set。
        (
            r#"{"op":"upsert","table":"docs","rows":[{"id":5,"n":1,"u":"q"}],"on_conflict":{"columns":["id"],"do":"update","set":[]}}"#,
            "42601",
        ),
        // 空の columns。
        (
            r#"{"op":"upsert","table":"docs","rows":[{"id":5,"n":1,"u":"q"}],"on_conflict":{"columns":[],"do":"nothing"}}"#,
            "42601",
        ),
        // 既定の set が空になる（id と対象列しか無い）。
        (
            r#"{"op":"upsert","table":"docs","rows":[{"id":5}],"on_conflict":{"columns":["id"],"do":"update"}}"#,
            "42601",
        ),
        // 行間でキー集合が異なる。
        (
            r#"{"op":"upsert","table":"docs","rows":[{"id":5,"n":1,"u":"q"},{"id":6,"n":2}],"on_conflict":{"columns":["id"],"do":"nothing"}}"#,
            "22000",
        ),
        // 空の rows。
        (
            r#"{"op":"upsert","table":"docs","rows":[],"on_conflict":{"columns":["id"],"do":"nothing"}}"#,
            "22000",
        ),
        // returning はスキーマ外。
        (
            r#"{"op":"upsert","table":"docs","rows":[{"id":5,"n":1,"u":"q"}],"on_conflict":{"columns":["id"],"do":"nothing"},"returning":"*"}"#,
            "42601",
        ),
    ];
    for (body, code) in cases {
        let resp = query(addr, &token, body);
        assert_eq!(http_common::wire_code_of(&resp), code, "body={body}");
    }
    assert_eq!(state(&core, TENANT_A), before, "no side effect");
}

#[test]
fn operation_id_resend_is_23505_for_same_content_and_22023_for_different() {
    let (core, _g) = new_core();
    seed_a(&core);
    let (addr, token) = login_alice(core.clone());
    let body = |n: u32| {
        format!(
            r#"{{"op":"upsert","table":"docs","rows":[{{"id":7,"n":{n},"u":"s7"}}],"on_conflict":{{"columns":["id"],"do":"update"}},"operation_id":"n21-op-1"}}"#
        )
    };
    let first = query(addr, &token, &body(1));
    assert_eq!(upserted_of(&first), 1);
    assert_eq!(
        std::str::from_utf8(&first.body).expect("utf-8"),
        r#"{"upserted":1,"operation_id":"n21-op-1"}"#
    );
    let same = query(addr, &token, &body(1));
    assert_eq!(http_common::wire_code_of(&same), "23505");
    let different = query(addr, &token, &body(2));
    assert_eq!(http_common::wire_code_of(&different), "22023");
}

#[test]
fn idempotency_key_header_supplies_operation_id() {
    let (core, _g) = new_core();
    seed_a(&core);
    let (addr, token) = login_alice(core.clone());
    let body = r#"{"op":"upsert","table":"docs","rows":[{"id":8,"n":1,"u":"s8"}],"on_conflict":{"columns":["id"],"do":"nothing"}}"#;
    let first = query_with(addr, &token, body, &[("Idempotency-Key", "n21-hdr-1")]);
    assert_eq!(upserted_of(&first), 1);
    let again = query_with(addr, &token, body, &[("Idempotency-Key", "n21-hdr-1")]);
    assert_eq!(http_common::wire_code_of(&again), "23505");
}

#[test]
fn sql_and_http_share_the_ledger_in_both_directions() {
    let (core, _g) = new_core();
    seed_a(&core);
    let (addr, token) = login_alice(core.clone());

    // SQL で実行した UPSERT を、同内容の HTTP upsert で再送すると `23505`。
    sql_rows_affected(sql(
        &core,
        TENANT_A,
        "INSERT INTO docs (id, n, u) VALUES (40, 1, 'x40') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'n21-sql-first'",
    ));
    let resend = query(
        addr,
        &token,
        r#"{"op":"upsert","table":"docs","rows":[{"id":40,"n":1,"u":"x40"}],"on_conflict":{"columns":["id"],"do":"nothing"},"operation_id":"n21-sql-first"}"#,
    );
    assert_eq!(http_common::wire_code_of(&resend), "23505");

    // HTTP で実行した UPSERT を、同内容の SQL で再送すると `23505`。
    let first = query(
        addr,
        &token,
        r#"{"op":"upsert","table":"docs","rows":[{"id":41,"n":1,"u":"x41"}],"on_conflict":{"columns":["id"],"do":"nothing"},"operation_id":"n21-http-first"}"#,
    );
    assert_eq!(upserted_of(&first), 1);
    let sql_resend = sql(
        &core,
        TENANT_A,
        "INSERT INTO docs (id, n, u) VALUES (41, 1, 'x41') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'n21-http-first'",
    );
    assert_eq!(sql_resend.expect_err("must be rejected"), "23505");
}

// --- RLS-9: 3 テナント越境 ---------------------------------------------------

/// tenant-b・tenant-c に、tenant-a が使う id・UNIQUE キーと同じ値の行を seed する。
fn seed_foreign(core: &EngineCore) {
    for (tenant, n) in [(TENANT_B, 111), (TENANT_C, 222)] {
        sql(
            core,
            tenant,
            &format!("INSERT INTO docs (id, n, u) VALUES (50, {n}, 'shared')"),
        )
        .expect("seed foreign");
    }
}

fn mask_operation_id(resp: &HttpResponse) -> String {
    let date = resp.header("date").expect("Date header").to_string();
    String::from_utf8_lossy(&resp.body).replace(&date, "")
}

#[test]
fn rls9_foreign_collisions_do_not_change_response_or_foreign_rows() {
    let bodies = [
        // id 衝突（DO NOTHING／DO UPDATE）。
        r#"{"op":"upsert","table":"docs","rows":[{"id":50,"n":1,"u":"mine"}],"on_conflict":{"columns":["id"],"do":"nothing"}}"#,
        r#"{"op":"upsert","table":"docs","rows":[{"id":50,"n":1,"u":"mine"}],"on_conflict":{"columns":["id"],"do":"update"}}"#,
        // UNIQUE キー衝突。
        r#"{"op":"upsert","table":"docs","rows":[{"id":51,"n":1,"u":"shared"}],"on_conflict":{"columns":["u"],"do":"nothing"}}"#,
        r#"{"op":"upsert","table":"docs","rows":[{"id":51,"n":1,"u":"shared"}],"on_conflict":{"columns":["u"],"do":"update"}}"#,
    ];
    for body in bodies {
        // 衝突なしの対照: 他テナントの行が無い core。
        let (clean_core, _g1) = new_core();
        let (clean_addr, clean_token) = login_alice(clean_core.clone());
        let clean = query(clean_addr, &clean_token, body);

        let (core, _g2) = new_core();
        seed_foreign(&core);
        let foreign_b = state(&core, TENANT_B);
        let foreign_c = state(&core, TENANT_C);
        let (addr, token) = login_alice(core.clone());
        let resp = query(addr, &token, body);

        assert_eq!(resp.status, clean.status, "body={body}");
        assert_eq!(upserted_of(&resp), upserted_of(&clean), "body={body}");
        assert_eq!(
            mask_operation_id(&resp),
            mask_operation_id(&clean),
            "response must be indistinguishable from the no-collision case: {body}"
        );
        // 他テナントの行は不変、自テナントには行ができる。
        assert_eq!(state(&core, TENANT_B), foreign_b, "tenant-b unchanged");
        assert_eq!(state(&core, TENANT_C), foreign_c, "tenant-c unchanged");
        assert_eq!(
            state(&core, TENANT_A),
            state(&clean_core, TENANT_A),
            "own rows identical to the no-collision case"
        );
        let text = String::from_utf8_lossy(&resp.body);
        assert!(!text.contains("tenant-"), "no tenant marker: {text}");
        assert!(!text.contains("shared"), "no foreign value: {text}");
    }
}
