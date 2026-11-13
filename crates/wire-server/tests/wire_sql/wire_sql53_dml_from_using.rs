//! `UPDATE ... FROM`／`DELETE ... USING` の簡易クエリ結合テスト（Issue #3202・SQL-53 (b)・WIRE-15 の
//! ポインタ）。psql の `-f` と同じ末尾 `;` 付き単独送信で、autocommit は `UPDATE n`／`DELETE n`・
//! 状態バイト `I`、明示トランザクションは `T` のまま `COMMIT` で永続化・`ROLLBACK` で巻き戻ることを固定する
//! （engine の暗黙 Tx 振り分けと `mark_written` が wire 経由でも働く契約）。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql53-dml-from-using-wire");
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
    storage
        .create_table(&TableSchema::new(
            "langs",
            vec![ColumnDef::new("lang", ColumnType::Text, false)],
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
    for (id, lang) in [(1, "ja"), (2, "en")] {
        expect_tag(
            stream,
            &format!(
                "INSERT INTO langs (id, lang) VALUES ({id}, '{lang}') USING OPERATION_ID 'lang-{id}';"
            ),
            "INSERT 0 1",
            b'I',
        );
    }
}

/// 行を返す SELECT の行数を数える（`ReadyForQuery` は `I` を期待）。
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

const UPD: &str = "UPDATE docs SET n = 9 FROM langs WHERE docs.lang = langs.lang;";
const DEL: &str = "DELETE FROM docs USING langs WHERE docs.lang = langs.lang;";
const UPDATED: &str = "SELECT id FROM docs WHERE n = 9 LIMIT 10";
const ALL: &str = "SELECT id FROM docs LIMIT 10";

#[test]
fn autocommit_update_from_and_delete_using_report_exact_tags() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    expect_tag(&mut stream, UPD, "UPDATE 2", b'I');
    assert_eq!(count(&mut stream, UPDATED), 2);
    expect_tag(&mut stream, DEL, "DELETE 2", b'I');
    assert_eq!(count(&mut stream, ALL), 1);
}

#[test]
fn explicit_transaction_commits_and_rolls_back() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, DEL, "DELETE 2", b'T');
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    assert_eq!(count(&mut stream, ALL), 3);

    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, UPD, "UPDATE 2", b'T');
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    assert_eq!(count(&mut stream, UPDATED), 2);
}
