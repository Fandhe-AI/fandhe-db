//! `RAISE` の通知と例外（SQL-70・Issue #2227）の wire 結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-70・ERR-5。`NoticeResponse`（'N'）が応答の
//! 最初のフレームより前に届くこと、`DEBUG`／`LOG` は送らないこと、`RAISE EXCEPTION` は利用者指定の
//! SQLSTATE を `C` に出して `W`（呼び出し文脈）を付けること、本 DB 自身のエラー（ネイティブの
//! `23505`）には `W` を付けず 3 フィールドのままであること（利用者の `23505` と区別できる）を固定する。
//! 簡易・拡張の両プロトコルを確かめる。

use crate::common;
use crate::temp_db;

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql70-raise-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
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

/// ErrorResponse／NoticeResponse の本体を、フィールドタグ→値の表へ分解する。
fn fields(body: &[u8]) -> BTreeMap<u8, String> {
    let mut out = BTreeMap::new();
    let mut i = 0usize;
    while let Some(&tag) = body.get(i) {
        if tag == 0 {
            break;
        }
        let rest = &body[i + 1..];
        let end = rest.iter().position(|b| *b == 0).expect("NUL terminated");
        out.insert(
            tag,
            String::from_utf8(rest[..end].to_vec()).expect("utf-8 field"),
        );
        i += 1 + end + 1;
    }
    out
}

const DO_NOTICES: &str = "DO $$ BEGIN \
    RAISE NOTICE 'hello %', 1; \
    RAISE WARNING 'careful' USING DETAIL = 'dd', HINT = 'hh'; \
    RAISE DEBUG 'd'; \
    RAISE LOG 'l'; \
    END $$";

#[test]
fn simple_query_sends_notices_before_command_complete_and_drops_debug_log() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    send_simple_query(&mut s, DO_NOTICES);

    let (t, body) = read_message(&mut s);
    assert_eq!(t, b'N');
    let f = fields(&body);
    assert_eq!(f[&b'S'], "NOTICE");
    assert_eq!(f[&b'V'], "NOTICE");
    assert_eq!(f[&b'C'], "00000");
    assert_eq!(f[&b'M'], "hello 1");

    let (t, body) = read_message(&mut s);
    assert_eq!(t, b'N');
    let f = fields(&body);
    assert_eq!(f[&b'S'], "WARNING");
    assert_eq!(f[&b'C'], "01000");
    assert_eq!(f[&b'D'], "dd");
    assert_eq!(f[&b'H'], "hh");

    // DEBUG／LOG は送らない。続くのは CommandComplete と ReadyForQuery。
    assert_eq!(read_command_complete(&mut s), "DO");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn raise_exception_returns_user_sqlstate_with_where_after_notices() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    send_simple_query(
        &mut s,
        "DO $$ BEGIN \
           RAISE NOTICE 'before'; \
           RAISE EXCEPTION 'dup' USING ERRCODE = '23505', DETAIL = 'dd', HINT = 'hh'; \
         END $$",
    );
    let (t, body) = read_message(&mut s);
    assert_eq!(t, b'N');
    assert_eq!(fields(&body)[&b'M'], "before");

    let (t, body) = read_message(&mut s);
    assert_eq!(t, b'E');
    let f = fields(&body);
    assert_eq!(f[&b'S'], "ERROR");
    assert_eq!(f[&b'C'], "23505");
    assert_eq!(f[&b'M'], "dup");
    assert_eq!(f[&b'D'], "dd");
    assert_eq!(f[&b'H'], "hh");
    let w = &f[&b'W'];
    assert!(
        w.contains("inline_code_block") && w.contains("at RAISE"),
        "{w}"
    );
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn native_unique_violation_has_no_where_field() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    for (id, op) in [(1, "op-1"), (1, "op-2")] {
        send_simple_query(
            &mut s,
            &format!(
                "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'r') \
                 USING OPERATION_ID '{op}'"
            ),
        );
        if op == "op-1" {
            assert_eq!(read_command_complete(&mut s), "INSERT 0 1");
            assert_eq!(read_ready_for_query_status(&mut s), b'I');
        }
    }
    let (t, body) = read_message(&mut s);
    assert_eq!(t, b'E');
    let f = fields(&body);
    assert_eq!(f[&b'C'], "23505");
    // 本 DB 自身の応答は S/C/M の 3 フィールドのまま（ERR-5）。利用者の 23505 と区別できる。
    assert!(!f.contains_key(&b'W'), "{f:?}");
    assert_eq!(f.len(), 3, "{f:?}");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn caught_exception_still_sends_notices_and_completes() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    send_simple_query(
        &mut s,
        "DO $$ BEGIN \
           BEGIN \
             RAISE EXCEPTION 'x'; \
           EXCEPTION WHEN OTHERS THEN \
             RAISE NOTICE 'caught %', sqlerrm; \
           END; \
         END $$",
    );
    let (t, body) = read_message(&mut s);
    assert_eq!(t, b'N');
    assert_eq!(fields(&body)[&b'M'], "caught x");
    assert_eq!(read_command_complete(&mut s), "DO");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_protocol_sends_notices_and_user_error_fields() {
    let (core, _g) = new_core();
    let mut s = connect(core);

    send_length_prefixed_message(&mut s, b'P', &parse_body("", DO_NOTICES, 0));
    send_length_prefixed_message(&mut s, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);
    let mut kinds = Vec::new();
    loop {
        let (t, _) = read_message(&mut s);
        kinds.push(t);
        if t == b'Z' {
            break;
        }
    }
    // ParseComplete・BindComplete・NoticeResponse×2・CommandComplete・ReadyForQuery。
    assert_eq!(kinds, vec![b'1', b'2', b'N', b'N', b'C', b'Z']);

    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body(
            "",
            "DO $$ BEGIN RAISE EXCEPTION 'ext' USING ERRCODE = '23505'; END $$",
            0,
        ),
    );
    send_length_prefixed_message(&mut s, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);
    let mut err_fields = None;
    loop {
        let (t, body) = read_message(&mut s);
        if t == b'E' {
            err_fields = Some(fields(&body));
        }
        if t == b'Z' {
            break;
        }
    }
    let f = err_fields.expect("ErrorResponse");
    assert_eq!(f[&b'C'], "23505");
    assert_eq!(f[&b'M'], "ext");
    assert!(f[&b'W'].contains("at RAISE"));
}
