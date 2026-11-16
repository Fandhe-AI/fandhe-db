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

/// 末尾 `;` 付き単独送信（psql 形）の `USING OPERATION_ID`（Issue #3203・RECOVER-10/11）: 初回は正確なタグ、
/// 再送は `23505`、別文への再利用は `22023` で、エラー後も接続は ReadyForQuery（`I`）へ戻り使い続けられる。
#[test]
fn operation_id_forms_report_tags_resend_and_reuse_errors() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    let upd =
        "UPDATE docs SET n = 9 FROM langs WHERE docs.lang = langs.lang USING OPERATION_ID 'w-x';";
    let del = "DELETE FROM docs USING langs WHERE docs.lang = langs.lang USING OPERATION_ID 'w-y';";
    expect_tag(&mut stream, upd, "UPDATE 2", b'I');
    // 再送は再適用されない（再送の間に一致行を増やしても n=9 の行数は変わらない）。
    expect_tag(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang, n) VALUES (4, '[0.4,0.1]', 'ja', 0) USING OPERATION_ID 'doc-4';",
        "INSERT 0 1",
        b'I',
    );
    send_simple_query(&mut stream, upd);
    expect_error_response_with_sqlstate(&mut stream, "23505");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count(&mut stream, UPDATED), 2);

    // 別文へ同じ ID を再利用すると 22023。
    send_simple_query(&mut stream, &del.replace("'w-y'", "'w-x'"));
    expect_error_response_with_sqlstate(&mut stream, "22023");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    expect_tag(&mut stream, del, "DELETE 3", b'I');
    send_simple_query(&mut stream, del);
    expect_error_response_with_sqlstate(&mut stream, "23505");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count(&mut stream, ALL), 1);
}

/// `EXPLAIN ...` の応答（`QUERY PLAN` 単一列・行・`EXPLAIN` タグ）を読み、行本文を連結して返す。
fn expect_explain(stream: &mut std::net::TcpStream, sql: &str, status: u8) -> String {
    send_simple_query(stream, sql);
    assert_eq!(
        read_row_description(stream),
        vec!["QUERY PLAN".to_string()],
        "{sql}"
    );
    let mut body = String::new();
    loop {
        let (kind, payload) = read_message(stream);
        match kind {
            b'D' => body.push_str(&String::from_utf8_lossy(&payload)),
            b'C' => {
                assert!(
                    String::from_utf8_lossy(&payload).starts_with("EXPLAIN"),
                    "{sql}"
                );
                break;
            }
            other => panic!("unexpected message {other}: {sql}"),
        }
    }
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
    body
}

/// 静的な `EXPLAIN`（Issue #3293）は末尾 `;` 付きの単独送信でも結合計画を返し、行を変えない。
#[test]
fn static_explain_returns_the_join_plan_without_writing() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    for sql in [format!("EXPLAIN {UPD}"), format!("EXPLAIN {DEL}")] {
        let body = expect_explain(&mut stream, &sql, b'I');
        assert!(body.contains("join_method: hash"), "{sql}: {body}");
    }
    assert_eq!(count(&mut stream, UPDATED), 0);
    assert_eq!(count(&mut stream, ALL), 3);
}

/// `EXPLAIN ANALYZE`（Issue #3293）は autocommit では書き込みが残り、明示 Tx では `ROLLBACK` で戻る。
#[test]
fn explain_analyze_writes_and_follows_transaction_state() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);

    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    let body = expect_explain(&mut stream, &format!("EXPLAIN ANALYZE {DEL}"), b'T');
    assert!(body.contains("actual_rows"), "{body}");
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    assert_eq!(count(&mut stream, ALL), 3);

    expect_explain(&mut stream, &format!("EXPLAIN ANALYZE {UPD}"), b'I');
    assert_eq!(count(&mut stream, UPDATED), 2);
}

/// 管理用セッションで DDL を 1 文実行する（wire 経由の DDL 権限とは独立）。
fn admin_ddl(core: &EngineCore, sql: &str) {
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    let ctx = engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id");
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Issue #3292: FROM／USING 側の単純形ビューは、末尾 `;` 付き単独送信でも `UPDATE n`／`DELETE n` を返す。
#[test]
fn view_source_reports_exact_tags_with_trailing_semicolon() {
    let (core, _g) = new_core();
    admin_ddl(&core, "CREATE VIEW lv AS SELECT id, lang FROM langs");
    let mut stream = connect(core);
    seed(&mut stream);

    expect_tag(
        &mut stream,
        "UPDATE docs SET n = 9 FROM lv WHERE docs.lang = lv.lang;",
        "UPDATE 2",
        b'I',
    );
    assert_eq!(count(&mut stream, UPDATED), 2);
    expect_tag(
        &mut stream,
        "DELETE FROM docs USING lv WHERE docs.lang = lv.lang;",
        "DELETE 2",
        b'I',
    );
    assert_eq!(count(&mut stream, ALL), 1);
}

/// Issue #3292: 一時表だけの形は同じ接続の簡易クエリで通る（同名の永続表には作用しない）。
#[test]
fn temp_only_joined_dml_reports_exact_tags() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    seed(&mut stream);
    for sql in [
        "CREATE TEMP TABLE docs (id BIGINT PRIMARY KEY, lang TEXT, n INTEGER);",
        "CREATE TEMP TABLE langs (id BIGINT PRIMARY KEY, lang TEXT);",
    ] {
        send_simple_query(&mut stream, sql);
        let _ = read_command_complete(&mut stream);
        assert_eq!(read_ready_for_query_status(&mut stream), b'I', "{sql}");
    }
    expect_tag(
        &mut stream,
        "INSERT INTO docs (id, lang, n) VALUES (1, 'ja', 0), (2, 'en', 0), (3, 'fr', 0);",
        "INSERT 0 3",
        b'I',
    );
    expect_tag(
        &mut stream,
        "INSERT INTO langs (id, lang) VALUES (1, 'ja'), (2, 'en');",
        "INSERT 0 2",
        b'I',
    );
    expect_tag(&mut stream, UPD, "UPDATE 2", b'I');
    assert_eq!(count(&mut stream, UPDATED), 2);
    expect_tag(&mut stream, DEL, "DELETE 2", b'I');
    assert_eq!(count(&mut stream, ALL), 1);
}
