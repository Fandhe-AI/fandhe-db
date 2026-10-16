//! Parse の型 OID 宣言を `ParameterDescription` と値の復号に用いる契約
//! （Issue #2436。ポインタ: WIRE-24・WIRE-12・WIRE-14）を、実際の wire 経路
//! （生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容: 宣言 OID の echo（列スロット・推論不能な式のスロット）、unknown（705）の
//! 推論扱い、text／binary 形式の値の宣言型としての復号・検証、過剰宣言の `08P01`。
//! 持たない型の OID の挙動は #2438 が変更するため固定しない。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use common::*;

type Stream = std::net::TcpStream;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire24-declared-param-oids");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new("s", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &PolicyContext::new("tenant-a").expect("tenant"),
        &mut session,
        "INSERT INTO t (id, n, b, d, s) VALUES (1, 10, 100, 1.5, 'a') USING OPERATION_ID 'seed-1'",
    )
    .expect("seed insert");
    (core, guard)
}

fn cell(core: &EngineCore, col: &str, id: u64) -> Cell {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &PolicyContext::with_visibilities(
                "tenant-a",
                [Visibility::Public, Visibility::Private],
            )
            .expect("tenant"),
            &mut session,
            &format!("SELECT {col} FROM t WHERE id = {id} LIMIT 10"),
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(r) => r.rows.into_iter().next().expect("row").cells[0].clone(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn connect(core: Arc<EngineCore>) -> Stream {
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users, core);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn expect_kind(stream: &mut Stream, kind: u8) -> Vec<u8> {
    let (k, body) = read_message(stream);
    assert_eq!(
        k as char,
        kind as char,
        "unexpected message: {}",
        String::from_utf8_lossy(&body)
    );
    body
}

fn describe_statement_msg(name: &str) -> Vec<u8> {
    let mut body = vec![b'S'];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn bind_msg(fmts: &[i16], values: &[&[u8]]) -> Vec<u8> {
    let mut body = vec![0u8, 0u8];
    body.extend_from_slice(&(fmts.len() as i16).to_be_bytes());
    for f in fmts {
        body.extend_from_slice(&f.to_be_bytes());
    }
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for v in values {
        body.extend_from_slice(&(v.len() as i32).to_be_bytes());
        body.extend_from_slice(v);
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn param_description_oids(body: &[u8]) -> Vec<i32> {
    let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
    (0..n)
        .map(|i| {
            i32::from_be_bytes([
                body[2 + 4 * i],
                body[3 + 4 * i],
                body[4 + 4 * i],
                body[5 + 4 * i],
            ])
        })
        .collect()
}

const OID_INT2: i32 = 21;
const OID_INT4: i32 = 23;
const OID_INT8: i32 = 20;
const OID_FLOAT4: i32 = 700;
const OID_DATE: i32 = 1082;
const OID_UNKNOWN: i32 = 705;

/// 宣言 OID 付きの Parse 本体。
fn parse_declared(name: &str, sql: &str, oids: &[i32]) -> Vec<u8> {
    let mut body = parse_body(name, sql, oids.len() as i16);
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    body
}

/// 宣言 OID 付き Parse → Describe(S) の ParameterDescription の OID を返す。
fn describe_declared(s: &mut Stream, sql: &str, oids: &[i32]) -> Vec<i32> {
    send_length_prefixed_message(s, b'P', &parse_declared("d", sql, oids));
    expect_kind(s, b'1');
    send_length_prefixed_message(s, b'D', &describe_statement_msg("d"));
    let got = param_description_oids(&expect_kind(s, b't'));
    let (k, _) = read_message(s);
    assert!(k == b'T' || k == b'n');
    send_length_prefixed_message(s, b'C', &describe_statement_msg("d"));
    expect_kind(s, b'3');
    got
}

/// 宣言 OID 付きで Parse → Bind → Execute → Sync を送り、成功ならコマンドタグ、
/// 失敗なら最初の SQLSTATE を返す。
fn run_declared(
    s: &mut Stream,
    sql: &str,
    oids: &[i32],
    fmts: &[i16],
    values: &[&[u8]],
) -> Result<String, String> {
    send_length_prefixed_message(s, b'P', &parse_declared("", sql, oids));
    send_length_prefixed_message(s, b'B', &bind_msg(fmts, values));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    let mut tag = String::new();
    let mut err: Option<String> = None;
    loop {
        let (k, body) = read_message(s);
        match k {
            b'C' => {
                tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'E' if err.is_none() => {
                let text = String::from_utf8_lossy(&body).to_string();
                err = Some(
                    text.split('\0')
                        .find_map(|f| f.strip_prefix('C').map(str::to_string))
                        .unwrap_or(text),
                );
            }
            b'Z' => break,
            _ => {}
        }
    }
    err.map_or(Ok(tag), Err)
}

fn assert_alive(s: &mut Stream) {
    send_simple_query(s, "SELECT id FROM t LIMIT 1");
    loop {
        let (k, _) = read_message(s);
        if k == b'Z' {
            break;
        }
    }
}

fn assert_parse_error_declared(s: &mut Stream, sql: &str, oids: &[i32], code: &str) {
    send_length_prefixed_message(s, b'P', &parse_declared("", sql, oids));
    let body = expect_kind(s, b'E');
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains(code), "{sql}: {text:?}");
    send_length_prefixed_message(s, b'S', b"");
    expect_kind(s, b'Z');
    assert_alive(s);
}

const EXPR: &str = "UPDATE t SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'x'";

#[test]
fn declared_oids_are_echoed_for_column_and_indeterminate_slots() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(
        describe_declared(&mut s, "SELECT id FROM t WHERE n = $1 LIMIT 5", &[OID_INT2]),
        vec![OID_INT2]
    );
    assert_eq!(
        describe_declared(&mut s, EXPR, &[OID_INT4, OID_INT4]),
        vec![OID_INT4, OID_INT4]
    );
    // 宣言件数がプレースホルダ数より少なければ残りは推論。
    assert_eq!(
        describe_declared(
            &mut s,
            "SELECT id FROM t WHERE n = $1 AND b = $2 LIMIT 5",
            &[OID_INT2]
        ),
        vec![OID_INT2, OID_INT8]
    );
}

#[test]
fn unknown_oid_is_inferred_like_zero() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(
        describe_declared(
            &mut s,
            "SELECT id FROM t WHERE n = $1 LIMIT 5",
            &[OID_UNKNOWN]
        ),
        vec![OID_INT4]
    );
    assert_parse_error_declared(&mut s, EXPR, &[OID_UNKNOWN, OID_UNKNOWN], "42P18");
    assert_parse_error_declared(&mut s, EXPR, &[OID_INT4, 0], "42P18");
}

#[test]
fn declared_type_validates_text_values_and_connection_recovers() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let by_n = "UPDATE t SET n = $1 WHERE id = 1 USING OPERATION_ID $2";
    let err = run_declared(&mut s, by_n, &[OID_INT2, 0], &[0], &[b"40000", b"o-1"])
        .expect_err("int2 out of range");
    assert_eq!(err, "22003");
    assert_alive(&mut s);
    let by_d = "UPDATE t SET d = $1 WHERE id = 1 USING OPERATION_ID $2";
    let err = run_declared(&mut s, by_d, &[OID_INT4, 0], &[0], &[b"1.5", b"o-2"])
        .expect_err("int4 with fraction");
    assert_eq!(err, "22P02");
    assert_alive(&mut s);
    let tag = run_declared(&mut s, by_n, &[OID_INT2, 0], &[0], &[b"7", b"o-3"]).expect("ok");
    assert_eq!(tag, "UPDATE 1");
}

#[test]
fn indeterminate_expression_binds_in_text_and_binary() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let sql = "UPDATE t SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID $3";
    let tag = run_declared(
        &mut s,
        sql,
        &[OID_INT4, OID_INT4],
        &[0],
        &[b"2", b"3", b"e-1"],
    )
    .expect("text bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(5)));
    let tag = run_declared(
        &mut s,
        sql,
        &[OID_INT4, OID_INT4],
        &[1, 1, 0],
        &[&20i32.to_be_bytes(), &22i32.to_be_bytes(), b"e-2"],
    )
    .expect("binary bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(42)));
}

#[test]
fn declared_date_binary_is_unsupported_and_legacy_table_is_unchanged() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    // `$1` は未参照番号（宣言 date がスロット型）。date のバイナリは 0A000。
    let sql = "UPDATE t SET n = $2 WHERE id = 1 USING OPERATION_ID 'x'";
    let err = run_declared(
        &mut s,
        sql,
        &[OID_DATE, 0],
        &[1, 0],
        &[&0i32.to_be_bytes(), b"5"],
    )
    .expect_err("date binary");
    assert_eq!(err, "0A000");
    assert_alive(&mut s);
    // 既存の互換表: 宣言 float4 を DOUBLE 列へバイナリで送るのは 0A000。
    let err = run_declared(
        &mut s,
        "UPDATE t SET d = $1 WHERE id = 1 USING OPERATION_ID $2",
        &[OID_FLOAT4, 0],
        &[1, 0],
        &[&1.0f32.to_be_bytes(), b"o-9"],
    )
    .expect_err("float4 into double slot");
    assert_eq!(err, "0A000");
    assert_alive(&mut s);
}

#[test]
fn over_declaration_is_08p01() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_parse_error_declared(
        &mut s,
        "SELECT id FROM t WHERE n = $1 LIMIT 5",
        &[OID_INT4, OID_INT4],
        "08P01",
    );
}
