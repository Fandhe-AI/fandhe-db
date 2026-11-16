//! 明示トランザクション内の表・索引の DDL（SQL-80・SQL-31・SQL-23・TABLE-22・TASK-349 のポインタ、
//! Issue #3313。設計記録: `docs/design/explicit-transaction.md`「明示トランザクション内の DDL の方式」）の
//! 結合テスト。
//!
//! `EngineCore::execute_sql_in_txn`（明示 Tx の production 入口）で、`CREATE TABLE`・`ALTER TABLE` の一部・
//! `DROP TABLE`・`CREATE INDEX`・`DROP INDEX` を共有 write txn へ積み、COMMIT で原子的に確定し、
//! ROLLBACK・失敗では痕跡を残さないことを固定する。DDL 後に実行できる文は暫定の許可リストに限り、
//! それ以外は `0A000` で fail-closed になる（受け皿は #3314。権限の再判定・拡張クエリ・UPDATE/DELETE 等）。
//! `sql31_transaction.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
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

/// DDL 権限を持つセッション（`--ddl-allowed-users` 相当）。
fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn tx<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_txn(&ctx("alice"), session, txn, sql)
}

fn tx_ok<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> SqlOutcome {
    tx(core, session, txn, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn tx_code<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> &'static str {
    tx(core, session, txn, sql).expect_err(sql).wire_code()
}

/// 別セッション（autocommit）からの SELECT。`Ok(行数)`／`Err(wire_code)`。
fn read_rows(core: &EngineCore, tenant: &str, sql: &str) -> Result<usize, &'static str> {
    match core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql) {
        Ok(SqlOutcome::Query(result)) => Ok(result.rows.len()),
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code()),
    }
}

fn tx_rows<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> usize {
    match tx_ok(core, session, txn, sql) {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn begin<'a>(core: &'a EngineCore, session: &mut SessionState, txn: &mut SessionTransaction<'a>) {
    assert_eq!(tx_ok(core, session, txn, "BEGIN"), SqlOutcome::Begin);
}

fn insert(id: u64, body: &str) -> String {
    format!("INSERT INTO t (id, body) VALUES ({id}, '{body}') USING OPERATION_ID 'op-{id}-{body}'")
}

const CREATE_T: &str = "CREATE TABLE t (id BIGINT PRIMARY KEY, body TEXT)";

#[test]
fn create_insert_select_commit_persists_atomically() {
    let (core, _guard) = new_core("sql80-create-commit");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(&core, &mut session, &mut txn, &insert(1, "a"));
    assert_eq!(
        tx_rows(&core, &mut session, &mut txn, "SELECT id, body FROM t"),
        1
    );
    // COMMIT 前は別セッション（別テナントを含む）から表の存在も見えない（存在しない表と同じ結果）。
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Err("42P01"));
    assert_eq!(read_rows(&core, "bob", "SELECT id FROM t"), Err("42P01"));
    assert_eq!(
        tx_ok(&core, &mut session, &mut txn, "COMMIT"),
        SqlOutcome::Commit
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Ok(1));
    // RLS: 別テナントには自分の行しか見えない（表は共有・行は不可視）。
    assert_eq!(read_rows(&core, "bob", "SELECT id FROM t"), Ok(0));
}

#[test]
fn rollback_leaves_no_table_rows_or_catalog_residue() {
    let (core, _guard) = new_core("sql80-create-rollback");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(&core, &mut session, &mut txn, &insert(1, "a"));
    assert_eq!(
        tx_ok(&core, &mut session, &mut txn, "ROLLBACK"),
        SqlOutcome::Rollback
    );
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Err("42P01"));
    // 同名の再作成が通る（カタログに残骸が無い）。別スキーマで作り直しても前の世代に引きずられない。
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, other INTEGER)",
    )
    .expect("recreate after rollback");
    assert_eq!(read_rows(&core, "alice", "SELECT other FROM t"), Ok(0));
    assert_eq!(
        read_rows(&core, "alice", "SELECT body FROM t"),
        Err("42703")
    );
}

#[test]
fn ddl_only_transaction_persists_on_commit() {
    let (core, _guard) = new_core("sql80-ddl-only");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Ok(0));
}

#[test]
fn alter_add_column_then_insert_and_select_use_the_new_schema() {
    let (core, _guard) = new_core("sql80-add-column");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER TABLE t ADD COLUMN note TEXT",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "INSERT INTO t (id, body, note) VALUES (1, 'a', 'n') USING OPERATION_ID 'op-note'",
    );
    assert_eq!(
        tx_rows(&core, &mut session, &mut txn, "SELECT note FROM t"),
        1
    );
    // 確定前の別セッションは古いスキーマのまま。
    assert_eq!(
        read_rows(&core, "alice", "SELECT note FROM t"),
        Err("42703")
    );
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT note FROM t"), Ok(1));
}

#[test]
fn alter_then_rollback_restores_the_committed_schema() {
    let (core, _guard) = new_core("sql80-add-column-rollback");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER TABLE t ADD COLUMN note TEXT",
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(
        read_rows(&core, "alice", "SELECT note FROM t"),
        Err("42703")
    );
}

#[test]
fn drop_table_commits_and_rolls_back_with_rows() {
    let (core, _guard) = new_core("sql80-drop");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(1, "a"))
        .expect("seed");
    // ROLLBACK: 行ごと戻る。
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE t");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Ok(1));
    // COMMIT: 表が消える。
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE t");
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Err("42P01"));
}

#[test]
fn create_then_drop_in_one_transaction_leaves_nothing() {
    let (core, _guard) = new_core("sql80-create-drop");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(&core, &mut session, &mut txn, &insert(1, "a"));
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE t");
    // 遅延 FK 検査の対象から外れ、COMMIT が成功する。
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Err("42P01"));
}

#[test]
fn unique_index_is_enforced_in_the_transaction_and_vanishes_on_rollback() {
    let (core, _guard) = new_core("sql80-unique-index");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE UNIQUE INDEX t_body_u ON t (body)",
    );
    tx_ok(&core, &mut session, &mut txn, &insert(1, "a"));
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, &insert(2, "a")),
        "23505"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    // 索引は残らない（重複した値の INSERT が通る）。
    core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(1, "a"))
        .expect("first");
    core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(2, "a"))
        .expect("duplicate is allowed without the index");
}

#[test]
fn drop_index_in_a_transaction_commits_and_rolls_back() {
    let (core, _guard) = new_core("sql80-drop-index");
    let mut session = ddl_session();
    for sql in [CREATE_T, "CREATE UNIQUE INDEX t_body_u ON t (body)"] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
            .expect(sql);
    }
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP INDEX t_body_u");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(1, "a"))
        .expect("first");
    assert_eq!(
        core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(2, "a"))
            .expect_err("index still enforced after rollback")
            .wire_code(),
        "23505"
    );
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP INDEX t_body_u");
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(2, "a"))
        .expect("index dropped by commit");
}

#[test]
fn failed_ddl_fails_the_transaction_and_commit_persists_nothing() {
    let (core, _guard) = new_core("sql80-failed-ddl");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    // 同名の再作成は `42P07`。Tx は `Failed`（DDL を積んだ Tx は復帰不可）。
    assert_eq!(tx_code(&core, &mut session, &mut txn, CREATE_T), "42P07");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(tx_code(&core, &mut session, &mut txn, "COMMIT"), "25P02");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Err("42P01"));
}

#[test]
// tracked: #3317
fn rollback_to_savepoint_after_ddl_is_rejected_and_rollback_discards_everything() {
    let (core, _guard) = new_core("sql80-savepoint");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT sp");
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    // DDL 後の ROLLBACK TO は未対応。Tx は復帰不可の失敗にする。
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "ROLLBACK TO sp"),
        "0A000"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Err("42P01"));
}

#[test]
fn ddl_in_a_read_only_transaction_is_rejected_without_side_effects() {
    let (core, _guard) = new_core("sql80-read-only");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    tx_ok(&core, &mut session, &mut txn, "BEGIN READ ONLY");
    assert_eq!(tx_code(&core, &mut session, &mut txn, CREATE_T), "25006");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Err("42P01"));
}

#[test]
fn ddl_without_ddl_permission_is_42501_and_leaks_no_existence() {
    let (core, _guard) = new_core("sql80-permission");
    let mut allowed = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut allowed, CREATE_T)
        .expect("create");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    // 既存の表と存在しない表で結果が変わらない。
    assert_eq!(tx_code(&core, &mut session, &mut txn, CREATE_T), "42501");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    begin(&core, &mut session, &mut txn);
    assert_eq!(
        tx_code(
            &core,
            &mut session,
            &mut txn,
            "CREATE TABLE other (id BIGINT PRIMARY KEY)"
        ),
        "42501"
    );
}

#[test]
// tracked: #3314
fn statements_not_yet_wired_after_ddl_are_rejected_with_0a000() {
    // DDL 後の UPDATE・DELETE・TRUNCATE・EXPLAIN・COPY 等は未対応。
    let (core, _guard) = new_core("sql80-post-ddl-gate");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    for sql in [
        "UPDATE t SET body = 'b' WHERE id = 1",
        "DELETE FROM t WHERE id = 1",
        "TRUNCATE t",
        "EXPLAIN SELECT id FROM t",
    ] {
        let mut txn = core.new_session_transaction();
        begin(&core, &mut session, &mut txn);
        tx_ok(
            &core,
            &mut session,
            &mut txn,
            "CREATE INDEX t_body ON t (body)",
        );
        assert_eq!(
            tx_code(&core, &mut session, &mut txn, sql),
            "0A000",
            "{sql}"
        );
        assert_eq!(txn.status(), TransactionStatus::Failed);
        tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    }
}

#[test]
// tracked: #3314
fn reads_after_a_drop_are_rejected_with_0a000() {
    // DROP 後の参照は未対応。確定済みカタログでの解析・ビュー展開が古い名前を見るため。
    let (core, _guard) = new_core("sql80-after-drop");
    let mut session = ddl_session();
    for sql in [CREATE_T, "CREATE TABLE u (id BIGINT PRIMARY KEY)"] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
            .expect(sql);
    }
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE t");
    // 消した表は存在しない。他の表への DDL 以外の文も、結線までは `0A000`。
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "SELECT id FROM t"),
        "42P01"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE t");
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "SELECT id FROM u"),
        "0A000"
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Ok(0));
}

// 制約の追加・削除・改名・型変更・SERIAL/IDENTITY 列の Tx 内対応は #3313 の範囲外（受け皿の子 Issue は未起票。
// 起票までは親の #3311 を指す）。
#[test]
// tracked: #3311
fn unsupported_alter_actions_and_serial_columns_are_rejected_with_0a000() {
    let (core, _guard) = new_core("sql80-unsupported-forms");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    for sql in [
        "ALTER TABLE t RENAME TO t2",
        "ALTER TABLE t ADD CONSTRAINT t_body_u UNIQUE (body)",
        "CREATE TABLE s (id BIGINT PRIMARY KEY, n SERIAL)",
    ] {
        let mut txn = core.new_session_transaction();
        begin(&core, &mut session, &mut txn);
        assert_eq!(
            tx_code(&core, &mut session, &mut txn, sql),
            "0A000",
            "{sql}"
        );
        tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    }
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM s"), Err("42P01"));
}

#[test]
fn ddl_counts_toward_the_statement_limit() {
    use engine::sql::transaction::TransactionLimits;
    let (core, _guard) = new_core("sql80-statement-limit");
    let core = core.with_transaction_limits(TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: 2,
    });
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE INDEX t_body ON t (body)",
    );
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "DROP INDEX t_body"),
        "54000"
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
}

#[test]
fn create_if_not_exists_sees_this_transactions_own_table() {
    let (core, _guard) = new_core("sql80-if-not-exists");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    // 同じ Tx で作った表は存在する（no-op で成功）。
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TABLE IF NOT EXISTS t (id BIGINT PRIMARY KEY, body TEXT)",
    );
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE t");
    // DROP した表は存在しない（作成される）。
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TABLE IF NOT EXISTS t (id BIGINT PRIMARY KEY, other INTEGER)",
    );
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT other FROM t"), Ok(0));
}

#[test]
fn generation_reuse_after_rollback_does_not_leak_stale_declarations() {
    // 世代番号は ROLLBACK 後の次の commit で再利用されうる。Tx 内で作った索引・表の宣言が、別セッションの
    // 読み取り・後続の作成に漏れない（V4。世代を鍵にするキャッシュに write txn の内容を載せない）。
    let (core, _guard) = new_core("sql80-generation-reuse");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE INDEX t_body ON t (body)",
    );
    tx_ok(&core, &mut session, &mut txn, &insert(1, "a"));
    assert_eq!(
        tx_rows(
            &core,
            &mut session,
            &mut txn,
            "SELECT id FROM t WHERE body = 'a'"
        ),
        1
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, body TEXT, extra INTEGER)",
    )
    .expect("recreate with another shape");
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "INSERT INTO t (id, body, extra) VALUES (7, 'z', 1) USING OPERATION_ID 'op-gen'",
    )
    .expect("insert");
    assert_eq!(
        read_rows(&core, "bob", "SELECT id FROM t WHERE body = 'a'"),
        Ok(0)
    );
    assert_eq!(
        read_rows(&core, "alice", "SELECT id FROM t WHERE body = 'a'"),
        Ok(0)
    );
    assert_eq!(
        read_rows(&core, "alice", "SELECT id FROM t WHERE body = 'z'"),
        Ok(1)
    );
}
#[test]
fn hnsw_and_gin_declarations_in_a_transaction_do_not_outlive_a_rollback() {
    // V4（P0）: Tx 内で宣言した HNSW・GIN 索引と、その上の検索結果が、ROLLBACK 後に別セッションの検索へ
    // 漏れない（世代を鍵にするキャッシュに write txn の内容を載せない）。
    let (core, _guard) = new_core("sql80-hnsw-gin");
    let mut session = ddl_session();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE d (id BIGINT PRIMARY KEY, v VECTOR(2), j JSONB)",
    )
    .expect("create");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE INDEX d_v ON d USING hnsw (v)",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE INDEX d_j ON d USING gin (j)",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "INSERT INTO d (id, v, j) VALUES (1, '[1.0, 0.0]', '{\"k\": 1}') USING OPERATION_ID 'hg-1'",
    );
    assert_eq!(
        tx_rows(
            &core,
            &mut session,
            &mut txn,
            "SELECT id FROM d ORDER BY v <=> '[1.0, 0.0]' LIMIT 3"
        ),
        1
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    // 別セッションの検索は、Tx 内の行・索引宣言の影響を受けない。
    assert_eq!(
        read_rows(
            &core,
            "alice",
            "SELECT id FROM d ORDER BY v <=> '[1.0, 0.0]' LIMIT 3"
        ),
        Ok(0)
    );
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "INSERT INTO d (id, v, j) VALUES (2, '[0.0, 1.0]', '{\"k\": 2}') USING OPERATION_ID 'hg-2'",
    )
    .expect("insert after rollback");
    assert_eq!(
        read_rows(
            &core,
            "alice",
            "SELECT id FROM d ORDER BY v <=> '[1.0, 0.0]' LIMIT 3"
        ),
        Ok(1)
    );
}

#[test]
fn rows_inserted_into_a_table_touched_in_the_transaction_stay_tenant_scoped() {
    // RLS: 表は共有・行は不可視。別テナントの Tx が DDL を積んでも、SELECT は自テナントの行だけを返す。
    let (core, _guard) = new_core("sql80-rls");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(&core, &mut session, &mut txn, &insert(1, "a"));
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    let mut bob_session = ddl_session();
    let mut bob_txn = core.new_session_transaction();
    let bob = ctx("bob");
    core.execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &bob,
        &mut bob_session,
        &mut bob_txn,
        "CREATE INDEX t_body ON t (body)",
    )
    .expect("bob index");
    core.execute_sql_in_txn(
        &bob,
        &mut bob_session,
        &mut bob_txn,
        "INSERT INTO t (id, body) VALUES (2, 'b') USING OPERATION_ID 'bob-1'",
    )
    .expect("bob insert");
    match core
        .execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, "SELECT id FROM t")
        .expect("bob select")
    {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 1),
        other => panic!("expected Query, got {other:?}"),
    }
    core.execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, "COMMIT")
        .expect("bob commit");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM t"), Ok(1));
    assert_eq!(read_rows(&core, "bob", "SELECT id FROM t"), Ok(1));
}

#[test]
fn a_second_writer_waits_and_does_not_observe_the_pending_table() {
    // 別セッションの同名 CREATE は writer の待機で `55P03` になり、COMMIT 前の表の存在を漏らさない。
    let (core, _guard) = new_core("sql80-second-writer");
    let core = core.with_write_lock_wait(std::time::Duration::from_millis(50));
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    let mut other = ddl_session();
    assert_eq!(
        core.execute_sql_in_session(&ctx("bob"), &mut other, CREATE_T)
            .expect_err("second writer")
            .wire_code(),
        "55P03"
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    core.execute_sql_in_session(&ctx("bob"), &mut other, CREATE_T)
        .expect("create after the writer is released");
}
#[test]
fn deferred_foreign_key_violation_on_tables_created_in_the_transaction_fails_the_commit() {
    // 親子を同じ Tx で作り、`INITIALLY DEFERRED` の FK 違反は COMMIT で `23503`。何も残らない。
    let (core, _guard) = new_core("sql80-deferred-fk");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TABLE p (id BIGINT PRIMARY KEY)",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TABLE c (id BIGINT PRIMARY KEY, pid BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "INSERT INTO c (id, pid) VALUES (1, 99) USING OPERATION_ID 'fk-1'",
    );
    assert_eq!(tx_code(&core, &mut session, &mut txn, "COMMIT"), "23503");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM c"), Err("42P01"));
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM p"), Err("42P01"));
}

#[test]
fn drop_if_exists_of_a_missing_table_in_a_transaction_commits_without_effect() {
    let (core, _guard) = new_core("sql80-drop-ghost");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE IF EXISTS ghost");
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
// tracked: #3314
fn extended_execute_after_ddl_is_rejected_for_everything_but_ddl() {
    // V7: Parse 済み（確定済みカタログで束縛した）文は、DDL を積んだ Tx では実行しない。
    // 古いスキーマで INSERT を束縛してしまう経路を作らない。
    let (core, _guard) = new_core("sql80-extended");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    let parsed_insert = core
        .parse_sql(&insert(1, "a"))
        .expect("parse insert against the committed schema");
    let parsed_index = core
        .parse_sql("CREATE UNIQUE INDEX t_body_u ON t (body)")
        .expect("parse index");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    // DDL 自体は拡張クエリ経由でも受理される。
    core.execute_parsed_in_txn(&ctx("alice"), &mut session, &mut txn, &parsed_index)
        .expect("DDL through the extended path");
    let err = core
        .execute_parsed_in_txn(&ctx("alice"), &mut session, &mut txn, &parsed_insert)
        .expect_err("INSERT after DDL through the extended path");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
    // 何も残らない（索引は作られていない）。
    core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(1, "a"))
        .expect("first");
    core.execute_sql_in_session(&ctx("alice"), &mut session, &insert(2, "a"))
        .expect("no unique index remains");
}

#[test]
// tracked: #3314
fn copy_after_ddl_is_rejected_with_0a000() {
    let (core, _guard) = new_core("sql80-copy");
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, CREATE_T)
        .expect("create");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE UNIQUE INDEX t_body_u ON t (body)",
    );
    let err = core
        .begin_copy_in_txn(
            &ctx("alice"),
            &session,
            &mut txn,
            "COPY t (id, body) FROM STDIN USING OPERATION_ID 'copy-1'",
        )
        .expect_err("COPY after DDL");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
}
