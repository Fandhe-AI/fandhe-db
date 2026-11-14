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

fn setup(s: &mut TcpStream) {
    exec(s, "CREATE TABLE tt (n INTEGER)");
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
            "CREATE TRIGGER tb BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION bump()"
        ),
        "CREATE TRIGGER"
    );
    assert_eq!(
        exec(
            &mut s,
            "CREATE TRIGGER ts AFTER INSERT ON tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()"
        ),
        "CREATE TRIGGER"
    );
    assert_eq!(exec(&mut s, "INSERT INTO tt (n) VALUES (1);"), "INSERT 0 1");
    assert_eq!(exec(&mut s, "INSERT INTO tt (n) VALUES (2)"), "INSERT 0 1");
    let mut v = select_col(&mut s, "SELECT n FROM tt");
    v.sort();
    assert_eq!(v, vec!["101", "102"]);
    assert_eq!(
        select_col(&mut s, "SELECT lv FROM audit"),
        vec!["STATEMENT", "STATEMENT"]
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
        "CREATE TRIGGER tb BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("s1", "INSERT INTO tt (n) VALUES (5)", 0),
    );
    assert_eq!(read_message(&mut s).0, b'1');
    send_length_prefixed_message(&mut s, b'B', &bind_body("p1", "s1"));
    assert_eq!(read_message(&mut s).0, b'2');
    send_length_prefixed_message(&mut s, b'E', &execute_body("p1", 0));
    assert_eq!(read_message(&mut s).0, b'C');
    send_length_prefixed_message(&mut s, b'S', &[]);
    assert_eq!(read_message(&mut s).0, b'Z');
    assert_eq!(select_col(&mut s, "SELECT n FROM tt"), vec!["105"]);
}

/// 未対応の本体（本体の DELETE）は `0A000` の ErrorResponse と ReadyForQuery で返り、同じ接続で次の文が
/// 実行でき、本表・監査に残渣が無い。
///
/// tracked: #3187
/// 本体の `DELETE` は窓口を広げる #3187 までの暫定の拒否（UPDATE は dispatch 済み）。
#[test]
fn wire_unsupported_body_is_0a000_and_connection_survives() {
    let (addr, _g) = serve();
    let mut s = connect(addr, "alice", "pw-alice");
    setup(&mut s);
    exec(
        &mut s,
        "CREATE FUNCTION upd() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (lv) VALUES ('x'); DELETE FROM audit WHERE lv = 'x'; RETURN NEW; END $$",
    );
    exec(
        &mut s,
        "CREATE TRIGGER tb BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION upd()",
    );
    send_simple_query(&mut s, "INSERT INTO tt (n) VALUES (1);");
    expect_error_response_with_sqlstate(&mut s, "0A000");
    read_ready_for_query(&mut s);
    assert!(select_col(&mut s, "SELECT n FROM tt").is_empty());
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
        "CREATE TRIGGER tb BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    exec(
        &mut a,
        "CREATE TRIGGER ts AFTER INSERT ON tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()",
    );
    let mut b = connect(addr, "bob", "pw-bob");
    assert_eq!(exec(&mut b, "INSERT INTO tt (n) VALUES (1);"), "INSERT 0 1");
    assert_eq!(select_col(&mut b, "SELECT n FROM tt"), vec!["1"]);
    assert!(select_col(&mut b, "SELECT lv FROM audit").is_empty());
    assert!(select_col(&mut a, "SELECT lv FROM audit").is_empty());
}
