//! 拡張クエリプロトコルの `$n` パラメータ束縛の結合テスト（Issue #1171・
//! TASK-217・WIRE-12。ポインタ: WIRE-14・SQL-1・SQL-5）。生バイトクライアントで
//! Parse／Describe／Bind／Execute／Sync を送り、次を固定する。
//!
//! - 同じ値をリテラルで書いた簡易クエリと応答（`DataRow`・`CommandComplete`）が
//!   一致すること（第 2 の実行器を作らない不変条件）。
//! - Describe(S) の `ParameterDescription` が推論 OID・宣言 OID の echo を返すこと。
//! - エラー写像（`54000`／`08P01`／`22P02`／`22000`／`42601`／`0A000`）と、
//!   いずれのエラーも Sync で同期回復すること。
//! - インジェクション文字列が不透明なリテラルとして扱われること・テナント境界・
//!   Failed トランザクション・portal 束縛値バイト上限。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder};
use engine::kernel::CpuScalarProvider;
use engine::query_planner::{LlmClient, PlanError};
use engine::storage::Storage;

use common::*;

type Stream = std::net::TcpStream;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let (storage, guard) = open_storage();
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

fn open_storage() -> (Storage, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire12-param-binding");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        ))
        .expect("create table");
    (storage, guard)
}

fn seed(core: &EngineCore, n: u64) {
    for i in 1..=n {
        let sql = format!(
            "INSERT INTO documents (id, embedding, body) VALUES ({i}, '[0.{i},0.2,0.3]', 'row-{i}') USING OPERATION_ID 'seed-{i}'"
        );
        let mut session = engine::sql::mode::SessionState::default();
        core.execute_sql_in_session(
            &engine::policy::PolicyContext::new("tenant-a").expect("tenant"),
            &mut session,
            &sql,
        )
        .expect("seed insert");
    }
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

fn parse_msg(name: &str, sql: &str, declared: &[i32]) -> Vec<u8> {
    let mut body = parse_body(name, sql, declared.len() as i16);
    for oid in declared {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    body
}

/// パラメータ format code 列 `fmts` と値列 `values`（`None` は NULL）を持つ Bind。
fn bind_msg(portal: &str, stmt: &str, fmts: &[i16], values: &[Option<&[u8]>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(stmt.as_bytes());
    body.push(0);
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

fn describe_msg(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn send(stream: &mut Stream, kind: u8, body: &[u8]) {
    send_length_prefixed_message(stream, kind, body);
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

/// ErrorResponse の SQLSTATE を確認し、Sync で同期回復して接続が継続する
/// （簡易クエリが通る）ことまで確認する。
fn expect_error_then_recovers(stream: &mut Stream, sqlstate: &str) {
    let body = expect_kind(stream, b'E');
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains(sqlstate), "expected {sqlstate} in {text:?}");
    send(stream, b'S', b"");
    let z = expect_kind(stream, b'Z');
    assert_eq!(z, [b'I']);
    send_simple_query(stream, "SELECT id FROM documents LIMIT 1");
    loop {
        let (k, _) = read_message(stream);
        if k == b'Z' {
            break;
        }
    }
}

/// `Z` までのメッセージのうち `D`（DataRow）本体と `C`（CommandComplete）本体を集める。
fn collect_until_z(stream: &mut Stream) -> (Vec<Vec<u8>>, String) {
    let mut rows = Vec::new();
    let mut tag = String::new();
    loop {
        let (k, body) = read_message(stream);
        match k {
            b'D' => rows.push(body),
            b'C' => {
                tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'Z' => break,
            b'E' => panic!("unexpected error: {}", String::from_utf8_lossy(&body)),
            _ => {}
        }
    }
    rows.sort();
    (rows, tag)
}

/// Parse→Bind→Execute→Sync を送り、結果行とタグを返す。
fn run_extended(
    stream: &mut Stream,
    sql: &str,
    fmts: &[i16],
    values: &[Option<&[u8]>],
) -> (Vec<Vec<u8>>, String) {
    send(stream, b'P', &parse_msg("", sql, &[]));
    send(stream, b'B', &bind_msg("", "", fmts, values));
    send(stream, b'E', &execute_body("", 0));
    send(stream, b'S', b"");
    expect_kind(stream, b'1');
    expect_kind(stream, b'2');
    collect_until_z(stream)
}

fn run_simple(stream: &mut Stream, sql: &str) -> (Vec<Vec<u8>>, String) {
    send_simple_query(stream, sql);
    let mut rows = Vec::new();
    let mut tag = String::new();
    loop {
        let (k, body) = read_message(stream);
        match k {
            b'D' => rows.push(body),
            b'C' => {
                tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'Z' => break,
            b'E' => panic!("unexpected error: {}", String::from_utf8_lossy(&body)),
            _ => {}
        }
    }
    rows.sort();
    (rows, tag)
}

/// Parse→Bind→Execute→Sync が失敗する場合の ErrorResponse を確認する。
fn send_pbes_expect_error(
    stream: &mut Stream,
    sql: &str,
    declared: &[i32],
    fmts: &[i16],
    values: &[Option<&[u8]>],
    sqlstate: &str,
) {
    send(stream, b'P', &parse_msg("", sql, declared));
    send(stream, b'B', &bind_msg("", "", fmts, values));
    send(stream, b'E', &execute_body("", 0));
    send(stream, b'S', b"");
    // Parse で失敗する場合は Parse の応答が ErrorResponse、Bind で失敗する場合は
    // ParseComplete の後。どちらでも最初の 'E' を確認する。
    let mut first = read_message(stream);
    while first.0 == b'1' || first.0 == b'2' {
        first = read_message(stream);
    }
    assert_eq!(first.0 as char, 'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&first.1).to_string();
    assert!(text.contains(sqlstate), "expected {sqlstate} in {text:?}");
    // ignore-till-sync により残りは読み捨てられ、Sync で Z が返る。
    let z = expect_kind(stream, b'Z');
    assert_eq!(z, [b'I']);
    send_simple_query(stream, "SELECT id FROM documents LIMIT 1");
    loop {
        let (k, _) = read_message(stream);
        if k == b'Z' {
            break;
        }
    }
}

fn param_description_oids(body: &[u8]) -> Vec<i32> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
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

fn describe_statement_oids(stream: &mut Stream, sql: &str, declared: &[i32]) -> Vec<i32> {
    send(stream, b'P', &parse_msg("d", sql, declared));
    expect_kind(stream, b'1');
    send(stream, b'D', &describe_msg(b'S', "d"));
    let body = expect_kind(stream, b't');
    let oids = param_description_oids(&body);
    // 続く RowDescription／NoData を読み捨てる。
    let (k, _) = read_message(stream);
    assert!(k == b'T' || k == b'n');
    send(stream, b'C', &describe_msg(b'S', "d"));
    expect_kind(stream, b'3');
    oids
}

const VEC_SQL_LIT: &str =
    "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";
const VEC_SQL_PARAM: &str = "SELECT id, body FROM documents ORDER BY embedding <=> $1 LIMIT 5";

#[test]
fn text_binding_matches_literal_form() {
    let (core, _g) = new_core();
    seed(&core, 3);
    let addr = spawn(core);
    let mut ext = connect(addr, "alice");
    let mut simple = connect(addr, "alice");

    let got = run_extended(
        &mut ext,
        "SELECT id, body FROM documents WHERE body = $1 LIMIT 10",
        &[],
        &[Some(b"row-2")],
    );
    let want = run_simple(
        &mut simple,
        "SELECT id, body FROM documents WHERE body = 'row-2' LIMIT 10",
    );
    assert_eq!(got, want);
    assert_eq!(got.0.len(), 1);

    let got = run_extended(&mut ext, VEC_SQL_PARAM, &[], &[Some(b"[0.1,0.2,0.3]")]);
    let want = run_simple(&mut simple, VEC_SQL_LIT);
    assert_eq!(got, want);
    assert_eq!(got.0.len(), 3);
}

#[test]
fn binary_text_slot_matches_text_and_vector_slot_rejects_binary() {
    let (core, _g) = new_core();
    seed(&core, 2);
    let addr = spawn(core);
    let mut ext = connect(addr, "alice");

    let sql = "SELECT id, body FROM documents WHERE body = $1 LIMIT 10";
    let text = run_extended(&mut ext, sql, &[0], &[Some(b"row-1")]);
    let binary = run_extended(&mut ext, sql, &[1], &[Some(b"row-1")]);
    assert_eq!(text, binary);
    assert_eq!(text.0.len(), 1);

    // ベクトル位置へのバイナリは 0A000（WIRE-14）。
    send_pbes_expect_error(
        &mut ext,
        VEC_SQL_PARAM,
        &[],
        &[1],
        &[Some(b"[0.1,0.2,0.3]")],
        "0A000",
    );
    // 未知の format code は 08P01。
    send_pbes_expect_error(&mut ext, sql, &[], &[2], &[Some(b"row-1")], "08P01");
}

#[test]
fn parameter_description_reports_inferred_and_declared_oids() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut s = connect(addr, "alice");

    assert_eq!(
        describe_statement_oids(&mut s, VEC_SQL_PARAM, &[]),
        vec![25]
    );
    assert_eq!(
        describe_statement_oids(
            &mut s,
            "SELECT id FROM documents WHERE body = $1 LIMIT 10",
            &[]
        ),
        vec![25]
    );
    assert_eq!(
        describe_statement_oids(
            &mut s,
            "INSERT INTO documents (id, embedding, body) VALUES (99, $1, $2) USING OPERATION_ID $3",
            &[]
        ),
        vec![25, 25, 25]
    );
    // 宣言 OID の echo。
    assert_eq!(
        describe_statement_oids(
            &mut s,
            "SELECT id FROM documents WHERE body = $1 LIMIT 10",
            &[1043]
        ),
        vec![1043]
    );
    // 宣言件数 < プレースホルダ数: 残りは推論値で補う。
    assert_eq!(
        describe_statement_oids(
            &mut s,
            "SELECT id FROM documents WHERE body = $1 AND body = $2 LIMIT 10",
            &[20]
        ),
        vec![20, 25]
    );
}

#[test]
fn insert_with_bound_values_and_operation_id() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut ext = connect(addr, "alice");
    let mut simple = connect(addr, "alice");

    let sql =
        "INSERT INTO documents (id, embedding, body) VALUES (99, $1, $2) USING OPERATION_ID $3";
    let (_, tag) = run_extended(
        &mut ext,
        sql,
        &[],
        &[Some(b"[0.1,0.2,0.3]"), Some(b"inserted"), Some(b"op-1")],
    );
    assert!(tag.starts_with("INSERT"), "tag: {tag}");
    let (rows, _) = run_simple(
        &mut simple,
        "SELECT id, body FROM documents WHERE body = 'inserted' LIMIT 10",
    );
    assert_eq!(rows.len(), 1);

    // 同一 operation_id の再実行はリテラル形と同じ 23505。
    send_pbes_expect_error(
        &mut ext,
        sql,
        &[],
        &[],
        &[Some(b"[0.1,0.2,0.3]"), Some(b"inserted"), Some(b"op-1")],
        "23505",
    );
}

#[test]
fn error_mapping_and_recovery() {
    let (core, _g) = new_core();
    seed(&core, 1);
    let addr = spawn(core);
    let mut s = connect(addr, "alice");
    let sql = "SELECT id FROM documents WHERE body = $1 LIMIT 10";

    // 宣言型 65 件 → 54000。
    let declared: Vec<i32> = vec![25; 65];
    send_pbes_expect_error(&mut s, sql, &declared, &[], &[Some(b"x")], "54000");
    // SQL 中の $65 → 54000。
    send_pbes_expect_error(
        &mut s,
        "SELECT id FROM documents WHERE body = $65 LIMIT 10",
        &[],
        &[],
        &[],
        "54000",
    );
    // 値数不一致（0 件・2 件）→ 08P01。
    send_pbes_expect_error(&mut s, sql, &[], &[], &[], "08P01");
    send_pbes_expect_error(&mut s, sql, &[], &[], &[Some(b"a"), Some(b"b")], "08P01");
    // format code 個数不正 → 08P01。
    send_pbes_expect_error(&mut s, sql, &[], &[0, 0], &[Some(b"a")], "08P01");
    // 宣言件数 > プレースホルダ数 → 08P01。
    send_pbes_expect_error(&mut s, sql, &[25, 25], &[], &[Some(b"a")], "08P01");
    // 非 UTF-8・NUL → 22P02。NULL → 22000。
    send_pbes_expect_error(&mut s, sql, &[], &[], &[Some(&[0xff, 0xfe])], "22P02");
    send_pbes_expect_error(&mut s, sql, &[], &[], &[Some(b"a\0b")], "22P02");
    send_pbes_expect_error(&mut s, sql, &[], &[], &[None], "22000");
    // 受理位置外（LIMIT $1）→ 42601。
    send_pbes_expect_error(
        &mut s,
        "SELECT id FROM documents LIMIT $1",
        &[],
        &[],
        &[Some(b"1")],
        "42601",
    );
    // ベクトル次元不一致はリテラル形と同じコード。
    let lit = {
        let mut simple = connect(addr, "alice");
        send_simple_query(
            &mut simple,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2]' LIMIT 1",
        );
        let (k, body) = read_message(&mut simple);
        assert_eq!(k, b'E');
        String::from_utf8_lossy(&body).to_string()
    };
    let code = lit
        .split('\0')
        .find(|f| f.starts_with('C'))
        .map(|f| f[1..].to_string())
        .expect("sqlstate field");
    send_pbes_expect_error(
        &mut s,
        VEC_SQL_PARAM,
        &[],
        &[],
        &[Some(b"[0.1,0.2]")],
        &code,
    );
}

#[test]
fn injection_strings_stay_opaque_literals() {
    let (core, _g) = new_core();
    seed(&core, 2);
    let addr = spawn(core);
    let mut ext = connect(addr, "alice");
    let mut simple = connect(addr, "alice");

    for payload in ["' OR '1'='1", "'); DROP TABLE documents; --"] {
        let got = run_extended(
            &mut ext,
            "SELECT id, body FROM documents WHERE body = $1 LIMIT 10",
            &[],
            &[Some(payload.as_bytes())],
        );
        assert!(got.0.is_empty(), "payload must match nothing: {payload}");
        assert_eq!(got.1, "SELECT 0");
    }
    // テーブルは存続する。
    let (rows, _) = run_simple(&mut simple, "SELECT id FROM documents LIMIT 10");
    assert_eq!(rows.len(), 2);
}

#[test]
fn tenant_boundary_holds_for_parameterized_queries() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut alice = connect(addr, "alice");
    let mut bob = connect(addr, "bob");
    let mut alice_simple = connect(addr, "alice");

    let insert =
        "INSERT INTO documents (id, embedding, body) VALUES (1, $1, $2) USING OPERATION_ID $3";
    run_extended(
        &mut alice,
        insert,
        &[],
        &[Some(b"[0.1,0.2,0.3]"), Some(b"alice-secret"), Some(b"a-op")],
    );

    let sel = "SELECT id, body FROM documents WHERE body = $1 LIMIT 10";
    let a = run_extended(&mut alice, sel, &[], &[Some(b"alice-secret")]);
    let a_lit = run_simple(
        &mut alice_simple,
        "SELECT id, body FROM documents WHERE body = 'alice-secret' LIMIT 10",
    );
    assert_eq!(a, a_lit);
    let b = run_extended(&mut bob, sel, &[], &[Some(b"alice-secret")]);
    assert!(b.0.is_empty(), "bob must not see alice's row");
    assert_eq!(b.1, "SELECT 0");
}

#[test]
fn failed_transaction_rejects_parameterized_statements() {
    let (core, _g) = new_core();
    seed(&core, 1);
    let addr = spawn(core);
    let mut s = connect(addr, "alice");

    send_simple_query(&mut s, "BEGIN");
    loop {
        let (k, _) = read_message(&mut s);
        if k == b'Z' {
            break;
        }
    }
    send_simple_query(&mut s, "SELECT id FROM missing_table LIMIT 1");
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b'E');
    loop {
        let (k, _) = read_message(&mut s);
        if k == b'Z' {
            break;
        }
    }

    send(
        &mut s,
        b'P',
        &parse_msg("", "SELECT id FROM documents WHERE body = $1 LIMIT 10", &[]),
    );
    let body = expect_kind(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("25P02"));
    send(&mut s, b'S', b"");
    let z = expect_kind(&mut s, b'Z');
    assert_eq!(z, [b'E']);
}

#[test]
fn named_statement_is_reusable_with_different_values() {
    let (core, _g) = new_core();
    seed(&core, 3);
    let addr = spawn(core);
    let mut s = connect(addr, "alice");

    send(
        &mut s,
        b'P',
        &parse_msg(
            "q",
            "SELECT id, body FROM documents WHERE body = $1 LIMIT 10",
            &[],
        ),
    );
    send(&mut s, b'B', &bind_msg("p1", "q", &[], &[Some(b"row-1")]));
    send(&mut s, b'B', &bind_msg("p2", "q", &[], &[Some(b"row-3")]));
    send(&mut s, b'E', &execute_body("p2", 0));
    send(&mut s, b'E', &execute_body("p1", 0));
    send(&mut s, b'S', b"");
    expect_kind(&mut s, b'1');
    expect_kind(&mut s, b'2');
    expect_kind(&mut s, b'2');
    let d2 = expect_kind(&mut s, b'D');
    assert!(String::from_utf8_lossy(&d2).contains("row-3"));
    let c = expect_kind(&mut s, b'C');
    assert!(String::from_utf8_lossy(&c).starts_with("SELECT 1"));
    let d1 = expect_kind(&mut s, b'D');
    assert!(String::from_utf8_lossy(&d1).contains("row-1"));
    expect_kind(&mut s, b'C');
    expect_kind(&mut s, b'Z');
}

#[test]
fn bound_bytes_limit_per_connection_recovers_after_sync() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut s = connect(addr, "alice");

    let big = vec![b'x'; 900 * 1024];
    send(
        &mut s,
        b'P',
        &parse_msg(
            "q",
            "SELECT id FROM documents WHERE body = $1 LIMIT 10",
            &[],
        ),
    );
    expect_kind(&mut s, b'1');
    // 4 MiB の上限に対し 900 KiB を 4 個は通り、5 個目で 54000。
    for i in 0..4 {
        send(
            &mut s,
            b'B',
            &bind_msg(&format!("p{i}"), "q", &[], &[Some(&big)]),
        );
        expect_kind(&mut s, b'2');
    }
    send(&mut s, b'B', &bind_msg("p4", "q", &[], &[Some(&big)]));
    expect_error_then_recovers(&mut s, "54000");

    // Sync で portal が破棄されたため、再び Bind できる（計上が戻る）。
    send(&mut s, b'B', &bind_msg("p0", "q", &[], &[Some(&big)]));
    expect_kind(&mut s, b'2');
    send(&mut s, b'S', b"");
    expect_kind(&mut s, b'Z');
}

// ---------------------------------------------------------------------------
// USING PLAN($1)
// ---------------------------------------------------------------------------

struct DeterministicEmbedder;

impl Embedder for DeterministicEmbedder {
    fn dim(&self) -> u32 {
        4
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| vec![t.len() as f32 * 0.01; 4])
            .collect())
    }
}

struct StubLlm;

impl LlmClient for StubLlm {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(
            r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#
                .to_string(),
        )
    }
}

#[test]
fn using_plan_parameter_matches_literal_form() {
    use engine::policy::PolicyContext;
    use engine::recovery::required_op_id::OperationId;
    use engine::row_codec::Value;
    use engine::storage::Visibility;

    let path = temp_db::unique_db_path("wire12-using-plan");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("tenant");
    for (id, emb, p, b) in [
        (1u64, vec![0.1, 0.2, 0.3, 0.4], "docs/a.md", "alpha content"),
        (2u64, vec![0.4, 0.3, 0.2, 0.1], "docs/b.md", "beta content"),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb),
                Value::Text(p.to_string()),
                Value::Text(b.to_string()),
            ],
            &OperationId::parse(&format!("wire12-plan-op-{id}")).expect("op id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder))
        .with_query_planner(Box::new(StubLlm));
    let addr = spawn(Arc::new(core));
    let mut ext = connect(addr, "alice");
    let mut simple = connect(addr, "alice");

    let got = run_extended(
        &mut ext,
        "SELECT id FROM docs USING PLAN($1) LIMIT 10",
        &[],
        &[Some(b"find content")],
    );
    let want = run_simple(
        &mut simple,
        "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    );
    assert_eq!(got, want);
    assert_eq!(got.0.len(), 2);
}
// --- 型付き置換（Issue #1342・WIRE-12）: 行 id・INTEGER・BOOLEAN 列 -----------

#[test]
fn typed_slots_report_column_oids() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut s = connect(addr, "alice");

    assert_eq!(
        describe_statement_oids(
            &mut s,
            "SELECT id FROM documents WHERE id = $1 LIMIT 5",
            &[]
        ),
        vec![1700]
    );
    assert_eq!(
        describe_statement_oids(&mut s, "SELECT id FROM documents WHERE n = $1 LIMIT 5", &[]),
        vec![23]
    );
    assert_eq!(
        describe_statement_oids(
            &mut s,
            "SELECT id FROM documents WHERE flag = $1 LIMIT 5",
            &[]
        ),
        vec![16]
    );
    assert_eq!(
        describe_statement_oids(
            &mut s,
            "INSERT INTO documents (id, embedding, n, flag) VALUES ($1, '[0.1,0.2,0.3]', $2, $3) USING OPERATION_ID 'x'",
            &[]
        ),
        vec![1700, 23, 16]
    );
    // 宣言 OID の echo は従来どおり。
    assert_eq!(
        describe_statement_oids(
            &mut s,
            "SELECT id FROM documents WHERE n = $1 LIMIT 5",
            &[21]
        ),
        vec![21]
    );
}

#[test]
fn typed_text_binding_matches_literal_form_and_tenant_boundary_holds() {
    let (core, _g) = new_core();
    let addr = spawn(core);
    let mut alice = connect(addr, "alice");
    let mut alice_simple = connect(addr, "alice");
    let mut bob = connect(addr, "bob");

    let insert = "INSERT INTO documents (id, embedding, body, n, flag) VALUES ($1, '[0.1,0.2,0.3]', 'typed', $2, $3) USING OPERATION_ID $4";
    let (_, tag) = run_extended(
        &mut alice,
        insert,
        &[],
        &[Some(b"7"), Some(b"-5"), Some(b"t"), Some(b"typed-op")],
    );
    assert_eq!(tag, "INSERT 0 1");

    for (sql, vals, lit) in [
        (
            "SELECT id, body FROM documents WHERE id = $1 LIMIT 5",
            &b"7"[..],
            "SELECT id, body FROM documents WHERE id = 7 LIMIT 5",
        ),
        (
            "SELECT id, body FROM documents WHERE flag = $1 LIMIT 5",
            &b"TRUE"[..],
            "SELECT id, body FROM documents WHERE flag = true LIMIT 5",
        ),
        (
            "SELECT id, body FROM documents WHERE flag = $1 LIMIT 5",
            &b"f"[..],
            "SELECT id, body FROM documents WHERE flag = false LIMIT 5",
        ),
        (
            "SELECT id, body FROM documents WHERE n = $1 LIMIT 5",
            &b"3"[..],
            "SELECT id, body FROM documents WHERE n = 3 LIMIT 5",
        ),
    ] {
        let got = run_extended(&mut alice, sql, &[], &[Some(vals)]);
        let want = run_simple(&mut alice_simple, lit);
        assert_eq!(got, want, "{sql}");
    }
    let by_id = run_extended(
        &mut alice,
        "SELECT id FROM documents WHERE id = $1 LIMIT 5",
        &[],
        &[Some(b"7")],
    );
    assert_eq!(by_id.0.len(), 1);
    // 他テナントの行は束縛した id でも見えない。
    let other = run_extended(
        &mut bob,
        "SELECT id FROM documents WHERE id = $1 LIMIT 5",
        &[],
        &[Some(b"7")],
    );
    assert!(other.0.is_empty());

    // 束縛した DELETE がリテラル形と同じく 1 行を消す。
    let (_, tag) = run_extended(
        &mut alice,
        "DELETE FROM documents WHERE id = $1 USING OPERATION_ID $2",
        &[],
        &[Some(b"7"), Some(b"typed-del")],
    );
    assert_eq!(tag, "DELETE 1");
}

#[test]
fn typed_slots_reject_malformed_values_with_22p02_and_binary_with_0a000() {
    let (core, _g) = new_core();
    seed(&core, 1);
    let addr = spawn(core);
    let mut s = connect(addr, "alice");
    let int_sql = "SELECT id FROM documents WHERE n = $1 LIMIT 5";
    let bool_sql = "SELECT id FROM documents WHERE flag = $1 LIMIT 5";
    let id_sql = "SELECT id FROM documents WHERE id = $1 LIMIT 5";

    for bad in [&b"abc"[..], b"5.0", b"+5", b"1 OR 1=1"] {
        send_pbes_expect_error(&mut s, int_sql, &[], &[], &[Some(bad)], "22P02");
    }
    send_pbes_expect_error(&mut s, bool_sql, &[], &[], &[Some(b"maybe")], "22P02");
    send_pbes_expect_error(&mut s, id_sql, &[], &[], &[Some(b"abc")], "22P02");
    // 数値・真偽値スロットのバイナリ format は text として誤解釈せず 0A000。
    send_pbes_expect_error(&mut s, int_sql, &[], &[1], &[Some(b"1234")], "0A000");
    send_pbes_expect_error(&mut s, bool_sql, &[], &[1], &[Some(b"t")], "0A000");
    send_pbes_expect_error(&mut s, id_sql, &[], &[1], &[Some(b"1")], "0A000");
}
