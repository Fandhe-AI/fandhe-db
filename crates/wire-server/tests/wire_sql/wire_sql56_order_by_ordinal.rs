//! `ORDER BY` の序数指定（`ORDER BY 1, 2 DESC`）が、PostgreSQL wire プロトコル v3 の簡易
//! クエリ・拡張クエリ双方の生バイトクライアント越しに契約どおりの応答として観測できる
//! ことを検証する結合テスト（Issue #1792。ポインタ: `docs/spec/05-tasks.md` TASK-298・
//! `docs/spec/04-behavior/sql-surface.md` SQL-56 (2)、関連: WIRE-11・WIRE-12、ERR-1・ERR-4）。
//!
//! 責務境界: 序数の解決規則そのものは engine 側の `crates/engine/tests/
//! sql56_order_by_ordinal.rs`（in-process）が確定オラクルである。本ファイルは同じ規則を
//! wire フレーミング越しに再確認し、`DataRow` の順序・`RowDescription`・`ErrorResponse` の
//! SQLSTATE（範囲外 `42P10`、序数にならない形 `42601`）を固定する。拡張クエリの結果は
//! リテラル形の簡易クエリとバイト一致させ、第 2 の実行器を作らない。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

type Stream = TcpStream;

/// `(id, lang, n)`。tenant-a の Public 行。
const ROWS: [(u64, &str, i32); 6] = [
    (1, "ja", 3),
    (2, "en", 1),
    (3, "ja", 2),
    (4, "fr", 4),
    (5, "en", 5),
    (6, "fr", 0),
];

fn insert(storage: &Storage, tenant: &str, vis: Visibility, id: u64, lang: &str, n: i32) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx,
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Integer(n),
        ],
        &OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id"),
    )
    .expect("insert row");
}

fn setup(name: &str) -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(name);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    for (id, lang, n) in ROWS {
        insert(&storage, "tenant-a", Visibility::Public, id, lang, n);
    }
    // tenant-c の Private 行（alice・bob から見えてはならない RLS 対照）。
    insert(
        &storage,
        "tenant-c",
        Visibility::Private,
        100,
        "zz-secret",
        999,
    );
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "pw"),
        ("bob", "tenant-b", "pw"),
        ("carol", "tenant-c", "pw"),
    ]);
    (spawn_server_with_engine(&users, core), guard)
}

fn connect(addr: SocketAddr, user: &str) -> Stream {
    authenticate_to_ready_for_query(addr, user, "pw")
}

// ---------- 応答の収集 ----------

/// `Z` までに受けた応答の観測結果。
#[derive(Debug, Default, PartialEq)]
struct Outcome {
    /// `RowDescription` の `(列名, 型 OID)`。
    columns: Vec<(String, i32)>,
    rows: Vec<Vec<Option<String>>>,
    tag: String,
    /// 最初の `ErrorResponse` の SQLSTATE（`C` フィールド）。
    sqlstate: Option<String>,
}

fn cstr(body: &[u8], pos: &mut usize) -> Option<String> {
    let rest = body.get(*pos..)?;
    let nul = rest.iter().position(|&b| b == 0)?;
    let s = String::from_utf8_lossy(rest.get(..nul)?).to_string();
    *pos += nul + 1;
    Some(s)
}

fn be_i32(body: &[u8], pos: usize) -> i32 {
    let b: [u8; 4] = body
        .get(pos..pos + 4)
        .and_then(|s| s.try_into().ok())
        .expect("4 bytes");
    i32::from_be_bytes(b)
}

fn be_i16(body: &[u8], pos: usize) -> i16 {
    let b: [u8; 2] = body
        .get(pos..pos + 2)
        .and_then(|s| s.try_into().ok())
        .expect("2 bytes");
    i16::from_be_bytes(b)
}

fn parse_row_description(body: &[u8]) -> Vec<(String, i32)> {
    let n = be_i16(body, 0) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        let name = cstr(body, &mut pos).expect("column name");
        // table oid(4) + attnum(2) の後が type oid。
        out.push((name, be_i32(body, pos + 6)));
        pos += 4 + 2 + 4 + 2 + 4 + 2;
    }
    out
}

fn parse_data_row(body: &[u8]) -> Vec<Option<String>> {
    let n = be_i16(body, 0) as usize;
    let mut pos = 2;
    let mut cells = Vec::new();
    for _ in 0..n {
        let len = be_i32(body, pos);
        pos += 4;
        if len < 0 {
            cells.push(None);
            continue;
        }
        let end = pos + len as usize;
        let bytes = body.get(pos..end).expect("cell bytes");
        cells.push(Some(String::from_utf8_lossy(bytes).to_string()));
        pos = end;
    }
    cells
}

/// `ErrorResponse` のフィールド列から `C`（SQLSTATE）を完全一致照合用に取り出す。
fn sqlstate_of(body: &[u8]) -> Option<String> {
    let mut pos = 0;
    while let Some(&kind) = body.get(pos) {
        if kind == 0 {
            return None;
        }
        pos += 1;
        let value = cstr(body, &mut pos)?;
        if kind == b'C' {
            return Some(value);
        }
    }
    None
}

/// `Z('I')` までのメッセージを順序保存で集める（`1`・`2`・`3` は読み捨てる）。
fn collect(stream: &mut Stream) -> Outcome {
    let mut out = Outcome::default();
    loop {
        let (k, body) = read_message(stream);
        match k {
            b'T' => out.columns = parse_row_description(&body),
            b'D' => out.rows.push(parse_data_row(&body)),
            b'C' => {
                out.tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'E' => {
                if out.sqlstate.is_none() {
                    out.sqlstate = sqlstate_of(&body);
                }
            }
            b'Z' => {
                assert_eq!(body, [b'I'], "ReadyForQuery must report idle");
                return out;
            }
            _ => {}
        }
    }
}

fn simple(stream: &mut Stream, sql: &str) -> Outcome {
    send_simple_query(stream, sql);
    collect(stream)
}

fn send(stream: &mut Stream, kind: u8, body: &[u8]) {
    send_length_prefixed_message(stream, kind, body);
}

fn bind_msg() -> Vec<u8> {
    let mut body = vec![0u8, 0u8]; // portal "", statement ""
    body.extend_from_slice(&0i16.to_be_bytes()); // param format codes
    body.extend_from_slice(&0i16.to_be_bytes()); // param count
    body.extend_from_slice(&0i16.to_be_bytes()); // result format codes
    body
}

fn describe_portal() -> Vec<u8> {
    vec![b'P', 0]
}

/// Parse→Bind→Describe(Portal)→Execute→Sync。列名・OID も得るため portal を Describe する。
fn extended(stream: &mut Stream, sql: &str) -> Outcome {
    send(stream, b'P', &parse_body("", sql, 0));
    send(stream, b'B', &bind_msg());
    send(stream, b'D', &describe_portal());
    send(stream, b'E', &execute_body("", 0));
    send(stream, b'S', b"");
    collect(stream)
}

fn id_column(out: &Outcome) -> Vec<String> {
    out.rows
        .iter()
        .map(|r| r.first().cloned().flatten().expect("first cell"))
        .collect()
}

fn ids(want: &[u64]) -> Vec<String> {
    want.iter().map(|i| i.to_string()).collect()
}

/// 簡易クエリが `sqlstate` で失敗し、同じ接続で次のクエリが通る（セッション継続）。
fn assert_simple_error(stream: &mut Stream, sql: &str, sqlstate: &str) {
    let out = simple(stream, sql);
    assert_eq!(out.sqlstate.as_deref(), Some(sqlstate), "{sql}: {out:?}");
    assert!(out.rows.is_empty(), "{sql}");
    let next = simple(stream, "SELECT id FROM docs ORDER BY id LIMIT 1");
    assert_eq!(next.sqlstate, None, "session must continue after: {sql}");
    assert_eq!(next.rows.len(), 1);
}

/// 拡張クエリが `sqlstate` で失敗し、Sync で回復して接続が継続する。
fn assert_extended_error(stream: &mut Stream, sql: &str, sqlstate: &str) {
    let out = extended(stream, sql);
    assert_eq!(out.sqlstate.as_deref(), Some(sqlstate), "{sql}");
    assert!(out.rows.is_empty(), "{sql}");
    let next = simple(stream, "SELECT id FROM docs ORDER BY id LIMIT 1");
    assert_eq!(next.sqlstate, None, "session must continue after: {sql}");
}

// ---------- テスト ----------

/// 序数の並び順と `RowDescription`・コマンドタグが、列名で書いた同じ文と一致する。
#[test]
fn simple_ordinal_matches_column_name_form() {
    let (addr, _g) = setup("wire-sql56-simple");
    let mut s = connect(addr, "alice");

    // 出力: id(1), lang(2), n(3)。lang 昇順・n 降順、同点なし。
    let by_ordinal = simple(
        &mut s,
        "SELECT id, lang, n FROM docs ORDER BY 2, 3 DESC LIMIT 100",
    );
    let by_name = simple(
        &mut s,
        "SELECT id, lang, n FROM docs ORDER BY lang, n DESC LIMIT 100",
    );
    assert_eq!(by_ordinal.sqlstate, None);
    assert_eq!(by_ordinal, by_name);
    // en: 5(n=5),2(n=1) / fr: 4(n=4),6(n=0) / ja: 1(n=3),3(n=2)。
    assert_eq!(id_column(&by_ordinal), ids(&[5, 2, 4, 6, 1, 3]));
    assert_eq!(by_ordinal.tag, "SELECT 6");
    assert_eq!(by_ordinal.columns.len(), 3);

    // `*` 展開後の位置（1 = id）。
    let star = simple(&mut s, "SELECT * FROM docs ORDER BY 1 DESC LIMIT 100");
    assert_eq!(star.sqlstate, None);
    assert_eq!(id_column(&star), ids(&[6, 5, 4, 3, 2, 1]));
}

/// 拡張クエリの序数は、リテラル形の簡易クエリと完全一致する。
#[test]
fn extended_ordinal_matches_simple_form() {
    let (addr, _g) = setup("wire-sql56-extended");
    let mut ext = connect(addr, "alice");
    let mut sim = connect(addr, "alice");

    let sql = "SELECT id, lang, n FROM docs ORDER BY 3 DESC, 1 LIMIT 100";
    let got = extended(&mut ext, sql);
    let want = simple(&mut sim, sql);
    assert_eq!(got.sqlstate, None);
    assert_eq!(got.rows, want.rows);
    assert_eq!(got.tag, want.tag);
    assert_eq!(got.columns, want.columns);
    assert_eq!(id_column(&got), ids(&[5, 4, 1, 3, 2, 6]));
}

/// 範囲外の序数は `42P10`、序数にならない形は `42601`。どちらも接続は継続する。
#[test]
fn ordinal_error_contract_simple_and_extended() {
    let (addr, _g) = setup("wire-sql56-errors");
    let mut s = connect(addr, "alice");

    for sql in [
        "SELECT id, lang FROM docs ORDER BY 0 LIMIT 5",
        "SELECT id, lang FROM docs ORDER BY -1 LIMIT 5",
        "SELECT id, lang FROM docs ORDER BY 3 LIMIT 5",
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY 3",
    ] {
        assert_simple_error(&mut s, sql, "42P10");
        assert_extended_error(&mut s, sql, "42P10");
    }
    for sql in [
        "SELECT id, lang FROM docs ORDER BY 1 + 0 LIMIT 5",
        "SELECT id, lang FROM docs ORDER BY 1.5 LIMIT 5",
        "SELECT id, lang FROM docs ORDER BY $1 LIMIT 5",
    ] {
        assert_simple_error(&mut s, sql, "42601");
    }
}

/// 他テナントの行（Private）は序数の並べ替えにも現れず、範囲外の判定は可視行に依存しない。
#[test]
fn ordinal_respects_rls_boundary() {
    let (addr, _g) = setup("wire-sql56-rls");
    let mut alice = connect(addr, "alice");
    let mut bob = connect(addr, "bob");

    let a = simple(
        &mut alice,
        "SELECT id, lang FROM docs ORDER BY 2 DESC LIMIT 100",
    );
    assert_eq!(a.sqlstate, None);
    assert!(a
        .rows
        .iter()
        .all(|r| r.get(1) != Some(&Some("zz-secret".into()))));
    assert_eq!(a.rows.len(), ROWS.len());

    // bob（tenant-b）から見える Public 行も同じ（tenant-a の Public 行は共有）。判定の有無は
    // 行の可視性に依存しない。
    assert_simple_error(
        &mut bob,
        "SELECT id, lang FROM docs ORDER BY 3 LIMIT 5",
        "42P10",
    );
}
