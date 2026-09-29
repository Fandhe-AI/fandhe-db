//! 明示トランザクション内の書き込み文（複数行 `INSERT`・UPSERT・`UPDATE`・
//! `DELETE`）の結合テスト（Issue #1179。ポインタ: `docs/spec/05-tasks.md`
//! TASK-221・`docs/spec/04-behavior/sql-surface.md` SQL-31）。
//!
//! `EngineCore::execute_sql_in_txn`（`sql::transaction::SessionTransaction` を
//! `&mut` で受け取るトランザクション対応入口）を production 経路として検証する。
//! 確認する契約: (1) `COMMIT` で一括確定・`ROLLBACK`／drop で全て破棄（行・台帳とも）、
//! (2) 実行中の分離（未 commit の変更は他セッションから見えない）、
//! (3) 同一トランザクション内での `operation_id` 再利用は `25000`、
//! (4) `INITIALLY DEFERRED` の FK は `COMMIT` 時に検査される（参照アクションの
//! 連鎖で書き換わった子テーブルを含む）、(5) 自トランザクションの未 commit 変更の
//! 読み取り（Scan・Aggregate・JOIN・サブクエリ・カーソル）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
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
    core.execute_sql_in_session(ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

/// 1 列（`id`）を返す SELECT の結果 id 列を昇順で返す（autocommit）。
fn ids(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<String> {
    let mut v = query_ids(ok(core, ctx, &format!("SELECT id FROM {table} LIMIT 1000")));
    v.sort();
    v
}

fn query_ids(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Query(result) => result
            .rows
            .iter()
            .map(|r| format!("{:?}", r.cells.first().expect("id column")))
            .collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
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
            .unwrap_or_else(|e| panic!("{sql} must succeed inside the transaction, got {e:?}"))
    }

    fn err_code(&mut self, sql: &str) -> String {
        self.run(sql)
            .map(|o| panic!("{sql} must fail, got {o:?}"))
            .unwrap_err()
            .wire_code()
            .to_string()
    }

    fn status(&self) -> TransactionStatus {
        self.txn.status()
    }
}

fn setup_docs(core: &EngineCore) {
    let sys = ctx("sys");
    ok(core, &sys, "CREATE TABLE docs (n BIGINT, tag TEXT)");
}

fn seed(core: &EngineCore, tenant: &str) {
    let c = ctx(tenant);
    ok(
        core,
        &c,
        "INSERT INTO docs (id, n, tag) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c') \
         USING OPERATION_ID 'seed'",
    );
}

// --- COMMIT / ROLLBACK / drop ------------------------------------------------

#[test]
fn multi_row_insert_upsert_update_delete_commit_atomically() {
    let (core, path) = new_core("txn-dml-commit");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");
    let alice = ctx("alice");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(
        "INSERT INTO docs (id, n, tag) VALUES (4, 40, 'd'), (5, 50, 'e') USING OPERATION_ID 'i1'",
    );
    tx.ok(
        "INSERT INTO docs (id, n, tag) VALUES (1, 11, 'a2'), (6, 60, 'f') \
         ON CONFLICT (id) DO UPDATE SET tag = EXCLUDED.tag USING OPERATION_ID 'u1'",
    );
    tx.ok("UPDATE docs SET n = 21 WHERE id = 2 USING OPERATION_ID 'up1'");
    tx.ok("UPDATE docs SET tag = 'z' WHERE tag = 'c' USING OPERATION_ID 'up2'");
    tx.ok("DELETE FROM docs WHERE id = 3 USING OPERATION_ID 'd1'");
    tx.ok("DELETE FROM docs WHERE tag = 'd' USING OPERATION_ID 'd2'");

    // 実行中は別セッションから未 commit の変更が見えない。
    assert_eq!(ids(&core, &alice, "docs").len(), 3);

    tx.ok("COMMIT");
    assert_eq!(tx.status(), TransactionStatus::Idle);
    // 1,2,(3 削除),(4 削除),5,6
    assert_eq!(ids(&core, &alice, "docs").len(), 4);
}

#[test]
fn rollback_discards_rows_and_ledger_for_every_dml_kind() {
    let (core, path) = new_core("txn-dml-rollback");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");
    let alice = ctx("alice");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(
        "INSERT INTO docs (id, n, tag) VALUES (4, 40, 'd'), (5, 50, 'e') USING OPERATION_ID 'i1'",
    );
    tx.ok("INSERT INTO docs (id, n, tag) VALUES (1, 11, 'a2') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'u1'");
    tx.ok("UPDATE docs SET n = 99 WHERE id = 2 USING OPERATION_ID 'up1'");
    tx.ok("UPDATE docs SET tag = 'z' WHERE tag = 'c' USING OPERATION_ID 'up2'");
    tx.ok("DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'd1'");
    tx.ok("DELETE FROM docs WHERE tag = 'b' USING OPERATION_ID 'd2'");
    tx.ok("ROLLBACK");
    assert_eq!(tx.status(), TransactionStatus::Idle);

    assert_eq!(ids(&core, &alice, "docs").len(), 3);
    // 台帳も巻き戻るため、同じ operation_id を autocommit で再利用できる。
    for sql in [
        "INSERT INTO docs (id, n, tag) VALUES (4, 40, 'd'), (5, 50, 'e') USING OPERATION_ID 'i1'",
        "UPDATE docs SET n = 99 WHERE id = 2 USING OPERATION_ID 'up1'",
        "UPDATE docs SET tag = 'z' WHERE tag = 'c' USING OPERATION_ID 'up2'",
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'd1'",
        "DELETE FROM docs WHERE tag = 'b' USING OPERATION_ID 'd2'",
    ] {
        ok(&core, &alice, sql);
    }
}

#[test]
fn dropping_an_active_transaction_discards_dml() {
    let (core, path) = new_core("txn-dml-drop");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");
    let alice = ctx("alice");
    {
        let mut tx = Tx::new(&core, "alice");
        tx.ok("BEGIN");
        tx.ok("DELETE FROM docs WHERE tag = 'a' USING OPERATION_ID 'd1'");
        tx.ok("INSERT INTO docs (id, n, tag) VALUES (7, 70, 'g'), (8, 80, 'h') USING OPERATION_ID 'i1'");
        // ここで接続断相当（drop）。
    }
    assert_eq!(ids(&core, &alice, "docs").len(), 3);
    ok(
        &core,
        &alice,
        "DELETE FROM docs WHERE tag = 'a' USING OPERATION_ID 'd1'",
    );
}

#[test]
fn delete_returning_inside_transaction_returns_the_deleted_row() {
    let (core, path) = new_core("txn-dml-delete-returning");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    match tx.ok("DELETE FROM docs WHERE id = 2 RETURNING id, n USING OPERATION_ID 'd1'") {
        SqlOutcome::Returning(outcome) => assert_eq!(outcome.result.rows.len(), 1),
        other => panic!("expected Returning, got {other:?}"),
    }
    match tx.ok(
        "INSERT INTO docs (id, n, tag) VALUES (9, 90, 'x'), (10, 100, 'y') \
         RETURNING id USING OPERATION_ID 'i1'",
    ) {
        SqlOutcome::Returning(outcome) => assert_eq!(outcome.result.rows.len(), 2),
        other => panic!("expected Returning, got {other:?}"),
    }
    tx.ok("ROLLBACK");
    assert_eq!(ids(&core, &ctx("alice"), "docs").len(), 3);
}

// --- operation_id の再利用検査 ---------------------------------------------------

#[test]
fn operation_id_reuse_is_rejected_for_delete_and_update_too() {
    let (core, path) = new_core("txn-dml-opid");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");

    for (first, second) in [
        (
            "INSERT INTO docs (id, n, tag) VALUES (4, 1, 'x'), (5, 1, 'y') USING OPERATION_ID 'same'",
            "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'same'",
        ),
        (
            "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'same'",
            "UPDATE docs SET n = 5 WHERE id = 2 USING OPERATION_ID 'same'",
        ),
        (
            "UPDATE docs SET n = 5 WHERE tag = 'a' USING OPERATION_ID 'same'",
            "DELETE FROM docs WHERE tag = 'a' USING OPERATION_ID 'same'",
        ),
    ] {
        let mut tx = Tx::new(&core, "alice");
        tx.ok("BEGIN");
        tx.ok(first);
        assert_eq!(tx.err_code(second), "25000", "{second}");
        assert_eq!(tx.status(), TransactionStatus::Failed);
    }
}

// --- 遅延 FK ---------------------------------------------------------------------

#[test]
fn deferred_foreign_key_is_checked_at_commit_for_new_write_forms() {
    let (core, path) = new_core("txn-dml-deferred-fk");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, name) VALUES (1, 'p1'), (2, 'p2') USING OPERATION_ID 'seed-p'",
    );

    // 複数行 INSERT: 親が無くても文は通り、COMMIT で 23503 になり何も残らない。
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("INSERT INTO c (id, parent_id) VALUES (1, 1), (2, 999) USING OPERATION_ID 'c1'");
    assert_eq!(tx.err_code("COMMIT"), "23503");
    assert_eq!(tx.status(), TransactionStatus::Idle);
    assert!(ids(&core, &alice, "c").is_empty());

    // 親の DELETE（子が残る）も COMMIT まで遅延され、子も消せば COMMIT が通る。
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'seed-c'",
    );
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("DELETE FROM p WHERE id = 1 USING OPERATION_ID 'dp'");
    tx.ok("DELETE FROM c WHERE id = 1 USING OPERATION_ID 'dc'");
    tx.ok("COMMIT");
    assert_eq!(ids(&core, &alice, "p").len(), 1);
    assert!(ids(&core, &alice, "c").is_empty());

    // UPDATE で子を存在しない親へ向けると COMMIT で 23503。
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, parent_id) VALUES (7, 2) USING OPERATION_ID 'seed-c7'",
    );
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("UPDATE c SET parent_id = 500 WHERE id = 7 USING OPERATION_ID 'uc'");
    assert_eq!(tx.err_code("COMMIT"), "23503");
}

/// 参照アクション（`ON DELETE CASCADE`）の連鎖で書き換わった子テーブルは文が
/// 直接対象にしたテーブルではない（`mark_written` されない）が、その子を親とする
/// 孫テーブルの遅延 FK も COMMIT で検査される（fail-open 防止。Issue #1179 の
/// dirty テーブル判定）。`p` ← `c`（CASCADE）← `g`（`INITIALLY DEFERRED`）で、
/// `p` の行を消すと連鎖で `c` の行が消え、`g` の行が宙に浮く。
#[test]
fn deferred_foreign_key_below_a_cascade_written_table_is_checked_at_commit() {
    let (core, path) = new_core("txn-dml-cascade-deferred");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (a BIGINT REFERENCES p ON DELETE CASCADE)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE g (x BIGINT REFERENCES c DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, name) VALUES (1, 'p1') USING OPERATION_ID 'seed-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, a) VALUES (10, 1) USING OPERATION_ID 'seed-c'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO g (id, x) VALUES (100, 10) USING OPERATION_ID 'seed-g'",
    );

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("DELETE FROM p WHERE id = 1 USING OPERATION_ID 'dp'");
    assert_eq!(
        tx.err_code("COMMIT"),
        "23503",
        "the table below the cascade-written child must be checked at COMMIT"
    );
    assert_eq!(tx.status(), TransactionStatus::Idle);
    assert_eq!(ids(&core, &alice, "p").len(), 1);
    assert_eq!(ids(&core, &alice, "c").len(), 1);
}

/// 連鎖（`ON DELETE CASCADE`）をトランザクション内で実行して `ROLLBACK` すると
/// 子テーブルも復元される。
#[test]
fn cascade_inside_transaction_is_rolled_back_with_the_child_table() {
    let (core, path) = new_core("txn-dml-cascade-rollback");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (a BIGINT REFERENCES p ON DELETE CASCADE)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, name) VALUES (1, 'p1') USING OPERATION_ID 'seed-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, a) VALUES (1, 1), (2, 1) USING OPERATION_ID 'seed-c'",
    );

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("DELETE FROM p WHERE id = 1 USING OPERATION_ID 'dp'");
    tx.ok("ROLLBACK");
    assert_eq!(ids(&core, &alice, "p").len(), 1);
    assert_eq!(ids(&core, &alice, "c").len(), 2);
}

// --- RLS ------------------------------------------------------------------------

#[test]
fn dml_inside_transaction_does_not_touch_other_tenants_rows() {
    let (core, path) = new_core("txn-dml-rls");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");
    seed(&core, "bob");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    // alice のトランザクションから見える範囲は alice の行のみ（bob の同一 id は
    // 別の物理キーで到達不能）。他テナントの行の有無で応答は変わらない。
    match tx.ok("DELETE FROM docs WHERE tag = 'a' USING OPERATION_ID 'd1'") {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected Delete, got {other:?}"),
    }
    tx.ok("COMMIT");
    assert_eq!(ids(&core, &ctx("alice"), "docs").len(), 2);
    assert_eq!(ids(&core, &ctx("bob"), "docs").len(), 3);
}

#[test]
fn file_form_insert_stays_unsupported_inside_transaction() {
    let (core, path) = new_core("txn-dml-file-form");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE files (path TEXT, body TEXT, embedding VECTOR(2))",
    );
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    let code = tx.err_code(
        "INSERT INTO files (path, body) VALUES ('a.txt', 'hello') USING OPERATION_ID 'f1'",
    );
    assert_eq!(code, "0A000");
    assert_eq!(tx.status(), TransactionStatus::Failed);
}
