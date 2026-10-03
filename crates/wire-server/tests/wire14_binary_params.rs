//! 拡張クエリの Bind でバイナリ形式（format code = 1）のパラメータを
//! int4／int8／float4／float8／bool／bytea／uuid へ復号する挙動（WIRE-14・
//! TASK-218・Issue #1345。ポインタ: WIRE-12・ERR-1/2/4）を、実際の wire 経路
//! （生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容: バイナリ ≡ テキスト（同じ値を text 形式で送った場合と同一の
//! 応答）・長さ不正の `08P01`・非対応スロットの `0A000`（判定順は 0A000 が先）・
//! いずれのエラーも Sync で回復して接続が維持されること・テナント境界。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

type Stream = std::net::TcpStream;
type Outcome = Result<(Vec<Vec<u8>>, String), String>;

const UID: [u8; 16] = [
    0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
];
const UID_TEXT: &str = "12345678-9abc-def0-1234-56789abcdef0";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire14-binary-params");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
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
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let sql = format!(
        "INSERT INTO t (id, embedding, n, b, flag, blob, uid) VALUES \
         (1, '[1,0]', 7, 9000000000, true, '\\x00ff10', '{UID_TEXT}') USING OPERATION_ID 'seed-1'"
    );
    let mut session = engine::sql::mode::SessionState::default();
    core.execute_sql_in_session(
        &engine::policy::PolicyContext::new("tenant-a").expect("tenant"),
        &mut session,
        &sql,
    )
    .expect("seed insert");
    (core, guard)
}

fn connect(addr: std::net::SocketAddr, user: &str) -> Stream {
    authenticate_to_ready_for_query(addr, user, "correct-horse")
}

fn spawn(core: Arc<EngineCore>) -> std::net::SocketAddr {
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "correct-horse"),
    ]);
    spawn_server_with_engine(&users, core)
}

fn parse_msg(sql: &str, declared: &[i32]) -> Vec<u8> {
    let mut body = parse_body("", sql, declared.len() as i16);
    for oid in declared {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    body
}

fn bind_msg(fmts: &[i16], values: &[Option<&[u8]>]) -> Vec<u8> {
    let mut body = vec![0u8, 0u8];
    body.extend_from_slice(&(fmts.len() as i16).to_be_bytes());
    for f in fmts {
        body.extend_from_slice(&f.to_be_bytes());
    }
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for v in values {
        match v {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

/// Parse→Bind→Execute→Sync を送り、成功なら（行, タグ）、失敗なら最初の
/// ErrorResponse の SQLSTATE を返す。Z まで読み切るため接続は継続して使える。
fn query(
    s: &mut Stream,
    sql: &str,
    declared: &[i32],
    fmts: &[i16],
    values: &[Option<&[u8]>],
) -> Outcome {
    send_length_prefixed_message(s, b'P', &parse_msg(sql, declared));
    send_length_prefixed_message(s, b'B', &bind_msg(fmts, values));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    let mut rows = Vec::new();
    let mut tag = String::new();
    let mut err: Option<String> = None;
    loop {
        let (k, body) = read_message(s);
        match k {
            b'D' => rows.push(body),
            b'C' => {
                tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'E' => {
                if err.is_none() {
                    let text = String::from_utf8_lossy(&body).to_string();
                    let code = text
                        .split('\0')
                        .find_map(|f| f.strip_prefix('C').map(str::to_string))
                        .unwrap_or(text);
                    err = Some(code);
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    rows.sort();
    match err {
        Some(e) => Err(e),
        None => Ok((rows, tag)),
    }
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

fn expect_err(o: Outcome, code: &str, s: &mut Stream) {
    assert_eq!(o.expect_err("expected error"), code);
    assert_alive(s);
}

fn i4(v: i32) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

#[test]
fn binary_equals_text_for_integer_bool_bytea_uuid() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    let hit = |o: Outcome| {
        let (rows, _) = o.expect("query ok");
        rows.len()
    };

    // int4（宣言 0・宣言 23・宣言 20）と int8（宣言 0・宣言 20・宣言 23）。
    for declared in [&[][..], &[23], &[20]] {
        let text = query(
            &mut s,
            "SELECT id FROM t WHERE n = $1 LIMIT 5",
            declared,
            &[0],
            &[Some(b"7")],
        );
        let bin_val = if declared == [20] {
            7i64.to_be_bytes().to_vec()
        } else {
            i4(7)
        };
        let bin = query(
            &mut s,
            "SELECT id FROM t WHERE n = $1 LIMIT 5",
            declared,
            &[1],
            &[Some(&bin_val)],
        );
        assert_eq!(text, bin, "int4 slot declared {declared:?}");
        assert_eq!(hit(bin), 1);
    }
    let text = query(
        &mut s,
        "SELECT id FROM t WHERE b = $1 LIMIT 5",
        &[20],
        &[0],
        &[Some(b"9000000000")],
    );
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE b = $1 LIMIT 5",
        &[20],
        &[1],
        &[Some(&9_000_000_000i64.to_be_bytes())],
    );
    assert_eq!(text, bin);
    assert_eq!(hit(bin), 1);
    // 宣言 0 の bigint 列は推論 int8。
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE b = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&9_000_000_000i64.to_be_bytes())],
    );
    assert_eq!(hit(bin), 1);

    // id スロット: 宣言 20／23 は受理、宣言 0（実効 numeric）は 0A000。
    let text = query(
        &mut s,
        "SELECT id FROM t WHERE id = $1 LIMIT 5",
        &[20],
        &[0],
        &[Some(b"1")],
    );
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE id = $1 LIMIT 5",
        &[20],
        &[1],
        &[Some(&1i64.to_be_bytes())],
    );
    assert_eq!(text, bin);
    assert_eq!(hit(bin), 1);
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE id = $1 LIMIT 5",
        &[23],
        &[1],
        &[Some(&i4(1))],
    );
    assert_eq!(hit(bin), 1);
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE id = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&i4(1))],
    );
    expect_err(o, "0A000", &mut s);

    // bool
    for (byte, txt) in [(1u8, "t"), (0u8, "f")] {
        let text = query(
            &mut s,
            "SELECT id FROM t WHERE flag = $1 LIMIT 5",
            &[],
            &[0],
            &[Some(txt.as_bytes())],
        );
        let bin = query(
            &mut s,
            "SELECT id FROM t WHERE flag = $1 LIMIT 5",
            &[],
            &[1],
            &[Some(&[byte])],
        );
        assert_eq!(text, bin);
    }
    assert_eq!(
        hit(query(
            &mut s,
            "SELECT id FROM t WHERE flag = $1 LIMIT 5",
            &[],
            &[1],
            &[Some(&[1])]
        )),
        1
    );

    // bytea（NUL を含む値はテキスト経路では表現できないが、バイナリでは正しく束縛される）
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE blob = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&[0x00, 0xff, 0x10])],
    );
    let text = query(
        &mut s,
        "SELECT id FROM t WHERE blob = $1 LIMIT 5",
        &[],
        &[0],
        &[Some(b"\\x00ff10")],
    );
    assert_eq!(text, bin);
    assert_eq!(hit(bin), 1);
    let miss = query(
        &mut s,
        "SELECT id FROM t WHERE blob = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&[0x00, 0xff])],
    );
    assert_eq!(hit(miss), 0);

    // uuid（16 バイトを UTF-8 として誤解釈しない）
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE uid = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&UID)],
    );
    let text = query(
        &mut s,
        "SELECT id FROM t WHERE uid = $1 LIMIT 5",
        &[],
        &[0],
        &[Some(UID_TEXT.as_bytes())],
    );
    assert_eq!(text, bin);
    assert_eq!(hit(bin), 1);
}

#[test]
fn binary_float_matches_text_outcome() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    // 現状の engine は REAL／DOUBLE 列への文字列リテラルを拒否する（型付き束縛は
    // 別 Issue）。バイナリは同じ SQLSTATE（または同じ結果）になることを固定する。
    let text = query(
        &mut s,
        "SELECT id FROM t WHERE d = $1 LIMIT 5",
        &[],
        &[0],
        &[Some(b"1.5")],
    );
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE d = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&1.5f64.to_be_bytes())],
    );
    assert_eq!(text, bin);
    let text = query(
        &mut s,
        "SELECT id FROM t WHERE r = $1 LIMIT 5",
        &[],
        &[0],
        &[Some(b"0.25")],
    );
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE r = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&0.25f32.to_be_bytes())],
    );
    assert_eq!(text, bin);
    // 長さ不正は宣言が許容する型に対して 08P01。
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE d = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&0.25f32.to_be_bytes())],
    );
    expect_err(o, "08P01", &mut s);
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE r = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&1.5f64.to_be_bytes())],
    );
    expect_err(o, "08P01", &mut s);
    // 互換表外（Double 列への宣言 700）は 0A000。
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE d = $1 LIMIT 5",
        &[700],
        &[1],
        &[Some(&0.25f32.to_be_bytes())],
    );
    expect_err(o, "0A000", &mut s);
}

#[test]
fn wrong_lengths_are_08p01_and_connection_survives() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    let cases: [(&str, Vec<u8>); 8] = [
        ("SELECT id FROM t WHERE n = $1 LIMIT 5", vec![0; 3]),
        ("SELECT id FROM t WHERE n = $1 LIMIT 5", vec![0; 5]),
        ("SELECT id FROM t WHERE b = $1 LIMIT 5", vec![0; 4]),
        ("SELECT id FROM t WHERE flag = $1 LIMIT 5", vec![]),
        ("SELECT id FROM t WHERE flag = $1 LIMIT 5", vec![0; 2]),
        ("SELECT id FROM t WHERE uid = $1 LIMIT 5", vec![0; 15]),
        ("SELECT id FROM t WHERE uid = $1 LIMIT 5", vec![0; 17]),
        ("SELECT id FROM t WHERE id = $1 LIMIT 5", vec![0; 4]),
    ];
    for (sql, val) in cases {
        let declared: &[i32] = if sql.contains("WHERE id = $1") {
            &[20]
        } else {
            &[]
        };
        let o = query(&mut s, sql, declared, &[1], &[Some(&val)]);
        expect_err(o, "08P01", &mut s);
    }
}

#[test]
fn unsupported_slots_are_0a000_and_checked_before_lengths() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    for sql in [
        "SELECT id FROM t WHERE price = $1 LIMIT 5",
        "SELECT id FROM t WHERE day = $1 LIMIT 5",
    ] {
        let o = query(&mut s, sql, &[], &[1], &[Some(b"1")]);
        expect_err(o, "0A000", &mut s);
    }
    // 宣言 int2 は互換表外。
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE n = $1 LIMIT 5",
        &[21],
        &[1],
        &[Some(&[0, 7])],
    );
    expect_err(o, "0A000", &mut s);
    // 非対応スロット（price）と長さ不正スロット（n）が混在 → 0A000 が先。
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE n = $1 AND price = $2 LIMIT 5",
        &[],
        &[1, 1],
        &[Some(&[0, 1]), Some(b"1")],
    );
    expect_err(o, "0A000", &mut s);
    // NULL は format に関わらず従来どおり 22000。
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE n = $1 LIMIT 5",
        &[],
        &[1],
        &[None],
    );
    expect_err(o, "22000", &mut s);
}

#[test]
fn tenant_boundary_holds_for_binary_params() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut alice = connect(addr, "alice");
    let mut bob = connect(addr, "bob");
    let a = query(
        &mut alice,
        "SELECT id FROM t WHERE uid = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&UID)],
    );
    assert_eq!(a.expect("alice").0.len(), 1);
    let b = query(
        &mut bob,
        "SELECT id FROM t WHERE uid = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&UID)],
    );
    assert_eq!(b.expect("bob").0.len(), 0);
    let o = query(
        &mut bob,
        "SELECT id FROM t WHERE uid = $1 LIMIT 5",
        &[],
        &[1],
        &[Some(&[0u8; 3])],
    );
    expect_err(o, "08P01", &mut bob);
}
