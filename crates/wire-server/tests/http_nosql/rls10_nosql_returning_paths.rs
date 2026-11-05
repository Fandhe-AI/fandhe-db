//! NoSQL（HTTP）表層の `returning` の越境 0 件を、全形状 × 閲覧テナント 3 つ × 可視性 2 モードで
//! 機械的に固定する層 A 結合テスト（Issue #2091。ポインタ: NOSQL-22・RLS-7〜RLS-11・SQL-21。
//! 流儀は engine の `rls10_write_constraint_paths.rs` と同じ）。
//!
//! 双子 DB を使う。baseline は閲覧テナントの行だけ、flooded は他 2 テナントの Public／Private 行を
//! 同じ id・同じ `lang` で衝突させて追加する。検査:
//!
//! - T0 陽性対照: 影響行あり・返却行ありのセルが実際に発生する
//! - T1 独立オラクル: 期待件数・期待返却 id がテスト側の定数と一致し、本文に他テナントの
//!   トークンが現れない（production の可視性判定はオラクルに使わない）
//! - T2 baseline と flooded の本文が（`Date` を除き）バイト一致する
//! - T3 flooded の他テナント行が事前と事後で物理的に一致する
//! - T5 試行総数が固定（空回り防止）
//! - T6 捏造した漏えいを検出器が検出できる（負の対照）

use crate::common;
use crate::http_common;

use std::sync::Arc;
use std::time::Instant;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
/// 総セル数（閲覧テナント 3 × モード 2 × 形状 5。空回り防止）。
const CELL_COUNT: usize = 30;

/// `(id, public, lang)`。全テナント共通の id・lang（越境を実効的に検出するため衝突させる）。
const ROWS: [(u64, bool, &str); 4] = [
    (1, false, "ja"),
    (2, false, "ja"),
    (3, true, "ja"),
    (4, false, "en"),
];

fn token(tenant: &str, id: u64) -> String {
    format!("tok-{tenant}-{id}")
}

fn full_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_db(label: &str, tenants: &[&str]) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    for tenant in tenants {
        for (id, public, lang) in ROWS {
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &full_ctx(tenant),
                id,
                if public {
                    Visibility::Public
                } else {
                    Visibility::Private
                },
                &[
                    Value::Vector(vec![0.5, 0.25]),
                    Value::Text(lang.to_string()),
                    Value::Text(token(tenant, id)),
                ],
                &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id"),
            )
            .expect("seed");
        }
    }
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

/// 他テナントの行の物理スナップショット（RLS に依存しない読み取り: そのテナント自身の文脈で全件）。
fn physical(core: &EngineCore, tenants: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for t in tenants {
        let r = core
            .execute_sql_in_session(
                &full_ctx(t),
                &mut SessionState::default(),
                "SELECT id, lang, body FROM docs LIMIT 1000",
            )
            .expect("select");
        let SqlOutcome::Query(q) = r else {
            panic!("expected Query")
        };
        let mut rows: Vec<String> = q
            .rows
            .iter()
            .map(|r| format!("{t}:{}:{:?}", r.id, r.cells))
            // 他テナントの Public 行も `SELECT` には現れるため、自テナントのトークンを持つ行
            // （＝そのテナント所有の行）だけを物理状態として扱う。所有行が書き換え・削除されると
            // トークンが失われ、集合から消えて差分になる。
            .filter(|row| row.contains(&format!("\"tok-{t}-")))
            .collect();
        rows.sort();
        out.extend(rows);
    }
    out
}

struct Client {
    addr: std::net::SocketAddr,
    token: String,
}

fn spawn(core: Arc<EngineCore>, viewer: &str, private: bool) -> Client {
    let users_path = common::write_user_store_file(&[("alice", viewer, "pw-alice")]);
    let sessions = SessionStore::new();
    let ctx = if private {
        full_ctx(viewer)
    } else {
        PolicyContext::new(viewer).expect("valid tenant")
    };
    let token = sessions
        .issue(ctx, Instant::now())
        .expect("issue")
        .encoded()
        .to_string();
    let addr = http_common::spawn_router_listener_with_engine(&users_path, sessions, core);
    Client { addr, token }
}

fn query(c: &Client, body: &str) -> HttpResponse {
    let auth = format!("Bearer {}", c.token);
    let len = body.len().to_string();
    let headers: Vec<(&str, &str)> = vec![
        ("Authorization", &auth),
        ("Content-Type", "application/json"),
        ("Content-Length", &len),
    ];
    let request = http_common::build_request("/v1/query", &headers, body.as_bytes());
    http_common::parse_single_response(&http_common::send_raw(
        c.addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// 返却 `rows` の件数（JSON を構造的に読む）。
fn returned_rows(resp: &HttpResponse) -> usize {
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let JsonValue::Object(obj) = parse_json(&text).expect("json") else {
        panic!("object expected: {text}");
    };
    match obj.get("rows") {
        Some(JsonValue::Array(a)) => a.len(),
        other => panic!("rows array expected: {other:?} in {text}"),
    }
}

/// 本文に他テナントのトークンが現れるか（検出器。T6 で負の対照を取る）。
fn leaks(body: &str, viewer: &str) -> bool {
    TENANTS
        .iter()
        .filter(|t| **t != viewer)
        .any(|t| body.contains(&format!("tok-{t}-")))
}

/// 形状: (名前, 本文, 期待する影響行数〔private モード, public のみモード〕, 返却本文に自テナントのトークンが残るか)。
fn shapes() -> Vec<(&'static str, String, (usize, usize), bool)> {
    vec![
        (
            "update-where",
            r#"{"op":"update","table":"docs","set":{"body":"upd1"},"where":{"id":1},"returning":"*"}"#
                .to_string(),
            (1, 0),
            false,
        ),
        (
            "update-filter",
            r#"{"op":"update","table":"docs","set":{"body":"upd2"},"filter":[{"column":"lang","op":"eq","value":"ja"}],"returning":["id","lang","body"]}"#
                .to_string(),
            // lang=ja の自テナント行は id 1,2（private）と 3（public）。
            (3, 1),
            false,
        ),
        (
            "delete-where",
            r#"{"op":"delete","table":"docs","where":{"id":2},"returning":"*"}"#.to_string(),
            (1, 0),
            false,
        ),
        (
            "delete-filter",
            r#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"eq","value":"en"}],"returning":"*"}"#
                .to_string(),
            (1, 0),
            true,
        ),
        (
            "insert",
            r#"{"op":"insert","table":"docs","rows":[{"id":20,"embedding":[0.5,0.25],"lang":"fr","body":"new"}],"returning":"*"}"#
                .to_string(),
            // 挿入行は Private 固定。Public のみの主体は書き込み前に拒否される（500・0 行）。
            (1, 0),
            false,
        ),
    ]
}

#[test]
fn returning_never_crosses_tenant_boundaries() {
    let mut trials = 0usize;
    let mut positive_controls = 0usize;
    for viewer in TENANTS {
        let others: Vec<&str> = TENANTS.iter().copied().filter(|t| *t != viewer).collect();
        for private in [true, false] {
            let (base, _g1) = new_db(&format!("rls10-ret-base-{viewer}-{private}"), &[viewer]);
            let (flood, _g2) = new_db(&format!("rls10-ret-flood-{viewer}-{private}"), &TENANTS);
            let other_before = physical(&flood, &others);
            let cb = spawn(base, viewer, private);
            let cf = spawn(flood.clone(), viewer, private);

            for (name, body, (n_private, n_public), has_token) in shapes() {
                let expected = if private { n_private } else { n_public };
                let rb = query(&cb, &body);
                let rf = query(&cf, &body);
                trials += 1;

                // T2: baseline と flooded で本文がバイト一致する（`Date` は本文外）。
                assert_eq!(rb.status, rf.status, "{viewer} {private} {name}");
                assert_eq!(rb.body, rf.body, "{viewer} {private} {name}: body differs");

                let text = String::from_utf8_lossy(&rf.body).into_owned();
                // T1: 他テナントのトークンが現れない。
                assert!(!leaks(&text, viewer), "{viewer} {private} {name}: {text}");

                if name == "insert" && !private {
                    // Public のみの主体の INSERT ... RETURNING は書き込み前に fail-closed。
                    assert_eq!(rf.status, 500, "{viewer} {name}: {text}");
                    continue;
                }
                assert_eq!(rf.status, 200, "{viewer} {private} {name}: {text}");
                // T1: 独立オラクルの件数と、返却行数・件数キーが一致する。
                assert_eq!(returned_rows(&rf), expected, "{viewer} {private} {name}");
                let key = match &body[7..11] {
                    "upda" => "updated",
                    "dele" => "deleted",
                    _ => "inserted",
                };
                assert!(
                    text.starts_with(&format!(r#"{{"{key}":{expected},"#)),
                    "{viewer} {private} {name}: {text}"
                );
                if has_token && expected > 0 {
                    assert!(
                        text.contains(&format!("tok-{viewer}-")),
                        "{viewer} {private} {name}: {text}"
                    );
                }
                if expected > 0 {
                    positive_controls += 1;
                }
            }
            // T3: 他テナントの行は事前と事後で物理的に一致する。
            assert_eq!(
                physical(&flood, &others),
                other_before,
                "{viewer} {private}: foreign rows changed"
            );
        }
    }
    // T5: 試行総数が固定。T0: 陽性対照が実際に発生している。
    assert_eq!(trials, CELL_COUNT);
    assert!(positive_controls >= CELL_COUNT / 2, "{positive_controls}");
}

/// T6: 捏造した漏えい（他テナントのトークンを含む本文）を検出器が検出できる。
#[test]
fn leak_detector_detects_fabricated_leak() {
    assert!(leaks(
        r#"{"deleted":1,"rows":[["tok-tenant-b-1"]]}"#,
        "tenant-a"
    ));
    assert!(!leaks(
        r#"{"deleted":1,"rows":[["tok-tenant-a-1"]]}"#,
        "tenant-a"
    ));
}
