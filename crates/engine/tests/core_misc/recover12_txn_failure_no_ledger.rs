//! 明示トランザクションの失敗時に `operation_id` 台帳へ何も残らないこと、および読み取り文に
//! 付けた `USING OPERATION_ID` の扱いを固定する結合テスト（Issue #2068）。ポインタ:
//! `docs/spec/04-behavior/` の RECOVER-12・RECOVER-9・SQL-10・SQL-31・ERR-6。
//!
//! 台帳エントリは `COMMIT` と同じ redb の write txn で永続化されるため、`ROLLBACK`・
//! 事前拒否（`23502`・`42601`）の後には残らず、新しい `BEGIN` の内側で同じ `operation_id` を
//! （別の内容でも）使える。`recover12_explicit_txn_resend.rs` が commit 済みの再送判定を担うのに
//! 対し、本ファイルは commit されなかった側（台帳の副作用ゼロ）を台帳照会 API
//! （`EngineCore::operation_recorded`・`last_operation_id`）で直接確かめる。
//! `ROLLBACK TO` 後の再利用は `sql69_savepoint_*` が担うため扱わない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::{LastOperationLookup, LedgerLookup};
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

/// `LedgerMode::Ledgered`（operation_id 必須）の core と一時 DB の後始末ガードを作る。
fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::Ledgered);
    (core, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn step<'a>(
    core: &'a EngineCore,
    caller: &PolicyContext,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_txn(caller, &mut SessionState::default(), txn, sql)
}

fn begin<'a>(core: &'a EngineCore, caller: &PolicyContext) -> SessionTransaction<'a> {
    let mut txn = core.new_session_transaction();
    assert_eq!(
        step(core, caller, &mut txn, "BEGIN").expect("begin"),
        SqlOutcome::Begin
    );
    txn
}

fn commit<'a>(core: &'a EngineCore, caller: &PolicyContext, txn: &mut SessionTransaction<'a>) {
    assert_eq!(
        step(core, caller, txn, "COMMIT").expect("commit"),
        SqlOutcome::Commit
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

/// 失敗済み（`Failed`）の Tx を `ROLLBACK` して `Idle` へ戻す。後続文は `25P02`、`COMMIT` も
/// `25P02` で拒否されることもここで確かめる。
fn rollback_failed<'a>(
    core: &'a EngineCore,
    caller: &PolicyContext,
    txn: &mut SessionTransaction<'a>,
) {
    assert_eq!(txn.status(), TransactionStatus::Failed);
    for sql in [
        "INSERT INTO documents (id, embedding, body) VALUES (99, '[1.0, 0.0]', 'z') \
         USING OPERATION_ID 'blocked-1'",
        "COMMIT",
    ] {
        let err = step(core, caller, txn, sql).expect_err("blocked in a failed transaction");
        assert_eq!(err.wire_code(), "25P02", "{sql}");
    }
    assert_eq!(
        step(core, caller, txn, "ROLLBACK").expect("rollback"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

fn rollback<'a>(core: &'a EngineCore, caller: &PolicyContext, txn: &mut SessionTransaction<'a>) {
    assert_eq!(
        step(core, caller, txn, "ROLLBACK").expect("rollback"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

fn recorded(core: &EngineCore, caller: &PolicyContext, op: &str) -> LedgerLookup {
    core.operation_recorded(caller, TABLE, &OperationId::parse(op).expect("valid op id"))
        .expect("ledger lookup")
}

fn assert_not_recorded(core: &EngineCore, caller: &PolicyContext, ops: &[&str]) {
    for op in ops {
        assert_eq!(
            recorded(core, caller, op),
            LedgerLookup::NotRecorded,
            "{op} must not be in the ledger"
        );
    }
}

fn assert_recorded(core: &EngineCore, caller: &PolicyContext, ops: &[&str]) {
    for op in ops {
        assert_eq!(
            recorded(core, caller, op),
            LedgerLookup::Recorded,
            "{op} must be in the ledger"
        );
    }
}

fn assert_no_last_operation(core: &EngineCore, caller: &PolicyContext) {
    assert_eq!(
        core.last_operation_id(caller, TABLE).expect("last op"),
        LastOperationLookup::NotFound
    );
}

fn row_count(core: &EngineCore, caller: &PolicyContext) -> usize {
    core.execute_sql(caller, "SELECT id FROM documents LIMIT 100")
        .expect("scan")
        .rows
        .len()
}

fn ins(id: u64, body: &str, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[1.0, 0.0]', '{body}') \
         USING OPERATION_ID '{op}'"
    )
}

fn upd(from: &str, to: &str, op: &str) -> String {
    format!("UPDATE documents SET body = '{to}' WHERE body = '{from}' USING OPERATION_ID '{op}'")
}

fn del(body: &str, op: &str) -> String {
    format!("DELETE FROM documents WHERE body = '{body}' USING OPERATION_ID '{op}'")
}

/// 地ならし用: autocommit で 1 行を確定させる（UPDATE・DELETE の対象にする）。
fn seed(core: &EngineCore, caller: &PolicyContext, id: u64, body: &str, op: &str) {
    core.execute_sql_in_session(caller, &mut SessionState::default(), &ins(id, body, op))
        .expect("seed");
}

/// `ROLLBACK` した Tx の INSERT／UPDATE／DELETE は台帳に残らず、新しい `BEGIN` の内側で同じ
/// `operation_id` を再利用できる。commit 後は記録され、さらに新しい `BEGIN` での再送は
/// 台帳由来の `23505`（記録する経路が実在することの対照）になる。
#[test]
fn rollback_then_same_ids_reusable_in_new_begin() {
    let (core, _g) = new_core("recover12-fail-rb");
    let a = ctx("tenant-a");
    seed(&core, &a, 1, "old", "seed-1");
    seed(&core, &a, 2, "gone", "seed-2");

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(10, "new", "t-1")).expect("insert");
    step(&core, &a, &mut txn, &upd("old", "mid", "t-2")).expect("update");
    step(&core, &a, &mut txn, &del("gone", "t-3")).expect("delete");
    rollback(&core, &a, &mut txn);

    assert_not_recorded(&core, &a, &["t-1", "t-2", "t-3"]);
    assert_eq!(
        core.last_operation_id(&a, TABLE).expect("last op"),
        LastOperationLookup::Committed(OperationId::parse("seed-2").expect("op")),
        "last_operation_id must stay at the last committed autocommit statement"
    );
    assert_eq!(row_count(&core, &a), 2);

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(10, "new", "t-1")).expect("insert again");
    step(&core, &a, &mut txn, &upd("old", "mid", "t-2")).expect("update again");
    step(&core, &a, &mut txn, &del("gone", "t-3")).expect("delete again");
    commit(&core, &a, &mut txn);
    assert_recorded(&core, &a, &["t-1", "t-2", "t-3"]);
    assert_eq!(row_count(&core, &a), 2);

    let mut txn = begin(&core, &a);
    let err = step(&core, &a, &mut txn, &ins(10, "new", "t-1")).expect_err("committed resend");
    assert!(
        matches!(err, SqlSurfaceError::DuplicateOperationId),
        "expected DuplicateOperationId, got {err:?}"
    );
    assert_eq!(err.wire_code(), "23505");
    rollback_failed(&core, &a, &mut txn);
}

/// ROLLBACK した `operation_id` を別の内容で再利用しても内容不一致（`22023`）にならない。
/// 台帳に内容ハッシュも残っていないことの証跡。
#[test]
fn rollback_then_same_id_with_different_content_is_not_content_mismatch() {
    let (core, _g) = new_core("recover12-fail-diff");
    let a = ctx("tenant-a");

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "first", "t-1")).expect("insert");
    rollback(&core, &a, &mut txn);
    assert_not_recorded(&core, &a, &["t-1"]);

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(2, "second", "t-1")).expect("different content, same id");
    commit(&core, &a, &mut txn);
    assert_recorded(&core, &a, &["t-1"]);
    assert_eq!(row_count(&core, &a), 1);
}

/// Ledgered 構成で `USING OPERATION_ID` 句を欠いた INSERT／UPDATE／DELETE は `23502` で事前拒否され
/// 台帳を増やさない。同じ Tx で先に成功していた文の ID も ROLLBACK 後に再利用できる。
#[test]
fn missing_operation_id_23502_in_txn_leaves_no_ledger_and_ids_reusable() {
    let missing = [
        (
            "insert",
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[1.0, 0.0]', 'b')".to_string(),
            ins(2, "b", "m-2"),
        ),
        (
            "update",
            "UPDATE documents SET body = 'x' WHERE body = 'a'".to_string(),
            upd("a", "x", "m-2"),
        ),
        (
            "delete",
            "DELETE FROM documents WHERE body = 'a'".to_string(),
            del("a", "m-2"),
        ),
    ];
    for (label, without_clause, with_clause) in missing {
        let (core, _g) = new_core(&format!("recover12-fail-missing-{label}"));
        let a = ctx("tenant-a");
        seed(&core, &a, 1, "a", "seed-1");

        let mut txn = begin(&core, &a);
        step(&core, &a, &mut txn, &ins(5, "c", "m-1")).expect("first statement succeeds");
        let err = step(&core, &a, &mut txn, &without_clause).expect_err("missing clause");
        assert!(
            matches!(err, SqlSurfaceError::MissingOperationId),
            "{label}: expected MissingOperationId, got {err:?}"
        );
        assert_eq!(err.wire_code(), "23502", "{label}");
        assert_eq!(txn.status(), TransactionStatus::Failed, "{label}");
        rollback_failed(&core, &a, &mut txn);
        assert_not_recorded(&core, &a, &["m-1", "m-2"]);
        assert_eq!(row_count(&core, &a), 1, "{label}");

        let mut txn = begin(&core, &a);
        step(&core, &a, &mut txn, &ins(5, "c", "m-1")).expect("reuse m-1");
        step(&core, &a, &mut txn, &with_clause).expect("fixed statement");
        commit(&core, &a, &mut txn);
        assert_recorded(&core, &a, &["m-1", "m-2"]);
    }
}

/// NOT NULL 列を省いた INSERT は `23502` で失敗する。台帳照合は行制約の検査より先に走るため共有
/// write txn へ台帳が書かれうるが、Tx の abort で破棄される。autocommit の同じ失敗も対照にする。
#[test]
fn not_null_violation_23502_in_txn_discards_ledger_entry_of_failed_statement() {
    let (core, _g) = new_core("recover12-fail-notnull");
    let a = ctx("tenant-a");
    let no_body = |op: &str| {
        format!(
            "INSERT INTO documents (id, embedding) VALUES (2, '[1.0, 0.0]') \
             USING OPERATION_ID '{op}'"
        )
    };

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "n-1")).expect("first statement");
    let err = step(&core, &a, &mut txn, &no_body("n-2")).expect_err("not null violation");
    assert_eq!(err.wire_code(), "23502");
    assert!(
        !matches!(err, SqlSurfaceError::MissingOperationId),
        "must be a row constraint failure, got {err:?}"
    );
    rollback_failed(&core, &a, &mut txn);
    assert_not_recorded(&core, &a, &["n-1", "n-2"]);
    assert_no_last_operation(&core, &a);

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "n-1")).expect("reuse n-1");
    step(&core, &a, &mut txn, &ins(2, "b", "n-2")).expect("completed statement reuses n-2");
    commit(&core, &a, &mut txn);
    assert_recorded(&core, &a, &["n-1", "n-2"]);
    assert_eq!(row_count(&core, &a), 2);

    // 対照: autocommit の同じ失敗も台帳を残さず、補った文の再送が成功する。
    let err = core
        .execute_sql_in_session(&a, &mut SessionState::default(), &no_body("n-3"))
        .expect_err("autocommit not null violation");
    assert_eq!(err.wire_code(), "23502");
    assert_not_recorded(&core, &a, &["n-3"]);
    core.execute_sql_in_session(&a, &mut SessionState::default(), &ins(3, "c", "n-3"))
        .expect("completed autocommit statement");
    assert_recorded(&core, &a, &["n-3"]);
}

/// 構文誤り（`42601`）の文は台帳を増やさず、同じ Tx で先に成功していた文の ID も含めて
/// ROLLBACK 後に再利用できる。
#[test]
fn syntax_error_42601_in_txn_leaves_no_ledger_and_ids_reusable() {
    let (core, _g) = new_core("recover12-fail-syntax");
    let a = ctx("tenant-a");

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "s-1")).expect("first statement");
    let err = step(
        &core,
        &a,
        &mut txn,
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[1.0, 0.0]', 'b' \
         USING OPERATION_ID 's-2'",
    )
    .expect_err("syntax error");
    assert!(
        matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }),
        "expected UnsupportedSyntax, got {err:?}"
    );
    assert_eq!(err.wire_code(), "42601");
    rollback_failed(&core, &a, &mut txn);
    assert_not_recorded(&core, &a, &["s-1", "s-2"]);
    assert_no_last_operation(&core, &a);

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "s-1")).expect("reuse s-1");
    step(&core, &a, &mut txn, &ins(2, "b", "s-2")).expect("corrected s-2");
    commit(&core, &a, &mut txn);
    assert_recorded(&core, &a, &["s-1", "s-2"]);
}

/// 読み取り文（kNN・走査・集約・定数・EXPLAIN）に付けた `USING OPERATION_ID` は production 経路の
/// 全入口（autocommit・明示 Tx・`BEGIN READ ONLY`）で `42601` により拒否される。無視されて受理される
/// 形は無く、台帳にも `seen_operation_ids` にも登録されない。
#[test]
fn read_statement_with_operation_id_is_rejected_with_42601_everywhere() {
    let reads = [
        "SELECT id FROM documents ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10 \
         USING OPERATION_ID 'r-1'",
        "SELECT id FROM documents LIMIT 10 USING OPERATION_ID 'r-1'",
        "SELECT COUNT(*) FROM documents USING OPERATION_ID 'r-1'",
        "SELECT 1 USING OPERATION_ID 'r-1'",
        "EXPLAIN SELECT id FROM documents LIMIT 10 USING OPERATION_ID 'r-1'",
    ];
    let (core, _g) = new_core("recover12-fail-read");
    let a = ctx("tenant-a");
    for sql in reads {
        // autocommit
        let err = core
            .execute_sql_in_session(&a, &mut SessionState::default(), sql)
            .expect_err("autocommit read with clause");
        assert!(
            matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }),
            "{sql}: {err:?}"
        );
        assert_eq!(err.wire_code(), "42601", "{sql}");
        assert_not_recorded(&core, &a, &["r-1"]);

        // 明示 Tx と READ ONLY の Tx（parse の段で拒否され 25006 には届かない）。
        for begin_sql in ["BEGIN", "BEGIN READ ONLY"] {
            let mut txn = core.new_session_transaction();
            step(&core, &a, &mut txn, begin_sql).expect("begin");
            let err = step(&core, &a, &mut txn, sql).expect_err("read with clause in txn");
            assert!(
                matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }),
                "{sql}: {err:?}"
            );
            assert_eq!(err.wire_code(), "42601", "{begin_sql}: {sql}");
            assert_eq!(
                txn.status(),
                TransactionStatus::Failed,
                "{begin_sql}: {sql}"
            );
            rollback_failed(&core, &a, &mut txn);
        }
        assert_not_recorded(&core, &a, &["r-1"]);
    }

    // 拒否された読み取りの ID は登録されず、続く書き込みで通常どおり使える。
    seed(&core, &a, 1, "a", "r-1");
    assert_recorded(&core, &a, &["r-1"]);
}

/// 同じ明示 Tx で書き込みに使った ID を読み取り文に付けても、`25000`（Tx 内の ID 再利用）ではなく
/// 構文拒否（`42601`）になり、ROLLBACK 後は書き込みの ID も未記録で、新しい `BEGIN` で再利用できる。
#[test]
fn read_statement_reusing_write_id_in_txn_fails_as_syntax_and_ids_reusable() {
    let (core, _g) = new_core("recover12-fail-read-reuse");
    let a = ctx("tenant-a");

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "w-1")).expect("write");
    let err = step(
        &core,
        &a,
        &mut txn,
        "SELECT id FROM documents LIMIT 10 USING OPERATION_ID 'w-1'",
    )
    .expect_err("read with the id of an earlier write");
    assert_eq!(err.wire_code(), "42601");
    rollback_failed(&core, &a, &mut txn);
    assert_not_recorded(&core, &a, &["w-1"]);

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "w-1")).expect("reuse w-1");
    commit(&core, &a, &mut txn);
    assert_recorded(&core, &a, &["w-1"]);
}

/// 台帳はテナント単位。失敗・ROLLBACK した ID は他テナントの commit に影響されず、エラー文言は
/// 固定で他テナントの情報を含まない。
#[test]
fn failure_ledger_is_tenant_scoped() {
    let (core, _g) = new_core("recover12-fail-tenant");
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");

    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "x-1")).expect("a insert");
    let err = step(
        &core,
        &a,
        &mut txn,
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[1.0, 0.0]', 'b')",
    )
    .expect_err("missing clause");
    assert_eq!(err.wire_code(), "23502");
    assert!(
        !err.client_message().contains("tenant-b"),
        "message must not carry other tenants: {}",
        err.client_message()
    );
    rollback_failed(&core, &a, &mut txn);

    let mut txn = begin(&core, &b);
    step(&core, &b, &mut txn, &ins(1, "b", "x-1")).expect("b insert with the same id");
    commit(&core, &b, &mut txn);
    assert_recorded(&core, &b, &["x-1"]);
    assert_not_recorded(&core, &a, &["x-1"]);

    // a の再利用は b の記録に影響されず成功する。
    let mut txn = begin(&core, &a);
    step(&core, &a, &mut txn, &ins(1, "a", "x-1")).expect("a reuses x-1");
    commit(&core, &a, &mut txn);
    assert_recorded(&core, &a, &["x-1"]);
    assert_eq!(row_count(&core, &a), 1);
    assert_eq!(row_count(&core, &b), 1);
}
