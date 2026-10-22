//! 移行済みの既存表へ `id` を省略した `INSERT ... SELECT` の IDENTITY 採番（SQL-51・SQL-34。Issue #2615。
//! 関連: TABLE-23・TABLE-24。設計記録は `docs/design/migrated-id-identity.md`・`docs/design/insert-select.md`）。
//!
//! 検証の軸: 採番は自テナントの最大 `id` + 1 から行順に所有 SEQUENCE（`items_id_seq`）で払い出され、
//! `LIMIT` 省略形でも同じ。`USING OPERATION_ID` の再送は `23505` で値を消費せず、operation_id 無しの
//! 再実行は新しい値を採番する。自己参照（`INSERT INTO items SELECT ... FROM items`）は文の開始時点の
//! 行だけを読む。明示トランザクション経路でも採番できる。
//! fixture は `table24_migrated_id_batch.rs` と同じ（従来方式で作って開き直すと移行される）。

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Visibility;

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = session();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// `RETURNING id` の全行の `id`。
fn returned_ids(out: &SqlOutcome) -> Vec<u64> {
    match out {
        SqlOutcome::Returning(r) => r
            .result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Integer(v)) => *v,
                other => panic!("id cell {other:?}"),
            })
            .collect(),
        other => panic!("expected RETURNING, got {other:?}"),
    }
}

/// 自テナントの `items` の `id` を昇順で返す。
fn ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let mut v: Vec<u64> = core
        .execute_sql(&ctx(tenant), "SELECT id FROM items LIMIT 1000")
        .expect("select")
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

/// 従来方式の `items (n BIGINT)`（tenant-a の id 3・10、tenant-b の id 5）を移行し、読み取り元 `src` を足す。
fn fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    {
        let core = EngineCore::open(&path).expect("open");
        let mut s = session();
        core.execute_sql_in_session_legacy(&ctx("sys"), &mut s, "CREATE TABLE items (n BIGINT)")
            .expect("create");
        for (t, id) in [("tenant-a", 3), ("tenant-a", 10), ("tenant-b", 5)] {
            let sql = format!("INSERT INTO items (id, n) VALUES ({id}, {id})");
            core.execute_sql_in_session_legacy(&ctx(t), &mut s, &sql)
                .expect("seed");
        }
    }
    let core = EngineCore::open(&path).expect("reopen migrates");
    ok(
        &core,
        "sys",
        "CREATE TABLE src (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    for t in ["tenant-a", "tenant-b"] {
        ok(
            &core,
            t,
            "INSERT INTO src (id, n) VALUES (1, 100), (2, 101), (3, 102)",
        );
    }
    (guard, core)
}

#[test]
fn omitted_id_is_assigned_in_row_order_per_tenant() {
    let (_g, core) = fixture("sql51-mid-order");
    let a = ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) SELECT n FROM src ORDER BY n LIMIT 100 RETURNING id",
    );
    assert_eq!(returned_ids(&a), vec![11, 12, 13]);
    // LIMIT 省略形でも同じく採番される（tenant-b は最大 id 5 の次から）。
    let b = ok(
        &core,
        "tenant-b",
        "INSERT INTO items (n) SELECT n FROM src ORDER BY n RETURNING id",
    );
    assert_eq!(returned_ids(&b), vec![6, 7, 8]);
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12, 13]);
    assert_eq!(ids(&core, "tenant-b"), vec![5, 6, 7, 8]);
}

#[test]
fn resend_with_operation_id_is_23505_and_does_not_consume_values() {
    let (_g, core) = fixture("sql51-mid-resend");
    let sql = "INSERT INTO items (n) SELECT n FROM src ORDER BY n RETURNING id \
               USING OPERATION_ID 'is-1'";
    assert_eq!(returned_ids(&ok(&core, "tenant-a", sql)), vec![11, 12, 13]);
    assert_eq!(code(&core, "tenant-a", sql), "23505");
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12, 13]);
    let next = ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) VALUES (9) RETURNING id",
    );
    assert_eq!(returned_ids(&next), vec![14]);
}

#[test]
fn rerun_without_operation_id_assigns_new_values() {
    let (_g, core) = fixture("sql51-mid-rerun");
    let sql = "INSERT INTO items (n) SELECT n FROM src ORDER BY n RETURNING id";
    assert_eq!(returned_ids(&ok(&core, "tenant-a", sql)), vec![11, 12, 13]);
    assert_eq!(returned_ids(&ok(&core, "tenant-a", sql)), vec![14, 15, 16]);
}

#[test]
fn self_reference_reads_statement_snapshot_and_assigns_ids() {
    let (_g, core) = fixture("sql51-mid-self");
    // tenant-a の items は 2 行（id 3・10）。自己参照で、ちょうど 2 行が増え、新しい id は連番。
    let out = ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) SELECT n FROM items RETURNING id",
    );
    assert_eq!(returned_ids(&out), vec![11, 12]);
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12]);
    // 2 回目は 4 行がちょうど 2 倍（8 行）になる。
    ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) SELECT n FROM items",
    );
    assert_eq!(ids(&core, "tenant-a").len(), 8);
}

#[test]
fn explicit_transaction_assigns_ids() {
    let (_g, core) = fixture("sql51-mid-txn");
    let c = ctx("tenant-a");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &c,
        &mut s,
        &mut txn,
        "INSERT INTO items (n) SELECT n FROM src ORDER BY n",
    )
    .expect("insert select in txn");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12, 13]);
}
