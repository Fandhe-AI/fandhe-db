//! 配列の型 OID 公告と多次元配列の wire 出力・要素数上限（Issue #1606。ポインタ:
//! `docs/spec/04-behavior/` TABLE-35 (3)(4)・SQL-46・WIRE-13・WIRE-14・WIRE-24）の層 A 結合テスト。
//!
//! 確認する契約:
//! - 配列列・配列式の RowDescription は要素型ごとの PostgreSQL 配列型 OID（`int4[]`=1007 など）
//!   を公告し、次元数は OID に影響しない
//! - 多次元配列のテキスト出力は PostgreSQL の入れ子 `{}` 形式で、再投入すると同値になる
//! - 拡張クエリの Describe も同じ OID を返し、Parse で配列 OID を宣言すると `0A000`（接続は維持）
//! - 配列のバイナリ結果形式は `0A000`（接続は維持）
//! - `SchemaLimits` の配列要素数上限が多次元の宣言にも効く（既定値の挙動は不変）

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::schema_limits::SchemaLimits;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;

/// 全要素型の配列列（多次元を含む）を持つ `g` テーブルを SQL DDL で作った `EngineCore`。
fn new_core(limits: SchemaLimits, tag: &str) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    new_core_with_ddl(
        limits,
        tag,
        "CREATE TABLE g (embedding VECTOR(2) NOT NULL, \
         a_text TEXT[4], a_bool BOOLEAN[4], a_int INTEGER[4], a_big BIGINT[4], \
         a_real REAL[4], a_dbl DOUBLE PRECISION[4], a_date DATE[4], a_ts TIMESTAMP[4], \
         a_uuid UUID[4], a_num NUMERIC(5,2)[4], a_bytea BYTEA[4], a_json JSON[4], \
         a_jsonb JSONB[4], m INTEGER[4][], m3 TEXT[16][][])",
    )
}

/// 指定の設定値と DDL 1 文で起動した `EngineCore` を返す。
fn new_core_with_ddl(
    limits: SchemaLimits,
    tag: &str,
    ddl: &str,
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(tag);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_schema_limits(limits);
    run_ddl(&core, ddl);
    (Arc::new(core), guard)
}

fn run_ddl(core: &EngineCore, sql: &str) {
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .expect("ddl");
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn exec(stream: &mut TcpStream, sql: &str) {
    send_simple_query(stream, sql);
    let _tag = read_command_complete(stream);
    read_ready_for_query(stream);
}

/// 簡易クエリを実行し `(列名, OID, typlen)` と、あれば先頭行の DataRow を返す
/// （型公告のみ確認したいクエリでは 0 行でもよい）。
/// `select` が返す列メタ（名前・OID・typlen）。
type Columns = Vec<(String, i32, i16)>;

fn select(stream: &mut TcpStream, sql: &str) -> (Columns, Vec<Option<String>>) {
    send_simple_query(stream, sql);
    let cols = read_row_description_with_types(stream);
    let mut first = None;
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' if first.is_none() => first = Some(parse_data_row(&body)),
            b'D' => {}
            b'C' => {}
            b'Z' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    (cols, first.unwrap_or_default())
}

/// DataRow 本文を各セルのテキストへ分解する（NULL は `None`）。
fn parse_data_row(body: &[u8]) -> Vec<Option<String>> {
    let count = body
        .get(..2)
        .map(|b| i16::from_be_bytes([b[0], b[1]]))
        .expect("count");
    let mut pos = 2usize;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = body.get(pos..pos + 4).expect("len");
        let len = i32::from_be_bytes([len[0], len[1], len[2], len[3]]);
        pos += 4;
        if len < 0 {
            out.push(None);
            continue;
        }
        let len = usize::try_from(len).expect("len");
        let cell = body.get(pos..pos + len).expect("cell");
        out.push(Some(String::from_utf8_lossy(cell).to_string()));
        pos += len;
    }
    out
}

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

/// RowDescription 本文から各列の OID だけを取り出す（添字を使わず `get` で境界検査する）。
fn row_description_oids(body: &[u8]) -> Vec<i32> {
    let count = body
        .get(..2)
        .map(|b| i16::from_be_bytes([b[0], b[1]]))
        .expect("count");
    let mut pos = 2usize;
    let mut out = Vec::new();
    for _ in 0..count {
        while *body.get(pos).expect("name") != 0 {
            pos += 1;
        }
        pos += 1 + 4 + 2;
        let oid = body.get(pos..pos + 4).expect("oid");
        out.push(i32::from_be_bytes([oid[0], oid[1], oid[2], oid[3]]));
        pos += 4 + 2 + 4 + 2;
    }
    out
}

fn sync_and_ready(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z');
    assert_eq!(body, [b'I']);
}

/// 型 OID（AC2）: 要素型ごとの配列 OID・typlen -1。多次元でも 1 次元と同じ。
#[test]
fn array_columns_advertise_postgres_array_type_oids() {
    let (core, _g) = new_core(SchemaLimits::default(), "wire-array-oid");
    let mut s = connect(core);
    let (cols, _) = select(
        &mut s,
        "SELECT a_text, a_bool, a_int, a_big, a_real, a_dbl, a_date, a_ts, a_uuid, a_num, \
         a_bytea, a_json, a_jsonb, m, m3 FROM g LIMIT 10",
    );
    let expected: [(&str, i32); 15] = [
        ("a_text", 1009),
        ("a_bool", 1000),
        ("a_int", 1007),
        ("a_big", 1016),
        ("a_real", 1021),
        ("a_dbl", 1022),
        ("a_date", 1182),
        ("a_ts", 1115),
        ("a_uuid", 2951),
        ("a_num", 1231),
        ("a_bytea", 1001),
        ("a_json", 199),
        ("a_jsonb", 3807),
        ("m", 1007),
        ("m3", 1009),
    ];
    assert_eq!(cols.len(), expected.len());
    for ((name, oid, typlen), (en, eo)) in cols.iter().zip(expected.iter()) {
        assert_eq!(name, en);
        assert_eq!(oid, eo, "oid mismatch for {name}");
        assert_eq!(*typlen, -1, "typlen mismatch for {name}");
    }
}

/// 多次元のテキスト出力（R1）: 入れ子 `{}`・NULL・引用が PostgreSQL 形式で、出力を
/// そのままリテラルとして再投入すると同値になる。
#[test]
fn multi_dimensional_text_output_roundtrips() {
    let (core, _g) = new_core(SchemaLimits::default(), "wire-array-multidim-text");
    let mut s = connect(core);
    exec(
        &mut s,
        "INSERT INTO g (id, embedding, m, m3) VALUES (1, '[0.1,0.2]', '{{1,2},{3,NULL}}', \
         '{{{\"a b\",\"\"},{\"NULL\",\"q\\\"r\"}},{{x,y},{z,w}}}') USING OPERATION_ID 'op-md1'",
    );
    let (_, row) = select(&mut s, "SELECT m, m3 FROM g WHERE id = 1 LIMIT 1");
    assert_eq!(row[0].as_deref(), Some("{{1,2},{3,NULL}}"));
    let m3 = row[1].clone().expect("m3 text");
    assert_eq!(m3, "{{{\"a b\",\"\"},{\"NULL\",\"q\\\"r\"}},{{x,y},{z,w}}}");

    // 出力をそのまま再投入して同値になる。
    let sql = format!(
        "INSERT INTO g (id, embedding, m3) VALUES (2, '[0.1,0.2]', '{}') \
         USING OPERATION_ID 'op-md2'",
        m3.replace('\'', "''")
    );
    exec(&mut s, &sql);
    let (_, row2) = select(&mut s, "SELECT m3 FROM g WHERE id = 2 LIMIT 1");
    assert_eq!(row2[0].as_deref(), Some(m3.as_str()));
}

/// 式の結果列（ARRAY コンストラクタ・スライス・CAST）も配列 OID を公告する。
#[test]
fn array_expression_results_advertise_array_oids() {
    let (core, _g) = new_core(SchemaLimits::default(), "wire-array-expr-oid");
    let mut s = connect(core);
    exec(
        &mut s,
        "INSERT INTO g (id, embedding, m) VALUES (1, '[0.1,0.2]', '{{1,2},{3,4}}') \
         USING OPERATION_ID 'op-ex1'",
    );
    let (cols, row) = select(&mut s, "SELECT ARRAY[1,2] FROM g LIMIT 1");
    assert_eq!(cols[0].1, 1007);
    assert_eq!(row[0].as_deref(), Some("{1,2}"));

    let (cols, row) = select(&mut s, "SELECT m[1:1] FROM g WHERE id = 1 LIMIT 1");
    assert_eq!(cols[0].1, 1007);
    assert_eq!(row[0].as_deref(), Some("{{1,2}}"));

    let (cols, row) = select(&mut s, "SELECT CAST('{1,2}' AS bigint[]) FROM g LIMIT 1");
    assert_eq!(cols[0].1, 1016);
    assert_eq!(row[0].as_deref(), Some("{1,2}"));
    let (cols, _) = select(&mut s, "SELECT '{1,2}'::bigint[] FROM g LIMIT 1");
    assert_eq!(cols[0].1, 1016);
}

/// 拡張クエリ: Describe(Statement) が配列 OID を返し、Parse で配列 OID を宣言すると
/// 従来どおり `0A000`（WIRE-24）。エラー後も Sync で回復する。
#[test]
fn extended_describe_advertises_array_oid_and_declared_array_oid_is_rejected() {
    let (core, _g) = new_core(SchemaLimits::default(), "wire-array-ext-oid");
    let mut s = connect(core);

    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("s", "SELECT m, a_text FROM g LIMIT 10", 0),
    );
    send_length_prefixed_message(&mut s, b'D', &describe_body(b'S', "s"));
    send_length_prefixed_message(&mut s, b'S', b"");
    let (kind, _) = read_message(&mut s);
    assert_eq!(kind, b'1', "ParseComplete");
    let (kind, _) = read_message(&mut s);
    assert_eq!(kind, b't', "ParameterDescription");
    let (kind, body) = read_message(&mut s);
    assert_eq!(kind, b'T', "RowDescription");
    assert_eq!(row_description_oids(&body), vec![1007, 1009]);
    let (kind, _) = read_message(&mut s);
    assert_eq!(kind, b'Z');

    // Parse で配列 OID（int4[]=1007）を宣言すると 0A000。
    let mut parse = parse_body("s2", "SELECT m FROM g WHERE id = $1 LIMIT 10", 1);
    parse.extend_from_slice(&1007i32.to_be_bytes());
    send_length_prefixed_message(&mut s, b'P', &parse);
    let (kind, body) = read_message(&mut s);
    assert_eq!(kind, b'E');
    assert!(String::from_utf8_lossy(&body).contains("0A000"));
    sync_and_ready(&mut s);

    // 接続は維持され、同じ接続で簡易クエリが通る。
    let (cols, _) = select(&mut s, "SELECT m FROM g LIMIT 1");
    assert_eq!(cols[0].1, 1007);
}

/// 配列（1 次元・多次元）のバイナリ結果形式は `0A000` で、接続は維持される
/// （TABLE-35 (4)・WIRE-14）。
#[test]
fn array_binary_result_format_is_0a000_and_connection_survives() {
    let (core, _g) = new_core(SchemaLimits::default(), "wire-array-binary");
    let mut s = connect(core);
    exec(
        &mut s,
        "INSERT INTO g (id, embedding, a_int, m) \
         VALUES (1, '[0.1,0.2]', '{1,2}', '{{1,2},{3,4}}') USING OPERATION_ID 'op-bin1'",
    );
    for (i, col) in ["a_int", "m"].into_iter().enumerate() {
        let (stmt, portal) = (format!("sb{i}"), format!("pb{i}"));
        let sql = format!("SELECT {col} FROM g WHERE id = 1 LIMIT 10");
        send_length_prefixed_message(&mut s, b'P', &parse_body(&stmt, &sql, 0));
        let (kind, _) = read_message(&mut s);
        assert_eq!(kind, b'1');
        // 結果 format code を binary(1) 1 件で Bind する。
        let mut bind = bind_body(&portal, &stmt);
        let n = bind.len();
        bind.truncate(n - 2);
        bind.extend_from_slice(&1i16.to_be_bytes());
        bind.extend_from_slice(&1i16.to_be_bytes());
        send_length_prefixed_message(&mut s, b'B', &bind);
        let (kind, body) = read_message(&mut s);
        assert_eq!(kind, b'E', "col={col}");
        assert!(
            String::from_utf8_lossy(&body).contains("0A000"),
            "col={col}"
        );
        sync_and_ready(&mut s);
        // 回復後、テキスト形式は通る。
        let (_, row) = select(&mut s, &sql);
        assert!(row[0].is_some(), "col={col}");
    }
}

/// 要素数上限（R4・AC3）: 設定値が多次元の宣言（`INTEGER[][]`）の要素総数にも効く。
/// 既定値では 6 要素を受理する。
#[test]
fn configured_array_limit_applies_to_multi_dimensional_columns() {
    let ddl = "CREATE TABLE lim (embedding VECTOR(2) NOT NULL, m INTEGER[][])";
    let (core, _g) = new_core_with_ddl(
        SchemaLimits::new(256, 4, 256),
        "wire-array-limit-small",
        ddl,
    );
    let mut s = connect(core);
    exec(
        &mut s,
        "INSERT INTO lim (id, embedding, m) VALUES (1, '[0.1,0.2]', '{{1,2},{3,4}}') \
         USING OPERATION_ID 'op-lim1'",
    );
    send_simple_query(
        &mut s,
        "INSERT INTO lim (id, embedding, m) VALUES (2, '[0.1,0.2]', '{{1,2,3},{4,5,6}}') \
         USING OPERATION_ID 'op-lim2'",
    );
    expect_error_response_with_sqlstate(&mut s, "54000");
    read_ready_for_query(&mut s);

    // 既定値では 6 要素を受理する。
    let (core, _g2) = new_core_with_ddl(SchemaLimits::default(), "wire-array-limit-default", ddl);
    let mut s = connect(core);
    exec(
        &mut s,
        "INSERT INTO lim (id, embedding, m) VALUES (1, '[0.1,0.2]', '{{1,2,3},{4,5,6}}') \
         USING OPERATION_ID 'op-lim3'",
    );
}
