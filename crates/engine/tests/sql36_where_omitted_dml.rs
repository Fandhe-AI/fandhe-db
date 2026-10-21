//! `WHERE` 省略の `UPDATE`／`DELETE` の受理（SQL-36。`docs/spec/04-behavior/` のポインタのみ。
//! Issue #1743）の結合テスト。`tests/sql41_bool_literal_where.rs` と同じ流儀（実 `Storage`＋
//! `CpuScalarProvider`＋`execute_sql_in_session`）で、RLS 境界・影響行数上限・`RETURNING`・
//! 台帳（RECOVER-11）・分割実行・明示トランザクションの各契約が述語形と同じであることを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::parser::DmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core(limits: DmlLimits) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql36-where-omitted");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_dml_limits(limits),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .map_err(|e| e.wire_code().to_string())
}

fn seed(core: &EngineCore, ctx: &PolicyContext, tag: &str, ids: &[u64]) {
    for id in ids {
        run(
            core,
            ctx,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', 'ja') \
                 USING OPERATION_ID 'seed-{tag}-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("seed {tag}-{id}: {e}"));
    }
}

fn ids_of(core: &EngineCore, ctx: &PolicyContext) -> Vec<u64> {
    let r = core
        .execute_sql(ctx, &format!("SELECT id FROM {TABLE} LIMIT 100"))
        .expect("select");
    let mut v: Vec<u64> = r
        .rows
        .iter()
        .map(|row| match row.cells.first() {
            Some(Cell::Integer(n)) => *n,
            Some(Cell::SignedInteger(n)) => u64::try_from(*n).expect("id >= 0"),
            other => panic!("unexpected id cell: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn count_lang(core: &EngineCore, ctx: &PolicyContext, lang: &str) -> usize {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE lang = '{lang}' LIMIT 100"),
    )
    .expect("select")
    .rows
    .len()
}

fn affected(o: &SqlOutcome) -> u64 {
    match o {
        SqlOutcome::Delete(d) => d.rows_affected,
        SqlOutcome::Update(u) => u.rows_affected,
        SqlOutcome::Returning(r) => r.rows_affected,
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn sql36_rls_boundary_only_own_tenant_rows() {
    let (core, path) = new_core(DmlLimits::default());
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    seed(&core, &a, "a", &[1, 2, 3]);
    seed(&core, &b, "b", &[11, 12]);

    let o = run(
        &core,
        &a,
        &format!("UPDATE {TABLE} SET lang = 'xx' USING OPERATION_ID 'u1'"),
    )
    .expect("update");
    assert_eq!(affected(&o), 3);
    assert_eq!(count_lang(&core, &a, "xx"), 3);
    assert_eq!(count_lang(&core, &b, "xx"), 0);

    let o = run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} USING OPERATION_ID 'd1'"),
    )
    .expect("delete");
    assert_eq!(affected(&o), 3);
    assert!(ids_of(&core, &a).is_empty());
    assert_eq!(ids_of(&core, &b).len(), 2);
}

#[test]
fn sql36_affected_rows_limit_rejects_whole_statement_without_side_effects() {
    let limits = DmlLimits {
        max_affected_rows: std::num::NonZeroUsize::new(2),
        ..DmlLimits::default()
    };
    let (core, path) = new_core(limits);
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2, 3]);

    let sql = format!("DELETE FROM {TABLE} USING OPERATION_ID 'over'");
    assert_eq!(run(&core, &a, &sql).unwrap_err(), "54000");
    let sql_u = format!("UPDATE {TABLE} SET lang = 'xx' USING OPERATION_ID 'over-u'");
    assert_eq!(run(&core, &a, &sql_u).unwrap_err(), "54000");
    assert_eq!(ids_of(&core, &a), vec![1, 2, 3]);
    assert_eq!(count_lang(&core, &a, "xx"), 0);

    // 台帳に記録されていないので、同じ operation_id は上限内の状態で新規実行として成功する。
    run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} WHERE id = 3 USING OPERATION_ID 'x'"),
    )
    .expect("trim to 2 rows");
    let o = run(&core, &a, &sql).expect("within limit after rejection");
    assert_eq!(affected(&o), 2);
}

#[test]
fn sql36_no_limit_by_default_affects_all_rows() {
    let (core, path) = new_core(DmlLimits::default());
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2, 3, 4, 5]);
    let o = run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} USING OPERATION_ID 'all'"),
    )
    .expect("ok");
    assert_eq!(affected(&o), 5);
}

fn returning_shape(o: &SqlOutcome) -> (Vec<String>, Vec<Vec<String>>) {
    match o {
        SqlOutcome::Returning(r) => {
            let cols = r.result.columns.iter().map(|c| format!("{c:?}")).collect();
            let mut rows: Vec<Vec<String>> = r
                .result
                .rows
                .iter()
                .map(|row| row.cells.iter().map(|c| format!("{c:?}")).collect())
                .collect();
            rows.sort();
            (cols, rows)
        }
        other => panic!("expected Returning, got {other:?}"),
    }
}

#[test]
fn sql36_returning_matches_predicate_form() {
    let mk = || {
        let (core, path) = new_core(DmlLimits::default());
        let a = ctx_for("tenant-a");
        seed(&core, &a, "a", &[1, 2, 3]);
        (core, path, a)
    };
    let (c1, p1, a1) = mk();
    let _g1 = CleanupGuard(p1);
    let (c2, p2, a2) = mk();
    let _g2 = CleanupGuard(p2);

    let omitted = run(
        &c1,
        &a1,
        &format!("UPDATE {TABLE} SET lang = 'xx' RETURNING id, lang USING OPERATION_ID 'r'"),
    )
    .expect("omitted");
    let pred = run(
        &c2,
        &a2,
        &format!(
            "UPDATE {TABLE} SET lang = 'xx' WHERE id >= 0 RETURNING id, lang USING OPERATION_ID 'r'"
        ),
    )
    .expect("pred");
    assert_eq!(returning_shape(&omitted), returning_shape(&pred));
    assert_eq!(returning_shape(&omitted).1.len(), 3);

    let omitted = run(
        &c1,
        &a1,
        &format!("DELETE FROM {TABLE} RETURNING id, lang USING OPERATION_ID 'rd'"),
    )
    .expect("omitted");
    let pred = run(
        &c2,
        &a2,
        &format!("DELETE FROM {TABLE} WHERE id >= 0 RETURNING id, lang USING OPERATION_ID 'rd'"),
    )
    .expect("pred");
    assert_eq!(returning_shape(&omitted), returning_shape(&pred));
    assert_eq!(returning_shape(&omitted).1.len(), 3);
}

#[test]
fn sql36_ledger_resend_and_statement_mismatch() {
    let (core, path) = new_core(DmlLimits::default());
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2]);
    let sql = format!("DELETE FROM {TABLE} USING OPERATION_ID 'led'");
    run(&core, &a, &sql).expect("first");
    // 再送は既存の台帳判定（同一内容なら再実行せず成功または 23505）に従う。
    match run(&core, &a, &sql) {
        Ok(o) => assert_eq!(affected(&o), 2),
        Err(code) => assert_eq!(code, "23505"),
    }
    let other = format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'led'");
    assert_eq!(run(&core, &a, &other).unwrap_err(), "22023");
}

#[test]
fn sql36_partitioned_requires_operation_id_and_covers_all_rows() {
    let (core, path) = new_core(DmlLimits::default());
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    seed(&core, &a, "a", &[1, 2, 3, 4, 5]);
    seed(&core, &b, "b", &[11]);
    assert_eq!(
        run(
            &core,
            &a,
            &format!("DELETE FROM {TABLE} PARTITIONED CHUNK 2")
        )
        .unwrap_err(),
        "23502"
    );
    run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} USING OPERATION_ID 'p1' PARTITIONED CHUNK 2"),
    )
    .expect("partitioned");
    assert!(ids_of(&core, &a).is_empty());
    assert_eq!(ids_of(&core, &b), vec![11]);
}

#[test]
fn sql36_empty_table_affects_zero_rows() {
    let (core, path) = new_core(DmlLimits::default());
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let o = run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} USING OPERATION_ID 'e1'"),
    )
    .expect("ok");
    assert_eq!(affected(&o), 0);
    let o = run(
        &core,
        &a,
        &format!("UPDATE {TABLE} SET lang = 'x' USING OPERATION_ID 'e2'"),
    )
    .expect("ok");
    assert_eq!(affected(&o), 0);
}

#[test]
fn sql36_explicit_transaction_rollback_restores_rows() {
    let (core, path) = new_core(DmlLimits::default());
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2]);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN".to_string(),
        format!("DELETE FROM {TABLE} USING OPERATION_ID 'tx1'"),
        format!("UPDATE {TABLE} SET lang = 'xx' USING OPERATION_ID 'tx2'"),
    ] {
        core.execute_sql_in_txn(&a, &mut session, &mut txn, &sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    core.execute_sql_in_txn(&a, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(ids_of(&core, &a), vec![1, 2]);
}

#[test]
fn sql36_trivially_true_and_visible_only_predicates_stay_rejected() {
    let (core, path) = new_core(DmlLimits::default());
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1]);
    for p in ["visible()", "TRUE", "lang = 'ja' OR TRUE"] {
        let sql = format!("DELETE FROM {TABLE} WHERE {p} USING OPERATION_ID 'z'");
        let r = run(&core, &a, &sql);
        assert_eq!(r.clone().unwrap_err(), "42601", "{p}: {r:?}");
    }
    assert_eq!(ids_of(&core, &a), vec![1]);
}
