//! `BEGIN` を含まない複数文メッセージの暗黙トランザクション（Issue #1175・WIRE-16・
//! SQL-31・RECOVER-12）を、生バイトの wire クライアント（`tests/common`）越しに検証する
//! 結合テスト。ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-16・
//! `docs/spec/05-tasks.md` TASK-219。
//!
//! 実行方式の選択（`statement_splitter::plan_multi_statement`）・分割規則の単体挙動は
//! `crates/engine/src/sql/statement_splitter.rs` の単体テストが固定する。本ファイルは
//! 原子的な commit・途中エラー時の全体ロールバック（行・`operation_id` 台帳・セッション
//! 状態）・`ReadyForQuery` の状態バイト・RLS の不変・区切りの密輸が起きないことを
//! wire 経由で確認する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// `docs` テーブル（`embedding VECTOR(3)` + `lang TEXT`）に 3 テナントの `Public` 行を
/// 1 件ずつ投入した `EngineCore`（`wire16_multi_statement.rs` と同型）。
fn new_core_three_tenant_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire16-implicit");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let corpus: [(&str, u64, [f32; 3], &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0, 0.0], "ja"),
        ("tenant-b", 2, [0.0, 1.0, 0.0], "en"),
        ("tenant-c", 3, [0.0, 0.0, 1.0], "ja"),
    ];
    for (tenant, id, emb, lang) in corpus {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_both(core: Arc<EngineCore>) -> (std::net::TcpStream, std::net::TcpStream) {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let alice = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let bob = authenticate_to_ready_for_query(addr, "bob", "pw-bob");
    (alice, bob)
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID '{op}'"
    )
}

/// `SELECT id FROM docs LIMIT 100` で、呼び出したセッションから見える id を昇順で返す。
fn visible_ids(stream: &mut std::net::TcpStream) -> Vec<String> {
    send_simple_query(stream, "SELECT id FROM docs LIMIT 100");
    let _columns = read_row_description(stream);
    let mut ids = Vec::new();
    loop {
        // `DataRow` か `CommandComplete` のどちらが来るかを先頭バイトで判別する。
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => {
                let len = i16::from_be_bytes([body[0], body[1]]);
                assert_eq!(len, 1);
                let flen = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
                ids.push(String::from_utf8(body[6..6 + flen].to_vec()).expect("utf8"));
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(stream);
    ids.sort();
    ids
}

/// (1) 複数の書き込みは 1 回の commit にまとまり、応答の後の `ReadyForQuery` は `'I'`。
#[test]
fn multiple_inserts_commit_atomically_and_report_idle() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!("{}; {}", insert_sql(61, "imp-1"), insert_sql(62, "imp-2")),
    );
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');

    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3", "61", "62"]);
}

/// (2)(5) 途中でエラーになると先行する書き込みも残らず、`operation_id` 台帳も
/// ロールバックされるため同じ `operation_id` を再利用できる。失敗後の接続は
/// `Failed`（`25P02`）にならず `'I'` のまま次のメッセージを処理できる。
#[test]
fn error_mid_message_rolls_back_everything_and_connection_stays_idle() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!(
            "{}; {}; SELECT id FROM no_such_table LIMIT 1",
            insert_sql(61, "imp-1"),
            insert_sql(62, "imp-2")
        ),
    );
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut alice, "42P01");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');

    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3"]);

    // 台帳もロールバックされているので、同じ operation_id で再実行できる。
    send_simple_query(
        &mut alice,
        &format!("{}; {}", insert_sql(61, "imp-1"), insert_sql(62, "imp-2")),
    );
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');
}

/// 暗黙トランザクション内で対応できない文（`UPDATE ... RETURNING`。Issue #1182・#1179 で
/// 明示トランザクション内の `UPDATE` 自体は対応済み）は `0A000`。先行する書き込みは
/// 残らず、接続は `'I'`。
#[test]
fn unsupported_statement_in_implicit_transaction_rolls_back_with_0a000() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!(
            "{}; UPDATE docs SET lang = 'en' WHERE id = 1 RETURNING id USING OPERATION_ID 'imp-u'",
            insert_sql(61, "imp-1")
        ),
    );
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut alice, "0A000");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');
    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3"]);
}

/// (3) 途中エラーの場合、メッセージ内の `SET search_mode` はメッセージ受信前の値へ
/// 復元される（`precision` のままなら低確信クエリで 0 行になる）。
#[test]
fn session_state_is_restored_when_the_implicit_transaction_fails() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!(
            "SET search_mode = 'precision'; {}; SELECT id FROM no_such_table LIMIT 1",
            insert_sql(61, "imp-1")
        ),
    );
    assert_eq!(read_command_complete(&mut alice), "SET");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut alice, "42P01");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');

    send_simple_query(
        &mut alice,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
    );
    let _columns = read_row_description(&mut alice);
    for _ in 0..3 {
        let _row = read_data_row(&mut alice);
    }
    assert_eq!(read_command_complete(&mut alice), "SELECT 3");
    read_ready_for_query(&mut alice);
}

/// 成功した場合は `SET search_mode` が次のメッセージへ持ち越される。
#[test]
fn session_state_persists_when_the_implicit_transaction_commits() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!(
            "SET search_mode = 'precision'; {}; {}",
            insert_sql(61, "imp-1"),
            insert_sql(62, "imp-2")
        ),
    );
    assert_eq!(read_command_complete(&mut alice), "SET");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');

    // `precision` が持ち越されていれば、明確な top1 の無いクエリは 0 行になる。
    send_simple_query(
        &mut alice,
        "SELECT id FROM docs ORDER BY embedding <=> '[0.5,0.5,0.5]' LIMIT 3",
    );
    let _columns = read_row_description(&mut alice);
    let tag = read_command_complete(&mut alice);
    assert_eq!(tag, "SELECT 0");
    read_ready_for_query(&mut alice);
}

/// (4) 暗黙トランザクション内での `operation_id` 再利用は `25000`。全体がロールバックされる。
#[test]
fn reusing_operation_id_within_the_message_is_25000_and_rolls_back() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!(
            "{}; {}",
            insert_sql(61, "imp-dup"),
            insert_sql(62, "imp-dup")
        ),
    );
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut alice, "25000");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');
    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3"]);
}

/// (6) RLS: alice の暗黙トランザクションの書き込み（`Private`）は commit 後も bob から
/// 見えない。エラー応答はテナントによらず同一（他テナントの存在を漏らさない）。
#[test]
fn implicit_transaction_respects_tenant_boundaries() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, mut bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!("{}; {}", insert_sql(61, "imp-a1"), insert_sql(62, "imp-a2")),
    );
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');

    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3", "61", "62"]);
    assert_eq!(visible_ids(&mut bob), vec!["1", "2", "3"]);

    // 同じ形のメッセージが失敗した場合、両テナントは同一の ErrorResponse を受け取る。
    // 対応外の文（`UPDATE ... RETURNING`）で失敗する形を使う（トランザクション内の SELECT は
    // Issue #1179 で自トランザクションの未 commit 変更を読めるようになったため失敗しない）。
    let failing = |op: &str| {
        format!(
            "{}; UPDATE docs SET lang = 'en' WHERE id = 1 RETURNING id USING OPERATION_ID 'x-{op}'",
            insert_sql(70, op)
        )
    };
    let mut responses = Vec::new();
    for (stream, op) in [(&mut alice, "imp-f-a"), (&mut bob, "imp-f-b")] {
        send_simple_query(stream, &failing(op));
        assert_eq!(read_command_complete(stream), "INSERT 0 1");
        let (kind, body) = read_message(stream);
        assert_eq!(kind, b'E');
        responses.push(body);
        assert_eq!(read_ready_for_query_status(stream), b'I');
    }
    assert_eq!(
        responses[0], responses[1],
        "error responses must not depend on the tenant"
    );
}

/// (7) 入れ子ブロックコメントに隠した区切りで文を密輸できない。コメントを含む断片は
/// 単一文のときと同じ `42601` で拒否され、隠した `INSERT` は実行されない。
#[test]
fn nested_block_comment_cannot_smuggle_a_statement() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!(
            "SELECT id FROM docs LIMIT 1; /* a /* b */ ; {} */ ; SELECT id FROM docs LIMIT 1",
            insert_sql(99, "smuggled")
        ),
    );
    let _columns = read_row_description(&mut alice);
    let _row = read_data_row(&mut alice);
    assert_eq!(read_command_complete(&mut alice), "SELECT 1");
    expect_error_response_with_sqlstate(&mut alice, "42601");
    read_ready_for_query(&mut alice);

    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3"]);
}

/// 未終端のブロックコメントに続く文は分割されず、全文が `42601` で拒否される
/// （1 文も実行されない）。
#[test]
fn unterminated_block_comment_rejects_the_whole_message() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(
        &mut alice,
        &format!(
            "{}; /* never closed; {}",
            insert_sql(98, "unterm-1"),
            insert_sql(97, "unterm-2")
        ),
    );
    expect_error_response_with_sqlstate(&mut alice, "42601");
    read_ready_for_query(&mut alice);
    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3"]);
}

/// 明示トランザクションが `Failed` の間は、書き込みを含む複数文メッセージも暗黙
/// トランザクションにはならず、先頭の文が `25P02` で拒否される（状態は `'E'` のまま）。
#[test]
fn failed_explicit_transaction_is_not_replaced_by_an_implicit_one() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(&mut alice, "BEGIN");
    assert_eq!(read_command_complete(&mut alice), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut alice), b'T');
    send_simple_query(&mut alice, "SELECT nonsense FROM");
    expect_error_response_with_sqlstate(&mut alice, "42601");
    assert_eq!(read_ready_for_query_status(&mut alice), b'E');

    send_simple_query(
        &mut alice,
        &format!("{}; {}", insert_sql(61, "imp-1"), insert_sql(62, "imp-2")),
    );
    expect_error_response_with_sqlstate(&mut alice, "25P02");
    assert_eq!(read_ready_for_query_status(&mut alice), b'E');

    send_simple_query(&mut alice, "ROLLBACK");
    assert_eq!(read_command_complete(&mut alice), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');
    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3"]);
}

/// 明示トランザクション中の複数書き込みメッセージは従来どおり順次実行され、
/// `'T'` のまま `COMMIT` で確定する（暗黙トランザクションで包み直さない）。
#[test]
fn active_explicit_transaction_keeps_sequential_execution() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut alice, _bob) = spawn_both(core);

    send_simple_query(&mut alice, "BEGIN");
    assert_eq!(read_command_complete(&mut alice), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut alice), b'T');

    send_simple_query(
        &mut alice,
        &format!("{}; {}", insert_sql(61, "imp-1"), insert_sql(62, "imp-2")),
    );
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut alice), b'T');

    send_simple_query(&mut alice, "COMMIT");
    assert_eq!(read_command_complete(&mut alice), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');
    assert_eq!(visible_ids(&mut alice), vec!["1", "2", "3", "61", "62"]);
}
