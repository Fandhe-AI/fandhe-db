//! `EXTRACT(field FROM src)` と、広域取得の `ORDER BY`・`GROUP BY` 集計の
//! `HAVING`／`ORDER BY` に置いたスカラー式（関数呼び出し・`CASE`・`COALESCE`・
//! `NULLIF`・`EXTRACT`）が、PostgreSQL wire プロトコル v3 の簡易クエリ・拡張クエリ
//! 双方の生バイトクライアント越しに契約どおりの応答として観測できることを検証する
//! 結合テスト（Issue #1276。ポインタ: `docs/spec/05-tasks.md` TASK-210・
//! `docs/spec/04-behavior/sql-surface.md` SQL-26、関連: WIRE-11・WIRE-12・WIRE-13、
//! RLS-7・RLS-8・RLS-11、ERR-1・ERR-2・ERR-4）。
//!
//! 責務境界: 並び順・集計値・拒否形状の規則そのものは engine 側の
//! `crates/engine/tests/sql26_extract.rs`・`sql26_order_by_having_expressions.rs`
//! （in-process）が確定オラクルである。本ファイルは同じ規則を wire フレーミング越しに
//! 再確認し、`RowDescription` の列名と型 OID・`DataRow` のテキスト表現・
//! `ErrorResponse` の SQLSTATE・拡張クエリの `ParameterDescription` 推論を固定する。
//! 拡張クエリの値はリテラル形の簡易クエリとバイト一致させ、第 2 の実行器を作らない。
//!
//! 回帰ガード: prepared statement のパラメータ型推論がテーブル名を引く `FROM` は
//! 括弧深さ 0 の最初の `FROM` である。SELECT リストの `EXTRACT(year FROM d)` が
//! 括弧内に `FROM` を持っても推論が壊れないことを `ParameterDescription` で固定する。
//!
//! 対象外: 三クライアント harness（`extended_syntax_e2e.rs`・`#[ignore]` で
//! `make e2e-three-client` から明示実行）。実クライアントが開発環境に無く `make ci`
//! でも実行されないため、常時実行される本層で固定する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

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

const JAN_1_2020: i32 = 18_262;
const JAN_1_2021: i32 = 18_628;
const JAN_1_2022: i32 = 18_993;
const JAN_1_2023: i32 = 19_358;

/// `(id, title, lang, n, 日付 = 1 月 1 日の日数 + オフセット)`。
/// engine の `sql26_order_by_having_expressions.rs` の `ROWS` と同じ値。
type FixtureRow = (
    u64,
    Option<&'static str>,
    &'static str,
    Option<i32>,
    Option<i32>,
);

const ROWS: [FixtureRow; 8] = [
    (1, Some("Banana"), "ja", Some(3), Some(JAN_1_2021 + 10)),
    (2, Some("apple"), "en", Some(1), Some(JAN_1_2020 + 5)),
    (3, Some("Cherry"), "ja", None, Some(JAN_1_2022)),
    (4, None, "en", Some(5), Some(JAN_1_2021 + 200)),
    (5, Some("banana"), "ja", Some(2), None),
    (6, Some("Apple"), "fr", Some(4), Some(JAN_1_2023 + 1)),
    (7, Some("cherry"), "en", Some(0), Some(JAN_1_2020 + 100)),
    (8, Some("date"), "fr", Some(3), Some(JAN_1_2022 + 250)),
];

/// tenant-c の Private 行（alice・bob から見えてはならない RLS 対照）。
const SECRET_ROW: FixtureRow = (
    100,
    Some("AAA-secret"),
    "zz-secret",
    Some(999),
    Some(JAN_1_2020),
);

const AGG: &str = "SELECT lang, COUNT(*) AS c, SUM(n) AS s FROM docs GROUP BY lang";

fn insert(storage: &Storage, tenant: &str, vis: Visibility, r: &FixtureRow) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let text = |s: Option<&str>| s.map(|v| Value::Text(v.to_string())).unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx,
        r.0,
        vis,
        &[
            Value::Vector(vec![r.0 as f32, 0.0]),
            text(r.1),
            text(Some(r.2)),
            r.3.map(Value::Integer).unwrap_or(Value::Null),
            r.4.map(Value::Date).unwrap_or(Value::Null),
        ],
        &OperationId::parse(&format!("test-op-{}", r.0)).expect("valid operation_id"),
    )
    .expect("insert row");
}

/// tenant-a の Public 行 8 件と tenant-c の Private 行 1 件を持つサーバーを起動する。
fn setup(name: &str) -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(name);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("title", ColumnType::Text, true),
                ColumnDef::new("lang", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("d", ColumnType::Date, true),
            ],
        ))
        .expect("create table");
    for r in &ROWS {
        insert(&storage, "tenant-a", Visibility::Public, r);
    }
    insert(&storage, "tenant-c", Visibility::Private, &SECRET_ROW);
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
    /// 最初の `ErrorResponse` の全文（秘密値の非混入確認用）。
    error_text: Option<String>,
    /// `ParameterDescription` の OID 列。
    param_oids: Option<Vec<i32>>,
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

fn param_description_oids(body: &[u8]) -> Vec<i32> {
    let n = be_i16(body, 0) as usize;
    (0..n).map(|i| be_i32(body, 2 + 4 * i)).collect()
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
            b't' => out.param_oids = Some(param_description_oids(&body)),
            b'E' => {
                if out.sqlstate.is_none() {
                    out.sqlstate = sqlstate_of(&body);
                    out.error_text = Some(String::from_utf8_lossy(&body).to_string());
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

fn parse_msg(name: &str, sql: &str) -> Vec<u8> {
    parse_body(name, sql, 0)
}

fn bind_msg(values: &[&str]) -> Vec<u8> {
    let mut body = vec![0u8, 0u8]; // portal "", statement ""
    body.extend_from_slice(&0i16.to_be_bytes()); // param format codes: 全て text
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for v in values {
        body.extend_from_slice(&(v.len() as i32).to_be_bytes());
        body.extend_from_slice(v.as_bytes());
    }
    body.extend_from_slice(&0i16.to_be_bytes()); // result format codes
    body
}

fn describe_msg(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

/// Parse→Bind→Describe(Portal)→Execute→Sync。列名・OID も得るため portal を Describe する。
fn extended(stream: &mut Stream, sql: &str, values: &[&str]) -> Outcome {
    send(stream, b'P', &parse_msg("", sql));
    send(stream, b'B', &bind_msg(values));
    send(stream, b'D', &describe_msg(b'P', ""));
    send(stream, b'E', &execute_body("", 0));
    send(stream, b'S', b"");
    collect(stream)
}

/// Parse→Describe(Statement)→Sync。`ParameterDescription` と `RowDescription` を得る。
fn describe_statement(stream: &mut Stream, sql: &str) -> Outcome {
    send(stream, b'P', &parse_msg("d", sql));
    send(stream, b'D', &describe_msg(b'S', "d"));
    send(stream, b'S', b"");
    let out = collect(stream);
    send(stream, b'C', &describe_msg(b'S', "d"));
    send(stream, b'S', b"");
    collect(stream);
    out
}

// ---------- 期待値ヘルパー ----------

fn cell(s: &str) -> Option<String> {
    Some(s.to_string())
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

fn assert_ids(stream: &mut Stream, sql: &str, want: &[u64]) {
    let out = simple(stream, sql);
    assert_eq!(out.sqlstate, None, "{sql}");
    assert_eq!(id_column(&out), ids(want), "{sql}");
    assert_eq!(out.tag, format!("SELECT {}", want.len()), "{sql}");
    assert_eq!(out.columns, vec![("id".to_string(), 1700)], "{sql}");
}

fn assert_langs(stream: &mut Stream, tail: &str, want: &[&str]) {
    let sql = format!("{AGG} {tail}");
    let out = simple(stream, &sql);
    assert_eq!(out.sqlstate, None, "{sql}");
    assert_eq!(
        id_column(&out),
        want.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "{sql}"
    );
    assert_eq!(out.tag, format!("SELECT {}", want.len()), "{sql}");
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

/// 拡張クエリ（`$n` を含む）が `sqlstate` で失敗し、Sync で回復して接続が継続する。
fn assert_extended_error(stream: &mut Stream, sql: &str, values: &[&str], sqlstate: &str) {
    let out = extended(stream, sql, values);
    assert_eq!(out.sqlstate.as_deref(), Some(sqlstate), "{sql}");
    assert!(out.rows.is_empty(), "{sql}");
    let next = simple(stream, "SELECT id FROM docs ORDER BY id LIMIT 1");
    assert_eq!(next.sqlstate, None, "session must continue after: {sql}");
}

// ---------- 簡易クエリ: EXTRACT ----------

#[test]
fn simple_extract_reports_column_name_oid_and_values() {
    let (addr, _g) = setup("wire-sql26-extract-simple");
    let mut s = connect(addr, "alice");

    let out = simple(
        &mut s,
        "SELECT id, EXTRACT(year FROM d) FROM docs ORDER BY id LIMIT 100",
    );
    assert_eq!(out.sqlstate, None);
    assert_eq!(
        out.columns,
        vec![("id".to_string(), 1700), ("extract".to_string(), 701)]
    );
    let years: Vec<Option<String>> = out.rows.iter().map(|r| r[1].clone()).collect();
    assert_eq!(
        years,
        vec![
            cell("2021"),
            cell("2020"),
            cell("2022"),
            cell("2021"),
            None,
            cell("2023"),
            cell("2020"),
            cell("2022")
        ]
    );
    assert_eq!(out.tag, "SELECT 8");

    // 別名は列名になる。
    let out = simple(
        &mut s,
        "SELECT EXTRACT(month FROM d) AS m FROM docs ORDER BY id LIMIT 1",
    );
    assert_eq!(out.columns, vec![("m".to_string(), 701)]);
    assert_eq!(out.rows, vec![vec![cell("1")]]);

    // date_part との同値性（列名だけが異なる）。
    let ex = simple(
        &mut s,
        "SELECT EXTRACT(doy FROM d) FROM docs ORDER BY id LIMIT 100",
    );
    let dp = simple(
        &mut s,
        "SELECT date_part('doy', d) FROM docs ORDER BY id LIMIT 100",
    );
    assert_eq!(ex.rows, dp.rows);
    assert_eq!(ex.columns, vec![("extract".to_string(), 701)]);
    assert_eq!(dp.columns, vec![("date_part".to_string(), 701)]);
    assert_eq!(ex.rows[0], vec![cell("11")]);
}

// ---------- 簡易クエリ: 広域取得の式 ORDER BY ----------

#[test]
fn simple_scan_order_by_expressions_match_oracle() {
    let (addr, _g) = setup("wire-sql26-scan-order");
    let mut s = connect(addr, "alice");

    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY EXTRACT(year FROM d) DESC, id LIMIT 100",
        &[5, 6, 3, 8, 1, 4, 2, 7],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY EXTRACT(year FROM d), id LIMIT 100",
        &[2, 7, 1, 4, 3, 8, 6, 5],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY lower(title), id LIMIT 100",
        &[2, 6, 1, 5, 3, 7, 8, 4],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY lower(title) DESC, id LIMIT 100",
        &[4, 8, 3, 7, 1, 5, 2, 6],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY CASE WHEN n > 2 THEN 0 ELSE 1 END, id LIMIT 100",
        &[1, 4, 6, 8, 2, 3, 5, 7],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY COALESCE(lower(title), ''), id LIMIT 100",
        &[4, 2, 6, 1, 5, 3, 7, 8],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY NULLIF(lower(title), 'apple'), id LIMIT 100",
        &[1, 5, 3, 7, 8, 2, 4, 6],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY lower(title), id LIMIT 3 OFFSET 2",
        &[1, 5, 3],
    );
    assert_ids(
        &mut s,
        "SELECT id FROM docs ORDER BY lang, lower(title) DESC, id LIMIT 100",
        &[4, 7, 2, 8, 6, 3, 1, 5],
    );
}

// ---------- 簡易クエリ: GROUP BY の式 HAVING／ORDER BY ----------

#[test]
fn simple_aggregate_having_and_order_by_expressions() {
    let (addr, _g) = setup("wire-sql26-agg");
    let mut s = connect(addr, "alice");

    let out = simple(&mut s, &format!("{AGG} ORDER BY lang"));
    assert_eq!(
        out.columns,
        vec![
            ("lang".to_string(), 25),
            ("c".to_string(), 20),
            ("s".to_string(), 20)
        ]
    );
    assert_eq!(
        out.rows,
        vec![
            vec![cell("en"), cell("3"), cell("6")],
            vec![cell("fr"), cell("2"), cell("7")],
            vec![cell("ja"), cell("3"), cell("5")],
        ]
    );
    assert_eq!(out.tag, "SELECT 3");

    assert_langs(&mut s, "HAVING abs(c - 3) < 1 ORDER BY lang", &["en", "ja"]);
    assert_langs(
        &mut s,
        "HAVING CASE WHEN s > 5 THEN 1 ELSE 0 END = 1 ORDER BY lang",
        &["en", "fr"],
    );
    assert_langs(
        &mut s,
        "HAVING COALESCE(s, 0) >= 6 ORDER BY lang",
        &["en", "fr"],
    );
    assert_langs(&mut s, "HAVING NULLIF(c, 3) > 0 ORDER BY lang", &["fr"]);
    assert_langs(&mut s, "HAVING lower(lang) = 'ja' ORDER BY lang", &["ja"]);
    assert_langs(&mut s, "ORDER BY lower(lang) DESC", &["ja", "fr", "en"]);
    assert_langs(&mut s, "ORDER BY abs(s - 6), lang", &["en", "fr", "ja"]);
    assert_langs(&mut s, "ORDER BY COALESCE(s, 0) DESC", &["fr", "en", "ja"]);
    assert_langs(
        &mut s,
        "HAVING abs(c - 3) < 1 ORDER BY lower(lang) DESC LIMIT 1",
        &["ja"],
    );
}

// ---------- 簡易クエリ: 未対応形の ErrorResponse ----------

#[test]
fn simple_unsupported_forms_return_exact_sqlstate_and_session_continues() {
    let (addr, _g) = setup("wire-sql26-errors");
    let mut s = connect(addr, "alice");

    for sql in [
        "SELECT EXTRACT(year d) FROM docs LIMIT 1".to_string(),
        "SELECT EXTRACT(year FROM d extra) FROM docs LIMIT 1".to_string(),
        "SELECT EXTRACT(year FROM) FROM docs LIMIT 1".to_string(),
        "SELECT id FROM docs ORDER BY count(*) LIMIT 5".to_string(),
        "SELECT id FROM docs ORDER BY 1 LIMIT 5".to_string(),
        "SELECT id FROM docs ORDER BY lower(title), embedding <=> '[1.0,0.0]' LIMIT 5".to_string(),
        format!("{AGG} HAVING count(*) > 1"),
        format!("{AGG} ORDER BY sum(n)"),
    ] {
        assert_simple_error(&mut s, &sql, "42601");
    }

    for sql in [
        "SELECT id FROM docs ORDER BY lower(nosuchcol) LIMIT 5".to_string(),
        "SELECT id FROM docs ORDER BY vec_div(embedding, 2) LIMIT 5".to_string(),
        format!("{AGG} HAVING abs(id) > 1"),
        format!("{AGG} HAVING lower(title) = 'a'"),
    ] {
        assert_simple_error(&mut s, &sql, "22000");
    }

    // 評価時エラー（0 除算）。
    assert_simple_error(
        &mut s,
        "SELECT id FROM docs ORDER BY abs(1 / (n - n)) LIMIT 100",
        "22012",
    );
}

#[test]
fn simple_extract_unknown_field_matches_date_part_sqlstate() {
    let (addr, _g) = setup("wire-sql26-unknown-field");
    let mut s = connect(addr, "alice");

    let ex = simple(&mut s, "SELECT EXTRACT(nosuch FROM d) FROM docs LIMIT 1");
    let dp = simple(&mut s, "SELECT date_part('nosuch', d) FROM docs LIMIT 1");
    assert!(ex.sqlstate.is_some());
    assert_eq!(ex.sqlstate, dp.sqlstate);
    assert_eq!(ex.sqlstate.as_deref(), Some("22000"));
}

// ---------- 拡張クエリ: パラメータ型推論 ----------

#[test]
fn extended_param_inference_survives_extract_in_select_list() {
    let (addr, _g) = setup("wire-sql26-param-infer");
    let mut s = connect(addr, "alice");

    // 回帰ガード: SELECT リストの EXTRACT(year FROM d) は括弧内に FROM を持つ。
    // テーブルを引く FROM を「最初の FROM」にすると d をテーブル名と取り違え、
    // `d = $1` の推論が Text(25) へ落ちる。`d`（DATE）は TEXT でも id でもないため、
    // この取り違えを区別できる。
    let out = describe_statement(
        &mut s,
        "SELECT EXTRACT(year FROM d) AS y, id FROM docs WHERE d = $1 ORDER BY id LIMIT 10",
    );
    assert_eq!(out.sqlstate, None);
    assert_eq!(out.param_oids, Some(vec![1082]));
    assert_eq!(
        out.columns,
        vec![("y".to_string(), 701), ("id".to_string(), 1700)]
    );

    let out = describe_statement(
        &mut s,
        "SELECT EXTRACT(month FROM d) AS m FROM docs WHERE d = $1 AND lang = $2 LIMIT 10",
    );
    assert_eq!(out.param_oids, Some(vec![1082, 25]));

    // 対照: 旧実装でも通る形（FROM が WHERE より前に 1 つだけ）。回帰ガードではない。
    let out = describe_statement(
        &mut s,
        "SELECT id FROM docs WHERE d = $1 ORDER BY EXTRACT(month FROM d), id LIMIT 10",
    );
    assert_eq!(out.param_oids, Some(vec![1082]));
}

// ---------- 拡張クエリ: 束縛値とリテラル形の一致 ----------

#[test]
fn extended_binding_matches_literal_form() {
    let (addr, _g) = setup("wire-sql26-ext-bind");
    let mut ext = connect(addr, "alice");
    let mut sim = connect(addr, "alice");

    // 2021-01-11 は id 1 の日付。
    let got = extended(
        &mut ext,
        "SELECT EXTRACT(year FROM d) AS y, id FROM docs WHERE d = $1 ORDER BY id LIMIT 10",
        &["2021-01-11"],
    );
    let want = simple(
        &mut sim,
        "SELECT EXTRACT(year FROM d) AS y, id FROM docs WHERE d = '2021-01-11' ORDER BY id LIMIT 10",
    );
    assert_eq!(got.sqlstate, None);
    assert_eq!(got.rows, want.rows);
    assert_eq!(got.tag, want.tag);
    assert_eq!(got.columns, want.columns);
    assert_eq!(got.rows, vec![vec![cell("2021"), cell("1")]]);
    assert_eq!(got.tag, "SELECT 1");

    let got = extended(
        &mut ext,
        "SELECT lang, COUNT(*) AS c, SUM(n) AS s FROM docs WHERE lang = $1 GROUP BY lang HAVING abs(c - 3) < 1 ORDER BY lower(lang) DESC",
        &["ja"],
    );
    let want = simple(
        &mut sim,
        "SELECT lang, COUNT(*) AS c, SUM(n) AS s FROM docs WHERE lang = 'ja' GROUP BY lang HAVING abs(c - 3) < 1 ORDER BY lower(lang) DESC",
    );
    assert_eq!(got.sqlstate, None);
    assert_eq!(got.rows, want.rows);
    assert_eq!(got.tag, want.tag);
    assert_eq!(got.columns, want.columns);
    assert_eq!(got.rows, vec![vec![cell("ja"), cell("3"), cell("5")]]);

    let got = extended(
        &mut ext,
        "SELECT id, EXTRACT(year FROM d) FROM docs WHERE lang = $1 ORDER BY EXTRACT(year FROM d) DESC, id LIMIT 100",
        &["ja"],
    );
    let want = simple(
        &mut sim,
        "SELECT id, EXTRACT(year FROM d) FROM docs WHERE lang = 'ja' ORDER BY EXTRACT(year FROM d) DESC, id LIMIT 100",
    );
    assert_eq!(got.sqlstate, None);
    assert_eq!(got.rows, want.rows);
    assert_eq!(got.tag, want.tag);
    assert_eq!(got.columns, want.columns);
    assert_eq!(id_column(&got), ids(&[5, 3, 1]));
}

// ---------- 拡張クエリ: 未対応形の ErrorResponse ----------

#[test]
fn extended_unsupported_forms_return_exact_sqlstate_and_recover() {
    let (addr, _g) = setup("wire-sql26-ext-errors");
    let mut s = connect(addr, "alice");

    // `$n` の位置が未対応（左辺が識別子でない・GROUP BY で WHERE 領域が閉じる）。
    assert_extended_error(
        &mut s,
        "SELECT id FROM docs WHERE EXTRACT(year FROM d) = $1 LIMIT 10",
        &["2021"],
        "42601",
    );
    assert_extended_error(
        &mut s,
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang HAVING lower(lang) = $1",
        &["ja"],
        "42601",
    );
    assert_extended_error(
        &mut s,
        "SELECT EXTRACT(year d) FROM docs WHERE d = $1",
        &["2021-01-11"],
        "42601",
    );
    assert_extended_error(
        &mut s,
        "SELECT id FROM docs WHERE d = $1 ORDER BY lower(nosuchcol) LIMIT 3",
        &["2021-01-11"],
        "22000",
    );
    assert_extended_error(
        &mut s,
        "SELECT lang, COUNT(*) AS c FROM docs WHERE lang = $1 GROUP BY lang HAVING abs(id) > 1",
        &["ja"],
        "22000",
    );
}

// ---------- RLS（テナント境界）の非退行 ----------

#[test]
fn rls_boundary_holds_for_expression_positions() {
    let (addr, _g) = setup("wire-sql26-rls");
    let mut alice = connect(addr, "alice");
    let mut bob = connect(addr, "bob");
    let mut carol = connect(addr, "carol");

    let secret = format!("{AGG} HAVING lower(lang) = 'zz-secret'");
    let out = simple(&mut alice, &secret);
    assert_eq!(out.sqlstate, None);
    assert!(out.rows.is_empty());
    assert_eq!(out.tag, "SELECT 0");
    // 対照: 所有テナントには見える。
    let out = simple(&mut carol, &secret);
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0], cell("zz-secret"));

    for stream in [&mut alice, &mut bob] {
        let out = simple(
            stream,
            "SELECT id FROM docs ORDER BY EXTRACT(year FROM d), id LIMIT 100",
        );
        assert!(!id_column(&out).contains(&"100".to_string()));
        let out = simple(stream, &format!("{AGG} ORDER BY lower(lang) DESC"));
        assert_eq!(id_column(&out), ["ja", "fr", "en"]);
    }

    // 評価エラー本文に他テナントの値が混入しない。
    let out = simple(
        &mut alice,
        "SELECT id FROM docs ORDER BY abs(1 / (n - n)) LIMIT 100",
    );
    assert_eq!(out.sqlstate.as_deref(), Some("22012"));
    let text = out.error_text.expect("error text");
    assert!(!text.contains("AAA-secret") && !text.contains("zz-secret"));

    // 拡張クエリでも他テナントの Private 行は束縛値で引き当てられない。
    let out = extended(
        &mut bob,
        "SELECT lang, COUNT(*) AS c FROM docs WHERE lang = $1 GROUP BY lang",
        &["zz-secret"],
    );
    assert_eq!(out.sqlstate, None);
    assert!(out.rows.is_empty());
    assert_eq!(out.tag, "SELECT 0");
}
