//! 利用者ポリシーの `USING` の適用（RLS-13・Issue #2276）の pg wire 結合テスト。簡易クエリ（'Q'）と
//! 拡張クエリ（Parse／Bind／Execute／Sync）の両方で、(1) 除かれた行が結果に現れない、
//! (2) 隠れた行だけを対象にした UPDATE／DELETE が `UPDATE 0`／`DELETE 0` の CommandComplete で成功する
//! （エラーにならない）、(3) ポリシーを反映できないランキング付き検索が `0A000` で拒否される、を固定する。
//! 合成規則の全行列は engine の `tests/table_rls/rls13_policy_using.rs` が独立オラクルで確定している。
//! ポインタ: `docs/spec/04-behavior/rls.md` の RLS-13。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// tenant-a の alice が接続する。行は tenant-a の Public で 3 件（id 1 ja・2 en・3 ja）。
fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-policy-using");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("tenant");
    for (id, lang) in [(1u64, "ja"), (2, "en"), (3, "ja")] {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("seed row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    let stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    (stream, guard)
}

fn simple_ok(s: &mut TcpStream, sql: &str) -> String {
    send_simple_query(s, sql);
    let tag = read_command_complete(s);
    assert_eq!(read_ready_for_query_status(s), b'I', "{sql}");
    tag
}

/// 簡易クエリで SELECT し、各行の最初の列（text）を返す。
fn simple_select_first_column(s: &mut TcpStream, sql: &str) -> Vec<String> {
    send_simple_query(s, sql);
    read_row_description(s);
    let mut rows = Vec::new();
    loop {
        let (kind, body) = read_message(s);
        match kind {
            b'D' => {
                let len = i32::from_be_bytes(body[2..6].try_into().expect("len"));
                assert!(len >= 0);
                rows.push(String::from_utf8(body[6..6 + len as usize].to_vec()).expect("utf8"));
            }
            b'C' => break,
            other => panic!("unexpected message {}", other as char),
        }
    }
    assert_eq!(read_ready_for_query_status(s), b'I');
    rows
}

fn parse_body(name: &str, sql: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b.extend_from_slice(sql.as_bytes());
    b.push(0);
    b.extend_from_slice(&0i16.to_be_bytes());
    b
}

fn bind_body() -> Vec<u8> {
    let mut b = vec![0u8, 0u8];
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&0i16.to_be_bytes());
    b
}

fn execute_body() -> Vec<u8> {
    let mut b = vec![0u8];
    b.extend_from_slice(&0i32.to_be_bytes());
    b
}

fn error_sqlstate(body: &[u8]) -> String {
    let mut pos = 0;
    while pos < body.len() && body[pos] != 0 {
        let code = body[pos];
        let end = body[pos + 1..].iter().position(|b| *b == 0).expect("nul") + pos + 1;
        if code == b'C' {
            return String::from_utf8(body[pos + 1..end].to_vec()).expect("utf8");
        }
        pos = end + 1;
    }
    panic!("no SQLSTATE field");
}

/// 拡張クエリで 1 文を実行し、(結果行数, CommandComplete タグ) を返す。エラーなら SQLSTATE を `Err`。
fn extended(s: &mut TcpStream, sql: &str) -> Result<(usize, String), String> {
    send_length_prefixed_message(s, b'P', &parse_body("", sql));
    send_length_prefixed_message(s, b'B', &bind_body());
    send_length_prefixed_message(s, b'E', &execute_body());
    send_length_prefixed_message(s, b'S', b"");
    let mut rows = 0usize;
    let mut tag = None;
    let mut err = None;
    loop {
        let (kind, body) = read_message(s);
        match kind {
            b'1' | b'2' | b'T' | b'n' => {}
            b'D' => rows += 1,
            b'C' => {
                tag = Some(String::from_utf8(body[..body.len() - 1].to_vec()).expect("utf8"));
            }
            b'E' => err = Some(error_sqlstate(&body)),
            b'Z' => break,
            other => panic!("unexpected message {}", other as char),
        }
    }
    match (err, tag) {
        (Some(code), _) => Err(code),
        (None, Some(tag)) => Ok((rows, tag)),
        (None, None) => panic!("no CommandComplete and no ErrorResponse"),
    }
}

#[test]
fn simple_query_applies_using_to_select_update_delete_and_rejects_ranked_search() {
    let (mut s, _g) = connect();
    assert_eq!(
        simple_ok(&mut s, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY"),
        "ALTER TABLE"
    );
    // ポリシーが 0 件なら既定拒否（結果 0 行）。
    assert!(simple_select_first_column(&mut s, "SELECT id FROM docs").is_empty());
    simple_ok(
        &mut s,
        "CREATE POLICY p ON docs FOR ALL USING (lang = 'ja')",
    );
    let mut ids = simple_select_first_column(&mut s, "SELECT id FROM docs");
    ids.sort();
    assert_eq!(ids, vec!["1", "3"]);
    // 隠れた行（id 2: en）だけを対象にした UPDATE／DELETE は 0 件で成功する（エラーにならない）。
    assert_eq!(
        simple_ok(
            &mut s,
            "UPDATE docs SET lang = 'x' WHERE id = 2 USING OPERATION_ID 'op-u0'"
        ),
        "UPDATE 0"
    );
    assert_eq!(
        simple_ok(
            &mut s,
            "DELETE FROM docs WHERE id = 2 USING OPERATION_ID 'op-d0'"
        ),
        "DELETE 0"
    );
    // 通る行は従来どおり 1 件。
    assert_eq!(
        simple_ok(
            &mut s,
            "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'op-d1'"
        ),
        "DELETE 1"
    );
    // ポリシーを反映できないランキング付き検索は 0A000。接続は維持される。
    send_simple_query(
        &mut s,
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 2",
    );
    expect_error_response_with_sqlstate(&mut s, "0A000");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_query_applies_using_to_select_update_delete_and_rejects_ranked_search() {
    let (mut s, _g) = connect();
    simple_ok(&mut s, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    simple_ok(
        &mut s,
        "CREATE POLICY p ON docs FOR ALL USING (lang = 'ja')",
    );
    let (rows, tag) = extended(&mut s, "SELECT id FROM docs").expect("select");
    assert_eq!(rows, 2);
    assert_eq!(tag, "SELECT 2");
    assert_eq!(
        extended(
            &mut s,
            "UPDATE docs SET lang = 'x' WHERE id = 2 USING OPERATION_ID 'op-eu0'"
        ),
        Ok((0, "UPDATE 0".to_string()))
    );
    assert_eq!(
        extended(
            &mut s,
            "DELETE FROM docs WHERE id = 2 USING OPERATION_ID 'op-ed0'"
        ),
        Ok((0, "DELETE 0".to_string()))
    );
    assert_eq!(
        extended(
            &mut s,
            "DELETE FROM docs WHERE id = 3 USING OPERATION_ID 'op-ed3'"
        ),
        Ok((0, "DELETE 1".to_string()))
    );
    assert_eq!(
        extended(
            &mut s,
            "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 2"
        ),
        Err("0A000".to_string())
    );
}
