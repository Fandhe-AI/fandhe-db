//! `POST /v1/query`（NoSQL 表層）で、利用者ポリシー（RLS-13）が「テナント境界の内側で狭めるだけ」であり、
//! 他テナントの行の存在が HTTP 応答から推知できないことの横断回帰（Issue #2281）。
//! ポインタ: `docs/spec/04-behavior/rls.md` RLS-13・補助 RLS-12・`docs/spec/04-behavior/error-format.md`
//! ERR-4。設計記録は `docs/design/row-security-policy-composition.md`「非漏えい試験（#2281）」節。
//!
//! 既存の `rls13_nosql_policy.rs`（#2280）は SQL 表層との一致と隠れた行の存在秘匿を個別に固定する。
//! 本ファイルはそれを書き直さず、双子サーバー方式を足す。他テナントが Private 行（観測側と同じ id を
//! 含む）・自前の RLS・ポリシーを置いたサーバー（X）と、置いていないサーバー（Y）へ同じリクエスト列を
//! 送り、ステータス・`Date` を除くヘッダ・本文がバイト一致することを固定する（検索の順位・件数・
//! 更新件数・`explain`・エラー本文を含む）。各リクエストの期待ステータスと本文の要点はテスト側の独立な
//! 表で与え、production の判定は呼ばない。HTTP 主体はロール未束縛のため各表は `FORCE` を付ける。

use crate::common;
use crate::http_common;

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

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

/// alice の行の真実（独立オラクル）。ポリシー `lang = 'ja'` を通るのは id 1・3。
const ALICE_ROWS: [(u64, &str, i32, [f32; 2]); 4] = [
    (1, "ja", 1, [1.0, 0.1]),
    (2, "en", 2, [1.0, 0.2]),
    (3, "ja", 3, [1.0, 0.3]),
    (4, "fr", 4, [1.0, 0.4]),
];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn sql_ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

/// alice の行とポリシーを先に作り（内部 ID を X/Y で揃える）、`foreign` のときだけ bob（tenant-b）の
/// Private 行（alice と同じ id 1・2、alice に無い id 100・200。クエリに最も近いベクトル）・bob 自身の
/// RLS とポリシーを置く。bob の `n` は 1000 + id で、alice の応答に現れれば漏えいと判別できる。
fn fixture(label: &str, foreign: bool) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    for (id, lang, n, e) in ALICE_ROWS {
        sql_ok(
            &core,
            TENANT_A,
            &format!(
                "INSERT INTO docs (id, embedding, lang, n) VALUES ({id}, '[{},{}]', '{lang}', {n})",
                e[0], e[1]
            ),
        );
    }
    for sql in [
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE docs FORCE ROW LEVEL SECURITY",
        "CREATE POLICY p ON docs FOR ALL USING (lang = 'ja') WITH CHECK (lang = 'ja')",
    ] {
        sql_ok(&core, TENANT_A, sql);
    }
    if foreign {
        for id in [1u64, 2, 100, 200] {
            sql_ok(
                &core,
                TENANT_B,
                &format!(
                    "INSERT INTO docs (id, embedding, lang, n) \
                     VALUES ({id}, '[1,0.2]', 'ja', {})",
                    1000 + id
                ),
            );
        }
        sql_ok(
            &core,
            TENANT_B,
            "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        );
        sql_ok(
            &core,
            TENANT_B,
            "CREATE POLICY zz_secret_pol ON docs FOR ALL USING (true)",
        );
    }
    (core, guard)
}

struct Http {
    addr: std::net::SocketAddr,
    token: String,
}

fn http(core: &Arc<EngineCore>) -> Http {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        Arc::clone(core),
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
        addr,
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
    Http { addr, token }
}

fn q(h: &Http, body: &str) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", h.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        h.addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

/// `Date` ヘッダを除いた応答全体（バイト比較用）。
fn strip_date(resp: &HttpResponse) -> String {
    let mut out = format!("{} {}\n", resp.status, resp.reason);
    for (name, value) in &resp.headers {
        if !name.eq_ignore_ascii_case("date") {
            out.push_str(&format!("{name}: {value}\n"));
        }
    }
    out.push('\n');
    out.push_str(&body_text(resp));
    out
}

/// 1 リクエストの期待。`contains` は本文に含まれるべき断片（空なら検査しない）。
struct Step {
    body: String,
    status: u16,
    contains: &'static str,
}

fn step(body: &str, status: u16, contains: &'static str) -> Step {
    Step {
        body: body.to_string(),
        status,
        contains,
    }
}

fn insert_body(id: u64, lang: &str) -> String {
    format!(
        r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.5,0.5],"lang":"{lang}","n":0}}]}}"#
    )
}

/// 読み取り・書き込みの全 op を通す系列。期待値は alice の行の真実だけから出す（他テナントの行は
/// 期待に一切入らない）。書き込みは X/Y で同じ順に適用されるので状態も揃う。
fn script() -> Vec<Step> {
    vec![
        // 読み取り: 他テナントと同じ id・他テナントにだけ在る id は、存在しない id と同じ 0 件。
        step(
            r#"{"op":"scan","table":"docs","limit":100,"columns":["id","lang"]}"#,
            200,
            r#""row_count":2"#,
        ),
        step(
            r#"{"op":"scan","table":"docs","limit":100,"columns":["id"],"filter":[{"column":"n","op":"eq","value":1100}]}"#,
            200,
            r#""row_count":0"#,
        ),
        step(
            r#"{"op":"scan","table":"docs","limit":100,"columns":["id"],"filter":[{"column":"n","op":"eq","value":2}]}"#,
            200,
            r#""row_count":0"#,
        ),
        step(
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"n"}]}"#,
            200,
            "",
        ),
        step(
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"]}"#,
            200,
            "",
        ),
        // 検索: 他テナントの最近傍行（ベクトル [1,0.2]）が枠を消費せず、順位・スコアが変わらない。
        step(
            r#"{"op":"search","table":"docs","vector":[1.0,0.2],"limit":2,"columns":["id"]}"#,
            200,
            r#""row_count":2"#,
        ),
        step(
            r#"{"op":"scan","table":"docs","limit":5,"explain":true}"#,
            200,
            "",
        ),
        step(
            r#"{"op":"search","table":"docs","vector":[1.0,0.2],"limit":2,"explain":true}"#,
            200,
            "",
        ),
        // 書き込み: 他テナントと同じ id の insert は成功、WITH CHECK 違反は 403・42501、
        // 隠れた自行と同じ id は 409・23505（テナント内の一意性）。
        step(&insert_body(100, "ja"), 200, ""),
        step(&insert_body(51, "SECRETLANG"), 403, "42501"),
        step(&insert_body(2, "ja"), 409, "23505"),
        // update・delete: 他テナントにだけ在る id・隠れた自行は存在しない id と同じ 0 件。
        step(
            r#"{"op":"update","table":"docs","set":{"lang":"ja","n":77},"where":{"id":200}}"#,
            200,
            r#""updated":0"#,
        ),
        step(
            r#"{"op":"update","table":"docs","set":{"lang":"ja","n":77},"where":{"id":2}}"#,
            200,
            r#""updated":0"#,
        ),
        step(
            r#"{"op":"update","table":"docs","set":{"lang":"ja","n":77},"where":{"id":1}}"#,
            200,
            r#""updated":1"#,
        ),
        step(
            r#"{"op":"update","table":"docs","set":{"lang":"SECRETLANG","n":1},"where":{"id":3}}"#,
            403,
            "42501",
        ),
        step(
            r#"{"op":"delete","table":"docs","where":{"id":200}}"#,
            200,
            r#""deleted":0"#,
        ),
        step(
            r#"{"op":"delete","table":"docs","where":{"id":2}}"#,
            200,
            r#""deleted":0"#,
        ),
        step(
            r#"{"op":"update","table":"docs","set":{"n":500},"filter":[{"column":"n","op":"gte","value":0}]}"#,
            200,
            "",
        ),
        step(
            r#"{"op":"update","table":"docs","set":{"n":600},"filter":[{"column":"n","op":"gte","value":0}],"mode":"partitioned","operation_id":"nl-upd"}"#,
            200,
            "",
        ),
        step(
            r#"{"op":"delete","table":"docs","filter":[{"column":"n","op":"gte","value":1000}],"mode":"partitioned","operation_id":"nl-del"}"#,
            200,
            "",
        ),
        step(
            r#"{"op":"scan","table":"docs","limit":100,"columns":["id","n"]}"#,
            200,
            "",
        ),
    ]
}

/// 双子サーバー（X: 他テナントの行あり・Y: なし）へ同じ系列を流し、全応答がバイト一致する。各応答は
/// 期待ステータス・要点に従い、他テナントの値・テナント ID・セッショントークンを含まない。
#[test]
fn http_responses_are_identical_with_and_without_foreign_tenant_rows() {
    let (core_x, _gx) = fixture("rls13-nosql-nl-x", true);
    let (core_y, _gy) = fixture("rls13-nosql-nl-y", false);
    let (hx, hy) = (http(&core_x), http(&core_y));
    let steps = script();
    let mut compared = 0usize;
    for (i, s) in steps.iter().enumerate() {
        let (rx, ry) = (q(&hx, &s.body), q(&hy, &s.body));
        assert_eq!(
            rx.status,
            s.status,
            "step {i} {}: {}",
            s.body,
            body_text(&rx)
        );
        assert!(
            s.contains.is_empty() || body_text(&rx).contains(s.contains),
            "step {i} {}: expected {} in {}",
            s.body,
            s.contains,
            body_text(&rx)
        );
        assert_eq!(strip_date(&rx), strip_date(&ry), "step {i}: {}", s.body);
        let text = body_text(&rx);
        assert!(!text.contains(TENANT_B), "step {i}: {text}");
        // bob の n は 1001・1002・1100・1200。alice の応答に現れたら漏えい。
        for leaked in ["1001", "1002", "1100", "1200"] {
            assert!(!text.contains(leaked), "step {i}: {leaked} in {text}");
        }
        if rx.status >= 400 {
            // 拒否本文に行の値・テナント ID・トークンを含めない。
            http_common::assert_message_does_not_echo(&rx, "SECRETLANG");
            http_common::assert_message_does_not_echo(&rx, TENANT_A);
            http_common::assert_message_does_not_echo(&rx, &hx.token);
        }
        compared += 1;
    }
    assert_eq!(compared, steps.len());
    assert!(compared >= 20, "script drifted: {compared}");

    // 他テナントの行は全リクエスト後も初期値のまま（bob の視点で SQL から読む）。
    let mut s = SessionState::default();
    s.allow_ddl();
    let rows = match core_x
        .execute_sql_in_session(
            &ctx(TENANT_B),
            &mut s,
            "SELECT id, n FROM docs ORDER BY id LIMIT 100",
        )
        .expect("bob read")
    {
        SqlOutcome::Query(q) => q.rows,
        other => panic!("{other:?}"),
    };
    let dump: Vec<(u64, String)> = rows
        .iter()
        .map(|r| (r.id, format!("{:?}", r.cells)))
        .collect();
    let ids: Vec<u64> = dump.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![1, 2, 100, 200], "{dump:?}");
    for (id, cells) in &dump {
        assert!(
            cells.contains(&format!("{}", 1000 + id)),
            "bob row {id} changed: {cells}"
        );
    }
}

/// 双子サーバー間で本文が一致するだけでは「両方が空振りで同じエラー」の場合を見逃すため、陽性対照として
/// 観測側が見る行集合が真実（id 1・3）と一致することを固定する。
#[test]
fn visible_set_matches_oracle_with_foreign_rows_present() {
    let (core, _g) = fixture("rls13-nosql-nl-oracle", true);
    let h = http(&core);
    let r = q(
        &h,
        r#"{"op":"scan","table":"docs","limit":100,"columns":["id"],"sort":[{"column":"id","dir":"asc"}]}"#,
    );
    assert_eq!(r.status, 200, "{}", body_text(&r));
    let text = body_text(&r);
    let JsonValue::Object(mut top) = parse_json(&text).expect("json") else {
        panic!("object expected: {text}");
    };
    let Some(JsonValue::Array(rows)) = top.remove("rows") else {
        panic!("rows: {text}");
    };
    let ids: Vec<u64> = rows
        .into_iter()
        .map(|r| match r {
            JsonValue::Array(cells) => match cells.first() {
                Some(JsonValue::Number(n)) => n.as_f64() as u64,
                other => panic!("id cell: {other:?}"),
            },
            other => panic!("row: {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 3]);
}
