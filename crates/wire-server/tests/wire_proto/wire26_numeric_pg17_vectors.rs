//! numeric（OID 1700）のバイナリ送受信を PostgreSQL 17 の固定ベクタと照合する層 A
//! 結合テスト（WIRE-26・WIRE-14・TABLE-13。TASK-339・Issue #2444。ポインタ:
//! `docs/spec/04-behavior/wire-protocol.md`）。
//!
//! 役割: #2442（結果列のバイナリ符号化）・#2443（パラメータのバイナリ復号）の実装が、
//! PG17 の `numeric_send`／受信検証とバイト単位・SQLSTATE 単位で整合することを固定する。
//! 結果側は全ベクタが PG17 とバイト一致する。パラメータ側の境界は、PG17 と一致する行と
//! WIRE-26 の期待（長さ不正は `08P01`・アロケーション前の `ndigits` 上限・NaN／Infinity は
//! テキスト形式と同じコード）に従って意図的に異なる行を表で持ち、差分は `diff` フラグと
//! PG17 実測コードで明示する（本リポの SQLSTATE を PG に合わせる変更は本テストの対象外）。
//!
//! 採取環境: PostgreSQL 17.11（Debian 17.11-1.pgdg13+2・x86_64）。
//! 再採取手順（スクリプトはリポに置かない）:
//! - 結果側: `docker run --rm -d -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17` の後、
//!   `psql -U postgres -At -c "select encode(numeric_send('<v>'::numeric(p,s)),'hex')"`。
//!   集計は `select encode(numeric_send(avg(x)),'hex') from (values ...) t(x)` で取る。
//! - パラメータ側: psycopg 3 の Dumper（`format = Format.BINARY`・`oid = 1700`・`dump` が
//!   生バイトを返す）で `select %s::numeric(p,s)::text` へ Bind し、`sqlstate` を記録する。
//!
//! 層構成: API 層（エンコーダ単体）・wire 層の結果側・パラメータ往復・境界の SQLSTATE・
//! テナント境界。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::{parse_for_column, parse_literal_exact};
use engine::policy::PolicyContext;
use engine::storage::Storage;
use wire_server::result_encoder::binary;

use common::*;

type Stream = std::net::TcpStream;

const TABLE: &str = "numvec";

/// 1 ベクタ = 1 行（NUMERIC(p,s) 列の `numeric_send` 実測）。
struct ColVec {
    /// 行 id（`100 + 通し番号`）。
    id: u64,
    col: &'static str,
    precision: u8,
    scale: u8,
    /// PG17 へ与えた入力テキスト。
    input: String,
    /// PG17 の `numeric_out`（列の scale へ丸めた後）。
    text: String,
    hex: &'static str,
}

fn nines(n: usize) -> String {
    "9".repeat(n)
}

type ColRow = (&'static str, u8, u8, String, String, &'static str);

fn col_vectors() -> Vec<ColVec> {
    let zeros37 = "0".repeat(37);
    let rows: Vec<ColRow> = vec![
        ("c52", 5, 2, "0".into(), "0.00".into(), "0000000000000002"),
        (
            "c52",
            5,
            2,
            "123.45".into(),
            "123.45".into(),
            "0002000000000002007b1194",
        ),
        (
            "c52",
            5,
            2,
            "-123.45".into(),
            "-123.45".into(),
            "0002000040000002007b1194",
        ),
        (
            "c52",
            5,
            2,
            "0.01".into(),
            "0.01".into(),
            "0001ffff000000020064",
        ),
        (
            "c52",
            5,
            2,
            "-0.5".into(),
            "-0.50".into(),
            "0001ffff400000021388",
        ),
        (
            "c52",
            5,
            2,
            "1".into(),
            "1.00".into(),
            "00010000000000020001",
        ),
        (
            "c52",
            5,
            2,
            "999.99".into(),
            "999.99".into(),
            "000200000000000203e726ac",
        ),
        (
            "c52",
            5,
            2,
            "123.456".into(),
            "123.46".into(),
            "0002000000000002007b11f8",
        ),
        (
            "c52",
            5,
            2,
            "-0.00".into(),
            "0.00".into(),
            "0000000000000002",
        ),
        (
            "c21",
            2,
            1,
            "0.1".into(),
            "0.1".into(),
            "0001ffff0000000103e8",
        ),
        (
            "c53",
            5,
            3,
            "1.234".into(),
            "1.234".into(),
            "000200000000000300010924",
        ),
        (
            "c65",
            6,
            5,
            "1.00001".into(),
            "1.00001".into(),
            "00030000000000050001000003e8",
        ),
        (
            "c54",
            5,
            4,
            "0.005".into(),
            "0.0050".into(),
            "0001ffff000000040032",
        ),
        (
            "c61",
            6,
            1,
            "99999.9".into(),
            "99999.9".into(),
            "00030001000000010009270f2328",
        ),
        (
            "c100",
            10,
            0,
            "10000".into(),
            "10000".into(),
            "00010001000000000001",
        ),
        (
            "c72",
            7,
            2,
            "10000".into(),
            "10000.00".into(),
            "00010001000000020001",
        ),
        (
            "c134",
            13,
            4,
            "100000000.0001".into(),
            "100000000.0001".into(),
            "00040002000000040001000000000001",
        ),
        (
            "c380",
            38,
            0,
            nines(38),
            nines(38),
            "000a0009000000000063270f270f270f270f270f270f270f270f270f",
        ),
        (
            "c380",
            38,
            0,
            format!("-{}", nines(38)),
            format!("-{}", nines(38)),
            "000a0009400000000063270f270f270f270f270f270f270f270f270f",
        ),
        (
            "c3838",
            38,
            38,
            format!("0.{zeros37}1"),
            format!("0.{zeros37}1"),
            "0001fff6000000260064",
        ),
        (
            "c3838",
            38,
            38,
            format!("-0.{}", nines(38)),
            format!("-0.{}", nines(38)),
            "000affff40000026270f270f270f270f270f270f270f270f270f26ac",
        ),
        (
            "c3837",
            38,
            37,
            format!("9.{}", nines(37)),
            format!("9.{}", nines(37)),
            "000b0000000000250009270f270f270f270f270f270f270f270f270f2328",
        ),
        (
            "c3818",
            38,
            18,
            "12345678901234567890.123456789012345678".into(),
            "12345678901234567890.123456789012345678".into(),
            "000a00040000001204d2162e23340d801ed204d2162e23340d801e78",
        ),
        (
            "c190",
            19,
            0,
            "9223372036854775807".into(),
            "9223372036854775807".into(),
            "0005000400000000039a0d2c0170156516af",
        ),
        (
            "c190",
            19,
            0,
            "-9223372036854775808".into(),
            "-9223372036854775808".into(),
            "0005000440000000039a0d2c0170156516b0",
        ),
    ];
    rows.into_iter()
        .enumerate()
        .map(|(i, (col, precision, scale, input, text, hex))| ColVec {
            id: 101 + i as u64,
            col,
            precision,
            scale,
            input,
            text,
            hex,
        })
        .collect()
}

/// `id` 列（符号なし 64bit・dscale 0）のベクタ。`numvec` に同居させる 0 と 1。
const ID_VECTORS: &[(u64, &str)] = &[(0, "0000000000000000"), (1, "00010000000000000001")];

/// u64::MAX の id は、同じ表に行があると `WHERE id = ..` の比較が `22003`（厳密比較できる
/// 範囲外）になるため、専用表 `numid` に 1 行だけ置いて `LIMIT` つき全件読みで検証する。
const ID_MAX_TABLE: &str = "numid";
const ID_MAX_VECTOR: (u64, &str) = (u64::MAX, "000500040000000007341a5802e103bb064f");

/// 集計の種別。AvgN = AVG(n)、SumB = SUM(b)、AvgB = AVG(b)、SumC = SUM(c52)。
#[derive(Clone, Copy)]
enum Agg {
    AvgN,
    SumB,
    AvgB,
    SumC,
}

impl Agg {
    fn sql(self, g: i32) -> String {
        let expr = match self {
            Agg::AvgN => "AVG(n)",
            Agg::SumB => "SUM(b)",
            Agg::AvgB => "AVG(b)",
            Agg::SumC => "SUM(c52)",
        };
        format!("SELECT {expr} FROM {TABLE} WHERE g = {g}")
    }
}

/// 集計ベクタ: `(グループ g, 式, PG17 テキスト, PG17 hex)`。
const AGG_VECTORS: &[(i32, Agg, &str, &str)] = &[
    (
        29,
        Agg::AvgN,
        "1.5000000000000000",
        "000200000000001000011388",
    ),
    (
        30,
        Agg::AvgN,
        "2.3333333333333333",
        "000500000000001000020d050d050d050d05",
    ),
    (31, Agg::AvgN, "7.0000000000000000", "00010000000000100007"),
    (
        32,
        Agg::AvgN,
        "-1.5000000000000000",
        "000200004000001000011388",
    ),
    (
        33,
        Agg::AvgN,
        "13.6666666666666667",
        "0005000000000010000d1a0a1a0a1a0a1a0b",
    ),
    (34, Agg::SumB, "18000000000", "000100020000000000b4"),
    (
        35,
        Agg::SumB,
        "9223372036854775808",
        "0005000400000000039a0d2c0170156516b0",
    ),
    (
        36,
        Agg::AvgB,
        "-4611686013927387904",
        "000500044000000001cd0696008b0ab21ee0",
    ),
    (37, Agg::SumB, "8999999999", "00030002000000000059270f270f"),
    (
        38,
        Agg::AvgB,
        "2999999999.66666667",
        "0005000200000008001d270f270f1a0a1a0b",
    ),
    (39, Agg::SumC, "124.00", "0001000000000002007c"),
];

type AggRow = (i32, Option<i32>, Option<i64>, Option<&'static str>);

/// グループごとの入力行 `(g, n, b, c52)`（PG17 で採取した入力と同じ）。
fn agg_rows() -> Vec<AggRow> {
    let mut v: Vec<AggRow> = Vec::new();
    for n in [1, 2] {
        v.push((29, Some(n), None, None));
    }
    for n in [1, 2, 4] {
        v.push((30, Some(n), None, None));
    }
    v.push((31, Some(7), None, None));
    for n in [-1, -2] {
        v.push((32, Some(n), None, None));
    }
    for n in [i32::MAX, i32::MIN, 42] {
        v.push((33, Some(n), None, None));
    }
    for b in [9_000_000_000i64, 9_000_000_000] {
        v.push((34, None, Some(b), None));
    }
    for b in [i64::MAX, 1] {
        v.push((35, None, Some(b), None));
    }
    for b in [9_000_000_000i64, i64::MIN] {
        v.push((36, None, Some(b), None));
    }
    for g in [37, 38] {
        for b in [9_000_000_000i64, i64::MIN, i64::MAX] {
            v.push((g, None, Some(b), None));
        }
    }
    for c in ["123.45", "0.55"] {
        v.push((39, None, None, Some(c)));
    }
    v
}

fn unhex(h: &str) -> Vec<u8> {
    assert_eq!(h.len() % 2, 0, "odd hex length");
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

fn tenant_a() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("tenant a")
}

fn tenant_b() -> PolicyContext {
    PolicyContext::new("tenant-b").expect("tenant b")
}

fn numeric_col(name: &str, precision: u8, scale: u8) -> ColumnDef {
    ColumnDef::new(name, ColumnType::Numeric { precision, scale }, true)
}

/// 一時 DB・スキーマ・シード（tenant-a に全ベクタ、tenant-b に 1 行）を用意する。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire26-numeric-pg17");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mut cols = vec![
        ColumnDef::new("embedding", ColumnType::Vector(2), false),
        ColumnDef::new("g", ColumnType::Integer, true),
        ColumnDef::new("n", ColumnType::Integer, true),
        ColumnDef::new("b", ColumnType::BigInt, true),
    ];
    for (name, p, s) in [
        ("c52", 5, 2),
        ("c21", 2, 1),
        ("c53", 5, 3),
        ("c65", 6, 5),
        ("c54", 5, 4),
        ("c61", 6, 1),
        ("c100", 10, 0),
        ("c72", 7, 2),
        ("c134", 13, 4),
        ("c380", 38, 0),
        ("c3838", 38, 38),
        ("c3837", 38, 37),
        ("c3818", 38, 18),
        ("c190", 19, 0),
    ] {
        cols.push(numeric_col(name, p, s));
    }
    storage
        .create_table(&TableSchema::new(TABLE, cols))
        .expect("create table");
    storage
        .create_table(&TableSchema::new(
            ID_MAX_TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create numid");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut session = engine::sql::mode::SessionState::default();
    let mut run = |ctx: &PolicyContext, sql: String| {
        core.execute_sql_in_session(ctx, &mut session, &sql)
            .unwrap_or_else(|e| panic!("seed failed: {sql}: {e:?}"));
    };
    for v in col_vectors() {
        run(
            &tenant_a(),
            format!(
                "INSERT INTO {TABLE} (id, embedding, {col}) VALUES ({id}, '[0,1]', '{input}') \
                 USING OPERATION_ID 'seed-col-{id}'",
                col = v.col,
                id = v.id,
                input = v.input
            ),
        );
    }
    storage_max_id_table(&core);
    for (id, _) in ID_VECTORS {
        run(
            &tenant_a(),
            format!(
                "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[0,1]') \
                 USING OPERATION_ID 'seed-id-{id}'"
            ),
        );
    }
    for (i, (g, n, b, c)) in agg_rows().into_iter().enumerate() {
        let id = 2000 + i as u64;
        let lit = |o: Option<String>| o.unwrap_or_else(|| "NULL".into());
        run(
            &tenant_a(),
            format!(
                "INSERT INTO {TABLE} (id, embedding, g, n, b, c52) VALUES \
                 ({id}, '[0,1]', {g}, {n}, {b}, {c}) USING OPERATION_ID 'seed-agg-{id}'",
                n = lit(n.map(|v| v.to_string())),
                b = lit(b.map(|v| v.to_string())),
                c = lit(c.map(|v| format!("'{v}'"))),
            ),
        );
    }
    run(
        &tenant_b(),
        format!(
            "INSERT INTO {TABLE} (id, embedding, g, n, b, c52) VALUES \
             (5000, '[0,1]', 29, 5, 1, '1.00') USING OPERATION_ID 'seed-bob'"
        ),
    );
    (core, guard)
}

/// u64::MAX の id を専用表 `numid`（`new_core` が作成）へ 1 行だけ入れる（`ID_MAX_TABLE` の理由を参照）。
fn storage_max_id_table(core: &EngineCore) {
    let mut session = engine::sql::mode::SessionState::default();
    core.execute_sql_in_session(
        &tenant_a(),
        &mut session,
        &format!(
            "INSERT INTO {ID_MAX_TABLE} (id, embedding) VALUES ({}, '[0,1]') \
             USING OPERATION_ID 'seed-id-max'",
            ID_MAX_VECTOR.0
        ),
    )
    .expect("seed numid");
}

fn spawn(core: Arc<EngineCore>) -> std::net::SocketAddr {
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "correct-horse"),
    ]);
    spawn_server_with_engine(&users, core)
}

fn connect(addr: std::net::SocketAddr, user: &str) -> Stream {
    authenticate_to_ready_for_query(addr, user, "correct-horse")
}

/// Parse→Bind→Describe(portal)→Execute→Sync の応答。
#[derive(Debug, Default)]
struct Resp {
    /// RowDescription の `(型 OID, format code)`。
    fields: Vec<(i32, i16)>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
    /// 最初の ErrorResponse の `(SQLSTATE, メッセージ)`。
    err: Option<(String, String)>,
}

fn parse_msg(sql: &str, declared: &[i32]) -> Vec<u8> {
    let mut body = parse_body("", sql, declared.len() as i16);
    for oid in declared {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    body
}

/// パラメータ・結果の両方の format code を指定できる Bind 本文（無名 portal／statement）。
fn bind_msg(pfmts: &[i16], values: &[Option<&[u8]>], rfmts: &[i16]) -> Vec<u8> {
    let mut body = vec![0u8, 0u8];
    body.extend_from_slice(&(pfmts.len() as i16).to_be_bytes());
    for f in pfmts {
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
    body.extend_from_slice(&(rfmts.len() as i16).to_be_bytes());
    for f in rfmts {
        body.extend_from_slice(&f.to_be_bytes());
    }
    body
}

fn parse_row_description(body: &[u8]) -> Vec<(i32, i16)> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        while body[pos] != 0 {
            pos += 1;
        }
        pos += 1 + 4 + 2; // NUL・table oid・column attr
        let oid = i32::from_be_bytes(body[pos..pos + 4].try_into().expect("oid"));
        pos += 4 + 2 + 4; // type oid・typlen・typmod
        let fmt = i16::from_be_bytes([body[pos], body[pos + 1]]);
        pos += 2;
        out.push((oid, fmt));
    }
    out
}

fn parse_data_row(body: &[u8]) -> Vec<Option<Vec<u8>>> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut cells = Vec::new();
    for _ in 0..n {
        let len = i32::from_be_bytes(body[pos..pos + 4].try_into().expect("len"));
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

fn exec(
    s: &mut Stream,
    sql: &str,
    declared: &[i32],
    pfmts: &[i16],
    values: &[Option<&[u8]>],
    rfmts: &[i16],
) -> Resp {
    send_length_prefixed_message(s, b'P', &parse_msg(sql, declared));
    send_length_prefixed_message(s, b'B', &bind_msg(pfmts, values, rfmts));
    send_length_prefixed_message(s, b'D', b"P\0");
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    let mut resp = Resp::default();
    loop {
        let (k, body) = read_message(s);
        match k {
            b'T' => resp.fields = parse_row_description(&body),
            b'D' => resp.rows.push(parse_data_row(&body)),
            b'E' => {
                if resp.err.is_none() {
                    let mut code = String::new();
                    let mut msg = String::new();
                    for f in body.split(|b| *b == 0) {
                        match f.split_first() {
                            Some((b'C', rest)) => code = String::from_utf8_lossy(rest).into(),
                            Some((b'M', rest)) => msg = String::from_utf8_lossy(rest).into(),
                            _ => {}
                        }
                    }
                    resp.err = Some((code, msg));
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    resp
}

/// 単一セル（1 行 1 列）の SELECT を指定の結果 format で実行して値を取り出す。
fn one_cell(s: &mut Stream, sql: &str, rfmt: i16) -> (Resp, Vec<u8>) {
    let r = exec(s, sql, &[], &[], &[], &[rfmt]);
    assert!(r.err.is_none(), "{sql}: {:?}", r.err);
    assert_eq!(r.rows.len(), 1, "{sql}: want exactly 1 row");
    let cell = r.rows[0][0]
        .clone()
        .unwrap_or_else(|| panic!("{sql}: NULL"));
    (r, cell)
}

fn assert_numeric_field(r: &Resp, fmt: i16, ctx: &str) {
    assert_eq!(r.fields, vec![(1700, fmt)], "{ctx}: RowDescription");
}

/// 結果側の 1 ベクタ: binary はバイト完全一致、text は PG17 の `numeric_out` と一致。
fn assert_wire_vector(s: &mut Stream, sql: &str, text: &str, hex: &str) {
    let (r, bin) = one_cell(s, sql, 1);
    assert_numeric_field(&r, 1, sql);
    assert_eq!(bin, unhex(hex), "{sql}: numeric_send bytes");
    let (r, txt) = one_cell(s, sql, 0);
    assert_numeric_field(&r, 0, sql);
    assert_eq!(String::from_utf8(txt).expect("utf8"), text, "{sql}: text");
}

fn encode(neg: bool, mag: u128, scale: u8) -> Vec<u8> {
    let (buf, n) = binary::numeric(neg, mag, scale).expect("encode");
    buf[..n].to_vec()
}

/// API 層: 列の丸め・集計の結果テキストを `Decimal` へ読み、エンコーダ単体が PG17 の
/// `numeric_send` とバイト一致することを確かめる（scale の導出は wire 層のテストが担う）。
#[test]
fn pg17_numeric_send_vectors_match_encoder_api() {
    for v in col_vectors() {
        let d = parse_for_column(&v.input, v.precision, v.scale).expect("parse");
        let u = d.unscaled();
        assert_eq!(
            encode(u < 0, u.unsigned_abs(), d.scale()),
            unhex(v.hex),
            "{} {}",
            v.col,
            v.input
        );
    }
    for (v, hex) in ID_VECTORS.iter().chain([&ID_MAX_VECTOR]) {
        assert_eq!(encode(false, u128::from(*v), 0), unhex(hex), "id {v}");
    }
    for (g, _, text, hex) in AGG_VECTORS {
        let d = parse_literal_exact(text).expect("parse");
        let u = d.unscaled();
        assert_eq!(
            encode(u < 0, u.unsigned_abs(), d.scale()),
            unhex(hex),
            "g={g} {text}"
        );
    }
}

/// wire 層（結果側）: 列・id・集計の binary セルが PG17 の `numeric_send` と完全一致し、
/// 同じ値の text 結果が PG17 の `numeric_out` と一致する。
#[test]
fn pg17_numeric_send_vectors_match_on_wire() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    for v in col_vectors() {
        let sql = format!("SELECT {} FROM {TABLE} WHERE id = {} LIMIT 1", v.col, v.id);
        assert_wire_vector(&mut s, &sql, &v.text, v.hex);
    }
    for (id, hex) in ID_VECTORS {
        let sql = format!("SELECT id FROM {TABLE} WHERE id = {id} LIMIT 10");
        assert_wire_vector(&mut s, &sql, &id.to_string(), hex);
    }
    let (max_id, max_hex) = ID_MAX_VECTOR;
    let sql = format!("SELECT id FROM {ID_MAX_TABLE} LIMIT 10");
    assert_wire_vector(&mut s, &sql, &max_id.to_string(), max_hex);
    for (g, agg, text, hex) in AGG_VECTORS {
        assert_wire_vector(&mut s, &agg.sql(*g), text, hex);
    }
    // NULL セルは長さ -1 のまま（numeric 列でも変わらない）。
    let r = exec(
        &mut s,
        &format!("SELECT c52 FROM {TABLE} WHERE id = 0 LIMIT 10"),
        &[],
        &[],
        &[],
        &[1],
    );
    assert_eq!(r.rows, vec![vec![None]]);
}

/// パラメータ往復: PG17 が送出するバイト列をそのまま binary パラメータとして受理し、
/// 読み戻した binary が元のバイト列と一致する（復号→再符号化の往復一致）。
#[test]
fn pg17_numeric_send_vectors_round_trip_as_binary_params() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    for (round, declared) in [&[][..], &[1700]].into_iter().enumerate() {
        for v in col_vectors() {
            let id = 10_000 + 1_000 * round as u64 + v.id;
            let raw = unhex(v.hex);
            let ins = format!(
                "INSERT INTO {TABLE} (id, embedding, {}) VALUES ({id}, '[0,1]', $1) \
                 USING OPERATION_ID 'rt-{id}'",
                v.col
            );
            let r = exec(&mut s, &ins, declared, &[1], &[Some(&raw)], &[]);
            assert!(r.err.is_none(), "{} {}: {:?}", v.col, v.input, r.err);
            let sql = format!("SELECT {} FROM {TABLE} WHERE id = {id} LIMIT 10", v.col);
            assert_wire_vector(&mut s, &sql, &v.text, v.hex);

            // WHERE 句でも binary と text で同じ応答になる。
            let sel = format!(
                "SELECT id FROM {TABLE} WHERE {} = $1 AND id = {id} LIMIT 10",
                v.col
            );
            let by_bin = exec(&mut s, &sel, declared, &[1], &[Some(&raw)], &[0]);
            let by_text = exec(&mut s, &sel, &[], &[0], &[Some(v.text.as_bytes())], &[0]);
            assert!(by_bin.err.is_none(), "{:?}", by_bin.err);
            assert_eq!(by_bin.rows.len(), 1, "{} {}", v.col, v.input);
            assert_eq!(by_bin.rows, by_text.rows, "{} {}", v.col, v.input);
        }
    }
}

/// 受信境界の結果。`Ok(text)` は受理して列へ格納した値の text 表現、`Err(code)` は SQLSTATE。
type Outcome = Result<&'static str, &'static str>;

fn fmt_outcome(o: &Outcome) -> String {
    match o {
        Ok(t) => format!("OK:{t}"),
        Err(c) => format!("ERR:{c}"),
    }
}

/// `numeric_send` 形式のヘッダ（ndigits・weight・sign・dscale）と桁群を並べる。
fn raw(ndigits: u16, weight: i16, sign: u16, dscale: u16, groups: &[u16]) -> Vec<u8> {
    let mut v = Vec::new();
    for w in [ndigits, weight as u16, sign, dscale] {
        v.extend_from_slice(&w.to_be_bytes());
    }
    for g in groups {
        v.extend_from_slice(&g.to_be_bytes());
    }
    v
}

fn well_formed(weight: i16, sign: u16, dscale: u16, groups: &[u16]) -> Vec<u8> {
    raw(groups.len() as u16, weight, sign, dscale, groups)
}

struct Boundary {
    name: &'static str,
    raw: Vec<u8>,
    col: &'static str,
    /// 本リポの期待（現状を固定する）。
    expect: Outcome,
    /// PG17 実測。`expect` と異なる行は意図的な差分。
    pg17: Outcome,
}

fn b(
    name: &'static str,
    raw: Vec<u8>,
    col: &'static str,
    expect: Outcome,
    pg17: Outcome,
) -> Boundary {
    Boundary {
        name,
        raw,
        col,
        expect,
        pg17,
    }
}

/// 意図的な差分の行名（PG17 実測と本リポの期待が異なるもの）。差分を増減させる変更は
/// このリストの更新を要し、黙って PG17 と乖離させない。
const INTENTIONAL_DIFFS: &[&str] = &[
    "B5 trailing byte",
    "B7 ndigits=33",
    "B13 NaN",
    "B14a +Infinity",
    "B14b -Infinity",
    "B16a dscale=1022",
    "B16b dscale=0x3FFF",
    "B25 short body + bad sign",
    "B26 trailing byte + bad sign",
];

const NINES38: &str = "99999999999999999999999999999999999999";

fn boundaries() -> Vec<Boundary> {
    let mut big = vec![99u16];
    big.extend([9999u16; 9]);
    let mut big39 = vec![999u16];
    big39.extend([9999u16; 9]);
    let mut ok_extra = well_formed(0, 0, 2, &[123, 4500]);
    ok_extra.push(0);
    let mut bad_sign_extra = well_formed(0, 0x8000, 0, &[1]);
    bad_sign_extra.push(0);
    let mut g32 = vec![1u16];
    g32.extend([0u16; 31]);
    vec![
        b("B1 0 bytes", vec![], "c52", Err("08P01"), Err("08P01")),
        b("B2 7 bytes", vec![0; 7], "c52", Err("08P01"), Err("08P01")),
        b(
            "B3 ndigits=2 with 1 group",
            raw(2, 0, 0, 2, &[123]),
            "c52",
            Err("08P01"),
            Err("08P01"),
        ),
        b(
            "B4 valid",
            well_formed(0, 0, 2, &[123, 4500]),
            "c52",
            Ok("123.45"),
            Ok("123.45"),
        ),
        b(
            "B5 trailing byte",
            ok_extra,
            "c52",
            Err("08P01"),
            Err("22P03"),
        ),
        b(
            "B6 ndigits=32",
            well_formed(0, 0, 0, &g32),
            "c52",
            Ok("1.00"),
            Ok("1.00"),
        ),
        b(
            "B7 ndigits=33",
            raw(33, 0, 0, 0, &[0; 33]),
            "c52",
            Err("08P01"),
            Ok("0.00"),
        ),
        b(
            "B8a sign=0x8000",
            well_formed(0, 0x8000, 0, &[1]),
            "c52",
            Err("22P03"),
            Err("22P03"),
        ),
        b(
            "B8b sign=0x1000",
            well_formed(0, 0x1000, 0, &[1]),
            "c52",
            Err("22P03"),
            Err("22P03"),
        ),
        b(
            "B9 dscale=0x4000",
            well_formed(0, 0, 0x4000, &[1]),
            "c52",
            Err("22P03"),
            Err("22P03"),
        ),
        b(
            "B10 group=10000",
            well_formed(0, 0, 0, &[10000]),
            "c52",
            Err("22P03"),
            Err("22P03"),
        ),
        b(
            "B11a group=9999 into (38,0)",
            well_formed(0, 0, 0, &[9999]),
            "c380",
            Ok("9999"),
            Ok("9999"),
        ),
        b(
            "B11b group=9999 into (5,2)",
            well_formed(0, 0, 0, &[9999]),
            "c52",
            Err("22003"),
            Err("22003"),
        ),
        b(
            "B12 NaN with group=10000",
            well_formed(0, 0xC000, 0, &[10000]),
            "c52",
            Err("22P03"),
            Err("22P03"),
        ),
        b(
            "B13 NaN",
            raw(0, 0, 0xC000, 0, &[]),
            "c52",
            Err("22P02"),
            Ok("NaN"),
        ),
        b(
            "B14a +Infinity",
            raw(0, 0, 0xD000, 0, &[]),
            "c52",
            Err("22P02"),
            Err("22003"),
        ),
        b(
            "B14b -Infinity",
            raw(0, 0, 0xF000, 0, &[]),
            "c52",
            Err("22P02"),
            Err("22003"),
        ),
        b(
            "B15 dscale=1021",
            well_formed(0, 0, 1021, &[1]),
            "c52",
            Ok("1.00"),
            Ok("1.00"),
        ),
        b(
            "B16a dscale=1022",
            well_formed(0, 0, 1022, &[1]),
            "c52",
            Err("22003"),
            Ok("1.00"),
        ),
        b(
            "B16b dscale=0x3FFF",
            well_formed(0, 0, 0x3FFF, &[1]),
            "c52",
            Err("22003"),
            Ok("1.00"),
        ),
        b(
            "B17 38 integer digits",
            well_formed(9, 0, 0, &big),
            "c380",
            Ok(NINES38),
            Ok(NINES38),
        ),
        b(
            "B18 39 integer digits",
            well_formed(9, 0, 0, &big39),
            "c380",
            Err("22003"),
            Err("22003"),
        ),
        b(
            "B19a weight=i16::MAX into (38,0)",
            well_formed(i16::MAX, 0, 0, &[1]),
            "c380",
            Err("22003"),
            Err("22003"),
        ),
        b(
            "B19b weight=i16::MAX into (5,2)",
            well_formed(i16::MAX, 0, 0, &[1]),
            "c52",
            Err("22003"),
            Err("22003"),
        ),
        b(
            "B20 999.994",
            well_formed(0, 0, 3, &[999, 9940]),
            "c52",
            Ok("999.99"),
            Ok("999.99"),
        ),
        b(
            "B21 999.995",
            well_formed(0, 0, 3, &[999, 9950]),
            "c52",
            Err("22003"),
            Err("22003"),
        ),
        b(
            "B22 truncate below dscale",
            well_formed(0, 0, 2, &[123, 4490]),
            "c52",
            Ok("123.44"),
            Ok("123.44"),
        ),
        b(
            "B23 truncates to zero",
            well_formed(-1, 0x4000, 2, &[10]),
            "c52",
            Ok("0.00"),
            Ok("0.00"),
        ),
        b(
            "B24a inner zero groups into (13,4)",
            well_formed(2, 0, 4, &[1, 0, 0, 1]),
            "c134",
            Ok("100000000.0001"),
            Ok("100000000.0001"),
        ),
        b(
            "B24b inner zero groups into (5,2)",
            well_formed(2, 0, 4, &[1, 0, 0, 1]),
            "c52",
            Err("22003"),
            Err("22003"),
        ),
        b(
            "B25 short body + bad sign",
            raw(2, 0, 0x8000, 0, &[1]),
            "c52",
            Err("08P01"),
            Err("22P03"),
        ),
        b(
            "B26 trailing byte + bad sign",
            bad_sign_extra,
            "c52",
            Err("08P01"),
            Err("22P03"),
        ),
        b(
            "B27 bad sign + group=10000",
            well_formed(0, 0x8000, 0, &[10000]),
            "c52",
            Err("22P03"),
            Err("22P03"),
        ),
        b(
            "B28 group=10000 + weight=i16::MAX",
            well_formed(i16::MAX, 0, 0, &[10000]),
            "c52",
            Err("22P03"),
            Err("22P03"),
        ),
    ]
}

fn assert_alive(s: &mut Stream) {
    let r = exec(
        s,
        &format!("SELECT id FROM {TABLE} LIMIT 1"),
        &[],
        &[],
        &[],
        &[0],
    );
    assert!(r.err.is_none(), "connection must survive: {:?}", r.err);
}

/// 境界の SQLSTATE: 本リポの現状を固定し、PG17 実測との差分を表で明示する。
#[test]
fn numeric_binary_param_sqlstate_boundaries_vs_pg17() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(core), "alice");
    for (i, c) in boundaries().into_iter().enumerate() {
        // 表の自己整合: PG17 実測と期待が異なる行は意図的な差分リストに載っている行だけ。
        let differs = fmt_outcome(&c.expect) != fmt_outcome(&c.pg17);
        assert_eq!(
            differs,
            INTENTIONAL_DIFFS.contains(&c.name),
            "{}: intentional diff list out of sync",
            c.name
        );
        let id = 50_000 + i as u64;
        let ins = format!(
            "INSERT INTO {TABLE} (id, embedding, {}) VALUES ({id}, '[0,1]', $1) \
             USING OPERATION_ID 'bd-{id}'",
            c.col
        );
        let r = exec(&mut s, &ins, &[], &[1], &[Some(&c.raw)], &[]);
        let readback = format!("SELECT {} FROM {TABLE} WHERE id = {id} LIMIT 10", c.col);
        match c.expect {
            Ok(text) => {
                assert!(r.err.is_none(), "{}: {:?}", c.name, r.err);
                let (_, cell) = one_cell(&mut s, &readback, 0);
                assert_eq!(String::from_utf8(cell).expect("utf8"), text, "{}", c.name);
            }
            Err(code) => {
                let (got, msg) = r.err.unwrap_or_else(|| panic!("{}: want {code}", c.name));
                assert_eq!(got, code, "{}: {msg}", c.name);
                // 情報漏えい検査: 表名を ErrorResponse へ出さない（engine の精度超過 `22003` は
                // 呼び出し元自身のテナントが定義した列名を含むため、列名は検査対象にしない）。
                assert!(!msg.contains(TABLE), "{}: table name leaked: {msg}", c.name);
                // 副作用ゼロ、かつ Sync で回復して接続が維持される。
                assert_alive(&mut s);
                let after = exec(&mut s, &readback, &[], &[], &[], &[0]);
                assert!(after.rows.is_empty(), "{}: row must not exist", c.name);
            }
        }
    }
}

/// テナント境界: binary の結果・パラメータ・集計に他テナントの行が混ざらず、不正入力の
/// エラー応答は両テナントで完全に同一（存在情報の側チャネルがない）。
#[test]
fn numeric_binary_pg17_vectors_respect_tenant_boundary() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut bob = connect(addr, "bob");

    let r = exec(
        &mut bob,
        &format!("SELECT id, c52 FROM {TABLE} LIMIT 10"),
        &[],
        &[],
        &[],
        &[1, 1],
    );
    assert!(r.err.is_none(), "{:?}", r.err);
    assert_eq!(r.rows.len(), 1, "bob sees only his own row");
    // id 5000: ndigits 1・weight 0・dscale 0・桁群 [5000]。c52 = 1.00: dscale 2・桁群 [1]。
    assert_eq!(r.rows[0][0], Some(unhex("00010000000000001388")));
    assert_eq!(r.rows[0][1], Some(unhex("00010000000000020001")));

    // 集計は bob の行だけから計算される（alice の g=29 は n=1,2 だが bob は n=5 のみ）。
    let (_, avg) = one_cell(&mut bob, &Agg::AvgN.sql(29), 1);
    assert_eq!(avg, unhex("00010000000000100005"));
    let (_, sum) = one_cell(
        &mut bob,
        &format!("SELECT SUM(b) FROM {TABLE} WHERE g = 29"),
        1,
    );
    assert_eq!(sum, unhex("00010000000000000001"));
    // alice の g=34（SUM(b) = 18000000000）は bob からは行が無く NULL。
    let r = exec(&mut bob, &Agg::SumB.sql(34), &[], &[], &[], &[1]);
    assert_eq!(r.rows, vec![vec![None]]);

    // alice の行に一致する値を binary パラメータで送っても 0 件。
    let raw123 = unhex("0002000000000002007b1194"); // 123.45
    let sel = format!("SELECT id FROM {TABLE} WHERE c52 = $1 LIMIT 10");
    let mut alice = connect(addr, "alice");
    let a = exec(&mut alice, &sel, &[], &[1], &[Some(&raw123)], &[0]);
    assert!(a.err.is_none(), "{:?}", a.err);
    // alice の 123.45 は単独ベクタの行と SUM(c52) 用の行の 2 件（bob の行は混ざらない）。
    assert_eq!(a.rows.len(), 2, "alice sees exactly her own rows");
    let bb = exec(&mut bob, &sel, &[], &[1], &[Some(&raw123)], &[0]);
    assert!(bb.err.is_none(), "{:?}", bb.err);
    assert!(bb.rows.is_empty(), "bob must not see alice's row");

    // 同じ不正バイト列へのエラー応答は両テナントで完全に同一。
    for (i, c) in boundaries().into_iter().enumerate() {
        if c.expect.is_ok() {
            continue;
        }
        let ins = |id: u64| {
            format!(
                "INSERT INTO {TABLE} (id, embedding, {}) VALUES ({id}, '[0,1]', $1) \
                 USING OPERATION_ID 'tn-{id}'",
                c.col
            )
        };
        let ra = exec(
            &mut alice,
            &ins(60_000 + i as u64),
            &[],
            &[1],
            &[Some(&c.raw)],
            &[],
        );
        let rb = exec(
            &mut bob,
            &ins(61_000 + i as u64),
            &[],
            &[1],
            &[Some(&c.raw)],
            &[],
        );
        assert!(ra.err.is_some(), "{}", c.name);
        assert_eq!(
            ra.err, rb.err,
            "{}: error must be tenant-independent",
            c.name
        );
    }
}
