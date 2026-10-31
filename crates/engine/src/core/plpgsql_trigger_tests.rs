//! `invoke_trigger_function_in_txn`・`trigger_vars_from_call` の結合テスト（Issue #2228・SQL-68。
//! ADR D9）。本物の `EngineCore`（許可リスト解析・同一 Tx・RLS）に対して、トリガ変数を与えて直接
//! 呼んだトリガ関数の戻り値の意味（行の書き換え・NULL による取りやめ・DELETE の OLD 返却）と、
//! 本体の副作用が呼び出し元の Tx に乗ることを確かめる。発火を通した検証（本番の
//! `TriggerFunctionInvoker`）は #1720 の担当。期待値は PostgreSQL 17 の PL/pgSQL トリガ関数の挙動。

use super::plpgsql_host::{PlCallOptions, TriggerInvocation};
use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::plpgsql::ast::{ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::host::PlScalar;
use crate::plpgsql::interp::TriggerVars;
use crate::plpgsql::parse_body;
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use crate::trigger::call::TriggerOutcome;

fn schema() -> TableSchema {
    TableSchema::new("tt", vec![ColumnDef::new("n", ColumnType::Integer, true)])
}

fn new_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("plpgsql-trigger");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    for ddl in [
        "CREATE TABLE tt (n INTEGER)",
        "CREATE TABLE audit (n INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx("t1"), &mut s, ddl)
            .expect("ddl");
    }
    (core, CleanupGuard(path))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn vars(op: &str, new: Option<i64>, old: Option<i64>) -> TriggerVars {
    let sc = |v: i64| vec![PlScalar::new(Cell::SignedInteger(v), ColumnType::Integer)];
    TriggerVars {
        name: "trg".to_string(),
        when: "BEFORE".to_string(),
        level: "ROW".to_string(),
        op: op.to_string(),
        table_name: "tt".to_string(),
        table_schema: "public".to_string(),
        nargs: 0,
        columns: vec![("n".to_string(), ColumnType::Integer)],
        new: new.map(sc),
        old: old.map(sc),
    }
}

/// 明示 Tx（BEGIN 済み）の上で `src` のトリガ関数を呼ぶ。
fn invoke<'e>(
    core: &'e EngineCore,
    c: &PolicyContext,
    s: &mut SessionState,
    txn: &mut crate::sql::transaction::SessionTransaction<'e>,
    src: &str,
    v: TriggerVars,
) -> Result<TriggerOutcome, SqlSurfaceError> {
    let mut body =
        parse_body(src, &ParseOptions::new(vec![], ReturnShape::Trigger)).expect("parse");
    validate_embedded(&mut body, core).expect("validate");
    let sc = schema();
    core.invoke_trigger_function_in_txn(
        c,
        s,
        txn,
        TriggerInvocation {
            body: &body,
            vars: v,
            schema: &sc,
            opts: PlCallOptions { may_write: true },
        },
    )
}

fn audit_count(core: &EngineCore, tenant: &str) -> u64 {
    let r = core
        .execute_sql(&ctx(tenant), "SELECT COUNT(*) FROM audit")
        .expect("count");
    match r.rows[0].cells[0] {
        Cell::Integer(v) => v,
        ref o => panic!("unexpected {o:?}"),
    }
}

#[test]
fn new_is_rewritten_and_null_skips() {
    let (core, _g) = new_core();
    let c = ctx("t1");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let out = invoke(
        &core,
        &c,
        &mut s,
        &mut txn,
        "BEGIN NEW.n := NEW.n + 1; RETURN NEW; END",
        vars("INSERT", Some(1), None),
    )
    .expect("rewrite");
    assert_eq!(out, TriggerOutcome::Proceed(vec![Value::Integer(2)]));
    let out = invoke(
        &core,
        &c,
        &mut s,
        &mut txn,
        "BEGIN RETURN NULL; END",
        vars("INSERT", Some(1), None),
    )
    .expect("skip");
    assert_eq!(out, TriggerOutcome::Skip);
    // DELETE の非 NULL 返却（OLD）は続行を意味する。
    let out = invoke(
        &core,
        &c,
        &mut s,
        &mut txn,
        "BEGIN RETURN OLD; END",
        vars("DELETE", None, Some(5)),
    )
    .expect("delete old");
    assert_eq!(out, TriggerOutcome::Proceed(vec![Value::Integer(5)]));
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
}

#[test]
fn body_side_effects_ride_the_caller_transaction_and_tenant() {
    let (core, _g) = new_core();
    let c = ctx("t1");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    invoke(
        &core,
        &c,
        &mut s,
        &mut txn,
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END",
        vars("INSERT", Some(7), None),
    )
    .expect("audit insert");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(audit_count(&core, "t1"), 0, "rolled back with the caller");

    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin 2");
    invoke(
        &core,
        &c,
        &mut s,
        &mut txn,
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END",
        vars("INSERT", Some(7), None),
    )
    .expect("audit insert 2");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(audit_count(&core, "t1"), 1);
    assert_eq!(audit_count(&core, "t2"), 0, "other tenants do not see it");
}

#[test]
fn errors_fail_the_transaction_and_idle_is_internal() {
    let (core, _g) = new_core();
    let c = ctx("t1");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    // Tx が無い（Idle）呼び出しは内部エラー（呼び出し側の不変条件違反）。
    let e = invoke(
        &core,
        &c,
        &mut s,
        &mut txn,
        "BEGIN RETURN NEW; END",
        vars("INSERT", Some(1), None),
    )
    .expect_err("idle");
    assert_eq!(e.wire_code(), "XX000");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    // 終端到達は 2F005、その後の文は 25P02。
    let e = invoke(
        &core,
        &c,
        &mut s,
        &mut txn,
        "BEGIN NULL; END",
        vars("INSERT", Some(1), None),
    )
    .expect_err("no return");
    assert_eq!(e.wire_code(), "2F005");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT COUNT(*) FROM audit")
        .expect_err("failed txn");
    assert_eq!(e.wire_code(), "25P02");
    let _ = SqlOutcome::Rollback;
}
