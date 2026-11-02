//! `COPY ... FROM STDIN`／`COPY ... TO STDOUT` への利用者定義ポリシー適用（RLS-13・Issue #2280）の
//! pg wire 結合テスト。ポインタ: `docs/spec/04-behavior/rls.md` RLS-13・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-17／WIRE-22／WIRE-23。設計記録は
//! `docs/design/row-security-policy-composition.md` の「NoSQL・COPY への適用（#2280）」。
//!
//! COPY FROM は複数行 INSERT と同じ束縛済み実行器へ、COPY TO は SELECT の実行器へ委譲するので、
//! INSERT の `WITH CHECK` 違反は CopyDone 後の ErrorResponse `42501`（text・csv・binary 共通。1 行でも
//! 違反すれば 1 行も書かれない）、COPY TO は SELECT と同じ USING で行が絞られる。engine 側の確定
//! オラクルは `crates/engine/tests/table_rls/rls13_policy_{with_check,using}.rs`。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// 違反行にだけ入れる秘密マーカー。ErrorResponse に載ってはならない。
const SECRET: &str = "SECRETMARKER";

fn connect(label: &str) -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("owner", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    (
        authenticate_to_ready_for_query(addr, "alice", "pw-alice"),
        guard,
    )
}

fn exec(s: &mut TcpStream, sql: &str, tag: &str) {
    send_simple_query(s, sql);
    assert_eq!(read_command_complete(s), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(s), b'I');
}

/// ENABLE・FORCE を掛け、`policies` を作る。
fn enable_with(s: &mut TcpStream, policies: &[&str]) {
    exec(s, "ALTER TABLE t ENABLE ROW LEVEL SECURITY", "ALTER TABLE");
    exec(s, "ALTER TABLE t FORCE ROW LEVEL SECURITY", "ALTER TABLE");
    for p in policies {
        exec(s, p, "CREATE POLICY");
    }
}

const INS_A: &str = "CREATE POLICY ins ON t FOR INSERT WITH CHECK (owner = 'a')";
const SEL_ALL: &str = "CREATE POLICY sel ON t FOR SELECT USING (true)";
const SEL_A: &str = "CREATE POLICY sel ON t FOR SELECT USING (owner = 'a')";

/// 自テナントから SELECT で見える行数。
fn visible_rows(s: &mut TcpStream) -> usize {
    send_simple_query(s, "SELECT id FROM t LIMIT 100");
    let _ = read_row_description(s);
    let mut n = 0;
    loop {
        let (ty, _body) = read_message(s);
        match ty {
            b'D' => n += 1,
            b'C' => break,
            other => panic!("unexpected message {}", other as char),
        }
    }
    assert_eq!(read_ready_for_query_status(s), b'I');
    n
}

/// CopyInResponse を受け取り、`chunks` を CopyData で送り CopyDone する。
fn copy_in(s: &mut TcpStream, sql: &str, payload: &[u8]) {
    send_simple_query(s, sql);
    let (ty, body) = read_message(s);
    assert_eq!(
        ty,
        b'G',
        "expected CopyInResponse, got {}: {:?}",
        ty as char,
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(s, b'd', payload);
    send_length_prefixed_message(s, b'c', b"");
}

/// ErrorResponse を読み、SQLSTATE が `42501` で、秘密値・テナント ID を含まないことを確認する。
fn expect_42501_without_leak(s: &mut TcpStream) {
    let (ty, body) = read_message(s);
    assert_eq!(ty, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(text.contains("C42501"), "{text}");
    assert!(
        !text.contains(SECRET) && !text.contains("tenant-a"),
        "{text}"
    );
}

/// `COPY ... TO STDOUT` の出力行（CopyData ごと）を返す。
fn copy_out(s: &mut TcpStream, sql: &str) -> (Vec<String>, String) {
    send_simple_query(s, sql);
    let (ty, _) = read_message(s);
    assert_eq!(ty, b'H', "expected CopyOutResponse");
    let mut rows = Vec::new();
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'd' => rows.push(String::from_utf8_lossy(&body).into_owned()),
            b'c' => break,
            other => panic!("unexpected message {}", other as char),
        }
    }
    let tag = read_command_complete(s);
    assert_eq!(read_ready_for_query_status(s), b'I');
    (rows, tag)
}

#[test]
fn copy_from_text_violation_is_42501_and_writes_nothing() {
    let (mut s, _g) = connect("wire-policy-copy-text");
    enable_with(&mut s, &[INS_A, SEL_ALL]);

    copy_in(
        &mut s,
        "COPY t (id, owner, n) FROM STDIN USING OPERATION_ID 'pc-ok'",
        b"1\ta\t1\n2\ta\t2\n",
    );
    assert_eq!(read_command_complete(&mut s), "COPY 2");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(visible_rows(&mut s), 2);

    // 3 行目だけが違反。先行の行も書かれない。
    copy_in(
        &mut s,
        "COPY t (id, owner, n) FROM STDIN USING OPERATION_ID 'pc-bad'",
        format!("3\ta\t3\n4\t{SECRET}\t4\n").as_bytes(),
    );
    expect_42501_without_leak(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(visible_rows(&mut s), 2);

    // 同じ接続でその後の文が実行できる。
    exec(
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (5, 'a', 5) USING OPERATION_ID 'pc-after'",
        "INSERT 0 1",
    );
    assert_eq!(visible_rows(&mut s), 3);
}

#[test]
fn copy_from_csv_violation_is_42501() {
    let (mut s, _g) = connect("wire-policy-copy-csv");
    enable_with(&mut s, &[INS_A, SEL_ALL]);
    copy_in(
        &mut s,
        "COPY t (id, owner, n) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'pc-csv'",
        format!("1,a,1\n2,{SECRET},2\n").as_bytes(),
    );
    expect_42501_without_leak(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(visible_rows(&mut s), 0);
}

/// `id`（numeric）・`owner`（text）・`n`（int4）の binary タプルを組み立てる。
fn binary_stream(rows: &[(u16, &str, i32)]) -> Vec<u8> {
    let mut v = b"PGCOPY\n\xff\r\n\0".to_vec();
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    for (id, owner, n) in rows {
        v.extend_from_slice(&3i16.to_be_bytes());
        // numeric_send（ndigits=1・weight=0・sign=0・dscale=0・桁群 id）。
        let mut num = Vec::new();
        for w in [1i16, 0, 0, 0, *id as i16] {
            num.extend_from_slice(&w.to_be_bytes());
        }
        for f in [num, owner.as_bytes().to_vec(), n.to_be_bytes().to_vec()] {
            v.extend_from_slice(&(f.len() as i32).to_be_bytes());
            v.extend_from_slice(&f);
        }
    }
    v.extend_from_slice(&(-1i16).to_be_bytes());
    v
}

#[test]
fn copy_from_binary_violation_is_42501() {
    let (mut s, _g) = connect("wire-policy-copy-binary");
    enable_with(&mut s, &[INS_A, SEL_ALL]);
    let sql = "COPY t (id, owner, n) FROM STDIN WITH (FORMAT binary) USING OPERATION_ID";

    copy_in(
        &mut s,
        &format!("{sql} 'pc-bin-ok'"),
        &binary_stream(&[(1, "a", 1), (2, "a", 2)]),
    );
    assert_eq!(read_command_complete(&mut s), "COPY 2");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');

    copy_in(
        &mut s,
        &format!("{sql} 'pc-bin-bad'"),
        &binary_stream(&[(3, "a", 3), (4, SECRET, 4)]),
    );
    expect_42501_without_leak(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(visible_rows(&mut s), 2);
}

#[test]
fn copy_from_violation_in_transaction_fails_it_until_rollback() {
    let (mut s, _g) = connect("wire-policy-copy-txn");
    enable_with(&mut s, &[INS_A, SEL_ALL]);
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');

    copy_in(
        &mut s,
        "COPY t (id, owner, n) FROM STDIN USING OPERATION_ID 'pc-txn'",
        format!("1\t{SECRET}\t1\n").as_bytes(),
    );
    expect_42501_without_leak(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'E');

    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(visible_rows(&mut s), 0);
}

#[test]
fn copy_from_without_insert_policy_is_default_deny() {
    let (mut s, _g) = connect("wire-policy-copy-deny");
    enable_with(&mut s, &[SEL_ALL]);
    copy_in(
        &mut s,
        "COPY t (id, owner, n) FROM STDIN USING OPERATION_ID 'pc-deny'",
        b"1\ta\t1\n",
    );
    expect_42501_without_leak(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(visible_rows(&mut s), 0);
}

#[test]
fn copy_to_follows_the_select_policy() {
    let (mut s, _g) = connect("wire-policy-copy-to");
    // ポリシーを掛ける前に、見える行（owner = 'a'）と隠れる行を入れる。
    exec(
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1), (2, 'b', 2), (3, 'a', 3) \
         USING OPERATION_ID 'pc-seed'",
        "INSERT 0 3",
    );
    enable_with(&mut s, &[SEL_A]);

    let (table_rows, table_tag) = copy_out(&mut s, "COPY t (id) TO STDOUT");
    let (paren_rows, paren_tag) = copy_out(&mut s, "COPY (SELECT id FROM t LIMIT 100) TO STDOUT");
    let mut expected = vec!["1\n".to_string(), "3\n".to_string()];
    for rows in [table_rows, paren_rows] {
        let mut got = rows;
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
    }
    assert_eq!(table_tag, "COPY 2");
    assert_eq!(paren_tag, "COPY 2");
    // 同じ接続の SELECT と同じ行数。
    assert_eq!(visible_rows(&mut s), 2);
}

#[test]
fn copy_to_without_select_policy_returns_zero_rows() {
    let (mut s, _g) = connect("wire-policy-copy-to-none");
    exec(
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 'pc-seed2'",
        "INSERT 0 1",
    );
    // INSERT 用のポリシーだけ（SELECT に適用できる PERMISSIVE が無い）。
    enable_with(&mut s, &[INS_A]);
    let (rows, tag) = copy_out(&mut s, "COPY t (id) TO STDOUT");
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(tag, "COPY 0");
    let (rows, tag) = copy_out(&mut s, "COPY (SELECT id FROM t LIMIT 100) TO STDOUT");
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(tag, "COPY 0");
}
