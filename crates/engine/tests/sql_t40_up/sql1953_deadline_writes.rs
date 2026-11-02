//! 書き込み・COPY・分割実行 DML への文単位デッドライン結線（Issue #1953。SQL-71・SQL-32 のポインタ）の
//! 結合テスト。
//!
//! `sql1951_statement_deadline` と同じ流儀で、既に過ぎた期限（`StatementDeadline::at(now)`）と
//! 1 時間先の期限を比べる。sleep を使わないので決定的に動く。確認する契約は次の 3 点。
//! - 期限切れなら `57014`（固定文言）で止まり、表の内容が変わらず、台帳にも記録が残らない
//!   （同じ `USING OPERATION_ID` を期限なしで再送すると成功する）。
//! - 明示トランザクションは `Failed`（`25P02`）になり、savepoint があれば `ROLLBACK TO` で復帰できる。
//! - COPY は開始時の期限を保持し、ガードが外れた後の commit でも `57014` になる。

use std::time::{Duration, Instant};

use engine::core::{CopyPlan, EngineCore};
use engine::interrupt::{DeadlineScope, StatementDeadline};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TIMEOUT_MSG: &str = "canceling statement due to statement timeout";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn expired() -> StatementDeadline {
    StatementDeadline::at(Instant::now())
}

fn far() -> StatementDeadline {
    StatementDeadline::after(Instant::now(), Duration::from_secs(3600))
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, k INTEGER, w INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx(), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO t (id, k, v) VALUES (1, 1, 10), (2, 2, 20), (3, 3, 30)",
    )
    .expect("seed t");
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO u (id, k, w) VALUES (1, 1, 100), (2, 2, 200)",
    )
    .expect("seed u");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
}

/// 表の全行を文字列化した断面（変化の有無の比較用）。
fn snapshot(core: &EngineCore, table: &str) -> String {
    match run(core, &format!("SELECT * FROM {table} ORDER BY id")).expect("snapshot") {
        SqlOutcome::Query(r) => format!("{:?}", r.rows),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 期限付きで文を実行して失敗を返す。
fn err_under(deadline: StatementDeadline, core: &EngineCore, sql: &str) -> SqlSurfaceError {
    let _scope = DeadlineScope::install(deadline);
    match run(core, sql) {
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
        Err(e) => e,
    }
}

/// SQL-71: 期限切れの autocommit 書き込みは `57014`・固定文言で止まり、表も台帳も変わらない。
#[test]
fn sql71_expired_deadline_leaves_no_trace_for_autocommit_writes() {
    let statements: &[&str] = &[
        // 単一行 INSERT も複数行と同じ契約（codex P1 指摘）。
        "INSERT INTO t (id, k, v) VALUES (9, 9, 9) USING OPERATION_ID 'w-ins1'",
        "INSERT INTO t (id, k, v) VALUES (10, 1, 1), (11, 2, 2) USING OPERATION_ID 'w-ins'",
        "UPDATE t SET v = v + 1 WHERE k > 0 USING OPERATION_ID 'w-upd'",
        "DELETE FROM t WHERE k > 0 USING OPERATION_ID 'w-del'",
        "INSERT INTO t (id, k, v) VALUES (1, 1, 99), (50, 5, 5) \
         ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'w-ups'",
        "TRUNCATE TABLE t USING OPERATION_ID 'w-trunc'",
    ];
    for sql in statements {
        let (core, _g) = open("sql1953-writes");
        let before = snapshot(&core, "t");
        let e = err_under(expired(), &core, sql);
        assert_eq!(e.wire_code(), "57014", "{sql}");
        assert_eq!(e.to_string(), TIMEOUT_MSG, "{sql}");
        assert_eq!(snapshot(&core, "t"), before, "table changed by {sql}");
        // 台帳に記録が残っていなければ、同じ operation_id の再送が期限なしで成功する（`23505` にならない）。
        run(&core, sql).unwrap_or_else(|e| panic!("resend must succeed for {sql}: {e:?}"));
        assert_ne!(snapshot(&core, "t"), before, "resend must apply {sql}");
    }
}

/// SQL-71: 十分先の期限なら期限なしと同じ結果になる。
#[test]
fn sql71_far_deadline_does_not_change_write_behavior() {
    let (core, _g) = open("sql1953-far");
    let _scope = DeadlineScope::install(far());
    run(
        &core,
        "UPDATE t SET v = v + 1 WHERE k > 0 USING OPERATION_ID 'f-upd'",
    )
    .expect("update");
    run(
        &core,
        "INSERT INTO t (id, k, v) VALUES (10, 1, 1), (11, 2, 2) USING OPERATION_ID 'f-ins'",
    )
    .expect("insert");
    run(
        &core,
        "DELETE FROM t WHERE k > 2 USING OPERATION_ID 'f-del'",
    )
    .expect("delete");
    match run(&core, "SELECT id FROM t ORDER BY id").expect("select") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 4),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// SQL-71・SQL-32: 明示トランザクション内の期限切れは `Failed`（`25P02`）にし、savepoint まで戻せる。
/// savepoint 後に書いた分は期限切れの文の分も含めて取り消され、台帳にも残らない。
#[test]
fn sql71_expired_write_in_explicit_transaction_recovers_with_rollback_to() {
    let (core, _g) = open("sql1953-txn");
    let before = snapshot(&core, "t");
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut exec =
        |_c: &EngineCore, sql: &str| core.execute_sql_in_txn(&caller, &mut session, &mut txn, sql);
    exec(&core, "BEGIN").expect("begin");
    exec(&core, "SAVEPOINT s").expect("savepoint");
    exec(
        &core,
        "INSERT INTO t (id, k, v) VALUES (20, 9, 9) USING OPERATION_ID 'x-ins'",
    )
    .expect("insert after savepoint");
    {
        let _scope = DeadlineScope::install(expired());
        let err = exec(
            &core,
            "UPDATE t SET v = v + 1 WHERE k > 0 USING OPERATION_ID 'x-upd'",
        )
        .expect_err("timed out");
        assert_eq!(err.wire_code(), "57014");
        assert_eq!(err.to_string(), TIMEOUT_MSG);
    }
    let err = exec(&core, "SELECT * FROM t").expect_err("failed transaction");
    assert_eq!(err.wire_code(), "25P02");
    exec(&core, "ROLLBACK TO s").expect("rollback to savepoint");
    // 巻き戻した operation_id は同じ Tx 内で再送できる（台帳に残っていない）。
    exec(
        &core,
        "UPDATE t SET v = v + 1 WHERE k > 0 USING OPERATION_ID 'x-upd'",
    )
    .expect("resend after recovery");
    exec(&core, "COMMIT").expect("commit");
    // savepoint 後の INSERT は取り消され、復帰後の UPDATE だけが残る。
    let after = snapshot(&core, "t");
    assert_ne!(after, before);
    assert!(
        !after.contains("20"),
        "INSERT must have been undone: {after}"
    );
}

/// SQL-71: savepoint なしで `ROLLBACK` すれば全部巻き戻る。
#[test]
fn sql71_expired_write_in_explicit_transaction_rolls_back_everything() {
    let (core, _g) = open("sql1953-txn-all");
    let before = snapshot(&core, "t");
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "INSERT INTO t (id, k, v) VALUES (20, 9, 9) USING OPERATION_ID 'y-ins'",
    )
    .expect("insert");
    {
        let _scope = DeadlineScope::install(expired());
        let err = core
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                "DELETE FROM t WHERE k > 0 USING OPERATION_ID 'y-del'",
            )
            .expect_err("timed out");
        assert_eq!(err.wire_code(), "57014");
    }
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(snapshot(&core, "t"), before);
}

fn copy_batch(
    core: &EngineCore,
    op: &str,
) -> Result<engine::sql::copy::CopyInBatch, SqlSurfaceError> {
    let plan = core.begin_copy(
        &ctx(),
        &SessionState::default(),
        &format!("COPY t (id, k, v) FROM STDIN USING OPERATION_ID '{op}'"),
    )?;
    let mut cs = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM plan"),
    };
    cs.feed(b"30\t1\t1\n31\t2\t2\n")?;
    cs.finish()
}

/// SQL-71: COPY は開始時の期限を保持する。`begin_copy` の間だけ期限を入れ、ガードを外した後の
/// commit でも `57014` になり、表は変わらない。
#[test]
fn sql71_copy_keeps_the_deadline_captured_at_begin() {
    let (core, _g) = open("sql1953-copy");
    let before = snapshot(&core, "t");
    let begun = {
        let _scope = DeadlineScope::install(expired());
        // 開始時点で期限切れでも `begin_copy` 自体は通る（確認点は feed 以降）。
        core.begin_copy(
            &ctx(),
            &SessionState::default(),
            "COPY t (id, k, v) FROM STDIN USING OPERATION_ID 'c-1'",
        )
        .expect("begin")
    };
    let mut cs = match begun {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM plan"),
    };
    let err = cs.feed(b"30\t1\t1\n").expect_err("feed after the deadline");
    assert_eq!(err.wire_code(), "57014");
    assert_eq!(err.to_string(), TIMEOUT_MSG);
    assert_eq!(snapshot(&core, "t"), before);
}

/// SQL-71: 取り込みを済ませた後に（commit を呼ぶスレッドの）期限が切れていれば、commit 段（行ループ）で
/// `57014` になる。取り込み時に期限が無いバッチでも、commit 呼び出し時点の期限が効く（厳しい側が勝つ）。
#[test]
fn sql71_copy_commit_stops_when_the_deadline_has_passed_at_commit() {
    let (core, _g) = open("sql1953-copy-commit");
    let before = snapshot(&core, "t");
    let batch = copy_batch(&core, "c-2").expect("ingest without a deadline");
    let err = {
        let _scope = DeadlineScope::install(expired());
        core.commit_copy_in(&ctx(), batch)
            .expect_err("deadline passed at commit")
    };
    assert_eq!(err.wire_code(), "57014");
    assert_eq!(err.to_string(), TIMEOUT_MSG);
    assert_eq!(snapshot(&core, "t"), before);
    // 同じ operation_id で期限なしの COPY をやり直せる（台帳に残っていない）。
    let batch = copy_batch(&core, "c-2").expect("ingest");
    core.commit_copy_in(&ctx(), batch).expect("commit");
    assert_ne!(snapshot(&core, "t"), before);
}

/// SQL-71: 1 行だけの COPY（単一行の書き込み経路へ委譲される）も、期限切れなら `57014` で止まる。
#[test]
fn sql71_single_row_copy_commit_stops_when_the_deadline_has_passed() {
    let (core, _g) = open("sql1953-copy-one");
    let before = snapshot(&core, "t");
    let plan = core
        .begin_copy(
            &ctx(),
            &SessionState::default(),
            "COPY t (id, k, v) FROM STDIN USING OPERATION_ID 'c-one'",
        )
        .expect("begin");
    let mut cs = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM plan"),
    };
    cs.feed(b"30\t1\t1\n").expect("feed");
    let batch = cs.finish().expect("finish");
    let err = {
        let _scope = DeadlineScope::install(expired());
        core.commit_copy_in(&ctx(), batch)
            .expect_err("deadline passed at commit")
    };
    assert_eq!(err.wire_code(), "57014");
    assert_eq!(snapshot(&core, "t"), before);
}

/// SQL-71: 十分先の期限なら COPY は正常に取り込まれる。
#[test]
fn sql71_copy_with_far_deadline_succeeds() {
    let (core, _g) = open("sql1953-copy-far");
    let before = snapshot(&core, "t");
    let _scope = DeadlineScope::install(far());
    let batch = copy_batch(&core, "c-3").expect("ingest");
    core.commit_copy_in(&ctx(), batch).expect("commit");
    assert_ne!(snapshot(&core, "t"), before);
}

/// SQL-32・SQL-71 (4): 分割実行 DML は最初の commit 前の期限切れで `57014`（副作用ゼロ）。
/// 同じ operation_id を期限なしで再送すると完走する。
#[test]
fn sql32_partitioned_dml_with_expired_deadline_changes_nothing() {
    let (core, _g) = open("sql1953-pdml");
    let before = snapshot(&core, "t");
    for sql in [
        "UPDATE t SET v = v + 1 WHERE k > 0 USING OPERATION_ID 'p-upd' PARTITIONED CHUNK 1",
        "DELETE FROM t WHERE k > 0 USING OPERATION_ID 'p-del' PARTITIONED CHUNK 1",
    ] {
        let e = err_under(expired(), &core, sql);
        assert_eq!(e.wire_code(), "57014", "{sql}");
        assert_eq!(e.to_string(), TIMEOUT_MSG, "{sql}");
        assert_eq!(snapshot(&core, "t"), before, "{sql}");
        run(&core, sql).unwrap_or_else(|e| panic!("resend must succeed for {sql}: {e:?}"));
        assert_ne!(snapshot(&core, "t"), before, "{sql}");
        // 次のケースのため元の行を戻す。
        run(&core, "TRUNCATE TABLE t USING OPERATION_ID 'p-reset-a'").ok();
        core.execute_insert_sql(
            &ctx(),
            "INSERT INTO t (id, k, v) VALUES (1, 1, 10), (2, 2, 20), (3, 3, 30)",
        )
        .ok();
    }
}
