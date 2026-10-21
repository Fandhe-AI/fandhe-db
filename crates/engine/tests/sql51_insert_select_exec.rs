//! `INSERT ... SELECT` の SQL 経路 e2e（SQL-51・Issue #1732 の parser・束縛と #1733 の実行本体の結線）。
//!
//! SQL 文から autocommit・明示トランザクションで実行し、行の挿入・RETURNING・RLS（他テナント行が内側
//! SELECT に入らない）・`USING OPERATION_ID` の再送（`23505`／内容不一致 `22023`）・ON CONFLICT 併用の
//! fail-closed を固定する。`sql51_insert_select_bind.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。
//! ポインタ: `docs/design/insert-select.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE src (id BIGINT PRIMARY KEY, a INTEGER, t TEXT)",
        "CREATE TABLE dst (id BIGINT PRIMARY KEY, a BIGINT, t TEXT)",
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    // alice の行 2 件・bob の行 1 件（同じ表。RLS で互いに不可視）
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, a, t) VALUES (1, 10, 'x'), (2, 20, 'y') USING OPERATION_ID 'seed-a'",
    )
    .expect("seed alice");
    core.execute_insert_sql(
        &ctx("bob"),
        "INSERT INTO src (id, a, t) VALUES (3, 30, 'z') USING OPERATION_ID 'seed-b'",
    )
    .expect("seed bob");
    (core, path)
}

fn run(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn code(r: Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError>) -> String {
    match r {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 表のテナント可視な行の `id` 列（昇順）。
fn ids(core: &EngineCore, tenant: &str, table: &str) -> Vec<String> {
    match run(core, tenant, &format!("SELECT id FROM {table} LIMIT 100")).expect("select") {
        SqlOutcome::Query(r) => {
            let mut v: Vec<String> = r
                .rows
                .iter()
                .map(|row| format!("{:?}", row.cells))
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn autocommit_inserts_rows_and_rls_hides_other_tenant_rows() {
    let (core, path) = setup("sql51-e2e-auto");
    let _g = CleanupGuard(path);
    let out = run(
        &core,
        "alice",
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'ins-1'",
    )
    .expect("insert select");
    match out {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected Insert, got {other:?}"),
    }
    // alice の 2 行だけが入る。bob の行 (id 3) は内側 SELECT の材料にならない。
    assert_eq!(ids(&core, "alice", "dst").len(), 2);
    // bob から alice の挿入行は見えない
    assert!(ids(&core, "bob", "dst").is_empty());
    // bob が同じ文を実行すると、bob 自身の 1 行だけが入る
    run(
        &core,
        "bob",
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'ins-b'",
    )
    .expect("bob insert select");
    assert_eq!(ids(&core, "bob", "dst").len(), 1);
    assert_eq!(ids(&core, "alice", "dst").len(), 2);
}

#[test]
fn returning_returns_inserted_rows() {
    let (core, path) = setup("sql51-e2e-ret");
    let _g = CleanupGuard(path);
    let out = run(
        &core,
        "alice",
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 RETURNING id, a \
         USING OPERATION_ID 'ret-1'",
    )
    .expect("insert select returning");
    match out {
        SqlOutcome::Returning(r) => {
            assert_eq!(r.rows_affected, 2);
            assert_eq!(r.result.rows.len(), 2);
            assert_eq!(r.result.columns.len(), 2);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
}

#[test]
fn operation_id_replay_is_rejected_without_double_insert() {
    let (core, path) = setup("sql51-e2e-replay");
    let _g = CleanupGuard(path);
    let sql = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 \
               USING OPERATION_ID 'replay-1'";
    run(&core, "alice", sql).expect("first");
    // 同一文・同一 operation_id の再送は 23505 で、行は増えない
    assert_eq!(code(run(&core, "alice", sql)), "23505");
    assert_eq!(ids(&core, "alice", "dst").len(), 2);
    // 内側の結果が変わった後でも同一文なら 23505（結果行の値は照合の材料にしない）
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, a, t) VALUES (4, 40, 'w') USING OPERATION_ID 'seed-a2'",
    )
    .expect("seed more");
    assert_eq!(code(run(&core, "alice", sql)), "23505");
    assert_eq!(ids(&core, "alice", "dst").len(), 2);
    // 同じ operation_id で別の文（内容不一致）は 22023 で拒否される
    let other = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src WHERE a = 10 LIMIT 100 \
                 USING OPERATION_ID 'replay-1'";
    assert_eq!(code(run(&core, "alice", other)), "22023");
    assert_eq!(ids(&core, "alice", "dst").len(), 2);
}

#[test]
fn zero_row_select_records_ledger_and_replay_is_rejected() {
    let (core, path) = setup("sql51-e2e-zero");
    let _g = CleanupGuard(path);
    let sql = "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src WHERE a = 999 LIMIT 100 \
               USING OPERATION_ID 'zero-1'";
    match run(&core, "alice", sql).expect("zero rows") {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected Insert, got {other:?}"),
    }
    assert_eq!(code(run(&core, "alice", sql)), "23505");
}

#[test]
fn explicit_transaction_commit_and_rollback() {
    let (core, path) = setup("sql51-e2e-txn");
    let _g = CleanupGuard(path);
    let mut txn = core.new_session_transaction();
    let mut ts = SessionState::default();
    let c = ctx("alice");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(
            &c,
            &mut ts,
            &mut txn,
            "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 \
             USING OPERATION_ID 'txn-1'",
        )
        .expect("insert select in txn");
    match out {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected Insert, got {other:?}"),
    }
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(ids(&core, "alice", "dst").is_empty());

    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin 2");
    core.execute_sql_in_txn(
        &c,
        &mut ts,
        &mut txn,
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'txn-2'",
    )
    .expect("insert select in txn 2");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(ids(&core, "alice", "dst").len(), 2);
    // bob には見えない
    assert!(ids(&core, "bob", "dst").is_empty());
}

#[test]
fn on_conflict_with_insert_select_stays_fail_closed() {
    let (core, path) = setup("sql51-e2e-conflict");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "alice",
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'oc-1'",
    );
    assert_eq!(code(r), "0A000");
    assert!(ids(&core, "alice", "dst").is_empty());
}
