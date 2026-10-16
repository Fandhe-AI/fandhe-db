//! Parse の型 OID 宣言を `ParameterDescription` と値の復号に用いる契約
//! （Issue #2436。ポインタ: WIRE-24・WIRE-12・WIRE-14）を、実際の wire 経路
//! （生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容: 宣言 OID の echo（列スロット・推論不能な式のスロット）、unknown（705）の
//! 推論扱い、text／binary 形式の値の宣言型としての復号・検証、過剰宣言の `08P01`。
//! 持たない型の OID（配列型・存在しない OID・負値）は Parse で `0A000` となり、
//! ignore-till-sync の同期回復で接続を維持する（Issue #2438。WIRE-24 (1)・WIRE-11）。

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
const OID_FLOAT8: i32 = 701;
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

/// 宣言 float8 の `$1 / $2` は text／binary のどちらでも整数値（`1`・`2`）を浮動小数として
/// 置換し、整数除算（0）にならず 0.5 が保存される（Issue #2436 codex P1）。
#[test]
fn float8_division_keeps_fraction_in_text_and_binary() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let sql = "UPDATE t SET d = $1 / $2 WHERE id = 1 USING OPERATION_ID $3";
    let tag = run_declared(
        &mut s,
        sql,
        &[OID_FLOAT8, OID_FLOAT8],
        &[0],
        &[b"1", b"2", b"f8-1"],
    )
    .expect("text bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "d", 1), Cell::Float(v) if v == 0.5));

    // 値を別に変えてから binary 形式で同じ式を実行する。
    run_declared(
        &mut s,
        "UPDATE t SET d = 9.0 WHERE id = 1 USING OPERATION_ID $1",
        &[],
        &[0],
        &[b"f8-reset"],
    )
    .expect("reset");
    let tag = run_declared(
        &mut s,
        sql,
        &[OID_FLOAT8, OID_FLOAT8],
        &[1, 1, 0],
        &[&1.0f64.to_be_bytes(), &2.0f64.to_be_bytes(), b"f8-2"],
    )
    .expect("binary bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "d", 1), Cell::Float(v) if v == 0.5));
}

/// 宣言 timestamptz（1184）・interval（1186）の binary Bind は、列スロットと同じ復号器で
/// 受理される（未参照番号の宣言型スロット。Bugbot 指摘）。不正長は fail-closed で拒否する。
#[test]
fn declared_timestamptz_and_interval_binary_are_accepted() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let sql = "UPDATE t SET n = $3 WHERE id = 1 USING OPERATION_ID $4";
    let ts = 0i64.to_be_bytes();
    let mut iv = Vec::new();
    iv.extend_from_slice(&1_000_000i64.to_be_bytes());
    iv.extend_from_slice(&1i32.to_be_bytes());
    iv.extend_from_slice(&0i32.to_be_bytes());
    let tag = run_declared(
        &mut s,
        sql,
        &[1184, 1186],
        &[1, 1, 0, 0],
        &[&ts, &iv, b"7", b"tz-1"],
    )
    .expect("binary bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(7)));
    let err = run_declared(
        &mut s,
        sql,
        &[1184, 1186],
        &[1, 1, 0, 0],
        &[&ts[..4], &iv, b"8", b"tz-2"],
    )
    .expect_err("short timestamptz");
    assert!(!err.is_empty());
    assert_alive(&mut s);
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

/// 宣言 OID 付き Parse を送り、ErrorResponse の本文（`C`・`M` フィールド込み）を返す。
/// 呼び出し後は Sync → ReadyForQuery まで消費して同期回復させる。
fn parse_error_text(s: &mut Stream, name: &str, sql: &str, oids: &[i32]) -> String {
    send_length_prefixed_message(s, b'P', &parse_declared(name, sql, oids));
    let body = expect_kind(s, b'E');
    send_length_prefixed_message(s, b'S', b"");
    expect_kind(s, b'Z');
    String::from_utf8_lossy(&body).to_string()
}

const SELECT_N: &str = "SELECT id FROM t WHERE n = $1 LIMIT 5";

#[test]
fn unsupported_oid_is_rejected_with_0a000_and_connection_survives() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    // 配列型・存在しない OID・負値・i32::MIN はいずれも持たない型（Issue #2438）。
    for oid in [1007, 99_999, -1, i32::MIN] {
        let text = parse_error_text(&mut s, "", SELECT_N, &[oid]);
        assert!(text.contains("C0A000"), "oid {oid}: {text:?}");
    }
    // 同じ接続で後続の Parse・Bind・Execute が正常に動く。
    let tag = run_declared(&mut s, SELECT_N, &[OID_INT4], &[0], &[b"10"]).expect("recovered");
    assert_eq!(tag, "SELECT 1");
    let tag = run_declared(&mut s, SELECT_N, &[0], &[0], &[b"10"]).expect("recovered");
    assert_eq!(tag, "SELECT 1");
}

#[test]
fn unsupported_oid_in_later_position_names_that_param() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let text = parse_error_text(
        &mut s,
        "",
        "SELECT id FROM t WHERE n = $1 AND b = $2 LIMIT 5",
        &[OID_INT4, 1007],
    );
    assert!(text.contains("C0A000"), "{text:?}");
    assert!(text.contains("$2"), "{text:?}");
    assert!(
        !text.contains("tenant") && !text.contains(" t "),
        "{text:?}"
    );
    assert_alive(&mut s);
}

#[test]
fn rejected_named_statement_is_not_stored() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    // 拒否された名前付き Parse は保存されない。
    let text = parse_error_text(&mut s, "q", SELECT_N, &[1007]);
    assert!(text.contains("C0A000"), "{text:?}");
    send_length_prefixed_message(&mut s, b'D', &describe_statement_msg("q"));
    let body = expect_kind(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("C08P01"));
    send_length_prefixed_message(&mut s, b'S', b"");
    expect_kind(&mut s, b'Z');
    // 既存の同名 statement は拒否された再 Parse で上書き・削除されない。
    send_length_prefixed_message(&mut s, b'P', &parse_declared("k", SELECT_N, &[OID_INT2]));
    expect_kind(&mut s, b'1');
    let text = parse_error_text(&mut s, "k", SELECT_N, &[1007]);
    assert!(text.contains("C0A000"), "{text:?}");
    send_length_prefixed_message(&mut s, b'D', &describe_statement_msg("k"));
    let oids = param_description_oids(&expect_kind(&mut s, b't'));
    assert_eq!(oids, vec![OID_INT2]);
    let (k, _) = read_message(&mut s);
    assert!(k == b'T' || k == b'n');
    send_length_prefixed_message(&mut s, b'S', b"");
    expect_kind(&mut s, b'Z');
}

#[test]
fn unsupported_oid_rejection_does_not_depend_on_catalog() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    // 存在する表・存在しない表で同一の SQLSTATE と message になる（存在情報の非漏えい）。
    let a = parse_error_text(&mut s, "", SELECT_N, &[1007]);
    let b = parse_error_text(
        &mut s,
        "",
        "SELECT id FROM no_such_table WHERE n = $1 LIMIT 5",
        &[1007],
    );
    assert!(a.contains("C0A000"), "{a:?}");
    assert_eq!(a, b);
}

#[test]
fn precedence_of_0a000() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    // 件数上限（54000）は OID 判定より先。
    let mut many = vec![OID_INT4; 64];
    many.push(1007);
    let text = parse_error_text(&mut s, "", SELECT_N, &many);
    assert!(text.contains("C54000"), "{text:?}");
    // 空文 + 宣言は 08P01。
    let text = parse_error_text(&mut s, "", "", &[1007]);
    assert!(text.contains("C08P01"), "{text:?}");
    // Failed トランザクション中は 25P02。
    send_simple_query(&mut s, "BEGIN");
    loop {
        if read_message(&mut s).0 == b'Z' {
            break;
        }
    }
    send_simple_query(&mut s, "SELECT * FROM no_such_table");
    loop {
        if read_message(&mut s).0 == b'Z' {
            break;
        }
    }
    let text = parse_error_text(&mut s, "", SELECT_N, &[1007]);
    assert!(text.contains("C25P02"), "{text:?}");
}

#[test]
fn indeterminate_position_with_unsupported_oid_is_0a000_not_42p18() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_parse_error_declared(&mut s, EXPR, &[1007, OID_INT4], "0A000");
}
