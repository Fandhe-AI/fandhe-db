//! 明示・暗黙トランザクション内の表・索引の DDL（SQL-80・SQL-31・WIRE-16 のポインタ、Issue #3313。
//! 設計記録: `docs/design/explicit-transaction.md`「明示トランザクション内の DDL の方式」・
//! `docs/design/wire-multi-statement.md`）を、生バイトの wire クライアント越しに検証する結合テスト。
//!
//! psql の `-f`（マイグレーション道具の通常形）は文末に `;` を付けて 1 メッセージに 1 文ずつ送る。
//! `BEGIN; CREATE TABLE ...; CREATE INDEX ...; INSERT ...; COMMIT;` が通り、COMMIT で原子的に確定し、
//! ROLLBACK・失敗では表も索引も残らないことを固定する。複数文メッセージの暗黙トランザクションでも
//! 同じ DDL が受理され、後続の文の失敗でメッセージ全体（表を含む）がロールバックされる。
//! DDL 後に実行できる文は暫定の許可リストに限り、拡張クエリで送る DDL 後の SELECT は `0A000`
//! （受け皿は #3311）。`sql31_trailing_semicolon.rs` と同じ流儀。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql80-txn-table-ddl-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

/// DDL 権限を持つユーザー `alice`（`--ddl-allowed-users` 相当）で接続する。
fn connect(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn expect_tag(stream: &mut std::net::TcpStream, sql: &str, tag: &str, status: u8) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

/// 1 文を送り、`ErrorResponse` の SQLSTATE と `ReadyForQuery` の状態バイトを返す。
fn expect_error(stream: &mut std::net::TcpStream, sql: &str, sqlstate: &str, status: u8) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, sqlstate);
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

/// `SELECT id FROM <table>` の行数（表が無ければ `Err(SQLSTATE を含まない形)` ではなく panic するため、
/// 存在する表だけに使う）。
fn count_rows(stream: &mut std::net::TcpStream, table: &str) -> usize {
    send_simple_query(
        stream,
        &format!("SELECT id FROM {table} ORDER BY id LIMIT 10"),
    );
    let _ = read_row_description(stream);
    let mut n = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => n += 1,
            b'C' => {}
            b'Z' => return n,
            other => panic!("unexpected message {other}"),
        }
    }
}

const CREATE_T: &str = "CREATE TABLE t (id BIGINT PRIMARY KEY, body TEXT);";

#[test]
fn psql_style_migration_commits_atomically() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_tag(
        &mut stream,
        "CREATE INDEX t_body ON t (body);",
        "CREATE INDEX",
        b'T',
    );
    expect_tag(
        &mut stream,
        "INSERT INTO t (id, body) VALUES (1, 'a') USING OPERATION_ID 'mig-1';",
        "INSERT 0 1",
        b'T',
    );
    // 同じ Tx で作った表の読み取り。
    assert_eq!(count_rows(&mut stream, "t"), 1);
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    assert_eq!(count_rows(&mut stream, "t"), 1);
}

#[test]
fn psql_style_migration_rolls_back_without_residue() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_tag(
        &mut stream,
        "CREATE UNIQUE INDEX t_body_u ON t (body);",
        "CREATE INDEX",
        b'T',
    );
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    expect_error(&mut stream, "SELECT id FROM t LIMIT 1;", "42P01", b'I');
    // 同名の再作成が通る（カタログ・索引に残骸が無い）。
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'I');
}

#[test]
fn failed_ddl_in_a_transaction_needs_rollback_and_leaves_nothing() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    // 同名の再作成は `42P07`。Tx は失敗状態（`'E'`）で、COMMIT は確定せず `ROLLBACK` になる。
    expect_error(&mut stream, CREATE_T, "42P07", b'E');
    expect_error(
        &mut stream,
        "INSERT INTO t (id, body) VALUES (1, 'a') USING OPERATION_ID 'f-1';",
        "25P02",
        b'E',
    );
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    expect_error(&mut stream, "SELECT id FROM t LIMIT 1;", "42P01", b'I');
}

#[test]
fn implicit_transaction_message_with_ddl_commits_as_a_whole() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    send_simple_query(
        &mut stream,
        "CREATE TABLE a (id BIGINT PRIMARY KEY, body TEXT); \
         INSERT INTO a (id, body) VALUES (1, 'x') USING OPERATION_ID 'imp-1'",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_rows(&mut stream, "a"), 1);
}

#[test]
fn implicit_transaction_message_rolls_back_the_table_when_a_later_statement_fails() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    send_simple_query(
        &mut stream,
        "CREATE TABLE a (id BIGINT PRIMARY KEY, body TEXT); \
         INSERT INTO a (id, body) VALUES (1, 'x') USING OPERATION_ID 'imp-2'; \
         INSERT INTO no_such_table (id) VALUES (1) USING OPERATION_ID 'imp-3'",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    // 表ごとロールバックされている。
    expect_error(&mut stream, "SELECT id FROM a LIMIT 1;", "42P01", b'I');
}

#[test]
// tracked: #3311
fn statements_not_yet_wired_after_ddl_fail_the_transaction_with_0a000() {
    // DDL 後の UPDATE・DELETE 等は未対応。Tx は失敗状態になり ROLLBACK で戻る。
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_error(
        &mut stream,
        "UPDATE t SET body = 'b' WHERE id = 1 USING OPERATION_ID 'u-1';",
        "0A000",
        b'E',
    );
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    expect_error(&mut stream, "SELECT id FROM t LIMIT 1;", "42P01", b'I');
}

#[test]
fn psql_style_scan_ddl_on_a_populated_table_commits() {
    // Issue #3314: 行のある表への一意索引・SET NOT NULL（既存行の走査を伴う DDL）が、Tx の期限の確認点つきで
    // 文末 `;` の単独送信でも通り、COMMIT で確定する。
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_tag(
        &mut stream,
        "INSERT INTO t (id, body) VALUES (1, 'a') USING OPERATION_ID 'scan-1';",
        "INSERT 0 1",
        b'T',
    );
    expect_tag(
        &mut stream,
        "CREATE UNIQUE INDEX t_body_u ON t (body);",
        "CREATE INDEX",
        b'T',
    );
    expect_tag(
        &mut stream,
        "ALTER TABLE t ALTER COLUMN body SET NOT NULL;",
        "ALTER TABLE",
        b'T',
    );
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    expect_error(
        &mut stream,
        "INSERT INTO t (id, body) VALUES (2, 'a') USING OPERATION_ID 'scan-2';",
        "23505",
        b'I',
    );
}

#[test]
fn partitioned_dml_after_ddl_is_25001_and_leaves_nothing() {
    // Issue #3314: Tx 内で禁じる分割実行は DDL 後でも `25001`（`0A000` ではない）。Tx は失敗状態で、
    // ROLLBACK 後に表は残らない。
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_error(
        &mut stream,
        "DELETE FROM t WHERE id > 0 USING OPERATION_ID 'p-1' PARTITIONED;",
        "25001",
        b'E',
    );
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    expect_error(&mut stream, "SELECT id FROM t LIMIT 1;", "42P01", b'I');
}

#[test]
fn partitioned_dml_in_a_ddl_message_is_rejected_before_any_statement_runs() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    send_simple_query(
        &mut stream,
        "CREATE TABLE a (id BIGINT PRIMARY KEY, body TEXT); \
         DELETE FROM a WHERE id > 0 USING OPERATION_ID 'p-2' PARTITIONED",
    );
    expect_error_response_with_sqlstate(&mut stream, "25001");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    expect_error(&mut stream, "SELECT id FROM a LIMIT 1;", "42P01", b'I');
}

#[test]
fn statement_limit_applies_to_ddl_in_explicit_and_implicit_transactions() {
    use engine::sql::transaction::TransactionLimits;
    let path = temp_db::unique_db_path("sql80-txn-ddl-limit-wire");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_transaction_limits(
            TransactionLimits {
                max_duration: std::time::Duration::from_secs(20),
                max_statements: 2,
            },
        ),
    );
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_tag(
        &mut stream,
        "CREATE INDEX t_body ON t (body);",
        "CREATE INDEX",
        b'T',
    );
    expect_error(&mut stream, "DROP INDEX t_body;", "54000", b'E');
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    expect_error(&mut stream, "SELECT id FROM t LIMIT 1;", "42P01", b'I');
    // 暗黙 Tx（複数文メッセージ）。超過したメッセージ全体が残らない。
    send_simple_query(
        &mut stream,
        "CREATE TABLE a (id BIGINT PRIMARY KEY); \
         CREATE INDEX a_id ON a (id); \
         DROP INDEX a_id",
    );
    let _ = read_command_complete(&mut stream);
    let _ = read_command_complete(&mut stream);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    expect_error(&mut stream, "SELECT id FROM a LIMIT 1;", "42P01", b'I');
}

#[test]
fn psql_style_rollback_to_discards_only_the_ddl_after_the_savepoint() {
    // Issue #3317: 文末 `;` の `SAVEPOINT`・`ROLLBACK TO` が DDL の後でも通り、savepoint より前の DDL だけが残る。
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_tag(&mut stream, "SAVEPOINT s;", "SAVEPOINT", b'T');
    expect_tag(
        &mut stream,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, body TEXT);",
        "CREATE TABLE",
        b'T',
    );
    expect_tag(&mut stream, "ROLLBACK TO s;", "ROLLBACK", b'T');
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    assert_eq!(count_rows(&mut stream, "t"), 0);
    expect_error(&mut stream, "SELECT id FROM u LIMIT 1;", "42P01", b'I');
}

#[test]
fn psql_style_failed_ddl_recovers_with_rollback_to() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'T');
    expect_tag(&mut stream, "SAVEPOINT s;", "SAVEPOINT", b'T');
    expect_error(&mut stream, CREATE_T, "42P07", b'E');
    expect_tag(&mut stream, "ROLLBACK TO s;", "ROLLBACK", b'T');
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    assert_eq!(count_rows(&mut stream, "t"), 0);
}

#[test]
fn connection_drop_after_ddl_and_savepoint_leaves_nothing_and_frees_the_writer() {
    let (core, _guard) = new_core();
    let mut first = connect(Arc::clone(&core));
    expect_tag(&mut first, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut first, CREATE_T, "CREATE TABLE", b'T');
    expect_tag(&mut first, "SAVEPOINT s;", "SAVEPOINT", b'T');
    expect_tag(
        &mut first,
        "CREATE TABLE u (id BIGINT PRIMARY KEY);",
        "CREATE TABLE",
        b'T',
    );
    // 接続断（ソケットを閉じる）。
    drop(first);
    let mut second = connect(core);
    // writer が解放され、表は残っていない（同名の再作成が通る。解放は非同期のため短く再試行する）。
    let mut created = false;
    for _ in 0..50 {
        send_simple_query(&mut second, CREATE_T);
        let (kind, _) = read_message(&mut second);
        let _ = read_ready_for_query_status(&mut second);
        if kind == b'C' {
            created = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(created, "writer was not released after the connection drop");
    expect_error(&mut second, "SELECT id FROM u LIMIT 1;", "42P01", b'I');
}
/// SQL-80・Issue #3316: 関数・ポリシー・行セキュリティ・ロールの DDL も、psql 形（末尾 `;` 付きを 1 文ずつ単独送信）の
/// 明示 Tx で通り、COMMIT で原子的に確定する。積んだ後の読み取りは固定の `0A000`（古い判定で通さない）で、
/// `ROLLBACK` の後に通常へ戻る。
// tracked: #3311
#[test]
fn psql_style_security_ddl_commits_and_guards_followers() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'I');
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, "CREATE ROLE reader;", "CREATE ROLE", b'T');
    expect_tag(
        &mut stream,
        "CREATE POLICY p ON t FOR SELECT USING (body = 'x');",
        "CREATE POLICY",
        b'T',
    );
    expect_tag(
        &mut stream,
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY;",
        "ALTER TABLE",
        b'T',
    );
    // 積んだ後の読み取りは固定の `0A000`（失敗した Tx は `E`）。
    expect_error(&mut stream, "SELECT id FROM t LIMIT 1;", "0A000", b'E');
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    // ROLLBACK では何も残らない（同名の再作成が通る）。
    expect_tag(&mut stream, "CREATE ROLE reader;", "CREATE ROLE", b'I');
    expect_tag(&mut stream, "DROP ROLE reader;", "DROP ROLE", b'I');
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, "CREATE ROLE reader;", "CREATE ROLE", b'T');
    expect_tag(
        &mut stream,
        "CREATE POLICY p ON t FOR SELECT USING (body = 'x');",
        "CREATE POLICY",
        b'T',
    );
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    // COMMIT で確定している（重複は `42710`）。
    expect_error(&mut stream, "CREATE ROLE reader;", "42710", b'I');
    expect_error(
        &mut stream,
        "CREATE POLICY p ON t FOR SELECT USING (body = 'x');",
        "42710",
        b'I',
    );
}

/// ソケット切断（Tx の drop）では、積んだロール・ポリシーの DDL は何も残らない。
#[test]
fn dropped_connection_discards_security_ddl() {
    let (core, _guard) = new_core();
    {
        let mut stream = connect(core.clone());
        expect_tag(&mut stream, CREATE_T, "CREATE TABLE", b'I');
        expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
        expect_tag(&mut stream, "CREATE ROLE ghost;", "CREATE ROLE", b'T');
        expect_tag(
            &mut stream,
            "CREATE POLICY p ON t FOR SELECT USING (body = 'x');",
            "CREATE POLICY",
            b'T',
        );
        // 切断（Tx は commit されずに drop される）。
    }
    let mut stream = connect(core);
    // writer の解放を待って、同名の作成が通る。
    expect_tag(&mut stream, "CREATE ROLE ghost;", "CREATE ROLE", b'I');
    expect_tag(
        &mut stream,
        "CREATE POLICY p ON t FOR SELECT USING (body = 'x');",
        "CREATE POLICY",
        b'I',
    );
}
