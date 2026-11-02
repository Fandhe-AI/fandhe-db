//! `ROLLBACK TO` と Tx 内の書き込み記録・世代の整合（Issue #1945・
//! `docs/spec/04-behavior/sql-surface.md` SQL-69 のポインタ）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、巻き戻し後の読み取りが巻き戻し後の
//! 状態を返すこと（受け入れ条件 A）、巻き戻した書き込みの一意制約・FK の検査が残らないこと
//! （受け入れ条件 B）、全書き込みを巻き戻した Tx の `COMMIT` が他セッションの競合判定（`40001`）を
//! 起こさないこと（世代）を固定する。DDL は Tx の外で行う（Tx 内 DDL は `0A000`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db::{unique_db_path, CleanupGuard};
use legacy_table::LegacyExec as _;

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// DDL 権限つきセッションで autocommit 実行する（テーブル作成・初期データ投入用）。
fn ok(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
    fn new(core: &'e EngineCore, tenant: &str) -> Self {
        Self {
            core,
            ctx: ctx(tenant),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        }
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.run(sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
    }

    fn err_code(&mut self, sql: &str) -> String {
        self.run(sql)
            .expect_err("statement must fail")
            .wire_code()
            .to_string()
    }

    /// `SELECT count(*) FROM <table>` の値（比較用の文字列表現）。
    fn count(&mut self, table: &str) -> String {
        count_of(self.ok(&format!("SELECT count(*) FROM {table}")))
    }
}

fn count_of(outcome: SqlOutcome) -> String {
    match outcome {
        SqlOutcome::Query(result) => {
            let cell = result
                .rows
                .first()
                .and_then(|r| r.cells.first())
                .expect("count cell");
            format!("{cell:?}")
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

fn autocommit_count(core: &EngineCore, tenant: &str, table: &str) -> String {
    count_of(ok(
        core,
        &ctx(tenant),
        &format!("SELECT count(*) FROM {table}"),
    ))
}

fn rows_len(outcome: SqlOutcome) -> usize {
    match outcome {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// (A) 巻き戻し後の読み取りが巻き戻し後の状態を返し、確定スナップショット経路（キャッシュ）へ戻る。
#[test]
fn reads_after_rollback_to_see_restored_state_and_reuse_caches() {
    let (core, _g) = new_core("sp1945-reads");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE t (n BIGINT)");
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO t (id, n) VALUES (1, 10), (2, 20) USING OPERATION_ID 'seed'",
    );
    let committed = autocommit_count(&core, "alice", "t");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("SAVEPOINT a");
    tx.ok("INSERT INTO t (id, n) VALUES (3, 30), (4, 40) USING OPERATION_ID 'i3'");
    tx.ok("DELETE FROM t WHERE id = 1 USING OPERATION_ID 'd1'");
    assert_ne!(tx.count("t"), committed);
    tx.ok("ROLLBACK TO a");
    let before = core.visible_bitmap_cache_stats().hits;
    assert_eq!(tx.count("t"), committed);
    assert_eq!(rows_len(tx.ok("SELECT id FROM t WHERE n = 10 LIMIT 10")), 1);
    assert_eq!(rows_len(tx.ok("SELECT id FROM t WHERE id = 3 LIMIT 10")), 0);
    // 確定スナップショット経路へ戻っているので、世代キーのキャッシュが再び効く。
    assert!(core.visible_bitmap_cache_stats().hits > before);
    tx.ok("COMMIT");
    assert_eq!(autocommit_count(&core, "alice", "t"), committed);
}

/// (B-1) 巻き戻した一意キーは再利用でき、書き直した値は再び衝突する。
#[test]
fn unique_check_is_correct_after_rollback_to_and_rewrite() {
    let (core, _g) = new_core("sp1945-unique");
    ok(&core, &ctx("sys"), "CREATE TABLE u (k TEXT UNIQUE)");
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("SAVEPOINT a");
    tx.ok("INSERT INTO u (id, k) VALUES (1, 'x') USING OPERATION_ID 'i1'");
    tx.ok("ROLLBACK TO a");
    tx.ok("INSERT INTO u (id, k) VALUES (2, 'x') USING OPERATION_ID 'i2'");
    assert_eq!(
        tx.err_code("INSERT INTO u (id, k) VALUES (3, 'x') USING OPERATION_ID 'i3'"),
        "23505"
    );
}

/// (B-2) 即時 FK: 巻き戻した親行を参照する子は `23503`。
#[test]
fn immediate_fk_rejects_child_of_rolled_back_parent() {
    let (core, _g) = new_core("sp1945-fk-imm");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p)",
    );
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("SAVEPOINT a");
    tx.ok("INSERT INTO p (id, name) VALUES (1, 'p1') USING OPERATION_ID 'p1'");
    tx.ok("ROLLBACK TO a");
    assert_eq!(
        tx.err_code("INSERT INTO c (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'c1'"),
        "23503"
    );
}

/// (B-3) 遅延 FK: 巻き戻した違反は COMMIT に残らず、巻き戻した親に頼る子は COMMIT で `23503`。
#[test]
fn deferred_fk_scope_matches_actual_writes_after_rollback_to() {
    let (core, _g) = new_core("sp1945-fk-def");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(
        &core,
        &ctx("alice"),
        "INSERT INTO p (id, name) VALUES (1, 'p1') USING OPERATION_ID 'seed'",
    );
    let empty = autocommit_count(&core, "alice", "c");

    // 違反する子を書いて巻き戻し、有効な子だけ書き直すと COMMIT は通る。
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("SAVEPOINT a");
    tx.ok("INSERT INTO c (id, parent_id) VALUES (1, 999) USING OPERATION_ID 'c1'");
    tx.ok("ROLLBACK TO a");
    tx.ok("INSERT INTO c (id, parent_id) VALUES (2, 1) USING OPERATION_ID 'c2'");
    tx.ok("COMMIT");
    assert_ne!(autocommit_count(&core, "alice", "c"), empty);

    // 巻き戻した親に頼る子は検査範囲から漏れず、COMMIT で 23503。
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("SAVEPOINT a");
    tx.ok("INSERT INTO p (id, name) VALUES (5, 'p5') USING OPERATION_ID 'p5'");
    tx.ok("ROLLBACK TO a");
    tx.ok("INSERT INTO c (id, parent_id) VALUES (3, 5) USING OPERATION_ID 'c3'");
    assert_eq!(tx.err_code("COMMIT"), "23503");
}

/// (G) 全書き込みを巻き戻した Tx の COMMIT は空の commit を行わず、読み取り済みの並行 Tx の
/// 書き込みを `40001` にしない（巻き戻した行の足跡・世代を残さない）。
#[test]
fn fully_rolled_back_commit_does_not_conflict_with_concurrent_reader() {
    let (core, _g) = new_core("sp1945-gen");
    ok(&core, &ctx("sys"), "CREATE TABLE t (n BIGINT)");
    let mut s2 = Tx::new(&core, "bob");
    s2.ok("BEGIN");
    s2.count("t");

    let mut s1 = Tx::new(&core, "alice");
    s1.ok("BEGIN");
    s1.ok("SAVEPOINT a");
    s1.ok("INSERT INTO t (id, n) VALUES (1, 1) USING OPERATION_ID 'g1'");
    s1.ok("ROLLBACK TO a");
    s1.ok("COMMIT");

    s2.ok("INSERT INTO t (id, n) VALUES (2, 2) USING OPERATION_ID 'g2'");
    s2.ok("COMMIT");
}

/// (R) 巻き戻し後の書き直しは Tx 内で見え、COMMIT 後に永続化される。
#[test]
fn rewrite_after_rollback_to_is_visible_and_persisted() {
    let (core, _g) = new_core("sp1945-rewrite");
    ok(&core, &ctx("sys"), "CREATE TABLE t (n BIGINT)");
    let empty = autocommit_count(&core, "alice", "t");
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("SAVEPOINT a");
    tx.ok("INSERT INTO t (id, n) VALUES (1, 1) USING OPERATION_ID 'r1'");
    tx.ok("ROLLBACK TO a");
    assert_eq!(tx.count("t"), empty);
    tx.ok("INSERT INTO t (id, n) VALUES (2, 2) USING OPERATION_ID 'r2'");
    assert_ne!(tx.count("t"), empty);
    tx.ok("COMMIT");
    assert_ne!(autocommit_count(&core, "alice", "t"), empty);
    // 他テナントの確定行は影響を受けない。
    assert_eq!(autocommit_count(&core, "carol", "t"), empty);
}
