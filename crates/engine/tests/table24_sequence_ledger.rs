//! 採番（`nextval`／`currval`）と `operation_id` 台帳の相互作用の結合テスト（Issue #1692）。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-24 (3)(4)、
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10・RECOVER-11。設計記録は
//! `docs/design/sequence-namespace-and-ledger.md`（D1・D2・D4、§11）。
//!
//! 採番そのものの挙動は `table24_sequence_nextval.rs`（Issue #1690）が担う。本ファイルは
//! 「台帳との相互作用」だけを固定する。検証する契約:
//!
//! - 同じ `operation_id` の再送は台帳が `23505`（`DuplicateOperationId`。行制約由来の
//!   `IdConflict` ではない）で拒否し、再実行も追加の採番もしない。再送は値を再現しない
//! - 内容照合ハッシュは採番前の構文形で計算する。同じ op で内容が違えば `22023` で拒否され、
//!   採番値をリテラルで書いても同一内容とはみなされない
//! - 再起動・明示トランザクション・NoSQL 経路（`execute_bound_insert_in_session`）でも同じ
//! - `operation_id` を省略した書き込みは台帳を通らず、再送は検出されない（欠番になる）
//! - テナントごとに台帳と系列が独立している
//!
//! 「値を消費しない」は、続く払い出しが期待どおりの次の値になることで確かめる。

use engine::catalog::TableSchema;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::{InsertLiteral, SqlSurfaceError, ValidatedInsert};
use engine::sql::exec::Cell;
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

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

/// DB・コア・DDL 権限つきセッションを用意する。SEQUENCE `s`・`s2`（既定）、表 `t (n BIGINT)`、
/// 表 `d (n BIGINT DEFAULT nextval('s'), tag TEXT)` を作る。
fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore, SessionState) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    setup(&core, &mut s);
    (path, guard, core, s)
}

fn setup(core: &EngineCore, s: &mut SessionState) {
    for sql in [
        "CREATE SEQUENCE s",
        "CREATE SEQUENCE s2",
        "CREATE TABLE t (n BIGINT)",
        "CREATE TABLE d (n BIGINT DEFAULT nextval('s'), tag TEXT)",
    ] {
        run(core, "sys", s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn run(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn ok(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> SqlOutcome {
    run(core, tenant, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

/// 失敗するはずの文のエラーを返す。
fn err(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> SqlSurfaceError {
    match run(core, tenant, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

/// 表 `table` の `n` 列を id 昇順で返す。
fn column_n(core: &EngineCore, tenant: &str, table: &str) -> Vec<i64> {
    let r = core
        .execute_sql(
            &ctx(tenant),
            &format!("SELECT id, n FROM {table} LIMIT 1000"),
        )
        .expect("select");
    let mut rows: Vec<(u64, i64)> = r
        .rows
        .iter()
        .map(|row| {
            let id = match row.cells[0] {
                Cell::Integer(v) => v,
                ref o => panic!("id cell {o:?}"),
            };
            let n = match row.cells[1] {
                Cell::SignedInteger(v) => v,
                ref o => panic!("n cell {o:?}"),
            };
            (id, n)
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, n)| n).collect()
}

/// 新しい op で `t` へ 1 行書き、払い出された値を返す（次の値の観測用）。
fn probe_next(core: &EngineCore, tenant: &str, id: u64, op: &str) -> i64 {
    let mut s = ddl_session();
    let out = ok(
        core,
        tenant,
        &mut s,
        &format!(
            "INSERT INTO t (id, n) VALUES ({id}, nextval('s')) RETURNING n \
             USING OPERATION_ID '{op}'"
        ),
    );
    match out {
        SqlOutcome::Returning(o) => match o.result.rows[0].cells[0] {
            Cell::SignedInteger(v) => v,
            ref c => panic!("unexpected cell {c:?}"),
        },
        other => panic!("expected Returning, got {other:?}"),
    }
}

fn t_values(id: u64, op: &str) -> String {
    format!("INSERT INTO t (id, n) VALUES ({id}, nextval('s')) USING OPERATION_ID '{op}'")
}

fn assert_dup_op(e: &SqlSurfaceError) {
    assert!(
        matches!(e, SqlSurfaceError::DuplicateOperationId),
        "expected DuplicateOperationId, got {e:?}"
    );
    assert_eq!(e.wire_code(), "23505");
}

fn assert_mismatch(e: &SqlSurfaceError) {
    assert!(
        matches!(e, SqlSurfaceError::OperationIdContentMismatch),
        "expected OperationIdContentMismatch, got {e:?}"
    );
    assert_eq!(e.wire_code(), "22023");
}

// ---------------------------------------------------------------------
// 1. 同じ op の再送は台帳が 23505 で拒否し、値を消費しない
// ---------------------------------------------------------------------

#[test]
fn resend_of_values_nextval_is_rejected_by_ledger_without_consuming() {
    let (_p, _g, core, mut s) = fixture("seqledger-values");
    ok(&core, "alice", &mut s, &t_values(1, "op1"));
    let e = err(&core, "alice", &mut s, &t_values(1, "op1"));
    assert_dup_op(&e);
    assert_eq!(column_n(&core, "alice", "t"), vec![1]);
    assert_eq!(probe_next(&core, "alice", 2, "op2"), 2);
}

#[test]
fn resend_of_default_nextval_is_rejected_by_ledger_without_consuming() {
    let (_p, _g, core, mut s) = fixture("seqledger-default");
    let sql = "INSERT INTO d (id, tag) VALUES (1, 'a') USING OPERATION_ID 'op1'";
    ok(&core, "alice", &mut s, sql);
    let e = err(&core, "alice", &mut s, sql);
    assert_dup_op(&e);
    assert_eq!(column_n(&core, "alice", "d"), vec![1]);
    assert_eq!(probe_next(&core, "alice", 1, "op2"), 2);
}

#[test]
fn resend_of_multi_row_values_is_rejected_without_consuming() {
    let (_p, _g, core, mut s) = fixture("seqledger-multirow");
    let sql = "INSERT INTO t (id, n) VALUES (1, nextval('s')), (2, nextval('s')), \
               (3, nextval('s')) USING OPERATION_ID 'op1'";
    ok(&core, "alice", &mut s, sql);
    let e = err(&core, "alice", &mut s, sql);
    assert_dup_op(&e);
    assert_eq!(column_n(&core, "alice", "t"), vec![1, 2, 3]);
    assert_eq!(probe_next(&core, "alice", 4, "op2"), 4);
}

#[test]
fn resend_with_returning_does_not_reproduce_the_value() {
    let (_p, _g, core, mut s) = fixture("seqledger-returning");
    let sql = "INSERT INTO t (id, n) VALUES (1, nextval('s')) RETURNING n \
               USING OPERATION_ID 'op1'";
    match ok(&core, "alice", &mut s, sql) {
        SqlOutcome::Returning(o) => assert_eq!(o.result.rows[0].cells[0], Cell::SignedInteger(1)),
        other => panic!("expected Returning, got {other:?}"),
    }
    // 再送は値を返さず、23505 で拒否される（同じ値を再現する経路は無い）。
    let e = err(&core, "alice", &mut s, sql);
    assert_dup_op(&e);
    assert_eq!(probe_next(&core, "alice", 2, "op2"), 2);
}

// ---------------------------------------------------------------------
// 2. 再起動を跨いだ再送
// ---------------------------------------------------------------------

#[test]
fn resend_after_reopen_is_rejected_and_series_continues() {
    let (p, _g, core, mut s) = fixture("seqledger-reopen");
    ok(&core, "alice", &mut s, &t_values(1, "op1"));
    ok(&core, "alice", &mut s, &t_values(2, "op2"));
    drop(core);
    let core = open_core(&p);
    let mut s2 = ddl_session();
    let e = err(&core, "alice", &mut s2, &t_values(2, "op2"));
    assert_dup_op(&e);
    assert_eq!(column_n(&core, "alice", "t"), vec![1, 2]);
    assert_eq!(probe_next(&core, "alice", 3, "op3"), 3);
}

// ---------------------------------------------------------------------
// 3. 明示トランザクションでの再送
// ---------------------------------------------------------------------

#[test]
fn autocommitted_op_resent_inside_begin_is_rejected_and_fails_the_txn() {
    let (_p, _g, core, mut s) = fixture("seqledger-txn-resend");
    ok(&core, "alice", &mut s, &t_values(1, "op1"));
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&c, &mut session, &mut txn, &t_values(1, "op1"))
        .expect_err("resend inside txn must fail");
    assert_dup_op(&e);
    assert_eq!(
        txn.status(),
        engine::sql::transaction::TransactionStatus::Failed
    );
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(column_n(&core, "alice", "t"), vec![1]);
    assert_eq!(probe_next(&core, "alice", 2, "op2"), 2);
}

#[test]
fn txn_committed_op_resent_as_autocommit_is_rejected() {
    let (_p, _g, core, _s) = fixture("seqledger-txn-commit-resend");
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN".to_string(),
        t_values(1, "op1"),
        "COMMIT".to_string(),
    ] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, &sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let mut s = ddl_session();
    let e = err(&core, "alice", &mut s, &t_values(1, "op1"));
    assert_dup_op(&e);
    assert_eq!(column_n(&core, "alice", "t"), vec![1]);
    assert_eq!(probe_next(&core, "alice", 2, "op2"), 2);
}

#[test]
fn same_op_twice_in_one_txn_is_a_transaction_state_error_not_23505() {
    let (_p, _g, core, _s) = fixture("seqledger-txn-same-op");
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, &t_values(1, "op1"))
        .expect("first");
    let e = core
        .execute_sql_in_txn(&c, &mut session, &mut txn, &t_values(2, "op1"))
        .expect_err("same op in the same txn must fail");
    assert!(
        matches!(e, SqlSurfaceError::InvalidTransactionState),
        "expected InvalidTransactionState, got {e:?}"
    );
    assert_ne!(e.wire_code(), "23505");
}

// ---------------------------------------------------------------------
// 4. 内容不一致は 22023（従来どおり）で、値を消費しない
// ---------------------------------------------------------------------

#[test]
fn different_content_under_the_same_op_is_22023_and_consumes_nothing() {
    let (_p, _g, core, mut s) = fixture("seqledger-mismatch");
    ok(&core, "alice", &mut s, &t_values(1, "op1"));
    for sql in [
        // SEQUENCE 名だけが違う。
        "INSERT INTO t (id, n) VALUES (1, nextval('s2')) USING OPERATION_ID 'op1'",
        // nextval を currval に変えた。
        "INSERT INTO t (id, n) VALUES (1, currval('s')) USING OPERATION_ID 'op1'",
        // 1 回目に払い出された値をリテラルで書いた（ハッシュは採番値を含まない）。
        "INSERT INTO t (id, n) VALUES (1, 1) USING OPERATION_ID 'op1'",
    ] {
        let e = err(&core, "alice", &mut s, sql);
        assert_mismatch(&e);
    }
    assert_eq!(column_n(&core, "alice", "t"), vec![1]);
    assert_eq!(probe_next(&core, "alice", 2, "op2"), 2);
    // 拒否された文が参照した s2 も消費されていない（初期値 1 のまま）。
    let out = ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (3, nextval('s2')) RETURNING n USING OPERATION_ID 'op3'",
    );
    match out {
        SqlOutcome::Returning(o) => {
            assert!(matches!(o.result.rows[0].cells[0], Cell::SignedInteger(1)))
        }
        other => panic!("expected Returning, got {other:?}"),
    }
}

#[test]
fn omitted_default_column_changed_to_explicit_is_22023() {
    let (_p, _g, core, mut s) = fixture("seqledger-mismatch-default");
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO d (id, tag) VALUES (1, 'a') USING OPERATION_ID 'op1'",
    );
    // 省略列（DEFAULT 適用）を明示列に変えると、構文形が違うので内容不一致。
    let e = err(
        &core,
        "alice",
        &mut s,
        "INSERT INTO d (id, n, tag) VALUES (1, 1, 'a') USING OPERATION_ID 'op1'",
    );
    assert_mismatch(&e);
    assert_eq!(column_n(&core, "alice", "d"), vec![1]);
    assert_eq!(probe_next(&core, "alice", 1, "op2"), 2);
}

// ---------------------------------------------------------------------
// 5. DEFAULT を変えた後の再送（設計 D2）
// ---------------------------------------------------------------------

#[test]
fn resend_after_default_change_is_a_duplicate_not_a_mismatch() {
    let (_p, _g, core, mut s) = fixture("seqledger-default-change");
    let sql = "INSERT INTO d (id, tag) VALUES (1, 'a') USING OPERATION_ID 'op1'";
    ok(&core, "alice", &mut s, sql);
    ok(
        &core,
        "sys",
        &mut s,
        "ALTER TABLE d ALTER COLUMN n SET DEFAULT nextval('s2')",
    );
    // 省略列の DEFAULT はハッシュの材料に含まれないので、内容不一致にならず重複として拒否される。
    let e = err(&core, "alice", &mut s, sql);
    assert_dup_op(&e);
    // どちらの SEQUENCE も消費していない（s は 2、s2 は 1 が次の値）。
    assert_eq!(probe_next(&core, "alice", 2, "op2"), 2);
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO d (id, tag) VALUES (3, 'c') USING OPERATION_ID 'op3'",
    );
    assert_eq!(column_n(&core, "alice", "d"), vec![1, 1]);
}

// ---------------------------------------------------------------------
// 6. operation_id 省略（既定 = Optional）
// ---------------------------------------------------------------------

#[test]
fn omitted_operation_id_is_not_recorded_and_each_statement_takes_a_new_value() {
    let (_p, _g, core, mut s) = fixture("seqledger-omitted");
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, nextval('s'))",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (2, nextval('s'))",
    );
    assert_eq!(column_n(&core, "alice", "t"), vec![1, 2]);
    let op = OperationId::parse("op1").expect("op id");
    assert_eq!(
        core.operation_recorded(&ctx("alice"), "t", &op)
            .expect("lookup"),
        LedgerLookup::NotRecorded
    );
}

#[test]
fn omitted_operation_id_resend_hits_the_row_constraint_and_leaves_a_gap() {
    let (_p, _g, core, mut s) = fixture("seqledger-omitted-resend");
    let sql = "INSERT INTO t (id, n) VALUES (1, nextval('s'))";
    ok(&core, "alice", &mut s, sql);
    // 台帳を通らないので、拒否は行 id の制約（IdConflict）由来であり DuplicateOperationId ではない。
    let e = err(&core, "alice", &mut s, sql);
    assert!(
        matches!(e, SqlSurfaceError::IdConflict),
        "expected IdConflict, got {e:?}"
    );
    assert_eq!(e.wire_code(), "23505");
    // 失敗した文の払い出し値 2 は欠番になる（PostgreSQL と同じ）。
    assert_eq!(column_n(&core, "alice", "t"), vec![1]);
    assert_eq!(probe_next(&core, "alice", 2, "op2"), 3);
}

// ---------------------------------------------------------------------
// 7. 必須モード
// ---------------------------------------------------------------------

#[test]
fn ledgered_mode_rejects_omitted_operation_id_without_consuming() {
    let path = unique_db_path("seqledger-ledgered");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path).with_ledger_mode(LedgerMode::Ledgered);
    let mut s = ddl_session();
    setup(&core, &mut s);
    let e = err(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, nextval('s'))",
    );
    assert_eq!(e.wire_code(), "23502");
    assert_eq!(probe_next(&core, "alice", 1, "op1"), 1);
}

// ---------------------------------------------------------------------
// 8. NoSQL 経路（execute_bound_insert_in_session）
// ---------------------------------------------------------------------

/// NoSQL 表層の `insert` op と同じ公開 API（`bind_insert` ＋ `execute_bound_insert_in_session`）で
/// 表 `d`（`DEFAULT nextval('s')`）へ 1 行書く。
fn nosql_insert(
    core: &EngineCore,
    tenant: &str,
    id: u64,
    op: Option<&str>,
) -> Result<engine::sql::exec::InsertOutcome, SqlSurfaceError> {
    let op_id = op.map(|o| OperationId::parse(o).expect("op id"));
    core.execute_bound_insert_in_session(
        &ctx(tenant),
        "d",
        1,
        op_id.as_ref(),
        |schema: &TableSchema| {
            let validated = ValidatedInsert {
                table_name: "d".to_string(),
                columns: vec!["id".to_string(), "tag".to_string()],
                rows: vec![vec![
                    InsertLiteral::Number(id.to_string()),
                    InsertLiteral::String("x".to_string()),
                ]],
                operation_id: op_id.clone(),
                returning: None,
                on_conflict: None,
            };
            engine::sql::parser::bind_insert(&validated, schema).map(|b| vec![b])
        },
    )
}

#[test]
fn nosql_default_nextval_resend_is_rejected_and_omitted_takes_a_new_value() {
    let (_p, _g, core, _s) = fixture("seqledger-nosql");
    nosql_insert(&core, "alice", 1, Some("op1")).expect("first");
    let e = nosql_insert(&core, "alice", 1, Some("op1")).expect_err("resend");
    assert_dup_op(&e);
    assert_eq!(column_n(&core, "alice", "d"), vec![1]);
    // 省略は台帳を通らず、新しい値を採番する。
    nosql_insert(&core, "alice", 2, None).expect("omitted");
    assert_eq!(column_n(&core, "alice", "d"), vec![1, 2]);
    assert_eq!(probe_next(&core, "alice", 1, "op3"), 3);
}

// ---------------------------------------------------------------------
// 9. テナント境界
// ---------------------------------------------------------------------

#[test]
fn ledger_and_series_are_independent_per_tenant() {
    let (_p, _g, core, mut s) = fixture("seqledger-tenants");
    ok(&core, "alice", &mut s, &t_values(1, "same-op"));
    // 同じ op 文字列でも別テナントの台帳とは衝突しない。
    let mut sb = ddl_session();
    ok(&core, "bob", &mut sb, &t_values(1, "same-op"));
    assert_eq!(column_n(&core, "alice", "t"), vec![1]);
    assert_eq!(column_n(&core, "bob", "t"), vec![1]);
    // alice の再送が bob の系列を動かさない。
    let e = err(&core, "alice", &mut s, &t_values(1, "same-op"));
    assert_dup_op(&e);
    assert_eq!(probe_next(&core, "bob", 2, "bob-op2"), 2);
    assert_eq!(probe_next(&core, "alice", 2, "alice-op2"), 2);
}
