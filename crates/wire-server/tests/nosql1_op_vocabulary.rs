//! `POST /v1/query` の op 語彙検証・拒否契約（Issue #774。対象ビヘイビア
//! TASK-179・NOSQL-1・NOSQL-9。ポインタ: `docs/spec/05-tasks.md` TASK-179・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1・NOSQL-9）の層 A テスト
//! 群。4 op（`search`／`scan`／`aggregate`／`insert`）はいずれも実行結線済み
//! （TASK-186・NOSQL-2〜6・Issue #764・#766・#768・#772）のため、本ファイルは
//! **seed 済み 2 テナント fixture** 上で以下を固定する。
//!
//! ## 既存テストとの役割分担
//!
//! - `nosql9_op_allowlist.rs`（Issue #759）: スローアウェイ core 上で 4 op の
//!   engine 到達（`42P01`）・語彙外 16 ケースの `0A000`・op 判定がスキーマ
//!   検証より先であること・未知キー 15 ケースの `42601`・`op` 欠落／非文字列／
//!   非オブジェクトの回帰・非漏えいを固定する。本ファイルはこれらを再検証
//!   しない
//! - `nosql1_endpoint_routing.rs`: NOSQL-1 の 3 エンドポイント限定・未知パス／
//!   非 `POST` の `08P01`・ルーティングと認証／本文検証の順序
//! - `nosql2_search.rs`・`nosql3_*`・`nosql4_*`・`nosql6_*`: 各 op の写像意味論
//!   （SQL パリティ・precision・`operation_id` 契約 等）
//!
//! 本ファイルが新たに固定するのは、(a) seed 済み fixture での 4 op **成功
//! 応答**（受理の非 vacuous 証跡）、(b) 語彙外 op の `0A000` が seed 済み・
//! 実行可能な状態でも不変で副作用を持たないこと、(c) RLS 段を狙う JSON 上の
//! 各フィールドが `42601` で拒否され、同じ本文からそのキーだけを外した対照が
//! 200 かつ RLS clean であること（拒否がキー起因である非 vacuous 証跡）、
//! (d) 拒否要求がセッションを消費・失効させないこと、の 4 点。
//!
//! ## RLS-5 の観測境界
//!
//! NoSQL 表層には `HINT ORDER` を受理する経路が存在しない（宣言していない
//! フィールドは [`crate::http::query::schema::ObjectSchema`] の未知キー検証
//! により `42601` で拒否される）ため、engine 側の安全網本体
//! （`crates/engine/src/rls.rs::RlsSafetyNet`・`sql/exec.rs` の無条件適用。
//! 対象ビヘイビア RLS-5・RLS-7・RLS-8）を wire 越しに単独で発火させることは
//! できない。本ファイルは「RLS 段を狙いうる JSON 上のすべてのつまみが
//! `42601` で拒否される、または受理されても不可視行が一切混入しない（RLS
//! clean）」ことのみを固定する。安全網そのものの確定オラクルは engine 側
//! （`crates/engine/tests/sql_evaluation_order.rs`・`rls.rs` の
//! `RlsSafetyNet` テスト）と SQL 表層 wire テスト（`tests/wire_hint_order.rs`）
//! に委ねる（`nosql3_scan_wire_parity.rs` の早期終了に関する書き方と同型）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
// `temp_db` は `http_common` が `pub mod temp_db;` として再エクスポートする
// ため、ここでは独自に `mod temp_db;` を宣言しない
// （`clippy::duplicate_mod` 回避。`http_common/mod.rs` のコメント参照）。
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::gate::UNSUPPORTED_OP_MESSAGE;
use wire_server::http::session::store::SessionStore;
use wire_server::limits::SESSION_TTL;

const TABLE: &str = "docs";

/// 距離 0 の他テナント（tenant-b）罠行の id。wire ログインが導出する
/// `PolicyContext`（`auth::verify`）は `Public` ＋ 自テナント `Private` のみを
/// 許可可視性とする（RLS-11・TASK-195・read-your-writes）ため、この id は
/// alice（tenant-a）のいずれの応答にも混入してはならない。
const OTHER_TENANT_TRAP_ID: u64 = 99;

/// 距離 0 の自テナント（tenant-a）罠行の id。RLS-11・TASK-195（read-your-writes）
/// により alice 自身のクエリでは legitimately 可視になりうる（漏えいではない）。
#[allow(dead_code)]
const OWN_TENANT_PRIVATE_ID: u64 = 100;

const TRAP_IDS: [u64; 1] = [OTHER_TENANT_TRAP_ID];

/// 罠行を一意に識別するセンチネル（`lang` 列。短い値は他の語の部分文字列と
/// 誤一致しうるため一意な値を使う）。他テナント（tenant-b）罠行のセンチネル。
/// alice の応答には絶対に現れてはならない。
const OTHER_TENANT_TRAP_SENTINEL: &str = "trap-b-99";

/// 自テナント（tenant-a）罠行のセンチネル。RLS-11・TASK-195（read-your-writes）
/// により `lang` 列を投影する alice のクエリには legitimately 現れうる
/// （漏えいではない）。
#[allow(dead_code)]
const OWN_TENANT_PRIVATE_SENTINEL: &str = "trap-a-100";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// `docs`（`embedding VECTOR(2)` + `lang TEXT`）へ tenant-a の Public 行 3 件
/// と、クエリベクトル `[1.0,0.0]` に距離 0 で一致する Private 罠行 2 件
/// （他テナント tenant-b の id=99・自テナント tenant-a の id=100）を仕込む
/// （`tests/wire_hint_order.rs::new_core_two_tenant_docs` と同型の構成）。
fn new_core_two_tenant_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql1-op-vocabulary-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let public_rows: [(u64, [f32; 2], &str); 3] = [
        (1, [0.9, 0.1], "ja"),
        (2, [0.0, 1.0], "ja"),
        (3, [0.1, 0.9], "en"),
    ];
    for (id, emb, lang) in public_rows {
        let op_id = OperationId::parse(&format!("nosql1-seed-a-{id}")).expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert tenant-a public row");
    }

    // tenant-b の Private 罠行（他テナント・距離 0）。
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_b,
        99,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("trap-b-99".to_string()),
        ],
        &OperationId::parse("nosql1-seed-trap-b-99").expect("valid operation_id"),
    )
    .expect("insert tenant-b trap row");

    // tenant-a 自身の Private 罠行（wire ctx には許可可視性として付与されて
    // いないため、自テナントであっても見えてはならない。距離 0）。
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
        100,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("trap-a-100".to_string()),
        ],
        &OperationId::parse("nosql1-seed-trap-a-100").expect("valid operation_id"),
    )
    .expect("insert tenant-a private trap row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `alice`（tenant-a）のみを持つユーザーストアを添えて production ルータを
/// 起動する（1 `#[test]` = 1 個の新規 core・リスナーの流儀。`nosql2_search.rs`
/// と同じ）。
fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core)
}

/// [`spawn`] と同じだが、セッションストアの同時枠を `max_sessions` 個に
/// 制限する（T4: 拒否要求がセッションを消費しないことを固定するため、枠 1
/// でログインを使い回す必要がある）。
fn spawn_with_session_limit(core: Arc<EngineCore>, max_sessions: usize) -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::with_limits(max_sessions, SESSION_TTL),
        core,
    )
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
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn post(addr: SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
    let auth_header = format!("Bearer {token}");
    let content_length = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_header),
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// テナントの Bearer トークンを都度発行し `/v1/query` へ本文を送る便宜 API
/// （セッション枠を使い切らないよう毎回新規ログインする。`nosql9_op_allowlist.rs::
/// query` と同型）。
fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

/// `{"columns":[{"name":...}],"rows":[[...]],"row_count":n}` 形の成功応答を
/// `(columns, rows)` へ解析する。応答が別の形（成功本文の形が違う op、
/// エラー応答等）であれば `None` を返す（`assert_rls_clean` が best-effort に
/// 使うための緩い解析。`nosql3_scan_wire_parity.rs::parse_body_str` の
/// 部分集合）。
fn parse_columns_rows(text: &str) -> Option<(Vec<String>, Vec<Vec<JsonValue>>)> {
    let JsonValue::Object(top) = parse_json(text).ok()? else {
        return None;
    };
    let columns = match top.get("columns") {
        Some(JsonValue::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                JsonValue::Object(col) => match col.get("name") {
                    Some(JsonValue::String(s)) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    let rows = match top.get("rows") {
        Some(JsonValue::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                JsonValue::Array(cells) => Some(cells.clone()),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    Some((columns, rows))
}

/// 応答が RLS clean であることを検証する: (1) 本文にいずれの罠センチネルも
/// 現れない、(2) `tenant-` マーカーが現れない、(3) 200 応答かつ `columns` に
/// `id` があれば、いずれの行の `id` も [`TRAP_IDS`]（他テナントの罠行 id）を
/// 含まない（`search`／`scan` の行集合応答を解析ベースで検査する。
/// `aggregate`／`insert` の成功応答には `id` 列が無いため (3) は無害に
/// skip される）。自テナント（tenant-a）の罠行 id=100・センチネルは
/// RLS-11・TASK-195（read-your-writes）により legitimately 現れうるため
/// ここでは検査しない。
fn assert_rls_clean(resp: &HttpResponse) {
    let text = body_utf8(resp);
    assert!(
        !text.contains(OTHER_TENANT_TRAP_SENTINEL),
        "response leaked other tenant's trap sentinel {OTHER_TENANT_TRAP_SENTINEL:?}: {text}"
    );
    assert!(
        !text.contains("tenant-"),
        "response leaked tenant id marker: {text}"
    );
    if resp.status == 200 {
        if let Some((columns, rows)) = parse_columns_rows(&text) {
            if let Some(id_idx) = columns.iter().position(|c| c == "id") {
                for row in &rows {
                    if let Some(JsonValue::Number(n)) = row.get(id_idx) {
                        let id = n.as_f64() as u64;
                        assert!(
                            !TRAP_IDS.contains(&id),
                            "trap id {id} leaked in response: {text}"
                        );
                    }
                }
            }
        }
    }
}

// --- T1: 4 op が受理され成功応答を返す（受入 1） --------------------------

#[test]
fn all_four_ops_succeed_against_seeded_fixture_with_distinct_bodies() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    // `columns` に `scan_resp`（`["id"]` のみ）と異なる列集合 `["id","lang"]`
    // を指定する。scan は順序保証のない契約（Issue #831 レビュー指摘）のため、
    // 行順序だけに頼った `assert_ne!` は「scan がたまたま search と同じ順序を
    // 返す」正当な実装でも失敗しうる。列集合そのものを変えることで、行順序に
    // 依存せず本文が構造的に異なることを保証する。
    let search_resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id","lang"]}"#,
    );
    assert_eq!(search_resp.status, 200, "search resp={search_resp:?}");
    // 可視行は Public 3 件 + 自テナント（tenant-a）Private 罠行 1 件（id=100。
    // RLS-11・TASK-195・read-your-writes）の計 4 件。他テナント罠行（id=99）は
    // `assert_rls_clean` が非混入を固定する。
    assert!(
        body_utf8(&search_resp).contains(r#""row_count":4"#),
        "{}",
        body_utf8(&search_resp)
    );
    assert_rls_clean(&search_resp);

    let scan_resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(scan_resp.status, 200, "scan resp={scan_resp:?}");
    assert!(
        body_utf8(&scan_resp).contains(r#""row_count":4"#),
        "{}",
        body_utf8(&scan_resp)
    );
    assert!(
        !body_utf8(&scan_resp).contains(r#""score""#),
        "scan projection must not include score: {}",
        body_utf8(&scan_resp)
    );
    assert_rls_clean(&scan_resp);

    let aggregate_resp = query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
    );
    assert_eq!(
        aggregate_resp.status, 200,
        "aggregate resp={aggregate_resp:?}"
    );
    // 可視行（Public 3 件 + 自テナント Private 罠行 1 件）を数え、他テナント
    // 罠行（id=99）は含まない（RLS-11・TASK-195）。
    assert!(
        body_utf8(&aggregate_resp).contains("[4]"),
        "{}",
        body_utf8(&aggregate_resp)
    );
    assert_rls_clean(&aggregate_resp);

    let insert_resp = query_as_alice(
        addr,
        br#"{"op":"insert","table":"docs","rows":[{"id":500,"embedding":[0.2,0.3]}],"operation_id":"nosql1-t1-insert"}"#,
    );
    assert_eq!(insert_resp.status, 200, "insert resp={insert_resp:?}");
    assert_eq!(
        body_utf8(&insert_resp),
        r#"{"inserted":1,"operation_id":"nosql1-t1-insert"}"#
    );
    assert_rls_clean(&insert_resp);

    // 4 応答本文が互いに異なることで固定応答への縮退を排除する。
    let bodies = [
        body_utf8(&search_resp),
        body_utf8(&scan_resp),
        body_utf8(&aggregate_resp),
        body_utf8(&insert_resp),
    ];
    for i in 0..bodies.len() {
        for j in (i + 1)..bodies.len() {
            assert_ne!(
                bodies[i], bodies[j],
                "response bodies must differ: {bodies:?}"
            );
        }
    }

    // insert の永続化を確認する。NoSQL 表層の `insert` は SQL-10
    // `execute_insert` と同じく常に `Visibility::Private` 固定で書き込み、
    // wire 認証が導出する `PolicyContext`（`Public` ＋ 自テナント `Private`。
    // RLS-11・TASK-195）は同一テナントが書いた `Private` 行を読み戻せる
    // （read-your-writes）ため、alice 自身の直後の wire scan で id=500 が
    // 可視になる。永続化自体も、保持している `Arc<EngineCore>` から
    // `Private` を含む ctx で直接 `SELECT` して二重に確認する。
    let scan_after_insert = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(scan_after_insert.status, 200, "resp={scan_after_insert:?}");
    assert!(
        body_utf8(&scan_after_insert).contains(r#""row_count":5"#),
        "wire scan (Public 3 + 自テナント Private 2 件: id=100, id=500) must be 5: {}",
        body_utf8(&scan_after_insert)
    );
    assert!(
        body_utf8(&scan_after_insert).contains("[500]"),
        "wire scan must observe the newly-inserted own-tenant Private row (id=500) via read-your-writes: {}",
        body_utf8(&scan_after_insert)
    );
    assert_rls_clean(&scan_after_insert);

    let ctx_a_with_private =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-a ctx");
    let mut session = engine::sql::mode::SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx_a_with_private,
            &mut session,
            "SELECT id FROM docs LIMIT 10",
        )
        .expect("select ok");
    let engine::sql::SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    assert!(
        result.rows.iter().any(|row| row.id == 500),
        "inserted row id=500 must be persisted (Private) and visible with Private ctx: {:?}",
        result.rows
    );
}

// --- T2: 語彙外 op は seed 済み・実行可能状態でも 0A000 で副作用なし（受入 2） ---

#[test]
fn vocabulary_outside_six_ops_rejects_with_0a000_and_has_no_side_effect() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    // `nosql9_op_allowlist.rs` と同じ語彙外集合（`update`／`delete` は
    // Issue #875 で語彙へ加わったため対象外。その 2 op の `filter: []`
    // （空配列）指定での「実行可能な状態でも副作用なし」は下の
    // `update_and_delete_empty_filter_form_rejects_with_42601_and_has_no_side_effect`
    // が固定する）。
    // 受理形（`vector`／`limit` 等）の残りフィールドを備えた本文で送り、
    // 「op 判定が実行可能な状態でも手前で止まる」ことを固定する。
    let unsupported_ops = [
        // `create_table`／`alter_table`／`drop_table` は Issue #910 で
        // 語彙へ加わったため対象から外す（`nosql13_ddl.rs` が固定する）。
        // NOSQL-13 の対象外である index／view 系 DDL 相当を代わりに使う。
        "create_index",
        "drop_index",
        "create_view",
        "drop_view",
        "call",
        "udf",
        "begin",
        "commit",
        "rollback",
        "select",
        "explain",
        "set",
        "SEARCH",
        " search",
        "",
    ];
    for op in unsupported_ops {
        let body = format!(r#"{{"op":"{op}","table":"docs","vector":[1.0,0.0],"limit":10}}"#);
        let resp = query_as_alice(addr, body.as_bytes());
        assert_eq!(resp.status, 501, "op={op:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "0A000", "op={op:?}");
        assert_eq!(http_common::error_code_of(&resp), "FEATURE_NOT_SUPPORTED");
        assert_eq!(
            http_common::error_message_of(&resp),
            UNSUPPORTED_OP_MESSAGE,
            "op={op:?}"
        );
        if !op.is_empty() {
            http_common::assert_message_does_not_echo(&resp, op);
        }
    }

    // 全拒否後も行数は不変（`drop_table` 相当が何も変更していないことを
    // 非 vacuous に確認する）。
    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""row_count":4"#),
        "{}",
        body_utf8(&resp)
    );
}

// --- update／delete: filter: []（空配列）は 42601 で副作用なし（Issue #875・#876・#1062） ---

#[test]
fn update_and_delete_empty_filter_form_rejects_with_42601_and_has_no_side_effect() {
    // `update`／`delete`（Issue #875・NOSQL-12）の `filter: []`（空配列）は
    // SQL 表層の `WHERE` 句省略とのパリティで `42601`（Issue #1062）。
    // engine を一切呼ばないため、seed 済み・実行可能な状態でも行データへの
    // 副作用は生じない。
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let bodies: [&[u8]; 2] = [
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[]}"#,
        br#"{"op":"delete","table":"docs","filter":[]}"#,
    ];
    for body in bodies {
        let resp = query_as_alice(addr, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""row_count":4"#),
        "{}",
        body_utf8(&resp)
    );
}

// --- update／delete: filter（述語形。非空）は実行結線済み（Issue #1062） -----

#[test]
fn update_and_delete_non_empty_filter_form_executes_and_affects_only_own_tenant_rows() {
    // `update`／`delete`（Issue #1062）の `filter`（述語形・非空）は
    // 語彙・スキーマ検証を通過した後 engine まで到達し、自テナント
    // （alice/tenant-a）が所有する行のみを対象に実行する
    // （`new_core_two_tenant_docs` は tenant-a に `lang: "ja"` を 2 行・
    // `lang: "en"` を 1 行、tenant-b に `lang: "trap-b-99"` の罠行を 1 行
    // 持つ。罠行は `eq` にどちらの値でも一致せず、対象テナントの計数にも
    // 現れない）。
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let resp = query_as_alice(
        addr,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"n1-pred-update-1"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""updated":2"#),
        "{}",
        body_utf8(&resp)
    );

    // tenant-a の可視行は 3 件すべて `lang: "en"` になった状態のため、
    // 述語形 `delete` は 3 件を対象とする（tenant-b の罠行は変更しない）。
    let resp = query_as_alice(
        addr,
        br#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"eq","value":"en"}],"operation_id":"n1-pred-delete-1"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""deleted":3"#),
        "{}",
        body_utf8(&resp)
    );
}

// --- update／delete: where（単一行 id 指定形）は実行結線済み（Issue #876） ---

#[test]
fn update_where_form_executes_and_updates_the_targeted_row() {
    // `update`（`where` 形。Issue #876）は seed 済み・実行可能な状態では
    // 自テナント（alice/tenant-a）が所有する行 id=1 を実際に更新する。
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let resp = query_as_alice(
        addr,
        br#"{"op":"update","table":"docs","set":{"lang":"fr"},"where":{"id":1},"operation_id":"nosql1-update-1"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""updated":1"#),
        "{}",
        body_utf8(&resp)
    );

    // 行数は不変（更新であり削除ではない）。
    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""row_count":4"#),
        "{}",
        body_utf8(&resp)
    );
}

#[test]
fn delete_where_form_executes_and_removes_the_targeted_row() {
    // `delete`（`where` 形。Issue #876）は seed 済み・実行可能な状態では
    // 自テナント（alice/tenant-a）が所有する行 id=1 を実際に削除する。
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let resp = query_as_alice(
        addr,
        br#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":"nosql1-delete-1"}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""deleted":1"#),
        "{}",
        body_utf8(&resp)
    );

    // 行数が 1 件減る。
    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""row_count":3"#),
        "{}",
        body_utf8(&resp)
    );
}

#[test]
fn update_where_form_without_operation_id_rejects_with_23502_and_no_side_effect() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let resp = query_as_alice(
        addr,
        br#"{"op":"update","table":"docs","set":{"lang":"fr"},"where":{"id":1}}"#,
    );
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "23502");

    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""row_count":4"#),
        "{}",
        body_utf8(&resp)
    );
}

// --- T3: RLS 段を狙うフィールドは 42601、最小差分対照は 200 かつ RLS clean（受入 3・4） ---

/// `(label, 拒否本文, 対照本文)`。対照本文は拒否本文から当該キーだけを外した
/// もの（拒否が「そのキーの存在」に起因することを非 vacuous に固定する）。
fn rls_targeting_cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // --- トップレベル hint_order（4 op） ---
        (
            "search/hint_order",
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],"hint_order":["path"]}"#,
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/hint_order",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"hint_order":["path"]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/hint_order",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"hint_order":["path"]}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- search_mode（4 op） ---
        (
            "search/search_mode",
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],"search_mode":"precision"}"#,
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/search_mode",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"search_mode":"precision"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/search_mode",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"search_mode":"precision"}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- mode（scan／aggregate。search は既知キーのため search_mode で代表） ---
        (
            "scan/mode",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"mode":"precision"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/mode",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"mode":"precision"}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- set/session/transaction（scan で代表） ---
        (
            "scan/set",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"set":"x"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/session",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"session":"x"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/transaction",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"transaction":"begin"}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        // --- filter 要素の column が RLS 述語名（search/scan/aggregate） ---
        (
            "search/filter-visible",
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],"filter":[{"column":"visible","op":"eq","value":"x"}]}"#,
            r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
        ),
        (
            "scan/filter-visible-call",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"visible()","op":"eq","value":"x"}]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
        ),
        (
            "aggregate/filter-VISIBLE",
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],"filter":[{"column":"VISIBLE","op":"eq","value":"x"}]}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        ),
        // --- filter 要素内の未知キー ---
        (
            "scan/filter-unknown-key",
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"lang","op":"eq","value":"ja","hint_order":["path"]}]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        ),
    ]
}

#[test]
fn rls_targeting_fields_reject_with_42601_and_minimal_diff_contrast_succeeds() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    for (label, rejected_body, contrast_body) in rls_targeting_cases() {
        let rejected = query_as_alice(addr, rejected_body.as_bytes());
        assert_eq!(
            rejected.status, 400,
            "label={label} body={rejected_body} resp={rejected:?}"
        );
        assert_eq!(
            http_common::wire_code_of(&rejected),
            "42601",
            "label={label}"
        );
        let message = http_common::error_message_of(&rejected);
        assert!(
            !message.contains("tenant-a") && !message.contains("tenant-b"),
            "label={label} message must not leak tenant id: {message:?}"
        );

        let contrast = query_as_alice(addr, contrast_body.as_bytes());
        assert_eq!(
            contrast.status, 200,
            "label={label} contrast body={contrast_body} resp={contrast:?} (拒否が \
             キー以外の要因に起因している可能性がある)"
        );
        assert_rls_clean(&contrast);
    }
}

/// `insert` は行を実際に書くため専用ケースとして扱う。拒否本文と対照本文で
/// **同じ `operation_id`** を使うことで、「スキーマ拒否された要求は
/// `operation_id` 台帳に触れていない」ことを非 vacuous に固定する
/// （対照が `23505`／`22023` にならず素直に成功することで判定する）。
#[test]
fn insert_rls_targeting_fields_reject_and_shared_operation_id_contrast_succeeds() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let cases: [(&str, u64, &str); 4] = [
        ("insert/hint_order", 501, "nosql1-insert-hint-order"),
        ("insert/search_mode", 502, "nosql1-insert-search-mode"),
        ("insert/mode", 503, "nosql1-insert-mode"),
        (
            "insert/filter-not-applicable",
            504,
            "nosql1-insert-transaction",
        ),
    ];

    for (label, id, op_id) in cases {
        let rejected_body = match label {
            "insert/hint_order" => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","hint_order":["path"]}}"#
            ),
            "insert/search_mode" => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","search_mode":"precision"}}"#
            ),
            "insert/mode" => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","mode":"precision"}}"#
            ),
            _ => format!(
                r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}","transaction":"begin"}}"#
            ),
        };
        let rejected = query_as_alice(addr, rejected_body.as_bytes());
        assert_eq!(
            rejected.status, 400,
            "label={label} body={rejected_body} resp={rejected:?}"
        );
        assert_eq!(
            http_common::wire_code_of(&rejected),
            "42601",
            "label={label}"
        );

        // 対照: 同じ operation_id・同じ id でキーだけ外す。拒否がスキーマ
        // 検証段で止まり台帳に触れていなければ 200（`23505`／`22023` になら
        // ない）。
        let contrast_body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":{id},"embedding":[0.1,0.2]}}],"operation_id":"{op_id}"}}"#
        );
        let contrast = query_as_alice(addr, contrast_body.as_bytes());
        assert_eq!(
            contrast.status, 200,
            "label={label} contrast body={contrast_body} resp={contrast:?} (拒否された \
             要求が operation_id 台帳へ書き込んでいた可能性がある)"
        );
        assert_eq!(
            body_utf8(&contrast),
            format!(r#"{{"inserted":1,"operation_id":"{op_id}"}}"#)
        );
    }
}

/// RLS clean の強化（`scan` は順序保証を持たず・`aggregate` は件数のみの
/// ため）: `filter` を罠行だけに一致する条件にすると `search`／`scan` は
/// `row_count: 0`、`aggregate` の count は 0 になる（順序に依存せず RLS を
/// 単独で切り分ける）。
#[test]
fn filter_matching_only_trap_rows_returns_empty_visible_set() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let search_resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"],
            "filter":[{"column":"lang","op":"eq","value":"trap-b-99"}]}"#,
    );
    assert_eq!(search_resp.status, 200, "resp={search_resp:?}");
    assert!(
        body_utf8(&search_resp).contains(r#""row_count":0"#),
        "{}",
        body_utf8(&search_resp)
    );
    assert_rls_clean(&search_resp);

    let scan_resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"],
            "filter":[{"column":"lang","op":"eq","value":"trap-b-99"}]}"#,
    );
    assert_eq!(scan_resp.status, 200, "resp={scan_resp:?}");
    assert!(
        body_utf8(&scan_resp).contains(r#""row_count":0"#),
        "{}",
        body_utf8(&scan_resp)
    );
    assert_rls_clean(&scan_resp);

    let aggregate_resp = query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],
            "filter":[{"column":"lang","op":"eq","value":"trap-b-99"}]}"#,
    );
    assert_eq!(aggregate_resp.status, 200, "resp={aggregate_resp:?}");
    assert!(
        body_utf8(&aggregate_resp).contains("[0]"),
        "{}",
        body_utf8(&aggregate_resp)
    );
    assert_rls_clean(&aggregate_resp);
}

// --- T4: 拒否要求はセッションを消費・失効させない ---------------------------

#[test]
fn rejected_requests_do_not_consume_or_expire_the_session() {
    let (core, _guard) = new_core_two_tenant_docs();
    // 枠 1 でログインを 1 回だけ行い、以降はトークンを使い回す（拒否要求が
    // 新たな枠を確保しないこと・既存トークンを失効させないことを固定する）。
    let addr = spawn_with_session_limit(Arc::clone(&core), 1);
    let token = login(addr, "alice", "pw-alice");

    let hint_order_rejected = post(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"hint_order":["path"]}"#,
    );
    assert_eq!(
        hint_order_rejected.status, 400,
        "resp={hint_order_rejected:?}"
    );
    assert_eq!(http_common::wire_code_of(&hint_order_rejected), "42601");

    let unsupported_op_rejected = post(
        addr,
        &token,
        // `drop_table` は Issue #910 で語彙へ加わったため、引き続き語彙外の
        // `drop_index`（NOSQL-13 の対象外）を使う。
        br#"{"op":"drop_index","table":"docs","vector":[1.0,0.0],"limit":10}"#,
    );
    assert_eq!(
        unsupported_op_rejected.status, 501,
        "resp={unsupported_op_rejected:?}"
    );
    assert_eq!(http_common::wire_code_of(&unsupported_op_rejected), "0A000");

    // 同一トークンで最後に送った対照が 200・行数不変・RLS clean であれば、
    // 上記 2 件の拒否がトークンを失効させず、枠を新たに確保もしなかった
    // ことの非 vacuous な証跡になる（枠 1 のまま再ログインせずに成功する）。
    let contrast = post(
        addr,
        &token,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    );
    assert_eq!(contrast.status, 200, "resp={contrast:?}");
    assert!(
        body_utf8(&contrast).contains(r#""row_count":4"#),
        "{}",
        body_utf8(&contrast)
    );
    assert_rls_clean(&contrast);
}

// --- T5: 非漏えい ------------------------------------------------------------

#[test]
fn success_and_rejection_bodies_never_leak_tenant_credentials_or_trap_sentinels() {
    let (core, _guard) = new_core_two_tenant_docs();
    let addr = spawn(Arc::clone(&core));

    let mut bodies: Vec<String> = Vec::new();

    // T1 相当の成功応答。
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"]}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"insert","table":"docs","rows":[{"id":600,"embedding":[0.4,0.5]}],"operation_id":"nosql1-t5-insert"}"#,
    )));

    // T2 相当の語彙外拒否・T3 相当の RLS 段拒否。
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"begin","table":"docs","vector":[1.0,0.0],"limit":10}"#,
    )));
    bodies.push(body_utf8(&query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"hint_order":["path"]}"#,
    )));

    for body in bodies {
        assert!(!body.contains("tenant-a"), "leaked tenant-a: {body}");
        assert!(!body.contains("tenant-b"), "leaked tenant-b: {body}");
        assert!(!body.contains("pw-alice"), "leaked password: {body}");
        // 自テナント（tenant-a）の罠センチネルは RLS-11・TASK-195
        // （read-your-writes）により legitimately 現れうるため検査しない
        // （他テナントのセンチネルのみ絶対禁止）。
        assert!(
            !body.contains(OTHER_TENANT_TRAP_SENTINEL),
            "leaked other tenant's sentinel {OTHER_TENANT_TRAP_SENTINEL}: {body}"
        );
    }
}
