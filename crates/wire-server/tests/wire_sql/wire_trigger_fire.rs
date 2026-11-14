//! INSERT トリガの発火（BEFORE ROW・AFTER STATEMENT）を wire 越しに通す E2E（TABLE-45・SQL-68・RLS-10。
//! Issue #3185）。`EngineCore` の既定 invoker（本番の PL/pgSQL トリガ実行）を注入なしで使い、
//! 簡易クエリ（psql と同じ末尾 `;` 付きの単独文を含む）と拡張クエリの両方で、セッション入口が発火元の
//! 主体の写しを張っていることを確認する。エラー契約・テナント境界の確定オラクルは
//! `crates/engine/tests/table_rls/table45_trigger_fire.rs`（in-process）。
//! 設計記録は `docs/design/plpgsql-subset.md` の「Issue #3185 追記」。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn serve() -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-trigger-fire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice", "bob"]);
    (addr, guard)
}

fn connect(addr: std::net::SocketAddr, user: &str, pw: &str) -> TcpStream {
    authenticate_to_ready_for_query(addr, user, pw)
}

/// 成功する 1 文を送り、コマンドタグを返す。
fn exec(s: &mut TcpStream, sql: &str) -> String {
    send_simple_query(s, sql);
    let tag = read_command_complete(s);
    read_ready_for_query(s);
    tag
}

/// 1 列の SELECT を送り、全行の値を返す。
fn select_col(s: &mut TcpStream, sql: &str) -> Vec<String> {
    send_simple_query(s, sql);
    let _ = read_row_description(s);
    let mut out = Vec::new();
    loop {
        let (kind, body) = read_message(s);
        match kind {
            b'D' => {
                // DataRow: 列数(i16) + 列長(i32) + 値。1 列だけを読む。
                let len = body
                    .get(2..6)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(i32::from_be_bytes)
                    .expect("column length");
                let n = usize::try_from(len).expect("non-null column");
                let v = body.get(6..6 + n).expect("column value");
                out.push(String::from_utf8_lossy(v).to_string());
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(s);
    out
}

fn create_tt(s: &mut TcpStream) {
    exec(s, "CREATE SCHEMA s");
    exec(s, "CREATE TABLE s.tt (n INTEGER)");
}

/// 対象表はテナントの SCHEMA `s` の `s.tt`（共有 public 表へのトリガは定義時に拒否される。Issue #3361）。
/// 監査表 `audit` は public のまま（本体の INSERT は SCHEMA 修飾の書き込み先を受けない）。
fn setup(s: &mut TcpStream) {
    create_tt(s);
    exec(s, "CREATE TABLE audit (lv TEXT)");
    exec(
        s,
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.n := NEW.n + 100; RETURN NEW; END $$",
    );
    exec(
        s,
        "CREATE FUNCTION stmt_audit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (lv) VALUES (TG_LEVEL); RETURN NULL; END $$",
    );
}

/// 成功経路の `CREATE TRIGGER` タグ・BEFORE ROW の書き換え・AFTER STATEMENT の監査行が、簡易クエリ
/// （末尾 `;` の有無どちらでも）で発火する。
#[test]
fn wire_simple_query_fires_before_row_and_after_statement() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    setup(&mut s);
    assert_eq!(
        exec(
            &mut s,
            "CREATE TRIGGER tb BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION bump()"
        ),
        "CREATE TRIGGER"
    );
    assert_eq!(
        exec(
            &mut s,
            "CREATE TRIGGER ts AFTER INSERT ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()"
        ),
        "CREATE TRIGGER"
    );
    assert_eq!(
        exec(&mut s, "INSERT INTO s.tt (n) VALUES (1);"),
        "INSERT 0 1"
    );
    assert_eq!(
        exec(&mut s, "INSERT INTO s.tt (n) VALUES (2)"),
        "INSERT 0 1"
    );
    let mut v = select_col(&mut s, "SELECT n FROM s.tt");
    v.sort();
    assert_eq!(v, vec!["101", "102"]);
    assert_eq!(
        select_col(&mut s, "SELECT lv FROM audit"),
        vec!["STATEMENT", "STATEMENT"]
    );
}

/// BEFORE UPDATE ROW の書き換えと AFTER DELETE STATEMENT の監査行が、末尾 `;` 付きの単独送信・
/// 複数文の簡易クエリで発火する（Issue #3282。NoSQL 表層と同じ実行器・発火計画を SQL 側でも固定する）。
#[test]
fn wire_simple_query_fires_update_and_delete_triggers() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    setup(&mut s);
    exec(&mut s, "INSERT INTO s.tt (n) VALUES (1)");
    exec(
        &mut s,
        "CREATE TRIGGER tu BEFORE UPDATE ON s.tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    exec(
        &mut s,
        "CREATE TRIGGER td AFTER DELETE ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()",
    );
    assert_eq!(
        exec(&mut s, "UPDATE s.tt SET n = 5 WHERE n = 1;"),
        "UPDATE 1"
    );
    assert_eq!(select_col(&mut s, "SELECT n FROM s.tt"), vec!["105"]);
    assert_eq!(exec(&mut s, "DELETE FROM s.tt WHERE n = 105;"), "DELETE 1");
    assert_eq!(
        select_col(&mut s, "SELECT lv FROM audit"),
        vec!["STATEMENT"]
    );
    assert_eq!(
        select_col(&mut s, "SELECT n FROM s.tt"),
        Vec::<String>::new()
    );
}

/// 拡張クエリ（Parse/Bind/Execute/Sync）で送った INSERT でも発火する。
#[test]
fn wire_extended_query_fires_trigger() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    setup(&mut s);
    exec(
        &mut s,
        "CREATE TRIGGER tb BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("s1", "INSERT INTO s.tt (n) VALUES (5)", 0),
    );
    assert_eq!(read_message(&mut s).0, b'1');
    send_length_prefixed_message(&mut s, b'B', &bind_body("p1", "s1"));
    assert_eq!(read_message(&mut s).0, b'2');
    send_length_prefixed_message(&mut s, b'E', &execute_body("p1", 0));
    assert_eq!(read_message(&mut s).0, b'C');
    send_length_prefixed_message(&mut s, b'S', &[]);
    assert_eq!(read_message(&mut s).0, b'Z');
    assert_eq!(select_col(&mut s, "SELECT n FROM s.tt"), vec!["105"]);
}

/// 本体の `DELETE` と表を読む `SELECT`（Issue #3187）が、psql と同じ末尾 `;` 付きの単独送信でも通る。
/// `DELETE` は同じ Tx で先に書いた監査行を消し、`SELECT ... INTO` は自 Tx の未 commit の行を読む。
#[test]
fn wire_trigger_body_delete_and_select_run() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    setup(&mut s);
    exec(
        &mut s,
        "CREATE FUNCTION dsel() RETURNS trigger LANGUAGE plpgsql AS $$ \
         DECLARE c INTEGER; \
         BEGIN INSERT INTO audit (lv) VALUES ('x'); INSERT INTO audit (lv) VALUES ('y'); \
         DELETE FROM audit WHERE lv = 'x'; \
         SELECT count(*) INTO c FROM audit; NEW.n := c; RETURN NEW; END $$",
    );
    exec(
        &mut s,
        "CREATE TRIGGER tb BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION dsel()",
    );
    assert_eq!(
        exec(&mut s, "INSERT INTO s.tt (n) VALUES (9);"),
        "INSERT 0 1"
    );
    assert_eq!(select_col(&mut s, "SELECT n FROM s.tt"), vec!["1"]);
    assert_eq!(select_col(&mut s, "SELECT lv FROM audit"), vec!["y"]);
}

/// 窓口の許可リストに無い本体（`INSERT ... SELECT`）は `0A000` の ErrorResponse と ReadyForQuery で返り、
/// 同じ接続で次の文が実行でき、本表・監査に残渣が無い。
#[test]
fn wire_unsupported_body_is_0a000_and_connection_survives() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    setup(&mut s);
    exec(
        &mut s,
        "CREATE FUNCTION upd() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (lv) VALUES ('x'); INSERT INTO audit (lv) SELECT 'z'; RETURN NEW; END $$",
    );
    exec(
        &mut s,
        "CREATE TRIGGER tb BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION upd()",
    );
    send_simple_query(&mut s, "INSERT INTO s.tt (n) VALUES (1);");
    expect_error_response_with_sqlstate(&mut s, "0A000");
    read_ready_for_query(&mut s);
    assert!(select_col(&mut s, "SELECT n FROM s.tt").is_empty());
    assert!(select_col(&mut s, "SELECT lv FROM audit").is_empty());
}

/// 別テナント（bob）の INSERT では alice のトリガが発火せず、alice の監査行も bob から見えない。
#[test]
fn wire_trigger_does_not_fire_for_other_tenant() {
    let (addr, _g) = serve();
    let mut a = connect(addr, "alice", "pw-alice");
    setup(&mut a);
    exec(
        &mut a,
        "CREATE TRIGGER tb BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    exec(
        &mut a,
        "CREATE TRIGGER ts AFTER INSERT ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()",
    );
    let mut b = connect(addr, "bob", "pw-bob");
    create_tt(&mut b);
    assert_eq!(
        exec(&mut b, "INSERT INTO s.tt (n) VALUES (1);"),
        "INSERT 0 1"
    );
    assert_eq!(select_col(&mut b, "SELECT n FROM s.tt"), vec!["1"]);
    assert!(select_col(&mut b, "SELECT lv FROM audit").is_empty());
    assert!(select_col(&mut a, "SELECT lv FROM audit").is_empty());
}

/// 台帳（Issue #3188）: `USING OPERATION_ID` つきの INSERT（末尾 `;` 付きの単独送信）の再送は `23505` で、
/// トリガ本体は再実行されず、同じ接続で次の文が実行できる。内容違いの再送は `22023`。他テナントが同じ
/// operation_id を使っても alice のトリガは発火しない。
#[test]
fn wire_replay_with_operation_id_is_rejected_without_refiring() {
    let (addr, _g) = serve();
    let mut a = connect(addr, "alice", "pw-alice");
    setup(&mut a);
    exec(
        &mut a,
        "CREATE TRIGGER ts AFTER INSERT ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()",
    );
    let sql = "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-w';";
    assert_eq!(exec(&mut a, sql), "INSERT 0 1");
    for _ in 0..2 {
        send_simple_query(&mut a, sql);
        expect_error_response_with_sqlstate(&mut a, "23505");
        read_ready_for_query(&mut a);
    }
    send_simple_query(
        &mut a,
        "INSERT INTO s.tt (n) VALUES (2) USING OPERATION_ID 'op-w';",
    );
    expect_error_response_with_sqlstate(&mut a, "22023");
    read_ready_for_query(&mut a);
    assert_eq!(select_col(&mut a, "SELECT n FROM s.tt"), vec!["1"]);
    assert_eq!(
        select_col(&mut a, "SELECT lv FROM audit"),
        vec!["STATEMENT"]
    );
    let mut b = connect(addr, "bob", "pw-bob");
    create_tt(&mut b);
    assert_eq!(
        exec(
            &mut b,
            "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-w';"
        ),
        "INSERT 0 1"
    );
    assert_eq!(
        select_col(&mut a, "SELECT lv FROM audit"),
        vec!["STATEMENT"]
    );
    assert!(select_col(&mut b, "SELECT lv FROM audit").is_empty());
}
