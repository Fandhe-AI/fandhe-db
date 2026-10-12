//! 式内 `$n` の型推論（Issue #1546。ポインタ: SQL-45 期待欄 (6)・WIRE-12・WIRE-14・ERR-6）を、
//! 実際の wire 経路（生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容: `UPDATE ... SET` 右辺・`INSERT ... VALUES` 要素の式内 `$n` の
//! `ParameterDescription` OID（演算の相手の列の型）、text／binary 両形式の Bind → Execute の
//! 結果、型を決められない `$n` の Parse 時 `42P18`（Sync で回復して接続が継続すること）。

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

const OID_INT4: i32 = 23;
const OID_INT8: i32 = 20;
/// `id` 列の `$n` は numeric として公告される（既存の `WHERE id = $n` と同じ）。
const OID_ID: i32 = 1700;
const OID_FLOAT8: i32 = 701;
const OID_TEXT: i32 = 25;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql45-param-infer");
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

#[test]
fn set_expression_params_are_described_with_the_operand_column_type() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let tail = " WHERE id = $2 USING OPERATION_ID $3";
    assert_eq!(
        describe_oids(&mut s, &format!("UPDATE t SET n = n + $1{tail}")),
        vec![OID_INT4, OID_ID, OID_TEXT]
    );
    assert_eq!(
        describe_oids(&mut s, &format!("UPDATE t SET b = $1 * b{tail}")),
        vec![OID_INT8, OID_ID, OID_TEXT]
    );
    assert_eq!(
        describe_oids(&mut s, &format!("UPDATE t SET d = d + $1{tail}")),
        vec![OID_FLOAT8, OID_ID, OID_TEXT]
    );
    assert_eq!(
        describe_oids(&mut s, &format!("UPDATE t SET s = upper($1){tail}")),
        vec![OID_TEXT, OID_ID, OID_TEXT]
    );
}

#[test]
fn set_expression_text_and_binary_binds_apply_the_expression() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());

    // text 形式。
    let tag = run(
        &mut s,
        "UPDATE t SET n = n + $1 WHERE id = $2 USING OPERATION_ID $3",
        &[0],
        &[b"5", b"1", b"w-1"],
    )
    .expect("text bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(15)));

    // binary 形式（int4・int8）。`$1` は INTEGER 列の型（int4）として復号される。
    let tag = run(
        &mut s,
        "UPDATE t SET n = n + $1 WHERE id = $2 USING OPERATION_ID $3",
        &[1, 0, 0],
        &[&7i32.to_be_bytes(), b"1", b"w-2"],
    )
    .expect("binary bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(22)));

    // BIGINT 列は int8、DOUBLE 列は float8 のバイナリで受ける。
    run(
        &mut s,
        "UPDATE t SET b = b + $1 WHERE id = $2 USING OPERATION_ID $3",
        &[1, 0, 0],
        &[&5i64.to_be_bytes(), b"1", b"w-3"],
    )
    .expect("int8 bind");
    assert!(matches!(cell(&core, "b", 1), Cell::SignedInteger(105)));
    run(
        &mut s,
        "UPDATE t SET d = d * $1 WHERE id = $2 USING OPERATION_ID $3",
        &[1, 0, 0],
        &[&2.0f64.to_be_bytes(), b"1", b"w-4"],
    )
    .expect("float8 bind");
    assert!(matches!(cell(&core, "d", 1), Cell::Float(v) if (v - 3.0).abs() < 1e-9));

    // text 引数（binary 形式のテキストは UTF-8 恒等）。
    run(
        &mut s,
        "UPDATE t SET s = upper($1) WHERE id = $2 USING OPERATION_ID $3",
        &[1, 0, 0],
        &[b"zz", b"1", b"w-5"],
    )
    .expect("text arg bind");
    assert!(matches!(cell(&core, "s", 1), Cell::Text(ref t) if t == "ZZ"));

    // 不正な値は 22P02。Sync で回復して接続が継続する。
    let err = run(
        &mut s,
        "UPDATE t SET n = n + $1 WHERE id = $2 USING OPERATION_ID $3",
        &[0],
        &[b"abc", b"1", b"w-6"],
    )
    .expect_err("invalid integer");
    assert_eq!(err, "22P02");
    assert_alive(&mut s);
}

#[test]
fn values_expression_params_are_described_and_bound() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let sql = "INSERT INTO t (id, n, s) VALUES ($1, 1 + $2, upper($3)) USING OPERATION_ID $4";
    assert_eq!(
        describe_oids(&mut s, sql),
        vec![OID_ID, OID_INT4, OID_TEXT, OID_TEXT]
    );
    // `$1` は `id`（要素単独・パターン 5）で `id` 列の型。
    let tag = run(&mut s, sql, &[0], &[b"2", b"41", b"hi", b"v-1"]).expect("bind");
    assert!(tag.starts_with("INSERT"), "{tag}");
    assert!(matches!(cell(&core, "n", 2), Cell::SignedInteger(42)));
    assert!(matches!(cell(&core, "s", 2), Cell::Text(ref t) if t == "HI"));
}

#[test]
fn indeterminate_params_fail_parse_with_42p18_and_the_connection_survives() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    for sql in [
        "UPDATE t SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'x'",
        "UPDATE t SET n = COALESCE($1, $2) WHERE id = 1 USING OPERATION_ID 'x'",
        "INSERT INTO t (id, n) VALUES (3, $1 + $2) USING OPERATION_ID 'x'",
    ] {
        send_length_prefixed_message(&mut s, b'P', &parse_msg("", sql));
        let body = expect_kind(&mut s, b'E');
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains("42P18"), "{sql}: {text:?}");
        send_length_prefixed_message(&mut s, b'S', b"");
        expect_kind(&mut s, b'Z');
        assert_alive(&mut s);
    }
}

#[test]
fn values_division_by_param_defers_value_errors_to_bind() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let sql = "INSERT INTO t (id, n) VALUES (5, 10 / $1) USING OPERATION_ID $2";
    // Parse は構造と型だけ検証し、仮値 0 による 22012 を返さない。
    assert_eq!(describe_oids(&mut s, sql), vec![OID_INT4, OID_TEXT]);
    let tag = run(&mut s, sql, &[0], &[b"2", b"d-1"]).expect("bind");
    assert!(tag.starts_with("INSERT"), "{tag}");
    assert!(matches!(cell(&core, "n", 5), Cell::SignedInteger(5)));
    // 実値 0 は Bind で 22012。
    let err = run(&mut s, sql, &[0], &[b"0", b"d-2"]).expect_err("div by zero");
    assert_eq!(err, "22012");
    assert_alive(&mut s);
}

#[test]
fn date_literal_neighbor_is_rejected_with_0a000() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let sql =
        "UPDATE t SET s = COALESCE($1, DATE '2024-01-01') WHERE id = 1 USING OPERATION_ID 'x'";
    send_length_prefixed_message(&mut s, b'P', &parse_msg("", sql));
    let body = expect_kind(&mut s, b'E');
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains("0A000"), "{text:?}");
    send_length_prefixed_message(&mut s, b'S', b"");
    expect_kind(&mut s, b'Z');
    assert_alive(&mut s);
}
/// Parse の `E` 応答に `code` が含まれ、Sync で回復して接続が継続することを確認する。
fn assert_parse_error(s: &mut Stream, sql: &str, code: &str) {
    send_length_prefixed_message(s, b'P', &parse_msg("", sql));
    let body = expect_kind(s, b'E');
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains(code), "{sql}: {text:?}");
    send_length_prefixed_message(s, b'S', b"");
    expect_kind(s, b'Z');
    assert_alive(s);
}

#[test]
fn values_unary_minus_param_is_inferred_and_bound() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let sql = "INSERT INTO t (id, n) VALUES (6, -$1) USING OPERATION_ID $2";
    assert_eq!(describe_oids(&mut s, sql), vec![OID_INT4, OID_TEXT]);
    let tag = run(&mut s, sql, &[0], &[b"7", b"m-1"]).expect("bind");
    assert!(tag.starts_with("INSERT"), "{tag}");
    assert!(matches!(cell(&core, "n", 6), Cell::SignedInteger(-7)));
}

#[test]
fn syntax_error_precedes_42p18_even_when_constant_folding_would_fail() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_parse_error(
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, $1 + $2) USING OPERATION_ID 'x' garbage",
        "42601",
    );
}

#[test]
fn constant_division_by_zero_is_not_deferred_by_other_params() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_parse_error(
        &mut s,
        "INSERT INTO t (id, n) VALUES (7, 10 / 0) USING OPERATION_ID $1",
        "22012",
    );
}

#[test]
fn nested_date_arithmetic_is_rejected_with_0a000() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_parse_error(
        &mut s,
        "UPDATE t SET n = $1 + (1 + DATE '2024-01-01') WHERE id = 1 USING OPERATION_ID 'x'",
        "0A000",
    );
}
