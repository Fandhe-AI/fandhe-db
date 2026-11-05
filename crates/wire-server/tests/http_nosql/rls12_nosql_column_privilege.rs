//! NoSQL（HTTP）表層の列単位の権限制御が、SQL 表層と食い違わず何も漏らさないことの行列テスト
//! （Issue #2084。ポインタ: RLS-12・SQL-40。ADR `docs/design/column-level-privilege.md` の C4・C5・C6 と §8・§9）。
//!
//! engine 側の `tests/table_rls/rls12_column_privilege_paths.rs` が SQL の全経路を固定する。本ファイルは
//! HTTP の `scan`／`search`／`aggregate`（と各 `explain`）を、同じロールを束縛した engine の SQL と突き合わせる。
//!
//! HTTP の bound 読み取りは入口で列を特定できず、列で満たせるビットを全列への要求として判定する
//! （ADR §8「PG より厳しい箇所」）。このため関係は等号ではなく一方向になる。
//!
//! - HTTP が成功するなら、同じ要求を表す SQL も成功し、行数が一致する（HTTP の成功集合は SQL の部分集合）
//! - HTTP の `42501` の本文は、SQL の `42501` の `client_message` と同じ固定文言を持つ
//!
//! 検査は engine 側と同じ枠組み。T0 非空虚性（表単位の付与で全 op が成功し、拒否も 1 件以上）、T1 独立オラクル
//! （期待の真理値表。production の判定関数は呼ばない）、T2 不変性（拒否応答は `Date` を除きバイト一致、他テナントの
//! 行・付与は応答を変えない）、T3 explain は拒否時に計画を返さない、T6 負の対照、試行総数の固定。
//! ロール ID は作成順（付与者 `adm` = 1、`r<i>` = `2 + i`、他テナントの `adm` = 9、`r<i>` = `10 + i`）。

use crate::common;
use crate::http_common;

use std::sync::Arc;
use std::time::Instant;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::{SessionRole, SessionStore};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const REAL_COLS: [&str; 4] = ["embedding", "pub_c", "secret", "n"];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gs {
    None,
    PubCol,
    PubEmb,
    AllReal,
    Table,
    TableThenRevoked,
}

const STATES: [Gs; 6] = [
    Gs::None,
    Gs::PubCol,
    Gs::PubEmb,
    Gs::AllReal,
    Gs::Table,
    Gs::TableThenRevoked,
];

impl Gs {
    fn idx(self) -> u64 {
        STATES.iter().position(|s| *s == self).expect("state") as u64
    }
    fn table_level(self) -> bool {
        self == Gs::Table
    }
    /// 実効の列単位の付与（表単位の `REVOKE` は列単位の付与も取り消す）。
    fn cols(self) -> &'static [&'static str] {
        match self {
            Gs::None | Gs::Table | Gs::TableThenRevoked => &[],
            Gs::PubCol => &["pub_c"],
            Gs::PubEmb => &["pub_c", "embedding"],
            Gs::AllReal => &REAL_COLS,
        }
    }
}

fn role_id(s: Gs) -> u64 {
    2 + s.idx()
}

const ADMIN: u64 = 1;
const FOREIGN_ADMIN: u64 = 2 + STATES.len() as u64;

fn operator(tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

fn bound(tenant: &str, id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(tenant));
    if ddl {
        s.allow_ddl();
    }
    s
}

fn setup(core: &EngineCore, tenant: &str, admin: u64, sql: &str) {
    let mut s = if sql.starts_with("GRANT") || sql.starts_with("REVOKE") {
        bound(tenant, admin, true)
    } else {
        operator(tenant)
    };
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), &mut s, sql) {
        panic!("setup {tenant}: {sql}: {e}");
    }
}

fn token(tenant: &str, table: &str, i: u64) -> String {
    format!("zzs_{tenant}_{table}_{i}")
}

fn seed_rows(core: &EngineCore, tenant: &str) {
    for i in 1..=2u64 {
        setup(
            core,
            tenant,
            ADMIN,
            &format!(
                "INSERT INTO docs (id, embedding, pub_c, secret, n) VALUES ({i}, '[1.0, 0.0]', 'pubtok_{i}', '{}', {i})",
                token(tenant, "docs", i)
            ),
        );
        setup(
            core,
            tenant,
            ADMIN,
            &format!(
                "INSERT INTO docs_a (embedding, pub_c, secret, n) VALUES ('[1.0, 0.0]', 'pubtok_{i}', '{}', {i})",
                token(tenant, "docs_a", i)
            ),
        );
    }
}

fn apply_grants(core: &EngineCore, tenant: &str, admin: u64, role: &str, s: Gs) {
    for t in ["docs", "docs_a"] {
        let g = |sql: String| setup(core, tenant, admin, &sql);
        match s {
            Gs::None => {}
            Gs::Table => g(format!("GRANT SELECT ON {t} TO {role}")),
            Gs::TableThenRevoked => {
                g(format!("GRANT SELECT ON {t} TO {role}"));
                g(format!("GRANT SELECT (pub_c) ON {t} TO {role}"));
                g(format!("REVOKE SELECT ON {t} FROM {role}"));
            }
            _ => {
                for c in s.cols() {
                    g(format!("GRANT SELECT ({c}) ON {t} TO {role}"));
                }
            }
        }
    }
}

struct World {
    core: Arc<EngineCore>,
    _g: temp_db::CleanupGuard,
    tenant: &'static str,
    addr: std::net::SocketAddr,
    /// 状態ごとの Bearer トークン。
    tokens: Vec<(Gs, String)>,
}

fn world(label: &str, viewer_idx: usize, flood: bool, foreign_grant: bool) -> World {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("pub_c", ColumnType::Text, false),
                ColumnDef::new("secret", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::BigInt, false),
            ],
        ))
        .expect("docs");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let tenant = TENANTS[viewer_idx];
    let foreign = TENANTS[(viewer_idx + 1) % 3];
    for t in [tenant, foreign] {
        setup(&core, t, ADMIN, "CREATE ROLE adm");
        for s in STATES {
            setup(&core, t, ADMIN, &format!("CREATE ROLE r{}", s.idx()));
        }
    }
    setup(
        &core,
        tenant,
        ADMIN,
        "CREATE TABLE docs_a (embedding vector(2), pub_c TEXT, secret TEXT, n BIGINT)",
    );
    seed_rows(&core, tenant);
    for s in STATES {
        apply_grants(&core, tenant, ADMIN, &format!("r{}", s.idx()), s);
    }
    if flood {
        seed_rows(&core, foreign);
        if foreign_grant {
            for s in STATES {
                for t in ["docs", "docs_a"] {
                    setup(
                        &core,
                        foreign,
                        FOREIGN_ADMIN,
                        &format!("GRANT SELECT ON {t} TO r{}", s.idx()),
                    );
                }
            }
        }
    }
    let core = Arc::new(core);
    let users_path = common::write_user_store_file(&[("alice", tenant, "pw-alice")]);
    let sessions = SessionStore::new();
    let mut tokens = Vec::new();
    for s in STATES {
        let t = sessions
            .issue_with_grant(
                ctx(tenant),
                false,
                Some(SessionRole {
                    role_id: role_id(s),
                    role_name: format!("r{}", s.idx()),
                    all_object_privileges: false,
                }),
                Instant::now(),
            )
            .expect("issue")
            .encoded()
            .to_string();
        tokens.push((s, t));
    }
    let addr = http_common::spawn_router_listener_with_engine(&users_path, sessions, core.clone());
    World {
        core,
        _g: guard,
        tenant: TENANTS[viewer_idx],
        addr,
        tokens,
    }
}

fn query(w: &World, g: Gs, body: &str) -> HttpResponse {
    let tok = &w.tokens.iter().find(|(s, _)| *s == g).expect("token").1;
    let auth = format!("Bearer {tok}");
    let len = body.len().to_string();
    let headers: Vec<(&str, &str)> = vec![
        ("Authorization", &auth),
        ("Content-Type", "application/json"),
        ("Content-Length", &len),
    ];
    let request = http_common::build_request("/v1/query", &headers, body.as_bytes());
    http_common::parse_single_response(&http_common::send_raw(
        w.addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// 応答全体（`Date` を除く状態・ヘッダ・本文）の比較用の形。
fn norm(r: &HttpResponse) -> String {
    let mut h: Vec<String> = r
        .headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("date"))
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    h.sort();
    format!(
        "{} {}\n{}\n{}",
        r.status,
        r.reason,
        h.join("\n"),
        String::from_utf8_lossy(&r.body)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Need {
    Named(&'static [&'static str]),
    All,
}

struct Op {
    name: &'static str,
    /// `{t}` は表名。
    json: &'static str,
    kind: &'static str,
    /// 同じ要求を表す SQL。
    sql: &'static str,
    /// SQL の要求（オラクル）。
    sql_need: Need,
}

const OPS: &[Op] = &[
    Op {
        name: "scan_cols_pub",
        json: r#"{"op":"scan","table":"{t}","limit":10,"columns":["pub_c"]}"#,
        kind: "scan",
        sql: "SELECT pub_c FROM {t} LIMIT 10",
        sql_need: Need::Named(&["pub_c"]),
    },
    Op {
        name: "scan_all",
        json: r#"{"op":"scan","table":"{t}","limit":10}"#,
        kind: "scan",
        sql: "SELECT * FROM {t} LIMIT 10",
        sql_need: Need::All,
    },
    Op {
        name: "scan_filter_secret",
        json: r#"{"op":"scan","table":"{t}","limit":10,"columns":["pub_c"],"filter":[{"column":"secret","op":"eq","value":"zzx"}]}"#,
        kind: "scan",
        sql: "SELECT pub_c FROM {t} WHERE secret = 'zzx' LIMIT 10",
        sql_need: Need::Named(&["pub_c", "secret"]),
    },
    Op {
        name: "scan_sort_secret",
        json: r#"{"op":"scan","table":"{t}","limit":10,"columns":["pub_c"],"sort":[{"column":"secret","dir":"asc"}]}"#,
        kind: "scan",
        sql: "SELECT pub_c FROM {t} ORDER BY secret LIMIT 10",
        sql_need: Need::Named(&["pub_c", "secret"]),
    },
    Op {
        name: "search_cols_pub",
        json: r#"{"op":"search","table":"{t}","vector":[1.0,0.0],"limit":3,"columns":["pub_c"]}"#,
        kind: "search",
        sql: "SELECT pub_c FROM {t} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3",
        sql_need: Need::Named(&["pub_c", "embedding"]),
    },
    Op {
        name: "search_filter_pub",
        json: r#"{"op":"search","table":"{t}","vector":[1.0,0.0],"limit":3,"columns":["pub_c"],"filter":[{"column":"pub_c","op":"eq","value":"pubtok_1"}]}"#,
        kind: "search",
        sql: "SELECT pub_c FROM {t} WHERE pub_c = 'pubtok_1' ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3",
        sql_need: Need::Named(&["pub_c", "embedding"]),
    },
    Op {
        name: "aggregate_count",
        json: r#"{"op":"aggregate","table":"{t}","aggregates":[{"fn":"count","column":"*"}]}"#,
        kind: "aggregate",
        sql: "SELECT count(*) FROM {t}",
        // `count(*)` は SQL では「いずれかの列」。HTTP は全列（一方向の関係）。
        sql_need: Need::Named(&[]),
    },
    Op {
        name: "aggregate_group_secret",
        json: r#"{"op":"aggregate","table":"{t}","aggregates":[{"fn":"count","column":"*"}],"group_by":"secret"}"#,
        kind: "aggregate",
        sql: "SELECT secret, count(*) FROM {t} GROUP BY secret",
        sql_need: Need::Named(&["secret"]),
    },
    Op {
        name: "aggregate_max_secret",
        json: r#"{"op":"aggregate","table":"{t}","aggregates":[{"fn":"max","column":"secret"}]}"#,
        kind: "aggregate",
        sql: "SELECT max(secret) FROM {t}",
        sql_need: Need::Named(&["secret"]),
    },
];

fn body_of(op: &Op, t: &str, explain: bool) -> String {
    let j = op.json.replace("{t}", t);
    if explain {
        // 末尾の `}` の前に `"explain":true` を足す。
        format!("{},\"explain\":true}}", &j[..j.len() - 1])
    } else {
        j
    }
}

fn sql_of(op: &Op, t: &str, explain: bool) -> String {
    let s = op.sql.replace("{t}", t);
    if explain {
        format!("EXPLAIN {s}")
    } else {
        s
    }
}

// ----- 独立オラクル -----

/// HTTP の期待。`Ok` は 200、それ以外は wire_code。
fn http_expect(table: &str, op: &Op, _explain: bool, g: Gs) -> &'static str {
    // 採番方式の表の `search`／`aggregate` は権限検査より前に固定文言の `0A000`。
    if table == "docs_a" && op.kind != "scan" {
        return "0A000";
    }
    if g.table_level() {
        return "ok";
    }
    // 採番方式の表の `scan` だけは、全実列への列単位の付与でも満たせる（全列要求のため）。
    if table == "docs_a" && op.kind == "scan" && REAL_COLS.iter().all(|c| g.cols().contains(c)) {
        return "ok";
    }
    "42501"
}

/// SQL の期待（同じ要求を表す SQL。EXPLAIN は全列）。
fn sql_ok(table: &str, op: &Op, explain: bool, g: Gs) -> bool {
    if g.table_level() {
        return true;
    }
    let need = if explain { Need::All } else { op.sql_need };
    match need {
        Need::All => table == "docs_a" && REAL_COLS.iter().all(|c| g.cols().contains(c)),
        Need::Named([]) => !g.cols().is_empty(),
        Need::Named(cols) => cols.iter().all(|c| g.cols().contains(c)),
    }
}

// ----- 検出器 -----

fn body_leak(
    body: &str,
    viewer: &str,
    foreign: &str,
    secret_granted: bool,
) -> Option<&'static str> {
    if body.contains(&format!("zzs_{foreign}_")) {
        return Some("foreign tenant token");
    }
    if !secret_granted && body.contains(&format!("zzs_{viewer}_")) {
        return Some("ungranted secret token");
    }
    None
}

fn echo_violation(body: &str) -> Option<String> {
    const BAD: &[&str] = &[
        "pub_c",
        "secret",
        "embedding",
        "docs",
        "docs_a",
        "column",
        "QUERY",
        "PLAN",
        "tenant-a",
        "tenant-b",
        "tenant-c",
    ];
    for b in BAD {
        if body.contains(b) {
            return Some(format!("forbidden marker {b:?}"));
        }
    }
    for tok in body.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if tok.len() == 2 && tok.starts_with('r') && tok[1..].parse::<u8>().is_ok() {
            return Some("role name".into());
        }
    }
    None
}

fn row_count(r: &HttpResponse) -> Option<usize> {
    let text = String::from_utf8_lossy(&r.body).into_owned();
    match engine::json::parse_json(&text).ok()? {
        engine::json::JsonValue::Object(o) => match o.get("rows") {
            Some(engine::json::JsonValue::Array(a)) => Some(a.len()),
            _ => None,
        },
        _ => None,
    }
}

fn sql_run(w: &World, g: Gs, sql: &str) -> Result<usize, (String, String)> {
    let mut s = bound(w.tenant, role_id(g), false);
    match w.core.execute_sql_in_session(&ctx(w.tenant), &mut s, sql) {
        Ok(SqlOutcome::Query(q)) => Ok(q.rows.len()),
        Ok(_) => Ok(0),
        Err(e) => Err((e.wire_code().to_string(), e.client_message())),
    }
}

type Key = (Gs, &'static str, &'static str, bool);

fn run_matrix(w: &World) -> std::collections::BTreeMap<String, (Key, HttpResponse)> {
    let mut m = std::collections::BTreeMap::new();
    for g in STATES {
        for t in ["docs", "docs_a"] {
            for op in OPS {
                for explain in [false, true] {
                    let r = query(w, g, &body_of(op, t, explain));
                    let key = (g, t, op.name, explain);
                    m.insert(format!("{key:?}"), (key, r));
                }
            }
        }
    }
    m
}

fn op_by_name(name: &str) -> &'static Op {
    OPS.iter().find(|o| o.name == name).expect("op")
}

#[test]
fn http_column_privilege_matrix_agrees_with_sql() {
    let mut attempts = 0usize;
    let mut ok_cells = 0usize;
    let mut denied_cells = 0usize;
    let mut denial_bodies: std::collections::BTreeSet<String> = Default::default();
    let mut failures: Vec<String> = Vec::new();
    for viewer_idx in 0..3 {
        let foreign = TENANTS[(viewer_idx + 1) % 3];
        let base = world("colpriv-http-base", viewer_idx, false, false);
        let flood = world("colpriv-http-flood", viewer_idx, true, true);
        let m_base = run_matrix(&base);
        let m_flood = run_matrix(&flood);
        for (k, (key, r)) in &m_base {
            attempts += 2;
            let (g, t, name, explain) = *key;
            let op = op_by_name(name);
            let (_, r2) = &m_flood[k];
            // T2: 他テナントの行・同名表・付与は閲覧テナントの応答を変えない。
            if norm(r) != norm(r2) {
                failures.push(format!("world differs at {k}"));
            }
            for resp in [r, r2] {
                let want = http_expect(t, op, explain, g);
                let got = if resp.status == 200 {
                    "ok".to_string()
                } else {
                    http_common::wire_code_of(resp)
                };
                if got != want {
                    failures.push(format!("http oracle: {k}: want {want} got {got}"));
                    continue;
                }
                let body = String::from_utf8_lossy(&resp.body).into_owned();
                let secret_granted = g.table_level() || g.cols().contains(&"secret");
                if let Some(l) = body_leak(&body, base.tenant, foreign, secret_granted) {
                    failures.push(format!("leak ({l}) at {k}"));
                }
                if got == "42501" {
                    if let Some(v) = echo_violation(&body) {
                        failures.push(format!("denial echo ({v}) at {k}"));
                    }
                }
            }
            // 一方向のパリティ。
            let sql = sql_run(&base, g, &sql_of(op, t, explain));
            match (&sql, sql_ok(t, op, explain, g)) {
                (Ok(_), true) => {}
                (Err((c, _)), false) if c == "42501" => {}
                other => failures.push(format!("sql oracle: {k}: {other:?}")),
            }
            if r.status == 200 {
                ok_cells += 1;
                if sql.is_err() {
                    failures.push(format!("parity broken (http ok, sql denied) at {k}"));
                }
                if !explain {
                    if let (Some(n), Ok(m)) = (row_count(r), &sql) {
                        if n != *m && op.kind == "scan" {
                            failures.push(format!("row count differs at {k}: http {n} sql {m}"));
                        }
                    }
                }
            } else if http_common::wire_code_of(r) == "42501" {
                denied_cells += 1;
                denial_bodies.insert(norm(r));
                // 固定文言は SQL の `client_message` と一致する（SQL 側が拒否のときに限らず、HTTP の拒否本文が
                // SQL の固定文言を載せる）。
                if let Err((_, msg)) = sql_run(&base, Gs::None, "SELECT pub_c FROM docs LIMIT 1") {
                    let body = String::from_utf8_lossy(&r.body);
                    if !body.contains(&msg) {
                        failures.push(format!("denial text differs from SQL at {k}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "failures: {failures:#?}");
    // T2: 拒否応答は op・付与状態・表によらず `Date` を除きバイト一致する。
    assert_eq!(
        denial_bodies.len(),
        1,
        "42501 responses must be identical, got {} variants",
        denial_bodies.len()
    );
    // T0: 成功と拒否の両方が実際に発生する。
    assert!(ok_cells > 0 && denied_cells > 0);
    assert_eq!(attempts, EXPECTED_ATTEMPTS, "matrix size drifted");
}

/// 試行総数（閲覧テナント 3 × 世界 2 × 付与状態 6 × 表 2 × op 9 × explain 2）。
const EXPECTED_ATTEMPTS: usize = 3 * 2 * 6 * 2 * 9 * 2;

// ----- T6 負の対照 -----

#[test]
fn detectors_flag_fabricated_violations() {
    assert!(body_leak("zzs_tenant-b_docs_1", "tenant-a", "tenant-b", true).is_some());
    assert!(body_leak("zzs_tenant-a_docs_1", "tenant-a", "tenant-b", false).is_some());
    assert!(body_leak("zzs_tenant-a_docs_1", "tenant-a", "tenant-b", true).is_none());
    for bad in [
        "permission denied for column secret",
        "permission denied for table docs",
        "QUERY PLAN",
        "role r3 denied",
        "tenant-b",
    ] {
        assert!(echo_violation(bad).is_some(), "{bad}");
    }
    assert!(echo_violation("permission denied").is_none());
    // オラクル。
    let scan_all = op_by_name("scan_all");
    assert_eq!(http_expect("docs", scan_all, false, Gs::PubCol), "42501");
    assert_eq!(http_expect("docs", scan_all, false, Gs::Table), "ok");
    assert_eq!(http_expect("docs_a", scan_all, false, Gs::AllReal), "ok");
    assert_eq!(
        http_expect("docs_a", op_by_name("aggregate_count"), false, Gs::None),
        "0A000"
    );
    assert!(sql_ok(
        "docs",
        op_by_name("scan_cols_pub"),
        false,
        Gs::PubCol
    ));
    assert!(!sql_ok(
        "docs",
        op_by_name("scan_cols_pub"),
        true,
        Gs::PubCol
    ));
    // 応答の比較子は `Date` だけを無視する。
    let a = HttpResponse {
        status: 403,
        reason: "Forbidden".into(),
        headers: vec![("Date".into(), "x".into())],
        body: b"a".to_vec(),
    };
    let mut b = HttpResponse {
        status: 403,
        reason: "Forbidden".into(),
        headers: Vec::new(),
        body: b"a".to_vec(),
    };
    b.headers = vec![("Date".into(), "y".into())];
    assert_eq!(norm(&a), norm(&b));
    b.body = b"b".to_vec();
    assert_ne!(norm(&a), norm(&b));
}
