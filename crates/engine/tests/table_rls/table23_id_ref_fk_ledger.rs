//! 採番方式の親（`id` は宣言した普通の列）への `id` 参照 FK の書き込みを `USING OPERATION_ID` で
//! 再送したときの冪等性（台帳）の結合テスト（Issue #3182。ポインタ: `docs/spec/04-behavior/` の
//! TABLE-17・TABLE-23・RECOVER-10〜12）。
//!
//! 役割: 子の INSERT・親の DELETE（CASCADE／SET NULL の連鎖を含む）の再送が、同一内容なら `23505`
//! （既存の台帳契約）で二重適用されず、内容違いの再利用は `22023`、FK 違反で失敗した文は台帳を
//! 消費しないことを固定する。台帳キーのテナント分離、明示トランザクション内の連鎖と COMMIT 後の
//! 再送、`EngineCore` を開き直した後の再送も同じ結果になることを確かめる。
//! 設計記録は `docs/design/rowid-primary-key-separation.md` §15。

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Visibility;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(core: &EngineCore, c: &PolicyContext, sql: &str) -> Result<SqlOutcome, String> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(c, &mut s, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn run(core: &EngineCore, c: &PolicyContext, sql: &str) -> SqlOutcome {
    exec(core, c, sql).unwrap_or_else(|code| panic!("{sql}: {code}"))
}

fn code_of(core: &EngineCore, c: &PolicyContext, sql: &str) -> String {
    match exec(core, c, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(code) => code,
    }
}

fn count(core: &EngineCore, c: &PolicyContext, sql: &str) -> usize {
    match run(core, c, sql) {
        SqlOutcome::Query(QueryResult { rows, .. }) => rows.len(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn setup(core: &EngineCore, c: &PolicyContext) {
    for ddl in [
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
        "CREATE TABLE posts_c (author BIGINT REFERENCES users (id) ON DELETE CASCADE, body TEXT)",
        "CREATE TABLE posts_s (author BIGINT REFERENCES users (id) ON DELETE SET NULL, body TEXT)",
    ] {
        run(core, c, ddl);
    }
}

#[test]
fn child_insert_resend_is_rejected_and_never_duplicates_rows() {
    let path = unique_db_path("t23-ledger-child");
    let _g = CleanupGuard(path.clone());
    let core = EngineCore::open(&path).expect("open");
    let a = ctx("alice");
    setup(&core, &a);
    run(
        &core,
        &a,
        "INSERT INTO users (id, name) VALUES (1, 'a') USING OPERATION_ID 'op-u1'",
    );
    let ins = "INSERT INTO posts_c (author, body) VALUES (1, 'x') USING OPERATION_ID 'op-c1'";
    run(&core, &a, ins);
    // 同一内容の再送は台帳契約どおり 23505 で、行は二重にならない。
    assert_eq!(code_of(&core, &a, ins), "23505");
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 1);
    // 内容違いの operation_id 再利用は 22023。
    assert_eq!(
        code_of(
            &core,
            &a,
            "INSERT INTO posts_c (author, body) VALUES (1, 'y') USING OPERATION_ID 'op-c1'"
        ),
        "22023"
    );
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 1);
}

#[test]
fn fk_violation_does_not_consume_the_operation_id() {
    let path = unique_db_path("t23-ledger-fkfail");
    let _g = CleanupGuard(path.clone());
    let core = EngineCore::open(&path).expect("open");
    let a = ctx("alice");
    setup(&core, &a);
    let ins = "INSERT INTO posts_c (author, body) VALUES (7, 'x') USING OPERATION_ID 'op-late'";
    // 親がまだ無いので 23503。副作用なし。
    assert_eq!(code_of(&core, &a, ins), "23503");
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 0);
    // 親を作った後の同一 operation_id の再試行は成功する（失敗した文は台帳を消費していない）。
    run(
        &core,
        &a,
        "INSERT INTO users (id, name) VALUES (7, 'g') USING OPERATION_ID 'op-u7'",
    );
    run(&core, &a, ins);
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 1);
    assert_eq!(code_of(&core, &a, ins), "23505");
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 1);
}

#[test]
fn parent_delete_cascade_resend_is_not_applied_twice_even_after_id_is_recreated() {
    let path = unique_db_path("t23-ledger-cascade");
    let _g = CleanupGuard(path.clone());
    let core = EngineCore::open(&path).expect("open");
    let a = ctx("alice");
    setup(&core, &a);
    for sql in [
        "INSERT INTO users (id, name) VALUES (1, 'a') USING OPERATION_ID 'op-u1'",
        "INSERT INTO posts_c (author, body) VALUES (1, 'c') USING OPERATION_ID 'op-c1'",
        "INSERT INTO posts_s (author, body) VALUES (1, 's') USING OPERATION_ID 'op-s1'",
    ] {
        run(&core, &a, sql);
    }
    let del = "DELETE FROM users WHERE id = 1 USING OPERATION_ID 'op-del'";
    run(&core, &a, del);
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 0);
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts_s WHERE author IS NULL LIMIT 10"
        ),
        1
    );
    // 同じ id の親と子を作り直したあとの再送は 23505 で、新しい親・子を巻き込まない。
    for sql in [
        "INSERT INTO users (id, name) VALUES (1, 'again') USING OPERATION_ID 'op-u1b'",
        "INSERT INTO posts_c (author, body) VALUES (1, 'c2') USING OPERATION_ID 'op-c2'",
        "INSERT INTO posts_s (author, body) VALUES (1, 's2') USING OPERATION_ID 'op-s2'",
    ] {
        run(&core, &a, sql);
    }
    assert_eq!(code_of(&core, &a, del), "23505");
    assert_eq!(count(&core, &a, "SELECT name FROM users LIMIT 10"), 1);
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 1);
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts_s WHERE author = 1 LIMIT 10"
        ),
        1
    );
}

#[test]
fn ledger_keys_are_tenant_scoped_for_cascading_deletes() {
    let path = unique_db_path("t23-ledger-tenant");
    let _g = CleanupGuard(path.clone());
    let core = EngineCore::open(&path).expect("open");
    let alice = ctx("alice");
    let bob = ctx("bob");
    setup(&core, &alice);
    for (c, who) in [(&alice, "a"), (&bob, "b")] {
        run(
            &core,
            c,
            &format!(
                "INSERT INTO users (id, name) VALUES (1, '{who}') USING OPERATION_ID 'op-shared-u'"
            ),
        );
        run(
            &core,
            c,
            &format!(
                "INSERT INTO posts_c (author, body) VALUES (1, '{who}') USING OPERATION_ID 'op-shared-c'"
            ),
        );
    }
    // 同一の operation_id 文字列を別テナントが使っても独立に成功し、連鎖は自テナントだけに及ぶ。
    let del = "DELETE FROM users WHERE id = 1 USING OPERATION_ID 'op-shared-del'";
    run(&core, &alice, del);
    assert_eq!(count(&core, &alice, "SELECT body FROM posts_c LIMIT 10"), 0);
    assert_eq!(count(&core, &bob, "SELECT body FROM posts_c LIMIT 10"), 1);
    run(&core, &bob, del);
    assert_eq!(count(&core, &bob, "SELECT body FROM posts_c LIMIT 10"), 0);
    // それぞれの再送は 23505。
    assert_eq!(code_of(&core, &alice, del), "23505");
    assert_eq!(code_of(&core, &bob, del), "23505");
}

#[test]
fn explicit_transaction_cascade_resend_matches_after_commit_and_reopen() {
    let path = unique_db_path("t23-ledger-txn");
    let _g = CleanupGuard(path.clone());
    let a = ctx("alice");
    let del = "DELETE FROM users WHERE id = 1 USING OPERATION_ID 'op-tx-del'";
    {
        let core = EngineCore::open(&path).expect("open");
        setup(&core, &a);
        for sql in [
            "INSERT INTO users (id, name) VALUES (1, 'a') USING OPERATION_ID 'op-u1'",
            "INSERT INTO posts_c (author, body) VALUES (1, 'c') USING OPERATION_ID 'op-c1'",
            "INSERT INTO posts_s (author, body) VALUES (1, 's') USING OPERATION_ID 'op-s1'",
        ] {
            run(&core, &a, sql);
        }
        let mut s = SessionState::default();
        let mut txn = core.new_session_transaction();
        for sql in ["BEGIN", del, "COMMIT"] {
            core.execute_sql_in_txn(&a, &mut s, &mut txn, sql)
                .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
        }
        assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 0);
        // COMMIT 後の再送（autocommit 経路）は 23505。
        assert_eq!(code_of(&core, &a, del), "23505");
    }
    // 開き直した後も同じ結果で、連鎖の事後状態は変わらない。
    let core = EngineCore::open(&path).expect("reopen");
    assert_eq!(code_of(&core, &a, del), "23505");
    assert_eq!(
        code_of(
            &core,
            &a,
            "INSERT INTO posts_c (author, body) VALUES (1, 'c') USING OPERATION_ID 'op-c1'"
        ),
        "23505"
    );
    assert_eq!(count(&core, &a, "SELECT name FROM users LIMIT 10"), 0);
    assert_eq!(count(&core, &a, "SELECT body FROM posts_c LIMIT 10"), 0);
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts_s WHERE author IS NULL LIMIT 10"
        ),
        1
    );
}
