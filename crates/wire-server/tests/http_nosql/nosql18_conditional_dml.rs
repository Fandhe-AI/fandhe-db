//! `POST /v1/query`（`op: "update"`／`op: "delete"`）の期待値の条件 `if` の契約を production
//! ルータ経由で固定する層 A 結合テスト（Issue #2098。対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-18・NOSQL-12、
//! `docs/spec/04-behavior/rls.md` RLS-9・RLS-10、
//! `docs/spec/04-behavior/recovery.md` RECOVER-10・RECOVER-11。設計判断は
//! `docs/design/conditional-update-existence-hiding.md`）。
//!
//! ## 役割分担
//!
//! - `crates/wire-server/src/http/query/{schema,dml_target,partitioned}.rs` の unit tests:
//!   `if` の型・空配列・併用拒否の境界。本ファイルは wire 越しの観測に徹する。
//! - 観点: 対象指定 3 形（従来の `id` 形・PRIMARY KEY 形・`filter` 形）× update／delete、
//!   不一致・不存在・他テナント・不可視で応答が 1 バイトも違わないこと（存在秘匿）、
//!   拒否系が副作用ゼロであること、台帳との相互作用、SQL 表層との `content_hash` 一致。

use crate::common;
use crate::http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    new_core_with(false)
}

/// `seeded` なら、条件 `lang = 'ja'` に一致する「他テナントの Public 行 id=100（読めるが所有しない）」と
/// 「他テナントの Private 行 id=200（セッションからは不可視）」を先に入れる（境界が破れていれば `if` が一致して作用してしまう配置）。
fn new_core_with(seeded: bool) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql18-conditional-dml");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    if seeded {
        for (tenant, id, visibility) in [
            (TENANT_B, 100, Visibility::Public),
            (TENANT_B, 200, Visibility::Private),
        ] {
            let ctx = PolicyContext::new(tenant).expect("valid tenant");
            engine::tenant::insert_typed_row(
                &storage,
                TABLE,
                &ctx,
                id,
                visibility,
                &[
                    Value::Vector(vec![0.1, 0.2, 0.3]),
                    Value::Text("ja".to_string()),
                ],
                &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id"),
            )
            .expect("seed row");
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::Ledgered);
    (Arc::new(core), guard)
}

fn ctx_all(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

struct Srv {
    addr: std::net::SocketAddr,
    token: String,
}

fn spawn(core: Arc<EngineCore>) -> Srv {
    let users = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr = http_common::spawn_router_listener_with_engine(&users, SessionStore::new(), core);
    let body = br#"{"user":"alice","password":"pw-alice"}"#;
    let req = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &req,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let token = match parse_json(&text).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected token, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    };
    Srv { addr, token }
}

impl Srv {
    fn q(&self, body: &str) -> HttpResponse {
        let req = http_common::build_request(
            "/v1/query",
            &[
                ("Authorization", &format!("Bearer {}", self.token)),
                ("Content-Type", "application/json"),
                ("Content-Length", &body.len().to_string()),
            ],
            body.as_bytes(),
        );
        http_common::parse_single_response(&http_common::send_raw(
            self.addr,
            &req,
            AfterWrite::HalfClose,
        ))
    }
}

fn text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn ok(resp: &HttpResponse) -> String {
    assert_eq!(resp.status, 200, "{resp:?} {}", text(resp));
    text(resp)
}

fn assert_code(resp: &HttpResponse, status: u16, code: &str) {
    assert_eq!(resp.status, status, "{resp:?} {}", text(resp));
    assert_eq!(http_common::wire_code_of(resp), code, "{}", text(resp));
}

/// `Date` を除いた応答全体（バイト同一性比較用）。
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

fn sql(core: &EngineCore, stmt: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx_all(TENANT_A), &mut session, stmt)
        .unwrap_or_else(|e| panic!("sql failed: {stmt}: {e:?}"));
}

/// tenant-a の（Private 込みで）読める行 `(id, lang)` を id 昇順で返す。
fn read_back(core: &EngineCore) -> Vec<(u64, Option<String>)> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx_all(TENANT_A),
            &mut session,
            "SELECT lang FROM docs LIMIT 100",
        )
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
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

fn insert(srv: &Srv, id: u64, lang: &str) {
    ok(&srv.q(&format!(
        r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2,0.3],"lang":"{lang}"}}],"operation_id":"seed-{id}"}}"#
    )));
}

const IF_JA: &str = r#"[{"column":"lang","op":"eq","value":"ja"}]"#;

fn upd_id(id: u64, cond: &str, op: &str) -> String {
    format!(
        r#"{{"op":"update","table":"docs","set":{{"lang":"en"}},"where":{{"id":{id}}},"if":{cond},"operation_id":"{op}"}}"#
    )
}

fn del_id(id: u64, cond: &str, op: &str) -> String {
    format!(
        r#"{{"op":"delete","table":"docs","where":{{"id":{id}}},"if":{cond},"operation_id":"{op}"}}"#
    )
}

// --- 1. 対象指定 3 形 × update／delete ------------------------------------------

#[test]
fn legacy_id_form_update_applies_only_when_condition_matches() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");

    let miss = ok(&srv.q(&upd_id(
        1,
        r#"[{"column":"lang","op":"eq","value":"fr"}]"#,
        "c1-miss",
    )));
    assert_eq!(miss, r#"{"updated":0,"operation_id":"c1-miss"}"#);
    assert_eq!(read_back(&core), vec![(1, Some("ja".to_string()))]);

    let hit = ok(&srv.q(&upd_id(1, IF_JA, "c1-hit")));
    assert_eq!(hit, r#"{"updated":1,"operation_id":"c1-hit"}"#);
    assert_eq!(read_back(&core), vec![(1, Some("en".to_string()))]);
}

#[test]
fn legacy_id_form_delete_applies_only_when_condition_matches() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");

    let miss = ok(&srv.q(&del_id(
        1,
        r#"[{"column":"lang","op":"eq","value":"fr"}]"#,
        "c2-miss",
    )));
    assert_eq!(miss, r#"{"deleted":0,"operation_id":"c2-miss"}"#);
    assert_eq!(read_back(&core), vec![(1, Some("ja".to_string()))]);

    let hit = ok(&srv.q(&del_id(1, IF_JA, "c2-hit")));
    assert_eq!(hit, r#"{"deleted":1,"operation_id":"c2-hit"}"#);
    assert!(read_back(&core).is_empty());
}

#[test]
fn filter_plus_condition_narrows_the_matched_rows() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");
    insert(&srv, 2, "ja");
    // `filter` だけなら 2 行当たる。`if` の `id` は疑似列のため、`lang` と別の条件で絞る代わりに
    // 先に片方の値を変えて 1 行にする。
    ok(&srv.q(
        r#"{"op":"update","table":"docs","set":{"lang":"fr"},"where":{"id":2},"operation_id":"c3-pre"}"#,
    ));
    let body = ok(&srv.q(
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[{"column":"lang","op":"in","value":["ja","fr"]}],"if":[{"column":"lang","op":"eq","value":"fr"}],"operation_id":"c3"}"#,
    ));
    assert!(body.contains(r#""updated":1"#), "{body}");
    assert_eq!(
        read_back(&core),
        vec![(1, Some("ja".to_string())), (2, Some("en".to_string()))]
    );

    let body = ok(&srv.q(
        r#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"in","value":["ja","en"]}],"if":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"c3-del"}"#,
    ));
    assert!(body.contains(r#""deleted":1"#), "{body}");
    assert_eq!(read_back(&core), vec![(2, Some("en".to_string()))]);
}

const COMPOSITE: &str = "CREATE TABLE t (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (a, b))";

fn seed_composite(core: &EngineCore, srv: &Srv) {
    sql(core, COMPOSITE);
    ok(&srv.q(
        r#"{"op":"insert","table":"t","rows":[{"a":"k","b":1,"v":"one"},{"a":"k","b":2,"v":"two"}],"operation_id":"seed-pk"}"#,
    ));
}

fn pk_values(srv: &Srv) -> String {
    ok(&srv.q(r#"{"op":"scan","table":"t","limit":100,"sort":[{"column":"b","dir":"asc"}]}"#))
}

#[test]
fn primary_key_form_update_and_delete_respect_condition() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);

    let miss = ok(&srv.q(
        r#"{"op":"update","table":"t","set":{"v":"x"},"where":{"a":"k","b":1},"if":[{"column":"v","op":"eq","value":"nope"}],"operation_id":"pk-u0"}"#,
    ));
    assert!(miss.contains(r#""updated":0"#), "{miss}");
    let hit = ok(&srv.q(
        r#"{"op":"update","table":"t","set":{"v":"x"},"where":{"a":"k","b":1},"if":[{"column":"v","op":"eq","value":"one"}],"operation_id":"pk-u1"}"#,
    ));
    assert!(hit.contains(r#""updated":1"#), "{hit}");
    let values = pk_values(&srv);
    assert!(
        values.contains(r#""x""#) && values.contains("two"),
        "{values}"
    );

    let miss = ok(&srv.q(
        r#"{"op":"delete","table":"t","where":{"a":"k","b":2},"if":[{"column":"v","op":"eq","value":"one"}],"operation_id":"pk-d0"}"#,
    ));
    assert!(miss.contains(r#""deleted":0"#), "{miss}");
    let hit = ok(&srv.q(
        r#"{"op":"delete","table":"t","where":{"a":"k","b":2},"if":[{"column":"v","op":"eq","value":"two"}],"operation_id":"pk-d1"}"#,
    ));
    assert!(hit.contains(r#""deleted":1"#), "{hit}");
    assert!(!pk_values(&srv).contains("two"));
}

// --- 2. 存在秘匿（全場合で応答が同一） ------------------------------------------

#[test]
fn zero_row_responses_are_byte_identical_across_all_non_match_causes() {
    // 同一 `operation_id` は内容が違うと 22023 になるため、原因ごとに新しい core を立てて
    // 同じ要求（同じ `operation_id`・同じ本文）を投げ、応答全体を比較する。
    // 条件 `lang = 'ja'` は、他テナントの Public 行(100)・Private 行(200)には一致する。
    // 自テナントの可視行(1)は不一致、999 は不存在。
    let run = |build: &dyn Fn(u64, &str) -> String, id: u64, op: &str| {
        let (core, g) = new_core_with(true);
        let srv = spawn(core.clone());
        insert(&srv, 1, "fr");
        let resp = srv.q(&build(id, op));
        (resp, core, g)
    };
    for (build, key, op) in [
        (
            &(|id, op: &str| upd_id(id, IF_JA, op)) as &dyn Fn(u64, &str) -> String,
            "updated",
            "same",
        ),
        (&|id, op: &str| del_id(id, IF_JA, op), "deleted", "same-d"),
    ] {
        let (mismatch, _c, _g) = run(build, 1, op);
        assert_eq!(
            ok(&mismatch),
            format!(r#"{{"{key}":0,"operation_id":"{op}"}}"#)
        );
        for id in [999, 100, 200] {
            let (other, core, _g) = run(build, id, op);
            assert_eq!(strip_date(&mismatch), strip_date(&other), "{key} id={id}");
            // 他テナント行・不可視行は変わらない。
            assert_eq!(
                read_back(&core),
                vec![(1, Some("fr".to_string())), (100, Some("ja".to_string()))]
            );
        }
    }

    let (core, _g) = new_core_with(true);
    let srv = spawn(core.clone());
    insert(&srv, 1, "fr");
    // filter 形でも、他テナント行・不可視行に一致し得る条件が 0 件応答になる。
    let by_filter = srv.q(
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[{"column":"lang","op":"eq","value":"ja"}],"if":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"same-f"}"#,
    );
    assert_eq!(ok(&by_filter), r#"{"updated":0,"operation_id":"same-f"}"#);
    // 自テナントの行(1)は変わらず、他テナントの行(100・200)も変わらない（所有者 tenant-b から読む）。
    assert_eq!(
        read_back(&core),
        vec![(1, Some("fr".to_string())), (100, Some("ja".to_string()))]
    );
    let mut session = SessionState::default();
    let SqlOutcome::Query(result) = core
        .execute_sql_in_session(
            &ctx_all(TENANT_B),
            &mut session,
            "SELECT lang FROM docs LIMIT 10",
        )
        .expect("select b")
    else {
        panic!("query expected");
    };
    let mut ja_ids: Vec<u64> = result
        .rows
        .iter()
        .filter(|r| matches!(r.cells.first(), Some(engine::sql::exec::Cell::Text(s)) if s == "ja"))
        .map(|r| r.id)
        .collect();
    ja_ids.sort();
    assert_eq!(ja_ids, vec![100, 200]);
}

// --- 3. 拒否系（engine 呼び出し前・副作用ゼロ） ---------------------------------

#[test]
fn invalid_conditions_are_rejected_without_side_effects() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");

    let cases: [(&str, &str); 9] = [
        ("[]", "42601"),
        ("{}", "42601"),
        ("null", "42601"),
        (r#"[{"column":"zz","op":"eq","value":"x"}]"#, "42703"),
        (r#"[{"column":"lang","op":"eq","value":5}]"#, "42601"),
        (r#"[{"column":"embedding","op":"gt","value":1}]"#, "22000"),
        (r#"[{"column":"visible","op":"eq","value":true}]"#, "42601"),
        (
            r#"[{"column":"lang","op":"like_all","value":"a"}]"#,
            "42601",
        ),
        (r#"[{"column":"lang"}]"#, "42601"),
    ];
    for (cond, code) in cases {
        for body in [upd_id(1, cond, "rej"), del_id(1, cond, "rej")] {
            let resp = srv.q(&body);
            assert_ne!(resp.status, 200, "cond={cond} {}", text(&resp));
            assert_eq!(
                http_common::wire_code_of(&resp),
                code,
                "cond={cond} {}",
                text(&resp)
            );
        }
    }
    assert_eq!(read_back(&core), vec![(1, Some("ja".to_string()))]);
    // 台帳へも記録されていない（拒否は engine 呼び出し前、または束縛失敗）。
    let ok_resp = ok(&srv.q(&upd_id(1, IF_JA, "rej")));
    assert!(ok_resp.contains(r#""updated":1"#), "{ok_resp}");
}

#[test]
fn condition_with_partitioned_mode_is_rejected_with_42601() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");
    let upd = srv.q(
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[{"column":"lang","op":"eq","value":"ja"}],"if":[{"column":"lang","op":"eq","value":"ja"}],"mode":"partitioned","operation_id":"pt-u"}"#,
    );
    assert_code(&upd, 400, "42601");
    let del = srv.q(
        r#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"eq","value":"ja"}],"if":[{"column":"lang","op":"eq","value":"ja"}],"mode":"partitioned","operation_id":"pt-d"}"#,
    );
    assert_code(&del, 400, "42601");
    assert_eq!(read_back(&core), vec![(1, Some("ja".to_string()))]);
}

#[test]
fn condition_alone_without_target_is_rejected_with_42601() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");
    let upd = srv.q(
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"if":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"alone-u"}"#,
    );
    assert_code(&upd, 400, "42601");
    let del = srv.q(
        r#"{"op":"delete","table":"docs","if":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"alone-d"}"#,
    );
    assert_code(&del, 400, "42601");
    assert_eq!(read_back(&core), vec![(1, Some("ja".to_string()))]);
}

#[test]
fn filter_plus_condition_exceeding_leaf_limit_is_rejected_with_54000() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");
    let leaves = |n: usize| {
        let leaf = r#"{"column":"lang","op":"eq","value":"ja"}"#;
        format!("[{}]", vec![leaf; n].join(","))
    };
    let body = format!(
        r#"{{"op":"update","table":"docs","set":{{"lang":"en"}},"filter":{},"if":{},"operation_id":"lim"}}"#,
        leaves(200),
        leaves(100)
    );
    let resp = srv.q(&body);
    assert_code(&resp, 413, "54000");
    assert_eq!(read_back(&core), vec![(1, Some("ja".to_string()))]);
}

// --- 4. 台帳 -----------------------------------------------------------------------

#[test]
fn ledger_records_zero_row_condition_attempts() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");
    let miss = r#"[{"column":"lang","op":"eq","value":"fr"}]"#;
    ok(&srv.q(&upd_id(1, miss, "led")));
    // 同じ operation_id・同じ内容 → 23505。
    assert_code(&srv.q(&upd_id(1, miss, "led")), 409, "23505");
    // 同じ operation_id・条件だけ違う → 22023。
    assert_code(&srv.q(&upd_id(1, IF_JA, "led")), 400, "22023");
    assert_eq!(read_back(&core), vec![(1, Some("ja".to_string()))]);
}

// --- 5. 表層間の content_hash 一致 --------------------------------------------------

#[test]
fn legacy_id_form_matches_sql_where_id_and_condition() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");
    sql(
        &core,
        "UPDATE docs SET lang = 'en' WHERE id = 1 AND lang = 'ja' USING OPERATION_ID 'x-u'",
    );
    assert_code(&srv.q(&upd_id(1, IF_JA, "x-u")), 409, "23505");

    insert(&srv, 2, "ja");
    sql(
        &core,
        "DELETE FROM docs WHERE id = 2 AND lang = 'ja' USING OPERATION_ID 'x-d'",
    );
    assert_code(&srv.q(&del_id(2, IF_JA, "x-d")), 409, "23505");
}

#[test]
fn filter_form_matches_sql_where_filter_and_condition() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    insert(&srv, 1, "ja");
    sql(
        &core,
        "UPDATE docs SET lang = 'en' WHERE lang IN ('ja', 'fr') AND lang = 'ja' USING OPERATION_ID 'xf-u'",
    );
    let resp = srv.q(
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[{"column":"lang","op":"in","value":["ja","fr"]}],"if":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"xf-u"}"#,
    );
    assert_code(&resp, 409, "23505");
}

#[test]
fn primary_key_form_matches_sql_where_pk_and_condition() {
    let (core, _g) = new_core();
    let srv = spawn(core.clone());
    seed_composite(&core, &srv);
    sql(
        &core,
        "UPDATE t SET v = 'x' WHERE a = 'k' AND b = 1 AND v = 'one' USING OPERATION_ID 'xp-u'",
    );
    let resp = srv.q(
        r#"{"op":"update","table":"t","set":{"v":"x"},"where":{"a":"k","b":1},"if":[{"column":"v","op":"eq","value":"one"}],"operation_id":"xp-u"}"#,
    );
    assert_code(&resp, 409, "23505");
}
