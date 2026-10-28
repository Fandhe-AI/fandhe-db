//! 非等価・`BETWEEN` の `$n` の「NULL 以外の値」を、同じ値をリテラルで書いた簡易クエリ（'Q'）の
//! 結果と照合し、RLS 3 テナントで固定する層 A 結合テスト（Issue #2433、親 #2429。ポインタ:
//! TASK-336・SQL-78・SQL-24・SQL-19・WIRE-12）。
//!
//! 固定する内容（プロダクションコードは変えない。拡張クエリ経路と簡易クエリ経路の結果比較だけを担う）:
//! - text 形式の Bind（宣言 OID 0）の結果（DataRow 本体・CommandComplete タグ・SQLSTATE）が、
//!   同じ接続の簡易クエリのリテラル形と一致すること。合わせる先はリテラル形の現状であり、
//!   PostgreSQL の挙動ではない（受理か拒否かも固定せず、一致だけを固定する）。
//! - alice（tenant-a）・bob（tenant-b）・carol（tenant-c・行なし）の結果・拒否応答・DML の影響に、
//!   他テナントの行・存在情報が現れないこと（RLS 相当・fail-closed・`wire_code` 契約）。
//!
//! NULL の同値性・ParameterDescription の型公告は `wire_sql78_noneq_between_param.rs`（Issue #2432）、
//! 無改造 3 クライアントでの同検証は `three_client_extended_e2e.rs`（層 B。Issue #2433 の節）が担う。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use common::*;

type Stream = std::net::TcpStream;

const OID_INT4: i32 = 23;

/// tenant-a と tenant-b が `n` の値域を交互に重ねて持つ（`n > 0` が漏れれば両テナントにまたがる）。
const A_VALUES: [u32; 3] = [1, 5, 9];
const B_VALUES: [u32; 3] = [2, 6, 10];

fn uuid_text(v: u32) -> String {
    format!("00000000-0000-0000-0000-{v:012}")
}

/// 列 `col` の、値番号 `v` に対応するテキスト値（seed の値と同じ導出）。
fn raw_value(col: &str, v: u32) -> String {
    match col {
        "n" => v.to_string(),
        "b" => (v * 100).to_string(),
        "d" => format!("{v}.5"),
        "s" => format!("s{v:02}"),
        "day" => format!("2024-01-{v:02}"),
        "u" => uuid_text(v),
        "flag" => (v % 2 == 1).to_string(),
        other => panic!("unknown column {other}"),
    }
}

/// リテラル形での値の書き方（数値・真偽は素のまま、それ以外は `'..'`。`'` は二重化する）。
fn literal_of(col: &str, raw: &str) -> String {
    match col {
        "n" | "b" | "d" | "flag" => raw.to_string(),
        _ => format!("'{}'", raw.replace('\'', "''")),
    }
}

/// `BETWEEN` の境界のリテラル形。BOOLEAN 列の境界は文字列リテラル（`'true'`）が受理形で、
/// 素の `true`／`false` は `42601`（`$n` の置換側も文字列リテラルへ置換する。Issue #1431 の既存契約）。
fn between_literal_of(col: &str, raw: &str) -> String {
    if col == "flag" {
        format!("'{raw}'")
    } else {
        literal_of(col, raw)
    }
}

const COLS: [&str; 7] = ["n", "b", "d", "s", "day", "u", "flag"];
const OPS: [&str; 6] = ["<", ">", "<=", ">=", "<>", "!="];

fn create_db(tag: &str) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(tag);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(
        &PolicyContext::new("tenant-a").expect("tenant"),
        &mut ddl,
        "CREATE TABLE r (n INTEGER, b BIGINT, d DOUBLE PRECISION, s TEXT, day DATE, u UUID, \
         flag BOOLEAN, label TEXT)",
    )
    .expect("create table");
    for (tenant, prefix, values) in [("tenant-a", "a", A_VALUES), ("tenant-b", "b", B_VALUES)] {
        let mut session = SessionState::default();
        for v in values {
            core.execute_sql_in_session(
                &PolicyContext::new(tenant).expect("tenant"),
                &mut session,
                &format!(
                    "INSERT INTO r (n, b, d, s, day, u, flag, label) VALUES ({n}, {b}, {d}, \
                     '{s}', '{day}', '{u}', {flag}, '{prefix}-{v}') \
                     USING OPERATION_ID 'seed-{prefix}-{v}'",
                    n = raw_value("n", v),
                    b = raw_value("b", v),
                    d = raw_value("d", v),
                    s = raw_value("s", v),
                    day = raw_value("day", v),
                    u = raw_value("u", v),
                    flag = v % 2 == 1,
                ),
            )
            .expect("seed insert");
        }
    }
    (core, guard)
}

const USERS: [(&str, &str, &str); 3] = [
    ("alice", "tenant-a", "pw-alice-2433"),
    ("bob", "tenant-b", "pw-bob-2433"),
    ("carol", "tenant-c", "pw-carol-2433"),
];

fn connect_all(core: Arc<EngineCore>) -> Vec<Stream> {
    let users = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users, core);
    USERS
        .iter()
        .map(|(user, _, pw)| authenticate_to_ready_for_query(addr, user, pw))
        .collect()
}

/// Bind 本体（結果形式はすべて text）。
fn bind_msg(fmts: &[i16], values: &[&[u8]]) -> Vec<u8> {
    let mut body = vec![0u8, 0u8];
    body.extend_from_slice(&(fmts.len() as i16).to_be_bytes());
    for f in fmts {
        body.extend_from_slice(&f.to_be_bytes());
    }
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for v in values {
        body.extend_from_slice(&(v.len() as i32).to_be_bytes());
        body.extend_from_slice(v);
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn parse_declared(sql: &str, oids: &[i32]) -> Vec<u8> {
    let mut body = parse_body("", sql, oids.len() as i16);
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    body
}

/// 1 往復分の応答の要約（DataRow 本体・CommandComplete タグ・最初の ErrorResponse の SQLSTATE）。
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    rows: Vec<Vec<u8>>,
    tag: String,
    sqlstate: Option<String>,
}

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

/// 宣言 OID 付き Parse → Bind → Execute → Sync を送って応答を要約する。ErrorResponse 全文も返す。
fn run_extended(
    s: &mut Stream,
    sql: &str,
    oids: &[i32],
    fmts: &[i16],
    values: &[&[u8]],
) -> (Outcome, String) {
    send_length_prefixed_message(s, b'P', &parse_declared(sql, oids));
    send_length_prefixed_message(s, b'B', &bind_msg(fmts, values));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    let mut err = String::new();
    let out = collect_until_ready(s, &mut err);
    (out, err)
}

fn run_simple(s: &mut Stream, sql: &str) -> (Outcome, String) {
    send_simple_query(s, sql);
    let mut err = String::new();
    let out = collect_until_ready(s, &mut err);
    (out, err)
}

fn assert_alive(s: &mut Stream) {
    let (out, _) = run_simple(s, "SELECT n FROM r LIMIT 1");
    assert_eq!(out.sqlstate, None);
}

/// DataRow（1 列目）のテキスト値。
fn first_text(row: &[u8]) -> String {
    let len = i32::from_be_bytes([row[2], row[3], row[4], row[5]]);
    let len = usize::try_from(len).expect("non-null");
    String::from_utf8_lossy(&row[6..6 + len]).to_string()
}

/// 拡張形（`$n`）とリテラル形の組。`values` は `$1`... に Bind するテキスト値。
struct Case {
    ext: String,
    literal: String,
    values: Vec<String>,
}

/// 単一比較と `[NOT] BETWEEN`（全列）・複合条件・不正値のケース一覧（SELECT）。
fn select_cases() -> Vec<Case> {
    let head = "SELECT label FROM r WHERE";
    let tail = "ORDER BY label LIMIT 100";
    let mut cases = Vec::new();
    for col in COLS {
        let mid = raw_value(col, 5);
        for op in OPS {
            cases.push(Case {
                ext: format!("{head} {col} {op} $1 {tail}"),
                literal: format!("{head} {col} {op} {} {tail}", literal_of(col, &mid)),
                values: vec![mid.clone()],
            });
        }
        let (lo, hi) = (raw_value(col, 2), raw_value(col, 6));
        for neg in ["", "NOT "] {
            cases.push(Case {
                ext: format!("{head} {col} {neg}BETWEEN $1 AND $2 {tail}"),
                literal: format!(
                    "{head} {col} {neg}BETWEEN {} AND {} {tail}",
                    between_literal_of(col, &lo),
                    between_literal_of(col, &hi)
                ),
                values: vec![lo.clone(), hi.clone()],
            });
        }
    }
    // 複合（AND／OR・等価との混在）。
    let compound: [(&str, &str, &str, &str); 3] = [
        ("n > $1 AND s <= $2", "n > 1 AND s <= 's06'", "1", "s06"),
        (
            "n < $1 OR day >= $2",
            "n < 3 OR day >= '2024-01-09'",
            "3",
            "2024-01-09",
        ),
        (
            "n <> $1 AND label = $2",
            "n <> 5 AND label = 'a-1'",
            "5",
            "a-1",
        ),
    ];
    for (ext, lit, v1, v2) in compound {
        cases.push(Case {
            ext: format!("{head} {ext} {tail}"),
            literal: format!("{head} {lit} {tail}"),
            values: vec![v1.to_string(), v2.to_string()],
        });
    }
    // 不正値・インジェクション風の値は不透明なリテラルのまま扱われる（結果はリテラル形に追従）。
    for (col, bad) in [
        ("day", "not-a-date"),
        ("n", "abc"),
        ("u", "zzz"),
        ("s", "x' OR '1'='1"),
    ] {
        cases.push(Case {
            ext: format!("{head} {col} <> $1 {tail}"),
            literal: format!("{head} {col} <> '{}' {tail}", bad.replace('\'', "''")),
            values: vec![bad.to_string()],
        });
    }
    cases
}

fn run_case(s: &mut Stream, case: &Case) -> (Outcome, Outcome) {
    let vals: Vec<&[u8]> = case.values.iter().map(|v| v.as_bytes()).collect();
    let (ext, _) = run_extended(s, &case.ext, &[], &[], &vals);
    let (lit, _) = run_simple(s, &case.literal);
    (ext, lit)
}

// SQL-78・SQL-24・WIRE-12: 非 NULL の Bind は、同じ値のリテラル形の簡易クエリと
// 行・タグ・SQLSTATE が 3 テナントすべてで一致する（受理か拒否かはリテラル形に追従）。
#[test]
fn noneq_between_values_match_literal_form_for_each_tenant() {
    let (core, _g) = create_db("wire-sql78-rls-values");
    let mut conns = connect_all(core);
    let cases = select_cases();
    let mut accepted_nonempty = 0usize;
    for (i, s) in conns.iter_mut().enumerate() {
        for case in &cases {
            let (ext, lit) = run_case(s, case);
            assert_eq!(ext, lit, "tenant #{i}: {} vs {}", case.ext, case.literal);
            if i == 0 && ext.sqlstate.is_none() && !ext.rows.is_empty() {
                accepted_nonempty += 1;
            }
        }
        assert_alive(s);
    }
    // 非 vacuous: 受理され行を返すケースが十分にある（全部拒否・全部空では検証にならない）。
    assert!(
        accepted_nonempty >= 30,
        "too few accepted non-empty cases: {accepted_nonempty}"
    );
    // インジェクション風の値は、全行が「異なる値」として残る（行が増減しない）。
    let inj = cases.last().expect("injection case");
    let (ext, _) = run_case(&mut conns[0], inj);
    assert_eq!(ext.sqlstate, None);
    assert_eq!(ext.rows.len(), A_VALUES.len());
    // binary int4 の Bind と宣言 OID 23 もリテラル形と一致する（互換表内の型）。
    for s in conns.iter_mut() {
        let five = 5i32.to_be_bytes();
        let (ext, _) = run_extended(
            s,
            "SELECT label FROM r WHERE n > $1 ORDER BY label LIMIT 100",
            &[OID_INT4],
            &[1],
            &[&five],
        );
        let (lit, _) = run_simple(
            s,
            "SELECT label FROM r WHERE n > 5 ORDER BY label LIMIT 100",
        );
        assert_eq!(ext, lit, "binary int4");
    }
}

// RLS 相当・SQL-78: 結果は自テナントの行だけ。carol（行なし）は空、他テナントの行は出ない。
#[test]
fn noneq_between_results_do_not_cross_tenants() {
    let (core, _g) = create_db("wire-sql78-rls-cross");
    let mut conns = connect_all(core);
    let probes = [
        (
            "SELECT label FROM r WHERE n > $1 ORDER BY label LIMIT 100",
            "SELECT label FROM r WHERE n > 0 ORDER BY label LIMIT 100",
            vec!["0"],
        ),
        (
            "SELECT label FROM r WHERE n BETWEEN $1 AND $2 ORDER BY label LIMIT 100",
            "SELECT label FROM r WHERE n BETWEEN 0 AND 100 ORDER BY label LIMIT 100",
            vec!["0", "100"],
        ),
        (
            "SELECT label FROM r WHERE day <> $1 ORDER BY label LIMIT 100",
            "SELECT label FROM r WHERE day <> '2000-01-01' ORDER BY label LIMIT 100",
            vec!["2000-01-01"],
        ),
    ];
    // (自テナントの label 接頭辞, 件数)
    let expect = [(Some("a-"), 3usize), (Some("b-"), 3), (None, 0)];
    for (ext_sql, lit_sql, values) in probes {
        let vals: Vec<&[u8]> = values.iter().map(|v| v.as_bytes()).collect();
        for (i, s) in conns.iter_mut().enumerate() {
            let (ext, _) = run_extended(s, ext_sql, &[], &[], &vals);
            let (lit, _) = run_simple(s, lit_sql);
            assert_eq!(ext, lit, "tenant #{i}: {ext_sql}");
            assert_eq!(ext.sqlstate, None, "tenant #{i}: {ext_sql}");
            let labels: Vec<String> = ext.rows.iter().map(|r| first_text(r)).collect();
            assert_eq!(labels.len(), expect[i].1, "tenant #{i}: {labels:?}");
            if let Some(prefix) = expect[i].0 {
                assert!(labels.iter().all(|l| l.starts_with(prefix)), "{labels:?}");
            }
        }
    }
}

// RLS・`wire_code` 契約: 拒否応答の本文が 3 テナントでバイト一致し、テナント id・他テナントの値を含まず、
// 拒否後も接続が維持される。
#[test]
fn noneq_between_rejections_are_identical_across_tenants() {
    let (core, _g) = create_db("wire-sql78-rls-reject");
    let mut conns = connect_all(core);
    let rejects: [(&str, Vec<&str>, &str); 5] = [
        (
            "SELECT label FROM r WHERE nosuchcol > $1 LIMIT 5",
            vec!["1"],
            "42703",
        ),
        (
            "SELECT label FROM nosuch WHERE n > $1 LIMIT 5",
            vec!["1"],
            "42P01",
        ),
        (
            "SELECT label FROM r WHERE day > $1 LIMIT 5",
            vec!["not-a-date"],
            "",
        ),
        (
            "SELECT label FROM r WHERE n BETWEEN $1 AND $2 LIMIT 5",
            vec!["abc", "9"],
            "",
        ),
        // 範囲外の形（左辺が `$n`）は Parse の段で拒否される。
        (
            "SELECT label FROM r WHERE $1 < n LIMIT 5",
            vec!["1"],
            "42601",
        ),
    ];
    let mut bodies: Vec<Vec<String>> = vec![Vec::new(); 3];
    for (i, s) in conns.iter_mut().enumerate() {
        for (sql, values, want) in &rejects {
            let vals: Vec<&[u8]> = values.iter().map(|v| v.as_bytes()).collect();
            let (out, err_text) = run_extended(s, sql, &[], &[], &vals);
            let state = out.sqlstate.clone().expect("rejected");
            if !want.is_empty() {
                assert_eq!(state, *want, "{sql}");
            }
            bodies[i].push(err_text);
            assert_alive(s);
        }
    }
    for i in 1..3 {
        assert_eq!(
            bodies[0], bodies[i],
            "rejection bodies differ for tenant #{i}"
        );
    }
    for body in bodies.iter().flatten() {
        for leak in ["tenant", "a-1", "a-5", "a-9", "b-2", "b-6", "b-10"] {
            assert!(!body.contains(leak), "leak {leak:?} in {body:?}");
        }
    }
}

/// DML 1 件分。拡張形（`$n`）・値・リテラル形・期待タグ。
struct Dml {
    tenant_idx: usize,
    ext: &'static str,
    values: Vec<&'static str>,
    literal: &'static str,
    tag: &'static str,
}

fn snapshot(core: &EngineCore, tenant: &str) -> Vec<String> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("tenant"),
            &mut session,
            "SELECT label, n FROM r ORDER BY n LIMIT 100",
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

// SQL-19・SQL-78: 述語形 UPDATE／DELETE の非等価・BETWEEN の `$n` は、リテラル形と同じ影響件数・
// 同じ結果状態になり、他テナントの行は変わらない。
#[test]
fn predicate_dml_noneq_between_matches_literal_and_respects_tenants() {
    let (core_ext, _g1) = create_db("wire-sql78-rls-dml-ext");
    let (core_lit, _g2) = create_db("wire-sql78-rls-dml-lit");
    let mut ext_conns = connect_all(core_ext.clone());
    let mut lit_conns = connect_all(core_lit.clone());
    let ops = [
        Dml {
            tenant_idx: 1,
            ext: "UPDATE r SET label = 'upd' WHERE n > $1 USING OPERATION_ID $2",
            values: vec!["4", "op-1"],
            literal: "UPDATE r SET label = 'upd' WHERE n > 4 USING OPERATION_ID 'op-1'",
            tag: "UPDATE 2",
        },
        Dml {
            tenant_idx: 1,
            ext: "DELETE FROM r WHERE day BETWEEN $1 AND $2 USING OPERATION_ID $3",
            values: vec!["2024-01-01", "2024-01-07", "op-2"],
            literal: "DELETE FROM r WHERE day BETWEEN '2024-01-01' AND '2024-01-07' \
                      USING OPERATION_ID 'op-2'",
            tag: "DELETE 2",
        },
        // carol は行を持たないので、どの条件でも 0 件。
        Dml {
            tenant_idx: 2,
            ext: "UPDATE r SET label = 'hijack' WHERE n > $1 USING OPERATION_ID $2",
            values: vec!["0", "op-3"],
            literal: "UPDATE r SET label = 'hijack' WHERE n > 0 USING OPERATION_ID 'op-3'",
            tag: "UPDATE 0",
        },
        Dml {
            tenant_idx: 2,
            ext: "DELETE FROM r WHERE n <> $1 USING OPERATION_ID $2",
            values: vec!["-1", "op-4"],
            literal: "DELETE FROM r WHERE n <> -1 USING OPERATION_ID 'op-4'",
            tag: "DELETE 0",
        },
        // bob の DML が済んだ後でも、alice は自分の行だけを更新する。
        Dml {
            tenant_idx: 0,
            ext: "UPDATE r SET label = 'a-upd' WHERE n <> $1 USING OPERATION_ID $2",
            values: vec!["5", "op-5"],
            literal: "UPDATE r SET label = 'a-upd' WHERE n <> 5 USING OPERATION_ID 'op-5'",
            tag: "UPDATE 2",
        },
    ];
    for op in &ops {
        let vals: Vec<&[u8]> = op.values.iter().map(|v| v.as_bytes()).collect();
        let (ext, ext_err) = run_extended(&mut ext_conns[op.tenant_idx], op.ext, &[], &[], &vals);
        let (lit, _) = run_simple(&mut lit_conns[op.tenant_idx], op.literal);
        assert_eq!(ext, lit, "{} vs {}", op.ext, op.literal);
        assert_eq!(ext.sqlstate, None, "{}: {ext_err}", op.ext);
        assert_eq!(ext.tag, op.tag, "{}", op.ext);
    }
    for tenant in ["tenant-a", "tenant-b", "tenant-c"] {
        assert_eq!(
            snapshot(&core_ext, tenant),
            snapshot(&core_lit, tenant),
            "{tenant}"
        );
    }
    // 結果状態の実値（非 vacuous）: bob は n=10 だけ残り、alice は自分の 2 行だけ変わる。
    assert_eq!(snapshot(&core_ext, "tenant-b").len(), 1);
    let alice = snapshot(&core_ext, "tenant-a");
    assert_eq!(alice.len(), 3);
    assert_eq!(alice.iter().filter(|r| r.contains("a-upd")).count(), 2);
    assert!(alice
        .iter()
        .all(|r| !r.contains("hijack") && !r.contains("\"upd\"")));
    assert!(snapshot(&core_ext, "tenant-c").is_empty());
    for s in ext_conns.iter_mut().chain(lit_conns.iter_mut()) {
        assert_alive(s);
    }
}
