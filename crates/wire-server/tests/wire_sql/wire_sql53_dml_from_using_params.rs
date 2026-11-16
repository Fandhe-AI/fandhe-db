//! `UPDATE ... FROM`／`DELETE ... USING` の拡張クエリ `$n`（SQL-53 (b)・WIRE-12・Issue #3297）の
//! pg wire 結合テスト。Parse／Describe／Bind／Execute／Sync で、`ParameterDescription` の OID・
//! CommandComplete タグ・公開名重複の `42712` を固定する。サーバーはポート 0・一意な一時ディレクトリ。

use crate::common;
use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

const TEXT_OID: u32 = 25;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql53-dml-from-using-params-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("d", ColumnType::Date, true),
            ],
        ))
        .expect("create docs");
    storage
        .create_table(&TableSchema::new(
            "langs",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("label", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::BigInt, true),
                ColumnDef::new("d", ColumnType::Date, true),
            ],
        ))
        .expect("create langs");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn simple(stream: &mut TcpStream, sql: &str, tag: &str) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(stream), b'I', "{sql}");
}

fn seed(stream: &mut TcpStream) {
    for (id, lang) in [(1, "ja"), (2, "en")] {
        simple(
            stream,
            &format!(
                "INSERT INTO docs (id, embedding, lang, n) VALUES ({id}, '[0.{id},0.1]', '{lang}', 0) \
                 USING OPERATION_ID 'doc-{id}';"
            ),
            "INSERT 0 1",
        );
    }
    simple(
        stream,
        "INSERT INTO langs (id, lang, label) VALUES (1, 'ja', 'Japanese') USING OPERATION_ID 'lang-1';",
        "INSERT 0 1",
    );
}

fn describe_statement_body(name: &str) -> Vec<u8> {
    let mut body = vec![b'S'];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn bind_with_text_params(portal: &str, statement: &str, values: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&i16::try_from(values.len()).expect("count").to_be_bytes());
    for v in values {
        body.extend_from_slice(&i32::try_from(v.len()).expect("len").to_be_bytes());
        body.extend_from_slice(v.as_bytes());
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

/// Parse → Describe(S) → Bind → Execute → Sync。ParameterDescription の OID 列と CommandComplete
/// タグを返す。
fn extended(stream: &mut TcpStream, sql: &str, values: &[&str]) -> (Vec<u32>, String) {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(stream, b'D', &describe_statement_body(""));
    send_length_prefixed_message(stream, b'B', &bind_with_text_params("", "", values));
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    send_length_prefixed_message(stream, b'S', &[]);
    let mut oids = Vec::new();
    let mut tag = String::new();
    loop {
        let (t, body) = read_message(stream);
        match t {
            b't' => {
                let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
                for i in 0..n {
                    let at = 2 + i * 4;
                    oids.push(u32::from_be_bytes([
                        body[at],
                        body[at + 1],
                        body[at + 2],
                        body[at + 3],
                    ]));
                }
            }
            b'C' => tag = String::from_utf8_lossy(&body[..body.len() - 1]).to_string(),
            b'Z' => break,
            b'1' | b'2' | b'n' => {}
            other => panic!(
                "unexpected message {} {}",
                other as char,
                String::from_utf8_lossy(&body)
            ),
        }
    }
    (oids, tag)
}

#[test]
fn extended_update_from_and_delete_using_with_params() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);
    let (oids, tag) = extended(
        &mut stream,
        "UPDATE docs SET lang = $1 FROM langs WHERE docs.lang = langs.lang AND langs.label = $2",
        &["zz", "Japanese"],
    );
    assert_eq!(oids, vec![TEXT_OID, TEXT_OID]);
    assert_eq!(tag, "UPDATE 1");
    // 末尾 `;` 付きの拡張クエリも通る。
    let (oids, tag) = extended(
        &mut stream,
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label = $1;",
        &["Japanese"],
    );
    assert_eq!(oids, vec![TEXT_OID]);
    assert_eq!(tag, "DELETE 0");
}

#[test]
fn duplicate_exposed_names_report_42712() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    send_simple_query(
        &mut stream,
        "UPDATE docs SET n = 1 FROM docs WHERE docs.id = docs.id;",
    );
    expect_error_response_with_sqlstate(&mut stream, "42712");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// SET 右辺の FROM 側修飾列（`langs.n`）は FROM 側の表の列型で `$n` を推論する（書き込み先の
/// 同名列 `docs.n`〔INTEGER〕ではなく `langs.n`〔TEXT〕）。
#[test]
fn set_expr_param_type_follows_from_side_qualified_column() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body(
            "",
            "UPDATE docs SET n = langs.n + $1 FROM langs WHERE docs.lang = langs.lang",
            0,
        ),
    );
    send_length_prefixed_message(&mut stream, b'D', &describe_statement_body(""));
    send_length_prefixed_message(&mut stream, b'S', &[]);
    let mut oids = Vec::new();
    loop {
        let (t, body) = read_message(&mut stream);
        match t {
            b't' => {
                let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
                for i in 0..n {
                    let at = 2 + i * 4;
                    oids.push(u32::from_be_bytes([
                        body[at],
                        body[at + 1],
                        body[at + 2],
                        body[at + 3],
                    ]));
                }
            }
            b'Z' => break,
            b'1' | b'n' => {}
            other => panic!(
                "unexpected message {} {}",
                other as char,
                String::from_utf8_lossy(&body)
            ),
        }
    }
    assert_eq!(oids, vec![20]); // int8（BIGINT）
}

/// Parse → Describe(S) → Bind → Execute → Sync が全段で成功することを確かめる（DATE 列へ `$n` を
/// 渡す形。Describe 用ダミー値が実値として検証され `22007` になる回帰の再現。PR #3472 指摘）。
fn assert_extended_ok(sql: &str, values: &[&str], expect_tag: &str) {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);
    let (_oids, tag) = extended(&mut stream, sql, values);
    assert_eq!(tag, expect_tag, "{sql}");
}

#[test]
fn describe_date_param_with_trailing_semicolon() {
    assert_extended_ok(
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.d = $1;",
        &["2024-01-02"],
        "DELETE 0",
    );
}

#[test]
fn describe_date_param_through_view_source() {
    let (core, _g) = new_core();
    {
        let mut session = engine::sql::mode::SessionState::default();
        session.allow_ddl();
        let ctx = engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id");
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE VIEW v AS SELECT id, lang, d FROM langs WHERE label = 'Japanese'",
        )
        .expect("create view");
    }
    let mut stream = connect(core);
    seed(&mut stream);
    let (_oids, tag) = extended(
        &mut stream,
        "DELETE FROM docs USING v WHERE docs.lang = v.lang AND v.d = $1",
        &["2024-01-02"],
    );
    assert_eq!(tag, "DELETE 0");
}

#[test]
fn describe_date_param_in_row_value_assignment() {
    assert_extended_ok(
        "UPDATE docs SET (d, lang) = ($1, $2) FROM langs WHERE docs.lang = langs.lang",
        &["2024-01-02", "ja"],
        "UPDATE 1",
    );
}
