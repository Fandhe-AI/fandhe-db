//! `CHECK` 制約（TABLE-16・TASK-204、Issue #906）の宣言（`CREATE TABLE`）・
//! 永続化（カタログ v7）・書き込み時検査（単一行 INSERT・複数行 INSERT・
//! ファイル形 INSERT・UPSERT・単一行 UPDATE・述語つき UPDATE・COPY FROM・
//! 明示トランザクション・Rust API の生 `RowInput` 経路）の結合テスト。書き込み時
//! 検査はいずれも単一の検査点 `constraint::enforce_row_constraints_in_txn` を通る。
//! ポインタ: `docs/spec/05-tasks.md` TASK-204・
//! `docs/spec/04-behavior/table-model.md` TABLE-16・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `EngineCore::execute_sql_in_session`／`execute_insert_sql`（先頭トークンの
//! 覗き見判定による `INSERT`／`UPDATE`／`CREATE TABLE` の各分岐）を production
//! 経路として検証する（`sql_create_table.rs`・`sql_upsert.rs`・
//! `sql_update_single_row.rs` と同じ流儀。実 `Storage`＋`CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）。詳細な設計判断は
//! `docs/design/sql-check-constraint.md` 参照。
//!
//! ファイル形 INSERT（`sql::exec::execute_file_insert` →
//! `incremental::index_file` → `tenant::replace_typed_rows_by_text_key`）は
//! `tests/recovery_content_hash.rs` の `new_file_core`／`file_insert_sql` と
//! 同じ流儀（`HashingEmbedder`・`IncrementalConfig`）で専用のヘルパーを持つ。

use engine::chunking::ChunkingConfig;
use engine::core::EngineCore;
use engine::embedding::HashingEmbedder;
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn granted_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn select_count(core: &EngineCore, alice: &PolicyContext, sql: &str) -> usize {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(alice, &mut session, sql)
        .expect("SELECT should succeed")
    {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

// --- DDL: CREATE TABLE の CHECK 宣言・永続化 -----------------------------

#[test]
fn create_table_with_column_level_check_persists_and_survives_reopen() {
    let (core, path) = new_core("check-persist");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("CREATE TABLE with CHECK should succeed");

    // 違反する INSERT が拒否されることで、CHECK が永続化されたことを間接的に
    // 確認する（カタログ再デコード経路を経由することの確認は下記の再オープン
    // テストで直接行う）。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (1, 'b') USING OPERATION_ID 'op-1'",
        )
        .expect_err("violating row must be rejected");
    assert_eq!(err.wire_code(), "23514");

    drop(core);
    // ストレージを再オープンしても検査が継続する（カタログから CHECK を
    // 再デコードして再コンパイルする経路の確認）。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (1, 'b') USING OPERATION_ID 'op-2'",
        )
        .expect_err("violating row must still be rejected after reopen");
    assert_eq!(err.wire_code(), "23514");
}

#[test]
fn create_table_rejects_check_referencing_unknown_column() {
    let (core, path) = new_core("check-unknown-col");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (body TEXT, CHECK (missing = 'x'))",
        )
        .expect_err("unknown column reference must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn create_table_rejects_check_calling_visible() {
    let (core, path) = new_core("check-visible");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (body TEXT, CHECK (visible()))",
        )
        .expect_err("visible() must be rejected in CHECK");
    assert_eq!(err.wire_code(), "42601");
}

// --- 単一行 INSERT --------------------------------------------------------

#[test]
fn insert_violating_check_is_rejected_with_no_side_effects() {
    let (core, path) = new_core("check-insert-violate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CONSTRAINT kind_ck CHECK (kind = 'a'))",
    )
    .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (1, 'b') USING OPERATION_ID 'op-1'",
        )
        .expect_err("violating row must be rejected");
    assert_eq!(err.wire_code(), "23514");
    // 制約名は含むが値・id・テナントは含まない固定文言。
    assert!(err.client_message().contains("kind_ck"));
    assert!(!err.client_message().contains('b'));

    // 副作用ゼロ: 行が存在しない。
    assert_eq!(
        select_count(
            &core,
            &alice,
            "SELECT id FROM docs WHERE kind = 'a' LIMIT 100"
        ),
        0
    );
    // 副作用ゼロ: 台帳エントリも残らない——同一 `operation_id` を正しい内容
    // （制約を満たす行）で再送すると成功する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind) VALUES (1, 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("resend with the same operation_id and a valid row must succeed");
    assert_eq!(
        select_count(
            &core,
            &alice,
            "SELECT id FROM docs WHERE kind = 'a' LIMIT 100"
        ),
        1
    );
}

#[test]
fn insert_satisfying_check_succeeds() {
    let (core, path) = new_core("check-insert-ok");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind) VALUES (1, 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("satisfying row must be accepted");
}

/// SQL-24・TASK-208、Issue #914: `CHECK` 本体の `LIKE` も中間一致・後方一致の
/// 一般形を受理し、書き込み時検査（`23514`）が新しい意味論で動作することを
/// 固定する（`declarative_filter::DeclarativeFilter::like` 経由。`sql::
/// allowlist::parse_check_body` は `parse_where` と同じ文法を共有するため
/// 構文層は無改造）。
#[test]
fn insert_violating_like_general_form_check_is_rejected_with_23514() {
    let (core, path) = new_core("check-insert-like-general-form");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (path TEXT CONSTRAINT path_ck CHECK (path LIKE '%.rs'))",
    )
    .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, path) VALUES (1, 'notes.md') USING OPERATION_ID 'op-1'",
        )
        .expect_err("path not ending with .rs must violate the CHECK");
    assert_eq!(err.wire_code(), "23514");
    assert!(err.client_message().contains("path_ck"));

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, path) VALUES (2, 'src/lib.rs') USING OPERATION_ID 'op-2'",
    )
    .expect("path ending with .rs must satisfy the CHECK");
}

#[test]
fn insert_null_column_is_treated_as_unknown_not_violation() {
    // 三値論理（設計 D1）: CHECK が参照する列が NULL の行は違反にしない。
    let (core, path) = new_core("check-insert-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'), body TEXT)",
    )
    .expect("create table");

    // `kind` を省略（NULL）した INSERT は CHECK を違反させない。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, body) VALUES (1, 'hello') USING OPERATION_ID 'op-1'",
    )
    .expect("NULL kind must not violate the CHECK constraint");
}

// --- 複数行 INSERT（バッチ。SQL-16・TASK-190） -----------------------------

#[test]
fn multi_row_insert_rejects_whole_batch_when_any_row_violates() {
    let (core, path) = new_core("check-multi-insert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (1, 'a'), (2, 'b') \
             USING OPERATION_ID 'op-1'",
        )
        .expect_err("batch with one violating row must be rejected entirely");
    assert_eq!(err.wire_code(), "23514");

    // 副作用ゼロ: id=1（制約を満たす行）も含め 1 件も書き込まれない。
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        0
    );
}

// --- UPSERT（SQL-20・TASK-193） -------------------------------------------

#[test]
fn upsert_do_update_violating_check_is_rejected() {
    let (core, path) = new_core("check-upsert-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind) VALUES (1, 'a') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (1, 'a') \
             ON CONFLICT (id) DO UPDATE SET kind = 'b' \
             USING OPERATION_ID 'op-upsert'",
        )
        .expect_err("DO UPDATE producing a violating row must be rejected");
    assert_eq!(err.wire_code(), "23514");

    // 副作用ゼロ: 既存行は変更されない。
    assert_eq!(
        select_count(
            &core,
            &alice,
            "SELECT id FROM docs WHERE kind = 'a' LIMIT 100"
        ),
        1
    );
}

#[test]
fn upsert_do_nothing_does_not_evaluate_check() {
    let (core, path) = new_core("check-upsert-do-nothing");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind) VALUES (1, 'a') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    // `DO NOTHING` は行を書かないため CHECK を評価しない（既存行がどんな値でも
    // 成功する）。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind) VALUES (1, 'a') \
         ON CONFLICT (id) DO NOTHING \
         USING OPERATION_ID 'op-upsert'",
    )
    .expect("DO NOTHING must succeed regardless of CHECK");
}

#[test]
fn upsert_new_insert_branch_violating_check_is_rejected() {
    let (core, path) = new_core("check-upsert-insert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (1, 'b') \
             ON CONFLICT (id) DO UPDATE SET kind = 'a' \
             USING OPERATION_ID 'op-1'",
        )
        .expect_err("non-conflicting insert branch must still be checked");
    assert_eq!(err.wire_code(), "23514");
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        0
    );
}

// --- 単一行 UPDATE（SQL-17・TASK-191） -------------------------------------

#[test]
fn single_row_update_violating_check_is_rejected_with_no_side_effects() {
    let (core, path) = new_core("check-update-single");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind) VALUES (1, 'a') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            "UPDATE docs SET kind = 'b' WHERE id = 1 USING OPERATION_ID 'op-upd'",
        )
        .expect_err("violating UPDATE must be rejected");
    assert_eq!(err.wire_code(), "23514");

    assert_eq!(
        select_count(
            &core,
            &alice,
            "SELECT id FROM docs WHERE kind = 'a' LIMIT 100"
        ),
        1
    );
}

// --- 述語つき UPDATE（SQL-19・TASK-192） -----------------------------------

#[test]
fn predicate_update_rejects_whole_statement_when_any_matched_row_violates() {
    let (core, path) = new_core("check-update-predicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT, status TEXT CHECK (status = 'ok'))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind, status) VALUES (1, 'x', 'ok'), (2, 'x', 'ok') \
         USING OPERATION_ID 'op-seed'",
    )
    .expect("seed rows");

    // `kind = 'x'` は 2 行に一致し、`status` を `'bad'` にする SET はいずれの
    // 一致行も違反させる。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            "UPDATE docs SET status = 'bad' WHERE kind = 'x' USING OPERATION_ID 'op-upd'",
        )
        .expect_err("predicate UPDATE producing violations must be rejected entirely");
    assert_eq!(err.wire_code(), "23514");

    // 副作用ゼロ: 両行とも変更されない。
    assert_eq!(
        select_count(
            &core,
            &alice,
            "SELECT id FROM docs WHERE status = 'ok' LIMIT 100"
        ),
        2
    );
}

// --- ALTER TABLE との相互作用（D5） ----------------------------------------

#[test]
fn drop_column_referenced_by_check_is_rejected() {
    let (core, path) = new_core("check-drop-column");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'), body TEXT)",
    )
    .expect("create table");
    // `ALTER TABLE` は SQL 表層未実装（TASK-203 の Rust API のみ）のため、
    // `EngineCore`（redb の単一ライター制約により `Storage` を排他保持する）を
    // 破棄してから素の `Storage` を開き直す（`table19_drop_alter_column.rs`
    // と同じ流儀）。
    drop(core);
    let storage = Storage::open(&path).expect("reopen storage");

    let err = storage
        .alter_table_drop_column("docs", "kind")
        .expect_err("dropping a column referenced by CHECK must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::DependentObjectsStillExist(_)
    ));

    // 参照しない列の削除は引き続き成功する。
    storage
        .alter_table_drop_column("docs", "body")
        .expect("dropping an unreferenced column must succeed");
}

// --- ファイル形 INSERT（codex-review 指摘・Issue #906 レビュー対応） ------

/// `tests/recovery_content_hash.rs::new_file_core` と同じ流儀（`HashingEmbedder`・
/// `IncrementalConfig`）で、`path` 列に CHECK 制約を持つテーブルを用意する。
fn new_file_core_with_check(label: &str, check_sql: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(16).expect("valid dim")))
        .with_incremental_config(IncrementalConfig {
            chunking: ChunkingConfig {
                lines_per_chunk: 10,
                max_markdown_section_chars: None,
            },
            ..IncrementalConfig::default()
        });
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("CREATE TABLE docs (embedding VECTOR(16), path TEXT {check_sql}, body TEXT)"),
    )
    .expect("create table with CHECK on path");
    (core, path)
}

fn file_insert_sql(path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (path, body) VALUES ('{path}', '{body}') USING OPERATION_ID '{op_id}'"
    )
}

/// codex-review 指摘（Issue #906）: ファイル形 INSERT
/// （`sql::exec::execute_file_insert` → `incremental::index_file` →
/// `tenant::replace_typed_rows_by_text_key`）は型付き値列 `rows: &[Vec<Value>]`
/// を受け取る構造化 API であり、生 `RowInput` 経路と異なり CHECK を強制しても
/// 非構造化 metadata 契約と衝突しない。この経路が CHECK を回避できないことを
/// 固定する（再現シナリオ: `path LIKE 'allowed/%'` のテーブルへ
/// `path = 'forbidden/x'` のファイル形 INSERT を送る）。
#[test]
fn file_insert_violating_check_is_rejected_with_no_side_effects() {
    let (core, path) =
        new_file_core_with_check("check-file-insert-violate", "CHECK (path LIKE 'allowed/%')");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");

    let err = core
        .execute_insert_sql(
            &alice,
            &file_insert_sql("forbidden/x", "hello world", "op-file-1"),
        )
        .expect_err("file-form insert violating CHECK must be rejected");
    assert_eq!(err.wire_code(), "23514");

    // 副作用ゼロ: 行が存在しない。
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        0
    );

    // 副作用ゼロ: 台帳エントリも残らない——同一 `operation_id` を正しい
    // パス（制約を満たす行）で再送すると成功する。
    core.execute_insert_sql(
        &alice,
        &file_insert_sql("allowed/x", "hello world", "op-file-1"),
    )
    .expect("resend with the same operation_id and an allowed path must succeed");
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        1
    );
}

#[test]
fn file_insert_satisfying_check_succeeds() {
    let (core, path) =
        new_file_core_with_check("check-file-insert-ok", "CHECK (path LIKE 'allowed/%')");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");

    core.execute_insert_sql(
        &alice,
        &file_insert_sql("allowed/a.md", "hello world", "op-file-ok"),
    )
    .expect("file-form insert satisfying CHECK must succeed");
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        1
    );
}

// --- main 上への再配線で追加した経路（constraint モジュールの単一検査点） ---

fn count_all(core: &EngineCore, caller: &PolicyContext) -> usize {
    select_count(core, caller, "SELECT id FROM docs LIMIT 100")
}

/// 明示トランザクション（SQL-31・TASK-221）内の書き込みも、共有 write
/// トランザクション上で同じ検査点（`constraint::enforce_row_constraints_in_txn`）
/// を通る。違反文は `23514` でトランザクションを `Failed` にし、ROLLBACK 後は
/// 先行文の行も含め何も残らない。満たす行だけのトランザクションは COMMIT できる。
#[test]
fn explicit_transaction_insert_violating_check_is_rejected() {
    use engine::sql::transaction::TransactionStatus;
    let (core, path) = new_core("check-txn");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, kind) VALUES (1, 'a') USING OPERATION_ID 'op-t1'",
    )
    .expect("satisfying row inside the transaction");
    let err = core
        .execute_sql_in_txn(
            &alice,
            &mut session,
            &mut txn,
            "INSERT INTO docs (id, kind) VALUES (2, 'b') USING OPERATION_ID 'op-t2'",
        )
        .expect_err("violating row inside the transaction must be rejected");
    assert_eq!(err.wire_code(), "23514");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(
        count_all(&core, &alice),
        0,
        "nothing must remain after ROLLBACK"
    );

    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, kind) VALUES (3, 'a') USING OPERATION_ID 'op-t3'",
    )
    .expect("satisfying row");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count_all(&core, &alice), 1);
}

/// `COPY ... FROM STDIN`（WIRE-17・TASK-220）も同じ検査点を通り、1 行でも違反
/// すればバッチ全体が未反映のまま `23514` で拒否される。
#[test]
fn copy_from_stdin_violating_check_rejects_whole_batch() {
    use engine::core::CopyPlan;
    let (core, path) = new_core("check-copy");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl,
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
    )
    .expect("create table");

    let run = |data: &[u8], op: &str| {
        let session = SessionState::default();
        let sql = format!("COPY docs (id, kind) FROM STDIN USING OPERATION_ID '{op}'");
        let mut copy = match core.begin_copy(&alice, &session, &sql).expect("begin copy") {
            CopyPlan::From(s) => s,
            CopyPlan::To(..) => panic!("expected COPY FROM plan"),
        };
        copy.feed(data)?;
        let batch = copy.finish()?;
        core.commit_copy_in(&alice, batch)
    };
    let err = run(b"1\ta\n2\tb\n", "copy-1").expect_err("violating batch must be rejected");
    assert_eq!(err.wire_code(), "23514");
    assert_eq!(count_all(&core, &alice), 0);
    let outcome = run(b"1\ta\n2\ta\n", "copy-2").expect("satisfying batch");
    assert_eq!(outcome.rows_affected, 2);
}

/// Rust API の生 `RowInput` 経路（`tenant::insert_row`）も同じ検査点を通る
/// （旧実装では対象外だったギャップを、書き込み後の読み戻し検査により解消）。
#[test]
fn raw_row_input_api_is_checked() {
    use engine::recovery::required_op_id::OperationId;
    use engine::row_codec::{encode_scalar_columns, Value};
    use engine::storage::RowInput;
    use engine::tenant::TenantWriteError;

    let path = unique_db_path("check-raw-row");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut ddl = granted_session();
        core.execute_sql_in_session(
            &ctx("alice"),
            &mut ddl,
            "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))",
        )
        .expect("create table");
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let schema = storage.get_table_schema("docs").expect("schema");
    let alice = ctx("alice");
    let write = |id: u64, kind: &str, op: &str| {
        let metadata =
            encode_scalar_columns(&schema, &[Value::Text(kind.to_string())]).expect("encode");
        let row = RowInput {
            tenant_id: "alice",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &metadata,
        };
        engine::tenant::insert_row(
            &storage,
            "docs",
            &alice,
            id,
            &row,
            &OperationId::parse(op).expect("op id"),
        )
    };
    let err = write(1, "b", "raw-1").expect_err("violating raw row must be rejected");
    assert!(
        matches!(&err, TenantWriteError::CheckViolation { constraint } if constraint == "docs_kind_check"),
        "{err:?}"
    );
    write(1, "a", "raw-1").expect("satisfying raw row with the same operation_id");
}

/// `CHECK` と UNIQUE の両方に違反する行は `23514`（CHECK を一意性より先に評価
/// する。PostgreSQL と同じ順序）。UNIQUE のみの違反は従来どおり `23505`。
#[test]
fn check_is_evaluated_before_unique() {
    let (core, path) = new_core("check-before-unique");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl,
        "CREATE TABLE docs (kind TEXT UNIQUE CHECK (kind LIKE 'a%'))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, kind) VALUES (1, 'ab') USING OPERATION_ID 'op-1'",
    )
    .expect("first row");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (2, 'ab') USING OPERATION_ID 'op-2'",
        )
        .expect_err("unique-only violation");
    assert_eq!(err.wire_code(), "23505");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, kind) VALUES (3, 'zz') USING OPERATION_ID 'op-3'",
        )
        .expect_err("check violation");
    assert_eq!(err.wire_code(), "23514");
}

/// 回帰（PR レビュー指摘 Medium）: 列 `constraint` の定義とも、制約名 `TEXT` の
/// 表制約とも読める `CREATE TABLE` は、列を黙って消した別スキーマを作らず
/// `42601` で拒否する（テーブルも作られない）。
#[test]
fn create_table_rejects_ambiguous_constraint_column_without_creating_table() {
    let (core, path) = new_core("check-ambiguous");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl = granted_session();
    for sql in [
        "CREATE TABLE docs (constraint TEXT CHECK (body = 'a'), body TEXT)",
        "CREATE TABLE docs (check TEXT, body TEXT)",
    ] {
        let err = core
            .execute_sql_in_session(&alice, &mut ddl, sql)
            .expect_err("ambiguous definition must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            "SELECT id FROM docs LIMIT 1",
        )
        .expect_err("table must not exist");
    assert_eq!(err.wire_code(), "42P01");
}

/// `CHECK` 参照列の `DROP COLUMN` 拒否と、索引宣言の掃除（TASK-206・INDEX-7、
/// Issue #908。`alter_table_drop_column` が削除列を含む宣言を同一 txn で消す）の
/// 関係: 拒否は索引掃除より前に判定され write トランザクションを commit しない
/// ため、`CHECK` 参照列を含む索引宣言は残る。参照しない列の削除では、その列を
/// 含む宣言だけが従来どおり掃除される。
#[test]
fn drop_column_rejected_by_check_keeps_index_declarations() {
    let (core, path) = new_core("check-drop-column-index");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    for sql in [
        "CREATE TABLE docs (kind TEXT CHECK (kind = 'a'), body TEXT)",
        "CREATE INDEX docs_kind_idx ON docs (kind)",
        "CREATE INDEX docs_body_idx ON docs (body)",
    ] {
        core.execute_sql_in_session(&alice, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    drop(core);
    let storage = Storage::open(&path).expect("reopen storage");
    let index_names = |storage: &Storage| {
        let mut names: Vec<String> = storage
            .list_indexes()
            .expect("list indexes")
            .into_iter()
            .map(|d| d.name)
            .collect();
        names.sort();
        names
    };

    let err = storage
        .alter_table_drop_column("docs", "kind")
        .expect_err("dropping a CHECK-referenced column must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::DependentObjectsStillExist(_)
    ));
    assert_eq!(
        index_names(&storage),
        vec!["docs_body_idx".to_string(), "docs_kind_idx".to_string()],
        "a rejected DROP COLUMN must not remove any index declaration"
    );

    storage
        .alter_table_drop_column("docs", "body")
        .expect("dropping an unreferenced column must succeed");
    assert_eq!(index_names(&storage), vec!["docs_kind_idx".to_string()]);
    assert_eq!(
        storage
            .get_table_schema("docs")
            .expect("schema")
            .columns
            .len(),
        1
    );
}

// --- 数値列（INTEGER/BIGINT）の CHECK 式比較（Issue #1075・TABLE-16 ポインタ）
// -----------------------------------------------------------------------

/// 数値列比較を含む `CHECK` の宣言・永続化・再オープン後の継続検査
/// （`create_table_with_column_level_check_persists_and_survives_reopen` と
/// 同じ流儀）。
#[test]
fn create_table_with_integer_column_check_persists_and_survives_reopen() {
    let (core, path) = new_core("check-integer-persist");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CONSTRAINT qty_ck CHECK (qty > 0))",
    )
    .expect("CREATE TABLE with numeric CHECK should succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, qty) VALUES (1, -1) USING OPERATION_ID 'op-1'",
        )
        .expect_err("violating row must be rejected");
    assert_eq!(err.wire_code(), "23514");
    assert!(err.client_message().contains("qty_ck"));

    drop(core);
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, qty) VALUES (1, -1) USING OPERATION_ID 'op-2'",
        )
        .expect_err("violating row must still be rejected after reopen");
    assert_eq!(err.wire_code(), "23514");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, qty) VALUES (1, 1) USING OPERATION_ID 'op-3'",
    )
    .expect("satisfying row must be accepted after reopen");
}

/// 回帰ガード（設計 D-3: 式述語が参照する数値列を `column_mask` へ OR
/// し忘れると、`scan_scalar_columns_masked` がマスク外＝NULL 扱いにし、
/// `enforce` の NULL スキップにより違反が黙って通過する fail-open になる）。
/// 複数行 INSERT で 1 行でも違反すればバッチ全体が拒否されることも併せて
/// 固定する。
#[test]
fn multi_row_insert_rejects_batch_with_numeric_check_violation() {
    let (core, path) = new_core("check-integer-multi-insert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (qty > 0))",
    )
    .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, qty) VALUES (1, 1), (2, -1) \
             USING OPERATION_ID 'op-1'",
        )
        .expect_err("batch with one violating row must be rejected entirely");
    assert_eq!(err.wire_code(), "23514");
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        0
    );
}

/// 三値論理（SQL 標準）: 数値列が NULL の行は UNKNOWN として通過する
/// （TEXT 列の `compiled_checks_enforce_treats_null_column_as_unknown_not_violation`
/// と同じ契約を INSERT 経路で固定する）。
#[test]
fn insert_with_null_numeric_column_passes_check() {
    let (core, path) = new_core("check-integer-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (qty > 0))",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (1) USING OPERATION_ID 'op-1'",
    )
    .expect("NULL qty must pass the CHECK (UNKNOWN, not a violation)");
}

/// 単一行 UPDATE でも数値列 CHECK が検査される
/// （`single_row_update_violating_check_is_rejected_with_no_side_effects` と
/// 同じ流儀）。
#[test]
fn single_row_update_violating_numeric_check_is_rejected() {
    let (core, path) = new_core("check-integer-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (qty > 0))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, qty) VALUES (1, 1) USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            "UPDATE docs SET qty = -1 WHERE id = 1 USING OPERATION_ID 'op-upd'",
        )
        .expect_err("violating UPDATE must be rejected");
    assert_eq!(err.wire_code(), "23514");
    // 副作用ゼロ: 行はそのまま残る（`qty = 1` は等価述語が文字列リテラルのみ
    // 受理する既存の制約により WHERE で直接検証できないため、`id` で存在を
    // 確認する）。
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs WHERE id = 1 LIMIT 100"),
        1
    );
}

/// 評価エラー（0 除算）は違反（`23514`）にも通過にも丸めず、fail-closed に
/// 拒否する（設計 D-4。行の値・id・テナントを含まない固定文言）。オーナー判断
/// （2026-09-28・Issue #1075、ERR-6・SQL-26・TABLE-16 ポインタ）: `wire_code`
/// は `XX000` 固定ではなく、通常の式評価（`WHERE`／`SELECT` と共有する
/// `sql::expr_program::ExprProgram`）と同じ `22000` になる。PostgreSQL と
/// 同様、CHECK 評価中のエラーは制約違反ではなく式評価エラーとして返す。
#[test]
fn insert_triggering_division_by_zero_in_check_returns_same_sqlstate_as_normal_expr_eval() {
    let (core, path) = new_core("check-integer-div-zero");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (100 / qty > 1))",
    )
    .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, qty) VALUES (1, 0) USING OPERATION_ID 'op-1'",
        )
        .expect_err("division by zero during CHECK evaluation must fail closed");
    assert_eq!(err.wire_code(), "22000");
    // 副作用ゼロ: 行は書き込まれない。
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        0
    );
}

/// UPDATE が CHECK の式評価エラーを起こす経路（`enforce_row_constraints_in_txn`
/// の共通検査点は INSERT と同一）でも、同じ `22000` を返し副作用ゼロを保つ
/// ことを固定する（オーナー判断 2026-09-28・Issue #1075）。
#[test]
fn update_triggering_division_by_zero_in_check_returns_same_sqlstate_as_normal_expr_eval() {
    let (core, path) = new_core("check-integer-div-zero-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (100 / qty > 1))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, qty) VALUES (1, 1) USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            "UPDATE docs SET qty = 0 WHERE id = 1 USING OPERATION_ID 'op-upd'",
        )
        .expect_err("division by zero during CHECK evaluation must fail closed");
    assert_eq!(err.wire_code(), "22000");
    // 副作用ゼロ: 更新前の値のまま残る。
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs WHERE id = 1 LIMIT 100"),
        1
    );
}

/// `ON CONFLICT (id) DO UPDATE` が CHECK の式評価エラーを起こす経路
/// （`tenant::upsert_typed_row` 経由でも同じ `enforce_row_constraints_in_txn`
/// を通る）でも同じ `22000` を返し副作用ゼロを保つ（オーナー判断
/// 2026-09-28・Issue #1075）。
#[test]
fn upsert_do_update_triggering_division_by_zero_in_check_returns_same_sqlstate_as_normal_expr_eval()
{
    let (core, path) = new_core("check-integer-div-zero-upsert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (100 / qty > 1))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, qty) VALUES (1, 1) USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, qty) VALUES (1, 1) \
             ON CONFLICT (id) DO UPDATE SET qty = 0 \
             USING OPERATION_ID 'op-upsert'",
        )
        .expect_err("division by zero during CHECK evaluation must fail closed");
    assert_eq!(err.wire_code(), "22000");
    // 副作用ゼロ: 更新前の値のまま残る。
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs WHERE id = 1 LIMIT 100"),
        1
    );
}

/// `CHECK` が参照する数値列も `TEXT` 列と同じく `ALTER TABLE ... DROP COLUMN`
/// の依存検査で拒否される（`drop_column_rejected_by_check_keeps_index_declarations`
/// と同じ流儀の対象列違い）。
#[test]
fn drop_column_referenced_by_numeric_check_is_rejected() {
    let (core, path) = new_core("check-integer-drop-column");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (qty > 0), body TEXT)",
    )
    .expect("create table");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_drop_column("docs", "qty")
        .expect_err("dropping a CHECK-referenced numeric column must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::DependentObjectsStillExist(_)
    ));
}

/// `CHECK` が参照する数値列は `ALTER TABLE ... ALTER COLUMN TYPE` 相当の型変更
/// （`alter_table_widen_numeric_precision`）からも拒否される（設計 D5。
/// `schema_check_references_column` の依存判定は列型を問わず先に走るため、
/// `ColumnType::Numeric` 以外の列（ここでは INTEGER）でも
/// `IncompatibleTypeChange` より前に `DependentObjectsStillExist` になる。
/// Issue #1075 以前は CHECK が INTEGER 列を参照できず、この経路自体が到達
/// 不能だった＝実際には検証されていなかった分岐）。
#[test]
fn widen_numeric_precision_on_check_referenced_integer_column_is_rejected() {
    let (core, path) = new_core("check-integer-widen-precision");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (qty > 0))",
    )
    .expect("create table");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_widen_numeric_precision("docs", "qty", 10)
        .expect_err("widening a CHECK-referenced column must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::DependentObjectsStillExist(_)
    ));
}

/// 複数の数値列（INTEGER・BIGINT）を跨ぐ表制約 `CHECK` の宣言・依存列記録・
/// 書き込み時検査を固定する（列制約と表制約が混在するケース）。
#[test]
fn multi_column_integer_bigint_table_check_enforces_and_records_both_columns() {
    let (core, path) = new_core("check-integer-bigint-multi-column");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (qty INTEGER CHECK (qty > 0), lim BIGINT, CONSTRAINT c CHECK (qty <= lim))",
    )
    .expect("create table");

    // 列制約（qty > 0）には違反しないが、表制約（qty <= lim）に違反する行。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, qty, lim) VALUES (1, 5, 1) USING OPERATION_ID 'op-1'",
        )
        .expect_err("qty <= lim violation must be rejected");
    assert_eq!(err.wire_code(), "23514");
    assert!(err.client_message().contains('c'));

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, qty, lim) VALUES (1, 5, 10) USING OPERATION_ID 'op-2'",
    )
    .expect("qty <= lim satisfied must be accepted");
    assert_eq!(
        select_count(&core, &alice, "SELECT id FROM docs LIMIT 100"),
        1
    );
}

/// Issue #1183: 汎用の `WHERE` でも数値列の式内参照が受理される（旧契約は
/// `CHECK` 専用の opt-in で `WHERE` 側は `22000` 拒否だった）。空テーブルに対する
/// 束縛・実行が成功することで、CHECK と共通の束縛になったことを固定する。
#[test]
fn select_where_numeric_column_expression_is_accepted() {
    let (core, path) = new_core("check-integer-where-accepted");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (qty INTEGER)")
        .expect("create table");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        "SELECT id FROM docs WHERE qty > 0 LIMIT 100",
    )
    .expect("numeric column expression in WHERE must be accepted");
}
