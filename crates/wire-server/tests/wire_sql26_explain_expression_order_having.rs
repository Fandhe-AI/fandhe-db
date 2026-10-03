//! `EXPLAIN` と式の `ORDER BY`／`HAVING` の併用が、PostgreSQL wire プロトコル v3 の
//! 簡易クエリ・拡張クエリ双方で受理され、engine のセッション経由入口と同じ契約
//! （静的判定の QUERY PLAN 行のみ・非 EXPLAIN と同じ SQLSTATE 分類・RLS 非漏えい）に
//! なることを固定する結合テスト（Issue #1350。ポインタ: `docs/spec/05-tasks.md` TASK-210・
//! `docs/spec/04-behavior/sql-surface.md` SQL-26・SQL-27、関連: RLS-7・ERR-1・ERR-2）。
//!
//! 責務境界: engine 側の同名契約は `crates/engine/tests/sql26_order_by_having_expressions.rs`
//! （`explain_accepts_expression_order_by_and_having_in_session` ほか）が固定する。
//! 本ファイルは wire フレーミング越しに同じ契約を再確認する。生バイトクライアントの
//! ヘルパーは `wire_sql26_extract_order_having.rs` と同じ流儀で複製している。

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

// ---------- テスト ----------

const SCAN_EXPR: &str = "SELECT id FROM docs ORDER BY lower(title) LIMIT 5";
const SCAN_MIXED: &str = "SELECT id FROM docs ORDER BY lang DESC, lower(title) LIMIT 5";
const AGG_HAVING: &str =
    "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang HAVING lower(lang) = 'ja'";
const AGG_ORDER: &str =
    "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY lower(lang) DESC";
const AGG_PLAIN: &str = "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang";

fn plan_lines(out: &Outcome) -> Vec<String> {
    out.rows
        .iter()
        .map(|r| r.first().cloned().flatten().expect("plan line"))
        .collect()
}

fn assert_explain_ok(out: &Outcome, sql: &str) {
    assert_eq!(out.sqlstate, None, "{sql}: {out:?}");
    assert_eq!(out.tag, "EXPLAIN", "{sql}");
    assert_eq!(
        out.columns.first().map(|c| c.0.as_str()),
        Some("QUERY PLAN"),
        "{sql}"
    );
}

#[test]
fn explain_with_expression_order_by_and_having_is_accepted_in_both_protocols() {
    let (addr, _g) = setup("wire-sql26-explain-accept");
    let mut s = connect(addr, "alice");
    let scan_expected = vec![
        "scalar_plan: plain_scan".to_string(),
        "access_path: full_scan".to_string(),
    ];
    let agg_base = simple(&mut s, &format!("EXPLAIN {AGG_PLAIN}"));
    assert_explain_ok(&agg_base, AGG_PLAIN);
    for sql in [SCAN_EXPR, SCAN_MIXED, AGG_HAVING, AGG_ORDER] {
        let explain = format!("EXPLAIN {sql}");
        let simple_out = simple(&mut s, &explain);
        assert_explain_ok(&simple_out, &explain);
        if sql.contains("GROUP BY") {
            // 集計の行内容は式の有無で変わらない（静的判定のみ）。
            assert_eq!(plan_lines(&simple_out), plan_lines(&agg_base), "{explain}");
        } else {
            assert_eq!(plan_lines(&simple_out), scan_expected, "{explain}");
        }
        // 拡張クエリは簡易クエリと同じ応答になる。
        let ext_out = extended(&mut s, &explain, &[]);
        assert_eq!(ext_out, simple_out, "{explain}");
        // Describe(Statement) も QUERY PLAN の RowDescription を返す。
        let described = describe_statement(&mut s, &explain);
        assert_eq!(described.sqlstate, None, "{explain}");
        assert_eq!(
            described.columns.first().map(|c| c.0.as_str()),
            Some("QUERY PLAN"),
            "{explain}"
        );
    }
}

#[test]
fn explain_expression_errors_match_non_explain_in_both_protocols() {
    let (addr, _g) = setup("wire-sql26-explain-parity");
    let mut s = connect(addr, "alice");
    let bodies = [
        "SELECT id FROM docs ORDER BY lower(nope) LIMIT 3".to_string(),
        "SELECT id FROM docs ORDER BY vec_div(embedding, 2) LIMIT 3".to_string(),
        "SELECT id FROM docs ORDER BY count(*) LIMIT 3".to_string(),
        "SELECT DISTINCT lang FROM docs ORDER BY lower(lang)".to_string(),
        "SELECT id FROM missing ORDER BY lower(title) LIMIT 3".to_string(),
        format!("{AGG} HAVING count(*) > 1"),
        format!("{AGG} HAVING lang = 'ja'"),
        format!("{AGG} HAVING lower(title) = 'a'"),
        format!("{AGG} ORDER BY lower(title)"),
        // 式位置の `$n` は EXPLAIN 有無ともに同じ拒否になる（受理の拡張はしない）。
        "SELECT id FROM docs ORDER BY lower($1) LIMIT 3".to_string(),
    ];
    for body in &bodies {
        let plain = simple(&mut s, body);
        let code = plain
            .sqlstate
            .clone()
            .unwrap_or_else(|| panic!("{body} must fail"));
        let explained = simple(&mut s, &format!("EXPLAIN {body}"));
        assert_eq!(
            explained.sqlstate.as_deref(),
            Some(code.as_str()),
            "simple {body}"
        );
        assert!(explained.rows.is_empty(), "{body}");
        let ext_plain = extended(&mut s, body, &[]);
        let ext_explained = extended(&mut s, &format!("EXPLAIN {body}"), &[]);
        assert_eq!(
            ext_plain.sqlstate.as_deref(),
            Some(code.as_str()),
            "ext {body}"
        );
        assert_eq!(
            ext_explained.sqlstate.as_deref(),
            Some(code.as_str()),
            "ext explain {body}"
        );
        // エラー後もセッションが継続する。
        let next = simple(&mut s, &format!("EXPLAIN {SCAN_EXPR}"));
        assert_explain_ok(&next, SCAN_EXPR);
    }
}

#[test]
fn explain_does_not_evaluate_expression_body() {
    let (addr, _g) = setup("wire-sql26-explain-noeval");
    let mut s = connect(addr, "alice");
    let body = "SELECT id FROM docs ORDER BY abs(1 / (n - n)) LIMIT 100";
    assert_eq!(simple(&mut s, body).sqlstate.as_deref(), Some("22012"));
    let out = simple(&mut s, &format!("EXPLAIN {body}"));
    assert_explain_ok(&out, body);
}

#[test]
fn explain_response_is_identical_across_tenant_visibility_contexts() {
    let (addr, _g) = setup("wire-sql26-explain-rls");
    // alice: 自テナント行あり / bob: 他テナントの行が見えない / carol: Private 行の所有者。
    for sql in [SCAN_EXPR, AGG_HAVING, AGG_ORDER] {
        let explain = format!("EXPLAIN {sql}");
        let mut reference: Option<Outcome> = None;
        for user in ["alice", "bob", "carol"] {
            let mut s = connect(addr, user);
            for out in [simple(&mut s, &explain), extended(&mut s, &explain, &[])] {
                assert_explain_ok(&out, &explain);
                let text = format!("{out:?}");
                assert!(!text.contains("zz-secret"), "{explain} leaked for {user}");
                match &reference {
                    None => reference = Some(out),
                    Some(r) => assert_eq!(&out, r, "{explain} differs for {user}"),
                }
            }
        }
    }
}
