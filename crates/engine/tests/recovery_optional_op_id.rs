//! `operation_id` 任意モード（既定）の結合テスト（Issue #1704、対象ビヘイビア: RECOVER-1。
//! ポインタ: `docs/spec/04-behavior/recovery.md` RECOVER-1・`docs/design/
//! operation-id-optional.md`）。
//!
//! `EngineCore` をモード指定なしで構築した既定（`LedgerMode::Optional`）で、書き込み文の
//! `USING OPERATION_ID` 省略が成功し台帳に記録されないこと、`LedgerMode::Ledgered`
//! （`--require-operation-id` 相当）では従来どおり `23502` になること、分割実行 DML は
//! 構成に関わらず省略を `23502` で拒否すること、`operation_id` を指定した場合の契約
//! （重複拒否・内容不一致）が不変であることを固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::{LastOperationLookup, LedgerLookup};
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str, mode: Option<LedgerMode>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(mode) = mode {
        core = core.with_ledger_mode(mode);
    }
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, n BIGINT, u TEXT)",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn rows(core: &EngineCore, tenant: &str) -> usize {
    core.execute_sql(&ctx(tenant), "SELECT id FROM docs LIMIT 1000")
        .expect("scan")
        .rows
        .len()
}

fn step<'a>(
    core: &'a EngineCore,
    caller: &PolicyContext,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> SqlOutcome {
    core.execute_sql_in_txn(caller, &mut SessionState::default(), txn, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation_id")
}

/// 台帳に何も記録されていない（`last_operation_id` が `NotFound`）ことを確かめる。
fn assert_ledger_untouched(core: &EngineCore, tenant: &str) {
    assert_eq!(
        core.last_operation_id(&ctx(tenant), "docs")
            .expect("last_operation_id"),
        LastOperationLookup::NotFound
    );
}

#[test]
fn default_mode_accepts_omitted_operation_id_and_does_not_record() {
    let (core, _g) = new_core("opt-insert", None);
    ok(&core, "t", "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')");
    ok(
        &core,
        "t",
        "INSERT INTO docs (id, n, u) VALUES (2, 2, 'b'), (3, 3, 'c')",
    );
    ok(
        &core,
        "t",
        "INSERT INTO docs (id, n, u) VALUES (4, 4, 'd') USING OPERATION_ID NULL",
    );
    assert_eq!(rows(&core, "t"), 4);
    assert_ledger_untouched(&core, "t");
}

#[test]
fn default_mode_accepts_omitted_update_delete_upsert_truncate() {
    let (core, _g) = new_core("opt-dml", None);
    ok(
        &core,
        "t",
        "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a'), (2, 2, 'b')",
    );
    ok(&core, "t", "UPDATE docs SET u = 'x' WHERE id = 1");
    ok(&core, "t", "UPDATE docs SET u = 'y' WHERE n > 1");
    ok(&core, "t", "DELETE FROM docs WHERE id = 1");
    ok(&core, "t", "DELETE FROM docs WHERE n > 1");
    assert_eq!(rows(&core, "t"), 0);
    ok(
        &core,
        "t",
        "INSERT INTO docs (id, n, u) VALUES (5, 5, 'e') ON CONFLICT (id) DO NOTHING",
    );
    ok(&core, "t", "TRUNCATE TABLE docs");
    assert_eq!(rows(&core, "t"), 0);
    assert_ledger_untouched(&core, "t");
}

#[test]
fn omitted_writes_apply_every_time_like_a_plain_write() {
    let (core, _g) = new_core("opt-resend", None);
    let sql = "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')";
    ok(&core, "t", sql);
    // 再送の検出はしない。同じ id は行 id 衝突として扱われ、台帳由来の拒否ではない。
    let c = code(&core, "t", sql);
    assert_eq!(c, "23505");
    ok(&core, "t", "INSERT INTO docs (id, n, u) VALUES (2, 7, 'p')");
    ok(&core, "t", "INSERT INTO docs (id, n, u) VALUES (3, 7, 'p')");
    assert_eq!(rows(&core, "t"), 3);
    assert_ledger_untouched(&core, "t");
}

#[test]
fn explicit_transaction_with_omitted_operation_id_commits_and_rolls_back() {
    let (core, _g) = new_core("opt-txn", None);
    let caller = ctx("t");
    let mut txn = core.new_session_transaction();
    step(&core, &caller, &mut txn, "BEGIN");
    step(
        &core,
        &caller,
        &mut txn,
        "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')",
    );
    step(
        &core,
        &caller,
        &mut txn,
        "UPDATE docs SET u = 'z' WHERE id = 1",
    );
    step(&core, &caller, &mut txn, "COMMIT");
    assert_eq!(rows(&core, "t"), 1);

    let mut txn = core.new_session_transaction();
    step(&core, &caller, &mut txn, "BEGIN");
    step(
        &core,
        &caller,
        &mut txn,
        "INSERT INTO docs (id, n, u) VALUES (2, 2, 'b')",
    );
    step(&core, &caller, &mut txn, "ROLLBACK");
    assert_eq!(rows(&core, "t"), 1);
    assert_ledger_untouched(&core, "t");
}

#[test]
fn row_apis_accept_none_by_default() {
    use engine::storage::RowInput;
    let (core, _g) = new_core("opt-row-api", None);
    let c = ctx("t");
    let row = RowInput {
        tenant_id: "t",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2, 0.3],
        metadata: &[],
    };
    // `docs` はベクトル列を持たないため、行 API の成否ではなく 23502 でないことだけを見る。
    for r in [
        core.insert_row(&c, "docs", 1, &row, None).map(|_| ()),
        core.update_row(&c, "docs", 1, &row, None).map(|_| ()),
        core.delete_row(&c, "docs", 1, None).map(|_| ()),
    ] {
        if let Err(e) = r {
            assert_ne!(
                e.wire_code(),
                "23502",
                "must not be rejected as missing op id"
            );
        }
    }
}

#[test]
fn specified_operation_id_keeps_dedup_and_content_mismatch_contract() {
    let (core, _g) = new_core("opt-specified", None);
    let sql = "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a') USING OPERATION_ID 'op-1'";
    ok(&core, "t", sql);
    assert_eq!(code(&core, "t", sql), "23505");
    assert_eq!(
        code(
            &core,
            "t",
            "INSERT INTO docs (id, n, u) VALUES (1, 1, 'different') USING OPERATION_ID 'op-1'"
        ),
        "22023"
    );
    assert_eq!(
        core.operation_recorded(&ctx("t"), "docs", &op("op-1"))
            .expect("lookup"),
        LedgerLookup::Recorded
    );
}

#[test]
fn omitted_and_specified_writes_can_be_mixed() {
    let (core, _g) = new_core("opt-mixed", None);
    ok(&core, "t", "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')");
    ok(
        &core,
        "t",
        "INSERT INTO docs (id, n, u) VALUES (2, 2, 'b') USING OPERATION_ID 'op-mix'",
    );
    ok(&core, "t", "INSERT INTO docs (id, n, u) VALUES (3, 3, 'c')");
    assert_eq!(
        core.last_operation_id(&ctx("t"), "docs").expect("last"),
        LastOperationLookup::Committed(op("op-mix"))
    );
}

#[test]
fn omitted_writes_keep_tenant_isolation() {
    let (core, _g) = new_core("opt-rls", None);
    ok(&core, "a", "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')");
    ok(&core, "b", "INSERT INTO docs (id, n, u) VALUES (1, 1, 'b')");
    ok(&core, "b", "DELETE FROM docs WHERE n > 0");
    assert_eq!(rows(&core, "a"), 1);
    assert_eq!(rows(&core, "b"), 0);
    ok(&core, "b", "TRUNCATE TABLE docs");
    assert_eq!(rows(&core, "a"), 1);
}

#[test]
fn ledgered_mode_still_rejects_omitted_operation_id() {
    let (core, _g) = new_core("opt-ledgered", Some(LedgerMode::Ledgered));
    for sql in [
        "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')",
        "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a') USING OPERATION_ID NULL",
        "UPDATE docs SET u = 'x' WHERE id = 1",
        "UPDATE docs SET u = 'x' WHERE n > 0",
        "DELETE FROM docs WHERE id = 1",
        "DELETE FROM docs WHERE n > 0",
        "TRUNCATE TABLE docs",
    ] {
        assert_eq!(code(&core, "t", sql), "23502", "{sql}");
    }
    assert_eq!(rows(&core, "t"), 0);
}

#[test]
fn partitioned_dml_requires_operation_id_in_every_mode() {
    for (label, mode) in [
        ("part-default", None),
        ("part-ledgered", Some(LedgerMode::Ledgered)),
        (
            "part-compare-only",
            Some(LedgerMode::CompareOnlyWithoutLedger),
        ),
    ] {
        let (core, _g) = new_core(label, mode);
        ok(
            &core,
            "t",
            "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a'), (2, 2, 'b') USING OPERATION_ID 'seed'",
        );
        for sql in [
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID NULL PARTITIONED",
            "DELETE FROM docs WHERE n > 0 PARTITIONED",
            "UPDATE docs SET u = 'x' WHERE n > 0 USING OPERATION_ID NULL PARTITIONED",
            "UPDATE docs SET u = 'x' WHERE n > 0 PARTITIONED CHUNK 1",
            "EXPLAIN DELETE FROM docs WHERE n > 0 USING OPERATION_ID NULL PARTITIONED",
        ] {
            assert_eq!(code(&core, "t", sql), "23502", "{label}: {sql}");
        }
        assert_eq!(rows(&core, "t"), 2, "{label}: rows must be untouched");
    }
}

#[test]
fn bound_partitioned_entries_require_operation_id_by_default() {
    let (core, _g) = new_core("part-bound", None);
    let c = ctx("t");
    let e = core
        .execute_bound_partitioned_delete_in_session(&c, "docs", None, None, |_s| {
            panic!("bind must not run before the operation_id gate")
        })
        .expect_err("must be rejected");
    assert_eq!(e.wire_code(), "23502");
    let e = core
        .execute_bound_partitioned_update_in_session(&c, "docs", None, None, |_s| {
            panic!("bind must not run before the operation_id gate")
        })
        .expect_err("must be rejected");
    assert_eq!(e.wire_code(), "23502");
}
