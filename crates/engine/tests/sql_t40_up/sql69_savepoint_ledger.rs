//! `ROLLBACK TO` と冪等台帳の整合（Issue #1944・RECOVER-1・SQL-69 のポインタ）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、巻き戻した `operation_id` を同じ Tx 内で
//! 再利用できること、`COMMIT` 後の永続の台帳（`op_ledger`・`last_op`。RECOVER-2・RECOVER-7）に
//! 巻き戻した文の記録が残らないことを固定する。巻き戻していない ID は従来どおり `25000` で拒否する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::{LastOperationLookup, LedgerLookup};
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility, WriteDurability};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core_with(tag: &str, durability: Option<WriteDurability>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = match durability {
        Some(d) => Storage::open_with_durability(&path, d).expect("open storage"),
        None => Storage::open(&path).expect("open storage"),
    };
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_write_lock_wait(std::time::Duration::from_millis(100));
    (core, guard)
}

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    new_core_with(tag, None)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op_id}'"
    )
}

const SELECT: &str = "SELECT id FROM documents ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10";

/// 別セッション（Tx なし）から見える確定済みの行数。
fn committed_rows(engine: &EngineCore, caller: &PolicyContext) -> usize {
    match engine
        .execute_sql_in_session(caller, &mut SessionState::default(), SELECT)
        .expect("select")
    {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn run<'e>(
    engine: &'e EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    engine.execute_sql_in_txn(caller, session, txn, sql)
}

fn begin_sp<'e>(
    core: &'e EngineCore,
    caller: &PolicyContext,
    s: &mut SessionState,
) -> SessionTransaction<'e> {
    let mut txn = core.new_session_transaction();
    run(core, caller, s, &mut txn, "BEGIN").unwrap();
    run(core, caller, s, &mut txn, "SAVEPOINT a").unwrap();
    txn
}

fn recorded(core: &EngineCore, caller: &PolicyContext, op: &str) -> LedgerLookup {
    core.operation_recorded(caller, TABLE, &OperationId::parse(op).unwrap())
        .expect("lookup")
}

/// 受け入れ条件 1・2 の本体（durability 構成ごとに再利用する）。
fn assert_reuse_and_ledger_clean(core: &EngineCore) {
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(core, &caller, &mut s);
    run(core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(core, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap();
    run(core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    // 受け入れ条件 1: 巻き戻した ID は同じ Tx 内で再利用できる。
    run(core, &caller, &mut s, &mut txn, &insert_sql(3, "op-3")).unwrap();
    run(core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    run(core, &caller, &mut s, &mut txn, &insert_sql(4, "op-4")).unwrap();
    run(core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    // 受け入れ条件 2: 台帳には確定した文だけが残る。
    assert_eq!(recorded(core, &caller, "op-4"), LedgerLookup::Recorded);
    for op in ["op-1", "op-2", "op-3"] {
        assert_eq!(
            recorded(core, &caller, op),
            LedgerLookup::NotRecorded,
            "{op}"
        );
    }
    assert_eq!(
        core.last_operation_id(&caller, TABLE).unwrap(),
        LastOperationLookup::Committed(OperationId::parse("op-4").unwrap())
    );
    // 巻き戻した ID を Tx の外から再送すると、新しい書き込みとして成功する。
    let mut outside = core.new_session_transaction();
    run(core, &caller, &mut s, &mut outside, &insert_sql(1, "op-1")).unwrap();
    assert_eq!(committed_rows(core, &caller), 2);
}

#[test]
fn rolled_back_operation_ids_are_reusable_and_ledger_is_clean() {
    let (core, _g) = new_core("sql69-ledger-basic");
    assert_reuse_and_ledger_clean(&core);
}

#[test]
fn rolled_back_operation_ids_work_without_durability() {
    let (core, _g) = new_core_with("sql69-ledger-nodur", Some(WriteDurability::None));
    assert_reuse_and_ledger_clean(&core);
}

#[test]
fn reused_operation_id_may_carry_different_content() {
    let (core, _g) = new_core("sql69-ledger-diff");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(9, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(recorded(&core, &caller, "op-1"), LedgerLookup::Recorded);
    assert_eq!(committed_rows(&core, &caller), 1);
}

#[test]
fn rollback_only_leaves_no_ledger_entry() {
    let (core, _g) = new_core("sql69-ledger-none");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(recorded(&core, &caller, "op-1"), LedgerLookup::NotRecorded);
    assert_eq!(
        core.last_operation_id(&caller, TABLE).unwrap(),
        LastOperationLookup::NotFound
    );
}

/// 巻き戻していない ID（`ROLLBACK TO` の後に登録した分）は従来どおり `25000` で拒否する。
#[test]
fn operation_id_registered_after_rollback_is_still_rejected_on_reuse() {
    let (core, _g) = new_core("sql69-ledger-still-dup");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = begin_sp(&core, &caller, &mut s);
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(3, "op-3")).unwrap();
    let err = run(&core, &caller, &mut s, &mut txn, &insert_sql(4, "op-3")).unwrap_err();
    assert_eq!(err.wire_code(), "25000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}
