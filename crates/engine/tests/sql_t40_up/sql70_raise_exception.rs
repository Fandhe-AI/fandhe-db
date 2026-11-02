//! `RAISE` と `EXCEPTION` ブロック（SQL-70・Issue #2227）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-70・設計記録は
//! `docs/design/plpgsql-subset.md`「Issue #2227 追記」。期待値は PostgreSQL 17 の公開された挙動
//! （捕捉後の状態は巻き戻り、変数は巻き戻らない・`SQLSTATE`／`SQLERRM`・カテゴリ一致・`OTHERS` は
//! `57014` を捕捉しない）に合わせる。
//!
//! 実 `Storage` の production 経路（`execute_sql_in_session`／`execute_sql_in_txn`）で、autocommit・
//! 明示 Tx・関数・通知区間・巻き戻せない場合の fail-closed を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::notice::{NoticeLevel, StatementNotices};
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut s, "CREATE TABLE t (n INTEGER)")
        .expect("create table");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut s, sql)
}

fn err(core: &EngineCore, sql: &str) -> SqlSurfaceError {
    match run(core, sql) {
        Ok(o) => panic!("must fail: {sql} -> {o:?}"),
        Err(e) => e,
    }
}

fn rows(core: &EngineCore) -> Vec<i64> {
    let r = core
        .execute_sql(&ctx("alice"), "SELECT n FROM t ORDER BY n")
        .expect("select");
    r.rows
        .iter()
        .map(|row| match row.cells[0] {
            Cell::Integer(v) => i64::try_from(v).expect("fits"),
            Cell::SignedInteger(v) => v,
            ref o => panic!("unexpected {o:?}"),
        })
        .collect()
}

#[test]
fn caught_error_rolls_back_block_writes_in_autocommit_do() {
    let (core, _g) = setup("sql70-rollback");
    run(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             INSERT INTO t (n) VALUES (1); \
             RAISE EXCEPTION 'boom'; \
           EXCEPTION WHEN raise_exception THEN \
             INSERT INTO t (n) VALUES (2); \
           END; \
         END $$",
    )
    .expect("do");
    assert_eq!(rows(&core), vec![2]);
}

#[test]
fn variables_are_not_rolled_back() {
    let (core, _g) = setup("sql70-vars");
    run(
        &core,
        "DO $$ DECLARE x int := 0; BEGIN \
           BEGIN \
             x := 5; \
             RAISE EXCEPTION 'a'; \
           EXCEPTION WHEN OTHERS THEN \
             INSERT INTO t (n) VALUES (x); \
           END; \
         END $$",
    )
    .expect("do");
    assert_eq!(rows(&core), vec![5]);
}

#[test]
fn sqlstate_and_sqlerrm_are_visible_in_handler() {
    let (core, _g) = setup("sql70-diag");
    run(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             RAISE EXCEPTION 'dup %', 1 USING ERRCODE = '23505'; \
           EXCEPTION WHEN unique_violation THEN \
             IF sqlstate = '23505' THEN \
               IF sqlerrm = 'dup 1' THEN \
               INSERT INTO t (n) VALUES (10); \
               END IF; \
             END IF; \
           END; \
         END $$",
    )
    .expect("do");
    assert_eq!(rows(&core), vec![10]);
}

#[test]
fn category_condition_matches_whole_class() {
    let (core, _g) = setup("sql70-category");
    run(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             RAISE division_by_zero; \
           EXCEPTION WHEN data_exception THEN \
             INSERT INTO t (n) VALUES (3); \
           END; \
         END $$",
    )
    .expect("do");
    assert_eq!(rows(&core), vec![3]);
}

#[test]
fn unmatched_error_propagates_with_user_sqlstate_and_context() {
    let (core, _g) = setup("sql70-propagate");
    let e = err(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             RAISE EXCEPTION 'x' USING ERRCODE = '23505', DETAIL = 'd', HINT = 'h'; \
           EXCEPTION WHEN division_by_zero THEN NULL; \
           END; \
         END $$",
    );
    // 分類は静的な P0001、利用者の SQLSTATE は別に持つ（SQL-70 (2)）。
    assert_eq!(e.wire_code(), "P0001");
    assert_eq!(e.effective_sqlstate(), "23505");
    let r = e.raised().expect("raised");
    assert_eq!(r.message(), "x");
    assert_eq!(r.detail(), Some("d"));
    assert_eq!(r.hint(), Some("h"));
    let w = r.context().expect("context");
    assert!(
        w.contains("inline_code_block") && w.contains("at RAISE"),
        "{w}"
    );
}

#[test]
fn others_does_not_catch_query_canceled() {
    let (core, _g) = setup("sql70-others-57014");
    let e = err(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             RAISE EXCEPTION USING ERRCODE = '57014'; \
           EXCEPTION WHEN OTHERS THEN NULL; \
           END; \
         END $$",
    );
    assert_eq!(e.effective_sqlstate(), "57014");
    // 明示すれば捕捉できる。
    run(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             RAISE EXCEPTION USING ERRCODE = '57014'; \
           EXCEPTION WHEN query_canceled THEN NULL; \
           END; \
         END $$",
    )
    .expect("explicit query_canceled is caught");
}

#[test]
fn rethrow_propagates_the_original_error_and_outside_handler_is_rejected() {
    let (core, _g) = setup("sql70-rethrow");
    let e = err(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             RAISE EXCEPTION 'orig' USING ERRCODE = 'P0100'; \
           EXCEPTION WHEN OTHERS THEN \
             RAISE; \
           END; \
         END $$",
    );
    assert_eq!(e.effective_sqlstate(), "P0100");
    assert_eq!(e.raised().expect("raised").message(), "orig");
    // ハンドラの外の引数なし RAISE は定義時に拒否する。
    assert_eq!(err(&core, "DO $$ BEGIN RAISE; END $$").wire_code(), "42601");
}

#[test]
fn error_inside_handler_propagates_to_outer_handler() {
    let (core, _g) = setup("sql70-nested");
    run(
        &core,
        "DO $$ BEGIN \
           BEGIN \
             BEGIN \
               RAISE EXCEPTION 'inner'; \
             EXCEPTION WHEN OTHERS THEN \
               RAISE EXCEPTION 'from handler' USING ERRCODE = 'P0200'; \
             END; \
           EXCEPTION WHEN SQLSTATE 'P0200' THEN \
             INSERT INTO t (n) VALUES (4); \
           END; \
         END $$",
    )
    .expect("do");
    assert_eq!(rows(&core), vec![4]);
}

#[test]
fn block_entered_after_writes_cannot_be_rolled_back_and_fails_closed() {
    let (core, _g) = setup("sql70-unrestorable");
    let e = err(
        &core,
        "DO $$ BEGIN \
           INSERT INTO t (n) VALUES (1); \
           BEGIN \
             RAISE EXCEPTION 'x'; \
           EXCEPTION WHEN OTHERS THEN NULL; \
           END; \
         END $$",
    );
    // 巻き戻さずに捕捉する fail-open を作らず、固定文言の 0A000 で Tx ごと失敗する。
    assert_eq!(e.wire_code(), "0A000");
    assert!(rows(&core).is_empty());
}

#[test]
fn error_free_block_after_writes_still_runs() {
    let (core, _g) = setup("sql70-no-error");
    run(
        &core,
        "DO $$ BEGIN \
           INSERT INTO t (n) VALUES (1); \
           BEGIN \
             INSERT INTO t (n) VALUES (2); \
           EXCEPTION WHEN OTHERS THEN NULL; \
           END; \
         END $$",
    )
    .expect("do");
    assert_eq!(rows(&core), vec![1, 2]);
}

#[test]
fn explicit_transaction_and_user_savepoints_are_unaffected() {
    let (core, _g) = setup("sql70-explicit");
    let mut txn = core.new_session_transaction();
    let mut s = SessionState::default();
    let c = ctx("alice");
    for sql in ["BEGIN", "SAVEPOINT a"] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .expect(sql);
    }
    core.execute_sql_in_txn(
        &c,
        &mut s,
        &mut txn,
        "DO $$ BEGIN \
           BEGIN \
             INSERT INTO t (n) VALUES (1); \
             RAISE EXCEPTION 'x'; \
           EXCEPTION WHEN OTHERS THEN \
             INSERT INTO t (n) VALUES (2); \
           END; \
         END $$",
    )
    .expect("caught inside an explicit transaction");
    // 内部 savepoint は残らず、利用者の `SAVEPOINT a` へ戻れる。
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK TO a")
        .expect("rollback to user savepoint");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    assert!(rows(&core).is_empty());
}

#[test]
fn caught_error_in_function_and_not_failing_the_transaction() {
    let (core, _g) = setup("sql70-function");
    let mut sess = SessionState::default();
    sess.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut sess,
        "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ \
         BEGIN \
           BEGIN \
             RAISE EXCEPTION 'x'; \
           EXCEPTION WHEN OTHERS THEN \
             RETURN 7; \
           END; \
           RETURN 0; \
         END $$",
    )
    .expect("create function");
    let out = core
        .execute_sql_in_session(&ctx("alice"), &mut sess, "SELECT f()")
        .expect("select f()");
    let SqlOutcome::Query(r) = out else {
        panic!("expected rows");
    };
    match r.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(v, 7),
        Cell::SignedInteger(v) => assert_eq!(v, 7),
        ref o => panic!("unexpected {o:?}"),
    }
}

#[test]
fn notices_are_collected_per_statement_and_debug_log_are_dropped() {
    let (core, _g) = setup("sql70-notice");
    let scope = StatementNotices::enter();
    run(
        &core,
        "DO $$ BEGIN \
           RAISE NOTICE 'n % %%', 1; \
           RAISE WARNING 'w'; \
           RAISE INFO 'i' USING DETAIL = 'dd', HINT = 'hh'; \
           RAISE DEBUG 'd'; \
           RAISE LOG 'l'; \
         END $$",
    )
    .expect("do");
    let got = scope.finish();
    let view: Vec<(NoticeLevel, &str, &str)> = got
        .iter()
        .map(|n| (n.level, n.sqlstate.as_str(), n.message.as_str()))
        .collect();
    assert_eq!(
        view,
        vec![
            (NoticeLevel::Notice, "00000", "n 1 %"),
            (NoticeLevel::Warning, "01000", "w"),
            (NoticeLevel::Info, "00000", "i"),
        ]
    );
    assert_eq!(got[2].detail.as_deref(), Some("dd"));
    assert_eq!(got[2].hint.as_deref(), Some("hh"));
}

#[test]
fn notices_outside_a_scope_are_dropped_silently() {
    let (core, _g) = setup("sql70-no-scope");
    run(&core, "DO $$ BEGIN RAISE NOTICE 'x'; END $$").expect("do");
}

#[test]
fn definition_time_errors() {
    let (core, _g) = setup("sql70-syntax");
    for (sql, code) in [
        // `%` の数と引数の数の不一致。
        ("DO $$ BEGIN RAISE NOTICE 'a %'; END $$", "42601"),
        ("DO $$ BEGIN RAISE NOTICE 'a', 1; END $$", "42601"),
        // 実装定義クラス VD の指定・不正な SQLSTATE。
        (
            "DO $$ BEGIN RAISE EXCEPTION USING ERRCODE = 'VD001'; END $$",
            "42601",
        ),
        (
            "DO $$ BEGIN RAISE EXCEPTION USING ERRCODE = 'p0001'; END $$",
            "42704",
        ),
        // 未知の条件名。
        ("DO $$ BEGIN RAISE no_such_condition; END $$", "42704"),
        (
            "DO $$ BEGIN NULL; EXCEPTION WHEN no_such THEN NULL; END $$",
            "42704",
        ),
        // 対象外の USING 項目・ASSERT。
        (
            "DO $$ BEGIN RAISE EXCEPTION 'x' USING TABLE = 't'; END $$",
            "0A000",
        ),
        ("DO $$ BEGIN ASSERT true; END $$", "0A000"),
    ] {
        assert_eq!(err(&core, sql).wire_code(), code, "{sql}");
    }
}
