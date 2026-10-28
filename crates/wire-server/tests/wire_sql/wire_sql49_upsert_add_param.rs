//! `INSERT ... ON CONFLICT ... DO UPDATE SET <col> = <col> + $n` の `$n` の型推論と受理
//! （Issue #2427。ポインタ: SQL-49・WIRE-12・WIRE-24・ERR-6）を、実際の wire 経路
//! （生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容: `ParameterDescription` の OID（代入先の列の型）、text／binary 両形式の
//! Bind → Execute の加算結果、Parse の宣言型の不一致（`42883`）・不正な値（`22P02`）・
//! 未対応の形（`42601`）の ErrorResponse と、Sync での回復後に接続が継続すること。

use crate::common;

use crate::temp_db;

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

const OID_INT4: i32 = 23;
const OID_INT8: i32 = 20;
const OID_INT2: i32 = 21;
const OID_FLOAT4: i32 = 700;
const OID_FLOAT8: i32 = 701;
const OID_TEXT: i32 = 25;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql49-upsert-add-param");
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
                ColumnDef::new("sm", ColumnType::SmallInt, true),
                ColumnDef::new("r", ColumnType::Real, true),
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
        "INSERT INTO t (id, n, b, d, s, sm, r) VALUES (1, 10, 100, 1.5, 'a', 5, 0.5) USING OPERATION_ID 'seed-1'",
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

fn parse_msg(name: &str, sql: &str) -> Vec<u8> {
    parse_body(name, sql, 0)
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

/// Parse → Describe(S) の ParameterDescription の OID を返す。
fn describe_oids(s: &mut Stream, sql: &str) -> Vec<i32> {
    send_length_prefixed_message(s, b'P', &parse_msg("d", sql));
    expect_kind(s, b'1');
    send_length_prefixed_message(s, b'D', &describe_statement_msg("d"));
    let oids = param_description_oids(&expect_kind(s, b't'));
    let (k, _) = read_message(s);
    assert!(k == b'T' || k == b'n');
    send_length_prefixed_message(s, b'C', &describe_statement_msg("d"));
    expect_kind(s, b'3');
    oids
}

/// Parse → Bind → Execute → Sync を送り、成功ならコマンドタグ、失敗なら SQLSTATE を返す。
fn run(s: &mut Stream, sql: &str, fmts: &[i16], values: &[&[u8]]) -> Result<String, String> {
    send_length_prefixed_message(s, b'P', &parse_msg("", sql));
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

fn upsert(col: &str) -> String {
    format!(
        "INSERT INTO t (id, {col}) VALUES (1, 0) ON CONFLICT (id) DO UPDATE SET {col} = {col} + $1 \
         USING OPERATION_ID $2"
    )
}

/// 型 OID を宣言した Parse メッセージ本体（`oids` は `$1` から順）。
fn parse_with_oids(sql: &str, oids: &[i32]) -> Vec<u8> {
    let mut body = parse_body("", sql, i16::try_from(oids.len()).expect("len"));
    for o in oids {
        body.extend_from_slice(&o.to_be_bytes());
    }
    body
}

/// Parse の `E` 応答に `code` が含まれ、Sync で回復して接続が継続することを確認する。
fn expect_parse_error(s: &mut Stream, body: &[u8], code: &str) {
    send_length_prefixed_message(s, b'P', body);
    let msg = expect_kind(s, b'E');
    let text = String::from_utf8_lossy(&msg).to_string();
    assert!(text.contains(code), "{text:?}");
    send_length_prefixed_message(s, b'S', b"");
    expect_kind(s, b'Z');
    assert_alive(s);
}

#[test]
fn upsert_add_param_is_described_with_the_assigned_column_type() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(
        describe_oids(&mut s, &upsert("n")),
        vec![OID_INT4, OID_TEXT]
    );
    assert_eq!(
        describe_oids(&mut s, &upsert("b")),
        vec![OID_INT8, OID_TEXT]
    );
    assert_eq!(
        describe_oids(&mut s, &upsert("sm")),
        vec![OID_INT2, OID_TEXT]
    );
    assert_eq!(
        describe_oids(&mut s, &upsert("r")),
        vec![OID_FLOAT4, OID_TEXT]
    );
    assert_eq!(
        describe_oids(&mut s, &upsert("d")),
        vec![OID_FLOAT8, OID_TEXT]
    );
}

#[test]
fn upsert_add_text_and_binary_binds_add_to_the_existing_row() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());

    let tag = run(&mut s, &upsert("n"), &[0], &[b"5", b"u-1"]).expect("text bind");
    assert!(tag.starts_with("INSERT"), "{tag}");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(15)));

    // binary 形式（int4・int8・float8）。`$1` は代入先の列の型として復号される。
    run(
        &mut s,
        &upsert("n"),
        &[1, 0],
        &[&(-3i32).to_be_bytes(), b"u-2"],
    )
    .expect("int4");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(12)));
    run(
        &mut s,
        &upsert("b"),
        &[1, 0],
        &[&5i64.to_be_bytes(), b"u-3"],
    )
    .expect("int8");
    assert!(matches!(cell(&core, "b", 1), Cell::SignedInteger(105)));
    run(
        &mut s,
        &upsert("d"),
        &[1, 0],
        &[&0.5f64.to_be_bytes(), b"u-4"],
    )
    .expect("float8");
    assert!(matches!(cell(&core, "d", 1), Cell::Float(v) if (v - 2.0).abs() < 1e-9));
}

#[test]
fn upsert_add_invalid_value_and_overflow_fail_and_the_connection_survives() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let err = run(&mut s, &upsert("n"), &[0], &[b"abc", b"e-1"]).expect_err("invalid");
    assert_eq!(err, "22P02");
    assert_alive(&mut s);
    // SMALLINT の範囲外は `22003` で行は不変。
    let err = run(&mut s, &upsert("sm"), &[0], &[b"40000", b"e-2"]).expect_err("overflow");
    assert_eq!(err, "22003");
    assert!(matches!(cell(&core, "sm", 1), Cell::SignedInteger(5)));
    assert_alive(&mut s);
}

#[test]
fn upsert_add_declared_type_mismatch_is_rejected_at_parse() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    // text(25) を整数列の加算オペランドに宣言 → `42883`。
    expect_parse_error(&mut s, &parse_with_oids(&upsert("n"), &[OID_TEXT]), "42883");
    // float8(701) を整数列に宣言 → `0A000`（リテラル経路の整数列への非整数加算と同じ）。
    expect_parse_error(
        &mut s,
        &parse_with_oids(&upsert("n"), &[OID_FLOAT8]),
        "0A000",
    );
    // 整数列に int8(20) の宣言は受理される。
    send_length_prefixed_message(&mut s, b'P', &parse_with_oids(&upsert("n"), &[OID_INT8]));
    expect_kind(&mut s, b'1');
    send_length_prefixed_message(&mut s, b'S', b"");
    expect_kind(&mut s, b'Z');
}

#[test]
fn upsert_add_unsupported_shapes_fail_parse_with_42601() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    for set in ["n = $1", "n = $1 + n", "n = b + $1", "n = n + $1 + 1"] {
        let sql = format!(
            "INSERT INTO t (id, n, b) VALUES (1, 0, 0) ON CONFLICT (id) DO UPDATE SET {set} \
             USING OPERATION_ID 'x'"
        );
        expect_parse_error(&mut s, &parse_msg("", &sql), "42601");
    }
}
