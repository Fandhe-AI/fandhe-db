//! writer 遅延取得（Issue #2400）の前後で既存挙動が変わらないことを固定するクレート内テスト
//! （Issue #2402。親 #2396・TASK-333。ポインタ: SQL-19・SQL-31）。
//!
//! `SessionTransaction::write_txn()`（`pub(crate)`）と `storage.write_txn_creations()` で writer の
//! 有無を直接観測する。`deferred_writer_tests.rs`（#2400）が書き込み文の種類ごとの取得を担うのに対し、
//! 本ファイルは「読み取りだけの区間（`USING PLAN`・`EXPLAIN`・カーソル）→ 最初の書き込み」の形、
//! ファイル形 `INSERT` の失敗、`COPY FROM`・`setval` の取得点を担う。
//! 結合側（read-your-writes・隔離・キャッシュ非汚染）は
//! `tests/sql_t01_39/sql31_deferred_writer_regression.rs` が担う。

use super::CopyPlan;
use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::embedding::HashingEmbedder;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::query_planner::{LlmClient, PlanError};
use crate::sql::mode::SessionState;
use crate::sql::transaction::{SessionTransaction, TransactionStatus};
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;

/// 固定の検索語展開を返すだけの LLM スタブ（外部 I/O を伴わない）。
struct FixedPlanner;

impl LlmClient for FixedPlanner {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#.to_string())
    }
}

fn new_core(tag: &str, with_embedder: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::BigInt, false),
            ],
        ))
        .expect("create documents");
    storage
        .create_table(&TableSchema::new(
            "files",
            vec![
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("embedding", ColumnType::Vector(DIM), true),
            ],
        ))
        .expect("create files");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(FixedPlanner));
    if with_embedder {
        core = core.with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")));
    }
    (core, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn file_insert(path: &str, op: &str) -> String {
    format!(
        "INSERT INTO files (path, body) VALUES ('{path}', 'alpha content') \
         USING OPERATION_ID '{op}'"
    )
}

fn row_count<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> usize {
    match core
        .execute_sql_in_txn(&ctx("tenant-a"), session, txn, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
    {
        crate::sql::SqlOutcome::Query(q) => q.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

/// 読み取りだけの区間（`USING PLAN`・`EXPLAIN`・カーソル）は writer も write txn も作らない。
#[test]
fn read_only_prefix_including_using_plan_and_explain_takes_no_writer() {
    let (core, _g) = new_core("deferred-regress-readonly", true);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&caller, &mut session, &file_insert("a.txt", "seed"))
        .expect("seed");
    let before = core.storage.write_txn_creations();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "SELECT path FROM files LIMIT 10",
        "SELECT path FROM files USING PLAN('find alpha') LIMIT 10",
        "EXPLAIN SELECT COUNT(*) FROM files",
        "EXPLAIN SELECT path FROM files USING PLAN('find alpha') LIMIT 10",
        "DECLARE c CURSOR FOR SELECT path FROM files LIMIT 10",
        "FETCH 1 FROM c",
    ] {
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
        assert!(txn.write_txn().is_none(), "{sql} must not take the writer");
    }
    assert_eq!(core.storage.write_txn_creations(), before);
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

/// 読み取り後のファイル形 `INSERT` が writer を取り、未 commit のチャンクが見え、ROLLBACK で解放される。
#[test]
fn file_form_insert_after_read_prefix_acquires_writer() {
    let (core, _g) = new_core("deferred-regress-file-insert", true);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let scan = "SELECT path FROM files LIMIT 10";
    assert_eq!(row_count(&core, &mut session, &mut txn, scan), 0);
    assert!(txn.write_txn().is_none());
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, &file_insert("a.txt", "f1"))
        .expect("file insert");
    assert!(
        txn.write_txn().is_some(),
        "file-form INSERT takes the writer"
    );
    assert_eq!(row_count(&core, &mut session, &mut txn, scan), 1);
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(txn.write_txn().is_none());
}

/// 最初の書き込みのファイル形 `INSERT` が失敗（埋め込みなし）しても writer を残さず fail-closed になる。
#[test]
fn file_form_insert_failure_as_first_write_releases_writer() {
    let (core, _g) = new_core("deferred-regress-file-fail", false);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &file_insert("a.txt", "f1"))
        .expect_err("no embedder");
    assert_eq!(err.wire_code(), "XX000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert!(
        txn.write_txn().is_none(),
        "a failed Tx must not hold the writer"
    );
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SELECT path FROM files LIMIT 1",
        )
        .expect_err("failed state");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

/// `COPY FROM` はデータ受信中に writer を持たず、取り込み確定（`commit_copy_in_txn`）で初めて取得する。
#[test]
fn copy_from_takes_writer_only_at_commit_after_read_prefix() {
    let (core, _g) = new_core("deferred-regress-copy", true);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let scan = "SELECT n FROM documents LIMIT 10";
    assert_eq!(row_count(&core, &mut session, &mut txn, scan), 0);
    let plan = core
        .begin_copy_in_txn(
            &caller,
            &session,
            &mut txn,
            "COPY documents (id, embedding, n) FROM STDIN USING OPERATION_ID 'copy-1'",
        )
        .expect("begin copy");
    assert!(txn.write_txn().is_none());
    let mut copy = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM plan"),
    };
    copy.feed(b"1\t[0.1,0.2]\t5\n2\t[0.1,0.2]\t6\n")
        .expect("feed");
    assert!(txn.write_txn().is_none(), "no writer while receiving data");
    let batch = copy.finish().expect("finish");
    assert!(txn.write_txn().is_none());
    core.commit_copy_in_txn(&caller, &mut txn, batch)
        .expect("commit copy");
    assert!(
        txn.write_txn().is_some(),
        "the batch commit takes the writer"
    );
    assert_eq!(row_count(&core, &mut session, &mut txn, scan), 2);
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(txn.write_txn().is_none());
    let mut other = SessionState::default();
    match core
        .execute_sql_in_session(&caller, &mut other, scan)
        .expect("select")
    {
        crate::sql::SqlOutcome::Query(q) => assert!(q.rows.is_empty()),
        other => panic!("unexpected outcome {other:?}"),
    }
}

/// `setval` は最初の書き込みとして writer を取る。メモリ上の現在値は ROLLBACK でも戻らない
/// （PostgreSQL と同じ非トランザクション性。`docs/design/sequence-namespace-and-ledger.md` D4）。
#[test]
fn setval_after_read_prefix_acquires_writer_and_stays_non_transactional() {
    let (core, _g) = new_core("deferred-regress-setval", true);
    let caller = ctx("tenant-a");
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(&caller, &mut ddl, "CREATE SEQUENCE s")
        .expect("create sequence");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "SELECT path FROM files LIMIT 1",
    )
    .expect("read");
    assert!(txn.write_txn().is_none());
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT setval('s', 100)")
        .expect("setval");
    assert!(txn.write_txn().is_some(), "setval takes the writer");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(txn.write_txn().is_none());
    let mut other = SessionState::default();
    // ROLLBACK 後も setval の値は残り、autocommit の nextval は続きの 101 を払い出す。
    core.execute_sql_in_session(
        &caller,
        &mut other,
        "INSERT INTO documents (id, embedding, n) VALUES (1, '[1.0, 0.0]', nextval('s')) \
         USING OPERATION_ID 'after-rollback'",
    )
    .expect("insert with nextval");
    let out = core
        .execute_sql_in_session(&caller, &mut other, "SELECT n FROM documents LIMIT 10")
        .expect("select");
    let text = format!("{out:?}");
    assert!(
        text.contains("SignedInteger(101)"),
        "setval stays after ROLLBACK: {text}"
    );
}
