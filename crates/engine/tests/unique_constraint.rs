//! UNIQUE 制約（TABLE-16・TASK-204、Issue #905）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-204・`docs/spec/04-behavior/data-model.md`
//! TABLE-16・TABLE-12・`docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)。
//!
//! `sql_create_table.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）。SQL 表層（`CREATE TABLE ... UNIQUE`・
//! `INSERT`・`UPDATE`・`UPSERT`・明示トランザクション）経由の検証を主とし、
//! `Storage::alter_table_add_unique_constraint` は最小限の確認に留める。
//! 一意性検査は主キー（Issue #903）と共有する単一の検査点
//! `constraint::enforce_unique_keys_in_txn` が担う。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::Storage;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant")
}

fn granted_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

// --- CREATE TABLE 構文（列制約・表制約） -------------------------------

#[test]
fn create_table_accepts_column_level_and_table_level_unique() {
    let (core, path) = new_core("uniq-create-ok");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT, c TEXT, UNIQUE (b, c))",
    )
    .expect("CREATE TABLE with column and table UNIQUE constraints must succeed");
}

#[test]
fn create_table_rejects_unique_on_undeclared_column() {
    let (core, path) = new_core("uniq-create-undeclared");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT, UNIQUE (z))",
        )
        .expect_err("UNIQUE referencing an undeclared column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_unique_on_vector_column() {
    let (core, path) = new_core("uniq-create-vector");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4) UNIQUE)",
        )
        .expect_err("UNIQUE on a VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- INSERT: 単一列・複合列・NULL・テナントスコープ ---------------------

#[test]
fn insert_rejects_duplicate_single_column_and_allows_distinct_values() {
    let (core, path) = new_core("uniq-insert-single");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (3, 'y') USING OPERATION_ID 'op-3'",
    )
    .expect("distinct value must succeed");
}

#[test]
fn insert_allows_multiple_null_rows_for_unique_column() {
    let (core, path) = new_core("uniq-insert-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (1) USING OPERATION_ID 'op-1'",
    )
    .expect("first NULL row must succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (2) USING OPERATION_ID 'op-2'",
    )
    .expect("second NULL row must also succeed (NULLS DISTINCT)");
}

#[test]
fn insert_composite_unique_requires_all_columns_to_match() {
    let (core, path) = new_core("uniq-insert-composite");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (b TEXT, c TEXT, UNIQUE (b, c))",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    // 片方だけ一致では違反にならない。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect("partial match must succeed");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, b, c) VALUES (3, 'x', 'y') USING OPERATION_ID 'op-3'",
        )
        .expect_err("full match must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn insert_uniqueness_is_scoped_per_tenant() {
    let (core, path) = new_core("uniq-insert-tenant-scope");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-alice'",
    )
    .expect("alice insert must succeed");
    // 他テナントが同じ値を保持していても成功する（RLS-9）。
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-bob'",
    )
    .expect("bob insert of the same value under a different tenant must succeed");
}

#[test]
fn insert_uniqueness_includes_private_rows_not_just_visible_ones() {
    // 母集合はテナント所有の全行（Public/Private を問わない）であり、可視
    // スナップショットではないことを固定する。書き込みセッション自身は常に
    // 自分の書いた行を書けるため、Private 可視性を持たない別コンテキストで
    // 同じテナントとして重複挿入を試みて確認する。
    let (core, path) = new_core("uniq-insert-private-scope");
    let _guard = CleanupGuard(path);
    let alice_all =
        PolicyContext::with_visibilities("alice", [engine::storage::Visibility::Private])
            .expect("valid tenant");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice_all,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice_all,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert (private) must succeed");
    let err = core
        .execute_insert_sql(
            &alice_all,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate against a private row must still be rejected");
    assert_eq!(err.wire_code(), "23505");
}

// --- バッチ内重複・台帳優先 ---------------------------------------------

#[test]
fn multi_row_insert_batch_rejects_internal_duplicate_with_no_partial_effect() {
    let (core, path) = new_core("uniq-insert-batch");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'x'), (2, 'x') USING OPERATION_ID 'op-batch'",
        )
        .expect_err("batch-internal duplicate must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: id=1 も反映されていない。
    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert!(rows.is_empty(), "no row must have been written");
}

#[test]
fn ledger_duplicate_operation_id_takes_priority_over_unique_violation() {
    // 同一 operation_id・同一内容の再送は、値そのものが重複していても
    // 台帳由来の 23505（DuplicateOperationId）として確定済み処理の再送を示す
    // 契約を維持する（TASK-101・RECOVER-10 の優先順位。UNIQUE 制約導入後も
    // 台帳照合が一意性検査より先に走ることを固定する）。
    let (core, path) = new_core("uniq-ledger-priority");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    // 同一 operation_id・同一内容の再送は台帳由来の 23505
    // （`DuplicateOperationId`。commit 済み確定の根拠）として拒否される。
    // 台帳照合が一意性検査より先に走るため、値そのものが UNIQUE 制約と
    // 衝突していても `UniqueConstraintViolation` 経由の別分類には化けない
    // （両者は現行アーキテクチャでは同一 wire_code `23505` を共有する）。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
        )
        .expect_err("identical resend must be classified as a ledger duplicate (23505)");
    assert_eq!(err.wire_code(), "23505");
}

// --- UPDATE / UPSERT ----------------------------------------------------

#[test]
fn update_rejects_conflicting_value_but_allows_self_assignment() {
    let (core, path) = new_core("uniq-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2");

    let err = core
        .execute_update_sql(
            &alice,
            "UPDATE docs SET a = 'x' WHERE id = 2 USING OPERATION_ID 'op-upd-1'",
        )
        .expect_err("updating to another row's value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 自身の現在値へ再度 SET するのは成功する（自己比較を除外する）。
    core.execute_update_sql(
        &alice,
        "UPDATE docs SET a = 'y' WHERE id = 2 USING OPERATION_ID 'op-upd-2'",
    )
    .expect("re-assigning the current value must succeed");
}

#[test]
fn upsert_do_update_rejects_conflicting_value() {
    let (core, path) = new_core("uniq-upsert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'op-upsert-1'",
        )
        .expect_err("DO UPDATE that collides with another row's value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 新規挿入分岐での違反も拒否される。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (3, 'x') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-2'",
        )
        .expect_err("DO NOTHING branch insert colliding on UNIQUE column must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

/// 複数行 UPSERT のバッチ内で、いずれも既存行と衝突しない「新規挿入」同士
/// （`DO UPDATE` 分岐を経由しない）が UNIQUE 列で衝突するケース（codex-review
/// 指摘・Issue #905 PR レビュー: 単一行版・`DO UPDATE` 版・INSERT バッチ内衝突版の
/// 結合テストは既存だったが、この組み合わせが欠けていた）。
/// 単一の検査点 `constraint::enforce_unique_keys_in_txn` は、書き込んだ行
/// （`written_ids`）同士のキー衝突を第 1 段で検出するため、新規挿入行同士の
/// バッチ内重複もここで拒否される。
#[test]
fn multi_row_upsert_rejects_internal_duplicate_among_new_insert_branches() {
    let (core, path) = new_core("uniq-upsert-batch-new-insert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    // id=1・id=2 はいずれもテーブルに存在しない（新規挿入分岐）。同じ値 'z' を
    // 持つため、既存行との比較では衝突しないがバッチ内候補同士では衝突する。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'z'), (2, 'z') \
             ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'op-upsert-batch-1'",
        )
        .expect_err("new-insert branches colliding with each other must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: どちらの行も反映されていない。
    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert!(rows.is_empty(), "no row must have been written");
}

// --- ALTER TABLE ADD UNIQUE（Rust API）・DROP COLUMN 依存検査 ----------

#[test]
fn alter_table_add_unique_constraint_rejects_existing_duplicates_with_no_side_effect() {
    let (core, path) = new_core("uniq-alter-add-reject");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 (duplicate value, no constraint yet)");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect_err("adding UNIQUE over a column with existing duplicates must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::UniqueConstraintViolation
    ));
    let schema = storage.get_table_schema("docs").expect("schema must exist");
    assert!(
        schema.unique_constraints().is_empty(),
        "the constraint must not have been persisted"
    );
}

#[test]
fn alter_table_add_unique_constraint_succeeds_when_no_duplicates_and_is_enforced_afterward() {
    let (core, path) = new_core("uniq-alter-add-ok");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect("adding UNIQUE with no existing duplicates must succeed");
    let schema = storage.get_table_schema("docs").expect("schema must exist");
    assert_eq!(schema.unique_constraints().len(), 1);

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("the newly added constraint must now be enforced");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn alter_table_drop_column_rejects_when_column_is_used_by_a_unique_constraint() {
    let (core, path) = new_core("uniq-drop-column-dependent");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT)",
    )
    .expect("create table");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_drop_column("docs", "a")
        .expect_err("dropping a column referenced by a UNIQUE constraint must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::DependentObjectsStillExist(_)
    ));
}

// --- 主キーとの併用 -------------------------------------------------------

/// `PRIMARY KEY`（Issue #903）と UNIQUE 制約を同一テーブルで併用した場合も、
/// 単一の検査点がそれぞれのキーを独立に判定する（主キー列は NULL 不可、UNIQUE
/// 列は NULLS DISTINCT）。
#[test]
fn primary_key_and_unique_constraints_are_enforced_independently() {
    let (core, path) = new_core("uniq-with-pk");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (code TEXT PRIMARY KEY, a TEXT UNIQUE)",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, code, a) VALUES (1, 'k1', 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, code, a) VALUES (2, 'k2', 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("UNIQUE violation must be rejected");
    assert_eq!(err.wire_code(), "23505");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, code, a) VALUES (3, 'k1', 'y') USING OPERATION_ID 'op-3'",
        )
        .expect_err("PRIMARY KEY violation must be rejected");
    assert_eq!(err.wire_code(), "23505");
    // UNIQUE 列の NULL は何行でも共存できる。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, code) VALUES (4, 'k4') USING OPERATION_ID 'op-4'",
    )
    .expect("NULL in UNIQUE column must be accepted");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, code) VALUES (5, 'k5') USING OPERATION_ID 'op-5'",
    )
    .expect("second NULL in UNIQUE column must be accepted");
}

// --- 明示トランザクション（SQL-31・TASK-221） -----------------------------

fn count_rows(core: &EngineCore, caller: &PolicyContext) -> usize {
    core.execute_sql(caller, "SELECT id FROM docs LIMIT 100")
        .expect("scan should succeed")
        .rows
        .len()
}

/// 明示トランザクション内の書き込みは共有 write トランザクションに未 commit の
/// まま積まれる。一意性検査は同じ write トランザクション内で走査するため、同一
/// トランザクション内の先行文が書いた未 commit 行との重複も見落とさず `23505`
/// で拒否し、トランザクションは `Failed` へ遷移する（ROLLBACK 後は何も残らない）。
#[test]
fn explicit_transaction_detects_duplicate_against_uncommitted_row_in_same_transaction() {
    let (core, path) = new_core("uniq-txn-dup");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl_session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl_session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    assert_eq!(
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
            .expect("begin"),
        SqlOutcome::Begin
    );
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-t1'",
    )
    .expect("first insert inside the transaction must succeed");
    let err = core
        .execute_sql_in_txn(
            &alice,
            &mut session,
            &mut txn,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-t2'",
        )
        .expect_err("duplicate against an uncommitted row of the same transaction");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(
        count_rows(&core, &alice),
        0,
        "nothing must remain after ROLLBACK"
    );
}

/// 明示トランザクション内の distinct な値は受理され、COMMIT 後は autocommit の
/// 書き込みに対しても一意性が効く。また同一トランザクション内で先に TRUNCATE
/// した（未 commit の削除）行の値は、後続 INSERT の衝突相手にならない。
#[test]
fn explicit_transaction_commits_distinct_values_and_sees_uncommitted_truncate() {
    let (core, path) = new_core("uniq-txn-commit");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl_session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl_session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-c1'",
    )
    .expect("insert x");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-c2'",
    )
    .expect("insert y");
    assert_eq!(
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
            .expect("commit"),
        SqlOutcome::Commit
    );
    assert_eq!(count_rows(&core, &alice), 2);

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (3, 'x') USING OPERATION_ID 'op-c3'",
        )
        .expect_err("committed values must be enforced for later autocommit writes");
    assert_eq!(err.wire_code(), "23505");

    // 同一トランザクション内で TRUNCATE してから同じ値を入れ直すのは成功する
    // （未 commit の削除も同じ write トランザクションの走査に反映される）。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-trunc'",
    )
    .expect("truncate inside the transaction");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (4, 'x') USING OPERATION_ID 'op-c4'",
    )
    .expect("re-inserting a value removed by an uncommitted TRUNCATE must succeed");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count_rows(&core, &alice), 1);
}

/// 一意性違反の応答は、他テナントが同じ値を持つかどうかに依存しない（他テナント
/// の値は母集合に含まれず、違反時の文言も値・テナントを含まない固定文言）。
#[test]
fn unique_violation_response_does_not_depend_on_other_tenants_rows() {
    let (core, path) = new_core("uniq-no-leak");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    // bob だけが 'secret' を保持している状態で、alice の書き込みは成功する。
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, a) VALUES (1, 'secret') USING OPERATION_ID 'op-b1'",
    )
    .expect("bob insert");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'secret') USING OPERATION_ID 'op-a1'",
    )
    .expect("alice insert of a value only bob holds must succeed");

    // alice 自身の重複による違反の文言には値もテナント名も含まれない。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'secret') USING OPERATION_ID 'op-a2'",
        )
        .expect_err("alice's own duplicate must be rejected");
    assert_eq!(err.wire_code(), "23505");
    let message = err.to_string();
    assert!(!message.contains("secret"), "{message}");
    assert!(!message.contains("bob"), "{message}");
    assert!(!message.contains("alice"), "{message}");
}

// --- ALTER TABLE ADD COLUMN（Issue #900）との併用 --------------------------

/// `ALTER TABLE ... ADD COLUMN` は既存の UNIQUE 制約を保持したまま列を追加し
/// （カタログ v6 の往復）、追加後も既存制約の構成列は名前で解決されるため、
/// 列の追加・削除（墓標）で物理位置がずれても検査対象の列を取り違えない。
/// ADD COLUMN 自体は列制約を受理しないため、追加列に UNIQUE は付かない。
#[test]
fn add_column_preserves_existing_unique_constraint_and_column_resolution() {
    let (core, path) = new_core("uniq-add-column");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT, b TEXT UNIQUE)",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'k') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    drop(core);

    // 先頭列 `a` を削除して墓標を作り、`b` の論理位置をずらす。
    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_drop_column("docs", "a")
        .expect("drop a non-constrained column");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    core.execute_sql_in_session(&alice, &mut session, "ALTER TABLE docs ADD COLUMN c TEXT")
        .expect("add column");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "ALTER TABLE docs ADD COLUMN d TEXT UNIQUE",
        )
        .expect_err("ADD COLUMN must not accept a UNIQUE column constraint");
    assert_eq!(err.wire_code(), "42601");

    // 既存制約（`b`）は ADD COLUMN 後も有効で、追加列 `c` を取り違えない。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, b, c) VALUES (2, 'k', 'z') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate on the existing UNIQUE column must still be rejected");
    assert_eq!(err.wire_code(), "23505");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (3, 'm', 'k') USING OPERATION_ID 'op-3'",
    )
    .expect("a value equal to b's in the unconstrained added column must be accepted");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (4, 'n', 'k') USING OPERATION_ID 'op-4'",
    )
    .expect("the added column carries no UNIQUE constraint");
}

// --- UNIQUE 制約の対象型拡張（TABLE-16・TASK-204、Issue #1073） -----------
//
// REAL・DOUBLE PRECISION・NUMERIC・JSON／JSONB・配列型は Rust API
// （`Storage::create_table`／`Storage::alter_table_add_unique_constraint`。
// いずれも `pub`）でのみ宣言できる（SQL 表層の `CREATE TABLE` は列型自体を
// TEXT・VECTOR・INTEGER・BIGINT にしか受理しない。`TableSchema::
// with_unique_constraints`・`UniqueConstraint::new` は `pub(crate)` のため
// このテストファイル〔別クレート〕からは呼べない）。このため以下のテストは
// `Storage::create_table` で列だけを持つスキーマを構築し、`alter_table_
// add_unique_constraint` で列ごとに UNIQUE 制約を追加した後、書き込みは
// production 経路である SQL の `INSERT`／`UPDATE`／UPSERT を経由する。

/// REAL・DOUBLE PRECISION・NUMERIC・JSON・JSONB・配列型それぞれに単一 UNIQUE
/// 制約を持つテーブルを構築するヘルパー。列名は型ごとに固定する。
fn extended_types_schema_without_unique() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new(
                "n",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("j", ColumnType::Json, true),
            ColumnDef::new("jb", ColumnType::Jsonb, true),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
        ],
    )
}

fn new_extended_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&extended_types_schema_without_unique())
        .expect("create table via Rust API (SQL CREATE TABLE cannot declare these types)");
    for column in ["r", "d", "n", "j", "jb", "tags"] {
        storage
            .alter_table_add_unique_constraint("docs", &[column])
            .unwrap_or_else(|e| panic!("add UNIQUE constraint on {column}: {e:?}"));
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// 全列 NULL の行を挿入する（テスト対象の 1 列だけを埋めるための土台）。
fn insert_all_null(core: &EngineCore, caller: &PolicyContext, id: u64, seq: u64) {
    core.execute_insert_sql(
        caller,
        &format!(
            "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2]') \
             USING OPERATION_ID 'seed-{id}-{seq}'"
        ),
    )
    .expect("insert of all-NULL row must succeed");
}

/// REAL・DOUBLE PRECISION 列の UNIQUE 制約は `-0.0` と `0.0` を同一値として
/// 扱う（`scalar_float::canonicalize_*` による正規化。Issue #1073）。
#[test]
fn real_and_double_unique_treats_negative_zero_as_equal_to_positive_zero() {
    let (core, path) = new_extended_core("uniq-ext-real-double-neg-zero");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, r, d) VALUES (1, '[0.1,0.2]', 0.0, 0.0) \
         USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, r, d) VALUES (2, '[0.1,0.2]', -0.0, 1.0) \
             USING OPERATION_ID 'op-2'",
        )
        .expect_err("-0.0 must conflict with an existing 0.0 on the REAL UNIQUE column");
    assert_eq!(err.wire_code(), "23505");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, r, d) VALUES (3, '[0.1,0.2]', 1.0, -0.0) \
             USING OPERATION_ID 'op-3'",
        )
        .expect_err(
            "-0.0 must conflict with an existing 0.0 on the DOUBLE PRECISION UNIQUE column",
        );
    assert_eq!(err.wire_code(), "23505");

    // 値として異なる REAL は成功する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, r, d) VALUES (4, '[0.1,0.2]', 1.0, 1.0) \
         USING OPERATION_ID 'op-4'",
    )
    .expect("a distinct REAL/DOUBLE value must succeed");
}

/// NUMERIC 列の UNIQUE 制約は表現の揺れ（末尾ゼロの有無）を同一値として扱う
/// （`1.5` と `1.50`）。
#[test]
fn numeric_unique_treats_trailing_zero_representations_as_equal() {
    let (core, path) = new_extended_core("uniq-ext-numeric-trailing-zero");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, n) VALUES (1, '[0.1,0.2]', 1.5) USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, n) VALUES (2, '[0.1,0.2]', 1.50) USING OPERATION_ID 'op-2'",
        )
        .expect_err("1.50 must conflict with an existing 1.5 on the NUMERIC UNIQUE column");
    assert_eq!(err.wire_code(), "23505");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, n) VALUES (3, '[0.1,0.2]', 1.51) USING OPERATION_ID 'op-3'",
    )
    .expect("a distinct NUMERIC value must succeed");
}

/// JSON・JSONB 列の UNIQUE 制約は、格納テキストのキー順・空白・数値表現が
/// 異なっても値として等価なら衝突する（`json::canonical_equality_text`
/// 経由。Issue #1073）。
#[test]
fn json_and_jsonb_unique_treats_textual_variants_as_equal_values() {
    let (core, path) = new_extended_core("uniq-ext-json-textual-variants");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, j, jb) VALUES (1, '[0.1,0.2]', '{\"a\":1,\"b\":2}', '{\"a\":1,\"b\":2}') \
         USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");

    // キー順・空白・数値表現（`1` vs `1.0`）が異なるが値として等価な JSON。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, j) VALUES (2, '[0.1,0.2]', '{ \"b\": 2, \"a\": 1.0 }') \
             USING OPERATION_ID 'op-2'",
        )
        .expect_err("a textually different but value-equal JSON must conflict");
    assert_eq!(err.wire_code(), "23505");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, jb) VALUES (3, '[0.1,0.2]', '{ \"b\": 2, \"a\": 1.0 }') \
             USING OPERATION_ID 'op-3'",
        )
        .expect_err("the same holds for the JSONB column");
    assert_eq!(err.wire_code(), "23505");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, j) VALUES (4, '[0.1,0.2]', '{\"a\":1,\"b\":3}') \
         USING OPERATION_ID 'op-4'",
    )
    .expect("a value-distinct JSON must succeed");
}

/// 配列列の UNIQUE 制約は要素順を区別する（`{a,b}` と `{b,a}` は衝突しない）。
#[test]
fn array_unique_distinguishes_element_order() {
    let (core, path) = new_extended_core("uniq-ext-array-order");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{a,b}') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, tags) VALUES (2, '[0.1,0.2]', '{b,a}') USING OPERATION_ID 'op-2'",
    )
    .expect("a different element order must not conflict");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, tags) VALUES (3, '[0.1,0.2]', '{a,b}') USING OPERATION_ID 'op-3'",
        )
        .expect_err("an identical array must conflict");
    assert_eq!(err.wire_code(), "23505");
}

/// 拡張型 UNIQUE 制約も NULL 複数行を許容する（NULLS DISTINCT。既存の TEXT 版
/// 一意性テストと同じ挙動）。
#[test]
fn extended_types_unique_allows_multiple_null_rows() {
    let (core, path) = new_extended_core("uniq-ext-null-rows");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    insert_all_null(&core, &alice, 1, 1);
    insert_all_null(&core, &alice, 2, 2);
}

/// 拡張型 UNIQUE 制約もテナント境界に閉じる（別テナントは同値でも成功する）。
#[test]
fn extended_types_unique_is_scoped_per_tenant() {
    let (core, path) = new_extended_core("uniq-ext-tenant-scope");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, n) VALUES (1, '[0.1,0.2]', 2.00) USING OPERATION_ID 'op-a-1'",
    )
    .expect("alice insert must succeed");
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, embedding, n) VALUES (1, '[0.1,0.2]', 2.0) USING OPERATION_ID 'op-b-1'",
    )
    .expect("bob insert with the same value must succeed (tenant boundary)");
}

/// NUMERIC 列への UPDATE は衝突する値を拒否し、自己代入は成功する
/// （既存の TEXT 版 `update_rejects_conflicting_value_but_allows_self_assignment`
/// と同じ挙動が拡張型でも成り立つことの固定）。
#[test]
fn update_on_numeric_unique_column_rejects_conflicting_value_but_allows_self_assignment() {
    let (core, path) = new_extended_core("uniq-ext-numeric-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, n) VALUES (1, '[0.1,0.2]', 1.00) USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, n) VALUES (2, '[0.1,0.2]', 2.00) USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2");

    let err = core
        .execute_update_sql(
            &alice,
            "UPDATE docs SET n = 1.0 WHERE id = 2 USING OPERATION_ID 'op-upd-1'",
        )
        .expect_err("updating to another row's value (even under a different textual form) must be rejected");
    assert_eq!(err.wire_code(), "23505");

    core.execute_update_sql(
        &alice,
        "UPDATE docs SET n = 2.0 WHERE id = 2 USING OPERATION_ID 'op-upd-2'",
    )
    .expect("re-assigning the current value must succeed");
}

/// 複数行 INSERT バッチ内の JSON 値衝突（値として等価だがテキストが異なる）は
/// 副作用ゼロで拒否される（`multi_row_insert_batch_rejects_internal_duplicate_with_no_partial_effect`
/// の拡張型版）。
#[test]
fn multi_row_insert_batch_rejects_json_internal_duplicate_with_no_partial_effect() {
    let (core, path) = new_extended_core("uniq-ext-json-batch-dup");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, j) VALUES \
             (1, '[0.1,0.2]', '{\"a\":1}'), (2, '[0.1,0.2]', '{ \"a\": 1.0 }') \
             USING OPERATION_ID 'op-batch-1'",
        )
        .expect_err("value-equal JSON within the same batch must be rejected");
    assert_eq!(err.wire_code(), "23505");

    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert!(rows.is_empty(), "no row must have been written");
}

/// **最重要のテスト**: `ALTER TABLE ... ADD UNIQUE`（Rust API）は、制約追加前
/// の既存行に「値として等価だがテキストが異なる」JSON の重複があれば
/// `CatalogError::UniqueConstraintViolation` で拒否し、制約をカタログへ
/// 永続化しない（`table_has_duplicate_unique_key` が
/// `constraint::push_canonical_component` と同じ正準キーで判定するため。
/// Issue #1073）。
#[test]
fn alter_table_add_unique_constraint_rejects_existing_json_duplicates_that_differ_only_textually() {
    let schema_without_unique = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("j", ColumnType::Json, true),
        ],
    );
    let path = unique_db_path("uniq-ext-alter-add-json-reject");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_without_unique)
            .expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let alice = ctx("alice");
        core.execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, j) VALUES (1, '[0.1,0.2]', '{\"a\":1,\"b\":2}') \
             USING OPERATION_ID 'op-1'",
        )
        .expect("insert 1");
        // キー順・空白だけが異なるが値として等価な JSON（制約追加前は無制約
        // なので受理される）。
        core.execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, j) VALUES (2, '[0.1,0.2]', '{ \"b\": 2, \"a\": 1 }') \
             USING OPERATION_ID 'op-2'",
        )
        .expect("insert 2 (duplicate value, no constraint yet)");
    }

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_add_unique_constraint("docs", &["j"])
        .expect_err(
            "adding UNIQUE over value-equal-but-textually-different JSON rows must be rejected",
        );
    assert!(matches!(
        err,
        engine::catalog::CatalogError::UniqueConstraintViolation
    ));
    let schema = storage.get_table_schema("docs").expect("schema must exist");
    assert!(
        schema.unique_constraints().is_empty(),
        "the constraint must not have been persisted"
    );
}
