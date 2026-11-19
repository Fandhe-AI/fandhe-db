//! `MERGE` と制約・生成列・トリガ・ビュー・RLS の規則（SQL-79・Issue #3355。親 #3349、設計記録
//! `docs/design/sql-merge.md` の D8・D12・§11）の pg wire 結合テスト。
//!
//! 簡易クエリ（psql 流の末尾 `;` 付き単独送信を含む）と拡張クエリ（Parse／Bind／Execute／Sync）で、
//! ビュー対象・文トリガつきの表・利用者ポリシーつきの表への MERGE が `CommandComplete MERGE n` で完了し、
//! INSTEAD OF つきのビュー（`0A000`）・ポリシー違反（`42501`）が ErrorResponse の SQLSTATE と
//! `ReadyForQuery` の状態になり、行が変わらないことを固定する。サーバーはポート 0・一意な一時ディレクトリで
//! 独立させ、時間計測は使わない。

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

fn start() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql79-merge-rules");
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
        // t = {1:10, 2:20, 3:30}、s = {2:200, 3:300, 4:400}。
        "INSERT INTO t (k, v) VALUES (1, 10), (2, 20), (3, 30)",
        "INSERT INTO s (k, v) VALUES (2, 200), (3, 300), (4, 400)",
        // ビュー対象: v < 25 の行（k = 1, 2）だけが見える。
        "CREATE VIEW tv AS SELECT k, v FROM t WHERE v < 25",
        // 利用者ポリシー: 新しい行も既存行も v < 100 でなければならない。
        "CREATE TABLE p (k BIGINT PRIMARY KEY, v BIGINT)",
        "INSERT INTO p (k, v) VALUES (1, 10), (2, 20)",
        "ALTER TABLE p ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY p_all ON p FOR ALL USING (v < 100)",
        // 文トリガと INSTEAD OF（SCHEMA 配下の利用者表）。
        "CREATE SCHEMA w",
        "CREATE TABLE w.pt (k BIGINT, v BIGINT)",
        "INSERT INTO w.pt (k, v) VALUES (1, 10), (2, 20)",
        "CREATE TABLE audit (n BIGINT)",
        "CREATE FUNCTION f_cnt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit (n) VALUES (1); RETURN NULL; END $$",
        "CREATE TRIGGER g_cnt AFTER UPDATE ON w.pt FOR EACH STATEMENT EXECUTE FUNCTION f_cnt()",
        "CREATE VIEW w.pv AS SELECT k, v FROM w.pt",
        "CREATE FUNCTION f_io() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
        "CREATE TRIGGER g_io INSTEAD OF DELETE ON w.pv FOR EACH ROW EXECUTE FUNCTION f_io()",
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

/// `SELECT k, v FROM <table> ORDER BY k` の `k:v` を `,` で連結した文字列（簡易クエリ）。
fn rows_of(s: &mut TcpStream, table: &str) -> String {
    send_simple_query(s, &format!("SELECT k, v FROM {table} ORDER BY k"));
    read_row_description(s);
    let mut out = Vec::new();
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'D' => {
                let mut pos = 2usize;
                let mut cells = Vec::new();
                for _ in 0..2 {
                    let len = i32::from_be_bytes([
                        body[pos],
                        body[pos + 1],
                        body[pos + 2],
                        body[pos + 3],
                    ]);
                    let len = usize::try_from(len).expect("non-null");
                    pos += 4;
                    cells.push(String::from_utf8_lossy(&body[pos..pos + len]).to_string());
                    pos += len;
                }
                out.push(cells.join(":"));
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(s);
    out.join(",")
}

/// 拡張クエリの `Sync` までの全メッセージを読み、(ErrorResponse の SQLSTATE, 最後の CommandComplete の
/// タグ, ReadyForQuery の状態) を返す。
fn read_until_ready(s: &mut TcpStream) -> (Option<String>, Option<String>, u8) {
    let mut code = None;
    let mut tag = None;
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'E' => {
                let text = String::from_utf8_lossy(&body).to_string();
                code = text
                    .split('\0')
                    .find_map(|f| f.strip_prefix('C').map(str::to_string));
            }
            b'C' => {
                tag = Some(
                    String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            b'Z' => return (code, tag, body.first().copied().unwrap_or(0)),
            _ => {}
        }
    }
}

fn simple(s: &mut TcpStream, sql: &str, want_tag: &str) {
    send_simple_query(s, sql);
    assert_eq!(read_command_complete(s), want_tag, "{sql}");
    assert_eq!(read_ready_for_query_status(s), b'I', "{sql}");
}

fn simple_error(s: &mut TcpStream, sql: &str, sqlstate: &str) {
    send_simple_query(s, sql);
    expect_error_response_with_sqlstate(s, sqlstate);
    assert_eq!(read_ready_for_query_status(s), b'I', "{sql}");
}

fn bind_no_params() -> Vec<u8> {
    let mut body = Vec::new();
    body.push(0); // portal ""
    body.push(0); // statement ""
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

#[test]
fn view_targets_apply_the_view_predicate_over_the_wire() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // 末尾 `;` 付きの単独送信。ビューの外の k = 3 は、元に居ても一致しない。
    simple(
        &mut s,
        "MERGE INTO tv USING s ON tv.k = s.k WHEN MATCHED THEN UPDATE SET v = 5;",
        "MERGE 1",
    );
    assert_eq!(rows_of(&mut s, "t"), "1:10,2:5,3:30");
    // 拡張クエリでも同じ。
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body(
            "",
            "MERGE INTO tv USING s ON tv.k = s.k WHEN NOT MATCHED BY SOURCE THEN DELETE",
            0,
        ),
    );
    send_length_prefixed_message(&mut s, b'B', &bind_no_params());
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);
    let (code, tag, status) = read_until_ready(&mut s);
    assert_eq!(code, None);
    assert_eq!(tag.as_deref(), Some("MERGE 1"));
    assert_eq!(status, b'I');
    assert_eq!(rows_of(&mut s, "t"), "2:5,3:30");
}

#[test]
fn a_view_with_instead_of_triggers_is_rejected_over_the_wire() {
    let (addr, _g) = start();
    let mut s = client(addr);
    simple_error(
        &mut s,
        "MERGE INTO w.pv AS d USING s ON d.k = s.k WHEN MATCHED THEN DELETE;",
        "0A000",
    );
    assert_eq!(rows_of(&mut s, "w.pt"), "1:10,2:20");
    // 同じ接続は ReadyForQuery の後も使える。
    assert_eq!(rows_of(&mut s, "t"), "1:10,2:20,3:30");
}

#[test]
fn statement_triggers_fire_once_per_statement_over_the_wire() {
    let (addr, _g) = start();
    let mut s = client(addr);
    simple(
        &mut s,
        "MERGE INTO w.pt AS d USING s ON d.k = s.k WHEN MATCHED THEN UPDATE SET v = 0;",
        "MERGE 1",
    );
    assert_eq!(rows_of(&mut s, "w.pt"), "1:10,2:0");
    send_simple_query(&mut s, "SELECT n FROM audit");
    read_row_description(&mut s);
    let mut rows = 0;
    loop {
        let (ty, _) = read_message(&mut s);
        match ty {
            b'D' => rows += 1,
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(&mut s);
    assert_eq!(rows, 1, "AFTER UPDATE の文トリガは 1 回だけ");
}

#[test]
fn user_policies_are_enforced_over_the_wire() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // s.v = 200 は WITH CHECK（v < 100 を USING で代用）を満たさない。文全体が拒否され、行は変わらない。
    simple_error(
        &mut s,
        "MERGE INTO p USING s ON p.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v;",
        "42501",
    );
    assert_eq!(rows_of(&mut s, "p"), "1:10,2:20");
    // 満たす値なら通る。
    simple(
        &mut s,
        "MERGE INTO p USING s ON p.k = s.k WHEN MATCHED THEN UPDATE SET v = 50;",
        "MERGE 1",
    );
    assert_eq!(rows_of(&mut s, "p"), "1:10,2:50");
}
