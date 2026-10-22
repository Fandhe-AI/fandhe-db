//! 非等価・`BETWEEN` の `$n`（Issue #2432、親 #2429。ポインタ: TASK-336・SQL-78・WIRE-12・
//! WIRE-24）を、実際の wire 経路（生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容:
//! - Describe(Statement) の `ParameterDescription` が、比較相手の列型の OID を返すこと
//!   （`SELECT`・述語形 `UPDATE`／`DELETE` 共通。存在しない列・表は `42703`／`42P01`、`$n op $m` は
//!   宣言が無ければ `42P18`、宣言 OID があればそのまま echo）。いずれも既存の汎用公告経路
//!   （`extended_query::build_param_slots`）の挙動を固定するもので、プロダクションコードは変えない。
//! - NULL（長さ -1）を Bind した結果（DataRow・CommandComplete タグ・SQLSTATE）が、同じ接続の
//!   簡易クエリで同じ位置にリテラル `NULL` を書いた形と一致すること。合わせる先はリテラル形の
//!   現状であり、PostgreSQL の挙動ではない（リテラル側の仕様は別課題）。
//! - binary 非対応スロットへの NULL が `0A000` のまま（現行挙動の固定。fail-closed を緩めない）。
//!
//! engine 層の同値性は `crates/engine/tests/prepared_params.rs`
//! （`bind_prepared_null_matches_literal_null_in_noneq_between`）。3 クライアントの層 B と
//! RLS 3 テナントの網羅（非 NULL 値のリテラル形との一致）は Issue #2433 の担当で、層 A は
//! `wire_sql78_noneq_between_rls.rs`、層 B は `three_client_extended_e2e.rs` が持つ。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use common::*;

type Stream = std::net::TcpStream;

const OID_INT4: i32 = 23;
const OID_INT8: i32 = 20;
const OID_FLOAT8: i32 = 701;
const OID_BOOL: i32 = 16;
const OID_TEXT: i32 = 25;
const OID_UUID: i32 = 2950;
const OID_DATE: i32 = 1082;

fn uuid_text(i: u8) -> String {
    format!("00000000-0000-0000-0000-00000000000{i}")
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql78-noneq-between-param");
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
                ColumnDef::new("f", ColumnType::Boolean, true),
                ColumnDef::new("u", ColumnType::Uuid, true),
                ColumnDef::new("dt", ColumnType::Date, true),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    seed(&core, 1..=3);
    (core, guard)
}

fn seed(core: &EngineCore, ids: std::ops::RangeInclusive<u8>) {
    let mut session = SessionState::default();
    for id in ids {
        core.execute_sql_in_session(
            &PolicyContext::new("tenant-a").expect("tenant"),
            &mut session,
            &format!(
                "INSERT INTO t (id, n, b, d, s, f, u, dt) VALUES ({id}, {id}0, {id}00, {id}.5, \
                 's{id}', {flag}, '{u}', '2024-01-0{id}') USING OPERATION_ID 'seed-{id}'",
                flag = if id % 2 == 1 { "TRUE" } else { "FALSE" },
                u = uuid_text(id),
            ),
        )
        .expect("seed insert");
    }
}

fn snapshot(core: &EngineCore) -> Vec<Vec<Cell>> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &PolicyContext::with_visibilities(
                "tenant-a",
                [Visibility::Public, Visibility::Private],
            )
            .expect("tenant"),
            &mut session,
            "SELECT n, b, d, s, f, u, dt FROM t ORDER BY id LIMIT 100",
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn connect_as(core: Arc<EngineCore>, user: &str) -> Stream {
    let users = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users, core);
    let pw = if user == "alice" {
        "correct-horse"
    } else {
        "battery-staple"
    };
    authenticate_to_ready_for_query(addr, user, pw)
}

fn connect(core: Arc<EngineCore>) -> Stream {
    connect_as(core, "alice")
}

fn describe_statement_msg(name: &str) -> Vec<u8> {
    let mut body = vec![b'S'];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

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

fn parse_declared(name: &str, sql: &str, oids: &[i32]) -> Vec<u8> {
    let mut body = parse_body(name, sql, oids.len() as i16);
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
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

/// 1 往復分の応答の要約。DataRow 本体・CommandComplete タグ・最初の ErrorResponse の
/// SQLSTATE／全文を保持し、拡張クエリと簡易クエリの結果比較に使う。
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    rows: Vec<Vec<u8>>,
    tag: String,
    sqlstate: Option<String>,
}

/// ReadyForQuery までの応答を読み切って要約する（ErrorResponse の全文は `err_text` へ）。
fn collect_until_ready(s: &mut Stream, err_text: &mut String) -> Outcome {
    let mut out = Outcome {
        rows: Vec::new(),
        tag: String::new(),
        sqlstate: None,
    };
    loop {
        let (k, body) = read_message(s);
        match k {
            b'D' => out.rows.push(body),
            b'C' => {
                out.tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'E' if out.sqlstate.is_none() => {
                let text = String::from_utf8_lossy(&body).to_string();
                out.sqlstate = text
                    .split('\0')
                    .find_map(|f| f.strip_prefix('C').map(str::to_string));
                *err_text = text;
            }
            b'Z' => return out,
            _ => {}
        }
    }
}

/// 宣言 OID 付き Parse → Bind → Execute → Sync を送って応答を要約する。
fn run_extended(
    s: &mut Stream,
    sql: &str,
    oids: &[i32],
    fmts: &[i16],
    values: &[Option<&[u8]>],
) -> (Outcome, String) {
    send_length_prefixed_message(s, b'P', &parse_declared("", sql, oids));
    send_length_prefixed_message(s, b'B', &bind_msg_opt(fmts, values));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    let mut err = String::new();
    let out = collect_until_ready(s, &mut err);
    (out, err)
}

/// 簡易クエリ（リテラル形）を送って応答を要約する。
fn run_simple(s: &mut Stream, sql: &str) -> Outcome {
    send_simple_query(s, sql);
    let mut err = String::new();
    collect_until_ready(s, &mut err)
}

/// Parse → Describe(S) の ParameterDescription の OID を返す。Parse が失敗した場合は
/// SQLSTATE を `Err` で返し、Sync で回復する。
fn try_describe_oids(s: &mut Stream, sql: &str, declared: &[i32]) -> Result<Vec<i32>, String> {
    // SELECT は LIMIT 必須（SQL 表層の契約）なので、テスト側で付けて本題の `$n` に集中する。
    let sql = if sql.starts_with("SELECT") {
        format!("{sql} LIMIT 5")
    } else {
        sql.to_string()
    };
    send_length_prefixed_message(s, b'P', &parse_declared("d", &sql, declared));
    send_length_prefixed_message(s, b'D', &describe_statement_msg("d"));
    send_length_prefixed_message(s, b'S', b"");
    let mut oids: Option<Vec<i32>> = None;
    let mut err: Option<String> = None;
    loop {
        let (k, body) = read_message(s);
        match k {
            b't' => oids = Some(param_description_oids(&body)),
            b'E' if err.is_none() => {
                err = Some(
                    String::from_utf8_lossy(&body)
                        .split('\0')
                        .find_map(|f| f.strip_prefix('C').map(str::to_string))
                        .unwrap_or_default(),
                )
            }
            b'Z' => break,
            _ => {}
        }
    }
    // Parse 成功・Describe 失敗の場合に名前付き文が残らないよう、成否によらず Close する。
    send_length_prefixed_message(s, b'C', &describe_statement_msg("d"));
    send_length_prefixed_message(s, b'S', b"");
    loop {
        let (k, _) = read_message(s);
        if k == b'Z' {
            break;
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    Ok(oids.expect("ParameterDescription"))
}

fn describe_oids(s: &mut Stream, sql: &str) -> Vec<i32> {
    try_describe_oids(s, sql, &[]).unwrap_or_else(|e| panic!("describe failed ({e}): {sql}"))
}

fn assert_alive(s: &mut Stream) {
    let out = run_simple(s, "SELECT id FROM t LIMIT 1");
    assert_eq!(out.sqlstate, None);
}

/// 列名と、その列の OID（`ParameterDescription` で期待する値）。
const COLS: [(&str, i32); 7] = [
    ("n", OID_INT4),
    ("b", OID_INT8),
    ("d", OID_FLOAT8),
    ("s", OID_TEXT),
    ("f", OID_BOOL),
    ("u", OID_UUID),
    ("dt", OID_DATE),
];

const OPS: [&str; 5] = ["<", ">", "<=", ">=", "<>"];

// SQL-78・WIRE-12: 非等価・BETWEEN の `$n` は比較相手の列型で公告される。
#[test]
fn noneq_and_between_params_are_described_with_column_types() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    // 本フィクスチャ（`TableSchema::new` による `LegacyId` 行識別の表）に限った現行挙動の固定:
    // BOOLEAN 列の比較全般と TEXT 列の BETWEEN は、リテラル形（`f <> TRUE`・
    // `s BETWEEN 'a' AND 'm'`）でもこの表では `22000` になり、`$n` 形も同じ
    // （fail-closed。リテラル形との一致を固定する）。SQL の CREATE TABLE で作った表では
    // これらも受理・描述される（`crates/engine/tests/sql78_where_noneq_between_params.rs`）。
    let rejected = |col: &str, between: bool| (col == "f" && !between) || (col == "s" && between);
    for (col, oid) in COLS {
        for op in OPS {
            let got = try_describe_oids(
                &mut s,
                &format!("SELECT id FROM t WHERE {col} {op} $1"),
                &[],
            );
            let want = if rejected(col, false) {
                Err("22000".to_string())
            } else {
                Ok(vec![oid])
            };
            assert_eq!(got, want, "{col} {op}");
        }
        for neg in ["", "NOT "] {
            let got = try_describe_oids(
                &mut s,
                &format!("SELECT id FROM t WHERE {col} {neg}BETWEEN $1 AND $2"),
                &[],
            );
            let want = if rejected(col, true) {
                Err("22000".to_string())
            } else {
                Ok(vec![oid, oid])
            };
            assert_eq!(got, want, "{col} {neg}BETWEEN");
        }
    }
    // 述語形 UPDATE／DELETE。
    assert_eq!(
        describe_oids(&mut s, "UPDATE t SET s = 'x' WHERE n > $1"),
        vec![OID_INT4]
    );
    assert_eq!(
        describe_oids(&mut s, "DELETE FROM t WHERE dt BETWEEN $1 AND $2"),
        vec![OID_DATE, OID_DATE]
    );
    // 複合条件では `$n` ごとに別々の列の型になる。
    assert_eq!(
        describe_oids(&mut s, "SELECT id FROM t WHERE n > $1 AND s <= $2"),
        vec![OID_INT4, OID_TEXT]
    );
    assert_eq!(
        describe_oids(
            &mut s,
            "SELECT id FROM t WHERE b <> $1 OR d BETWEEN $2 AND $3"
        ),
        vec![OID_INT8, OID_FLOAT8, OID_FLOAT8]
    );
    assert_alive(&mut s);
}

// SQL-78: 疑似列 `id` の非等価も等価形 `id = $1` と同じ公告になる（数値は決め打ちしない）。
#[test]
fn pseudo_id_noneq_param_is_described_like_equality() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let eq = describe_oids(&mut s, "SELECT n FROM t WHERE id = $1");
    assert_eq!(eq.len(), 1);
    for op in OPS {
        assert_eq!(
            describe_oids(&mut s, &format!("SELECT n FROM t WHERE id {op} $1")),
            eq,
            "id {op}"
        );
    }
    assert_eq!(
        describe_oids(&mut s, "SELECT n FROM t WHERE id BETWEEN $1 AND $2"),
        vec![eq[0], eq[0]]
    );
    assert_alive(&mut s);
}

// WIRE-12・WIRE-24: 存在しない列・表は Parse で `42703`／`42P01`（text への fallback はしない。
// fail-closed）、`$n op $m` は宣言が無ければ `42P18`、宣言 OID があればそのまま echo する。
// エラー後は Sync で回復し接続が続く。
#[test]
fn unknown_names_fail_closed_and_param_pair_needs_declaration() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(
        try_describe_oids(&mut s, "SELECT id FROM t WHERE nosuchcol > $1", &[]),
        Err("42703".to_string())
    );
    assert_eq!(
        try_describe_oids(&mut s, "SELECT id FROM nosuch WHERE n > $1", &[]),
        Err("42P01".to_string())
    );
    assert_alive(&mut s);
    assert_eq!(
        try_describe_oids(&mut s, "SELECT id FROM t WHERE $1 < $2", &[]),
        Err("42P18".to_string())
    );
    assert_alive(&mut s);
    assert_eq!(
        try_describe_oids(
            &mut s,
            "SELECT id FROM t WHERE $1 < $2",
            &[OID_INT4, OID_INT4]
        ),
        Ok(vec![OID_INT4, OID_INT4])
    );
    assert_alive(&mut s);
}

/// NULL を Bind した結果が、同じ位置へリテラル `NULL` を書いた簡易クエリと一致することを確かめ、
/// 双方が拒否なら接続が維持されることも見る。戻り値は受理されたか。
fn assert_null_parity(
    s: &mut Stream,
    template: &str,
    values: &[Option<&[u8]>],
    literal: &str,
) -> bool {
    let (ext, err_text) = run_extended(s, template, &[], &[], values);
    let lit = run_simple(s, literal);
    assert_eq!(ext, lit, "bind vs literal: {template}");
    // ErrorResponse の本文が非 NULL の Bind 値（s1 等の識別しやすい値）を含まない。
    for v in values.iter().flatten().filter(|v| v.len() >= 4) {
        let v = String::from_utf8_lossy(v);
        assert!(
            !err_text.contains(v.as_ref()),
            "error leaks value: {err_text}"
        );
    }
    assert_alive(s);
    ext.sqlstate.is_none()
}

// SQL-78・WIRE-12: NULL Bind はリテラル `NULL` の簡易クエリと同じ結果（行・タグ・SQLSTATE）になる。
#[test]
fn null_bind_matches_simple_query_literal_form() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    for (col, _) in COLS {
        for op in OPS {
            // 受理か拒否かはリテラル形の現状に従う（一致だけを固定する）。
            assert_null_parity(
                &mut s,
                &format!("SELECT id FROM t WHERE {col} {op} $1 LIMIT 5"),
                &[None],
                &format!("SELECT id FROM t WHERE {col} {op} NULL LIMIT 5"),
            );
        }
        let v: &[u8] = match col {
            "n" | "b" => b"20",
            "d" => b"2.5",
            "s" => b"s2",
            "f" => b"true",
            "u" => b"00000000-0000-0000-0000-000000000002",
            _ => b"2024-01-02",
        };
        let lit = std::str::from_utf8(v).expect("utf8");
        let q = |lo: &str, hi: &str, neg: &str| {
            format!("SELECT id FROM t WHERE {col} {neg}BETWEEN {lo} AND {hi} LIMIT 5")
        };
        let quote = |x: &str| match col {
            "n" | "b" | "d" | "f" => x.to_string(),
            _ => format!("'{x}'"),
        };
        for neg in ["", "NOT "] {
            let tpl = format!("SELECT id FROM t WHERE {col} {neg}BETWEEN $1 AND $2 LIMIT 5");
            assert_null_parity(&mut s, &tpl, &[None, Some(v)], &q("NULL", &quote(lit), neg));
            assert_null_parity(&mut s, &tpl, &[Some(v), None], &q(&quote(lit), "NULL", neg));
            assert_null_parity(&mut s, &tpl, &[None, None], &q("NULL", "NULL", neg));
        }
    }
    // 等価と非等価の混在。
    assert_null_parity(
        &mut s,
        "SELECT id FROM t WHERE n > $1 AND s = $2 LIMIT 5",
        &[None, Some(b"s1")],
        "SELECT id FROM t WHERE n > NULL AND s = 's1' LIMIT 5",
    );
    // 値 `NULL`（4 文字のテキスト）は SQL の NULL と区別される（インジェクション観点）。
    let text_null = assert_null_parity(
        &mut s,
        "SELECT id FROM t WHERE s <> $1 LIMIT 5",
        &[Some(b"NULL")],
        "SELECT id FROM t WHERE s <> 'NULL' LIMIT 5",
    );
    assert!(text_null);
    // 別テナントの接続でも応答の形は同じ（存在情報が出ない）。
    let (core2, _g2) = new_core();
    let mut bob = connect_as(core2, "bob");
    assert_null_parity(
        &mut bob,
        "SELECT id FROM t WHERE n > $1 LIMIT 5",
        &[None],
        "SELECT id FROM t WHERE n > NULL LIMIT 5",
    );
}

// SQL-78・WIRE-12: 述語形 UPDATE／DELETE でも NULL Bind はリテラル形と同じ結果になり、表が変わらない。
#[test]
fn null_bind_in_predicate_dml_matches_literal_and_leaves_table_unchanged() {
    let (core, _g) = new_core();
    let before = snapshot(&core);
    let mut s = connect(core.clone());
    assert_null_parity(
        &mut s,
        "UPDATE t SET s = 'x' WHERE n > $1",
        &[None],
        "UPDATE t SET s = 'x' WHERE n > NULL",
    );
    assert_null_parity(
        &mut s,
        "DELETE FROM t WHERE n <= $1",
        &[None],
        "DELETE FROM t WHERE n <= NULL",
    );
    assert_null_parity(
        &mut s,
        "DELETE FROM t WHERE dt BETWEEN $1 AND $2",
        &[None, Some(b"2024-01-02")],
        "DELETE FROM t WHERE dt BETWEEN NULL AND '2024-01-02'",
    );
    assert_eq!(
        snapshot(&core),
        before,
        "NULL predicate must not change rows"
    );
}

// WIRE-12: binary 非対応スロット（DATE）への NULL は、非等価の位置でも `0A000` のまま。
// 仕様準拠の固定ではなく現行挙動の固定であり、緩めない（fail-closed）。
#[test]
fn binary_format_null_on_unsupported_slot_stays_0a000() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    let (out, _) = run_extended(
        &mut s,
        "SELECT id FROM t WHERE dt > $1 LIMIT 5",
        &[],
        &[1],
        &[None],
    );
    assert_eq!(out.sqlstate.as_deref(), Some("0A000"));
    assert_alive(&mut s);
}
