//! NoSQL `search` の `highlight`（`ts_headline` 投影）が、RLS の可視範囲と列単位の権限を越えて
//! 本文を返さないことの越境マトリクス（Issue #3056。ポインタ: SEARCH-14・RLS-7・RLS-8・RLS-12・SQL-40）。
//!
//! engine 側 `tests/table_rls/rls10_ts_headline_paths.rs`（SQL の全経路）の NoSQL 版。評価は RLS の
//! 暗黙適用後の Top-k 行だけに走る（実行本体は SQL と同一）ことを、次の検査で固定する。
//!
//! - T0 陽性対照: 自テナントの可視行で強調とカナリアが返る
//! - T1 独立オラクル: 応答に現れるカナリア集合が、テスト側の真実値（可視行のみ）と一致する
//! - T2 flood 不変性: 他テナント・Private の行の有無で、応答が `Date` を除きバイト一致する
//! - T3 エラー応答が flood の有無で変わらず、カナリア・テナント名を含まない
//! - T4 データ依存カナリア: 上限を下げた core で、不可視行にだけ上限超過の本文を置いても成功し、
//!   陽性対照として可視行に置くと `54000`
//! - T5 列単位の権限: 列単位の付与しか無いロールは `42501`（固定文言・本文を含まない）、表単位の付与なら成功
//! - T6 負の対照と試行総数の固定

use crate::common;
use crate::http_common;

use std::sync::Arc;
use std::time::Instant;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::headline_limits::HeadlineLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::{SessionRole, SessionStore};

const TABLE: &str = "docs";

fn full_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn docs_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// 1 行分の種。`long` は本文を上限超過の長さにする（T4）。
struct Seed {
    tenant: &'static str,
    id: u64,
    vis: Visibility,
    canary: &'static str,
    long: bool,
}

fn body_of(seed: &Seed) -> String {
    let filler = if seed.long {
        " filler".repeat(40)
    } else {
        String::new()
    };
    format!("alpha {}{filler}", seed.canary)
}

fn insert(storage: &Storage, seed: &Seed) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &full_ctx(seed.tenant),
        seed.id,
        seed.vis,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text(body_of(seed))],
        &engine::recovery::required_op_id::OperationId::parse(&format!(
            "rls10hl-op-{}-{}",
            seed.tenant, seed.id
        ))
        .expect("valid operation_id"),
    )
    .expect("insert row");
}

/// 閲覧者 tenant-a に見える行。HTTP のログインが導出する `PolicyContext` は自テナントの `Private` も
/// 見せるため、可視集合は「全テナントの `Public` ＋ 自テナントの `Private`」（既存の `scan`／`search` と同じ）。
fn visible(first_long: bool) -> Vec<Seed> {
    vec![
        Seed {
            tenant: "tenant-a",
            id: 1,
            vis: Visibility::Public,
            canary: "zzcan_a_pub",
            long: first_long,
        },
        Seed {
            tenant: "tenant-a",
            id: 2,
            vis: Visibility::Private,
            canary: "zzcan_a_priv",
            long: false,
        },
        Seed {
            tenant: "tenant-b",
            id: 3,
            vis: Visibility::Public,
            canary: "zzcan_b_pub",
            long: false,
        },
    ]
}

/// 不可視の行（他テナントの `Private`）。
fn invisible(long: bool) -> Vec<Seed> {
    vec![
        Seed {
            tenant: "tenant-b",
            id: 4,
            vis: Visibility::Private,
            canary: "zzcan_b_priv",
            long,
        },
        Seed {
            tenant: "tenant-c",
            id: 5,
            vis: Visibility::Private,
            canary: "zzcan_c_priv",
            long,
        },
    ]
}

const ALL_CANARIES: [&str; 5] = [
    "zzcan_a_pub",
    "zzcan_a_priv",
    "zzcan_b_pub",
    "zzcan_b_priv",
    "zzcan_c_priv",
];
/// テスト側の真実値から導いた、閲覧者に見えるカナリア（production の可視性判定は呼ばない）。
const EXPECTED: [&str; 3] = ["zzcan_a_pub", "zzcan_a_priv", "zzcan_b_pub"];

struct World {
    addr: std::net::SocketAddr,
    _g: temp_db::CleanupGuard,
    _core: Arc<EngineCore>,
}

/// `flood` が真なら不可視の行を入れる。`visible_long` / `invisible_long` で本文を長くする。
fn world(
    label: &str,
    flood: bool,
    visible_long: bool,
    invisible_long: bool,
    limits: Option<HeadlineLimits>,
) -> World {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&docs_schema()).expect("create table");
    for seed in visible(visible_long) {
        insert(&storage, &seed);
    }
    if flood {
        for s in invisible(invisible_long) {
            insert(&storage, &s);
        }
    }
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_headline_limits(l);
    }
    let core = Arc::new(core);
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        Arc::clone(&core),
    );
    World {
        addr,
        _g: guard,
        _core: core,
    }
}

fn login(addr: std::net::SocketAddr) -> String {
    let body = r#"{"user":"alice","password":"pw-alice"}"#;
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
    match engine::json::parse_json(&String::from_utf8_lossy(&resp.body)).expect("json") {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

fn post(addr: std::net::SocketAddr, token: &str, body: &str) -> HttpResponse {
    let auth = format!("Bearer {token}");
    let len = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
            ("Content-Length", &len),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn query(w: &World, body: &str) -> HttpResponse {
    post(w.addr, &login(w.addr), body)
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

fn text(r: &HttpResponse) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

fn search(options: Option<&str>) -> String {
    let opts = options
        .map(|o| format!(r#","options":"{o}""#))
        .unwrap_or_default();
    format!(
        r#"{{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],
        "hybrid":{{"text":"alpha"}},"highlight":{{"column":"body"{opts}}}}}"#
    )
}

/// 応答に現れるカナリアの集合。
fn canaries_in(s: &str) -> Vec<&'static str> {
    ALL_CANARIES
        .iter()
        .copied()
        .filter(|c| s.contains(c))
        .collect()
}

/// T0・T1・T2・T3・T6。
#[test]
fn highlight_returns_only_visible_rows_and_is_flood_invariant() {
    let flooded = world("rls10hl-flood", true, false, false, None);
    let clean = world("rls10hl-clean", false, false, false, None);

    let mut attempts = 0usize;
    let mut positive = 0usize;
    let mut denied_or_error = 0usize;
    for options in [
        None,
        Some("StartSel=<i>, StopSel=</i>"),
        Some("HighlightAll=true"),
        Some("MaxWords=5, MinWords=2"),
    ] {
        let req = search(options);
        let a = query(&flooded, &req);
        let b = query(&clean, &req);
        attempts += 2;
        assert_eq!(a.status, 200, "options={options:?} resp={a:?}");
        // T0 陽性対照: 可視行の強調とカナリア。
        assert!(text(&a).contains("zzcan_a_pub"), "options={options:?}");
        positive += 1;
        // T1 独立オラクル: 真実値は可視行のカナリアだけ。
        assert_eq!(
            canaries_in(&text(&a)),
            EXPECTED.to_vec(),
            "options={options:?}"
        );
        // T2 flood 不変性。
        assert_eq!(norm(&a), norm(&b), "options={options:?}");
    }

    // T3 エラー応答は flood の有無で変わらず、カナリア・テナント名を含まない。
    for (bad, code) in [
        (Some("NoSuchOption=1"), "22023"),
        (Some("MaxWords=abc"), "22023"),
    ] {
        let req = search(bad);
        let a = query(&flooded, &req);
        let b = query(&clean, &req);
        attempts += 2;
        assert_eq!(http_common::wire_code_of(&a), code, "{a:?}");
        assert_eq!(norm(&a), norm(&b));
        let t = text(&a);
        assert!(canaries_in(&t).is_empty() && !t.contains("tenant-"), "{t}");
        denied_or_error += 1;
    }
    let shape = r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,
        "highlight":{"column":"body"}}"#;
    let a = query(&flooded, shape);
    let b = query(&clean, shape);
    attempts += 2;
    assert_eq!(http_common::wire_code_of(&a), "42601");
    assert_eq!(norm(&a), norm(&b));
    denied_or_error += 1;

    // T6 負の対照と試行総数。
    assert_eq!(positive, 4);
    assert_eq!(denied_or_error, 3);
    assert_eq!(attempts, 14);
}

/// T4: 上限を下げた core で、不可視行にだけ上限超過の本文を置いても結果は変わらず成功する。
/// 陽性対照として可視行を長くすると `54000`。NoSQL 側が CLI で下げた上限を使っていることも兼ねて固定する。
#[test]
fn highlight_limit_errors_depend_only_on_visible_rows() {
    let limits = Some(HeadlineLimits::new(64, 4));
    let req = search(None);

    let invisible_long = world("rls10hl-t4-inv", true, false, true, limits);
    let baseline = world("rls10hl-t4-base", false, false, false, limits);
    let a = query(&invisible_long, &req);
    let b = query(&baseline, &req);
    assert_eq!(a.status, 200, "resp={a:?}");
    assert_eq!(norm(&a), norm(&b));
    assert_eq!(canaries_in(&text(&a)), EXPECTED.to_vec());

    let visible_long = world("rls10hl-t4-vis", false, true, false, limits);
    let c = query(&visible_long, &req);
    assert_eq!(http_common::wire_code_of(&c), "54000", "resp={c:?}");
    assert!(canaries_in(&text(&c)).is_empty());
}

// --- T5 列単位の権限 ---

const ADMIN: u64 = 1;
const ROLES: [&str; 4] = ["r_none", "r_body", "r_cols", "r_table"];

fn operator(tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

fn bound(tenant: &str, id: u64) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(tenant));
    s.allow_ddl();
    s
}

fn setup(core: &EngineCore, sql: &str) {
    let mut s = if sql.starts_with("GRANT") {
        bound("tenant-a", ADMIN)
    } else {
        operator("tenant-a")
    };
    if let Err(e) = core.execute_sql_in_session(&full_ctx("tenant-a"), &mut s, sql) {
        panic!("setup: {sql}: {e}");
    }
}

fn role_world(label: &str) -> (World, Vec<(&'static str, String)>) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&docs_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    setup(&core, "CREATE ROLE adm");
    for r in ROLES {
        setup(&core, &format!("CREATE ROLE {r}"));
    }
    setup(
        &core,
        "INSERT INTO docs (id, embedding, body) VALUES (1, '[1.0, 0.0]', 'alpha zzcan_a_pub')",
    );
    setup(&core, "GRANT SELECT (body) ON docs TO r_body");
    setup(&core, "GRANT SELECT (embedding) ON docs TO r_cols");
    setup(&core, "GRANT SELECT (body) ON docs TO r_cols");
    setup(&core, "GRANT SELECT ON docs TO r_table");
    let core = Arc::new(core);
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let sessions = SessionStore::new();
    let mut tokens = Vec::new();
    for (i, r) in ROLES.iter().enumerate() {
        let t = sessions
            .issue_with_grant(
                full_ctx("tenant-a"),
                false,
                Some(SessionRole {
                    // 作成順（付与者 adm = 1、以降 2..）。
                    role_id: 2 + i as u64,
                    role_name: (*r).to_string(),
                    all_object_privileges: false,
                }),
                Instant::now(),
            )
            .expect("issue")
            .encoded()
            .to_string();
        tokens.push((*r, t));
    }
    let addr =
        http_common::spawn_router_listener_with_engine(&users_path, sessions, Arc::clone(&core));
    (
        World {
            addr,
            _g: guard,
            _core: core,
        },
        tokens,
    )
}

/// T5: `highlight` 付き `search` は、通常の `search` と同じ全列要求で権限判定される。
/// HTTP の bound 読み取りは列単位の付与だけでは満たせない（全実列への付与でも不可。rls12 と同じ）ため、
/// 列単位の付与しか無いロールは固定文言の `42501`（本文を含まない）、表単位の付与なら成功する。
#[test]
fn highlight_requires_full_column_privilege() {
    let (w, tokens) = role_world("rls10hl-t5");
    let req = search(None);
    let plain = r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],
        "hybrid":{"text":"alpha"}}"#;
    let mut ok = 0usize;
    let mut denied = 0usize;
    let mut denied_bodies = Vec::new();
    for (role, tok) in &tokens {
        let r = post(w.addr, tok, &req);
        let p = post(w.addr, tok, plain);
        // 権限の判定は highlight の有無で変わらない（highlight 経路で検査を迂回・強化しない）。
        assert_eq!(r.status, p.status, "{role}: {r:?} / {p:?}");
        let expect_ok = *role == "r_table";
        if expect_ok {
            assert_eq!(r.status, 200, "{role}: {r:?}");
            assert!(text(&r).contains("zzcan_a_pub"), "{role}");
            ok += 1;
        } else {
            assert_eq!(http_common::wire_code_of(&r), "42501", "{role}: {r:?}");
            assert!(!text(&r).contains("zzcan_a_pub"), "{role}");
            assert!(!text(&r).contains("alpha"), "{role}");
            denied_bodies.push(norm(&r));
            denied += 1;
        }
    }
    // 拒否応答はロールによらず同一（存在情報・付与状況を漏らさない）。
    assert!(denied_bodies.windows(2).all(|p| p[0] == p[1]));
    // T6 負の対照と試行総数。
    assert_eq!((ok, denied), (1, 3));
}
