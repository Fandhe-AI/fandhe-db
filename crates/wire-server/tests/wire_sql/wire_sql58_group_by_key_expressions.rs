//! `GROUP BY` キー式（`GROUP BY n / 2`・`GROUP BY upper(lang)` 等）が、PostgreSQL wire プロトコル v3 の
//! 簡易クエリ・拡張クエリ双方の生バイトクライアント越しに契約どおりの応答として観測できることを
//! 検証する結合テスト（Issue #1817。対象ビヘイビア: SQL-58。ポインタ: `docs/spec/04-behavior/` の
//! SQL-58、関連: WIRE-11・WIRE-12、ERR-1・ERR-2）。
//!
//! 責務境界: グループ化の結果・エラー分類の規則そのものは engine 側の
//! `crates/engine/tests/sql58_group_by_key_expressions.rs`（in-process）が確定オラクルである。
//! 本ファイルは `RowDescription`・`DataRow` の並び、`CommandComplete` のタグ、`ErrorResponse` の
//! SQLSTATE を wire フレーミング越しに固定する。

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

/// `(id, lang, n)`。ja = {3, NULL, 2}（count 3・sum 5）、en = {1, 5, 0}（count 3・sum 6）、
/// fr = {4, 3}（count 2・sum 7）。
const ROWS: [(u64, &str, Option<i32>); 8] = [
    (1, "ja", Some(3)),
    (2, "en", Some(1)),
    (3, "ja", None),
    (4, "en", Some(5)),
    (5, "ja", Some(2)),
    (6, "fr", Some(4)),
    (7, "en", Some(0)),
    (8, "fr", Some(3)),
];

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
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, lang, n) in ROWS {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
                n.map(Value::Integer).unwrap_or(Value::Null),
            ],
            &OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users = write_user_store_file(&[("alice", "tenant-a", "pw")]);
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

#[test]
fn simple_query_groups_by_an_expression_key() {
    let (addr, _g) = setup("wire-sql58-gk-simple");
    let mut s = connect(addr, "alice");

    // n / 2（整数除算）: 0={r2,r7} 1={r1,r5,r8} 2={r4,r6} NULL={r3}。キー昇順・NULL 末尾。
    let out = simple(&mut s, "SELECT COUNT(*) AS c FROM docs GROUP BY n / 2");
    assert_eq!(out.sqlstate, None);
    assert_eq!(
        out.columns.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        ["c"]
    );
    let counts: Vec<_> = out.rows.iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(counts, ["2", "3", "2", "1"]);
    assert_eq!(out.tag, "SELECT 4");

    // 関数キー: upper(lang) の 3 グループ（sum(n) 昇順）。
    let out = simple(
        &mut s,
        "SELECT COUNT(*), SUM(n) FROM docs GROUP BY upper(lang) ORDER BY SUM(n)",
    );
    assert_eq!(out.sqlstate, None);
    let rows: Vec<_> = out
        .rows
        .iter()
        .map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap()))
        .collect();
    let t = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(rows, vec![t("3", "5"), t("3", "6"), t("2", "7")]);
}

#[test]
fn extended_query_matches_the_simple_form() {
    let (addr, _g) = setup("wire-sql58-gk-extended");
    let mut s = connect(addr, "alice");
    for sql in [
        "SELECT COUNT(*) AS c FROM docs GROUP BY n / 2",
        "SELECT COUNT(*) AS c, SUM(n) AS s FROM docs GROUP BY upper(lang) ORDER BY SUM(n)",
    ] {
        let simple_out = simple(&mut s, sql);
        let extended_out = extended(&mut s, sql, &[]);
        assert_eq!(extended_out.sqlstate, None, "{sql}");
        assert_eq!(extended_out.columns, simple_out.columns, "{sql}");
        assert_eq!(extended_out.rows, simple_out.rows, "{sql}");
        assert_eq!(extended_out.tag, simple_out.tag, "{sql}");
    }
}

#[test]
fn rejected_forms_return_exact_sqlstate_and_session_continues() {
    let (addr, _g) = setup("wire-sql58-gk-errors");
    let mut s = connect(addr, "alice");
    for (sql, code) in [
        ("SELECT COUNT(*) FROM docs GROUP BY count(*)", "42803"),
        ("SELECT COUNT(*) FROM docs GROUP BY 1", "42601"),
        ("SELECT COUNT(*) FROM docs GROUP BY upper(nope)", "42703"),
        ("SELECT COUNT(*) FROM docs GROUP BY 10 / (n - 3)", "22012"),
        // 式キーとの構造一致は #1818。一致しない列参照は集計の外の参照として 42803。
        (
            "SELECT lang, COUNT(*) FROM docs GROUP BY upper(lang)",
            "42803",
        ),
    ] {
        let out = simple(&mut s, sql);
        assert_eq!(out.sqlstate.as_deref(), Some(code), "{sql}: {out:?}");
        assert!(out.rows.is_empty(), "{sql}");
        assert!(
            !out.error_text.unwrap_or_default().contains('\u{1}'),
            "internal name must not leak: {sql}"
        );
    }
    // エラー後も同じ接続でクエリが通る。
    let out = simple(&mut s, "SELECT COUNT(*) FROM docs GROUP BY n / 2");
    assert_eq!(out.sqlstate, None);
    assert_eq!(out.rows.len(), 4);
}
