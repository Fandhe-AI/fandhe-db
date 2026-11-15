//! ビューへの INSTEAD OF ROW トリガ（TABLE-45・TABLE-43・Issue #3193）の wire 越し検証（層 A）。
//!
//! 書き込みと行意味論の確定オラクルは `crates/engine/src/core/trigger_instead_of_*tests.rs` で、
//! 本ファイルは pg wire 経由で次を確かめる。
//! (1) 簡易クエリ（psql が末尾 `;` 付きで単独送信する形を含む）の `INSERT`／`UPDATE`／`DELETE` が
//!     トリガ付きビューへ通り、`CommandComplete` が `INSERT 0 n`／`UPDATE n`／`DELETE n` になること
//! (2) 拡張クエリ（Parse／Bind／Execute）でも同じ応答になり、自動更新不可ビューの Parse 段が
//!     拒否で止まらないこと
//! (3) 未対応の形（`RETURNING`）が `0A000` の ErrorResponse になり、トリガの無い別テナントの同名ビューは
//!     従来どおり `55000` で、どちらの応答にもテナント・内部キーが含まれないこと

use crate::common;
use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-view-instead-of");
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

/// 管理用セッションで DDL を実行する（wire 経由の DDL 権限とは独立）。
fn ddl_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    let ctx = PolicyContext::new(tenant).expect("valid tenant id");
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// tenant-a にだけ INSTEAD OF トリガ付きの非更新ビュー `s.nv` を、tenant-b にはトリガの無い同名ビューを作る。
fn prepare(core: &EngineCore) {
    for tenant in ["tenant-a", "tenant-b"] {
        ddl_as(core, tenant, "CREATE SCHEMA s");
        ddl_as(core, tenant, "CREATE TABLE s.tt (n INTEGER)");
        ddl_as(
            core,
            tenant,
            "CREATE VIEW s.nv AS SELECT n FROM s.tt LIMIT 1000",
        );
    }
    ddl_as(core, "tenant-a", "CREATE TABLE audit (tag TEXT, n INTEGER)");
    for (name, body) in [
        (
            "f_ins",
            "BEGIN INSERT INTO audit (tag, n) VALUES ('ins', NEW.n); RETURN NEW; END",
        ),
        (
            "f_upd",
            "BEGIN INSERT INTO audit (tag, n) VALUES ('upd', NEW.n); RETURN NEW; END",
        ),
        (
            "f_del",
            "BEGIN INSERT INTO audit (tag, n) VALUES ('del', OLD.n); RETURN OLD; END",
        ),
    ] {
        ddl_as(
            core,
            "tenant-a",
            &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
        );
    }
    for (trg, ev, f) in [
        ("g_ins", "INSERT", "f_ins"),
        ("g_upd", "UPDATE", "f_upd"),
        ("g_del", "DELETE", "f_del"),
    ] {
        ddl_as(
            core,
            "tenant-a",
            &format!(
                "CREATE TRIGGER {trg} INSTEAD OF {ev} ON s.nv FOR EACH ROW EXECUTE FUNCTION {f}()"
            ),
        );
    }
}

fn connect_both(core: Arc<EngineCore>) -> (TcpStream, TcpStream) {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let alice = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let bob = authenticate_to_ready_for_query(addr, "bob", "pw-bob");
    (alice, bob)
}

/// 書き込み文を簡易クエリで流し、`CommandComplete` のタグを返す。
fn exec(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

/// 拒否される文を流し、ErrorResponse 本体を文字列で返す（続く ReadyForQuery も読む）。
fn exec_error(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse for {sql}");
    read_ready_for_query(stream);
    String::from_utf8_lossy(&body).to_string()
}

/// 監査表の `tag` ごとの値（昇順）を読む。
fn audit(stream: &mut TcpStream, tag: &str) -> Vec<String> {
    send_simple_query(
        stream,
        &format!("SELECT n FROM audit WHERE tag = '{tag}' LIMIT 100"),
    );
    let _ = read_row_description(stream);
    let mut out = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => {
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
                out.push(String::from_utf8_lossy(&body[6..6 + len]).to_string());
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(stream);
    out.sort();
    out
}

/// 簡易クエリ: 末尾 `;` 付きの単独送信（psql の形）でも INSERT／UPDATE／DELETE がトリガへ振り替わり、
/// 影響行数が `CommandComplete` に載る。
#[test]
fn simple_query_view_writes_reach_instead_of_triggers() {
    let (core, _g) = new_core();
    prepare(&core);
    let (mut alice, _bob) = connect_both(core);

    assert_eq!(
        exec(&mut alice, "INSERT INTO s.nv (n) VALUES (1), (2);"),
        "INSERT 0 2"
    );
    assert_eq!(audit(&mut alice, "ins"), vec!["1", "2"]);

    assert_eq!(
        exec(&mut alice, "INSERT INTO s.tt (n) VALUES (10);"),
        "INSERT 0 1"
    );
    assert_eq!(
        exec(&mut alice, "UPDATE s.nv SET n = n + 1 WHERE n = 10;"),
        "UPDATE 1"
    );
    assert_eq!(audit(&mut alice, "upd"), vec!["11"]);

    assert_eq!(
        exec(&mut alice, "DELETE FROM s.nv WHERE n = 10;"),
        "DELETE 1"
    );
    assert_eq!(audit(&mut alice, "del"), vec!["10"]);
}

/// 拡張クエリ: 自動更新不可ビューへの INSERT が Parse 段で拒否されず、Execute で `INSERT 0 1` になる。
#[test]
fn extended_protocol_view_insert_reaches_instead_of_trigger() {
    let (core, _g) = new_core();
    prepare(&core);
    let (mut alice, _bob) = connect_both(core);

    let sql = "INSERT INTO s.nv (n) VALUES (5)";
    send_length_prefixed_message(&mut alice, b'P', &parse_body("s1", sql, 0));
    let (kind, _) = read_message(&mut alice);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(&mut alice, b'B', &bind_body("p1", "s1"));
    let (kind, _) = read_message(&mut alice);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut alice, b'E', &execute_body("p1", 0));
    let (kind, tag) = read_message(&mut alice);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "INSERT 0 1");
    send_length_prefixed_message(&mut alice, b'S', b"");
    let (kind, _) = read_message(&mut alice);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
    assert_eq!(audit(&mut alice, "ins"), vec!["5"]);
}

/// 未対応の形は `0A000`、トリガの無い別テナントの同名ビューは従来どおり `55000`。どちらの応答にも
/// テナント・内部キーを含めない。
#[test]
fn unsupported_forms_and_untriggered_views_do_not_leak() {
    let (core, _g) = new_core();
    prepare(&core);
    let (mut alice, mut bob) = connect_both(core);

    let returning = exec_error(&mut alice, "INSERT INTO s.nv (n) VALUES (1) RETURNING n");
    assert!(returning.contains("0A000"), "{returning}");
    let untriggered = exec_error(&mut bob, "INSERT INTO s.nv (n) VALUES (1)");
    assert!(untriggered.contains("55000"), "{untriggered}");
    for text in [&returning, &untriggered] {
        for needle in ["tenant-a", "tenant-b", "alice", "bob", "#"] {
            assert!(!text.contains(needle), "leaks {needle}: {text}");
        }
    }
    // alice の監査表には何も載っていない（0A000 は書き込み前に拒否）。
    assert!(audit(&mut alice, "ins").is_empty());
}

/// Bind 用の本体（text 形式の 1 パラメータ・結果形式なし）。
fn bind_one_text(portal: &str, statement: &str, value: &str) -> Vec<u8> {
    let mut bind = Vec::new();
    bind.extend_from_slice(format!("{portal}\0{statement}\0").as_bytes());
    bind.extend_from_slice(&0i16.to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&i32::try_from(value.len()).expect("len").to_be_bytes());
    bind.extend_from_slice(value.as_bytes());
    bind.extend_from_slice(&0i16.to_be_bytes());
    bind
}

/// 1 つの準備済み文を、別々の値で 2 回 Bind／Execute する。対象行（OLD）は Bind した値に従い、文の
/// 述語の写しが古いままになっていない。Describe も拒否されず、列メタ無し（NoData）になる。
fn execute_prepared_twice(
    stream: &mut TcpStream,
    name: &str,
    sql: &str,
    values: [&str; 2],
    tags: [&str; 2],
) {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "expected ParseComplete for {sql}");
    send_length_prefixed_message(stream, b'D', format!("S{name}\0").as_bytes());
    // ParameterDescription に続く NoData（INSTEAD OF 経路は RETURNING なしだけ受理する）。
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b't', "expected ParameterDescription for {sql}");
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'n', "expected NoData for {sql}");
    for (i, (value, tag)) in values.iter().zip(tags).enumerate() {
        // 実行済みのポータルは再利用せず、値ごとに別名のポータルへ Bind する。
        let portal = format!("{name}-p{i}");
        send_length_prefixed_message(stream, b'B', &bind_one_text(&portal, name, value));
        let (kind, _) = read_message(stream);
        assert_eq!(kind, b'2', "expected BindComplete for {sql} with {value}");
        send_length_prefixed_message(stream, b'E', &execute_body(&portal, 0));
        let (kind, body) = read_message(stream);
        assert_eq!(
            kind, b'C',
            "expected CommandComplete for {sql} with {value}"
        );
        assert_eq!(String::from_utf8_lossy(&body[..body.len() - 1]), tag);
    }
    send_length_prefixed_message(stream, b'S', b"");
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
}

/// 拡張クエリの UPDATE／DELETE: 同じ準備済み文を別のパラメータで繰り返し実行しても、トリガへ渡る
/// OLD 行は Bind した値に従う。
#[test]
fn extended_protocol_update_and_delete_follow_the_bound_parameter() {
    let (core, _g) = new_core();
    prepare(&core);
    let (mut alice, _bob) = connect_both(core);
    assert_eq!(
        exec(&mut alice, "INSERT INTO s.tt (n) VALUES (10), (20), (30);"),
        "INSERT 0 3"
    );

    execute_prepared_twice(
        &mut alice,
        "d1",
        "DELETE FROM s.nv WHERE n = $1",
        ["10", "30"],
        ["DELETE 1", "DELETE 1"],
    );
    assert_eq!(audit(&mut alice, "del"), vec!["10", "30"]);

    execute_prepared_twice(
        &mut alice,
        "u1",
        "UPDATE s.nv SET n = n + 1 WHERE n = $1",
        ["20", "999"],
        ["UPDATE 1", "UPDATE 0"],
    );
    assert_eq!(audit(&mut alice, "upd"), vec!["21"]);
}
