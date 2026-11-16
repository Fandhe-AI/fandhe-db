//! 別名つき単一表 `UPDATE`／`DELETE`（FROM／USING なし）の wire 結合テスト（Issue #3291・SQL-53・WIRE-15 の
//! ポインタ）。psql の `-f` と同じ末尾 `;` 付き単独送信（簡易クエリ）と、拡張クエリの `$n` 付き
//! Parse／Describe／Bind／Execute が、別名なし文と同じタグ・同じ ParameterDescription・同じ行になること、
//! 本名修飾が `42P01` の ErrorResponse になり接続が使い続けられることを固定する。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql53-single-alias-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create docs");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn connect(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn expect_tag(stream: &mut std::net::TcpStream, sql: &str, tag: &str, status: u8) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

fn seed(stream: &mut std::net::TcpStream) {
    for (id, lang) in [(1, "ja"), (2, "en"), (3, "fr")] {
        expect_tag(
            stream,
            &format!(
                "INSERT INTO docs (id, embedding, lang, n) VALUES ({id}, '[0.{id},0.1]', '{lang}', 0) \
                 USING OPERATION_ID 'doc-{id}';"
            ),
            "INSERT 0 1",
            b'I',
        );
    }
}

fn count(stream: &mut std::net::TcpStream, sql: &str) -> usize {
    send_simple_query(stream, sql);
    let _ = read_row_description(stream);
    let mut n = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => n += 1,
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    assert_eq!(read_ready_for_query_status(stream), b'I');
    n
}

#[test]
fn simple_query_aliased_update_and_delete_report_exact_tags() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    expect_tag(
        &mut stream,
        "UPDATE docs AS d SET n = 9 WHERE d.lang = 'ja';",
        "UPDATE 1",
        b'I',
    );
    assert_eq!(
        count(&mut stream, "SELECT id FROM docs WHERE n = 9 LIMIT 10"),
        1
    );

    // 明示トランザクション中は `T` のまま進み、ROLLBACK で巻き戻る。
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(
        &mut stream,
        "DELETE FROM docs d WHERE d.lang = 'en';",
        "DELETE 1",
        b'T',
    );
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    assert_eq!(count(&mut stream, "SELECT id FROM docs LIMIT 10"), 3);

    expect_tag(
        &mut stream,
        "DELETE FROM docs d WHERE d.lang = 'en';",
        "DELETE 1",
        b'I',
    );
    assert_eq!(count(&mut stream, "SELECT id FROM docs LIMIT 10"), 2);
}

#[test]
fn original_name_qualifier_is_42p01_and_connection_survives() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    send_simple_query(
        &mut stream,
        "UPDATE docs AS d SET n = 9 WHERE docs.lang = 'ja';",
    );
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    expect_tag(
        &mut stream,
        "UPDATE docs AS d SET n = 9 WHERE d.lang = 'ja';",
        "UPDATE 1",
        b'I',
    );
}

fn send_sync(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'S', &[]);
}

fn assert_ready_for_query(stream: &mut std::net::TcpStream) {
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
}

/// 拡張クエリ: Parse／Describe／Bind／Execute。ParameterDescription と実行結果を返す。
fn extended_update(
    stream: &mut std::net::TcpStream,
    name: &str,
    sql: &str,
    params: &[&str],
) -> (Vec<u8>, String) {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'1',
        "ParseComplete for {sql}: {}",
        String::from_utf8_lossy(&body)
    );
    let mut desc = vec![b'S'];
    desc.extend_from_slice(name.as_bytes());
    desc.push(0);
    send_length_prefixed_message(stream, b'D', &desc);
    let (kind, param_desc) = read_message(stream);
    assert_eq!(kind, b't', "ParameterDescription for {sql}");
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'n', "NoData for {sql}");

    let mut bind = vec![0u8]; // portal ""
    bind.extend_from_slice(name.as_bytes());
    bind.push(0);
    bind.extend_from_slice(&0i16.to_be_bytes());
    bind.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for p in params {
        bind.extend_from_slice(&(p.len() as i32).to_be_bytes());
        bind.extend_from_slice(p.as_bytes());
    }
    bind.extend_from_slice(&0i16.to_be_bytes());
    send_length_prefixed_message(stream, b'B', &bind);
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "BindComplete for {sql}");
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    let (kind, tag) = read_message(stream);
    assert_eq!(kind, b'C', "CommandComplete for {sql}");
    send_sync(stream);
    assert_ready_for_query(stream);
    (
        param_desc,
        String::from_utf8_lossy(&tag[..tag.len().saturating_sub(1)]).into_owned(),
    )
}

#[test]
fn extended_query_with_params_matches_unaliased() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    let (pd_aliased, tag_aliased) = extended_update(
        &mut stream,
        "s_alias",
        "UPDATE docs AS d SET n = $1 WHERE d.lang = $2",
        &["9", "ja"],
    );
    let (pd_plain, tag_plain) = extended_update(
        &mut stream,
        "s_plain",
        "UPDATE docs SET n = $1 WHERE lang = $2",
        &["9", "en"],
    );
    assert_eq!(pd_aliased, pd_plain);
    assert_eq!(tag_aliased, "UPDATE 1");
    assert_eq!(tag_aliased, tag_plain);
    assert_eq!(
        count(&mut stream, "SELECT id FROM docs WHERE n = 9 LIMIT 10"),
        2
    );
}
