//! `EngineCore::execute_bound_partitioned_{update,delete}_in_session`・
//! `show_partitioned_dml_in_session`・`cancel_partitioned_dml_in_session`（NoSQL 表層の
//! 分割実行 op 向け束縛済みセッション入口。Issue #1130。ポインタ: ADR
//! `docs/design/partitioned-dml.md` 10 節、NOSQL-12・SQL-19、RECOVER-11、RLS-9・RLS-10、
//! ERR-1・ERR-2・ERR-4）が engine クレート外から到達可能な公開 API であり、SQL 表層の
//! `... PARTITIONED`（`execute_sql_in_session` 経由）と**同一の共通本体**・**同一の内容照合
//! ハッシュドメイン**・**同一のジョブ表**に到達することを固定する結合テスト。
//!
//! 部分完了は「UNIQUE 列へ同じ値を `CHUNK 1` で書き込む」ことで 2 チャンク目に確定的な
//! `23505` を起こして作る（時間に依存しない。`partitioned_dml_sql.rs` と同じ流儀）。

use std::num::NonZeroUsize;

use engine::catalog::TableSchema;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::{InsertLiteral, SqlSurfaceError, WherePredicate};
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::parser::PartitionedDmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op(raw: &str) -> OperationId {
    OperationId::parse(raw).expect("valid operation_id")
}

fn nz(n: usize) -> Option<NonZeroUsize> {
    NonZeroUsize::new(n)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn sql_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn setup(label: &str, rows: &[(u64, i64, &str)]) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)",
    )
    .expect("create table");
    seed(&core, "alice", rows);
    (core, guard)
}

fn seed(core: &EngineCore, tenant: &str, rows: &[(u64, i64, &str)]) {
    let values: Vec<String> = rows
        .iter()
        .map(|(id, n, u)| format!("({id}, {n}, '{u}')"))
        .collect();
    ok(
        core,
        tenant,
        &format!(
            "INSERT INTO docs (id, n, u) VALUES {} USING OPERATION_ID 'seed-{tenant}'",
            values.join(", ")
        ),
    );
}

fn count(core: &EngineCore, tenant: &str) -> usize {
    match ok(core, tenant, "SELECT id FROM docs LIMIT 1000") {
        SqlOutcome::Query(q) => q.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn three_rows() -> Vec<(u64, i64, &'static str)> {
    vec![(1, 10, "a"), (2, 20, "b"), (3, 30, "c")]
}

/// 全行に一致する述語（SQL 側は `WHERE u LIKE '%'`）。
fn all_rows() -> Vec<WherePredicate> {
    vec![WherePredicate::Prefix {
        column: "u".to_string(),
        pattern: "%".to_string(),
    }]
}

const SQL_ALL: &str = "WHERE u LIKE '%'";

fn set_u(value: &str) -> Vec<(String, InsertLiteral)> {
    vec![("u".to_string(), InsertLiteral::String(value.to_string()))]
}

fn bound_delete(
    core: &EngineCore,
    tenant: &str,
    id: &str,
    chunk: Option<NonZeroUsize>,
) -> Result<u64, SqlSurfaceError> {
    core.execute_bound_partitioned_delete_in_session(
        &ctx(tenant),
        "docs",
        Some(&op(id)),
        chunk,
        |_schema: &TableSchema| Ok(all_rows()),
    )
    .map(|o| o.rows_affected)
}

fn bound_update(
    core: &EngineCore,
    tenant: &str,
    id: &str,
    value: &str,
    chunk: Option<NonZeroUsize>,
) -> Result<u64, SqlSurfaceError> {
    core.execute_bound_partitioned_update_in_session(
        &ctx(tenant),
        "docs",
        Some(&op(id)),
        chunk,
        |_schema: &TableSchema| Ok((set_u(value), all_rows())),
    )
    .map(|o| o.rows_affected)
}

fn status_rows(q: &QueryResult) -> Vec<(String, i64)> {
    q.rows
        .iter()
        .map(|r| match (&r.cells[0], &r.cells[1]) {
            (Cell::Text(s), Cell::SignedInteger(n)) => (s.clone(), *n),
            other => panic!("unexpected cells {other:?}"),
        })
        .collect()
}

fn show(core: &EngineCore, tenant: &str, table: &str, id: &str) -> QueryResult {
    core.show_partitioned_dml_in_session(&ctx(tenant), table, &op(id))
        .expect("show must succeed")
}

fn cancel(core: &EngineCore, tenant: &str, table: &str, id: &str) -> QueryResult {
    core.cancel_partitioned_dml_in_session(&ctx(tenant), table, &op(id))
        .expect("cancel must succeed")
}

/// UNIQUE 列へ同じ値を `CHUNK 1` で書き、1 チャンク目だけ commit して止める（SQL 経路）。
fn interrupt_via_sql(core: &EngineCore, id: &str) -> SqlSurfaceError {
    run(
        core,
        "alice",
        &format!(
            "UPDATE docs SET u = 'dup' {SQL_ALL} USING OPERATION_ID '{id}' PARTITIONED CHUNK 1"
        ),
    )
    .map(|o| panic!("must be interrupted, got {o:?}"))
    .unwrap_err()
}

fn set_n(value: &str) -> Vec<(String, InsertLiteral)> {
    vec![("n".to_string(), InsertLiteral::Number(value.to_string()))]
}

/// 一意制約に触れない `SET n = <value>` の分割実行 UPDATE（SQL 側は `SET n = <value>`）。
fn bound_update_n(
    core: &EngineCore,
    id: &str,
    value: &str,
    chunk: Option<NonZeroUsize>,
) -> Result<u64, SqlSurfaceError> {
    core.execute_bound_partitioned_update_in_session(
        &ctx("alice"),
        "docs",
        Some(&op(id)),
        chunk,
        |_schema: &TableSchema| Ok((set_n(value), all_rows())),
    )
    .map(|o| o.rows_affected)
}

fn code<T: std::fmt::Debug>(r: Result<T, SqlSurfaceError>) -> String {
    r.map(|v| panic!("must fail, got {v:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

// --- 完了・ハッシュドメイン共有 ---------------------------------------------------

#[test]
fn bound_entries_complete_with_cumulative_count() {
    let (core, _g) = setup("pdml-bound-complete", &three_rows());
    assert_eq!(
        bound_update_n(&core, "u-1", "99", nz(2)).expect("must succeed"),
        3
    );
    assert_eq!(
        bound_delete(&core, "alice", "d-1", nz(2)).expect("must succeed"),
        3
    );
    assert_eq!(count(&core, "alice"), 0);
    assert_eq!(
        status_rows(&show(&core, "alice", "docs", "d-1")),
        vec![("completed".to_string(), 3)]
    );
}

#[test]
fn sql_completed_then_bound_resend_is_23505_or_22023() {
    let (core, _g) = setup("pdml-bound-sql-then-bound", &three_rows());
    ok(
        &core,
        "alice",
        &format!("UPDATE docs SET n = 5 {SQL_ALL} USING OPERATION_ID 'x1' PARTITIONED CHUNK 2"),
    );
    // 同内容（チャンク幅は内容に含まれない）→ 重複。
    assert_eq!(code(bound_update_n(&core, "x1", "5", nz(1))), "23505");
    // 異内容 → 内容不一致。
    assert_eq!(code(bound_update_n(&core, "x1", "6", None)), "22023");
}

#[test]
fn bound_completed_then_sql_resend_is_23505() {
    let (core, _g) = setup("pdml-bound-bound-then-sql", &three_rows());
    assert_eq!(
        bound_delete(&core, "alice", "x2", None).expect("must succeed"),
        3
    );
    assert_eq!(
        sql_code(
            &core,
            "alice",
            &format!("DELETE FROM docs {SQL_ALL} USING OPERATION_ID 'x2' PARTITIONED"),
        ),
        "23505"
    );
}

#[test]
fn atomic_and_partitioned_bound_entries_do_not_share_a_hash_in_either_direction() {
    let (core, _g) = setup("pdml-bound-atomic-vs-part", &three_rows());
    // 原子的 → 分割実行。
    core.execute_bound_predicate_update_in_session(
        &ctx("alice"),
        "docs",
        Some(&op("m1")),
        |_schema: &TableSchema| Ok((set_n("7"), all_rows())),
    )
    .expect("atomic update");
    assert_eq!(code(bound_update_n(&core, "m1", "7", None)), "22023");
    // 分割実行 → 原子的。
    assert_eq!(
        bound_update_n(&core, "m2", "8", None).expect("must succeed"),
        3
    );
    let e = core
        .execute_bound_predicate_update_in_session(
            &ctx("alice"),
            "docs",
            Some(&op("m2")),
            |_schema: &TableSchema| Ok((set_n("8"), all_rows())),
        )
        .expect_err("must be rejected");
    assert_eq!(e.wire_code(), "22023");
}

// --- 再開 ------------------------------------------------------------------------

#[test]
fn sql_interrupted_job_resumes_through_the_bound_entry_and_vice_versa() {
    let (core, _g) = setup("pdml-bound-resume", &[(1, 10, "a"), (2, 20, "b")]);
    assert_eq!(interrupt_via_sql(&core, "r1").wire_code(), "VD001");
    // 原因を除去して bound 入口で再送 → 累計で完了。
    ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-1'",
    );
    assert_eq!(
        bound_update(&core, "alice", "r1", "dup", nz(1)).expect("must succeed"),
        2
    );
    assert_eq!(
        status_rows(&show(&core, "alice", "docs", "r1")),
        vec![("completed".to_string(), 2)]
    );

    // 逆方向: bound で中断 → SQL で再開。
    let (core, _g2) = setup("pdml-bound-resume-rev", &[(1, 10, "a"), (2, 20, "b")]);
    let e = bound_update(&core, "alice", "r2", "dup", nz(1)).unwrap_err();
    assert_eq!(e.wire_code(), "VD001");
    ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-2'",
    );
    ok(
        &core,
        "alice",
        &format!("UPDATE docs SET u = 'dup' {SQL_ALL} USING OPERATION_ID 'r2' PARTITIONED CHUNK 1"),
    );
    assert_eq!(
        status_rows(&show(&core, "alice", "docs", "r2")),
        vec![("completed".to_string(), 2)]
    );
}

// --- 判定順序 --------------------------------------------------------------------

#[test]
fn decision_order_is_ledger_then_chunk_then_table_then_predicate_form() {
    let (core, _g) = setup("pdml-bound-order", &three_rows());
    // 1. operation_id 無し → 23502（束縛 closure は呼ばれない）。
    let e = core
        .execute_bound_partitioned_delete_in_session(
            &ctx("alice"),
            "missing",
            None,
            nz(1_000_000_000),
            |_s: &TableSchema| panic!("bind must not run"),
        )
        .unwrap_err();
    assert_eq!(e.wire_code(), "23502");
    // 2. サーバー幅超過 → テーブル不存在より先に 22000。
    let e = core
        .execute_bound_partitioned_delete_in_session(
            &ctx("alice"),
            "missing",
            Some(&op("o1")),
            nz(usize::MAX),
            |_s: &TableSchema| panic!("bind must not run"),
        )
        .unwrap_err();
    assert_eq!(e.wire_code(), "22000");
    // 3. 未知テーブル → 42P01。
    let e = core
        .execute_bound_partitioned_delete_in_session(
            &ctx("alice"),
            "missing",
            Some(&op("o2")),
            None,
            |_s: &TableSchema| panic!("bind must not run"),
        )
        .unwrap_err();
    assert_eq!(e.wire_code(), "42P01");
    // 4. 非許可の述語形（空列）→ 42601。副作用なし。
    let e = core
        .execute_bound_partitioned_delete_in_session(
            &ctx("alice"),
            "docs",
            Some(&op("o3")),
            None,
            |_s: &TableSchema| Ok(Vec::new()),
        )
        .unwrap_err();
    assert_eq!(e.wire_code(), "42601");
    assert_eq!(count(&core, "alice"), 3);
}

// --- show / cancel ---------------------------------------------------------------

#[test]
fn show_and_cancel_not_found_cases_are_identical() {
    let (core, _g) = setup("pdml-bound-show-notfound", &[(1, 10, "a"), (2, 20, "b")]);
    seed(&core, "bob", &[(1, 1, "x"), (2, 2, "y")]);
    assert_eq!(interrupt_via_sql(&core, "shared").wire_code(), "VD001");

    let none_job = show(&core, "alice", "docs", "no-such-op");
    assert!(none_job.rows.is_empty());
    assert_eq!(none_job.columns.len(), 2);
    assert_eq!(show(&core, "alice", "no_such_table", "shared"), none_job);
    assert_eq!(show(&core, "carol", "docs", "shared"), none_job);
    assert_eq!(show(&core, "bob", "docs", "shared"), none_job);

    let none_cancel = cancel(&core, "alice", "docs", "no-such-op");
    assert_eq!(none_cancel, none_job);
    assert_eq!(cancel(&core, "alice", "no_such_table", "shared"), none_job);
    assert_eq!(cancel(&core, "carol", "docs", "shared"), none_job);
    // 他テナントの cancel は alice のジョブを変えない。
    assert_eq!(
        status_rows(&show(&core, "alice", "docs", "shared")),
        vec![("interrupted".to_string(), 1)]
    );
}

#[test]
fn invalid_table_identifier_is_42601_for_show_and_cancel() {
    let (core, _g) = setup("pdml-bound-show-ident", &three_rows());
    for bad in ["", "a b", "docs;", "1abc", "d\"x"] {
        let e = core
            .show_partitioned_dml_in_session(&ctx("alice"), bad, &op("i1"))
            .unwrap_err();
        assert_eq!(e.wire_code(), "42601", "show {bad:?}");
        let e = core
            .cancel_partitioned_dml_in_session(&ctx("alice"), bad, &op("i1"))
            .unwrap_err();
        assert_eq!(e.wire_code(), "42601", "cancel {bad:?}");
    }
}

#[test]
fn cancel_interrupted_job_then_resend_is_vd002() {
    let (core, _g) = setup("pdml-bound-cancel", &[(1, 10, "a"), (2, 20, "b")]);
    assert_eq!(interrupt_via_sql(&core, "c1").wire_code(), "VD001");
    assert_eq!(
        status_rows(&cancel(&core, "alice", "docs", "c1")),
        vec![("cancelled".to_string(), 1)]
    );
    let e = bound_update(&core, "alice", "c1", "dup", nz(1)).unwrap_err();
    assert_eq!(e.wire_code(), "VD002");
    assert!(
        matches!(e, SqlSurfaceError::PartitionedDmlCancelled { committed: 1 }),
        "{e:?}"
    );
    // 完了済みジョブへの cancel は completed のまま。
    assert_eq!(
        bound_delete(&core, "alice", "c2", None).expect("must succeed"),
        2
    );
    assert_eq!(
        status_rows(&cancel(&core, "alice", "docs", "c2")),
        vec![("completed".to_string(), 2)]
    );
}

// --- 台帳なし構成 ----------------------------------------------------------------

#[test]
fn bound_partitioned_dml_requires_the_operation_ledger() {
    let path = unique_db_path("pdml-bound-no-ledger");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
        .with_partitioned_dml_limits(PartitionedDmlLimits::default());
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)",
    )
    .expect("create table");
    ok(
        &core,
        "alice",
        "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')",
    );
    assert_eq!(code(bound_delete(&core, "alice", "nl", None)), "0A000");
    assert_eq!(count(&core, "alice"), 1);
}
