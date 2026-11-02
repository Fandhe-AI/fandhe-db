//! 失敗状態（`25P02`）からの `ROLLBACK TO` 復帰（Issue #1947・SQL-31・SQL-69 のポインタ。
//! `docs/spec/04-behavior/sql-surface.md`）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、文エラーで `Failed` へ遷移した後に
//! `ROLLBACK TO` で `InTransaction` へ復帰し、`COMMIT` に失敗した文の効果が残らないことを固定する。
//! 復帰できない失敗（savepoint 無し・上限超過）は従来どおり `ROLLBACK` のみ受理することと、
//! 復帰の余地がある間だけ writer を保持することも合わせて固定する。
//! 時間計測に依存しない（`55P03` は待機上限で判定する。期限切れは `tests/` 直下の単独テスト）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core_with_limits(
    tag: &str,
    limits: Option<TransactionLimits>,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_write_lock_wait(std::time::Duration::from_millis(100));
    if let Some(l) = limits {
        core = core.with_transaction_limits(l);
    }
    (core, guard)
}

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    new_core_with_limits(tag, None)
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

const SELECT: &str = "SELECT id FROM documents ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10";
/// 構文エラー（文の実行が失敗する入力）。
const BAD_SYNTAX: &str = "SELEC nonsense";

fn rows_of(outcome: SqlOutcome) -> usize {
    match outcome {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 別セッション（Tx なし）から見える確定済みの行数。
fn committed_rows(engine: &EngineCore, caller: &PolicyContext) -> usize {
    rows_of(
        engine
            .execute_sql_in_session(caller, &mut SessionState::default(), SELECT)
            .expect("select"),
    )
}

fn run<'e>(
    engine: &'e EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    engine.execute_sql_in_txn(caller, session, txn, sql)
}

fn code(r: Result<SqlOutcome, SqlSurfaceError>) -> &'static str {
    r.expect_err("expected an error").wire_code()
}

/// `BEGIN` → `SAVEPOINT a`（writer 取得前なので Restorable）。
fn begin_sp<'e>(
    core: &'e EngineCore,
    caller: &PolicyContext,
    s: &mut SessionState,
) -> SessionTransaction<'e> {
    let mut txn = core.new_session_transaction();
    run(core, caller, s, &mut txn, "BEGIN").unwrap();
    run(core, caller, s, &mut txn, "SAVEPOINT a").unwrap();
    txn
}

/// 別セッションの書き込みの結果コード（writer 保持なら待機上限の `55P03`、成功なら `None`）。
fn other_session_write_code(core: &EngineCore, id: u64, op: &str) -> Option<&'static str> {
    let mut other = SessionState::default();
    core.execute_sql_in_session(&ctx("tenant-b"), &mut other, &insert_sql(id, op))
        .err()
        .map(|e| e.wire_code())
}

#[test]
fn failed_then_rollback_to_then_commit_keeps_only_surviving_writes() {
    let (core, _g) = new_core("sql69-fr-basic");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    assert_eq!(txn.status(), TransactionStatus::Failed);
    // 失敗中は ROLLBACK TO 以外 25P02。
    assert_eq!(code(run(&core, &caller, &mut s, &mut txn, SELECT)), "25P02");
    assert_eq!(
        run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap(),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(3, "op-3")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    // savepoint 後の id=1 は巻き戻され、復帰後の id=3 だけが残る。
    assert_eq!(committed_rows(&core, &caller), 1);
}

#[test]
fn failure_after_writes_in_same_statement_batch_is_fully_undone() {
    let (core, _g) = new_core("sql69-fr-partial");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap();
    // 同じ operation_id の再利用（25000）で失敗させる。writer は書き込み済みの状態。
    assert_eq!(
        code(run(
            &core,
            &caller,
            &mut s,
            &mut txn,
            &insert_sql(3, "op-1")
        )),
        "25000"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    // 失敗後の復帰で panic せず、書き込みが全て取り消される。
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn operation_id_of_failed_statement_is_reusable_after_recovery() {
    let (core, _g) = new_core("sql69-fr-opid");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    // 巻き戻した ID は同じ Tx 内で再送できる（25000 にならない）。
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &caller), 1);
}

#[test]
fn repeated_failure_and_recovery_keeps_savepoint() {
    let (core, _g) = new_core("sql69-fr-repeat");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    for i in 0..3u64 {
        run(
            &core,
            &caller,
            &mut s,
            &mut txn,
            &insert_sql(i, &format!("op-{i}")),
        )
        .unwrap();
        assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
        assert_eq!(txn.status(), TransactionStatus::Failed);
        run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
        assert_eq!(txn.status(), TransactionStatus::InTransaction);
    }
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn failure_without_savepoint_cannot_recover_and_releases_writer() {
    let (core, _g) = new_core("sql69-fr-nosp");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        code(run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a")),
        "25P02"
    );
    // writer は即時に解放されている。
    assert_eq!(other_session_write_code(&core, 9, "other-1"), None);
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
fn released_savepoint_does_not_allow_recovery() {
    let (core, _g) = new_core("sql69-fr-released");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, "RELEASE a").unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    assert_eq!(
        code(run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a")),
        "25P02"
    );
}

#[test]
fn other_statements_while_failed_are_25p02_and_state_is_kept() {
    let (core, _g) = new_core("sql69-fr-other");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    for sql in [
        "COMMIT",
        "END",
        "SAVEPOINT b",
        "RELEASE a",
        "BEGIN",
        "SET TRANSACTION READ ONLY",
        SELECT,
    ] {
        assert_eq!(
            code(run(&core, &caller, &mut s, &mut txn, sql)),
            "25P02",
            "{sql}"
        );
        assert_eq!(txn.status(), TransactionStatus::Failed, "{sql}");
    }
    // 拒否の後も復帰できる。
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn rollback_while_recoverable_discards_everything() {
    let (core, _g) = new_core("sql69-fr-rollback");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(committed_rows(&core, &caller), 0);
    // writer は解放済み。
    assert_eq!(other_session_write_code(&core, 9, "other-1"), None);
}

#[test]
fn writer_is_held_while_recoverable_and_released_after_commit() {
    let (core, _g) = new_core("sql69-fr-writer");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    assert_eq!(other_session_write_code(&core, 9, "other-1"), Some("55P03"));
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    assert_eq!(other_session_write_code(&core, 9, "other-2"), Some("55P03"));
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(other_session_write_code(&core, 9, "other-3"), None);
}

#[test]
fn rollback_to_unknown_or_unrestorable_name_keeps_failed_state_recoverable() {
    let (core, _g) = new_core("sql69-fr-names");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    // writer が dirty になった後の savepoint は Unrestorable。
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT b").unwrap();
    assert!(run(&core, &caller, &mut s, &mut txn, BAD_SYNTAX).is_err());
    assert_eq!(
        code(run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO b")),
        "0A000"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        code(run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO zz")),
        "3B001"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn statement_limit_failure_is_not_recoverable() {
    let limits = TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: 3,
    };
    let (core, _g) = new_core_with_limits("sql69-fr-limit", Some(limits));
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap();
    assert_eq!(
        code(run(
            &core,
            &caller,
            &mut s,
            &mut txn,
            &insert_sql(3, "op-3")
        )),
        "54000"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        code(run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a")),
        "25P02"
    );
    // writer は解放済み。
    assert_eq!(other_session_write_code(&core, 9, "other-1"), None);
}

#[test]
fn recovery_stays_within_tenant_visibility() {
    let (core, _g) = new_core("sql69-fr-tenant");
    let a = ctx("tenant-a");
    let mut other = SessionState::default();
    core.execute_sql_in_session(&ctx("tenant-b"), &mut other, &insert_sql(7, "b-1"))
        .unwrap();
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &a, &mut s);
    assert!(run(&core, &a, &mut s, &mut txn, BAD_SYNTAX).is_err());
    run(&core, &a, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    // 復帰後も別テナントの行は見えない。
    assert_eq!(
        rows_of(run(&core, &a, &mut s, &mut txn, SELECT).unwrap()),
        0
    );
    run(&core, &a, &mut s, &mut txn, &insert_sql(8, "a-1")).unwrap();
    run(&core, &a, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &a), 1);
    assert_eq!(committed_rows(&core, &ctx("tenant-b")), 1);
}
