//! `UPDATE`／`DELETE` の `$n` プレースホルダ（Issue #1748。ポインタ: SQL-50 (4)・SQL-45 (6)・
//! WIRE-12・WIRE-14・WIRE-24）を、実際の wire 経路（生バイトクライアント）で固定する層 A 結合テスト。
//!
//! 固定する内容: `ParameterDescription` の OID（列の型）、text／binary 両形式の Bind → Execute の
//! 結果、型不一致（`22P02`／`08P01`／`0A000`）の ErrorResponse が値を含まないこと・Sync での回復後に
//! 接続が継続し表が変わらないこと、他テナントの行の id を指定した応答が存在しない id と同じこと。
//! engine 層の同値性・拒否形は `crates/engine/tests/sql50_update_delete_params.rs`。

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

fn uuid_text(i: u8) -> String {
    format!("00000000-0000-0000-0000-00000000000{i}")
}

fn uuid_bytes(i: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = i;
    b
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql50-update-delete-param");
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
    static GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // 再投入のたびに operation_id を変える（台帳の重複拒否を避ける）。
    let generation = GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut session = SessionState::default();
    for id in ids {
        core.execute_sql_in_session(
            &PolicyContext::new("tenant-a").expect("tenant"),
            &mut session,
            &format!(
                "INSERT INTO t (id, n, b, d, s, f, u, dt) VALUES ({id}, {id}0, {id}00, {id}.5, \
                 's{id}', {flag}, '{u}', '2024-01-0{id}') USING OPERATION_ID 'seed-{id}-{generation}'",
                flag = if id % 2 == 1 { "TRUE" } else { "FALSE" },
                u = uuid_text(id),
            ),
        )
        .expect("seed insert");
    }
}

fn rows(core: &EngineCore, sql: &str) -> Vec<Vec<Cell>> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &PolicyContext::with_visibilities(
                "tenant-a",
                [Visibility::Public, Visibility::Private],
            )
            .expect("tenant"),
            &mut session,
            sql,
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn cell(core: &EngineCore, col: &str, id: u64) -> Cell {
    rows(
        core,
        &format!("SELECT {col} FROM t WHERE id = {id} LIMIT 10"),
    )
    .into_iter()
    .next()
    .expect("row")
    .remove(0)
}

fn count(core: &EngineCore) -> usize {
    rows(core, "SELECT id FROM t LIMIT 100").len()
}

fn snapshot(core: &EngineCore) -> Vec<Vec<Cell>> {
    rows(
        core,
        "SELECT n, b, d, s, f, u, dt FROM t ORDER BY id LIMIT 100",
    )
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

fn expect_kind(stream: &mut Stream, kind: u8) -> Vec<u8> {
    let (k, body) = read_message(stream);
    assert_eq!(
        k as char,
        kind as char,
        "unexpected message: {}",
        String::from_utf8_lossy(&body)
    );
    body
}

fn describe_statement_msg(name: &str) -> Vec<u8> {
    let mut body = vec![b'S'];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

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

/// Parse → Describe(S) の ParameterDescription の OID を返す。
fn describe_oids(s: &mut Stream, sql: &str) -> Vec<i32> {
    send_length_prefixed_message(s, b'P', &parse_body("d", sql, 0));
    expect_kind(s, b'1');
    send_length_prefixed_message(s, b'D', &describe_statement_msg("d"));
    let oids = param_description_oids(&expect_kind(s, b't'));
    let (k, _) = read_message(s);
    assert!(k == b'T' || k == b'n');
    send_length_prefixed_message(s, b'C', &describe_statement_msg("d"));
    expect_kind(s, b'3');
    oids
}

/// Parse → Bind → Execute → Sync を送り、成功ならコマンドタグ、失敗なら
/// `(SQLSTATE, ErrorResponse 全文)` を返す。
fn run_full(
    s: &mut Stream,
    sql: &str,
    fmts: &[i16],
    values: &[&[u8]],
) -> Result<String, (String, String)> {
    send_length_prefixed_message(s, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(s, b'B', &bind_msg(fmts, values));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    let mut tag = String::new();
    let mut err: Option<(String, String)> = None;
    loop {
        let (k, body) = read_message(s);
        match k {
            b'C' => {
                tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'E' if err.is_none() => {
                let text = String::from_utf8_lossy(&body).to_string();
                let code = text
                    .split('\0')
                    .find_map(|f| f.strip_prefix('C').map(str::to_string))
                    .unwrap_or_else(|| text.clone());
                err = Some((code, text));
            }
            b'Z' => break,
            _ => {}
        }
    }
    err.map_or(Ok(tag), Err)
}

fn run(s: &mut Stream, sql: &str, fmts: &[i16], values: &[&[u8]]) -> Result<String, String> {
    run_full(s, sql, fmts, values).map_err(|(code, _)| code)
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

#[test]
fn update_and_delete_params_are_described_with_column_types() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(
        describe_oids(
            &mut s,
            "UPDATE t SET s = $1, d = $2 WHERE n = $3 AND f = $4"
        ),
        vec![OID_TEXT, OID_FLOAT8, OID_INT4, OID_BOOL]
    );
    assert_eq!(
        describe_oids(&mut s, "DELETE FROM t WHERE n = $1 AND u = $2"),
        vec![OID_INT4, OID_UUID]
    );
    assert_eq!(
        describe_oids(&mut s, "DELETE FROM t WHERE b = $1"),
        vec![OID_INT8]
    );
}

#[test]
fn update_set_and_where_params_bind_in_text_and_binary() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());

    // text
    let tag = run(
        &mut s,
        "UPDATE t SET n = $1, s = $2, f = $3 WHERE b = $4",
        &[0],
        &[b"11", b"x-text", b"false", b"100"],
    )
    .expect("text bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 1), Cell::SignedInteger(11)));
    assert!(matches!(cell(&core, "s", 1), Cell::Text(ref v) if v == "x-text"));

    // binary（SET: int4・text・bool、WHERE: int8）
    let tag = run(
        &mut s,
        "UPDATE t SET n = $1, s = $2, f = $3 WHERE b = $4",
        &[1],
        &[
            &22i32.to_be_bytes(),
            b"x-bin",
            &[1u8],
            &200i64.to_be_bytes(),
        ],
    )
    .expect("binary bind");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 2), Cell::SignedInteger(22)));
    assert!(matches!(cell(&core, "s", 2), Cell::Text(ref v) if v == "x-bin"));

    // binary の WHERE が uuid・SET が float8
    let tag = run(
        &mut s,
        "UPDATE t SET d = $1 WHERE u = $2",
        &[1],
        &[&9.25f64.to_be_bytes(), &uuid_bytes(3)],
    )
    .expect("uuid where");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "d", 3), Cell::Float(v) if (v - 9.25).abs() < 1e-9));

    // 同じ番号を SET と WHERE の両方で使う（binary int4）
    let tag = run(
        &mut s,
        "UPDATE t SET n = $1 WHERE n = $1",
        &[1],
        &[&30i32.to_be_bytes()],
    )
    .expect("same slot");
    assert_eq!(tag, "UPDATE 1");
    assert!(matches!(cell(&core, "n", 3), Cell::SignedInteger(30)));
}

#[test]
fn delete_params_bind_in_text_and_binary() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    // 同じ述語を format 0 / 1 で送り、どちらも 1 行消す。
    assert_eq!(
        run(&mut s, "DELETE FROM t WHERE n = $1", &[0], &[b"10"]).expect("text int4"),
        "DELETE 1"
    );
    assert_eq!(
        run(
            &mut s,
            "DELETE FROM t WHERE n = $1",
            &[1],
            &[&20i32.to_be_bytes()]
        )
        .expect("binary int4"),
        "DELETE 1"
    );
    assert_eq!(count(&core), 1);
    seed(&core, 1..=2);
    assert_eq!(
        run(
            &mut s,
            "DELETE FROM t WHERE b = $1",
            &[1],
            &[&100i64.to_be_bytes()]
        )
        .expect("binary int8"),
        "DELETE 1"
    );
    assert_eq!(
        run(
            &mut s,
            "DELETE FROM t WHERE u = $1",
            &[1],
            &[&uuid_bytes(2)]
        )
        .expect("binary uuid"),
        "DELETE 1"
    );
    assert_eq!(count(&core), 1);
    // f = FALSE の行は無い（残りは id 3 の TRUE）ため 0 行、TRUE の行を text／binary で削除する。
    assert_eq!(
        run(&mut s, "DELETE FROM t WHERE f = $1", &[1], &[&[0u8]]).expect("binary bool false"),
        "DELETE 0"
    );
    assert_eq!(
        run(&mut s, "DELETE FROM t WHERE f = $1", &[1], &[&[1u8]]).expect("binary bool true"),
        "DELETE 1"
    );
    assert_eq!(count(&core), 0);
    seed(&core, 1..=3);
    assert_eq!(
        run(&mut s, "DELETE FROM t WHERE f = $1", &[0], &[b"t"]).expect("text bool"),
        "DELETE 2"
    );
}

#[test]
fn mismatched_values_fail_closed_and_the_connection_survives() {
    let (core, _g) = new_core();
    let mut s = connect(core.clone());
    let before = snapshot(&core);

    for sql in [
        "UPDATE t SET s = 'x' WHERE n = $1",
        "DELETE FROM t WHERE n = $1",
    ] {
        // text: 整数スロットへ整数でない値 → 22P02。応答に値を含めない。
        let (code, text) = run_full(&mut s, sql, &[0], &[b"abc-secret"]).expect_err("22P02");
        assert_eq!(code, "22P02", "{sql}");
        assert!(!text.contains("secret"), "{text}");
        assert_alive(&mut s);

        // binary: int4 スロットへ 8 バイト（長さ不一致）→ 08P01。
        let (code, text) = run_full(&mut s, sql, &[1], &[&7i64.to_be_bytes()]).expect_err("08P01");
        assert_eq!(code, "08P01", "{sql}");
        assert!(!text.contains("secret"), "{text}");
        assert_alive(&mut s);

        // Bind の値の数の不一致 → 08P01。
        let (code, _) = run_full(&mut s, sql, &[0], &[b"1", b"2"]).expect_err("count");
        assert_eq!(code, "08P01", "{sql}");
        assert_alive(&mut s);
    }

    // binary 非対応スロット（DATE 列）→ 0A000。
    for sql in [
        "UPDATE t SET s = 'x' WHERE dt = $1",
        "DELETE FROM t WHERE dt = $1",
    ] {
        let (code, _) = run_full(&mut s, sql, &[1], &[b"2024-01-01"]).expect_err("0A000");
        assert_eq!(code, "0A000", "{sql}");
        assert_alive(&mut s);
    }

    // 非等価は #2429（SQL-78）の範囲。現状は Parse で 42601（fail-closed）。
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("", "DELETE FROM t WHERE n <> $1", 0),
    );
    let text = String::from_utf8_lossy(&expect_kind(&mut s, b'E')).to_string();
    assert!(text.contains("42601"), "{text}");
    send_length_prefixed_message(&mut s, b'S', b"");
    expect_kind(&mut s, b'Z');
    assert_alive(&mut s);

    assert_eq!(before, snapshot(&core));
}

#[test]
fn other_tenant_ids_look_like_missing_ids() {
    let (core, _g) = new_core();
    let mut s = connect_as(core.clone(), "bob");
    let before = snapshot(&core);
    for (sql, existing, missing) in [
        (
            "UPDATE t SET s = $1 WHERE id = $2",
            [&b"hijack"[..], &1i64.to_be_bytes()[..]],
            [&b"hijack"[..], &999i64.to_be_bytes()[..]],
        ),
        (
            "DELETE FROM t WHERE id = $1",
            [&1i64.to_be_bytes()[..], &[][..]],
            [&999i64.to_be_bytes()[..], &[][..]],
        ),
    ] {
        let n = if sql.starts_with("UPDATE") { 2 } else { 1 };
        let fmts: &[i16] = if n == 2 { &[0, 1] } else { &[1] };
        let a = run_full(&mut s, sql, fmts, &existing[..n]);
        let b = run_full(&mut s, sql, fmts, &missing[..n]);
        assert_eq!(a, b, "{sql}");
    }
    assert_eq!(before, snapshot(&core));
}
