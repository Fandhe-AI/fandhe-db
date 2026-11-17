//! LATERAL 派生表の wire 越し検証（Issue #3324。対象ビヘイビア: SQL-62 (7)。ポインタ: TASK-344・WIRE-11・
//! WIRE-12・RLS-10。親 #3319）。
//!
//! 責務境界: LATERAL の意味（再評価・NULL 補完・RLS・上限）は engine の `sql62_lateral_*`・
//! `rls10_lateral_paths` が確定オラクルである。本ファイルは簡易クエリ（psql 相当の末尾 `;`）・`EXPLAIN`・
//! 拡張クエリ（LATERAL 本文の `$n`）が wire フレーミング越しに `RowDescription`・`DataRow`・
//! `CommandComplete`・`ErrorResponse` として観測でき、エラー後も接続を継続できることを固定する。

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

/// 外側 `docs` と LATERAL の内側が同じ表。`(id, lang)`。ja = {1, 2}・en = {3}。
const ROWS: [(u64, &str); 3] = [(1, "ja"), (2, "ja"), (3, "en")];

fn setup(name: &str) -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(name);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, lang) in ROWS {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
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

/// PostgreSQL の型 OID（text）。
const OID_TEXT: i32 = 25;

const LATERAL_BODY: &str =
    "LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang ORDER BY lang LIMIT 2) t";

#[test]
fn simple_query_runs_lateral_forms_with_a_trailing_semicolon() {
    let (addr, _g) = setup("wire-lateral-simple");
    let mut s = connect(addr, "alice");
    // 単独送信・末尾 `;` 付き（psql 相当）。外側 3 行 × 内側（同じ lang の先頭 2 行）。
    let base =
        format!("SELECT d.id, t.lang FROM docs d, {LATERAL_BODY} ORDER BY d.id, t.lang LIMIT 100");
    let plain = simple(&mut s, &base);
    assert_eq!(plain.sqlstate, None, "{plain:?}");
    assert_eq!(plain.columns.len(), 2);
    // ja の 2 行は各 2 件、en の 1 行は 1 件。
    assert_eq!(plain.rows.len(), 5, "{plain:?}");
    assert_eq!(plain.tag, "SELECT 5");
    let with_semicolon = simple(&mut s, &format!("{base};"));
    assert_eq!(with_semicolon, plain);
    // LEFT JOIN LATERAL ... ON true は外側を保持する（一致の無い行がなくても件数は同じ）。
    let left = simple(
        &mut s,
        &format!("SELECT d.id, t.lang FROM docs d LEFT JOIN {LATERAL_BODY} ON true ORDER BY d.id, t.lang LIMIT 100;"),
    );
    assert_eq!(left.sqlstate, None, "{left:?}");
    assert_eq!(left.rows, plain.rows);
}

#[test]
fn explain_of_lateral_returns_the_closed_vocabulary_rows() {
    let (addr, _g) = setup("wire-lateral-explain");
    let mut s = connect(addr, "alice");
    let out = simple(
        &mut s,
        &format!("EXPLAIN SELECT d.id FROM docs d, {LATERAL_BODY}"),
    );
    assert_eq!(out.sqlstate, None, "{out:?}");
    assert_eq!(
        out.columns.first().map(|c| c.0.as_str()),
        Some("QUERY PLAN")
    );
    let lines: Vec<String> = out
        .rows
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .collect();
    assert!(
        lines.contains(&"join_method: lateral".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"reevaluation: per_outer_row".to_string()),
        "{lines:?}"
    );
    let all = lines.join("\n");
    assert!(
        !all.contains("docs") && !all.contains('\u{2}') && !all.contains("lateral0"),
        "{all}"
    );
    assert_eq!(out.tag, "EXPLAIN");
}

#[test]
fn extended_query_binds_a_param_in_the_lateral_body_like_the_literal_form() {
    let (addr, _g) = setup("wire-lateral-ext");
    let mut s = connect(addr, "alice");
    let param = "SELECT d.id, t.lang FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang AND x.lang = $1 ORDER BY lang LIMIT 2) t ORDER BY d.id, t.lang LIMIT 100".to_string();
    let literal = param.replace("$1", "'ja'");
    // Parse → Describe(Statement): ParameterDescription は text、RowDescription は 2 列。
    send(&mut s, b'P', &parse_msg("", &param));
    send(&mut s, b'D', &describe_msg(b'S', ""));
    send(&mut s, b'S', b"");
    let described = collect(&mut s);
    assert_eq!(described.sqlstate, None, "{described:?}");
    assert_eq!(described.param_oids, Some(vec![OID_TEXT]));
    assert_eq!(described.columns.len(), 2);
    // Parse → Bind → Describe(Portal) → Execute: 結果は簡易クエリ（リテラル）と一致する。
    let ext = extended(&mut s, &param, &["ja"]);
    assert_eq!(ext.sqlstate, None, "{ext:?}");
    let sim = simple(&mut s, &literal);
    assert_eq!(ext.columns, sim.columns);
    assert_eq!(ext.rows, sim.rows);
    assert_eq!(ext.tag, sim.tag);
    assert!(!ext.rows.is_empty());
}

#[test]
fn params_outside_the_lateral_body_are_rejected_and_the_session_continues() {
    let (addr, _g) = setup("wire-lateral-param-reject");
    let mut s = connect(addr, "alice");
    for sql in [
        format!("SELECT d.id FROM docs d, {LATERAL_BODY} WHERE d.lang = $1"),
        format!("SELECT d.id FROM docs d, {LATERAL_BODY} LIMIT $1"),
    ] {
        let out = extended(&mut s, &sql, &["ja"]);
        assert_eq!(out.sqlstate.as_deref(), Some("42601"), "{sql}: {out:?}");
        // 接続は継続し、後続の簡易クエリが成功する。
        let ok = simple(&mut s, "SELECT id FROM docs ORDER BY id LIMIT 1");
        assert_eq!(ok.sqlstate, None, "{ok:?}");
    }
}

// 本文が SELECT 以外の LATERAL は固定 `0A000` で拒否され、接続は継続する。
// intentional: docs/design/lateral-derived-table.md
#[test]
fn unsupported_lateral_shape_returns_a_fixed_error_and_keeps_the_connection() {
    let (addr, _g) = setup("wire-lateral-unsupported");
    let mut s = connect(addr, "alice");
    let out = simple(&mut s, "SELECT * FROM docs d, LATERAL (VALUES (1)) t");
    assert_eq!(out.sqlstate.as_deref(), Some("0A000"), "{out:?}");
    assert!(!out.error_text.unwrap_or_default().contains('\u{2}'));
    let ok = simple(&mut s, "SELECT id FROM docs ORDER BY id LIMIT 1");
    assert_eq!(ok.sqlstate, None, "{ok:?}");
}
