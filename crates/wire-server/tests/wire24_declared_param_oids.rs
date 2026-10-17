//! Parse の型 OID 宣言を `ParameterDescription` と値の復号に用いる契約
//! （Issue #2436。ポインタ: WIRE-24・WIRE-12・WIRE-14）を、実際の wire 経路
//! （生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容: 宣言 OID の echo（列スロット・推論不能な式のスロット）、unknown（705）の
//! 推論扱い、text／binary 形式の値の宣言型としての復号・検証、過剰宣言の `08P01`。
//! 持たない型の OID（配列型・存在しない OID・負値）は Parse で `0A000` となり、
//! ignore-till-sync の同期回復で接続を維持する（Issue #2438。WIRE-24 (1)・WIRE-11）。
//!
//! Issue #2440（WIRE-24・SQL-45 のポインタ）で、NULL パラメータ（リテラル NULL と同じ扱い）・
//! NOT NULL 列との交差・OID の混在・3 テナントでの拒否応答のバイト一致を追加で固定する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
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
    // NOT NULL 列を持つ別表（NULL パラメータが 23502 になることの確認用。Issue #2440）。
    storage
        .create_table(&TableSchema::new(
            "nn",
            vec![
                ColumnDef::new("req", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create nn");
    // 他テナントの Private 行（テナント境界の非観測性の確認用。Issue #2440）。
    for (tenant, id, n) in [("tenant-a", 5u64, 50), ("tenant-b", 6, 60)] {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "t",
            &ctx,
            id,
            Visibility::Private,
            &[
                Value::Integer(n),
                Value::BigInt(i64::from(n)),
                Value::Double(1.5),
                Value::Text("private".to_string()),
            ],
            &OperationId::parse(&format!("seed-private-{id}")).expect("op id"),
        )
        .expect("seed private row");
    }
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
const OID_TEXT: i32 = 25;
const OID_BOOL: i32 = 16;
const OID_VARCHAR: i32 = 1043;

/// 宣言型が文脈の型へ変換できない Parse は拒否される（Issue #2437。WIRE-24 (1)(5)・
/// SQL-41・SQL-45 のポインタ）: 代入の位置は `42804`、比較の位置は `42883`。
/// 拒否後も接続は維持され、名前付き statement は保存されない。
#[test]
fn unconvertible_declaration_is_rejected_and_connection_survives() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let assign = "UPDATE t SET n = $1 WHERE id = 1 USING OPERATION_ID 'x'";
    for oid in [OID_TEXT, OID_VARCHAR, OID_BOOL, OID_DATE] {
        assert_parse_error_declared(&mut s, assign, &[oid], "42804");
        let insert = "INSERT INTO t (id, n) VALUES (2, $1) USING OPERATION_ID 'y'";
        assert_parse_error_declared(&mut s, insert, &[oid], "42804");
    }
    for oid in [OID_TEXT, OID_BOOL, OID_DATE] {
        assert_parse_error_declared(&mut s, SELECT_N, &[oid], "42883");
    }
    // 文字列列への int8 の比較も拒否される。
    assert_parse_error_declared(
        &mut s,
        "SELECT id FROM t WHERE s = $1 LIMIT 5",
        &[OID_INT8],
        "42883",
    );
    // 拒否した名前付き statement は保存されない。
    let text = parse_error_text(&mut s, "q", SELECT_N, &[OID_TEXT]);
    assert!(text.contains("C42883"), "{text:?}");
    send_length_prefixed_message(&mut s, b'D', &describe_statement_msg("q"));
    let body = expect_kind(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("C08P01"));
    send_length_prefixed_message(&mut s, b'S', b"");
    expect_kind(&mut s, b'Z');
}

/// 変換できる宣言は従来どおり受理され、実行まで通って値が保存される。
#[test]
fn convertible_declaration_executes_and_stores() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let upd_n = "UPDATE t SET n = $1 WHERE id = 1 USING OPERATION_ID $2";
    let tag = run_declared(&mut s, upd_n, &[OID_INT8, 0], &[0], &[b"77", b"o-a"]).expect("int8");
    assert_eq!(tag, "UPDATE 1");
    assert_eq!(cell(&core, "n", 1), Cell::SignedInteger(77));
    let upd_s = "UPDATE t SET s = $1 WHERE id = 1 USING OPERATION_ID $2";
    let tag = run_declared(&mut s, upd_s, &[OID_INT4, 0], &[0], &[b"5", b"o-b"]).expect("int4");
    assert_eq!(tag, "UPDATE 1");
    assert_eq!(cell(&core, "s", 1), Cell::Text("5".to_string()));
    // 比較の位置は型をまたいでも暗黙の変換があれば受理する。
    let tag = run_declared(&mut s, SELECT_N, &[OID_INT8], &[0], &[b"77"]).expect("select");
    assert!(tag.starts_with("SELECT"), "{tag}");
}

/// `0A000`（持たない型）は変換可否の判定より先に返る。
#[test]
fn unsupported_oid_takes_precedence_over_conversion_check() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let text = parse_error_text(&mut s, "", SELECT_N, &[1007]);
    assert!(text.contains("C0A000"), "{text:?}");
}

/// 拒否／受理は他テナントの行の有無に依存しない（テナント境界）。行を持つテナントと
/// 持たないテナントで、同じ宣言は同じ応答になる。
#[test]
fn conversion_check_does_not_depend_on_tenant_rows() {
    let (core, _g) = new_core();
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users, core);
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    let ta = parse_error_text(&mut a, "", SELECT_N, &[OID_TEXT]);
    let tb = parse_error_text(&mut b, "", SELECT_N, &[OID_TEXT]);
    assert!(ta.contains("C42883"), "{ta:?}");
    assert_eq!(ta, tb);
    for s in [&mut a, &mut b] {
        send_length_prefixed_message(s, b'P', &parse_declared("", SELECT_N, &[OID_INT8]));
        expect_kind(s, b'1');
    }
}
// --- Issue #2440: NULL パラメータ・OID 混在・3 テナントの非観測性（WIRE-24・SQL-45） ---

/// 値に NULL（長さ -1）を含められる Bind 本体（結果形式はすべて text）。
fn bind_msg_opt(fmts: &[i16], values: &[Option<&[u8]>]) -> Vec<u8> {
    let mut body = vec![0u8, 0u8];
    body.extend_from_slice(&(fmts.len() as i16).to_be_bytes());
    for f in fmts {
        body.extend_from_slice(&f.to_be_bytes());
    }
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for v in values {
        match v {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                body.extend_from_slice(&(v.len() as i32).to_be_bytes());
                body.extend_from_slice(v);
            }
        }
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

/// Parse → Bind → Execute → Sync を送り、成功ならコマンドタグ、失敗なら最初の
/// ErrorResponse 本文（`C`・`M` フィールド込み）を返す。NULL を含む値を渡せる。
fn run_full(
    s: &mut Stream,
    sql: &str,
    oids: &[i32],
    fmts: &[i16],
    values: &[Option<&[u8]>],
) -> Result<String, String> {
    send_length_prefixed_message(s, b'P', &parse_declared("", sql, oids));
    send_length_prefixed_message(s, b'B', &bind_msg_opt(fmts, values));
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
            b'E' if err.is_none() => err = Some(String::from_utf8_lossy(&body).to_string()),
            b'Z' => break,
            _ => {}
        }
    }
    err.map_or(Ok(tag), Err)
}

/// ErrorResponse 本文から SQLSTATE（`C` フィールド）を取り出す。
fn sqlstate_of(err_body: &str) -> String {
    err_body
        .split('\0')
        .find_map(|f| f.strip_prefix('C').map(str::to_string))
        .unwrap_or_else(|| err_body.to_string())
}

const UPD_N_BY_ID: &str = "UPDATE t SET n = $1 WHERE id = $2 USING OPERATION_ID $3";

/// nullable 列への NULL は宣言 OID 0・int4・705 のいずれでも受理され、NULL が保存される
/// （WIRE-24 (3)）。INSERT／UPDATE の両方。
#[test]
fn null_param_is_stored_as_null_for_nullable_columns() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    for (i, oid) in [0, OID_INT4, OID_UNKNOWN].into_iter().enumerate() {
        // 値を入れてから NULL へ更新する。
        let seed = format!("n-set-{i}");
        let tag = run_declared(
            &mut s,
            UPD_N_BY_ID,
            &[],
            &[0],
            &[b"7", b"1", seed.as_bytes()],
        )
        .expect("set value");
        assert_eq!(tag, "UPDATE 1");
        assert_eq!(cell(&core, "n", 1), Cell::SignedInteger(7));
        let op = format!("n-null-{i}");
        let tag = run_full(
            &mut s,
            UPD_N_BY_ID,
            &[oid, 0, 0],
            &[0],
            &[None, Some(b"1"), Some(op.as_bytes())],
        )
        .expect("null update");
        assert_eq!(tag, "UPDATE 1", "oid {oid}");
        assert_eq!(cell(&core, "n", 1), Cell::Null, "oid {oid}");

        let id = 10 + i as u64;
        let ins = format!("INSERT INTO t (id, n) VALUES ({id}, $1) USING OPERATION_ID $2");
        let op = format!("n-ins-{i}");
        let tag = run_full(&mut s, &ins, &[oid, 0], &[0], &[None, Some(op.as_bytes())])
            .expect("null insert");
        assert_eq!(tag, "INSERT 0 1", "oid {oid}");
        assert_eq!(cell(&core, "n", id), Cell::Null, "oid {oid}");
    }
    assert_alive(&mut s);
}

/// NOT NULL 列への NULL は 23502（リテラル NULL と同じ）。接続は維持される。
#[test]
fn null_param_into_not_null_column_is_23502() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    for oid in [0, OID_UNKNOWN, OID_TEXT] {
        let op = format!("nn-{oid}");
        let err = run_full(
            &mut s,
            "INSERT INTO nn (id, req) VALUES (1, $1) USING OPERATION_ID $2",
            &[oid, 0],
            &[0],
            &[None, Some(op.as_bytes())],
        )
        .expect_err("not null");
        assert_eq!(sqlstate_of(&err), "23502", "oid {oid}");
        assert!(!err.contains("tenant"), "{err:?}");
    }
    assert_alive(&mut s);
}

/// 比較の位置・`USING OPERATION_ID` の NULL パラメータは、同じ位置にリテラル NULL を書いた
/// 文と同じ SQLSTATE になる。現行の SQLSTATE を直書きせず、リテラル側へ追従する形で
/// アサートする（SQL-41 側の改善に自然に追従できる）。
#[test]
fn null_param_in_rejected_positions_matches_literal_null() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let pairs = [
        (
            "SELECT id FROM t WHERE n = $1 LIMIT 5",
            "SELECT id FROM t WHERE n = NULL LIMIT 5",
        ),
        (
            "UPDATE t SET n = 1 WHERE id = 1 USING OPERATION_ID $1",
            "UPDATE t SET n = 1 WHERE id = 1 USING OPERATION_ID NULL",
        ),
    ];
    for (param_sql, literal_sql) in pairs {
        for oid in [0, OID_INT4] {
            let p = run_full(&mut s, param_sql, &[oid], &[0], &[None]);
            let l = run_full(&mut s, literal_sql, &[], &[0], &[]);
            assert_eq!(
                p.as_ref().map_err(|e| sqlstate_of(e)),
                l.as_ref().map_err(|e| sqlstate_of(e)),
                "{param_sql} (oid {oid})"
            );
        }
        assert_alive(&mut s);
    }
}

/// 型付き NULL の判定順: 変換できない宣言は値が NULL でも Parse の段で先に拒否される
/// （42804）。binary 非対応スロットに format=1 の NULL を送ると 0A000 が先に返る。後者は
/// PostgreSQL との既知の差分（現行の fail-closed を固定するもので、仕様準拠としての固定では
/// ない。#2438・#2439 の申し送り）。
#[test]
fn typed_null_checks_declaration_before_value() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let err = run_full(
        &mut s,
        UPD_N_BY_ID,
        &[OID_TEXT, 0, 0],
        &[0],
        &[None, Some(b"1"), Some(b"tn-1")],
    )
    .expect_err("text into integer");
    assert_eq!(sqlstate_of(&err), "42804");
    // date（binary 非対応）のスロットへ format=1 の NULL。
    let err = run_full(
        &mut s,
        "UPDATE t SET n = $2 WHERE id = 1 USING OPERATION_ID $3",
        &[OID_DATE, 0, 0],
        &[1, 0, 0],
        &[None, Some(b"5"), Some(b"tn-2")],
    )
    .expect_err("binary null into unsupported slot");
    assert_eq!(sqlstate_of(&err), "0A000");
    assert_alive(&mut s);
}

/// 同じ文の中で OID 0・705・宣言 OID を混ぜても、ParameterDescription は宣言を echo し
/// 残りを推論し、実行結果が一致する。
#[test]
fn mixed_declared_zero_and_unknown_oids_are_inferred_per_slot() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let sql = "UPDATE t SET n = $1, b = $2, d = $3 WHERE id = 1 USING OPERATION_ID $4";
    let described = describe_declared(&mut s, sql, &[0, OID_UNKNOWN, OID_FLOAT8]);
    assert_eq!(described[..3], [OID_INT4, OID_INT8, OID_FLOAT8]);
    let tag = run_declared(
        &mut s,
        sql,
        &[0, OID_UNKNOWN, OID_FLOAT8],
        &[0],
        &[b"3", b"4", b"2.5", b"mix-1"],
    )
    .expect("mixed");
    assert_eq!(tag, "UPDATE 1");
    assert_eq!(cell(&core, "n", 1), Cell::SignedInteger(3));
    assert_eq!(cell(&core, "b", 1), Cell::SignedInteger(4));
    assert_eq!(cell(&core, "d", 1), Cell::Float(2.5));
}

/// 3 テナント（tenant-a: Public+Private 行、tenant-b: Private 行、tenant-c: 行なし）で、
/// 拒否の ErrorResponse 本文がバイト一致し、受理（他テナント行・存在しない行への UPDATE）の
/// コマンドタグが所有の有無だけで決まる。表名・テナント id・値は応答に出ない
/// （RLS-11・WIRE-24 (5)）。
#[test]
fn rejections_and_acceptance_are_identical_across_three_tenants() {
    let (core, _g) = new_core();
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
        ("carol", "tenant-c", "tr0ub4dor"),
    ]);
    let addr = spawn_server_with_engine(&users, core.clone());
    let mut conns = [
        authenticate_to_ready_for_query(addr, "alice", "correct-horse"),
        authenticate_to_ready_for_query(addr, "bob", "battery-staple"),
        authenticate_to_ready_for_query(addr, "carol", "tr0ub4dor"),
    ];
    let missing_sql = "SELECT id FROM no_such_table WHERE n = $1 LIMIT 5";
    let mut bodies: Vec<Vec<String>> = vec![Vec::new(); 3];
    let mut tags: Vec<Vec<String>> = vec![Vec::new(); 3];
    for (i, s) in conns.iter_mut().enumerate() {
        // Parse の段の拒否（42804・42883・0A000・42P18）。
        bodies[i].push(parse_error_text(
            s,
            "",
            "UPDATE t SET n = $1 WHERE id = 1 USING OPERATION_ID 'x'",
            &[OID_TEXT],
        ));
        bodies[i].push(parse_error_text(s, "", SELECT_N, &[OID_TEXT]));
        bodies[i].push(parse_error_text(s, "", SELECT_N, &[1007]));
        bodies[i].push(parse_error_text(s, "", EXPR, &[0, 0]));
        // 実行時の拒否（NOT NULL への NULL）。
        let op = format!("tenants-nn-{i}");
        bodies[i].push(
            run_full(
                s,
                "INSERT INTO nn (id, req) VALUES (1, $1) USING OPERATION_ID $2",
                &[0, 0],
                &[0],
                &[None, Some(op.as_bytes())],
            )
            .expect_err("not null"),
        );
        // 宣言の有無で存在しない表の応答が変わらない（宣言が存在のオラクルにならない）。
        let missing_plain =
            run_full(s, missing_sql, &[], &[0], &[Some(b"1")]).expect_err("missing table");
        let missing_declared =
            run_full(s, missing_sql, &[OID_INT4], &[0], &[Some(b"1")]).expect_err("missing table");
        assert_eq!(missing_plain, missing_declared, "tenant #{i}");
        bodies[i].push(missing_declared);
        // 受理: 他テナントの Private 行（id 5・6）と存在しない行（999）。
        for id in ["5", "6", "999"] {
            let op = format!("tenants-upd-{i}-{id}");
            let tag = run_full(
                s,
                UPD_N_BY_ID,
                &[OID_INT4, 0, 0],
                &[0],
                &[None, Some(id.as_bytes()), Some(op.as_bytes())],
            )
            .unwrap_or_else(|e| panic!("tenant #{i} id {id}: {e}"));
            tags[i].push(tag);
        }
    }
    for i in 1..3 {
        assert_eq!(
            bodies[0], bodies[i],
            "rejection bodies differ for tenant #{i}"
        );
    }
    for body in &bodies[0] {
        assert!(
            !body.contains("tenant") && !body.contains("private"),
            "{body:?}"
        );
    }
    // alice は自分の id 5 だけを 1 件更新し、他は 0 件。bob は id 6、carol はどれも 0 件。
    // 他テナントの行は存在しない行（999）と同じ「0 件」としてしか観測できない。
    assert_eq!(tags[0], vec!["UPDATE 1", "UPDATE 0", "UPDATE 0"]);
    assert_eq!(tags[1], vec!["UPDATE 0", "UPDATE 1", "UPDATE 0"]);
    assert_eq!(tags[2], vec!["UPDATE 0", "UPDATE 0", "UPDATE 0"]);
}
