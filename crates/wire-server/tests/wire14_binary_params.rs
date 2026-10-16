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
                ColumnDef::new("s", ColumnType::SmallInt, true),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("blob", ColumnType::Bytea, true),
                ColumnDef::new("uid", ColumnType::Uuid, true),
                ColumnDef::new("day", ColumnType::Date, true),
                ColumnDef::new("tm", ColumnType::Time, true),
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
        "INSERT INTO t (id, embedding, n, s, b, r, d, flag, blob, uid) VALUES \
         (1, '[1,0]', 7, 300, 9000000000, 0.25, 1.5, true, '\\x00ff10', '{UID_TEXT}') USING OPERATION_ID 'seed-1'"
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
    let rows_of = |o: &Outcome| o.as_ref().expect("query ok").0.len();
    // Issue #1406: REAL／DOUBLE は型付き束縛が成立するため、バイナリはテキスト形式と
    // 同一の結果になることに加え、挿入済みの行が実際にヒットすることを固定する。
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
    assert_eq!(rows_of(&bin), 1);
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
    assert_eq!(rows_of(&bin), 1);
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
    // 宣言 int2 は拡大方向の変換として INTEGER スロットで受理する（TABLE-30・Issue #1561）。
    let text = query(
        &mut s,
        "SELECT id FROM t WHERE n = $1 LIMIT 5",
        &[21],
        &[0],
        &[Some(b"7")],
    );
    let bin = query(
        &mut s,
        "SELECT id FROM t WHERE n = $1 LIMIT 5",
        &[21],
        &[1],
        &[Some(&[0, 7])],
    );
    assert_eq!(text, bin);
    assert_eq!(bin.expect("int2 into integer").0.len(), 1);
    // 互換表に載らない宣言 OID（float4）は INTEGER スロットで 0A000 のまま。
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE n = $1 LIMIT 5",
        &[700],
        &[1],
        &[Some(&[0, 0, 0, 7])],
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
    // NULL は値が無いため format に関わらず 22000 では拒否されず、リテラル `n = NULL` と
    // 同じ規則（42703）になる（WIRE-24・Issue #2439）。
    let o = query(
        &mut s,
        "SELECT id FROM t WHERE n = $1 LIMIT 5",
        &[],
        &[1],
        &[None],
    );
    expect_err(o, "42703", &mut s);
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

/// SMALLINT スロット（TABLE-30・WIRE-12/14・Issue #1561）: int2 バイナリ ≡ テキスト、
/// 長さ不正は 08P01、より広い宣言 OID の範囲外値は engine が 22003 で拒否し副作用なし。
#[test]
fn smallint_slot_binary_params() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    let sql = "SELECT id FROM t WHERE s = $1 LIMIT 5";
    // 宣言 0（推論 int2）・宣言 21 の int2 バイナリは text と同一応答。
    for declared in [&[][..], &[21]] {
        let text = query(&mut s, sql, declared, &[0], &[Some(b"300")]);
        let bin = query(&mut s, sql, declared, &[1], &[Some(&300i16.to_be_bytes())]);
        assert_eq!(text, bin, "declared {declared:?}");
        assert_eq!(bin.expect("hit").0.len(), 1);
    }
    // 負値・極値も復号できる（ヒットしないが応答は text と一致）。
    for v in [i16::MIN, -1, i16::MAX] {
        let text = query(&mut s, sql, &[21], &[0], &[Some(v.to_string().as_bytes())]);
        let bin = query(&mut s, sql, &[21], &[1], &[Some(&v.to_be_bytes())]);
        assert_eq!(text, bin, "v={v}");
    }
    // 宣言 int4 の範囲内の値は text と同一応答。
    let text = query(&mut s, sql, &[23], &[0], &[Some(b"300")]);
    let bin = query(&mut s, sql, &[23], &[1], &[Some(&i4(300))]);
    assert_eq!(text, bin);
    // 長さ不正（1 バイト・3 バイト）は 08P01 で接続は維持される。
    for bad in [&[1u8][..], &[0, 0, 1][..]] {
        let o = query(&mut s, sql, &[21], &[1], &[Some(bad)]);
        expect_err(o, "08P01", &mut s);
    }
    // 範囲外の広い値の INSERT は 22003・副作用なし（宣言 int4／int8）。
    let ins = "INSERT INTO t (id, embedding, s) VALUES (2, '[0,1]', $1) USING OPERATION_ID 'oor-1'";
    let o = query(&mut s, ins, &[23], &[1], &[Some(&i4(32768))]);
    expect_err(o, "22003", &mut s);
    let o = query(
        &mut s,
        ins,
        &[20],
        &[1],
        &[Some(&(-32769i64).to_be_bytes())],
    );
    expect_err(o, "22003", &mut s);
    let o = query(&mut s, "SELECT id FROM t LIMIT 5", &[], &[], &[]);
    assert_eq!(o.expect("count").0.len(), 1, "no side effect");
}

/// TIME スロットのバイナリ param（TABLE-32・WIRE-14・Issue #1571）。バイナリ ≡ テキスト、
/// 長さ不正 `08P01`、範囲外 `22008`（丸めず副作用なし）、宣言 OID 不一致 `0A000`、
/// テキストの書式不正 `22007`・範囲外 `22008` の回帰、テナント境界。
#[test]
fn time_slot_binary_params() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core.clone()), "alice");
    let sel = "SELECT id FROM t WHERE tm IS NOT NULL LIMIT 20";
    // バイナリ INSERT ≡ テキスト INSERT（宣言 0・宣言 1083。境界値を含む）。
    let cases: [(i64, &str); 3] = [
        (0, "00:00:00"),
        (86_399_999_999, "23:59:59.999999"),
        (86_400_000_000, "24:00:00"),
    ];
    for (i, (micros, text)) in cases.iter().enumerate() {
        for (j, declared) in [&[][..], &[1083]].into_iter().enumerate() {
            let id_bin = 100 + (i * 10 + j * 2) as i32;
            let id_txt = id_bin + 1;
            let ins = |id: i32| {
                format!(
                    "INSERT INTO t (id, embedding, tm) VALUES ({id}, '[0,1]', $1) USING OPERATION_ID 'tm-{id}'"
                )
            };
            let o = query(
                &mut s,
                &ins(id_bin),
                declared,
                &[1],
                &[Some(&micros.to_be_bytes())],
            );
            assert!(o.is_ok(), "binary insert {micros}: {o:?}");
            let o = query(
                &mut s,
                &ins(id_txt),
                declared,
                &[0],
                &[Some(text.as_bytes())],
            );
            assert!(o.is_ok(), "text insert {text}: {o:?}");
            // 同じ値で読み戻した text 表現が一致する。
            let mut by = |id: i32| {
                query(
                    &mut s,
                    &format!("SELECT tm FROM t WHERE id = {id} LIMIT 1"),
                    &[],
                    &[],
                    &[],
                )
            };
            assert_eq!(by(id_bin), by(id_txt), "micros {micros}");
        }
    }
    let before = query(&mut s, sel, &[], &[], &[]).expect("rows").0.len();
    // 長さ不正（7・9 バイト）は 08P01。接続は維持される。
    let ins =
        "INSERT INTO t (id, embedding, tm) VALUES (900, '[0,1]', $1) USING OPERATION_ID 'tm-bad'";
    for bad in [&[0u8; 7][..], &[0u8; 9][..]] {
        let o = query(&mut s, ins, &[1083], &[1], &[Some(bad)]);
        expect_err(o, "08P01", &mut s);
    }
    // 範囲外（丸めて受理しない）は 22008・副作用なし。
    for bad in [-1i64, 86_400_000_001, i64::MIN, i64::MAX] {
        let o = query(&mut s, ins, &[1083], &[1], &[Some(&bad.to_be_bytes())]);
        expect_err(o, "22008", &mut s);
    }
    // 宣言 OID 不一致のバイナリは 0A000。
    for declared in [20, 1114, 25] {
        let o = query(&mut s, ins, &[declared], &[1], &[Some(&0i64.to_be_bytes())]);
        expect_err(o, "0A000", &mut s);
    }
    // テキスト形式の書式不正は 22007、範囲外は 22008（engine 経路の回帰）。
    let o = query(&mut s, ins, &[], &[0], &[Some(b"not-a-time")]);
    expect_err(o, "22007", &mut s);
    let o = query(&mut s, ins, &[], &[0], &[Some(b"24:00:00.000001")]);
    expect_err(o, "22008", &mut s);
    let after = query(&mut s, sel, &[], &[], &[]).expect("rows").0.len();
    assert_eq!(before, after, "no side effect");
    // テナント境界: bob から alice の TIME 行は見えない。
    let mut b = connect(spawn(core), "bob");
    let o = query(&mut b, sel, &[], &[], &[]);
    assert_eq!(o.expect("bob rows").0.len(), 0);
}
