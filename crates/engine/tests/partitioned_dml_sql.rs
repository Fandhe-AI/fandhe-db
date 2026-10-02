//! 分割実行 DML の SQL 表層（`UPDATE`／`DELETE ... USING OPERATION_ID '<id>' PARTITIONED
//! [CHUNK n]`・`SHOW`／`CANCEL PARTITIONED DML`・`EXPLAIN`）の結合テスト（Issue #1129。
//! ポインタ: ADR `docs/design/partitioned-dml.md`、SQL-19、RECOVER-11・RECOVER-12、
//! RLS-9・RLS-10、ERR-1・ERR-2・ERR-4）。
//!
//! `EngineCore::execute_sql_in_session`／`execute_sql_in_txn` を production 経路として検証する
//! （実 `Storage`＋`CpuScalarProvider`）。部分完了は「UNIQUE 列へ同じ値を `CHUNK 1` で
//! 書き込む」ことで 2 チャンク目に確定的な `23505` を起こして作る（時間に依存しない）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::parser::{DmlLimits, PartitionedDmlLimits};
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("sys"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Delete(o) => o.rows_affected,
        SqlOutcome::Update(o) => o.rows_affected,
        other => panic!("expected Delete/Update outcome, got {other:?}"),
    }
}

/// `SHOW`／`CANCEL` の結果を `(status, rows)` の列へ直す。
fn status_rows(outcome: SqlOutcome) -> Vec<(String, i64)> {
    match outcome {
        SqlOutcome::Query(q) => q
            .rows
            .iter()
            .map(|r| match (&r.cells[0], &r.cells[1]) {
                (Cell::Text(s), Cell::SignedInteger(n)) => (s.clone(), *n),
                other => panic!("unexpected cells {other:?}"),
            })
            .collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn show(core: &EngineCore, tenant: &str, table: &str, id: &str) -> Vec<(String, i64)> {
    status_rows(ok(
        core,
        tenant,
        &format!("SHOW PARTITIONED DML '{id}' ON {table}"),
    ))
}

fn setup(label: &str, rows: &[(u64, i64, &str)]) -> (EngineCore, CleanupGuard) {
    let (core, g) = new_core(label);
    ddl(&core, "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)");
    seed(&core, "alice", rows);
    (core, g)
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
    query(ok(core, tenant, "SELECT id FROM docs LIMIT 1000"))
        .rows
        .len()
}

fn five_rows() -> Vec<(u64, i64, &'static str)> {
    vec![
        (1, 10, "a"),
        (2, 20, "b"),
        (3, 30, "c"),
        (4, 40, "d"),
        (5, 50, "e"),
    ]
}

// --- 完了・再送 ---------------------------------------------------------------

#[test]
fn delete_completes_with_cumulative_count_and_resend_is_rejected() {
    let (core, _g) = setup("pdml-sql-complete", &five_rows());
    let sql = "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'p1' PARTITIONED CHUNK 2";
    assert_eq!(affected(ok(&core, "alice", sql)), 5);
    assert_eq!(count(&core, "alice"), 0);
    // 完了済みの再送は 23505、述語を変えると 22023。
    assert_eq!(code(&core, "alice", sql), "23505");
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 1 USING OPERATION_ID 'p1' PARTITIONED CHUNK 2"
        ),
        "22023"
    );
    // チャンク幅は内容照合の入力に含まれない（幅を変えた再送も同一視される）。
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'p1' PARTITIONED CHUNK 1"
        ),
        "23505"
    );
    // 同じ operation_id を原子実行へ使い回すと 22023。
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'p1'"
        ),
        "22023"
    );
    assert_eq!(
        show(&core, "alice", "docs", "p1"),
        vec![("completed".to_string(), 5)]
    );
}

#[test]
fn update_completes_with_cumulative_count() {
    let (core, _g) = setup("pdml-sql-update", &five_rows());
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'x' WHERE n > 100 USING OPERATION_ID 'u0' PARTITIONED",
    ));
    assert_eq!(n, 0);
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET n = 7 WHERE n > 20 USING OPERATION_ID 'u1' PARTITIONED CHUNK 1",
    ));
    assert_eq!(n, 3);
    assert_eq!(
        query(ok(
            &core,
            "alice",
            "SELECT id FROM docs WHERE n = 7 LIMIT 100"
        ))
        .rows
        .len(),
        3
    );
}

// --- 部分完了・再開 -------------------------------------------------------------

/// 2 行（id 1・2）に対し UNIQUE 列へ同じ値を `CHUNK 1` で書き、2 チャンク目で `23505` に
/// する。1 チャンク目（id 1）は commit 済みで `VD001` になる。
fn make_interrupted(core: &EngineCore, tenant: &str, op: &str) -> SqlSurfaceError {
    err(
        core,
        tenant,
        &format!(
            "UPDATE docs SET u = 'dup' WHERE n > 0 USING OPERATION_ID '{op}' PARTITIONED CHUNK 1"
        ),
    )
}

#[test]
fn partial_completion_reports_vd001_and_resend_resumes_to_cumulative_total() {
    let (core, _g) = setup("pdml-sql-partial", &[(1, 10, "a"), (2, 20, "b")]);
    let e = make_interrupted(&core, "alice", "op-int");
    assert_eq!(e.wire_code(), "VD001");
    let msg = e.to_string();
    assert!(msg.contains("committed 1 rows"), "{msg}");
    assert!(msg.contains("cause 23505"), "{msg}");
    assert!(msg.contains("operation_id 'op-int'"), "{msg}");
    assert_eq!(
        show(&core, "alice", "docs", "op-int"),
        vec![("interrupted".to_string(), 1)]
    );

    // 原因を取り除いて同じ文を再送すると、カーソルから再開して累計件数で完了する。
    ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-1'",
    );
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'dup' WHERE n > 0 USING OPERATION_ID 'op-int' PARTITIONED CHUNK 1",
    ));
    assert_eq!(n, 2);
    assert_eq!(
        show(&core, "alice", "docs", "op-int"),
        vec![("completed".to_string(), 2)]
    );
}

#[test]
fn failure_before_any_commit_keeps_the_cause_code_and_leaves_no_record() {
    let (core, _g) = setup("pdml-sql-zero", &[(1, 10, "a"), (2, 20, "b")]);
    // CHUNK 2 なので両行が同じチャンクに入り、最初のチャンクで 23505。
    let sql =
        "UPDATE docs SET u = 'dup' WHERE n > 0 USING OPERATION_ID 'op-zero' PARTITIONED CHUNK 2";
    assert_eq!(code(&core, "alice", sql), "23505");
    assert!(show(&core, "alice", "docs", "op-zero").is_empty());
    // 記録が残らないので、同じ operation_id を別の文で使える。
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'ok' WHERE id = 1 USING OPERATION_ID 'op-zero'",
    ));
    assert_eq!(n, 1);
}

#[test]
fn affected_rows_limit_is_54000_before_commit_and_vd001_after() {
    let path = unique_db_path("pdml-sql-limit");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_dml_limits(DmlLimits {
            max_affected_rows: std::num::NonZeroUsize::new(3),
            ..DmlLimits::default()
        });
    ddl(&core, "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)");
    seed(&core, "alice", &five_rows());
    // 2 チャンク目（累計 4 件 > 3）で超過: 1 チャンク目は commit 済みなので VD001（原因 54000）。
    let e = err(
        &core,
        "alice",
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'lim-a' PARTITIONED CHUNK 2",
    );
    assert_eq!(e.wire_code(), "VD001");
    assert!(e.to_string().contains("cause 54000"), "{e}");
    assert_eq!(count(&core, "alice"), 3);
    // 最初のチャンクで超過（1 チャンク 4 件 > 3）: 副作用なしの 54000。
    seed(&core, "bob", &five_rows());
    assert_eq!(
        code(
            &core,
            "bob",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'lim-b' PARTITIONED CHUNK 5"
        ),
        "54000"
    );
    assert_eq!(count(&core, "bob"), 5);
    assert!(show(&core, "bob", "docs", "lim-b").is_empty());
}

// --- トランザクション内の拒否 ---------------------------------------------------

#[test]
fn partitioned_dml_is_rejected_inside_an_explicit_transaction() {
    let (core, _g) = setup("pdml-sql-txn", &five_rows());
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut run_tx = |sql: &str| core.execute_sql_in_txn(&c, &mut session, &mut txn, sql);

    run_tx("BEGIN").expect("begin");
    let e = run_tx("DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'tx1' PARTITIONED")
        .expect_err("partitioned in txn");
    assert_eq!(e.wire_code(), "25001");
    let msg = e.to_string();
    assert_eq!(msg, "partitioned DML cannot run inside a transaction block");
    // 再 BEGIN の文言とは区別できる。
    assert!(!msg.contains("transaction already in progress"));
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(count(&core, "alice"), 5);
}

#[test]
fn cancel_is_rejected_in_a_transaction_but_show_and_explain_are_allowed() {
    let (core, _g) = setup("pdml-sql-txn2", &five_rows());
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut run_tx = |sql: &str| core.execute_sql_in_txn(&c, &mut session, &mut txn, sql);
    run_tx("BEGIN").expect("begin");
    // SHOW・EXPLAIN は読み取りのみなのでトランザクション内でも成功し、状態は壊れない。
    let shown = run_tx("SHOW PARTITIONED DML 'nope' ON docs").expect("show in txn");
    assert!(status_rows(shown).is_empty());
    run_tx("EXPLAIN DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'ex1' PARTITIONED CHUNK 2")
        .expect("explain in txn");
    let e = run_tx("CANCEL PARTITIONED DML 'x' ON docs").expect_err("cancel in txn");
    assert_eq!(e.wire_code(), "25001");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

// --- 取り消し ------------------------------------------------------------------

#[test]
fn cancel_interrupted_job_then_resend_is_vd002_and_show_reports_cancelled() {
    let (core, _g) = setup("pdml-sql-cancel", &[(1, 10, "a"), (2, 20, "b")]);
    assert_eq!(
        make_interrupted(&core, "alice", "op-c").wire_code(),
        "VD001"
    );
    let out = status_rows(ok(&core, "alice", "CANCEL PARTITIONED DML 'op-c' ON docs"));
    assert_eq!(out, vec![("cancelled".to_string(), 1)]);
    assert_eq!(
        show(&core, "alice", "docs", "op-c"),
        vec![("cancelled".to_string(), 1)]
    );
    let e = make_interrupted(&core, "alice", "op-c");
    assert_eq!(e.wire_code(), "VD002");
    assert!(e.to_string().contains("committed 1 rows"), "{e}");
    // 取り消しは commit 済みチャンクを戻さない。
    assert_eq!(
        query(ok(
            &core,
            "alice",
            "SELECT id FROM docs WHERE u = 'dup' LIMIT 10"
        ))
        .rows
        .len(),
        1
    );
    // 取り消し済みへの CANCEL は変化しない。
    let again = status_rows(ok(&core, "alice", "CANCEL PARTITIONED DML 'op-c' ON docs"));
    assert_eq!(again, vec![("cancelled".to_string(), 1)]);
}

#[test]
fn cancel_of_completed_job_leaves_it_completed() {
    let (core, _g) = setup("pdml-sql-cancel-done", &five_rows());
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'done' PARTITIONED CHUNK 2"
        )),
        5
    );
    let out = status_rows(ok(&core, "alice", "CANCEL PARTITIONED DML 'done' ON docs"));
    assert_eq!(out, vec![("completed".to_string(), 5)]);
    assert_eq!(
        show(&core, "alice", "docs", "done"),
        vec![("completed".to_string(), 5)]
    );
}

// --- 該当なしの同一性・テナント分離 ----------------------------------------------

#[test]
fn not_found_cases_are_indistinguishable_for_show_and_cancel() {
    let (core, _g) = setup("pdml-sql-indist", &[(1, 10, "a"), (2, 20, "b")]);
    seed(&core, "bob", &[(1, 1, "x"), (2, 2, "y")]);
    assert_eq!(
        make_interrupted(&core, "bob", "shared-op").wire_code(),
        "VD001"
    );
    // alice だけが同名のジョブを持つ。
    assert_eq!(
        make_interrupted(&core, "alice", "shared-op").wire_code(),
        "VD001"
    );

    for verb in ["SHOW", "CANCEL"] {
        let q = |tenant: &str, table: &str, id: &str| {
            query(ok(
                &core,
                tenant,
                &format!("{verb} PARTITIONED DML '{id}' ON {table}"),
            ))
        };
        // ジョブなし・テーブルなし・他テナント（carol はジョブを持たない）はすべて同一。
        let none_job = q("alice", "docs", "no-such-op");
        let none_table = q("alice", "no_such_table", "shared-op");
        let other_tenant = q("carol", "docs", "shared-op");
        assert!(none_job.rows.is_empty());
        assert_eq!(none_job, none_table, "{verb}");
        assert_eq!(none_job, other_tenant, "{verb}");
        // 2 列（status・rows）の列定義も同一。
        assert_eq!(none_job.columns.len(), 2);
    }
    // carol の CANCEL は alice のジョブへ影響しない。
    assert_eq!(
        show(&core, "alice", "docs", "shared-op"),
        vec![("interrupted".to_string(), 1)]
    );
}

#[test]
fn show_and_cancel_reject_invalid_identifiers_and_empty_ids() {
    let (core, _g) = setup("pdml-sql-ident", &five_rows());
    for verb in ["SHOW", "CANCEL"] {
        assert_eq!(
            code(
                &core,
                "alice",
                &format!("{verb} PARTITIONED DML 'x' ON \"d;docs\"")
            ),
            "42601"
        );
        assert_eq!(
            code(
                &core,
                "alice",
                &format!("{verb} PARTITIONED DML '' ON docs")
            ),
            "23502"
        );
        assert_eq!(
            code(&core, "alice", &format!("{verb} PARTITIONED DML 'x'")),
            "42601"
        );
        assert_eq!(
            code(
                &core,
                "alice",
                &format!("{verb} PARTITIONED DML 'x' ON docs extra")
            ),
            "42601"
        );
        let long = "x".repeat(300);
        assert_eq!(
            code(
                &core,
                "alice",
                &format!("{verb} PARTITIONED DML '{long}' ON docs")
            ),
            "22000"
        );
    }
}

#[test]
fn tenant_rows_and_jobs_are_isolated() {
    let (core, _g) = setup("pdml-sql-tenant", &five_rows());
    seed(&core, "bob", &five_rows());
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'iso' PARTITIONED CHUNK 2"
        )),
        5
    );
    assert_eq!(count(&core, "alice"), 0);
    assert_eq!(count(&core, "bob"), 5);
    assert!(show(&core, "bob", "docs", "iso").is_empty());
    // bob は同じ operation_id を別ジョブとして使える。
    assert_eq!(
        affected(ok(
            &core,
            "bob",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'iso' PARTITIONED CHUNK 2"
        )),
        5
    );
}

// --- 中断記録数の上限 -------------------------------------------------------------

#[test]
fn interrupted_record_limit_blocks_new_jobs_but_not_resumes() {
    let path = unique_db_path("pdml-sql-int-limit");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_partitioned_dml_limits(PartitionedDmlLimits {
            interrupted_record_limit: std::num::NonZeroU64::MIN,
            ..PartitionedDmlLimits::default()
        });
    ddl(&core, "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)");
    seed(&core, "alice", &[(1, 10, "a"), (2, 20, "b")]);
    assert_eq!(
        make_interrupted(&core, "alice", "int-a").wire_code(),
        "VD001"
    );
    // 別の operation_id の新しいジョブは 54000（副作用なし）。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = 99 WHERE n > 0 USING OPERATION_ID 'int-b' PARTITIONED"
        ),
        "54000"
    );
    assert!(show(&core, "alice", "docs", "int-b").is_empty());
    // 既存の中断ジョブの再送は上限の対象外（再度同じ原因で止まるが VD001）。
    assert_eq!(
        make_interrupted(&core, "alice", "int-a").wire_code(),
        "VD001"
    );
    // 取り消し済みは数えない。
    ok(&core, "alice", "CANCEL PARTITIONED DML 'int-a' ON docs");
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE docs SET n = 99 WHERE n > 0 USING OPERATION_ID 'int-b' PARTITIONED"
        )),
        2
    );
}

// --- EXPLAIN ----------------------------------------------------------------------

#[test]
fn explain_partitioned_reports_the_mode_and_has_no_side_effects() {
    let (core, _g) = setup("pdml-sql-explain", &five_rows());
    let out = ok(
        &core,
        "alice",
        "EXPLAIN DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'ex' PARTITIONED CHUNK 2",
    );
    let lines: Vec<String> = match out {
        SqlOutcome::Explain(q) => q
            .rows
            .iter()
            .map(|r| match &r.cells[0] {
                Cell::Text(t) => t.clone(),
                other => panic!("unexpected cell {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain outcome, got {other:?}"),
    };
    assert_eq!(lines[0], "execution: partitioned (non-atomic)");
    assert!(lines.contains(&"statement: delete".to_string()));
    assert!(lines.contains(&"chunk_rows: 2".to_string()));
    assert_eq!(count(&core, "alice"), 5);
    assert!(show(&core, "alice", "docs", "ex").is_empty());
    // UPDATE も同様。
    let out = ok(
        &core,
        "alice",
        "EXPLAIN UPDATE docs SET n = 1 WHERE n > 0 USING OPERATION_ID 'ex2' PARTITIONED",
    );
    assert!(matches!(out, SqlOutcome::Explain(_)));
    // 分割実行でない DML の EXPLAIN は従来どおり 42601。
    assert_eq!(
        code(
            &core,
            "alice",
            "EXPLAIN DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'ex3'"
        ),
        "42601"
    );
}

// --- 構文の受理・拒否 ---------------------------------------------------------------

#[test]
fn syntax_rejections() {
    let (core, _g) = setup("pdml-sql-syntax", &five_rows());
    let reject = |sql: &str, expected: &str| {
        assert_eq!(code(&core, "alice", sql), expected, "{sql}");
        // どの拒否も副作用を残さない。
        assert_eq!(count(&core, "alice"), 5, "{sql}");
    };
    // 単一行形・RETURNING・INSERT・TRUNCATE との併用は 42601。
    reject(
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'a' PARTITIONED",
        "42601",
    );
    reject(
        "UPDATE docs SET n = 1 WHERE id = 1 USING OPERATION_ID 'a' PARTITIONED",
        "42601",
    );
    reject(
        "DELETE FROM docs WHERE n > 0 RETURNING id USING OPERATION_ID 'a' PARTITIONED",
        "42601",
    );
    reject(
        "UPDATE docs SET n = 1 WHERE n > 0 RETURNING id USING OPERATION_ID 'a' PARTITIONED",
        "42601",
    );
    reject(
        "INSERT INTO docs (id, n, u) VALUES (9, 9, 'z') USING OPERATION_ID 'a' PARTITIONED",
        "42601",
    );
    reject(
        "INSERT INTO docs (id, n, u) VALUES (9, 9, 'z') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'a' PARTITIONED",
        "42601",
    );
    reject(
        "TRUNCATE TABLE docs USING OPERATION_ID 'a' PARTITIONED",
        "42601",
    );
    // CHUNK の値の検査。
    reject(
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED CHUNK 0",
        "22000",
    );
    reject(
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED CHUNK 99999999999999999999999",
        "22000",
    );
    reject(
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED CHUNK 1000001",
        "22000",
    );
    reject(
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED CHUNK 'x'",
        "42601",
    );
    reject(
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED CHUNK",
        "42601",
    );
    // 句の重複・位置違い。
    reject(
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED PARTITIONED",
        "42601",
    );
    reject(
        "DELETE FROM docs WHERE n > 0 PARTITIONED USING OPERATION_ID 'a'",
        "42601",
    );
    reject(
        "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' CHUNK 2 PARTITIONED",
        "42601",
    );
    // operation_id が無ければ 23502（台帳あり構成）。
    let c = code(&core, "alice", "DELETE FROM docs WHERE n > 0 PARTITIONED");
    assert_eq!(c, "23502");
    assert_eq!(count(&core, "alice"), 5);
}

#[test]
fn chunk_above_server_width_is_rejected_and_smaller_is_accepted() {
    let path = unique_db_path("pdml-sql-chunk-cap");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_partitioned_dml_limits(PartitionedDmlLimits {
            chunk_rows: std::num::NonZeroUsize::new(3).expect("non-zero"),
            ..PartitionedDmlLimits::default()
        });
    ddl(&core, "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)");
    seed(&core, "alice", &five_rows());
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED CHUNK 4"
        ),
        "22000"
    );
    assert_eq!(count(&core, "alice"), 5);
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED CHUNK 3"
        )),
        5
    );
}

#[test]
fn partitioned_dml_requires_the_operation_ledger() {
    let path = unique_db_path("pdml-sql-no-ledger");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::CompareOnlyWithoutLedger);
    ddl(&core, "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)");
    seed_without_ledger(&core);
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID 'a' PARTITIONED"
        ),
        "0A000"
    );
    assert_eq!(count(&core, "alice"), 1);
}

fn seed_without_ledger(core: &EngineCore) {
    ok(
        core,
        "alice",
        "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')",
    );
}

// --- 再起動後の「中断」扱い（Issue #1131。RECOVER-11・RECOVER-12） ---------------------

/// 開き直し（プロセス再起動の模擬。登録簿は空になる）。
fn reopen(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("reopen storage"),
        Box::new(CpuScalarProvider),
    )
}

/// 再起動すると `Running` 記録は SQL 層でも `interrupted` として見え、再送で累計件数のまま
/// 完了し、完了後に開き直しても `completed` が保たれる。
#[test]
fn interrupted_job_is_reported_interrupted_after_reopen_and_resumes() {
    let path = unique_db_path("pdml-sql-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = reopen(&path);
        ddl(&core, "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)");
        seed(&core, "alice", &[(1, 10, "a"), (2, 20, "b")]);
        assert_eq!(
            make_interrupted(&core, "alice", "op-re").wire_code(),
            "VD001"
        );
        assert_eq!(
            show(&core, "alice", "docs", "op-re"),
            vec![("interrupted".to_string(), 1)]
        );
    }
    let core = reopen(&path);
    assert_eq!(
        show(&core, "alice", "docs", "op-re"),
        vec![("interrupted".to_string(), 1)]
    );
    // 他テナントは存在を観測できない。
    assert!(show(&core, "bob", "docs", "op-re").is_empty());

    // 原因（衝突する UNIQUE 値）を取り除いて再送すると、累計件数で完了する。
    ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-1'",
    );
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET u = 'dup' WHERE n > 0 USING OPERATION_ID 'op-re' PARTITIONED CHUNK 1",
    ));
    assert_eq!(n, 2);
    drop(core);
    let core = reopen(&path);
    assert_eq!(
        show(&core, "alice", "docs", "op-re"),
        vec![("completed".to_string(), 2)]
    );
}

/// 再起動後の中断ジョブは `CANCEL` で `cancelled` になり、再送は `VD002` で拒否される。
#[test]
fn interrupted_job_can_be_cancelled_after_reopen() {
    let path = unique_db_path("pdml-sql-reopen-cancel");
    let _g = CleanupGuard(path.clone());
    {
        let core = reopen(&path);
        ddl(&core, "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE)");
        seed(&core, "alice", &[(1, 10, "a"), (2, 20, "b")]);
        assert_eq!(
            make_interrupted(&core, "alice", "op-rc").wire_code(),
            "VD001"
        );
    }
    let core = reopen(&path);
    let out = status_rows(ok(&core, "alice", "CANCEL PARTITIONED DML 'op-rc' ON docs"));
    assert_eq!(out, vec![("cancelled".to_string(), 1)]);
    assert_eq!(
        show(&core, "alice", "docs", "op-rc"),
        vec![("cancelled".to_string(), 1)]
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET u = 'dup' WHERE n > 0 USING OPERATION_ID 'op-rc' PARTITIONED CHUNK 1"
        ),
        "VD002"
    );
}
