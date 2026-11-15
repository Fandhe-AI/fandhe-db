//! 採番方式の親（`id` は宣言した普通の列）への `id` 参照 FK の参照アクション連鎖と遅延検査の結合テスト
//! （Issue #3180。ポインタ: `docs/spec/04-behavior/` の TABLE-17・TABLE-23）。
//!
//! 親が疑似列 `id` を持たないときの参照アクション（ON DELETE／ON UPDATE）は列参照経路で動く。
//! 検証の軸は、各アクションの連鎖・自己参照・上限（深さ 16）・遅延検査・テナント境界。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
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

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(ctx, &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn code_of(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    let mut s = SessionState::default();
    s.allow_ddl();
    match core.execute_sql_in_session(ctx, &mut s, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn query(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match run(core, ctx, sql) {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn texts(q: &QueryResult, column: usize) -> Vec<String> {
    let mut out: Vec<String> = q
        .rows
        .iter()
        .map(|r| match r.cells.get(column) {
            Some(Cell::Text(t)) => t.clone(),
            other => panic!("expected text cell, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn count(core: &EngineCore, c: &PolicyContext, sql: &str) -> usize {
    query(core, c, sql).rows.len()
}

fn setup_users_posts(core: &EngineCore, c: &PolicyContext, action: &str) {
    run(
        core,
        c,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run(
        core,
        c,
        &format!("CREATE TABLE posts (author BIGINT REFERENCES users (id) {action}, body TEXT)"),
    );
    run(
        core,
        c,
        "INSERT INTO users (id, name) VALUES (1, 'a'), (2, 'b')",
    );
    run(
        core,
        c,
        "INSERT INTO posts (author, body) VALUES (1, 'p1'), (1, 'p1b'), (2, 'p2')",
    );
}

#[test]
fn on_delete_cascade_removes_children_only_of_deleted_parent() {
    let (core, _g) = new_core("t23-act-del-cascade");
    let a = ctx("alice");
    setup_users_posts(&core, &a, "ON DELETE CASCADE");
    run(&core, &a, "DELETE FROM users WHERE id = 1");
    let rows = query(&core, &a, "SELECT body FROM posts LIMIT 10");
    assert_eq!(texts(&rows, 0), vec!["p2"]);
}

#[test]
fn on_delete_set_null_nulls_the_child_column() {
    let (core, _g) = new_core("t23-act-del-setnull");
    let a = ctx("alice");
    setup_users_posts(&core, &a, "ON DELETE SET NULL");
    run(&core, &a, "DELETE FROM users WHERE id = 1");
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author IS NULL LIMIT 10"
        ),
        2
    );
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 2 LIMIT 10"
        ),
        1
    );
}

#[test]
fn on_delete_set_default_requires_existing_default_parent() {
    let (core, _g) = new_core("t23-act-del-setdefault");
    let a = ctx("alice");
    run(
        &core,
        &a,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &a,
        "CREATE TABLE posts (author BIGINT DEFAULT 9 REFERENCES users (id) ON DELETE SET DEFAULT, body TEXT)",
    );
    run(&core, &a, "INSERT INTO users (id, name) VALUES (1, 'a')");
    run(
        &core,
        &a,
        "INSERT INTO posts (author, body) VALUES (1, 'x')",
    );
    // 既定値 9 の親が無いので 23503 で拒否され、副作用は無い。
    assert_eq!(
        code_of(&core, &a, "DELETE FROM users WHERE id = 1"),
        "23503"
    );
    assert_eq!(count(&core, &a, "SELECT name FROM users LIMIT 10"), 1);
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 1 LIMIT 10"
        ),
        1
    );
    run(&core, &a, "INSERT INTO users (id, name) VALUES (9, 'd')");
    run(&core, &a, "DELETE FROM users WHERE id = 1");
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 9 LIMIT 10"
        ),
        1
    );
}

#[test]
fn on_delete_restrict_and_no_action_reject_referenced_parent() {
    for action in ["ON DELETE RESTRICT", "ON DELETE NO ACTION", ""] {
        let (core, _g) = new_core("t23-act-del-restrict");
        let a = ctx("alice");
        setup_users_posts(&core, &a, action);
        assert_eq!(
            code_of(&core, &a, "DELETE FROM users WHERE id = 1"),
            "23503",
            "{action}"
        );
        assert_eq!(count(&core, &a, "SELECT name FROM users LIMIT 10"), 2);
        assert_eq!(count(&core, &a, "SELECT body FROM posts LIMIT 10"), 3);
    }
}

#[test]
fn on_update_actions_follow_parent_id_change() {
    let a = ctx("alice");
    let (core, _g) = new_core("t23-act-upd-cascade");
    setup_users_posts(&core, &a, "ON UPDATE CASCADE");
    run(&core, &a, "UPDATE users SET id = 10 WHERE id = 1");
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 10 LIMIT 10"
        ),
        2
    );
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 1 LIMIT 10"
        ),
        0
    );
    // 非キー列の更新は子に触れない。
    run(&core, &a, "UPDATE users SET name = 'z' WHERE id = 10");
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 10 LIMIT 10"
        ),
        2
    );

    let (core, _g) = new_core("t23-act-upd-setnull");
    setup_users_posts(&core, &a, "ON UPDATE SET NULL");
    run(&core, &a, "UPDATE users SET id = 10 WHERE id = 1");
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author IS NULL LIMIT 10"
        ),
        2
    );

    for action in ["ON UPDATE NO ACTION", "ON UPDATE RESTRICT"] {
        let (core, _g) = new_core("t23-act-upd-noaction");
        setup_users_posts(&core, &a, action);
        assert_eq!(
            code_of(&core, &a, "UPDATE users SET id = 10 WHERE id = 1"),
            "23503",
            "{action}"
        );
        assert_eq!(
            count(
                &core,
                &a,
                "SELECT body FROM posts WHERE author = 1 LIMIT 10"
            ),
            2
        );
    }
}

#[test]
fn self_reference_cascade_deletes_tree_and_survives_cycle() {
    let (core, _g) = new_core("t23-act-selfref");
    let a = ctx("alice");
    run(
        &core,
        &a,
        "CREATE TABLE selfref (id BIGINT PRIMARY KEY, up BIGINT REFERENCES selfref (id) ON DELETE CASCADE)",
    );
    run(
        &core,
        &a,
        "INSERT INTO selfref (id, up) VALUES (1, NULL), (2, 1), (3, 2), (4, 2), (5, NULL), (6, 5)",
    );
    run(&core, &a, "DELETE FROM selfref WHERE id = 1");
    assert_eq!(count(&core, &a, "SELECT id FROM selfref LIMIT 10"), 2);
    // 循環: 10 -> 11 -> 10。
    run(
        &core,
        &a,
        "INSERT INTO selfref (id, up) VALUES (10, NULL), (11, 10)",
    );
    run(&core, &a, "UPDATE selfref SET up = 11 WHERE id = 10");
    run(&core, &a, "DELETE FROM selfref WHERE id = 10");
    assert_eq!(count(&core, &a, "SELECT id FROM selfref LIMIT 10"), 2);
}

#[test]
fn deep_self_reference_chain_hits_depth_limit_with_no_side_effect() {
    let (core, _g) = new_core("t23-act-depth");
    let a = ctx("alice");
    run(
        &core,
        &a,
        "CREATE TABLE chain (id BIGINT PRIMARY KEY, up BIGINT REFERENCES chain (id) ON DELETE CASCADE)",
    );
    run(&core, &a, "INSERT INTO chain (id, up) VALUES (1, NULL)");
    for i in 2..=25 {
        run(
            &core,
            &a,
            &format!("INSERT INTO chain (id, up) VALUES ({i}, {})", i - 1),
        );
    }
    assert_eq!(
        code_of(&core, &a, "DELETE FROM chain WHERE id = 1"),
        "54000"
    );
    assert_eq!(count(&core, &a, "SELECT id FROM chain LIMIT 100"), 25);
}

#[test]
fn two_level_cascade_through_allocated_tables() {
    let (core, _g) = new_core("t23-act-grandchild");
    let a = ctx("alice");
    run(
        &core,
        &a,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &a,
        "CREATE TABLE posts (pid BIGINT PRIMARY KEY, author BIGINT REFERENCES users (id) ON DELETE CASCADE)",
    );
    run(
        &core,
        &a,
        "CREATE TABLE likes (post BIGINT REFERENCES posts (pid) ON DELETE SET NULL, who TEXT)",
    );
    run(&core, &a, "INSERT INTO users (id, name) VALUES (1, 'a')");
    run(&core, &a, "INSERT INTO posts (pid, author) VALUES (7, 1)");
    run(&core, &a, "INSERT INTO likes (post, who) VALUES (7, 'z')");
    run(&core, &a, "DELETE FROM users WHERE id = 1");
    assert_eq!(count(&core, &a, "SELECT pid FROM posts LIMIT 10"), 0);
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT who FROM likes WHERE post IS NULL LIMIT 10"
        ),
        1
    );
}

#[test]
fn mixed_column_types_cascade_through_key_recoder() {
    let (core, _g) = new_core("t23-act-types");
    let a = ctx("alice");
    run(
        &core,
        &a,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &a,
        "CREATE TABLE posts (author INTEGER REFERENCES users (id) ON DELETE CASCADE ON UPDATE CASCADE, body TEXT)",
    );
    run(
        &core,
        &a,
        "INSERT INTO users (id, name) VALUES (1, 'a'), (2, 'b')",
    );
    run(
        &core,
        &a,
        "INSERT INTO posts (author, body) VALUES (1, 'x'), (2, 'y')",
    );
    run(&core, &a, "UPDATE users SET id = 5 WHERE id = 2");
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 5 LIMIT 10"
        ),
        1
    );
    run(&core, &a, "DELETE FROM users WHERE id = 1");
    assert_eq!(
        texts(&query(&core, &a, "SELECT body FROM posts LIMIT 10"), 0),
        vec!["y"]
    );
}

#[test]
fn deferred_fk_is_checked_at_commit_with_parent_delete_and_update() {
    let (core, _g) = new_core("t23-act-deferred");
    let a = ctx("alice");
    run(
        &core,
        &a,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &a,
        "CREATE TABLE posts (author BIGINT, body TEXT, FOREIGN KEY (author) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED)",
    );
    run(&core, &a, "INSERT INTO users (id, name) VALUES (1, 'a')");
    run(
        &core,
        &a,
        "INSERT INTO posts (author, body) VALUES (1, 'x')",
    );

    let exec = |sqls: &[&str]| -> Result<(), String> {
        let mut s = SessionState::default();
        s.allow_ddl();
        let mut txn = core.new_session_transaction();
        for sql in sqls {
            core.execute_sql_in_txn(&a, &mut s, &mut txn, sql)
                .map_err(|e| e.wire_code().to_string())?;
        }
        Ok(())
    };
    // 孤児が残るまま COMMIT -> 23503 で全体が巻き戻る。
    assert_eq!(
        exec(&["BEGIN", "DELETE FROM users WHERE id = 1", "COMMIT"]).unwrap_err(),
        "23503"
    );
    assert_eq!(count(&core, &a, "SELECT name FROM users LIMIT 10"), 1);
    // 親を消し、COMMIT 前に子も消す -> 成功。
    exec(&[
        "BEGIN",
        "DELETE FROM users WHERE id = 1",
        "DELETE FROM posts WHERE author = 1",
        "COMMIT",
    ])
    .expect("resolved before commit");
    assert_eq!(count(&core, &a, "SELECT name FROM users LIMIT 10"), 0);
    assert_eq!(count(&core, &a, "SELECT body FROM posts LIMIT 10"), 0);
    // 親の id 更新後に子を付け替えれば成功し、付け替えなければ 23503。
    run(&core, &a, "INSERT INTO users (id, name) VALUES (1, 'a')");
    run(
        &core,
        &a,
        "INSERT INTO posts (author, body) VALUES (1, 'x')",
    );
    assert_eq!(
        exec(&["BEGIN", "UPDATE users SET id = 2 WHERE id = 1", "COMMIT"]).unwrap_err(),
        "23503"
    );
    exec(&[
        "BEGIN",
        "UPDATE users SET id = 2 WHERE id = 1",
        "UPDATE posts SET author = 2 WHERE author = 1",
        "COMMIT",
    ])
    .expect("re-pointed before commit");
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT body FROM posts WHERE author = 2 LIMIT 10"
        ),
        1
    );
}

#[test]
fn cascade_never_touches_another_tenants_rows_with_same_id() {
    let (core, _g) = new_core("t23-act-tenant");
    let alice = ctx("alice");
    let bob = ctx("bob");
    run(
        &core,
        &alice,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &alice,
        "CREATE TABLE posts (author BIGINT REFERENCES users (id) ON DELETE CASCADE ON UPDATE CASCADE, body TEXT)",
    );
    for c in [&alice, &bob] {
        run(&core, c, "INSERT INTO users (id, name) VALUES (1, 'u')");
        run(&core, c, "INSERT INTO posts (author, body) VALUES (1, 'x')");
    }
    run(&core, &alice, "UPDATE users SET id = 5 WHERE id = 1");
    assert_eq!(
        count(
            &core,
            &bob,
            "SELECT body FROM posts WHERE author = 1 LIMIT 10"
        ),
        1
    );
    run(&core, &alice, "DELETE FROM users WHERE id = 5");
    assert_eq!(count(&core, &alice, "SELECT body FROM posts LIMIT 10"), 0);
    assert_eq!(count(&core, &bob, "SELECT body FROM posts LIMIT 10"), 1);
    assert_eq!(count(&core, &bob, "SELECT name FROM users LIMIT 10"), 1);
}

/// 遅延 FK の COMMIT 時検査も他テナントの親を見つけない（Issue #3182）。他テナントだけが持つ `id` を
/// 指した `INITIALLY DEFERRED` の子は COMMIT で `23503` になり、どこにも無い `id` の場合と応答が一致する
/// （他テナントの存在を示さない）。自テナントの親を指す子は COMMIT できる。
#[test]
fn deferred_fk_commit_never_sees_another_tenants_parent() {
    let (core, _g) = new_core("t23-act-deferred-tenant");
    let alice = ctx("alice");
    let bob = ctx("bob");
    run(
        &core,
        &alice,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &alice,
        "CREATE TABLE posts (author BIGINT, body TEXT, FOREIGN KEY (author) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO users (id, name) VALUES (1, 'a')",
    );
    run(
        &core,
        &bob,
        "INSERT INTO users (id, name) VALUES (777, 'b')",
    );

    let commit_code = |c: &PolicyContext, insert: &str| -> String {
        let mut s = SessionState::default();
        let mut txn = core.new_session_transaction();
        // BEGIN と INSERT は遅延 FK なので必ず成功する。エラーは COMMIT の検査だけで比較する。
        for sql in ["BEGIN", insert] {
            core.execute_sql_in_txn(c, &mut s, &mut txn, sql)
                .unwrap_or_else(|e| panic!("{sql} must succeed: {}", e.wire_code()));
        }
        let mut last = String::from("ok");
        if let Err(e) = core.execute_sql_in_txn(c, &mut s, &mut txn, "COMMIT") {
            last = e.wire_code().to_string();
        }
        last
    };
    let hidden = commit_code(&alice, "INSERT INTO posts (author, body) VALUES (777, 'x')");
    let nowhere = commit_code(
        &alice,
        "INSERT INTO posts (author, body) VALUES (4242, 'x')",
    );
    assert_eq!(hidden, "23503");
    assert_eq!(hidden, nowhere);
    assert_eq!(count(&core, &alice, "SELECT body FROM posts LIMIT 10"), 0);
    assert_eq!(
        commit_code(&alice, "INSERT INTO posts (author, body) VALUES (1, 'x')"),
        "ok"
    );
    assert_eq!(count(&core, &alice, "SELECT body FROM posts LIMIT 10"), 1);
    // 他テナント（bob）は alice の id=1 を指せない（bob には id=1 の親が無い）。
    assert_eq!(
        commit_code(&bob, "INSERT INTO posts (author, body) VALUES (1, 'y')"),
        "23503"
    );
    assert_eq!(count(&core, &bob, "SELECT body FROM posts LIMIT 10"), 0);
}
