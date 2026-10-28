//! `--require-operation-id` の有無（既定＝任意／指定＝必須）× `USING OPERATION_ID` の
//! 省略／指定の行列回帰を、PostgreSQL wire v3（簡易クエリ・拡張クエリ・COPY）越しに固定する
//! 結合テスト（Issue #1705・親 #1701・RECOVER-1 ポインタ。設計: `docs/design/
//! operation-id-optional.md`）。
//!
//! - M1: 簡易クエリの行列（INSERT／UPDATE／DELETE／TRUNCATE／分割実行 DML）
//! - M2: 拡張クエリ（Parse/Bind/Execute）の省略・指定
//! - M3: COPY FROM STDIN の省略・指定
//! - M4: wire 越しの `SET`／`RESET` ではモードが変わらない（両方向）
//! - M5: StartupMessage の追加パラメータ（`options`・素のキー）ではモードが変わらない
//! - M6: HTTP のクエリ文字列ではモードが変わらない
//! - M7: HTTP（NoSQL）書き込み op の省略・指定・分割実行・show／cancel（Issue #2203）
//!
//! モードは CLI の解決関数（`require_operation_id_opt::resolve`）から決め、「フラグ → モード →
//! 挙動」を 1 本でつなぐ。バイナリ起動の代表経路は `wire_require_operation_id_cli.rs`（R4/R5）、
//! engine 層の行列は engine の `tests/recovery_ledger_mode_matrix.rs`。HTTP の書き込み op
//! 省略時の挙動は M7 で固定する（#2203）。

use crate::common;
use crate::http_common;

use http_common::temp_db;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// フラグ軸。`false` が既定（任意）、`true` が `--require-operation-id`（必須）。
const FLAGS: [bool; 2] = [false, true];

struct Fixture {
    addr: SocketAddr,
    core: Arc<EngineCore>,
    _db: temp_db::CleanupGuard,
    _users: UserStoreFile,
}

fn fixture(label: &str, require: bool) -> Fixture {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(wire_server::require_operation_id_opt::resolve(require)),
    );
    let users = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users, Arc::clone(&core));
    Fixture {
        addr,
        core,
        _db: guard,
        _users: users,
    }
}

fn connect(f: &Fixture) -> TcpStream {
    authenticate_to_ready_for_query(f.addr, "alice", "pw-alice")
}

/// 簡易クエリを送り、成功（CommandComplete → ReadyForQuery）を確認する。
fn q_ok(s: &mut TcpStream, sql: &str) {
    send_simple_query(s, sql);
    let _ = read_command_complete(s);
    read_ready_for_query(s);
}

/// 簡易クエリを送り、期待 SQLSTATE の ErrorResponse（→ ReadyForQuery）を確認する。
fn q_err(s: &mut TcpStream, sql: &str, sqlstate: &str) {
    send_simple_query(s, sql);
    expect_error_response_with_sqlstate(s, sqlstate);
    read_ready_for_query(s);
}

/// 省略形の書き込み文。seed 行（id=1）が存在する前提。
const OMITTED: [&str; 5] = [
    "INSERT INTO docs (id, embedding, lang) VALUES (9, '[1.0,0.0]', 'ja')",
    "UPDATE docs SET lang = 'x' WHERE id = 1",
    "UPDATE docs SET lang = 'x' WHERE lang = 'ja'",
    "DELETE FROM docs WHERE id = 1",
    "TRUNCATE TABLE docs",
];

const SEED: &str = "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') \
                    USING OPERATION_ID 'seed'";

fn count_rows(f: &Fixture) -> usize {
    let ctx = engine::policy::PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("tenant");
    f.core
        .execute_sql(&ctx, "SELECT id FROM docs LIMIT 1000")
        .expect("scan")
        .rows
        .len()
}

// ---------------------------------------------------------------------------
// M1: 簡易クエリ
// ---------------------------------------------------------------------------

#[test]
fn m1_simple_query_matrix_follows_flag() {
    for require in FLAGS {
        for sql in OMITTED {
            let f = fixture("wire-mx-m1", require);
            let mut s = connect(&f);
            q_ok(&mut s, SEED);
            if require {
                q_err(&mut s, sql, "23502");
                assert_eq!(count_rows(&f), 1, "require={require}: {sql}");
            } else {
                q_ok(&mut s, sql);
            }
        }
    }
}

#[test]
fn m1_specified_operation_id_contract_is_identical_in_both_modes() {
    for require in FLAGS {
        let f = fixture("wire-mx-m1-spec", require);
        let mut s = connect(&f);
        q_ok(&mut s, SEED);
        q_err(&mut s, SEED, "23505");
        q_err(
            &mut s,
            "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'other') \
             USING OPERATION_ID 'seed'",
            "22023",
        );
        q_ok(
            &mut s,
            "UPDATE docs SET lang = 'x' WHERE id = 1 USING OPERATION_ID 'u1'",
        );
        q_ok(
            &mut s,
            "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'd1'",
        );
    }
}

#[test]
fn m1_partitioned_dml_requires_operation_id_in_both_modes() {
    for require in FLAGS {
        let f = fixture("wire-mx-m1-part", require);
        let mut s = connect(&f);
        q_ok(&mut s, SEED);
        for sql in [
            "DELETE FROM docs WHERE lang = 'ja' PARTITIONED",
            "DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID NULL PARTITIONED",
            "UPDATE docs SET lang = 'x' WHERE lang = 'ja' PARTITIONED",
        ] {
            q_err(&mut s, sql, "23502");
        }
        assert_eq!(count_rows(&f), 1, "require={require}");
    }
}

// ---------------------------------------------------------------------------
// M2: 拡張クエリ
// ---------------------------------------------------------------------------

/// Bind メッセージ本体（テキスト形式の文字列パラメータ 0 個以上）を組み立てる。
fn bind_text_params(portal: &str, statement: &str, params: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for p in params {
        body.extend_from_slice(&(p.len() as i32).to_be_bytes());
        body.extend_from_slice(p.as_bytes());
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

/// Parse/Bind/Execute/Sync を送り、ReadyForQuery までの応答から ErrorResponse の
/// SQLSTATE（成功なら `None`）を返す。
fn extended(s: &mut TcpStream, sql: &str, params: &[&str]) -> Option<String> {
    send_length_prefixed_message(s, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(s, b'B', &bind_text_params("", "", params));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    let mut sqlstate = None;
    loop {
        let (t, body) = read_message(s);
        match t {
            b'E' => {
                // フィールド列: 型 1 バイト + NUL 終端文字列。`C` が SQLSTATE。
                let mut i = 0;
                while i < body.len() && body[i] != 0 {
                    let kind = body[i];
                    let end = body[i + 1..]
                        .iter()
                        .position(|b| *b == 0)
                        .map_or(body.len(), |p| i + 1 + p);
                    if kind == b'C' {
                        sqlstate = Some(String::from_utf8_lossy(&body[i + 1..end]).into_owned());
                    }
                    i = end + 1;
                }
            }
            b'Z' => return sqlstate,
            _ => {}
        }
    }
}

#[test]
fn m2_extended_query_follows_flag() {
    for require in FLAGS {
        let f = fixture("wire-mx-m2", require);
        let mut s = connect(&f);
        // 省略。
        let r = extended(
            &mut s,
            "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja')",
            &[],
        );
        assert_eq!(r.as_deref(), if require { Some("23502") } else { None });
        // 値を束縛した指定は両モードで成功し、再送は 23505。
        let sql = "INSERT INTO docs (id, embedding, lang) VALUES (2, '[1.0,0.0]', 'ja') \
                   USING OPERATION_ID $1";
        assert_eq!(extended(&mut s, sql, &["ext-1"]), None, "require={require}");
        assert_eq!(
            extended(&mut s, sql, &["ext-1"]).as_deref(),
            Some("23505"),
            "require={require}"
        );
    }
}

// ---------------------------------------------------------------------------
// M3: COPY FROM STDIN
// ---------------------------------------------------------------------------

#[test]
fn m3_copy_from_stdin_follows_flag() {
    for require in FLAGS {
        let f = fixture("wire-mx-m3", require);
        let mut s = connect(&f);
        // 省略。
        send_simple_query(&mut s, "COPY docs (id, embedding, lang) FROM STDIN");
        if require {
            expect_error_response_with_sqlstate(&mut s, "23502");
            read_ready_for_query(&mut s);
        } else {
            let (t, _) = read_message(&mut s);
            assert_eq!(t, b'G', "CopyInResponse expected by default");
            send_length_prefixed_message(&mut s, b'd', b"1\t[1.0,0.0]\tja\n");
            send_length_prefixed_message(&mut s, b'c', b"");
            assert_eq!(read_command_complete(&mut s), "COPY 1");
            read_ready_for_query(&mut s);
        }
        // 指定は両モードで成功する。
        send_simple_query(
            &mut s,
            "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-1'",
        );
        let (t, _) = read_message(&mut s);
        assert_eq!(t, b'G', "CopyInResponse expected");
        send_length_prefixed_message(&mut s, b'd', b"2\t[1.0,0.0]\tja\n");
        send_length_prefixed_message(&mut s, b'c', b"");
        assert_eq!(read_command_complete(&mut s), "COPY 1");
        read_ready_for_query(&mut s);
    }
}

// ---------------------------------------------------------------------------
// M4: wire 越しの SET / RESET
// ---------------------------------------------------------------------------

/// 切替試行文。いずれも拒否され、直後の省略書き込みの結果（モード）は不変。
const SWITCH_ATTEMPTS: [&str; 5] = [
    "SET require_operation_id = off",
    "SET require_operation_id = on",
    "SET ledger_mode = 'optional'",
    "SET ledger_mode = 'ledgered'",
    "RESET require_operation_id",
];

#[test]
fn m4_set_over_wire_does_not_switch_mode() {
    for require in FLAGS {
        let f = fixture("wire-mx-m4", require);
        let mut s = connect(&f);
        let mut next_id = 100;
        for attempt in SWITCH_ATTEMPTS {
            q_err(&mut s, attempt, "42601");
            // 同一接続・新規接続の双方でモードは不変。
            let mut fresh = connect(&f);
            for stream in [&mut s, &mut fresh] {
                next_id += 1;
                let sql = format!(
                    "INSERT INTO docs (id, embedding, lang) VALUES ({next_id}, '[1.0,0.0]', 'ja')"
                );
                if require {
                    q_err(stream, &sql, "23502");
                } else {
                    q_ok(stream, &sql);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// M5: StartupMessage の追加パラメータ
// ---------------------------------------------------------------------------

/// 任意の追加パラメータ付き StartupMessage で認証し ReadyForQuery まで進める。
fn connect_with_startup_params(addr: SocketAddr, extra: &[(&str, &str)]) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let mut ssl = Vec::new();
    ssl.extend_from_slice(&8i32.to_be_bytes());
    ssl.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl).expect("send SSLRequest");
    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(&resp, b"N");

    let mut params = Vec::new();
    let mut push = |k: &str, v: &str| {
        params.extend_from_slice(k.as_bytes());
        params.push(0);
        params.extend_from_slice(v.as_bytes());
        params.push(0);
    };
    push("user", "alice");
    push("database", "irrelevant-db-name");
    for (k, v) in extra {
        push(k, v);
    }
    params.push(0);
    let total = (4 + 4 + params.len()) as i32;
    let mut msg = Vec::new();
    msg.extend_from_slice(&total.to_be_bytes());
    msg.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    msg.extend_from_slice(&params);
    stream.write_all(&msg).expect("send StartupMessage");

    assert_eq!(read_auth_request_type(&mut stream), 3);
    send_password_message(&mut stream, "pw-alice");
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 30, "too many messages before ReadyForQuery");
        if read_message_type_discarding_body(&mut stream) == b'Z' {
            return stream;
        }
    }
}

#[test]
fn m5_startup_parameters_do_not_switch_mode() {
    let cases: [&[(&str, &str)]; 4] = [
        &[("options", "-c require_operation_id=off")],
        &[("options", "-c require_operation_id=on")],
        &[("require_operation_id", "off"), ("ledger_mode", "optional")],
        &[("require_operation_id", "on"), ("ledger_mode", "ledgered")],
    ];
    for require in FLAGS {
        for extra in cases {
            let f = fixture("wire-mx-m5", require);
            let mut s = connect_with_startup_params(f.addr, extra);
            let sql = "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja')";
            if require {
                q_err(&mut s, sql, "23502");
            } else {
                q_ok(&mut s, sql);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// M6: HTTP のクエリ文字列
// ---------------------------------------------------------------------------

#[test]
fn m6_http_query_string_does_not_switch_mode() {
    for require in FLAGS {
        let f = fixture("wire-mx-m6", require);
        let addr = http_common::spawn_router_listener_with_engine(
            &f._users,
            wire_server::http::session::store::SessionStore::new(),
            Arc::clone(&f.core),
        );
        for target in [
            "/v1/query?require_operation_id=false",
            "/v1/query?require_operation_id=true",
            "/v1/query?ledger_mode=optional",
        ] {
            let request = http_common::build_request(
                target,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Length", "0"),
                ],
                b"",
            );
            let resp = http_common::parse_single_response(&http_common::send_raw(
                addr,
                &request,
                http_common::AfterWrite::HalfClose,
            ));
            // クエリ文字列付きの target は未知ルート扱い（`nosql1_endpoint_routing.rs` と同流儀）。
            http_common::assert_reached_router(&resp);
        }
        // 試行後も同じ engine のモードは不変。
        let mut s = connect(&f);
        let sql = "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja')";
        if require {
            q_err(&mut s, sql, "23502");
        } else {
            q_ok(&mut s, sql);
        }
    }
}
// ---------------------------------------------------------------------------
// M7: HTTP（NoSQL）書き込み op（Issue #2203）
// ---------------------------------------------------------------------------

use engine::recovery::ledger::LastOperationLookup;
use engine::recovery::required_op_id::OperationId;
use http_common::{AfterWrite, HttpResponse};

/// operation_id の「省略」表現 3 種（欠落・`null`・空文字）。
const HTTP_OMITTED: [&str; 3] = ["", r#","operation_id":null"#, r#","operation_id":"""#];

struct HttpClient {
    addr: SocketAddr,
    token: String,
}

fn http_client(f: &Fixture) -> HttpClient {
    let addr = http_common::spawn_router_listener_with_engine(
        &f._users,
        wire_server::http::session::store::SessionStore::new(),
        Arc::clone(&f.core),
    );
    let body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let marker = "\"token\":\"";
    let start = text.find(marker).expect("token field") + marker.len();
    let end = start + text[start..].find('"').expect("token end");
    HttpClient {
        addr,
        token: text[start..end].to_string(),
    }
}

fn post(c: &HttpClient, body: &str) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", c.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        c.addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn row(id: u32, lang: &str) -> String {
    format!(r#"{{"id":{id},"embedding":[1,0],"lang":"{lang}"}}"#)
}

/// 指定付き operation_id（`seed-<先頭 id>`）で行を投入する（両モードで成功する）。
fn seed_rows(c: &HttpClient, ids: &[u32]) {
    let rows: Vec<String> = ids.iter().map(|i| row(*i, "ja")).collect();
    let resp = post(
        c,
        &format!(
            r#"{{"op":"insert","table":"docs","rows":[{}],"operation_id":"seed-{}"}}"#,
            rows.join(","),
            ids[0]
        ),
    );
    assert_eq!(resp.status, 200, "seed must succeed: {resp:?}");
}

/// 省略した書き込み 1 件を送り、モード別の期待（既定＝成功・応答に `operation_id` キー無し／
/// 必須＝`23502` で副作用なし）を検証する。副作用の有無は行数で判定する。
fn assert_omitted_write(
    f: &Fixture,
    c: &HttpClient,
    require: bool,
    body: &str,
    expected_ok_body: &str,
    rows_after_ok: usize,
) {
    let before = count_rows(f);
    let resp = post(c, body);
    if require {
        assert_eq!(resp.status, 400, "{body}: {resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "23502", "{body}");
        assert_eq!(count_rows(f), before, "{body}: must not change rows");
    } else {
        assert_eq!(resp.status, 200, "{body}: {resp:?}");
        assert_eq!(body_text(&resp), expected_ok_body, "{body}");
        assert!(!body_text(&resp).contains("operation_id"), "{body}");
        assert_eq!(count_rows(f), rows_after_ok, "{body}");
    }
}

const FILTER_JA: &str = r#""filter":[{"column":"lang","op":"eq","value":"ja"}]"#;

#[test]
fn m7a_http_write_ops_omitted_operation_id_follows_flag() {
    for require in FLAGS {
        let f = fixture("wire-mx-m7a", require);
        let c = http_client(&f);
        seed_rows(&c, &[1, 2, 3]);
        for (n, tail) in HTTP_OMITTED.into_iter().enumerate() {
            // insert: 既定では行が 1 件増える（後始末は指定付き delete）。
            let body = format!(
                r#"{{"op":"insert","table":"docs","rows":[{}]{tail}}}"#,
                row(100, "en")
            );
            assert_omitted_write(&f, &c, require, &body, r#"{"inserted":1}"#, 4);
            if !require {
                // 後始末は指定付き delete（operation_id は反復ごとに別値にする）。
                let del = format!(
                    r#"{{"op":"delete","table":"docs","where":{{"id":100}},"operation_id":"cleanup-{n}"}}"#
                );
                assert_eq!(post(&c, &del).status, 200);
            }
        }
        // update（where 形・filter 形）: 行数は不変。
        for tail in HTTP_OMITTED {
            let body = format!(
                r#"{{"op":"update","table":"docs","set":{{"lang":"ja"}},"where":{{"id":1}}{tail}}}"#
            );
            assert_omitted_write(&f, &c, require, &body, r#"{"updated":1}"#, 3);
            let body = format!(
                r#"{{"op":"update","table":"docs","set":{{"lang":"ja"}},{FILTER_JA}{tail}}}"#
            );
            assert_omitted_write(&f, &c, require, &body, r#"{"updated":3}"#, 3);
        }
        // delete（where 形）。
        for tail in HTTP_OMITTED {
            let body = format!(r#"{{"op":"delete","table":"docs","where":{{"id":1}}{tail}}}"#);
            assert_omitted_write(&f, &c, require, &body, r#"{"deleted":1}"#, 2);
            if !require {
                let ins = format!(
                    r#"{{"op":"insert","table":"docs","rows":[{}]{tail}}}"#,
                    row(1, "ja")
                );
                assert_eq!(post(&c, &ins).status, 200);
            }
        }
        // delete（filter 形）。
        for tail in HTTP_OMITTED {
            let body = format!(r#"{{"op":"delete","table":"docs",{FILTER_JA}{tail}}}"#);
            assert_omitted_write(&f, &c, require, &body, r#"{"deleted":3}"#, 0);
            if !require {
                let rows: Vec<String> = [1, 2, 3].iter().map(|i| row(*i, "ja")).collect();
                let ins = format!(
                    r#"{{"op":"insert","table":"docs","rows":[{}]{tail}}}"#,
                    rows.join(",")
                );
                assert_eq!(post(&c, &ins).status, 200);
            }
        }
    }
}

#[test]
fn m7b_http_omitted_write_does_not_touch_ledger_by_default() {
    let f = fixture("wire-mx-m7b", false);
    let c = http_client(&f);
    seed_rows(&c, &[1]);
    let ctx = engine::policy::PolicyContext::new("tenant-a").expect("tenant");
    let expect_last = |label: &str| {
        assert_eq!(
            f.core.last_operation_id(&ctx, "docs").expect("last"),
            LastOperationLookup::Committed(OperationId::parse("seed-1").expect("op")),
            "{label}"
        );
    };
    expect_last("after seed");
    // 同一内容の省略書き込みは何度でも成功する（台帳照合が働かず 23505 にならない）。
    let upd = r#"{"op":"update","table":"docs","set":{"lang":"fr"},"where":{"id":1}}"#;
    for _ in 0..2 {
        assert_eq!(post(&c, upd).status, 200);
        expect_last("after omitted update");
    }
}

#[test]
fn m7c_http_specified_operation_id_resend_contract_is_identical_in_both_modes() {
    for require in FLAGS {
        let f = fixture("wire-mx-m7c", require);
        let c = http_client(&f);
        let ins = format!(
            r#"{{"op":"insert","table":"docs","rows":[{}],"operation_id":"op-1"}}"#,
            row(1, "ja")
        );
        let first = post(&c, &ins);
        assert_eq!(first.status, 200, "{first:?}");
        assert_eq!(body_text(&first), r#"{"inserted":1,"operation_id":"op-1"}"#);
        // 同一内容の再送 -> 23505、内容不一致 -> 22023。
        let same = post(&c, &ins);
        assert_eq!(http_common::wire_code_of(&same), "23505", "{same:?}");
        let diff = post(
            &c,
            &format!(
                r#"{{"op":"insert","table":"docs","rows":[{}],"operation_id":"op-1"}}"#,
                row(2, "en")
            ),
        );
        assert_eq!(http_common::wire_code_of(&diff), "22023", "{diff:?}");
    }
}

#[test]
fn m7d_http_partitioned_ops_require_operation_id_in_both_modes() {
    for require in FLAGS {
        let f = fixture("wire-mx-m7d", require);
        let c = http_client(&f);
        seed_rows(&c, &[1, 2]);
        for tail in HTTP_OMITTED {
            for body in [
                format!(
                    r#"{{"op":"update","table":"docs","set":{{"lang":"x"}},{FILTER_JA},"mode":"partitioned"{tail}}}"#
                ),
                format!(
                    r#"{{"op":"delete","table":"docs",{FILTER_JA},"mode":"partitioned"{tail}}}"#
                ),
            ] {
                let resp = post(&c, &body);
                assert_eq!(resp.status, 400, "{body}: {resp:?}");
                assert_eq!(http_common::wire_code_of(&resp), "23502", "{body}");
                assert_eq!(count_rows(&f), 2, "{body}: must not change rows");
            }
        }
    }
}

#[test]
fn m7e_http_show_and_cancel_partitioned_dml_require_operation_id_in_both_modes() {
    for require in FLAGS {
        let f = fixture("wire-mx-m7e", require);
        let c = http_client(&f);
        for op in ["show_partitioned_dml", "cancel_partitioned_dml"] {
            for tail in HTTP_OMITTED {
                let body = format!(r#"{{"op":"{op}","table":"docs"{tail}}}"#);
                let resp = post(&c, &body);
                assert_eq!(resp.status, 400, "{body}: {resp:?}");
                assert_eq!(http_common::wire_code_of(&resp), "23502", "{body}");
            }
        }
    }
}
