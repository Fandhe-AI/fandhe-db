//! `insert`／`update`／`delete` の `returning` が engine の RETURNING 実行へ結線されていることを、
//! production ルータ経由（生バイトクライアント）で固定する層 A 結合テスト（Issue #2091。
//! 親 #2089。ポインタ: NOSQL-22・SQL-21・SQL-40・RLS-10・RECOVER-10）。
//!
//! ## 固定する内容
//!
//! - **パリティ**: 同じ seed の 2 つの core（双子）の片方へ HTTP、もう片方へ SQL
//!   `... RETURNING` を流し、SQL の `ReturningOutcome` を `encode_success_body_with_returning`
//!   に通した本文が HTTP 本文とバイト一致する（`"*"`／列配列 × insert・update・delete ×
//!   `where` 形・`filter` 述語形）。VECTOR 列を含む表で `"*"` がベクトル列を含む（SQL-21）
//! - 他テナント id・未存在 id の update／delete は `{"updated":0,...,"rows":[]}`（件数 0・0 行）
//! - 同じ `Idempotency-Key` での returning あり／なしの再送は、なし → なしの再送と同じ結果
//!   （内容照合ハッシュは `RETURNING` を材料にしない。RECOVER-10・11）

use crate::common;
use crate::http_common;
use crate::legacy_table;
use legacy_table::LegacyExec as _;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::{delete, insert, update};
use wire_server::http::session::store::SessionStore;

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `docs (embedding VECTOR(2), lang TEXT, n BIGINT)`。tenant-a に id 1〜3、tenant-b に id 1・9 を seed。
fn new_core(label: &str) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(
        &PolicyContext::new("sys").expect("sys ctx"),
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(2), lang TEXT, n BIGINT)",
    )
    .expect("create table");
    for (tenant, rows) in [
        (TENANT_A, vec![(1, "ja"), (2, "ja"), (3, "en")]),
        (TENANT_B, vec![(1, "ja"), (9, "ja")]),
    ] {
        for (id, lang) in rows {
            core.execute_sql_in_session_legacy(
                &ctx(tenant),
                &mut SessionState::default(),
                &format!(
                    "INSERT INTO docs (id, embedding, lang, n) VALUES ({id}, '[0.5,0.25]', '{lang}', {id}) \
                     USING OPERATION_ID 'seed-{tenant}-{id}'"
                ),
            )
            .expect("seed");
        }
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

#[derive(Clone, Copy)]
enum Kind {
    Insert,
    Update,
    Delete,
}

/// SQL を双子の core で実行し、HTTP と同じ本文へ組み立てる。
fn sql_body(core: &EngineCore, kind: Kind, sql: &str, op_id: &str) -> String {
    let outcome = core
        .execute_sql_in_session_legacy(&ctx(TENANT_A), &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let SqlOutcome::Returning(r) = outcome else {
        panic!("expected Returning outcome for {sql}");
    };
    let operation_id = Some(OperationId::parse(op_id).expect("op id"));
    match kind {
        Kind::Insert => insert::encode_success_body_with_returning(
            &insert::InsertSuccess {
                inserted: r.rows_affected,
                operation_id,
            },
            &r.result,
        ),
        Kind::Update => update::encode_success_body_with_returning(
            &update::UpdateSuccess {
                updated: r.rows_affected,
                operation_id,
            },
            &r.result,
        ),
        Kind::Delete => delete::encode_success_body_with_returning(
            &delete::DeleteSuccess {
                deleted: r.rows_affected,
                operation_id,
            },
            &r.result,
        ),
    }
    .expect("encode")
}

#[test]
fn http_returning_matches_sql_returning_byte_for_byte() {
    // (返却指定の JSON, SQL の返却列)
    let projections = [(r#""*""#, "*"), (r#"["id","lang","n"]"#, "id, lang, n")];
    for (pi, (json_ret, sql_ret)) in projections.into_iter().enumerate() {
        let (http_core, _g1) = new_core(&format!("nosql22-exec-http-{pi}"));
        let (sql_core, _g2) = new_core(&format!("nosql22-exec-sql-{pi}"));
        let c = spawn(http_core);

        // (種別, HTTP 本文, SQL 文, operation_id)
        let ins_http = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":10,"embedding":[0.5,0.25],"lang":"fr","n":10}},{{"id":11,"embedding":[0.5,0.25],"lang":"fr","n":11}}],"operation_id":"ins-1","returning":{json_ret}}}"#
        );
        let ins_sql = format!(
            "INSERT INTO docs (id, embedding, lang, n) VALUES (10, '[0.5,0.25]', 'fr', 10), \
             (11, '[0.5,0.25]', 'fr', 11) RETURNING {sql_ret} USING OPERATION_ID 'ins-1'"
        );
        let mut cases: Vec<(Kind, String, String, &str)> =
            vec![(Kind::Insert, ins_http, ins_sql, "ins-1")];
        for (i, id) in [1u64, 9, 77].into_iter().enumerate() {
            let op = ["upd-a", "upd-b", "upd-c"][i];
            cases.push((
                Kind::Update,
                format!(
                    r#"{{"op":"update","table":"docs","set":{{"lang":"en"}},"where":{{"id":{id}}},"operation_id":"{op}","returning":{json_ret}}}"#
                ),
                format!(
                    "UPDATE docs SET lang = 'en' WHERE id = {id} RETURNING {sql_ret} USING OPERATION_ID '{op}'"
                ),
                op,
            ));
        }
        cases.push((
            Kind::Update,
            format!(
                r#"{{"op":"update","table":"docs","set":{{"n":7}},"filter":[{{"column":"lang","op":"eq","value":"en"}}],"operation_id":"upd-f","returning":{json_ret}}}"#
            ),
            format!(
                "UPDATE docs SET n = 7 WHERE lang = 'en' RETURNING {sql_ret} USING OPERATION_ID 'upd-f'"
            ),
            "upd-f",
        ));
        for (i, id) in [2u64, 9, 77].into_iter().enumerate() {
            let op = ["del-a", "del-b", "del-c"][i];
            cases.push((
                Kind::Delete,
                format!(
                    r#"{{"op":"delete","table":"docs","where":{{"id":{id}}},"operation_id":"{op}","returning":{json_ret}}}"#
                ),
                format!(
                    "DELETE FROM docs WHERE id = {id} RETURNING {sql_ret} USING OPERATION_ID '{op}'"
                ),
                op,
            ));
        }
        cases.push((
            Kind::Delete,
            format!(
                r#"{{"op":"delete","table":"docs","filter":[{{"column":"lang","op":"eq","value":"fr"}}],"operation_id":"del-f","returning":{json_ret}}}"#
            ),
            format!(
                "DELETE FROM docs WHERE lang = 'fr' RETURNING {sql_ret} USING OPERATION_ID 'del-f'"
            ),
            "del-f",
        ));

        for (kind, http, sql, op) in cases {
            let resp = query(&c, &http, None);
            assert_eq!(resp.status, 200, "{http} {resp:?}");
            let want = sql_body(&sql_core, kind, &sql, op);
            assert_eq!(body_text(&resp), want, "{http}");
        }
    }
}

#[test]
fn star_includes_the_vector_column_and_foreign_ids_return_no_rows() {
    let (core, _g) = new_core("nosql22-exec-star");
    let c = spawn(core);
    let resp = query(
        &c,
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"returning":"*"}"#,
        None,
    );
    let text = body_text(&resp);
    assert!(text.starts_with(r#"{"updated":1,"columns":["#), "{text}");
    for col in ["embedding", "lang", "\"n\""] {
        assert!(text.contains(col), "missing {col}: {text}");
    }
    // tenant-b にだけ存在する id 9・未存在 id 77 は件数 0・0 行（存在の有無を区別しない）。
    for id in [9, 77] {
        for (op, key) in [("update", "updated"), ("delete", "deleted")] {
            let set = if op == "update" {
                r#""set":{"n":1},"#
            } else {
                ""
            };
            let body = format!(
                r#"{{"op":"{op}","table":"docs",{set}"where":{{"id":{id}}},"returning":["lang"]}}"#
            );
            let resp = query(&c, &body, None);
            let text = body_text(&resp);
            assert!(text.starts_with(&format!(r#"{{"{key}":0,"#)), "{text}");
            assert!(text.ends_with(r#""rows":[]}"#), "{text}");
            assert!(!text.contains("tenant-b"), "{text}");
        }
    }
}

#[test]
fn replay_with_and_without_returning_is_treated_alike() {
    // 内容照合ハッシュは `returning` を材料にしない。returning あり → なしの再送は、
    // なし → なしの再送と同じ応答コードに収束する。
    let upd = |ret: &str| {
        format!(
            r#"{{"op":"update","table":"docs","set":{{"lang":"en"}},"where":{{"id":1}},"operation_id":"rp-1"{ret}}}"#
        )
    };
    let (core_a, _g1) = new_core("nosql22-exec-replay-a");
    let (core_b, _g2) = new_core("nosql22-exec-replay-b");
    let a = spawn(core_a);
    let b = spawn(core_b);
    assert_eq!(query(&a, &upd(r#","returning":"*""#), None).status, 200);
    assert_eq!(query(&b, &upd(""), None).status, 200);
    let ra = query(&a, &upd(""), None);
    let rb = query(&b, &upd(""), None);
    assert_eq!(ra.status, rb.status, "{ra:?} {rb:?}");
    assert_eq!(
        http_common::wire_code_of(&ra),
        http_common::wire_code_of(&rb)
    );
    // 逆順（なし → あり）。
    let rc = query(&b, &upd(r#","returning":"*""#), None);
    assert_eq!(rc.status, rb.status, "{rc:?}");
}

/// 採番方式の表（PRIMARY KEY 基準の行アドレス）でも、`where` の PK アドレス形・`id` 省略の
/// `insert` の `returning` が SQL の `RETURNING` と同じ本文になる（TABLE-23 ポインタ）。
#[test]
fn allocated_identity_table_returning_matches_sql() {
    let mk = |label: &str| {
        let (core, g) = new_core(label);
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(
            &ctx(TENANT_A),
            &mut session,
            "CREATE TABLE kv (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (a, b))",
        )
        .expect("create kv");
        (core, g)
    };
    let (http_core, _g1) = mk("nosql22-exec-pk-http");
    let (sql_core, _g2) = mk("nosql22-exec-pk-sql");
    let c = spawn(http_core);

    let steps = [
        (
            Kind::Insert,
            r#"{"op":"insert","table":"kv","rows":[{"a":"k","b":1,"v":"one"},{"a":"k","b":2,"v":"two"}],"operation_id":"pk-ins","returning":["a","b","v"]}"#,
            "INSERT INTO kv (a, b, v) VALUES ('k', 1, 'one'), ('k', 2, 'two') \
             RETURNING a, b, v USING OPERATION_ID 'pk-ins'",
            "pk-ins",
        ),
        (
            Kind::Update,
            r#"{"op":"update","table":"kv","set":{"v":"x"},"where":{"a":"k","b":2},"operation_id":"pk-upd","returning":["a","b","v"]}"#,
            "UPDATE kv SET v = 'x' WHERE a = 'k' AND b = 2 RETURNING a, b, v \
             USING OPERATION_ID 'pk-upd'",
            "pk-upd",
        ),
        (
            Kind::Delete,
            r#"{"op":"delete","table":"kv","where":{"a":"k","b":1},"operation_id":"pk-del","returning":["a","b","v"]}"#,
            "DELETE FROM kv WHERE a = 'k' AND b = 1 RETURNING a, b, v USING OPERATION_ID 'pk-del'",
            "pk-del",
        ),
    ];
    for (kind, http, sql, op) in steps {
        let resp = query(&c, http, None);
        assert_eq!(resp.status, 200, "{http} {resp:?}");
        assert_eq!(
            body_text(&resp),
            sql_body(&sql_core, kind, sql, op),
            "{http}"
        );
    }
}
