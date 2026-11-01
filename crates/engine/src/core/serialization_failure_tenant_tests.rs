//! `40001` の可視性境界と副作用ゼロの内部観測（Issue #2407。ポインタ: ERR-7・SQL-38・TABLE-3・
//! TASK-333。設計は `docs/design/explicit-transaction.md` 1a-6・1a-7・1a-11）のクレート内テスト。
//!
//! 役割: 公開 API では作れない `Public` 行（`tenant::insert_typed_row`）と、世代・表世代・台帳の内部
//! 照会が要るケースを `EngineCore` の入口越しに検査する。公開 API だけで書ける 3 テナントの差分比較・
//! 並行ケースは `tests/sql_t01_39/sql31_serialization_failure_tenants.rs` が担う。
//! 判定は `PolicyContext::is_visible` の 1 本であり、テナントの一致ではないことを固定する。

use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::transaction::{SessionTransaction, TransactionStatus};
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::BigInt, false),
        ],
    )
}

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for name in ["ta", "tb"] {
        storage.create_table(&schema(name)).expect("create table");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

/// 可視性の文脈。`public` が偽なら Private だけを見る文脈。
fn ctx(tenant: &str, public: bool) -> PolicyContext {
    let vis: &[Visibility] = if public {
        &[Visibility::Public, Visibility::Private]
    } else {
        &[Visibility::Private]
    };
    PolicyContext::with_visibilities(tenant, vis.iter().copied()).expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant, true), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn insert(table: &str, id: u64, op: &str) -> String {
    format!(
        "INSERT INTO {table} (id, embedding, n) VALUES ({id}, '[1.0, 0.0]', 1) \
         USING OPERATION_ID '{op}'"
    )
}

/// `Public` 行を 1 件 commit する（SQL の INSERT は常に Private なので内部 API を使う）。
fn insert_public(core: &EngineCore, tenant: &str, table: &str, id: u64) {
    crate::tenant::insert_typed_row(
        &core.storage,
        table,
        &ctx(tenant, true),
        id,
        Visibility::Public,
        &[Value::Vector(vec![0.1, 0.2]), Value::BigInt(1)],
        &OperationId::parse(&format!("pub-{tenant}-{id}")).expect("op id"),
    )
    .expect("insert public row");
}

fn begin<'e>(
    core: &'e EngineCore,
    session: &mut SessionState,
    reader: &PolicyContext,
    sqls: &[&str],
) -> SessionTransaction<'e> {
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(reader, session, &mut txn, "BEGIN")
        .expect("begin");
    for sql in sqls {
        core.execute_sql_in_txn(reader, session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    txn
}

fn step<'e>(
    core: &'e EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    reader: &PolicyContext,
    sql: &str,
) -> Result<(), SqlSurfaceError> {
    core.execute_sql_in_txn(reader, session, txn, sql)
        .map(|_| ())
}

fn is_40001(r: &Result<(), SqlSurfaceError>) -> bool {
    matches!(r, Err(SqlSurfaceError::SerializationFailure))
}

/// `Public` 行の commit は、Public を見る文脈の読み手には原因になり、Private だけを見る文脈の
/// 読み手には原因にならない。どのテナントが書き手・読み手でも文脈だけで決まる（可視性の 1 本化）。
#[test]
fn public_commit_is_a_cause_only_for_readers_that_can_see_it() {
    for (writer, wname) in TENANTS.iter().enumerate() {
        for (reader, rname) in TENANTS.iter().enumerate() {
            if writer == reader {
                continue;
            }
            for sees_public in [true, false] {
                let (core, _g) = new_core("sftt-public");
                let rctx = ctx(rname, sees_public);
                let mut session = SessionState::default();
                let mut txn = begin(&core, &mut session, &rctx, &["SELECT id FROM ta"]);
                insert_public(&core, wname, "ta", 1);
                let r = step(
                    &core,
                    &mut session,
                    &mut txn,
                    &rctx,
                    &insert("tb", 9, "r-1"),
                );
                assert_eq!(
                    is_40001(&r),
                    sees_public,
                    "writer={writer} reader={reader} sees_public={sees_public}: {r:?}"
                );
                if sees_public {
                    assert_eq!(txn.status(), TransactionStatus::Failed);
                } else {
                    r.expect("invisible public commit is not a cause");
                    step(&core, &mut session, &mut txn, &rctx, "COMMIT").expect("commit");
                }
            }
        }
    }
}

/// 他テナントの Private 行と Public 行が混ざった世界でも、Private だけを見る読み手の結果は
/// 基準（他テナントの活動なし）と同じ。
#[test]
fn private_only_reader_is_unaffected_by_mixed_foreign_commits() {
    let (core, _g) = new_core("sftt-mixed");
    let rctx = ctx("tenant-a", false);
    let mut session = SessionState::default();
    let mut txn = begin(&core, &mut session, &rctx, &["SELECT id FROM ta"]);
    run(&core, "tenant-b", &insert("ta", 1, "b-1"));
    insert_public(&core, "tenant-c", "ta", 2);
    run(&core, "tenant-c", &insert("ta", 3, "c-1"));
    step(
        &core,
        &mut session,
        &mut txn,
        &rctx,
        &insert("tb", 9, "a-1"),
    )
    .expect("no cause");
    step(&core, &mut session, &mut txn, &rctx, "COMMIT").expect("commit");
}

/// ERR-7: `40001` の後、世代・表世代・`operation_id` 台帳に何も残らず、同じ `operation_id` で
/// 再実行できる。
#[test]
fn no_generation_or_ledger_change_after_40001() {
    let (core, _g) = new_core("sftt-ledger");
    for t in TENANTS {
        let rctx = ctx(t, true);
        let mut session = SessionState::default();
        let mut txn = begin(&core, &mut session, &rctx, &["SELECT id FROM ta"]);
        // 読み手自身の別セッションの commit で衝突を作る。
        let id = 100 + TENANTS.iter().position(|x| *x == t).expect("known") as u64;
        run(&core, t, &insert("ta", id, &format!("own-{t}")));
        let gen_before = core.storage.current_generation().expect("gen");
        let tg_before = core.storage.table_generation("tb").expect("table gen");

        let op = format!("op-{t}");
        let r = step(&core, &mut session, &mut txn, &rctx, &insert("tb", id, &op));
        assert!(is_40001(&r), "{r:?}");
        assert_eq!(core.storage.current_generation().expect("gen"), gen_before);
        assert_eq!(core.storage.table_generation("tb").expect("tg"), tg_before);
        let err = step(&core, &mut session, &mut txn, &rctx, "SELECT 1");
        assert!(err.is_err());
        step(&core, &mut session, &mut txn, &rctx, "ROLLBACK").expect("rollback");
        assert_eq!(txn.status(), TransactionStatus::Idle);

        // 台帳に残っていなければ、同じ operation_id が「初回」として通り、行も入る。
        run(&core, t, &insert("tb", id, &op));
        assert_eq!(
            core.storage.current_generation().expect("gen"),
            gen_before + 1,
            "retry commits exactly once"
        );
    }
}

/// ERR-7: `TRUNCATE` が `40001` になっても、どのテナントの行も消えない（表世代も不変）。
#[test]
fn truncate_rejected_by_40001_leaves_every_tenants_rows() {
    let (core, _g) = new_core("sftt-truncate");
    for (i, t) in TENANTS.iter().enumerate() {
        run(&core, t, &insert("tb", 1 + i as u64, &format!("seed-{t}")));
    }
    let tg_before = core.storage.table_generation("tb").expect("tg");
    let rctx = ctx("tenant-a", true);
    let mut session = SessionState::default();
    let mut txn = begin(&core, &mut session, &rctx, &["SELECT id FROM ta"]);
    run(&core, "tenant-a", &insert("ta", 1, "own-1"));
    let r = step(
        &core,
        &mut session,
        &mut txn,
        &rctx,
        "TRUNCATE TABLE tb USING OPERATION_ID 'tr-1'",
    );
    assert!(is_40001(&r), "{r:?}");
    assert_eq!(core.storage.table_generation("tb").expect("tg"), tg_before);
    step(&core, &mut session, &mut txn, &rctx, "ROLLBACK").expect("rollback");
    for t in TENANTS {
        let mut s = SessionState::default();
        let out = core
            .execute_sql_in_session(&ctx(t, true), &mut s, "SELECT id FROM tb")
            .expect("select");
        match out {
            crate::sql::SqlOutcome::Query(q) => assert_eq!(q.rows.len(), 1, "{t}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
