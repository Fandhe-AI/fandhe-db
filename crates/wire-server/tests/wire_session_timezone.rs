//! セッション TimeZone（`SET TIME ZONE`／StartupMessage の `TimeZone`／ParameterStatus。
//! TABLE-33・Issue #1575）の pg wire 結合テスト。
//!
//! (1) 起動時の ParameterStatus `TimeZone`（既定 UTC・キー大文字小文字無視・不正値は
//! AuthenticationOk 後に FATAL `22023` で切断・重複は `08P01`）、(2) `SET` 後の
//! `CommandComplete`→`ParameterStatus`→`ReadyForQuery` の順序と、値が変わらない場合の通知抑止
//! （同値の再 SET・1 メッセージ内の `BEGIN; SET; ROLLBACK`）、(3) 単純／拡張プロトコルの
//! DataRow と COPY TO がセッション TZ で整形されること、(4) `SHOW timezone` の応答形、
//! (5) 接続間で TZ が独立していること、を固定する。engine 側の解釈・22023 は
//! `crates/engine/tests/sql_session_timezone.rs` が担当する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

const PASSWORD: &str = "correct-horse";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-session-timezone");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut s = SessionState::default();
    s.allow_ddl();
    let ctx = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("tenant");
    core.execute_sql_in_session(&ctx, &mut s, "CREATE TABLE t (v TIMESTAMPTZ)")
        .expect("create table");
    (core, guard)
}

fn spawn(core: &Arc<EngineCore>) -> std::net::SocketAddr {
    let users_path = write_user_store_file(&[("alice", "tenant-a", PASSWORD)]);
    spawn_server_with_engine(&users_path, Arc::clone(core))
}

/// StartupMessage（user に加えて任意の追加パラメータ）を送る。
fn send_startup(stream: &mut TcpStream, extra: &[(&str, &str)]) {
    let mut params = Vec::new();
    for (k, v) in [("user", "alice")].iter().chain(extra.iter()) {
        params.extend_from_slice(k.as_bytes());
        params.push(0);
        params.extend_from_slice(v.as_bytes());
        params.push(0);
    }
    params.push(0);
    let total_len = (4 + 4 + params.len()) as i32;
    let mut msg = Vec::new();
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    msg.extend_from_slice(&params);
    stream.write_all(&msg).expect("send startup");
}

/// 1 メッセージ（型, 本体）を読む。
fn read_msg(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    read_message(stream)
}

/// ParameterStatus の本体を (name, value) へ。
fn parse_param_status(body: &[u8]) -> (String, String) {
    let mut parts = body.split(|b| *b == 0);
    let name = String::from_utf8_lossy(parts.next().unwrap_or_default()).to_string();
    let value = String::from_utf8_lossy(parts.next().unwrap_or_default()).to_string();
    (name, value)
}

/// 接続して認証を通し、ReadyForQuery までの ParameterStatus を集めて返す。
fn connect(
    addr: std::net::SocketAddr,
    extra: &[(&str, &str)],
) -> (TcpStream, Vec<(String, String)>) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    send_startup(&mut stream, extra);
    let (t, _) = read_msg(&mut stream);
    assert_eq!(t, b'R', "AuthenticationCleartextPassword");
    send_password_message(&mut stream, PASSWORD);
    let (t, body) = read_msg(&mut stream);
    assert_eq!((t, body.as_slice()), (b'R', &0i32.to_be_bytes()[..]));
    let mut params = Vec::new();
    loop {
        let (t, body) = read_msg(&mut stream);
        match t {
            b'S' => params.push(parse_param_status(&body)),
            b'Z' => break,
            b'K' => {}
            other => panic!("unexpected message {}", other as char),
        }
    }
    (stream, params)
}

fn tz_of(params: &[(String, String)]) -> Option<&str> {
    params
        .iter()
        .find(|(k, _)| k == "TimeZone")
        .map(|(_, v)| v.as_str())
}

/// ReadyForQuery まで（含む）の応答を `(型, 本体)` の列で返す。
fn until_ready(stream: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        let msg = read_msg(stream);
        let done = msg.0 == b'Z';
        out.push(msg);
        if done {
            return out;
        }
    }
}

fn types(msgs: &[(u8, Vec<u8>)]) -> String {
    msgs.iter().map(|(t, _)| *t as char).collect()
}

fn tag_of(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().saturating_sub(1)]).to_string()
}

fn run(stream: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    send_simple_query(stream, sql);
    until_ready(stream)
}

/// `SELECT id, v FROM t` の v の文字列（id 昇順の全行）。
fn select_v(stream: &mut TcpStream) -> Vec<String> {
    let msgs = run(stream, "SELECT id, v FROM t LIMIT 20");
    msgs.iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, body)| {
            // field count(2) + len(4) + id + len(4) + v
            let n = i16::from_be_bytes([body[0], body[1]]) as usize;
            assert_eq!(n, 2);
            let mut pos = 2;
            let mut cells = Vec::new();
            for _ in 0..n {
                let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4;
                cells.push(String::from_utf8_lossy(&body[pos..pos + len]).to_string());
                pos += len;
            }
            cells[1].clone()
        })
        .collect()
}

#[test]
fn startup_reports_utc_by_default() {
    let (core, _g) = new_core();
    let (_s, params) = connect(spawn(&core), &[]);
    assert_eq!(tz_of(&params), Some("UTC"));
}

#[test]
fn startup_time_zone_key_is_case_insensitive_and_value_is_normalized() {
    let (core, _g) = new_core();
    let (mut s, params) = connect(spawn(&core), &[("timezone", "asia/tokyo")]);
    assert_eq!(tz_of(&params), Some("Asia/Tokyo"));
    let msgs = run(&mut s, "SHOW timezone");
    assert_eq!(types(&msgs), "TDCZ");
    // DEFAULT は startup の値へ戻る。
    run(&mut s, "SET TIME ZONE 'UTC'");
    let msgs = run(&mut s, "SET TIME ZONE DEFAULT");
    assert_eq!(types(&msgs), "CSZ");
    assert_eq!(parse_param_status(&msgs[1].1).1, "Asia/Tokyo");
}

#[test]
fn invalid_startup_time_zone_is_fatal_22023_after_authentication_ok() {
    let (core, _g) = new_core();
    let mut stream = TcpStream::connect(spawn(&core)).expect("connect");
    send_startup(&mut stream, &[("TimeZone", "Mars/Olympus_Mons")]);
    let (t, _) = read_msg(&mut stream);
    assert_eq!(t, b'R');
    send_password_message(&mut stream, PASSWORD);
    let (t, body) = read_msg(&mut stream);
    assert_eq!((t, body.as_slice()), (b'R', &0i32.to_be_bytes()[..]));
    let (t, body) = read_msg(&mut stream);
    assert_eq!(t, b'E');
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains("SFATAL"), "{text:?}");
    assert!(text.contains("C22023"), "{text:?}");
    assert!(
        text.contains("Minvalid value for parameter \"TimeZone\"\0"),
        "{text:?}"
    );
    assert!(!text.contains("Mars"), "value must not be reflected");
    expect_connection_closed(&mut stream);
}

#[test]
fn duplicate_startup_time_zone_is_protocol_violation() {
    let (core, _g) = new_core();
    let mut stream = TcpStream::connect(spawn(&core)).expect("connect");
    send_startup(&mut stream, &[("TimeZone", "UTC"), ("timezone", "UTC")]);
    expect_error_response_with_sqlstate(&mut stream, "08P01");
}

#[test]
fn set_emits_parameter_status_only_when_value_changes() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);

    let msgs = run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(types(&msgs), "CSZ");
    assert_eq!(tag_of(&msgs[0].1), "SET");
    assert_eq!(
        parse_param_status(&msgs[1].1),
        ("TimeZone".to_string(), "Asia/Tokyo".to_string())
    );

    // 同じ値の再設定（表記が違っても正規化後が同じ）は通知しない。
    let msgs = run(&mut s, "SET timezone TO 'asia/tokyo'");
    assert_eq!(types(&msgs), "CZ");

    // 値が変わる再設定は通知する。
    let msgs = run(&mut s, "SET TIME ZONE 9");
    assert_eq!(types(&msgs), "CSZ");
    assert_eq!(parse_param_status(&msgs[1].1).1, "<+09>-09");
}

#[test]
fn rollback_notifies_the_restored_value_and_same_message_rollback_does_not() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);

    // メッセージを分けた BEGIN / SET / ROLLBACK は SET と ROLLBACK の双方で通知される。
    let msgs = run(&mut s, "BEGIN");
    assert_eq!(types(&msgs), "CZ");
    let msgs = run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(types(&msgs), "CSZ");
    let msgs = run(&mut s, "ROLLBACK");
    assert_eq!(types(&msgs), "CSZ");
    assert_eq!(parse_param_status(&msgs[1].1).1, "UTC");

    // 1 メッセージ内で戻ると最終値が変わらないので通知しない。
    let msgs = run(&mut s, "BEGIN; SET TIME ZONE 'Asia/Tokyo'; ROLLBACK");
    assert!(
        !msgs.iter().any(|(t, _)| *t == b'S'),
        "unexpected ParameterStatus: {}",
        types(&msgs)
    );
}

#[test]
fn data_rows_follow_the_session_time_zone() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    run(&mut s, "SET TIME ZONE 'America/New_York'");
    let msgs = run(
        &mut s,
        "INSERT INTO t (id, v) VALUES (1, '2026-07-01 12:00:00') USING OPERATION_ID 'o1'",
    );
    assert_eq!(tag_of(&msgs[0].1), "INSERT 0 1");
    run(
        &mut s,
        "INSERT INTO t (id, v) VALUES (2, '2026-01-01 12:00:00') USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        select_v(&mut s),
        ["2026-07-01 12:00:00-04", "2026-01-01 12:00:00-05"]
    );
    run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(
        select_v(&mut s),
        ["2026-07-02 01:00:00+09", "2026-01-02 02:00:00+09"]
    );
    run(&mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(
        select_v(&mut s),
        ["2026-07-01 16:00:00+00", "2026-01-01 17:00:00+00"]
    );
}

#[test]
fn show_returns_time_zone_column_and_show_tag() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    run(&mut s, "SET TIME ZONE 'Japan'");
    let msgs = run(&mut s, "SHOW TIME ZONE");
    assert_eq!(types(&msgs), "TDCZ");
    let body = &msgs[1].1;
    assert_eq!(&body[6..], b"Japan");
    assert_eq!(tag_of(&msgs[2].1), "SHOW");
    // 列名は TimeZone（text, OID 25）。
    let desc = &msgs[0].1;
    let text = String::from_utf8_lossy(desc).to_string();
    assert!(text.contains("TimeZone"));
}

#[test]
fn extended_protocol_matches_simple_protocol() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    run(
        &mut s,
        "INSERT INTO t (id, v) VALUES (1, '2026-01-01 09:00:00') USING OPERATION_ID 'o1'",
    );

    // 拡張プロトコルの SET: Sync 後に ParameterStatus が届く。
    for (ty, body) in [
        (b'P', parse_body("", "SET TIME ZONE 'America/New_York'", 0)),
        (b'B', bind_body("", "")),
        (b'E', execute_body("", 0)),
        (b'S', Vec::new()),
    ] {
        send_length_prefixed_message(&mut s, ty, &body);
    }
    let msgs = until_ready(&mut s);
    assert_eq!(types(&msgs), "12CSZ");
    assert_eq!(parse_param_status(&msgs[3].1).1, "America/New_York");

    // 拡張プロトコルの SELECT は新しい TZ で整形される。
    for (ty, body) in [
        (b'P', parse_body("", "SELECT id, v FROM t LIMIT 5", 0)),
        (b'B', bind_body("", "")),
        (b'E', execute_body("", 0)),
        (b'S', Vec::new()),
    ] {
        send_length_prefixed_message(&mut s, ty, &body);
    }
    let msgs = until_ready(&mut s);
    assert_eq!(types(&msgs), "12DCZ");
    let body = &msgs[2].1;
    assert!(
        String::from_utf8_lossy(body).contains("2025-12-31 19:00:00-05"),
        "{:?}",
        String::from_utf8_lossy(body)
    );
}

#[test]
fn connections_are_independent() {
    let (core, _g) = new_core();
    let addr = spawn(&core);
    let (mut a, _) = connect(addr, &[]);
    let (mut b, params_b) = connect(addr, &[]);
    assert_eq!(tz_of(&params_b), Some("UTC"));
    run(&mut a, "SET TIME ZONE 'Asia/Tokyo'");
    let msgs = run(&mut b, "SHOW timezone");
    assert_eq!(&msgs[1].1[6..], b"UTC");
    let msgs = run(&mut a, "SHOW timezone");
    assert_eq!(&msgs[1].1[6..], b"Asia/Tokyo");
}

#[test]
fn copy_to_is_utc_fixed_regardless_of_session_time_zone() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    run(
        &mut s,
        "INSERT INTO t (id, v) VALUES (1, '2026-01-01 00:00:00Z') USING OPERATION_ID 'o1'",
    );
    run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    let msgs = run(&mut s, "COPY (SELECT id, v FROM t LIMIT 5) TO STDOUT");
    let data: Vec<String> = msgs
        .iter()
        .filter(|(t, _)| *t == b'd')
        .map(|(_, b)| String::from_utf8_lossy(b).to_string())
        .collect();
    assert_eq!(data.len(), 1);
    assert!(data[0].contains("2026-01-01 00:00:00+00"), "{:?}", data[0]);
}

/// エラー終了する複数文メッセージでも、セッション復元後の TimeZone の ParameterStatus は
/// ReadyForQuery より前に届く（Issue #1575 レビュー指摘）。
#[test]
fn error_terminated_multi_statement_message_notifies_restored_value_before_ready() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    run(&mut s, "BEGIN");
    run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    let msgs = run(&mut s, "ROLLBACK; SELECT id FROM missing LIMIT 1");
    assert!(msgs.iter().any(|(t, _)| *t == b'E'), "{}", types(&msgs));
    assert_eq!(msgs.last().map(|(t, _)| *t), Some(b'Z'), "{}", types(&msgs));
    let reported = msgs
        .iter()
        .filter(|(t, _)| *t == b'S')
        .map(|(_, b)| parse_param_status(b).1)
        .collect::<Vec<_>>();
    assert_eq!(reported.last().map(String::as_str), Some("UTC"));
}

/// 読み取りが終端していることの回帰（ハーネスの健全性。未使用 import 警告も避ける）。
#[test]
fn harness_sanity() {
    let mut buf = [0u8; 1];
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    send_length_prefixed_message(&mut s, b'X', &[]);
    assert_eq!(s.read(&mut buf).unwrap_or(0), 0);
}
/// RowDescription（`T`）の第 `col` 列（0 起点）の型 OID。
fn column_oid(msgs: &[(u8, Vec<u8>)], col: usize) -> i32 {
    let body = &msgs
        .iter()
        .find(|(t, _)| *t == b'T')
        .expect("RowDescription")
        .1;
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    assert!(col < n);
    let mut pos = 2;
    for i in 0..n {
        let name_end = body[pos..].iter().position(|b| *b == 0).expect("name nul");
        pos += name_end + 1;
        // table oid(4) + attnum(2) + type oid(4) + typlen(2) + typmod(4) + format(2)
        let oid = i32::from_be_bytes(body[pos + 6..pos + 10].try_into().unwrap());
        if i == col {
            return oid;
        }
        pos += 18;
    }
    unreachable!("column index checked above")
}

/// 2 列目の DataRow 文字列（単純プロトコル）。
fn second_cells(msgs: &[(u8, Vec<u8>)]) -> Vec<String> {
    msgs.iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, body)| {
            let mut pos = 2;
            let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4 + len;
            let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            String::from_utf8_lossy(&body[pos..pos + len]).to_string()
        })
        .collect()
}

/// `AT TIME ZONE`／`date_trunc` の結果列の型 OID（timestamp=1114・timestamptz=1184）と、
/// timestamptz 結果がセッション TZ の offset 付きで整形されること（Issue #1576）。
#[test]
fn at_time_zone_result_types_and_session_formatting() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    run(
        &mut s,
        "INSERT INTO t (id, v) VALUES (1, '2026-07-01 12:00:00+00') USING OPERATION_ID 'o1'",
    );
    run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    // timestamptz → timestamp（壁時計。offset なし）。
    let msgs = run(&mut s, "SELECT id, v AT TIME ZONE 'UTC' FROM t LIMIT 5");
    assert_eq!(column_oid(&msgs, 1), 1114);
    assert_eq!(second_cells(&msgs), ["2026-07-01 12:00:00"]);
    // timestamptz → timestamptz（date_trunc。セッション TZ の offset 付き）。
    let msgs = run(&mut s, "SELECT id, date_trunc('day', v) FROM t LIMIT 5");
    assert_eq!(column_oid(&msgs, 1), 1184);
    assert_eq!(second_cells(&msgs), ["2026-07-01 00:00:00+09"]);
}

/// パラメータの無い prepared statement の定数畳み込み（ゾーン無し `TIMESTAMPTZ` リテラル）は、
/// Parse 時ではなく Bind 時のセッション TimeZone で評価する（Issue #1576 レビュー指摘）。
#[test]
fn parameterless_prepared_insert_uses_the_bind_time_zone() {
    let (core, _g) = new_core();
    let (mut s, _) = connect(spawn(&core), &[]);
    // UTC のまま Parse し、Bind 前に TZ を変える。
    for (ty, body) in [
        (
            b'P',
            parse_body(
                "s1",
                "INSERT INTO t (id, v) VALUES (1, TIMESTAMPTZ '2026-01-01 09:00:00') \
                 USING OPERATION_ID 'pp1'",
                0,
            ),
        ),
        (b'S', Vec::new()),
    ] {
        send_length_prefixed_message(&mut s, ty, &body);
    }
    until_ready(&mut s);
    run(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    for (ty, body) in [
        (b'B', bind_body("", "s1")),
        (b'E', execute_body("", 0)),
        (b'S', Vec::new()),
    ] {
        send_length_prefixed_message(&mut s, ty, &body);
    }
    until_ready(&mut s);
    // 東京の 09:00 は UTC の 00:00（東京表示で 09:00:00+09）。UTC で畳み込むと 18:00:00+09 になる。
    assert_eq!(select_v(&mut s), ["2026-01-01 09:00:00+09"]);
}
