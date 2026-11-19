//! `MERGE ... RETURNING` と `merge_action()`、コマンドタグ `MERGE n`（SQL-79・Issue #3356）の pg wire 結合テスト。
//!
//! 簡易クエリ（psql 流の末尾 `;` 付き単独送信を含む）で RowDescription（列名。`merge_action` は text）・
//! DataRow・CommandComplete `MERGE n`・ReadyForQuery `I` が返ること、拡張クエリ（Parse／Describe／Execute）が
//! 同じ列と行を返すこと、明示 Tx の中の実行と ROLLBACK／COMMIT、拒否の形（MERGE の外の `merge_action()`・式の内側）が
//! ErrorResponse になり ReadyForQuery の状態が正しいことを固定する。
//! サーバーはポート 0・一意な一時ディレクトリで独立させ、時間計測は使わない。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

const MERGE_UPDATE: &str =
    "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v \
     RETURNING t.k, t.v, merge_action()";
const MERGE_DELETE: &str = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE \
     RETURNING t.k, merge_action() AS act";

fn start() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql79-merge-returning");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut s = SessionState::default();
    s.allow_ddl();
    let ctx = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("tenant");
    for sql in [
        "CREATE TABLE t (k BIGINT PRIMARY KEY, v BIGINT)",
        "CREATE TABLE s (k BIGINT PRIMARY KEY, v BIGINT)",
        "INSERT INTO t (k, v) VALUES (1, 10)",
        "INSERT INTO s (k, v) VALUES (1, 100)",
    ] {
        core.execute_sql_in_session(&ctx, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (addr, guard)
}

fn client(addr: SocketAddr) -> TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn count_t(s: &mut TcpStream) -> String {
    send_simple_query(s, "SELECT COUNT(*) FROM t");
    read_row_description(s);
    let row = read_data_row(s);
    let _tag = read_command_complete(s);
    read_ready_for_query(s);
    row.first().cloned().flatten().expect("count")
}

fn send_sync(s: &mut TcpStream) {
    send_length_prefixed_message(s, b'S', &[]);
}

/// 拡張クエリの 1 往復の結果（受信したメッセージ種別の列・最後の CommandComplete のタグ・ErrorResponse の
/// SQLSTATE・最後の ReadyForQuery の状態）。
struct Round {
    types: Vec<u8>,
    tag: Option<String>,
    code: Option<String>,
    status: u8,
}

fn read_round(s: &mut TcpStream) -> Round {
    let mut r = Round {
        types: Vec::new(),
        tag: None,
        code: None,
        status: 0,
    };
    loop {
        let (ty, body) = read_message(s);
        r.types.push(ty);
        match ty {
            b'E' => {
                let text = String::from_utf8_lossy(&body).to_string();
                r.code = text
                    .split('\0')
                    .find_map(|f| f.strip_prefix('C').map(str::to_string));
            }
            b'C' => {
                r.tag = Some(
                    String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            b'Z' => {
                r.status = body.first().copied().unwrap_or(0);
                return r;
            }
            _ => {}
        }
    }
}

// 簡易クエリ: 末尾 `;` 付きの単独送信も同じ。RowDescription・DataRow・`MERGE 1`・ReadyForQuery `I`。
#[test]
fn simple_query_returns_rows_and_the_merge_tag() {
    let (addr, _g) = start();
    let mut s = client(addr);
    for sql in [MERGE_UPDATE.to_string(), format!("{MERGE_UPDATE};")] {
        send_simple_query(&mut s, &sql);
        let (cols, oids): (Vec<_>, Vec<_>) =
            read_row_description_with_oids(&mut s).into_iter().unzip();
        assert_eq!(cols, ["k", "v", "merge_action"], "{sql}");
        // `merge_action` は text（OID 25）。
        assert_eq!(oids.last().copied(), Some(25), "{sql}");
        let row = read_data_row(&mut s);
        assert_eq!(
            row,
            [
                Some("1".to_string()),
                Some("100".to_string()),
                Some("UPDATE".to_string())
            ],
            "{sql}"
        );
        assert_eq!(read_command_complete(&mut s), "MERGE 1", "{sql}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
    }
}

// 拡張クエリ: Describe（文）は RowDescription を返し、Execute は DataRow と `MERGE 1` を返す。
#[test]
fn extended_query_describes_and_executes_returning() {
    let (addr, _g) = start();
    let mut s = client(addr);

    send_length_prefixed_message(&mut s, b'P', &parse_body("m1", MERGE_DELETE, 0));
    let mut describe = vec![b'S'];
    describe.extend_from_slice(b"m1\0");
    send_length_prefixed_message(&mut s, b'D', &describe);
    send_sync(&mut s);
    let r = read_round(&mut s);
    assert_eq!(r.code, None);
    // ParseComplete・ParameterDescription・RowDescription・ReadyForQuery。
    assert_eq!(r.types, [b'1', b't', b'T', b'Z']);
    assert_eq!(r.status, b'I');
    assert_eq!(count_t(&mut s), "1");

    send_length_prefixed_message(&mut s, b'B', &bind_body("p1", "m1"));
    send_length_prefixed_message(&mut s, b'E', &execute_body("p1", 0));
    send_sync(&mut s);
    let r = read_round(&mut s);
    assert_eq!(r.code, None);
    assert_eq!(r.types, [b'2', b'D', b'C', b'Z']);
    assert_eq!(r.tag.as_deref(), Some("MERGE 1"));
    assert_eq!(r.status, b'I');
    assert_eq!(count_t(&mut s), "0");
}

// 明示 Tx: RETURNING つきが実行でき（Z は `T`）、ROLLBACK で痕跡が残らず、COMMIT で残る。
#[test]
fn explicit_transaction_runs_returning_then_rolls_back_or_commits() {
    let (addr, _g) = start();
    let mut s = client(addr);
    for (end, rows_left) in [("ROLLBACK", "1"), ("COMMIT", "0")] {
        send_simple_query(&mut s, "BEGIN");
        assert_eq!(read_command_complete(&mut s), "BEGIN");
        read_ready_for_query(&mut s);
        send_simple_query(&mut s, MERGE_DELETE);
        assert_eq!(read_row_description(&mut s), ["k", "act"]);
        assert_eq!(
            read_data_row(&mut s),
            [Some("1".to_string()), Some("DELETE".to_string())]
        );
        assert_eq!(read_command_complete(&mut s), "MERGE 1");
        assert_eq!(read_ready_for_query_status(&mut s), b'T');
        send_simple_query(&mut s, end);
        assert_eq!(read_command_complete(&mut s), end);
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
        assert_eq!(count_t(&mut s), rows_left, "{end}");
    }
}

// 拒否の形は ErrorResponse になり、ReadyForQuery の状態が正しい（autocommit は `I`、明示 Tx は `E`）。
#[test]
fn rejected_forms_return_an_error_and_a_correct_ready_status() {
    let (addr, _g) = start();
    let mut s = client(addr);
    for (sql, code) in [
        ("SELECT merge_action() FROM t", "42601"),
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING upper(merge_action())",
            "0A000",
        ),
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.v + s.v",
            "0A000",
        ),
    ] {
        send_simple_query(&mut s, sql);
        expect_error_response_with_sqlstate(&mut s, code);
        assert_eq!(read_ready_for_query_status(&mut s), b'I', "{sql}");
        assert_eq!(count_t(&mut s), "1", "{sql}");
    }
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, "SELECT merge_action() FROM t");
    expect_error_response_with_sqlstate(&mut s, "42601");
    assert_eq!(read_ready_for_query_status(&mut s), b'E');
    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    read_ready_for_query(&mut s);
}
