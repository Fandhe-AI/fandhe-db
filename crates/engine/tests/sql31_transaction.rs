//! 明示トランザクション `BEGIN`/`COMMIT`/`ROLLBACK`（SQL-31・TASK-221）の結合
//! テスト。ポインタ: `docs/spec/05-tasks.md` TASK-221・
//! `docs/spec/04-behavior/sql-surface.md` SQL-31・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `EngineCore::execute_sql_in_txn`（`sql::transaction::SessionTransaction` を
//! `&mut` で受け取るトランザクション対応入口）を production 経路として検証する。
//! `truncate_table.rs`・`insert_multi_row.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::{SearchMode, SessionState};
use engine::sql::transaction::{TransactionLimits, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql31-transaction");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op_id}'"
    )
}

#[test]
fn begin_insert_insert_commit_makes_both_rows_visible() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    assert_eq!(txn.status(), TransactionStatus::Idle);

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin"),
        SqlOutcome::Begin
    );
    assert_eq!(txn.status(), TransactionStatus::InTransaction);

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert 1");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect("insert 2");

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
            .expect("commit"),
        SqlOutcome::Commit
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after commit");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 同一トランザクション内で書き込んだテーブルの読み取りは、自トランザクションの未 commit
/// 変更を反映する（Issue #1179。読み取りは書き込みトランザクションを読み取り源にする）。
/// 別セッション（autocommit）からは見えない。
#[test]
fn reading_a_table_written_in_the_same_transaction_sees_the_uncommitted_row() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert");

    let select_sql = format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10");
    let outcome = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &select_sql)
        .expect("read of a table written in this txn sees the uncommitted row");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 1),
        other => panic!("expected Query, got {other:?}"),
    }
    assert_eq!(txn.status(), TransactionStatus::InTransaction);

    // 別セッションからは未 commit の行が見えない。
    let other = engine
        .execute_sql_in_session(&caller, &mut SessionState::default(), &select_sql)
        .expect("autocommit select from another session");
    match other {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }

    // `USING PLAN`・`EXPLAIN` を dirty テーブルへ向ける検証は辞書必須列（path/body）を持つ
    // テーブルが要るため `sql31_txn_file_insert_using_plan.rs`（Issue #1353）が担う。
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback"),
        SqlOutcome::Rollback
    );

    let outcome = engine
        .execute_sql_in_session(&caller, &mut SessionState::default(), &select_sql)
        .expect("select after rollback");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn rollback_discards_all_statements_in_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(10, "op-10"))
        .expect("insert");
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after rollback");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }

    // ROLLBACK 後に同じ operation_id で autocommit 再送すれば成功する
    // （台帳・行とも痕跡が残っていないことの証跡。RECOVER-12）。
    let outcome = engine
        .execute_sql_in_session(&caller, &mut session, &insert_sql(10, "op-10"))
        .expect("resend after rollback succeeds");
    assert!(matches!(outcome, SqlOutcome::Insert(_)));
}

/// 構文・許可リスト検証で失敗した文も、明示トランザクション中なら `Failed` へ
/// 遷移させる回帰テスト（PR #1041 レビュー指摘）。`Active` のまま残ると後続の
/// `COMMIT` が先行する `INSERT` を永続化してしまう。
#[test]
fn parse_error_inside_transaction_fails_the_transaction_and_commit_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SET search_mode = 'precision'",
        )
        .expect("set search_mode before begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(20, "op-20"))
        .expect("insert");

    // 2 つ目は `Failed` 中の拒否（`fail()` が `Failed` を `Idle` へ戻さないこと）。
    for bad_sql in ["SELEC id FROM documents", "DROP TABLE documents"] {
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, bad_sql)
            .expect_err("invalid statement is rejected");
        assert_eq!(
            txn.status(),
            TransactionStatus::Failed,
            "a rejected statement must fail the transaction: {bad_sql}"
        );
    }

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit is rejected while failed");
    assert_eq!(err.wire_code(), "25P02");
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback recovers from failed"),
        SqlOutcome::Rollback
    );
    // `Failed` 中のエラーで `fail()` が再度呼ばれても、BEGIN 時点のセッション
    // 状態は失われない。
    assert_eq!(session.search_mode(), Some(SearchMode::Precision));

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after rollback");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn nested_begin_fails_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect_err("nested BEGIN is rejected");
    assert_eq!(err.wire_code(), "25001");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    // Failed 中は ROLLBACK 以外すべて 25P02。
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect_err("statements are rejected while failed");
    assert_eq!(err.wire_code(), "25P02");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit is rejected while failed");
    assert_eq!(err.wire_code(), "25P02");

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback recovers from failed"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

/// Failed 中の `COMMIT`（`25P02`）が BEGIN 時点の `SessionState` を失わせない
/// 回帰テスト（Issue #942 レビュー指摘）。`BEGIN` 前にセッション状態を既定値
/// から変えておき、Failed 遷移 → `COMMIT` 拒否 → `ROLLBACK` の後も BEGIN 時点の
/// `search_mode` へ復元されることを固定する（既定値へ上書きされると `None` に
/// なる）。トランザクション内で変えた値が巻き戻ることもあわせて確認する。
#[test]
fn commit_while_failed_preserves_session_state_at_begin_for_rollback() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SET search_mode = 'precision'",
        )
        .expect("set search_mode before begin");
    assert_eq!(session.search_mode(), Some(SearchMode::Precision));

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SET search_mode = 'recall'",
        )
        .expect("set search_mode inside transaction");
    assert_eq!(session.search_mode(), Some(SearchMode::Recall));

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect_err("nested BEGIN is rejected");
    assert_eq!(err.wire_code(), "25001");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    // Failed 中の COMMIT は 25P02 で拒否され、Failed のまま据え置かれる。
    for _ in 0..2 {
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
            .expect_err("commit is rejected while failed");
        assert_eq!(err.wire_code(), "25P02");
        assert_eq!(txn.status(), TransactionStatus::Failed);
    }

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback recovers from failed"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(
        session.search_mode(),
        Some(SearchMode::Precision),
        "ROLLBACK must restore the session state captured at BEGIN"
    );
}

/// `Failed` 中は `ROLLBACK` 以外の文を parse より前に `25P02` で拒否する
/// （構文エラー・字句エラーの文も `42601` ではなく `25P02`）。`ROLLBACK` は
/// 受理して `Idle` へ戻る（PR #1041 レビュー指摘）。
#[test]
fn statements_while_failed_are_rejected_before_parsing_except_rollback() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect_err("nested BEGIN fails the transaction");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    for bad_sql in [
        "SELEC id FROM documents",
        "SELECT 'unterminated",
        "DROP TABLE documents",
        "",
    ] {
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, bad_sql)
            .expect_err("rejected while failed");
        assert_eq!(err.wire_code(), "25P02", "sql: {bad_sql:?}");
        assert_eq!(txn.status(), TransactionStatus::Failed);
    }

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "rollback work")
            .expect("rollback is accepted while failed"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
fn commit_or_rollback_without_begin_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit without begin");
    assert_eq!(err.wire_code(), "25P01");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect_err("rollback without begin");
    assert_eq!(err.wire_code(), "25P01");
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
fn reusing_operation_id_within_the_same_transaction_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "dup"))
        .expect("first insert with op id");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "dup"))
        .expect_err("reusing the same operation_id in the same txn is rejected");
    assert_eq!(err.wire_code(), "25000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

#[test]
fn unsupported_statement_inside_transaction_is_rejected_with_feature_not_supported() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    // DDL（`DROP TABLE` 等）は明示トランザクション内では未対応（対象外。
    // `DELETE`・`UPDATE`・UPSERT・複数行 `INSERT` は Issue #1179 で対応済み。
    // `sql31_txn_dml.rs` 参照）。
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DROP TABLE {TABLE}"),
        )
        .expect_err("DDL inside a transaction is not supported");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

#[test]
fn statement_count_limit_fails_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let engine = engine.with_transaction_limits(TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: 1,
    });
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("first statement is within the limit");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect_err("second statement exceeds max_statements");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

/// 明示トランザクション内の `COPY (...) TO STDOUT` も通常文と同じ文数上限に計上され、
/// 超過時は `54000` で `Failed` へ遷移する（Issue #1179 レビュー指摘）。
#[test]
fn copy_to_counts_toward_statement_limit_and_fails_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let engine = engine.with_transaction_limits(TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: 1,
    });
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    let sql = format!("COPY (SELECT id FROM {TABLE} LIMIT 10) TO STDOUT");

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .begin_copy_in_txn(&caller, &session, &mut txn, &sql)
        .expect("first COPY TO is within the limit");
    let err = engine
        .begin_copy_in_txn(&caller, &session, &mut txn, &sql)
        .expect_err("second COPY TO exceeds max_statements");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

fn visible_row_count(engine: &EngineCore, caller: &PolicyContext) -> usize {
    let outcome = engine
        .execute_sql_in_session(
            caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 持続時間の上限を過ぎてから `COMMIT` しても確定させず、`54000` で `Failed` へ
/// 遷移させる（PR #1041 レビュー指摘。以前は後続の文でしか検査しておらず、期限後の
/// `COMMIT` がそのまま書き込みを永続化していた）。
#[test]
fn commit_after_max_duration_is_rejected_and_not_persisted() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let max_duration = std::time::Duration::from_millis(200);
    let engine = engine.with_transaction_limits(TransactionLimits {
        max_duration,
        max_statements: 1_000,
    });
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(40, "op-40"))
        .expect("insert within the limit");
    std::thread::sleep(max_duration * 2);

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit after max_duration is rejected");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(visible_row_count(&engine, &caller), 0);

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(visible_row_count(&engine, &caller), 0);
}

/// 期限切れ後の次の要求（`release_if_expired`。wire 層が要求を受け取るたびに
/// 呼ぶ）でライタが解放され、別セッションの書き込みが通ること。遷移後の最初の
/// `COMMIT` には `54000` を 1 回だけ返し、以降は `25P02`（PR #1041 レビュー指摘）。
#[test]
fn release_if_expired_frees_the_writer_and_reports_the_limit_once() {
    let (engine, path) = new_core_with_short_write_lock_wait();
    let _cleanup = CleanupGuard(path);
    let max_duration = std::time::Duration::from_millis(200);
    let engine = engine.with_transaction_limits(TransactionLimits {
        max_duration,
        max_statements: 1_000,
    });
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(41, "op-41"))
        .expect("insert within the limit");
    assert!(!txn.release_if_expired(), "not expired yet");
    std::thread::sleep(max_duration * 2);

    assert!(txn.release_if_expired(), "expired transaction is released");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert!(!txn.release_if_expired(), "release is idempotent");

    // ライタが解放されているため、別セッションの autocommit 書き込みが通る。
    engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &insert_sql(42, "op-42"),
        )
        .expect("autocommit write after release");

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit after expiry is rejected");
    assert_eq!(err.wire_code(), "54000", "the expiry is reported once");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit is rejected while failed");
    assert_eq!(err.wire_code(), "25P02");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // id=41（期限切れトランザクション内）は破棄され、id=42 だけが残る。
    assert_eq!(visible_row_count(&engine, &caller), 1);
}

#[test]
fn truncate_inside_transaction_is_committed_atomically_with_inserts() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");

    // 事前に 1 行投入しておく（TRUNCATE の対象があることを確認するため）。
    engine
        .execute_sql_in_session(&caller, &mut SessionState::default(), &insert_sql(1, "pre"))
        .expect("seed row");

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'trunc-1'"),
        )
        .expect("truncate");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect("insert after truncate");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after commit");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(
            result.rows.len(),
            1,
            "id=1 は TRUNCATE で消え id=2 のみ残る"
        ),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn dropping_an_active_transaction_without_commit_leaves_no_trace() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    {
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "drop-1"))
            .expect("insert");
        // `txn` はここで drop される（接続断に相当。commit しない）。
    }

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after drop");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// [`new_core`] と同一だが、`Storage::with_write_lock_wait` で `writer_gate`
/// 待機上限を短く設定する（codex-review 指摘・Issue #942。`map_write_error`
/// 〔`sql/exec.rs`〕が `TenantWriteError::WriteLockTimeout` を `55P03` へ写像
/// することを、実際に別セッションの自動コミット書き込みが待機タイムアウト
/// する経路で固定するため）。
fn new_core_with_short_write_lock_wait() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql31-transaction-lock-timeout");
    let storage = Storage::open(&path)
        .expect("open storage")
        .with_write_lock_wait(std::time::Duration::from_millis(200));
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// 明示トランザクション（`BEGIN`）が `writer_gate` を保持している間、別セッションの
/// 自動コミット `INSERT` が待機タイムアウトすると `TenantWriteError::WriteLockTimeout`
/// が SQL 表層で `55P03`（`SqlSurfaceError::LockNotAvailable`）へ写像されることを
/// 固定する（codex-review 指摘・Issue #942）。`map_write_error`〔`sql/exec.rs`〕に
/// 専用アームが無いと `_` 節へ落ちて `XX000`（内部エラー）になり、クライアントが
/// リトライ可能なロック競合と内部エラーを判別できなくなる。
#[test]
fn autocommit_insert_times_out_with_lock_not_available_while_explicit_transaction_holds_writer_gate(
) {
    let (engine, path) = new_core_with_short_write_lock_wait();
    let _cleanup = CleanupGuard(path);
    let engine = std::sync::Arc::new(engine);
    let caller_a = ctx("tenant-a");
    let caller_b = ctx("tenant-b");

    let (began_tx, began_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

    let holder_engine = std::sync::Arc::clone(&engine);
    let holder = std::thread::spawn(move || {
        let mut session = SessionState::default();
        let mut txn = holder_engine.new_session_transaction();
        holder_engine
            .execute_sql_in_txn(&caller_a, &mut session, &mut txn, "BEGIN")
            .expect("begin holds writer_gate");
        began_tx.send(()).expect("notify begin done");
        // 別セッションの自動コミット `INSERT` がタイムアウトするまで `writer_gate`
        // を保持し続ける（解放シグナルを受けてから ROLLBACK する）。
        release_rx.recv().expect("wait for release signal");
        holder_engine
            .execute_sql_in_txn(&caller_a, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback releases writer_gate");
    });

    began_rx
        .recv()
        .expect("wait for BEGIN to acquire writer_gate");

    let err = engine
        .execute_sql_in_session(
            &caller_b,
            &mut SessionState::default(),
            &insert_sql(1, "op-1"),
        )
        .expect_err("autocommit INSERT must time out while writer_gate is held");
    assert_eq!(
        err.wire_code(),
        "55P03",
        "writer_gate 待機タイムアウトは 55P03（LOCK_NOT_AVAILABLE）へ写像されるべき \
         （`_` 節へ落ちて XX000 になる退行を検出する）"
    );

    release_tx.send(()).expect("signal holder to rollback");
    holder.join().expect("holder thread must not panic");
}

/// PR #1041 レビュー指摘（P1）の回帰: `parse_sql`／`parse_tokens` が `BEGIN`／
/// `COMMIT`／`ROLLBACK` を `ParsedSql::Transaction` として受理するようになった
/// 結果、トランザクション文脈を持たない `execute_sql_in_session`（`EngineCore::
/// execute_parsed_in_session` が実行本体）が、本 PR 以前の許可リスト外エラー
/// （`UnsupportedSyntax` ＝ `42601`）ではなく `TransactionFeatureNotSupported`
/// （`0A000`）を返すようになっていた。既存 API のエラー契約を変えないため、
/// 本エントリポイントでは `42601` を維持する（`0A000` は明示トランザクション
/// 対応の実行入口〔`execute_sql_in_txn`〕が `Active` 中に未対応の文を拒否する
/// 場合専用のまま）。
#[test]
fn execute_sql_in_session_rejects_transaction_control_statements_with_unsupported_syntax() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");

    for sql in ["BEGIN", "COMMIT", "ROLLBACK"] {
        let err = engine
            .execute_sql_in_session(&caller, &mut SessionState::default(), sql)
            .expect_err("transaction control statements are unsupported by this entry point");
        assert_eq!(
            err.wire_code(),
            "42601",
            "{sql} 経由の execute_sql_in_session は従来どおり 42601（許可リスト外） \
             を返すべき（0A000 への退行を検出する）"
        );
    }
}

// --- 接続断からの回復（RECOVER-12・ERR-6。Issue #1180） ------------------------
//
// クライアントは応答を失った明示トランザクションを、新しい `BEGIN` の中で先頭文から
// 同じ `operation_id` で再送して成否を確定する。台帳由来の `23505`
// （`DUPLICATE_OPERATION_ID`）だけが commit 済みの根拠になり、行制約由来の
// `23505`（`UNIQUE_VIOLATION`）は根拠にならない。

/// 応答を失った 2 文のトランザクションを COMMIT まで完了させる。
fn commit_two_inserts(engine: &EngineCore, caller: &PolicyContext) {
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    for sql in [
        "BEGIN".to_string(),
        insert_sql(1, "op-a"),
        insert_sql(2, "op-b"),
        "COMMIT".to_string(),
    ] {
        engine
            .execute_sql_in_txn(caller, &mut session, &mut txn, &sql)
            .expect("statement");
    }
}

#[test]
fn recovery_resend_in_new_begin_after_commit_is_ledger_duplicate() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    commit_two_inserts(&engine, &caller);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-a"))
        .expect_err("resend of a committed first statement must be a ledger duplicate");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(
        ClassifiedError::error_class(&err),
        ErrorClass::DuplicateOperationId
    );
    assert_eq!(
        ClassifiedError::error_class(&err).label(),
        "DUPLICATE_OPERATION_ID"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback"),
        SqlOutcome::Rollback
    );
    assert_eq!(visible_row_count(&engine, &caller), 2);
}

#[test]
fn recovery_resend_after_lost_uncommitted_transaction_applies_exactly_once() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    {
        let mut txn = engine.new_session_transaction();
        for sql in [
            "BEGIN".to_string(),
            insert_sql(1, "op-a"),
            insert_sql(2, "op-b"),
        ] {
            engine
                .execute_sql_in_txn(&caller, &mut session, &mut txn, &sql)
                .expect("statement");
        }
        // COMMIT せずに drop（接続断）。
    }

    let mut txn = engine.new_session_transaction();
    for sql in [
        "BEGIN".to_string(),
        insert_sql(1, "op-a"),
        insert_sql(2, "op-b"),
        "COMMIT".to_string(),
    ] {
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &sql)
            .expect("resend must succeed because nothing was committed");
    }
    assert_eq!(visible_row_count(&engine, &caller), 2);
    // 台帳には各 operation_id が 1 回ずつ記録されている（二重適用なし）。
    for (id, op) in [(1, "op-a"), (2, "op-b")] {
        let err = engine
            .execute_sql_in_session(&caller, &mut session, &insert_sql(id, op))
            .expect_err("already recorded");
        assert_eq!(
            ClassifiedError::error_class(&err),
            ErrorClass::DuplicateOperationId
        );
    }
    assert_eq!(visible_row_count(&engine, &caller), 2);
}

#[test]
fn recovery_row_constraint_conflict_is_not_a_commit_proof() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    // 別の operation_id が同じ行 id を先に確定させている。
    engine
        .execute_sql_in_session(&caller, &mut session, &insert_sql(1, "op-x"))
        .expect("autocommit insert");

    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-a"))
        .expect_err("row id conflict");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(
        ClassifiedError::error_class(&err),
        ErrorClass::UniqueViolation
    );
    assert_eq!(
        ClassifiedError::error_class(&err).label(),
        "UNIQUE_VIOLATION"
    );
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback"),
        SqlOutcome::Rollback
    );
    // op-a は台帳に残っていない（別 id なら成功する）。
    engine
        .execute_sql_in_session(&caller, &mut session, &insert_sql(3, "op-a"))
        .expect("op-a was rolled back and must be reusable");
}
