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
// --- 未 commit 変更の読み取り（read-your-writes） ---------------------------------

fn rows_of(outcome: SqlOutcome) -> Vec<Vec<String>> {
    match outcome {
        SqlOutcome::Query(result) => result
            .rows
            .iter()
            .map(|r| r.cells.iter().map(|c| format!("{c:?}")).collect())
            .collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn tx_rows(tx: &mut Tx<'_>, sql: &str) -> Vec<Vec<String>> {
    rows_of(tx.ok(sql))
}

fn auto_rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    let mut session = SessionState::default();
    rows_of(
        core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}")),
    )
}

fn count_of(n: i64) -> Vec<Vec<String>> {
    vec![vec![format!("Integer({n})")]]
}

#[test]
fn scan_and_aggregate_reads_reflect_insert_update_delete_inside_transaction() {
    let (core, path) = new_core("txn-read-scan-agg");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(
        "INSERT INTO docs (id, n, tag) VALUES (4, 40, 'a'), (5, 50, 'e') USING OPERATION_ID 'i1'",
    );
    tx.ok("UPDATE docs SET tag = 'z' WHERE id = 2 USING OPERATION_ID 'u1'");
    tx.ok("DELETE FROM docs WHERE id = 3 USING OPERATION_ID 'd1'");

    // Scan
    let a_rows = tx_rows(&mut tx, "SELECT id FROM docs WHERE tag = 'a' LIMIT 100");
    assert_eq!(a_rows.len(), 2, "ids 1 and 4 carry tag 'a'");
    let z_rows = tx_rows(&mut tx, "SELECT id FROM docs WHERE tag = 'z' LIMIT 100");
    assert_eq!(z_rows.len(), 1, "the UPDATE is visible");
    assert!(tx_rows(&mut tx, "SELECT id FROM docs WHERE tag = 'c' LIMIT 100").is_empty());
    // Aggregate（1,2,4,5 の 4 行）
    assert_eq!(tx_rows(&mut tx, "SELECT COUNT(*) FROM docs"), count_of(4));
    // GROUP BY
    let groups = tx_rows(&mut tx, "SELECT tag, COUNT(*) FROM docs GROUP BY tag");
    assert_eq!(groups.len(), 3, "tags a, z, e remain: {groups:?}");

    // 別セッションからは変更前のまま（3 行）。
    assert_eq!(
        auto_rows(&core, "alice", "SELECT COUNT(*) FROM docs"),
        count_of(3)
    );

    tx.ok("ROLLBACK");
    assert_eq!(
        auto_rows(&core, "alice", "SELECT COUNT(*) FROM docs"),
        count_of(3)
    );
}

#[test]
fn join_subquery_set_operation_and_cursor_reads_see_uncommitted_rows() {
    let (core, path) = new_core("txn-read-join-sub");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "alice");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(
        "INSERT INTO docs (id, n, tag) VALUES (4, 40, 'a'), (5, 50, 'e') USING OPERATION_ID 'i1'",
    );

    // 自己 JOIN（同一テーブルを 2 回開く。`TableAlreadyOpen` を起こさない）。
    let joined = tx_rows(
        &mut tx,
        "SELECT d1.id FROM docs AS d1 JOIN docs AS d2 ON d1.id = d2.id LIMIT 100",
    );
    assert_eq!(joined.len(), 5);
    // 同一テーブルへの `IN (SELECT ...)`。
    let in_sub = tx_rows(
        &mut tx,
        "SELECT id FROM docs WHERE tag IN (SELECT tag FROM docs WHERE id = 4 LIMIT 10) LIMIT 100",
    );
    assert_eq!(in_sub.len(), 2, "ids 1 and 4 share tag 'a'");
    // ウィンドウ関数。
    let windowed = tx_rows(
        &mut tx,
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY tag ORDER BY id) FROM docs LIMIT 100",
    );
    assert_eq!(windowed.len(), 5);
    // 集合演算。
    let union = tx_rows(
        &mut tx,
        "SELECT tag FROM docs UNION ALL SELECT tag FROM docs",
    );
    assert_eq!(union.len(), 10);
    // カーソル。
    tx.ok("DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 100");
    match tx.ok("FETCH 100 FROM c") {
        SqlOutcome::Fetch(result) => assert_eq!(result.rows.len(), 5),
        other => panic!("expected Fetch, got {other:?}"),
    }
    tx.ok("ROLLBACK");
}

#[test]
fn cascade_written_child_is_readable_and_reflects_the_cascade() {
    let (core, path) = new_core("txn-read-cascade");
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
    // 子テーブルは文が直接書き込んだテーブルではないが、連鎖で消えた内容が見える。
    assert!(tx_rows(&mut tx, "SELECT id FROM c LIMIT 100").is_empty());
    tx.ok("ROLLBACK");
    assert_eq!(ids(&core, &alice, "c").len(), 2);
}

/// ROLLBACK したトランザクションの未 commit 行から作ったキャッシュエントリが、後で
/// 世代が再利用されても確定済みデータとして返らないこと（P0。キャッシュは確定済み
/// スナップショットに対してのみ使う）。ベクトル検索・集計・Scan それぞれで確認する。
#[test]
fn rolled_back_rows_never_leak_through_caches_after_generation_reuse() {
    let (core, path) = new_core("txn-read-cache");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE vdocs (embedding VECTOR(2), tag TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO vdocs (id, embedding, tag) VALUES (1, '[1.0, 0.0]', 'a'), (2, '[0.9, 0.1]', 'b') \
         USING OPERATION_ID 'seed'",
    );
    let search = "SELECT id FROM vdocs ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10";
    let count = "SELECT COUNT(*) FROM vdocs";
    let scan = "SELECT id FROM vdocs WHERE tag = 'x' LIMIT 100";
    // キャッシュを温める（確定済みスナップショットでのみ利用される）。
    assert_eq!(auto_rows(&core, "alice", search).len(), 2);
    assert_eq!(auto_rows(&core, "alice", count), count_of(2));
    assert!(auto_rows(&core, "alice", scan).is_empty());

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(
        "INSERT INTO vdocs (id, embedding, tag) VALUES (3, '[1.0, 0.0]', 'x'), (4, '[1.0, 0.01]', 'x') \
         USING OPERATION_ID 'txn-ins'",
    );
    assert_eq!(tx_rows(&mut tx, search).len(), 4);
    assert_eq!(tx_rows(&mut tx, count), count_of(4));
    assert_eq!(tx_rows(&mut tx, scan).len(), 2);
    tx.ok("ROLLBACK");

    // 世代が再利用されうる別の書き込み（autocommit）の後も、ROLLBACK した行は現れない。
    ok(
        &core,
        &alice,
        "INSERT INTO vdocs (id, embedding, tag) VALUES (5, '[0.0, 1.0]', 'y') USING OPERATION_ID 'after'",
    );
    assert_eq!(auto_rows(&core, "alice", search).len(), 3);
    assert_eq!(auto_rows(&core, "alice", count), count_of(3));
    assert!(auto_rows(&core, "alice", scan).is_empty());
}

/// 書き込みトランザクションを読み取り源にしても、RLS（可視性・テナント境界）は同じ実行本体で
/// 適用される。他テナントの行・自テナントの `Private` 行（`Public` のみの文脈）は見えない。
#[test]
fn reads_inside_transaction_apply_rls_like_the_snapshot_path() {
    let (core, path) = new_core("txn-read-rls");
    let _guard = CleanupGuard(path);
    setup_docs(&core);
    seed(&core, "bob");

    // alice のトランザクション: 自分の行を書き込んで読む。bob の行は見えない。
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("INSERT INTO docs (id, n, tag) VALUES (1, 1, 'mine') USING OPERATION_ID 'i1'");
    assert_eq!(tx_rows(&mut tx, "SELECT id FROM docs LIMIT 100").len(), 1);
    assert_eq!(tx_rows(&mut tx, "SELECT COUNT(*) FROM docs"), count_of(1));
    tx.ok("ROLLBACK");

    // `Public` のみの文脈は、INSERT が `Private` で書いた自テナントの行も見えない。
    let public_only = PolicyContext::new("alice").expect("valid tenant");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&public_only, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &public_only,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, n, tag) VALUES (1, 1, 'hidden') USING OPERATION_ID 'i2'",
    )
    .expect("insert");
    let outcome = core
        .execute_sql_in_txn(
            &public_only,
            &mut session,
            &mut txn,
            "SELECT id FROM docs LIMIT 100",
        )
        .expect("select");
    assert!(rows_of(outcome).is_empty());
    core.execute_sql_in_txn(&public_only, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}
