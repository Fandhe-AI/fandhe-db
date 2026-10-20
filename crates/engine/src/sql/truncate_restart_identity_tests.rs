//! `TRUNCATE ... RESTART IDENTITY`／`CONTINUE IDENTITY`（SQL-48 (3)・TABLE-24、Issue #1645）の
//! crate 内結合テスト。所有者（`SequenceOwner`）を SQL から作る経路は後続 Issue（#1694）のため、
//! `Storage::set_sequence_owner` で所有関係を直接作って `EngineCore` の production 経路
//! （`execute_sql_in_session`／`execute_sql_in_txn`）を検証する。
//!
//! 検証する契約:
//! - 所有 SEQUENCE の自テナントの採番だけが START へ戻る（`RESTART WITH` でずれていても START）
//! - 他テナントの採番・所有していない SEQUENCE・省略／`CONTINUE IDENTITY` は戻らない
//! - CASCADE で連鎖した表の所有 SEQUENCE も戻る。拒否・再送（`23505`）では戻らない
//! - 明示トランザクション内は所有 SEQUENCE があれば `0A000`（行も状態も変わらない）
//! - 再オープン後（新しいアロケータ）もリセット後の値が続く

use crate::catalog::sequence::SequenceOwner;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn owner(table: &str) -> Option<SequenceOwner> {
    Some(SequenceOwner {
        table: table.to_string(),
        column: "n".to_string(),
    })
}

/// 表 `t`（`n BIGINT`）と、それが所有する SEQUENCE `s`（`seq_ddl` で定義）を作る。
fn fixture(label: &str, seq_ddl: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    ok(&core, "sys", seq_ddl);
    ok(&core, "sys", "CREATE TABLE t (n BIGINT)");
    core.storage_for_test()
        .set_sequence_owner("s", owner("t"))
        .expect("set owner");
    (path, guard, core)
}

fn insert_next(core: &EngineCore, tenant: &str, id: u64, op: &str) {
    ok(
        core,
        tenant,
        &format!("INSERT INTO t (id, n) VALUES ({id}, nextval('s')) USING OPERATION_ID '{op}'"),
    );
}

/// `n` 列を id 昇順で返す（テナントごと）。
fn column_n(core: &EngineCore, tenant: &str) -> Vec<i64> {
    let r = core
        .execute_sql(&ctx(tenant), "SELECT id, n FROM t LIMIT 1000")
        .expect("select");
    let mut rows: Vec<(u64, i64)> = r
        .rows
        .iter()
        .map(|row| {
            let id = match row.cells[0] {
                Cell::Integer(v) => v,
                ref o => panic!("id cell {o:?}"),
            };
            let n = match row.cells[1] {
                Cell::SignedInteger(v) => v,
                ref o => panic!("n cell {o:?}"),
            };
            (id, n)
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, n)| n).collect()
}

fn truncate(core: &EngineCore, tenant: &str, clause: &str, op: &str) {
    ok(
        core,
        tenant,
        &format!("TRUNCATE t {clause} USING OPERATION_ID '{op}'"),
    );
}

/// 受け入れ条件: リセット後の `nextval` が開始値を返す（既定 START 1）。
#[test]
fn restart_identity_resets_owned_sequence_to_start() {
    let (_p, _g, core) = fixture("trunc-restart-basic", "CREATE SEQUENCE s");
    for i in 1..=3 {
        insert_next(&core, "alice", i, &format!("a{i}"));
    }
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3]);
    truncate(&core, "alice", "RESTART IDENTITY", "r1");
    assert!(column_n(&core, "alice").is_empty());
    insert_next(&core, "alice", 1, "a4");
    assert_eq!(column_n(&core, "alice"), vec![1]);
}

/// 非既定の START でも、`ALTER SEQUENCE ... RESTART WITH` でずれた `restart_value` ではなく
/// START へ戻る。
#[test]
fn restart_identity_returns_to_start_not_restart_value() {
    let (_p, _g, core) = fixture("trunc-restart-start", "CREATE SEQUENCE s START WITH 100");
    insert_next(&core, "alice", 1, "a1");
    insert_next(&core, "alice", 2, "a2");
    ok(&core, "sys", "ALTER SEQUENCE s RESTART WITH 50");
    insert_next(&core, "alice", 3, "a3");
    assert_eq!(column_n(&core, "alice"), vec![100, 101, 50]);
    truncate(&core, "alice", "RESTART IDENTITY", "r1");
    insert_next(&core, "alice", 1, "a4");
    assert_eq!(column_n(&core, "alice"), vec![100]);
}

/// 省略形・`CONTINUE IDENTITY` は採番を戻さない。
#[test]
fn continue_identity_and_default_do_not_reset() {
    let (_p, _g, core) = fixture("trunc-continue", "CREATE SEQUENCE s");
    insert_next(&core, "alice", 1, "a1");
    insert_next(&core, "alice", 2, "a2");
    truncate(&core, "alice", "", "c1");
    insert_next(&core, "alice", 1, "a3");
    assert_eq!(column_n(&core, "alice"), vec![3]);
    truncate(&core, "alice", "CONTINUE IDENTITY", "c2");
    insert_next(&core, "alice", 1, "a4");
    assert_eq!(column_n(&core, "alice"), vec![4]);
}

/// テナント境界（P0）: alice のリセットは bob の採番に影響しない（状態行・メモリ上の現在値とも）。
#[test]
fn restart_identity_does_not_touch_other_tenants() {
    let (_p, _g, core) = fixture("trunc-restart-tenants", "CREATE SEQUENCE s");
    insert_next(&core, "alice", 1, "a1");
    insert_next(&core, "alice", 2, "a2");
    insert_next(&core, "bob", 1, "b1");
    insert_next(&core, "bob", 2, "b2");
    insert_next(&core, "bob", 3, "b3");
    truncate(&core, "alice", "RESTART IDENTITY", "r1");
    insert_next(&core, "alice", 1, "a3");
    insert_next(&core, "bob", 4, "b4");
    assert_eq!(column_n(&core, "alice"), vec![1]);
    assert_eq!(column_n(&core, "bob"), vec![1, 2, 3, 4]);
}

/// 所有していない SEQUENCE（`DEFAULT nextval` から参照されるだけ）は戻さない。
#[test]
fn restart_identity_ignores_unowned_sequences() {
    let path = unique_db_path("trunc-restart-unowned");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    ok(&core, "sys", "CREATE SEQUENCE s");
    ok(&core, "sys", "CREATE TABLE t (n BIGINT)");
    insert_next(&core, "alice", 1, "a1");
    insert_next(&core, "alice", 2, "a2");
    truncate(&core, "alice", "RESTART IDENTITY", "r1");
    insert_next(&core, "alice", 1, "a3");
    assert_eq!(column_n(&core, "alice"), vec![3]);
}

/// CASCADE で連鎖して加わった参照元の表が所有する SEQUENCE も戻る。RESTRICT での拒否
/// （`0A000`）では何も戻らない。
#[test]
fn restart_identity_follows_cascade_closure_and_rejection_keeps_state() {
    let path = unique_db_path("trunc-restart-cascade");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    ok(&core, "sys", "CREATE SEQUENCE s");
    ok(&core, "sys", "CREATE TABLE p (id_ BIGINT PRIMARY KEY)");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (n BIGINT, ref_ BIGINT REFERENCES p (id_))",
    );
    core.storage_for_test()
        .set_sequence_owner("s", owner("t"))
        .expect("set owner");
    ok(
        &core,
        "alice",
        "INSERT INTO p (id, id_) VALUES (1, 10) USING OPERATION_ID 'p1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n, ref_) VALUES (1, nextval('s'), 10) USING OPERATION_ID 't1'",
    );
    // RESTRICT: 参照元 `t` が指定外のため拒否され、採番は戻らない。
    assert_eq!(
        code(
            &core,
            "alice",
            "TRUNCATE p RESTART IDENTITY USING OPERATION_ID 'r1'"
        ),
        "0A000"
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n, ref_) VALUES (2, nextval('s'), 10) USING OPERATION_ID 't2'",
    );
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
    // CASCADE: 連鎖した `t` の所有 SEQUENCE が戻る。
    ok(
        &core,
        "alice",
        "TRUNCATE p RESTART IDENTITY CASCADE USING OPERATION_ID 'r2'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO p (id, id_) VALUES (1, 10) USING OPERATION_ID 'p2'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n, ref_) VALUES (1, nextval('s'), 10) USING OPERATION_ID 't3'",
    );
    assert_eq!(column_n(&core, "alice"), vec![1]);
}

/// 同じ `operation_id` の再送（`23505`）では戻らない（台帳で abort する）。内容が違えば `22023`。
#[test]
fn replayed_restart_identity_does_not_reset_again() {
    let (_p, _g, core) = fixture("trunc-restart-replay", "CREATE SEQUENCE s");
    insert_next(&core, "alice", 1, "a1");
    truncate(&core, "alice", "RESTART IDENTITY", "r1");
    insert_next(&core, "alice", 1, "a2");
    insert_next(&core, "alice", 2, "a3");
    assert_eq!(
        code(
            &core,
            "alice",
            "TRUNCATE t RESTART IDENTITY USING OPERATION_ID 'r1'"
        ),
        "23505"
    );
    // CONTINUE（別ドメイン）で同じ ID を使うと内容不一致。
    assert_eq!(
        code(&core, "alice", "TRUNCATE t USING OPERATION_ID 'r1'"),
        "22023"
    );
    insert_next(&core, "alice", 3, "a4");
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3]);
}

/// 明示的な `CONTINUE IDENTITY` と省略形は同じ内容ハッシュ（再送は `23505`）。
#[test]
fn explicit_continue_identity_hashes_like_default() {
    let (_p, _g, core) = fixture("trunc-continue-hash", "CREATE SEQUENCE s");
    truncate(&core, "alice", "", "c1");
    assert_eq!(
        code(
            &core,
            "alice",
            "TRUNCATE t CONTINUE IDENTITY USING OPERATION_ID 'c1'"
        ),
        "23505"
    );
}

/// 明示トランザクション内は所有 SEQUENCE があれば `0A000` で、行も採番状態も変わらない。
#[test]
fn restart_identity_in_transaction_with_owned_sequence_is_rejected() {
    let (_p, _g, core) = fixture("trunc-restart-txn", "CREATE SEQUENCE s");
    insert_next(&core, "alice", 1, "a1");
    insert_next(&core, "alice", 2, "a2");
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &c,
            &mut session,
            &mut txn,
            "TRUNCATE t RESTART IDENTITY USING OPERATION_ID 'r1'",
        )
        .expect_err("must be rejected in a transaction block");
    assert_eq!(err.wire_code(), "0A000");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
    insert_next(&core, "alice", 3, "a3");
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3]);
}

/// 所有 SEQUENCE が無い表なら、明示トランザクション内の `RESTART IDENTITY` も成功する（no-op）。
#[test]
fn restart_identity_in_transaction_without_owned_sequence_succeeds() {
    let path = unique_db_path("trunc-restart-txn-noseq");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    ok(&core, "sys", "CREATE TABLE t (n BIGINT)");
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n) VALUES (1, 7) USING OPERATION_ID 'a1'",
    );
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "TRUNCATE t RESTART IDENTITY USING OPERATION_ID 'r1'",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert!(column_n(&core, "alice").is_empty());
}

/// 再オープン（新しいアロケータ）後も、リセット後の値から続く。
#[test]
fn reset_survives_reopen() {
    let (path, _g, core) = fixture("trunc-restart-reopen", "CREATE SEQUENCE s START WITH 10");
    insert_next(&core, "alice", 1, "a1");
    insert_next(&core, "alice", 2, "a2");
    truncate(&core, "alice", "RESTART IDENTITY", "r1");
    drop(core);
    let core = open_core(&path);
    insert_next(&core, "alice", 1, "a3");
    insert_next(&core, "alice", 2, "a4");
    assert_eq!(column_n(&core, "alice"), vec![10, 11]);
}
