//! `POST /v1/query`（NoSQL 表層）の読み取り・書き込み op に、SQL と同じ利用者ポリシー（RLS-13・
//! Issue #2280）が効くことを production ルータ経由（生バイトクライアント）で固定する層 A 結合テスト。
//! ポインタ: `docs/spec/04-behavior/rls.md` RLS-13・`docs/spec/04-behavior/error-format.md` ERR-4・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1／6／12。設計記録は
//! `docs/design/row-security-policy-composition.md` の「NoSQL・COPY への適用（#2280）」。
//!
//! NoSQL の各 op は engine の共有実行器（SQL 表層と同じ `resolve_row_policy`／`RowPolicyWrite`）へ
//! 到達するため、wire-server に第 2 の評価器は無い。本ファイルは「同じテナント・同じ主体で SQL を
//! 実行したときと同じ行集合・件数になる」「ポリシーで隠れた id は存在しない id と応答が同じ」
//! 「WITH CHECK 違反は 403・`42501`（新しいラベルを作らない）」を、期待値を Rust 側の独立オラクルで
//! 出して固定する。HTTP 主体はロール未束縛（所有者除外・`BYPASSRLS` なし）なので、各表は `FORCE`
//! を付けてロール束縛の有無で結果が変わらないようにしてある。

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

/// alice（tenant-a）のシード行の真実（独立オラクル。production の判定は一切通さない）。
/// ポリシー `lang = 'ja'` を通るのは id 1・3 だけ。id 2（en）・4（fr）は隠れる。
const ALICE_ROWS: [(u64, &str, i32, [f32; 2]); 4] = [
    (1, "ja", 1, [1.0, 0.1]),
    (2, "en", 2, [1.0, 0.2]),
    (3, "ja", 3, [1.0, 0.3]),
    (4, "fr", 4, [1.0, 0.4]),
];
const VISIBLE: [u64; 2] = [1, 3];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn sql_ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut s = ddl_session();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn sql_ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<u64> {
    match sql_ok(core, tenant, sql) {
        SqlOutcome::Query(q) => q.rows.iter().map(|r| r.id).collect(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn sql_sorted_ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<u64> {
    let mut v = sql_ids(core, tenant, sql);
    v.sort_unstable();
    v
}

/// `docs(embedding, lang, n)` を作り、alice の 4 行と bob の 1 行（id 100）を入れ、`with_policy` なら
/// alice のテナントで `ENABLE`・`FORCE` と `lang = 'ja'` の `FOR ALL` ポリシーを掛ける。
fn fixture(label: &str, with_policy: bool) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    fixture_with(label, with_policy, 0)
}

/// [`fixture`] に、ポリシーを掛ける前に alice の `lang = 'en'` 行（id 10 以降）を `extra_hidden` 件足す版。
fn fixture_with(
    label: &str,
    with_policy: bool,
    extra_hidden: u64,
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
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
    sql_ok(
        &core,
        TENANT_B,
        "INSERT INTO docs (id, embedding, lang, n) VALUES (100, '[1,0.2]', 'ja', 100)",
    );
    for id in 10..10 + extra_hidden {
        sql_ok(
            &core,
            TENANT_A,
            &format!(
                "INSERT INTO docs (id, embedding, lang, n) VALUES ({id}, '[1,0.5]', 'en', {id})"
            ),
        );
    }
    if with_policy {
        for sql in [
            "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE docs FORCE ROW LEVEL SECURITY",
            "CREATE POLICY p ON docs FOR ALL USING (lang = 'ja') WITH CHECK (lang = 'ja')",
        ] {
            sql_ok(&core, TENANT_A, sql);
        }
    }
    (core, guard)
}

fn spawn_alice_session(core: Arc<EngineCore>) -> (std::net::SocketAddr, String) {
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
    let token = match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };
    (addr, token)
}

struct Http {
    addr: std::net::SocketAddr,
    token: String,
}

fn http(core: &Arc<EngineCore>) -> Http {
    let (addr, token) = spawn_alice_session(Arc::clone(core));
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

/// Date ヘッダを除いた応答全体（存在オラクルの同一性比較用）。
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

/// 成功応答の `rows` から `id` 列（`columns` に `id` を含めた要求）を取り出す（応答順）。
fn ids_in_order(resp: &HttpResponse) -> Vec<u64> {
    assert_eq!(resp.status, 200, "expected 200: {}", body_text(resp));
    let text = body_text(resp);
    let JsonValue::Object(mut top) = parse_json(&text).expect("json") else {
        panic!("object expected: {text}");
    };
    let Some(JsonValue::Array(cols)) = top.remove("columns") else {
        panic!("columns: {text}");
    };
    let idx = cols
        .iter()
        .position(|c| matches!(c, JsonValue::Object(o) if matches!(o.get("name"), Some(JsonValue::String(s)) if s == "id")))
        .unwrap_or_else(|| panic!("id column missing: {text}"));
    let Some(JsonValue::Array(rows)) = top.remove("rows") else {
        panic!("rows: {text}");
    };
    rows.into_iter()
        .map(|r| match r {
            JsonValue::Array(cells) => match cells.get(idx) {
                Some(JsonValue::Number(n)) => n.as_f64() as u64,
                other => panic!("id cell: {other:?}"),
            },
            other => panic!("row: {other:?}"),
        })
        .collect()
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

/// 自テナント（alice）の全行 id（昇順。SQL でテナント境界のみ・ポリシーを経由した結果）。
fn alice_ids(core: &EngineCore) -> Vec<u64> {
    sql_sorted_ids(core, TENANT_A, "SELECT id FROM docs LIMIT 1000")
}

// ---------------------------------------------------------------------
// 読み取り（R1）
// ---------------------------------------------------------------------

/// RLS-13: `scan` の行集合は SQL `SELECT` と一致し、隠れた行・他テナントの行を含まない。
#[test]
fn scan_matches_sql_select_and_oracle() {
    let (core, _g) = fixture("rls13-nosql-scan", true);
    let h = http(&core);
    let resp = q(
        &h,
        r#"{"op":"scan","table":"docs","limit":100,"columns":["id","lang"]}"#,
    );
    let got = sorted(ids_in_order(&resp));
    assert_eq!(got, VISIBLE.to_vec());
    assert_eq!(
        got,
        sql_sorted_ids(&core, TENANT_A, "SELECT id, lang FROM docs LIMIT 100")
    );

    // filter・sort・offset 付き。ポリシーは利用者の述語より先に効く。
    let resp = q(
        &h,
        r#"{"op":"scan","table":"docs","limit":100,"columns":["id","n"],"filter":[{"column":"n","op":"gte","value":0}],"sort":[{"column":"n","dir":"desc"}],"offset":1}"#,
    );
    assert_eq!(
        ids_in_order(&resp),
        sql_ids(
            &core,
            TENANT_A,
            "SELECT id, n FROM docs WHERE n >= 0 ORDER BY n DESC LIMIT 100 OFFSET 1"
        )
    );
    assert_eq!(ids_in_order(&resp), vec![1]);
}

/// RLS-13: 主キー等値（PK 点取得。SQL の `CREATE TABLE` で作った表）で隠れた行を指しても、存在しない
/// キーを指したときと応答が同じ。
#[test]
fn scan_by_primary_key_hidden_and_missing_are_indistinguishable() {
    let (core, _g) = fixture("rls13-nosql-scan-pk", false);
    for sql in [
        "CREATE TABLE kv (k BIGINT PRIMARY KEY, lang TEXT)",
        "INSERT INTO kv (k, lang) VALUES (1, 'ja')",
        "INSERT INTO kv (k, lang) VALUES (2, 'en')",
        "ALTER TABLE kv ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE kv FORCE ROW LEVEL SECURITY",
        "CREATE POLICY p ON kv FOR ALL USING (lang = 'ja') WITH CHECK (lang = 'ja')",
    ] {
        sql_ok(&core, TENANT_A, sql);
    }
    let h = http(&core);
    let by_key = |k: u64| {
        q(
            &h,
            &format!(
                r#"{{"op":"scan","table":"kv","limit":10,"columns":["k","lang"],"filter":[{{"column":"k","op":"eq","value":{k}}}]}}"#
            ),
        )
    };
    let visible = by_key(1);
    assert!(
        body_text(&visible).contains("\"ja\""),
        "{}",
        body_text(&visible)
    );
    let hidden = by_key(2);
    let missing = by_key(999);
    assert_eq!(hidden.status, 200, "{}", body_text(&hidden));
    assert!(
        body_text(&hidden).contains(r#""row_count":0"#),
        "{}",
        body_text(&hidden)
    );
    assert_eq!(strip_date(&hidden), strip_date(&missing));
}

/// RLS-13: `aggregate`（`COUNT`・`SUM`・`GROUP BY`）はポリシーを通る行だけを集計し、SQL と一致する。
#[test]
fn aggregate_matches_sql_and_oracle() {
    let (core, _g) = fixture("rls13-nosql-agg", true);
    let h = http(&core);
    let oracle_count = VISIBLE.len() as i64;
    let oracle_sum: i64 = ALICE_ROWS
        .iter()
        .filter(|r| VISIBLE.contains(&r.0))
        .map(|r| i64::from(r.2))
        .sum();

    let resp = q(
        &h,
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"n"}]}"#,
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    let text = body_text(&resp);
    let JsonValue::Object(mut top) = parse_json(&text).expect("json") else {
        panic!("object: {text}");
    };
    let Some(JsonValue::Array(rows)) = top.remove("rows") else {
        panic!("rows: {text}");
    };
    assert_eq!(rows.len(), 1, "{text}");
    let JsonValue::Array(cells) = &rows[0] else {
        panic!("row: {text}");
    };
    let num = |c: Option<&JsonValue>| match c {
        Some(JsonValue::Number(n)) => n.as_f64() as i64,
        other => panic!("number expected: {other:?} in {text}"),
    };
    assert_eq!(num(cells.first()), oracle_count);
    assert_eq!(num(cells.get(1)), oracle_sum);

    // SQL の同じ集計と一致する。
    let SqlOutcome::Query(sql) = sql_ok(&core, TENANT_A, "SELECT COUNT(*), SUM(n) FROM docs")
    else {
        panic!("query expected");
    };
    let cell_i64 = |c: &engine::sql::exec::Cell| match c {
        engine::sql::exec::Cell::Integer(v) => i64::try_from(*v).expect("fits"),
        engine::sql::exec::Cell::SignedInteger(v) => *v,
        other => panic!("integer cell expected: {other:?}"),
    };
    assert_eq!(cell_i64(&sql.rows[0].cells[0]), oracle_count);
    assert_eq!(cell_i64(&sql.rows[0].cells[1]), oracle_sum);

    // GROUP BY: 隠れた lang のグループは出ない。
    let resp = q(
        &h,
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"]}"#,
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    let text = body_text(&resp);
    assert!(text.contains("\"ja\""), "{text}");
    assert!(
        !text.contains("\"en\"") && !text.contains("\"fr\""),
        "{text}"
    );
}

/// RLS-13: `search`（`vector`）は SQL の距離順 `SELECT` と一致し、隠れた最近傍が上位 k の枠を消費しない。
#[test]
fn search_vector_matches_sql_and_hidden_nearest_does_not_consume_slots() {
    let (core, _g) = fixture("rls13-nosql-search", true);
    let h = http(&core);
    // クエリは隠れた id 2 の埋め込みそのもの（最近傍）。
    let resp = q(
        &h,
        r#"{"op":"search","table":"docs","vector":[1.0,0.2],"limit":2,"columns":["id"]}"#,
    );
    let got = ids_in_order(&resp);
    assert_eq!(
        sorted(got.clone()),
        VISIBLE.to_vec(),
        "{}",
        body_text(&resp)
    );
    let sql = sql_ids(
        &core,
        TENANT_A,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.2]' LIMIT 2",
    );
    assert_eq!(got, sql);
}

/// RLS-13: 適用表への `search` + `plan`（辞書構築）は SQL の `USING PLAN` と同じ分類で fail-closed に拒否し、
/// 文言に表名・ポリシー名を含めない。ポリシーを掛ける前は同じ要求が成功する（拒否の原因がポリシーであること）。
#[test]
fn search_plan_on_policy_table_is_rejected_like_sql_without_names() {
    use engine::embedding::{EmbedError, Embedder};
    use engine::query_planner::{LlmClient, PlanError};

    struct Emb;
    impl Embedder for Emb {
        fn dim(&self) -> u32 {
            2
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|t| vec![t.len() as f32 * 0.01; 2])
                .collect())
        }
    }
    struct Llm;
    impl LlmClient for Llm {
        fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
            Ok(r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#.to_string())
        }
    }

    let path = temp_db::unique_db_path("rls13-nosql-plan");
    let _g = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_embedder(Box::new(Emb))
            .with_query_planner(Box::new(Llm)),
    );
    sql_ok(
        &core,
        TENANT_A,
        "INSERT INTO docs (id, embedding, lang, path, body) VALUES (1, '[0.1,0.2]', 'ja', 'a.md', 'alpha text')",
    );
    let h = http(&core);
    let body = r#"{"op":"search","table":"docs","plan":"find content","limit":5,"columns":["id"]}"#;
    let before = q(&h, body);
    assert_eq!(before.status, 200, "{}", body_text(&before));

    for sql in [
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE docs FORCE ROW LEVEL SECURITY",
        "CREATE POLICY secretpolicy ON docs FOR ALL USING (lang = 'ja')",
    ] {
        sql_ok(&core, TENANT_A, sql);
    }
    let mut sess = ddl_session();
    let sql_err = core
        .execute_sql_in_session(
            &ctx(TENANT_A),
            &mut sess,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 5",
        )
        .expect_err("USING PLAN must be rejected");
    let resp = q(&h, body);
    // 現状は SQL の `USING PLAN` と同じ内部エラー分類（`XX000`・500）で fail-closed に拒否される
    // （`0A000` への分類替えは別 Issue。ここでは SQL との一致と非漏えいを固定する）。
    assert_eq!(http_common::wire_code_of(&resp), sql_err.wire_code());
    assert_eq!(resp.status, 500, "{}", body_text(&resp));
    http_common::assert_message_does_not_echo(&resp, "docs");
    http_common::assert_message_does_not_echo(&resp, "secretpolicy");
}

/// RLS-13: `explain: true` の応答は、隠れた行を増やしても変わらない（除外前の件数・候補数を出さない）。
#[test]
fn explain_does_not_depend_on_hidden_rows() {
    let bodies = [
        r#"{"op":"scan","table":"docs","limit":5,"explain":true}"#,
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"explain":true}"#,
        r#"{"op":"search","table":"docs","vector":[1.0,0.2],"limit":2,"explain":true}"#,
    ];
    let (core_a, _ga) = fixture("rls13-nosql-explain-a", true);
    // core_b だけに隠れた行（lang = 'en'）を増やす。
    let (core_b, _gb) = fixture_with("rls13-nosql-explain-b", true, 20);
    let (ha, hb) = (http(&core_a), http(&core_b));
    for body in bodies {
        let (ra, rb) = (q(&ha, body), q(&hb, body));
        assert_eq!(ra.status, 200, "{}", body_text(&ra));
        assert_eq!(body_text(&ra), body_text(&rb), "{body}");
    }
}

// ---------------------------------------------------------------------
// 書き込み（R2）
// ---------------------------------------------------------------------

fn insert_body(id: u64, lang: &str) -> String {
    format!(
        r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.5,0.5],"lang":"{lang}","n":0}}]}}"#
    )
}

/// RLS-13: `insert` は WITH CHECK を満たす行だけ書け、違反は 403・`42501` で何も書かれない。
#[test]
fn insert_with_check_violation_is_403_42501_and_writes_nothing() {
    let (core, _g) = fixture("rls13-nosql-insert", true);
    let h = http(&core);
    let before = alice_ids(&core);

    let ok = q(&h, &insert_body(50, "ja"));
    assert_eq!(ok.status, 200, "{}", body_text(&ok));
    assert_eq!(
        alice_ids(&core),
        sorted([before.clone(), vec![50]].concat())
    );

    let secret = "SECRETLANG";
    let bad = q(&h, &insert_body(51, secret));
    assert_eq!(bad.status, 403, "{}", body_text(&bad));
    assert_eq!(http_common::wire_code_of(&bad), "42501");
    assert!(body_text(&bad).contains("FORBIDDEN_TENANT_MISMATCH"));
    http_common::assert_message_does_not_echo(&bad, secret);
    http_common::assert_message_does_not_echo(&bad, TENANT_A);
    http_common::assert_message_does_not_echo(&bad, &h.token);
    assert_eq!(alice_ids(&core), sorted([before, vec![50]].concat()));
}

/// RLS-13: 隠れた同テナント行と id が衝突する `insert` は `23505`（テナント内の一意性判定。想定どおり）。
/// 他テナントの行と衝突する id は従来どおり成功する（他テナントの存在は漏れない）。
#[test]
fn insert_id_collision_is_tenant_scoped() {
    let (core, _g) = fixture("rls13-nosql-insert-collide", true);
    let h = http(&core);
    let hidden = q(&h, &insert_body(2, "ja"));
    assert_eq!(hidden.status, 409, "{}", body_text(&hidden));
    assert_eq!(http_common::wire_code_of(&hidden), "23505");
    // bob の id 100 とは衝突しない。
    let other = q(&h, &insert_body(100, "ja"));
    assert_eq!(other.status, 200, "{}", body_text(&other));
}

/// RLS-13: `update`（`where` 単一行形）。見える行は更新でき、隠れた行は存在しない id と応答が同じ
/// （`updated: 0`）で変わらない。WITH CHECK に違反する書き換えは 403・`42501` で行は変わらない。
#[test]
fn update_single_row_respects_using_and_with_check() {
    let (core, _g) = fixture("rls13-nosql-update-one", true);
    let h = http(&core);
    let upd = |id: u64, lang: &str, n: i32| {
        q(
            &h,
            &format!(
                r#"{{"op":"update","table":"docs","set":{{"lang":"{lang}","n":{n}}},"where":{{"id":{id}}}}}"#
            ),
        )
    };
    let ok = upd(1, "ja", 77);
    assert_eq!(ok.status, 200, "{}", body_text(&ok));
    assert!(
        body_text(&ok).contains(r#""updated":1"#),
        "{}",
        body_text(&ok)
    );

    let hidden = upd(2, "ja", 88);
    let missing = upd(999, "ja", 88);
    assert!(
        body_text(&hidden).contains(r#""updated":0"#),
        "{}",
        body_text(&hidden)
    );
    assert_eq!(strip_date(&hidden), strip_date(&missing));

    let bad = upd(3, "SECRETLANG", 99);
    assert_eq!(bad.status, 403, "{}", body_text(&bad));
    assert_eq!(http_common::wire_code_of(&bad), "42501");
    http_common::assert_message_does_not_echo(&bad, "SECRETLANG");

    // 隠れた行（id 2）・違反で拒否された行（id 3）は変わっていない。
    assert_eq!(
        sql_sorted_ids(&core, TENANT_A, "SELECT id FROM docs WHERE n = 77 LIMIT 10"),
        vec![1]
    );
    assert_eq!(
        sql_sorted_ids(&core, TENANT_A, "SELECT id FROM docs WHERE n = 88 LIMIT 10"),
        Vec::<u64>::new()
    );
    assert_eq!(
        sql_sorted_ids(&core, TENANT_A, "SELECT id FROM docs WHERE n = 3 LIMIT 10"),
        vec![3]
    );
}

/// RLS-13: `delete`（`where` 単一行形）。隠れた id は存在しない id と応答が同じ（`deleted: 0`）で、行は残る。
#[test]
fn delete_single_row_hidden_is_indistinguishable_from_missing() {
    let (core, _g) = fixture("rls13-nosql-delete-one", true);
    let h = http(&core);
    let del = |id: u64| {
        q(
            &h,
            &format!(r#"{{"op":"delete","table":"docs","where":{{"id":{id}}}}}"#),
        )
    };
    let hidden = del(2);
    let missing = del(999);
    assert!(
        body_text(&hidden).contains(r#""deleted":0"#),
        "{}",
        body_text(&hidden)
    );
    assert_eq!(strip_date(&hidden), strip_date(&missing));
    // 隠れた行は残っている（ポリシーを外して確認する）。
    sql_ok(
        &core,
        TENANT_A,
        "ALTER TABLE docs DISABLE ROW LEVEL SECURITY",
    );
    assert!(alice_ids(&core).contains(&2));
    sql_ok(
        &core,
        TENANT_A,
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
    );

    let ok = del(1);
    assert!(
        body_text(&ok).contains(r#""deleted":1"#),
        "{}",
        body_text(&ok)
    );
}

/// RLS-13: 述語形の `update`／`delete` の影響件数・対象行は、別フィクスチャで実行した SQL の同じ述語の
/// UPDATE／DELETE と一致する。
#[test]
fn predicate_update_and_delete_match_sql() {
    let filter = r#""filter":[{"column":"n","op":"gte","value":0}]"#;
    // UPDATE
    let (core_h, _gh) = fixture("rls13-nosql-pred-upd-h", true);
    let (core_s, _gs) = fixture("rls13-nosql-pred-upd-s", true);
    let resp = q(
        &http(&core_h),
        &format!(r#"{{"op":"update","table":"docs","set":{{"n":500}},{filter}}}"#),
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    let SqlOutcome::Update(sql) = sql_ok(&core_s, TENANT_A, "UPDATE docs SET n = 500 WHERE n >= 0")
    else {
        panic!("update outcome expected");
    };
    assert_eq!(sql.rows_affected, VISIBLE.len() as u64);
    assert!(
        body_text(&resp).contains(&format!(r#""updated":{}"#, sql.rows_affected)),
        "{}",
        body_text(&resp)
    );
    let probe = "SELECT id FROM docs WHERE n = 500 LIMIT 100";
    assert_eq!(
        sql_sorted_ids(&core_h, TENANT_A, probe),
        sql_sorted_ids(&core_s, TENANT_A, probe)
    );
    assert_eq!(sql_sorted_ids(&core_h, TENANT_A, probe), VISIBLE.to_vec());

    // DELETE
    let (core_h, _gh) = fixture("rls13-nosql-pred-del-h", true);
    let (core_s, _gs) = fixture("rls13-nosql-pred-del-s", true);
    let resp = q(
        &http(&core_h),
        &format!(r#"{{"op":"delete","table":"docs",{filter}}}"#),
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    let SqlOutcome::Delete(sql) = sql_ok(&core_s, TENANT_A, "DELETE FROM docs WHERE n >= 0") else {
        panic!("delete outcome expected");
    };
    assert!(
        body_text(&resp).contains(&format!(r#""deleted":{}"#, sql.rows_affected)),
        "{}",
        body_text(&resp)
    );
    for core in [&core_h, &core_s] {
        sql_ok(
            core,
            TENANT_A,
            "ALTER TABLE docs DISABLE ROW LEVEL SECURITY",
        );
        // 隠れていた行（id 2・4）だけが残る。
        assert_eq!(alice_ids(core), vec![2, 4]);
    }
}

/// RLS-13: 分割実行形（`mode: "partitioned"`）も隠れた行には作用せず、確定件数は見える行の数だけ。
#[test]
fn partitioned_update_and_delete_skip_hidden_rows() {
    let (core, _g) = fixture("rls13-nosql-partitioned", true);
    let h = http(&core);
    let resp = q(
        &h,
        r#"{"op":"update","table":"docs","set":{"n":600},"filter":[{"column":"n","op":"gte","value":0}],"mode":"partitioned","operation_id":"p13-upd"}"#,
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    assert!(
        body_text(&resp).contains(&format!(r#""updated":{}"#, VISIBLE.len())),
        "{}",
        body_text(&resp)
    );
    assert_eq!(
        sql_sorted_ids(
            &core,
            TENANT_A,
            "SELECT id FROM docs WHERE n = 600 LIMIT 100"
        ),
        VISIBLE.to_vec()
    );

    let resp = q(
        &h,
        r#"{"op":"delete","table":"docs","filter":[{"column":"n","op":"gte","value":0}],"mode":"partitioned","operation_id":"p13-del"}"#,
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    assert!(
        body_text(&resp).contains(&format!(r#""deleted":{}"#, VISIBLE.len())),
        "{}",
        body_text(&resp)
    );
    sql_ok(
        &core,
        TENANT_A,
        "ALTER TABLE docs DISABLE ROW LEVEL SECURITY",
    );
    assert_eq!(alice_ids(&core), vec![2, 4]);
}

// ---------------------------------------------------------------------
// NoSQL からポリシー DDL へは届かない（A05）
// ---------------------------------------------------------------------

/// RLS-13: `create_policy` は語彙外（`0A000`・501）、`alter_table` の行セキュリティ用キーは閉じたスキーマで
/// `42601`（400）。NoSQL から利用者ポリシーを管理する手段はない。
#[test]
fn policy_ddl_is_unreachable_from_nosql() {
    let (core, _g) = fixture("rls13-nosql-ddl", false);
    let h = http(&core);
    let resp = q(
        &h,
        r#"{"op":"create_policy","table":"docs","name":"p","using":"true"}"#,
    );
    assert_eq!(resp.status, 501, "{}", body_text(&resp));
    assert_eq!(http_common::wire_code_of(&resp), "0A000");
    let resp = q(
        &h,
        r#"{"op":"alter_table","table":"docs","enable_row_level_security":true}"#,
    );
    assert_eq!(resp.status, 400, "{}", body_text(&resp));
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}
