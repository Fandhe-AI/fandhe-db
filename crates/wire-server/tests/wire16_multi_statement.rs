//! 簡易クエリプロトコル 1 メッセージに含まれるセミコロン区切りの複数 SQL 文実行
//! （WIRE-16・TASK-219・Issue #938）を、生バイトの wire クライアント
//! （`tests/common`）越しに検証する結合テスト。
//!
//! 分割規則・文種別分類・「書き込みは最後の 1 文のみ」の制約自体は
//! `crates/engine/src/sql/statement_splitter.rs` の単体テストが固定する。
//! 本ファイルは wire フレーミング越しの応答順序（`RowDescription`/`DataRow`*/
//! `CommandComplete` を各文ごとに送出し `ReadyForQuery` は最後の文にのみ付く）・
//! エラー時の打ち切り・セッション状態の巻き戻し・RLS 不変・単一文の既存挙動が
//! wire 経由で崩れていないことを確認する。

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

/// `docs` テーブル（`embedding VECTOR(3)` + `lang TEXT`）を持つ `EngineCore` を
/// 新設し、3 テナント（alice/bob/carol）それぞれの `Public` 行を 1 件ずつ投入する
/// （`wire1_simple_query.rs` と同型の小規模コーパス）。
fn new_core_three_tenant_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire16-docs");
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

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn spawn_with_bob(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("bob", "tenant-b", "pw-bob")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "bob", "pw-bob")
}

/// 2 文の `SELECT` が連続する場合、`RowDescription`/`DataRow`/`CommandComplete`
/// の組が 2 回届いた後、`ReadyForQuery` はちょうど 1 回だけ届く。
#[test]
fn two_select_statements_share_a_single_ready_for_query() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 1; SELECT lang FROM docs LIMIT 1",
    );

    let columns1 = read_row_description(&mut stream);
    assert_eq!(columns1, vec!["id"]);
    let _row1 = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");

    let columns2 = read_row_description(&mut stream);
    assert_eq!(columns2, vec!["lang"]);
    let _row2 = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");

    read_ready_for_query(&mut stream);
}

/// 文字列リテラル内の `;`（`WHERE` の等価条件・`INSERT` の `VALUES`／
/// `USING OPERATION_ID` 値）は分割点として扱われず、各文はそのまま実行される
/// （書き込み文〔`INSERT`〕を最後に置き「書き込みは最後の 1 文のみ」の制約を
/// 満たす形にする）。
#[test]
fn semicolon_inside_string_literals_is_not_a_split_point() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT lang FROM docs WHERE lang = 'a;b' LIMIT 1; \
         INSERT INTO docs (id, embedding, lang) VALUES (10, '[0.1,0.2,0.3]', 'a;b') \
         USING OPERATION_ID 'op;with;semicolons'",
    );

    // 1 文目の時点では `lang = 'a;b'` に一致する行はまだ存在しない（0 行）。
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["lang"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");

    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");

    read_ready_for_query(&mut stream);

    // 挿入された行を単一文の `SELECT` で読み戻し、リテラル内の `;` を含む
    // `lang` 値・`operation_id` 値がいずれも正しく保存されていることを確認する。
    send_simple_query(&mut stream, "SELECT lang FROM docs WHERE id = 10 LIMIT 1");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("a;b"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 空文（`;;`・先頭 `;`・末尾の余剰 `;`）は無視される。`;` のみ・`; ;` のみの
/// メッセージは非空文が 0 個なので `EmptyQueryResponse` を返す。
#[test]
fn empty_statements_are_ignored_and_all_empty_message_yields_empty_query_response() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    // `SELECT 1;;` は要素 1 個の複数文経路（Statements）を通るが、実質 1 文と
    // 同じ結果になる。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 1;;");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 先頭 `;` を除去したうえで実行される。
    send_simple_query(&mut stream, ";SELECT id FROM docs LIMIT 1");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 末尾の余剰 `;` も無視される。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 1; ;");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 非空文が 0 個 → EmptyQueryResponse。
    send_simple_query(&mut stream, ";");
    expect_empty_query_response(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "; ;");
    expect_empty_query_response(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 非空文 16 個は受理し、17 個目（末尾が `INSERT`）は `54000` で 1 文も
/// 実行しない。台帳・行数のいずれにも副作用が残らないことを、同一
/// `operation_id` を単一文で再利用して成功することで確認する
/// （台帳に記録されていれば `23505` になるはず）。
#[test]
fn seventeen_statements_are_rejected_with_54000_and_no_partial_execution() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    let sixteen = "SELECT id FROM docs LIMIT 1;".repeat(16);
    send_simple_query(&mut stream, &sixteen);
    for _ in 0..16 {
        let _columns = read_row_description(&mut stream);
        let _row = read_data_row(&mut stream);
        assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    }
    read_ready_for_query(&mut stream);

    let seventeen = format!(
        "{}INSERT INTO docs (id, embedding, lang) VALUES (20, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'too-many-op'",
        "SELECT id FROM docs LIMIT 1;".repeat(16)
    );
    send_simple_query(&mut stream, &seventeen);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);

    // 行は増えておらず、`operation_id` も台帳未使用のまま（単一文で同じ
    // `operation_id` を使うと成功する）。
    send_simple_query(&mut stream, "SELECT id FROM docs WHERE id = 20 LIMIT 1");
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (20, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'too-many-op'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// 途中の文がエラーになった場合、先行文の応答の後に `ErrorResponse`＋
/// `ReadyForQuery` が 1 回だけ届き、後続の文（`INSERT`）は実行されない
/// （行は増えない・`operation_id` は台帳未使用のまま）。
#[test]
fn error_in_middle_statement_stops_remaining_statements() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 1; \
         SELECT id FROM no_such_table LIMIT 1; \
         INSERT INTO docs (id, embedding, lang) VALUES (30, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'never-runs'",
    );

    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");

    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs WHERE id = 30 LIMIT 1");
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (30, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'never-runs'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// 読み取り→書き込みの順（書き込みが最後の 1 文）は受理される。
#[test]
fn read_then_write_is_accepted() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 1; \
         INSERT INTO docs (id, embedding, lang) VALUES (40, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'read-then-write'",
    );

    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// 書き込みが最後以外にある組み合わせは暗黙トランザクションで原子的に実行される
/// （Issue #1175）。書き込みだけで完結する形は commit され、直後の読み取りは自トランザクションの
/// 未 commit 変更を反映する（Issue #1179）。暗黙トランザクション内で対応できない文
/// （`DROP TABLE`。`UPDATE ... RETURNING` は Issue #1272 で対応済み）は `0A000` でメッセージ全体をロールバックする。
/// いずれの場合も `ReadyForQuery` は `'I'`。
#[test]
fn write_not_last_runs_in_an_implicit_transaction_and_rolls_back_on_unsupported_statements() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    // (1) INSERT; INSERT は両方 commit される。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (51, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'wl-2'; \
         INSERT INTO docs (id, embedding, lang) VALUES (52, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'wl-3'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // (2) INSERT; 同じテーブルの SELECT は自トランザクションの未 commit 行を読め、
    //     メッセージ全体が commit される。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (50, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'wl-1'; SELECT id FROM docs WHERE id = 50 LIMIT 1",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream)[0].as_deref(), Some("50"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // (3) INSERT; DROP TABLE は暗黙トランザクション内では未対応。INSERT の応答の後に
    //     0A000 となり、INSERT はロールバックされる。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (53, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'wl-5'; \
         DROP TABLE docs",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // commit されたのは (1)(2) の 3 行だけ。(3) の副作用は残っていない
    // （id=53 なし・元の lang のまま）。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _columns = read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..6 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    ids.sort();
    assert_eq!(ids, vec!["1", "2", "3", "50", "51", "52"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 6");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT lang FROM docs WHERE id = 1 LIMIT 1");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("ja"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 複数文メッセージ内でエラーが発生した場合、セッション局所の変更
/// （`SET search_mode`）はメッセージ受信前の値へ巻き戻る。次のメッセージの
/// 句なし `SELECT` が `recall`（既定。3 行）のままであることで確認する
/// （`precision` に切り替わっていれば低確信クエリで 0 行になる）。
#[test]
fn set_search_mode_is_rolled_back_when_a_later_statement_in_the_message_fails() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    // `[1,0,0]` に対し id1=1.0・id2=0.0・id3=0.0 で top1 のみ明確
    // （`precision` 既定閾値 top1≥0.80・margin≥0.05 を満たす）。
    send_simple_query(
        &mut stream,
        "SET search_mode = 'precision'; SELECT id FROM no_such_table LIMIT 1",
    );
    assert_eq!(read_command_complete(&mut stream), "SET");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);

    // 巻き戻り済みなら既定（recall）のまま：3 行返る。
    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
    );
    let _columns = read_row_description(&mut stream);
    let mut count = 0;
    for _ in 0..3 {
        let _row = read_data_row(&mut stream);
        count += 1;
    }
    assert_eq!(
        count, 3,
        "SET search_mode must have been rolled back to the default (recall)"
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// 複数文メッセージが最後まで成功した場合、`SET search_mode` は巻き戻らず
/// 次のメッセージへ持ち越される（PostgreSQL の暗黙トランザクションが commit
/// された場合と同じ意味論）。
#[test]
fn set_search_mode_persists_when_the_whole_message_succeeds() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SET search_mode = 'precision'; SELECT id FROM docs LIMIT 1",
    );
    assert_eq!(read_command_complete(&mut stream), "SET");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 次のメッセージの句なし `SELECT` が `precision` のまま（top1 のみ明確な
    // クエリで 1 行のみ返る）であることで、成功時は持ち越されることを確認する。
    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(
        read_command_complete(&mut stream),
        "SELECT 1",
        "SET search_mode = 'precision' must persist across messages on success"
    );
    read_ready_for_query(&mut stream);
}

/// `CREATE FUNCTION` を含む複数文メッセージが途中で失敗した場合も同様に
/// 巻き戻り、次のメッセージでその関数は未定義のまま（未定義関数呼び出しは
/// `sql::udf_call` の束縛時検証により `42883`〔`UndefinedFunction`〕になる）。
#[test]
fn create_function_is_rolled_back_when_a_later_statement_in_the_message_fails() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "CREATE FUNCTION double_it(v) AS v * 2.0; SELECT id FROM no_such_table LIMIT 1",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE FUNCTION");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, double_it(2.0) AS doubled FROM docs LIMIT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "42883");
    read_ready_for_query(&mut stream);
}

/// RLS: bob（tenant-b）が複数文メッセージで送る各 `SELECT` は、単一文と同じ
/// 可視集合（`Public` 全件）を返し、他テナントの `Private` 行を含まない。
/// 最後の文（他テナント所有 id への `DELETE`）は単一文の場合と同じ
/// `DELETE 0` になる（RLS-9/10 の応答同一性）。
#[test]
fn rls_visibility_and_delete_response_are_unchanged_across_multi_statement() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_bob(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 10; \
         SELECT id FROM docs LIMIT 10; \
         DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'bob-delete-alice-row'",
    );

    for _ in 0..2 {
        let _columns = read_row_description(&mut stream);
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
        }
        ids.sort();
        assert_eq!(
            ids,
            vec!["1", "2", "3"],
            "bob must see all Public rows regardless of statement position"
        );
        assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    }

    // 他テナント（alice）所有の id=1 は削除されない（応答は未存在行と区別
    // しない `DELETE 0`。RLS-9/10）。
    assert_eq!(read_command_complete(&mut stream), "DELETE 0");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs WHERE id = 1 LIMIT 1");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"), "alice's row must survive");
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 単一文の既存挙動は構造的に不変（`SplitOutcome::Single` として無加工の
/// テキストが渡される）。代表的な単一文の応答フレーム・SQLSTATE・メッセージが
/// 従来どおりであることを固定する。コメントを含む文は分割せず元テキスト
/// 全体を `42601` にする（先行文の応答は出ない）。
#[test]
fn single_statement_behavior_is_unchanged() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 1");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 1;");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (60, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'single-op'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "UPDATE docs SET lang = 'en' WHERE id = 60 USING OPERATION_ID 'single-update'",
    );
    assert_eq!(read_command_complete(&mut stream), "UPDATE 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DELETE FROM docs WHERE id = 60 USING OPERATION_ID 'single-delete'",
    );
    assert_eq!(read_command_complete(&mut stream), "DELETE 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM no_such_table LIMIT 1");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);

    // 未終端のブロックコメントは分割せず、元テキスト全体を `42601` として拒否する
    // （1 文目の応答は出ない）。
    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 1; /* x ; SELECT id FROM docs LIMIT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// コメント・二重引用符識別子を含む文を受理する（Issue #1346）。コメント内の `;` は
/// 区切りにならず、コメントだけの断片は空文として無視される。
#[test]
fn comments_and_quoted_identifiers_are_accepted() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    // 行コメントを挟んだ 2 文。
    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 1 -- x ; ignored\n; SELECT lang FROM docs LIMIT 1",
    );
    assert_eq!(read_row_description(&mut stream), vec!["id"]);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    assert_eq!(read_row_description(&mut stream), vec!["lang"]);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 末尾のコメントだけの断片は単一文と同じ応答。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 1; -- done");
    assert_eq!(read_row_description(&mut stream), vec!["id"]);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // コメントだけは EmptyQueryResponse。
    for sql in ["-- only", "/* only */", ";-- c", "/* a */ ; /* b */"] {
        send_simple_query(&mut stream, sql);
        expect_empty_query_response(&mut stream);
        read_ready_for_query(&mut stream);
    }

    // 二重引用符識別子（列名・テーブル名）。
    send_simple_query(&mut stream, "SELECT \"id\" FROM \"docs\" LIMIT 1");
    assert_eq!(read_row_description(&mut stream), vec!["id"]);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 中身が許可形でない引用符識別子・未終端の引用符は `42601`。
    for sql in [
        "SELECT \"id\" FROM \"do cs\" LIMIT 1",
        "SELECT \"id\" FROM \"select\" LIMIT 1",
        "SELECT \"id FROM docs LIMIT 1",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42601");
        read_ready_for_query(&mut stream);
    }
}

/// 引用符付きテーブル名でも RLS（テナント境界）は同じく適用される。
#[test]
fn quoted_table_name_keeps_tenant_visibility() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_bob(core);

    send_simple_query(&mut stream, "SELECT id FROM \"docs\" LIMIT 10");
    let _columns = read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    ids.sort();
    assert_eq!(ids, vec!["1", "2", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}
/// 分割実行 DML（Issue #1129）: 複数文メッセージに `PARTITIONED` 付き DML や `CANCEL` が
/// 含まれると、位置・先行文の内容によらず `25001` で全体を拒否し何も実行しない
/// （黙って autocommit・原子実行へ切り替えない）。
#[test]
fn partitioned_dml_in_a_multi_statement_message_is_rejected_without_executing_anything() {
    let (core, _guard) = new_core_three_tenant_docs();
    let mut stream = spawn_with_alice(core);

    for sql in [
        "SELECT id FROM docs LIMIT 1; \
         DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID 'pm-1' PARTITIONED",
        "DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID 'pm-2' PARTITIONED; SELECT 1",
        "SELECT id FROM docs LIMIT 1; CANCEL PARTITIONED DML 'pm-3' ON docs",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate_and_message(
            &mut stream,
            "25001",
            "partitioned DML cannot run inside a transaction block",
        );
        assert_eq!(read_ready_for_query_status(&mut stream), b'I', "{sql}");
    }

    // 何も実行されていない（行は残り、ジョブ記録もない）。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _columns = read_row_description(&mut stream);
    for _ in 0..3 {
        let _row = read_data_row(&mut stream);
    }
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "SHOW PARTITIONED DML 'pm-1' ON docs");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["status", "rows"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);
}

/// 単一文の `PARTITIONED` 付き `DELETE` は完了時に `DELETE <累計件数>` を返し、1 チャンク
/// 以上 commit した後に止まると SQLSTATE `VD001` の `ErrorResponse` を返す（Issue #1129）。
#[test]
fn partitioned_dml_over_the_wire_reports_completion_and_vd001() {
    let path = temp_db::unique_db_path("wire-pdml");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    {
        let sys =
            PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
                .expect("tenant");
        let mut session = engine::sql::mode::SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(
            &sys,
            &mut session,
            "CREATE TABLE udocs (n BIGINT, u TEXT UNIQUE)",
        )
        .expect("create table");
        let alice =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("tenant");
        core.execute_sql_in_session(
            &alice,
            &mut session,
            "INSERT INTO udocs (id, n, u) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c') \
             USING OPERATION_ID 'seed'",
        )
        .expect("seed");
    }
    let mut stream = spawn_with_alice(Arc::new(core));

    // 2 チャンク目で UNIQUE 違反: 1 件 commit 済みなので VD001（件数・原因コード入り）。
    send_simple_query(
        &mut stream,
        "UPDATE udocs SET u = 'dup' WHERE n > 0 USING OPERATION_ID 'wp-1' PARTITIONED CHUNK 1",
    );
    expect_error_response_with_sqlstate_and_message(&mut stream, "VD001", "committed 1 rows");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // 進捗照会は `interrupted`。
    send_simple_query(&mut stream, "SHOW PARTITIONED DML 'wp-1' ON udocs");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["status", "rows"]);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("interrupted"));
    assert_eq!(row[1].as_deref(), Some("1"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 取り消すと、再送は VD002。
    send_simple_query(&mut stream, "CANCEL PARTITIONED DML 'wp-1' ON udocs");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("cancelled"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "UPDATE udocs SET u = 'dup' WHERE n > 0 USING OPERATION_ID 'wp-1' PARTITIONED CHUNK 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "VD002");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // 完了する分割実行は累計件数を CommandComplete で返す。
    send_simple_query(
        &mut stream,
        "DELETE FROM udocs WHERE n > 0 USING OPERATION_ID 'wp-2' PARTITIONED CHUNK 2",
    );
    assert_eq!(read_command_complete(&mut stream), "DELETE 3");
    read_ready_for_query(&mut stream);
}
