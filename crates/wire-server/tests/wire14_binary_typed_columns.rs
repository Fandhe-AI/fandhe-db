//! 拡張クエリプロトコルのバイナリ形式結果を数値・真偽値・bytea・uuid 列へ拡張
//! した挙動（WIRE-14・TASK-218・Issue #1172）を、実際の wire 経路（生バイト
//! クライアント）で固定する層 A 結合テスト。
//!
//! 範囲: 結果列のバイナリ符号化（テキスト形式と同じ値へ戻せること）・非対応型
//! への `0A000` 拒否と接続維持・Bind 本文の不正な長さ／件数の `08P01`・テナント
//! 境界。バイナリ形式パラメータの復号は wire 側の `$n` 束縛（WIRE-12・#935）が
//! 未結線のため本ファイルの対象外。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{ArrayValue, Value};
use engine::storage::{Storage, Visibility};
use engine::uuid::parse_uuid_text;

use common::*;

type Stream = std::net::TcpStream;
type Rows = Vec<Vec<Option<Vec<u8>>>>;

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

/// 結果 format code 部分だけを差し替えた Bind 本文（件数は `codes.len()`）。
fn bind_with_formats(portal: &str, statement: &str, codes: &[i16]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&(codes.len() as i16).to_be_bytes());
    for c in codes {
        body.extend_from_slice(&c.to_be_bytes());
    }
    body
}

/// 宣言件数に対して本文が足りない Bind（件数 3 と宣言し 1 件分しか送らない）。
fn bind_truncated(portal: &str, statement: &str) -> Vec<u8> {
    let mut body = bind_with_formats(portal, statement, &[1]);
    let n = body.len();
    body[n - 4..n - 2].copy_from_slice(&3i16.to_be_bytes());
    body
}

fn bind_negative_count(portal: &str, statement: &str) -> Vec<u8> {
    let mut body = bind_with_formats(portal, statement, &[]);
    let n = body.len();
    body[n - 2..].copy_from_slice(&(-1i16).to_be_bytes());
    body
}

fn send_sync(stream: &mut Stream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn expect_ready(stream: &mut Stream) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z');
    assert_eq!(body, [b'I']);
}

fn expect_error_then_recover(stream: &mut Stream, sqlstate: &str) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains(sqlstate), "want {sqlstate} in {text:?}");
    send_sync(stream);
    expect_ready(stream);
}

fn parse(stream: &mut Stream, statement: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'1',
        "expected ParseComplete: {}",
        String::from_utf8_lossy(&body)
    );
}

/// `sql` を指定 format code で Parse/Bind/Describe/Execute/Sync し、
/// `(RowDescription の format code 列, DataRow の各セル)` を返す。
fn run(stream: &mut Stream, sql: &str, codes: &[i16]) -> (Vec<i16>, Rows) {
    close_all(stream);
    parse(stream, "s", sql);
    send_length_prefixed_message(stream, b'B', &bind_with_formats("p", "s", codes));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(stream, b'D', &describe_body(b'P', "p"));
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'T');
    let formats = row_description_formats(&body);
    send_length_prefixed_message(stream, b'E', &execute_body("p", 0));
    let mut rows = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => rows.push(parse_data_row(&body)),
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    send_sync(stream);
    expect_ready(stream);
    (formats, rows)
}

/// 前回の失敗などで残った portal `p`／statement `s` を閉じる（Close は
/// 未存在でも CloseComplete を返す）。
fn close_all(stream: &mut Stream) {
    for (k, n) in [(b'P', "p"), (b'S', "s")] {
        send_length_prefixed_message(stream, b'C', &describe_body(k, n));
        let (kind, _) = read_message(stream);
        assert_eq!(kind, b'3');
    }
}

fn row_description_formats(body: &[u8]) -> Vec<i16> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        while body[pos] != 0 {
            pos += 1;
        }
        pos += 1 + 4 + 2 + 4 + 2 + 4;
        out.push(i16::from_be_bytes([body[pos], body[pos + 1]]));
        pos += 2;
    }
    out
}

fn parse_data_row(body: &[u8]) -> Vec<Option<Vec<u8>>> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut cells = Vec::new();
    for _ in 0..n {
        let len = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        if len < 0 {
            cells.push(None);
        } else {
            let len = len as usize;
            cells.push(Some(body[pos..pos + len].to_vec()));
            pos += len;
        }
    }
    cells
}

const SUPPORTED_COLS: &str = "n, b, r, d, flag, blob, uid";

/// バイナリセルを PostgreSQL の text 表現へ復号する（列順は `SUPPORTED_COLS`）。
fn decode_binary(col: usize, bytes: &[u8]) -> String {
    match col {
        0 => i32::from_be_bytes(bytes.try_into().unwrap()).to_string(),
        1 => i64::from_be_bytes(bytes.try_into().unwrap()).to_string(),
        // float4 列の text 表現は PostgreSQL の float4 出力形式（Issue #1173）。
        2 => engine::scalar_float::format_real(f32::from_bits(u32::from_be_bytes(
            bytes.try_into().unwrap(),
        ))),
        3 => engine::scalar_float::format_double(f64::from_bits(u64::from_be_bytes(
            bytes.try_into().unwrap(),
        ))),
        4 => {
            assert_eq!(bytes.len(), 1);
            if bytes[0] == 1 { "t" } else { "f" }.to_string()
        }
        5 => {
            let mut s = "\\x".to_string();
            for b in bytes {
                s.push_str(&format!("{b:02x}"));
            }
            s
        }
        6 => {
            assert_eq!(bytes.len(), 16);
            let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            format!(
                "{}-{}-{}-{}-{}",
                &h[0..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            )
        }
        _ => unreachable!(),
    }
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire14-binary-typed");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string()])
        .expect("enum");
    let tags = ArrayType::new(ArrayElemType::Text, 4).expect("array type");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("blob", ColumnType::Bytea, true),
                ColumnDef::new("uid", ColumnType::Uuid, true),
                ColumnDef::new("day", ColumnType::Date, true),
                ColumnDef::new("ts", ColumnType::Timestamp, true),
                ColumnDef::new("doc", ColumnType::Json, true),
                ColumnDef::new("docb", ColumnType::Jsonb, true),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("tags", ColumnType::Array(tags), true),
                ColumnDef::new("mood", ColumnType::Enum(mood), true),
            ],
        ))
        .expect("create table");

    let insert = |tenant: &str, id: u64, n: i32, vals: Vec<Value>| {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("tenant");
        let op = OperationId::parse(&format!("op-w14-{tenant}-{id}")).expect("op id");
        let mut row = vec![Value::Vector(vec![1.0, 0.0]), Value::Integer(n)];
        row.extend(vals);
        while row.len() < 15 {
            row.push(Value::Null);
        }
        engine::tenant::insert_typed_row(&storage, "t", &ctx, id, Visibility::Private, &row, &op)
            .expect("insert");
    };
    // 1: 代表値（非対応型の列も埋める）
    insert(
        "tenant-a",
        1,
        42,
        vec![
            Value::BigInt(9_000_000_000),
            Value::Real(0.1),
            Value::Double(-2.5),
            Value::Bool(true),
            Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            Value::Uuid(parse_uuid_text("12345678-9abc-def0-1234-56789abcdef0").unwrap()),
            Value::Date(0),
            Value::Timestamp(0),
            Value::Json("{\"a\":1}".to_string()),
            Value::Json("{\"a\":1}".to_string()),
            Value::Numeric(Decimal::from_parts(12345, 2).unwrap()),
            Value::Array(ArrayValue::Text(vec![Some("a".to_string())])),
            Value::Enum("happy".to_string()),
        ],
    );
    // 2: 境界値・空 bytea・false
    insert(
        "tenant-a",
        2,
        i32::MIN,
        vec![
            Value::BigInt(i64::MIN),
            Value::Real(-3.25),
            Value::Double(1.0e300),
            Value::Bool(false),
            Value::Bytes(Vec::new()),
            Value::Uuid(parse_uuid_text("00000000-0000-0000-0000-000000000000").unwrap()),
        ],
    );
    insert("tenant-a", 3, i32::MAX, vec![Value::BigInt(i64::MAX)]);
    // 4: NULL（n 以外すべて NULL は 3 と同様。n を含む全列 NULL は下で別途）
    // 10: 他テナントの Private 行
    insert("tenant-b", 10, 777, vec![Value::BigInt(777)]);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn connect(user: &str, pass: &str) -> (Stream, temp_db::CleanupGuard, std::net::SocketAddr) {
    let (core, guard) = new_core();
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users, core);
    (
        authenticate_to_ready_for_query(addr, user, pass),
        guard,
        addr,
    )
}

/// 受入 1: 7 型（int4/int8/float4/float8/bool/bytea/uuid）の binary 結果が
/// text 形式と同じ値へ戻る。境界値・NULL を含む。
#[test]
fn seven_types_binary_round_trip_matches_text() {
    let (mut s, _g, _) = connect("alice", "correct-horse");
    for id in [1, 2, 3] {
        let sql = format!("SELECT {SUPPORTED_COLS} FROM t WHERE id = {id} LIMIT 10");
        let (tf, text_rows) = run(&mut s, &sql, &[0]);
        assert!(tf.iter().all(|f| *f == 0));
        let (bf, bin_rows) = run(&mut s, &sql, &[1]);
        assert!(
            bf.iter().all(|f| *f == 1),
            "RowDescription must advertise binary"
        );
        assert_eq!(text_rows.len(), 1, "id={id}");
        assert_eq!(bin_rows.len(), 1, "id={id}");
        for (col, (t, b)) in text_rows[0].iter().zip(bin_rows[0].iter()).enumerate() {
            match (t, b) {
                (None, None) => {}
                (Some(t), Some(b)) => assert_eq!(
                    String::from_utf8(t.clone()).unwrap(),
                    decode_binary(col, b),
                    "id={id} col={col}"
                ),
                other => panic!("null mismatch id={id} col={col}: {other:?}"),
            }
        }
    }
}

/// 固定長型の長さ（typlen と一致）と golden バイトを wire 上で固定する。
#[test]
fn binary_golden_bytes_on_wire() {
    let (mut s, _g, _) = connect("alice", "correct-horse");
    let (_, rows) = run(
        &mut s,
        &format!("SELECT {SUPPORTED_COLS} FROM t WHERE id = 2 LIMIT 10"),
        &[1],
    );
    let row = &rows[0];
    assert_eq!(row[0].as_deref(), Some(&i32::MIN.to_be_bytes()[..]));
    assert_eq!(row[1].as_deref(), Some(&i64::MIN.to_be_bytes()[..]));
    assert_eq!(
        row[2].as_deref(),
        Some(&(-3.25f32).to_bits().to_be_bytes()[..])
    );
    assert_eq!(
        row[3].as_deref(),
        Some(&1.0e300f64.to_bits().to_be_bytes()[..])
    );
    assert_eq!(row[4].as_deref(), Some(&[0u8][..]));
    assert_eq!(
        row[5].as_deref(),
        Some(&[][..]),
        "empty bytea has length 0 (not NULL)"
    );
    assert_eq!(row[6].as_deref(), Some(&[0u8; 16][..]));
}

/// 列ごとに format code を混在させると列単位で独立に効く。
#[test]
fn per_column_format_codes_apply_independently() {
    let (mut s, _g, _) = connect("alice", "correct-horse");
    let (fmts, rows) = run(
        &mut s,
        "SELECT n, b, flag FROM t WHERE id = 1 LIMIT 10",
        &[1, 0, 1],
    );
    assert_eq!(fmts, vec![1, 0, 1]);
    assert_eq!(rows[0][0].as_deref(), Some(&42i32.to_be_bytes()[..]));
    assert_eq!(rows[0][1].as_deref(), Some(&b"9000000000"[..]));
    assert_eq!(rows[0][2].as_deref(), Some(&[1u8][..]));
}

/// 受入 2: 非対応型・非対応列への binary 要求は `0A000` で当該文だけ拒否し、
/// 接続は維持される（配列を含む）。
#[test]
fn unsupported_columns_binary_request_is_0a000_and_connection_survives() {
    let (mut s, _g, _) = connect("alice", "correct-horse");
    for col in [
        "tags",
        "mood",
        "embedding",
        "price",
        "day",
        "ts",
        "doc",
        "docb",
        "id",
    ] {
        let sql = format!("SELECT {col} FROM t WHERE id = 1 LIMIT 10");
        close_all(&mut s);
        parse(&mut s, "s", &sql);
        send_length_prefixed_message(&mut s, b'B', &bind_with_formats("p", "s", &[1]));
        expect_error_then_recover(&mut s, "0A000");
        // 回復後、同じ接続でテキスト形式は通る。
        let (_, rows) = run(&mut s, &sql, &[0]);
        assert_eq!(rows.len(), 1, "col={col}");
    }
}

/// 受入 3（入力側）: Bind 本文の不正な長さ・件数は `08P01`、Sync で回復する。
#[test]
fn malformed_bind_result_format_lengths_are_08p01_and_recover() {
    let (mut s, _g, _) = connect("alice", "correct-horse");
    let sql = "SELECT n, b FROM t WHERE id = 1 LIMIT 10";
    parse(&mut s, "s", sql);
    send_length_prefixed_message(&mut s, b'B', &bind_truncated("p", "s"));
    expect_error_then_recover(&mut s, "08P01");
    send_length_prefixed_message(&mut s, b'B', &bind_negative_count("p", "s"));
    expect_error_then_recover(&mut s, "08P01");
    send_length_prefixed_message(&mut s, b'B', &bind_with_formats("p", "s", &[1, 1, 1]));
    expect_error_then_recover(&mut s, "08P01");
    let (_, rows) = run(&mut s, sql, &[1]);
    assert_eq!(rows.len(), 1);
}

/// 受入 4: バイナリ形式でも他テナントの Private 行は結果へ混ざらない（RLS-7）。
#[test]
fn binary_results_respect_tenant_boundary() {
    let (mut s, _g, _) = connect("bob", "battery-staple");
    let (_, rows) = run(&mut s, "SELECT n FROM t LIMIT 10", &[1]);
    assert_eq!(rows.len(), 1, "bob sees only his own row");
    assert_eq!(rows[0][0].as_deref(), Some(&777i32.to_be_bytes()[..]));
    let (_, rows) = run(&mut s, "SELECT n FROM t WHERE id = 1 LIMIT 10", &[1]);
    assert!(rows.is_empty(), "alice's private row must not be visible");
}
