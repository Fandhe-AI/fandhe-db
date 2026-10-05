//! 明示トランザクションの成否確定手順（RECOVER-12・Issue #1354）を、行制約
//! （`PRIMARY KEY`／`UNIQUE`）を持つテーブルで固定する結合テスト。ポインタ:
//! `docs/spec/04-behavior/` の RECOVER-12・RECOVER-7・RECOVER-10・ERR-2・SQL-31・
//! TABLE-12・TABLE-16。
//!
//! `COMMIT` の応答を受け取れなかったクライアントは、新しい `BEGIN` の内側で元の
//! トランザクションの先頭文から順に同じ `operation_id` で再送して成否を確定する。
//! その判定が成立するのは、書き込み文で `operation_id` 台帳の照合が行制約の検査より
//! **先に**走るためである（`engine::constraint`・`engine::tenant` の呼び出し順）。
//! 台帳由来の重複（`SqlSurfaceError::DuplicateOperationId`）だけが commit 済みの根拠で、
//! 行制約由来（`UniqueViolation`・`IdConflict`）は根拠にならない。両者は `wire_code`
//! （`23505`）を共有するため、本ファイルは variant と固定文言の両方で区別を固定する。
//!
//! `sql31_transaction.rs` の回復系テストは制約を持たない表が対象で、台帳照合が行制約より
//! 先であること自体は守れない（行制約側が先に走る退行でも通る）。本ファイルはその穴を
//! 塞ぐ。autocommit 経路は `unique_constraint.rs` が担う。
//!
//! Issue #1403: 先頭文（または唯一の文）が 0 行の `DELETE` でも同じ再送判定が成立すること
//! （SQL-18・#983 で SQL 表層の 0 行 DELETE も台帳記録）を、単一行形・述語形の両方で固定する。

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 台帳由来の重複（commit 済み再送）の固定文言。pg wire の `M` フィールドにそのまま載る。
const LEDGER_MESSAGE: &str = "operation_id already recorded with the same content";
/// 行制約（`PRIMARY KEY`／`UNIQUE`）由来の固定文言。
const ROW_CONSTRAINT_MESSAGE: &str = "unique constraint violation";
/// 行 `id` 衝突の固定文言。
const ID_CONFLICT_MESSAGE: &str = "row id already exists";

/// 検証する制約構成（`PRIMARY KEY` のみ・`UNIQUE` のみ・両方）。
const TABLE_VARIANTS: [(&str, &str); 3] = [
    ("pk-only", "code TEXT PRIMARY KEY, sku TEXT"),
    ("unique-only", "code TEXT, sku TEXT UNIQUE"),
    ("pk-and-unique", "code TEXT PRIMARY KEY, sku TEXT UNIQUE"),
];

fn new_core(label: &str, columns: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    let sys = ctx("sys");
    core.execute_sql_in_session(
        &sys,
        &mut session,
        &format!("CREATE TABLE orders ({columns})"),
    )
    .expect("create table");
    (core, path)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ins(id: u64, code: &str, sku: &str, op: &str) -> String {
    format!(
        "INSERT INTO orders (id, code, sku) VALUES ({id}, '{code}', '{sku}') \
         USING OPERATION_ID '{op}'"
    )
}

fn visible_rows(core: &EngineCore, caller: &PolicyContext) -> usize {
    core.execute_sql(caller, "SELECT id FROM orders LIMIT 100")
        .expect("scan")
        .rows
        .len()
}

fn step<'a>(
    core: &'a EngineCore,
    caller: &PolicyContext,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_txn(caller, &mut SessionState::default(), txn, sql)
}

/// 新しい `SessionTransaction` を作って `BEGIN` する。
fn begin<'a>(core: &'a EngineCore, caller: &PolicyContext) -> SessionTransaction<'a> {
    let mut txn = core.new_session_transaction();
    assert_eq!(
        step(core, caller, &mut txn, "BEGIN").expect("begin"),
        SqlOutcome::Begin
    );
    txn
}

/// 台帳由来の `23505`（commit 済みの根拠）であること。
fn assert_ledger_duplicate(err: &SqlSurfaceError) {
    assert!(
        matches!(err, SqlSurfaceError::DuplicateOperationId),
        "expected DuplicateOperationId, got {err:?}"
    );
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(err.client_message(), LEDGER_MESSAGE);
    assert_eq!(
        ClassifiedError::error_class(err),
        ErrorClass::DuplicateOperationId
    );
}

/// 行制約由来の `23505`（commit 済みの根拠にならない）であること。
fn assert_row_constraint_violation(err: &SqlSurfaceError) {
    assert!(
        matches!(err, SqlSurfaceError::UniqueViolation),
        "expected UniqueViolation, got {err:?}"
    );
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(err.client_message(), ROW_CONSTRAINT_MESSAGE);
    assert_eq!(
        ClassifiedError::error_class(err),
        ErrorClass::UniqueViolation
    );
}

fn rollback_to_idle<'a>(
    core: &'a EngineCore,
    caller: &PolicyContext,
    txn: &mut SessionTransaction<'a>,
) {
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        step(core, caller, txn, "ROLLBACK").expect("rollback"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

/// commit 済みの 2 文トランザクションを、新しい `BEGIN` の内側で先頭文から再送すると
/// 台帳由来の `23505` になる（行制約由来に落ちない）。制約を持つ 3 構成すべてで固定する。
#[test]
fn resend_of_committed_transaction_is_ledger_duplicate_not_row_constraint() {
    for (label, columns) in TABLE_VARIANTS {
        let (core, path) = new_core(&format!("recover12-committed-{label}"), columns);
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");

        let mut txn = begin(&core, &alice);
        step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "tx-1")).expect("insert 1");
        step(&core, &alice, &mut txn, &ins(2, "c2", "s2", "tx-2")).expect("insert 2");
        assert_eq!(
            step(&core, &alice, &mut txn, "COMMIT").expect("commit"),
            SqlOutcome::Commit
        );

        // 応答を失ったクライアントが新しい BEGIN で先頭文を再送する。
        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "tx-1"))
            .expect_err("resend of a committed statement");
        assert_ledger_duplicate(&err);
        // 失敗後は ROLLBACK 待ち（25P02）で、何も適用されない。
        let blocked = step(&core, &alice, &mut txn, &ins(3, "c3", "s3", "tx-3"))
            .expect_err("statement in a failed transaction");
        assert_eq!(blocked.wire_code(), "25P02", "{label}");
        rollback_to_idle(&core, &alice, &mut txn);
        assert_eq!(visible_rows(&core, &alice), 2, "{label}");
    }
}

/// 対照（非 vacuous 性）: 同じ新 `BEGIN` 文脈でも、台帳に当たらない文は行制約・行 `id` 衝突として
/// 別の variant・文言で拒否される。ここが同じ分類へ潰れると、上のテストは順序の検証にならない。
#[test]
fn colliding_values_with_fresh_operation_id_are_row_constraint_violations() {
    for (label, columns) in TABLE_VARIANTS {
        let (core, path) = new_core(&format!("recover12-control-{label}"), columns);
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &ins(1, "c1", "s1", "seed-1"),
        )
        .expect("seed");

        // 制約列の値は同じで、id・operation_id が新しい。
        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &ins(2, "c1", "s1", "other-2"))
            .expect_err("value collision");
        assert_row_constraint_violation(&err);
        rollback_to_idle(&core, &alice, &mut txn);

        // 行 id が同じで operation_id だけ新しい（制約列の値は別）。
        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &ins(1, "c9", "s9", "other-3"))
            .expect_err("row id collision");
        assert!(
            matches!(err, SqlSurfaceError::IdConflict),
            "expected IdConflict, got {err:?}"
        );
        assert_eq!(err.wire_code(), "23505");
        assert_eq!(err.client_message(), ID_CONFLICT_MESSAGE);
        rollback_to_idle(&core, &alice, &mut txn);
        assert_eq!(visible_rows(&core, &alice), 1, "{label}");
    }
}

/// `COMMIT` 前に接続が断たれたトランザクションは何も残さず、新しい `BEGIN` での再送は
/// 通常どおり成功する。回復後に再び再送すると台帳由来になる（二重適用なし）。
#[test]
fn resend_after_uncommitted_drop_succeeds_then_becomes_ledger_duplicate() {
    for (label, columns) in TABLE_VARIANTS {
        let (core, path) = new_core(&format!("recover12-dropped-{label}"), columns);
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");
        {
            let mut txn = begin(&core, &alice);
            step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "tx-a")).expect("insert a");
            step(&core, &alice, &mut txn, &ins(2, "c2", "s2", "tx-b")).expect("insert b");
            // COMMIT せずに drop（接続断）。
        }
        assert_eq!(visible_rows(&core, &alice), 0, "{label}");

        let mut txn = begin(&core, &alice);
        step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "tx-a")).expect("resend a");
        step(&core, &alice, &mut txn, &ins(2, "c2", "s2", "tx-b")).expect("resend b");
        step(&core, &alice, &mut txn, "COMMIT").expect("commit");
        assert_eq!(visible_rows(&core, &alice), 2, "{label}");

        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "tx-a"))
            .expect_err("re-resend after recovery");
        assert_ledger_duplicate(&err);
        rollback_to_idle(&core, &alice, &mut txn);
        assert_eq!(visible_rows(&core, &alice), 2, "{label}");
    }
}

/// 未 commit のまま、再送文の値が別の確定済み行と衝突する場合は行制約由来の `23505` になり、
/// commit 済みとは判定されない。衝突を解消した後の再送は成功する（台帳に未記録だった証跡）。
#[test]
fn row_constraint_conflict_on_resend_is_not_a_commit_proof() {
    for (label, columns) in TABLE_VARIANTS {
        let (core, path) = new_core(&format!("recover12-conflict-{label}"), columns);
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");
        {
            let mut txn = begin(&core, &alice);
            step(&core, &alice, &mut txn, &ins(5, "e", "se", "r-1")).expect("insert");
            // COMMIT せずに drop（接続断）。
        }
        // 別経路が、別 id・同じ制約列の値を確定させる。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &ins(6, "e", "se", "other-6"),
        )
        .expect("conflicting autocommit insert");

        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &ins(5, "e", "se", "r-1"))
            .expect_err("resend colliding with a committed row");
        assert_row_constraint_violation(&err);
        rollback_to_idle(&core, &alice, &mut txn);

        // 衝突を解消してから同内容で再送すると成功する。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            "DELETE FROM orders WHERE id = 6 USING OPERATION_ID 'del-6'",
        )
        .expect("remove the conflicting row");
        let mut txn = begin(&core, &alice);
        step(&core, &alice, &mut txn, &ins(5, "e", "se", "r-1")).expect("resend after resolve");
        step(&core, &alice, &mut txn, "COMMIT").expect("commit");
        assert_eq!(visible_rows(&core, &alice), 1, "{label}");
    }
}

/// 台帳はテナント単位（RECOVER-2・TABLE-12）。他テナントが commit 済みの `operation_id` と
/// 同内容を送っても自テナントは通常成功し、他テナント側の再送は台帳由来のままになる。
/// エラー文言は固定で、他テナントの情報を含まない。
#[test]
fn ledger_is_tenant_scoped_for_explicit_transaction_resend() {
    let (core, path) = new_core("recover12-tenant", "code TEXT PRIMARY KEY, sku TEXT UNIQUE");
    let _guard = CleanupGuard(path);
    let alice = ctx("tenant-a");
    let bob = ctx("tenant-b");

    let mut txn = begin(&core, &alice);
    step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "tx-1")).expect("alice insert");
    step(&core, &alice, &mut txn, "COMMIT").expect("alice commit");

    // 同じ operation_id・同じ内容・同じ制約列の値でも、bob には alice の台帳も行も見えない。
    let mut txn = begin(&core, &bob);
    step(&core, &bob, &mut txn, &ins(1, "c1", "s1", "tx-1")).expect("bob insert must succeed");
    step(&core, &bob, &mut txn, "COMMIT").expect("bob commit");
    assert_eq!(visible_rows(&core, &bob), 1);

    let mut txn = begin(&core, &alice);
    let err = step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "tx-1"))
        .expect_err("alice resend stays a ledger duplicate");
    assert_ledger_duplicate(&err);
    rollback_to_idle(&core, &alice, &mut txn);
}
/// 0 行 `DELETE` の形（単一行形 `WHERE id = n`／述語形 `WHERE code = 'v'`）。
/// 述語形は `id` 以外の列を使う（`id` 指定は単一行形として束縛されるため）。
#[derive(Clone, Copy, Debug)]
enum DelForm {
    Single,
    Predicate,
}

const DEL_FORMS: [DelForm; 2] = [DelForm::Single, DelForm::Predicate];

/// 行 `n`（id = n・code = `z{n}`）を対象にする `DELETE ... USING OPERATION_ID` 文を組み立てる。
fn del_sql(form: DelForm, n: u64, op: &str) -> String {
    match form {
        DelForm::Single => format!("DELETE FROM orders WHERE id = {n} USING OPERATION_ID '{op}'"),
        DelForm::Predicate => {
            format!("DELETE FROM orders WHERE code = 'z{n}' USING OPERATION_ID '{op}'")
        }
    }
}

/// 対象行 `n` に一致する行を INSERT する文。
fn ins_target(n: u64, op: &str) -> String {
    ins(n, &format!("z{n}"), &format!("s{n}"), op)
}

/// 0 行 `DELETE` が成功し `rows_affected == 0` であることを確かめる。
fn assert_zero_row_delete(out: SqlOutcome) {
    match out {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected zero-row Delete, got {other:?}"),
    }
}

fn commit<'a>(core: &'a EngineCore, caller: &PolicyContext, txn: &mut SessionTransaction<'a>) {
    assert_eq!(
        step(core, caller, txn, "COMMIT").expect("commit"),
        SqlOutcome::Commit
    );
}

/// 唯一の文が 0 行 `DELETE` のトランザクションを commit した後、新しい `BEGIN` で同じ
/// `operation_id` を再送すると台帳由来の `23505` になる（RECOVER-12・SQL-18）。
#[test]
fn zero_row_delete_as_sole_statement_resend_is_ledger_duplicate() {
    for form in DEL_FORMS {
        let (core, path) = new_core("recover12-zero-del-sole", "code TEXT, sku TEXT");
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");

        let mut txn = begin(&core, &alice);
        assert_zero_row_delete(
            step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect("zero-row delete"),
        );
        commit(&core, &alice, &mut txn);

        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1"))
            .expect_err("resend of a committed zero-row delete");
        assert_ledger_duplicate(&err);
        let blocked = step(&core, &alice, &mut txn, &ins_target(9, "zd-x"))
            .expect_err("statement in a failed transaction");
        assert_eq!(blocked.wire_code(), "25P02", "{form:?}");
        rollback_to_idle(&core, &alice, &mut txn);
        assert_eq!(visible_rows(&core, &alice), 0, "{form:?}");
    }
}

/// 先頭文が 0 行 `DELETE`・後続が INSERT のトランザクションを commit した後、新しい `BEGIN` で
/// 先頭文を再送すると台帳由来の `23505` になり、INSERT は二重適用されない（RECOVER-12）。
#[test]
fn zero_row_delete_as_first_statement_resend_is_ledger_duplicate() {
    for form in DEL_FORMS {
        let (core, path) = new_core("recover12-zero-del-first", "code TEXT, sku TEXT");
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");

        let mut txn = begin(&core, &alice);
        assert_zero_row_delete(
            step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect("zero-row delete"),
        );
        step(&core, &alice, &mut txn, &ins(1, "c1", "s1", "zd-ins")).expect("insert");
        commit(&core, &alice, &mut txn);

        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1"))
            .expect_err("resend of the committed first statement");
        assert_ledger_duplicate(&err);
        rollback_to_idle(&core, &alice, &mut txn);
        assert_eq!(visible_rows(&core, &alice), 1, "{form:?}");
    }
}

/// 対照（非 vacuous 性）: COMMIT 前に接続が断たれた 0 行 `DELETE` は台帳を残さず、再送は通常
/// 成功する。commit 後の再送で初めて台帳由来になる（台帳エントリが COMMIT に由来する証跡）。
#[test]
fn zero_row_delete_dropped_before_commit_leaves_no_ledger_entry() {
    for form in DEL_FORMS {
        let (core, path) = new_core("recover12-zero-del-drop", "code TEXT, sku TEXT");
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");
        {
            let mut txn = begin(&core, &alice);
            assert_zero_row_delete(
                step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect("zero-row delete"),
            );
            // COMMIT せずに drop（接続断）。
        }

        let mut txn = begin(&core, &alice);
        assert_zero_row_delete(
            step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect("resend succeeds"),
        );
        commit(&core, &alice, &mut txn);

        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1"))
            .expect_err("re-resend after recovery");
        assert_ledger_duplicate(&err);
        rollback_to_idle(&core, &alice, &mut txn);
    }
}

/// 再送の安全性: 0 行 `DELETE` の commit 後に対象行が別経路で INSERT されても、再送は台帳由来の
/// `23505` で拒否され、後から入った行は削除されない（台帳照合が候補列挙・所有権判定より先）。
#[test]
fn zero_row_delete_resend_does_not_delete_row_inserted_later() {
    for form in DEL_FORMS {
        let (core, path) = new_core("recover12-zero-del-safe", "code TEXT, sku TEXT");
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");

        let mut txn = begin(&core, &alice);
        assert_zero_row_delete(
            step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect("zero-row delete"),
        );
        commit(&core, &alice, &mut txn);

        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &ins_target(7, "zd-ins"),
        )
        .expect("insert the target afterwards");

        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1"))
            .expect_err("resend must be rejected by the ledger");
        assert_ledger_duplicate(&err);
        rollback_to_idle(&core, &alice, &mut txn);
        assert_eq!(visible_rows(&core, &alice), 1, "{form:?}");
    }
}

/// テナント境界（RLS-10・TABLE-12）: 他テナントだけが持つ行への 0 行 `DELETE` は他テナントの行を
/// 変えず、再送は固定文言の台帳由来 `23505`。台帳はテナント単位なので他テナントの処理に影響しない。
#[test]
fn zero_row_delete_resend_is_tenant_scoped() {
    for form in DEL_FORMS {
        let (core, path) = new_core("recover12-zero-del-tenant", "code TEXT, sku TEXT");
        let _guard = CleanupGuard(path);
        let alice = ctx("tenant-a");
        let bob = ctx("tenant-b");
        core.execute_sql_in_session(&bob, &mut SessionState::default(), &ins_target(7, "b-ins"))
            .expect("bob seed");

        let mut txn = begin(&core, &alice);
        assert_zero_row_delete(
            step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect("zero-row delete"),
        );
        commit(&core, &alice, &mut txn);
        assert_eq!(visible_rows(&core, &bob), 1, "{form:?}");

        let mut txn = begin(&core, &alice);
        let err =
            step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect_err("alice resend");
        assert_ledger_duplicate(&err);
        rollback_to_idle(&core, &alice, &mut txn);

        // bob には alice の台帳が見えず、同じ operation_id・同じ文は通常処理（実削除）になる。
        let mut txn = begin(&core, &bob);
        match step(&core, &bob, &mut txn, &del_sql(form, 7, "zd-1")).expect("bob delete") {
            SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1, "{form:?}"),
            other => panic!("expected Delete, got {other:?}"),
        }
        commit(&core, &bob, &mut txn);
        assert_eq!(visible_rows(&core, &bob), 0, "{form:?}");
    }
}

/// 同じ `operation_id` で別の対象を再送すると内容不一致（`22023`）になる（RECOVER-10）。
#[test]
fn zero_row_delete_resend_with_different_target_is_content_mismatch() {
    for form in DEL_FORMS {
        let (core, path) = new_core("recover12-zero-del-mismatch", "code TEXT, sku TEXT");
        let _guard = CleanupGuard(path);
        let alice = ctx("alice");

        let mut txn = begin(&core, &alice);
        assert_zero_row_delete(
            step(&core, &alice, &mut txn, &del_sql(form, 7, "zd-1")).expect("zero-row delete"),
        );
        commit(&core, &alice, &mut txn);

        let mut txn = begin(&core, &alice);
        let err = step(&core, &alice, &mut txn, &del_sql(form, 8, "zd-1"))
            .expect_err("different target under the same operation_id");
        assert!(
            matches!(err, SqlSurfaceError::OperationIdContentMismatch),
            "expected OperationIdContentMismatch, got {err:?}"
        );
        assert_eq!(err.wire_code(), "22023");
        rollback_to_idle(&core, &alice, &mut txn);
    }
}
