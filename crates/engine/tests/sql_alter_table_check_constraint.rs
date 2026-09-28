//! `ALTER TABLE ... ADD [CONSTRAINT <name>] CHECK (<述語>)` ／
//! `ALTER TABLE ... DROP CONSTRAINT <name>`（CHECK 名。TABLE-16・TASK-204、
//! Issue #1068）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-204・
//! `docs/spec/04-behavior/data-model.md` TABLE-16・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・SQL-31・
//! `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
//! `docs/spec/04-behavior/error-format.md` ERR-1・ERR-2・ERR-4・ERR-6。
//!
//! `sql_alter_table_unique_constraint.rs`・`table16_check_constraint.rs` と
//! 同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／
//! `CleanupGuard`、`EngineCore::execute_sql_in_session` を production 経路として
//! 検証する）。既存行の走査は
//! `constraint::validate_existing_rows_for_check`（新設）を経由するため、ここでは
//! SQL 表層（構文・DDL 権限ゲート・名前解決・既存行検証・エラー分類）の観点を
//! 検証する。CHECK の書き込み時検査そのもの（`CREATE TABLE` 由来の CHECK）は
//! `table16_check_constraint.rs` がカバー済みで重複させない。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

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
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn exec(
    core: &EngineCore,
    session: &mut SessionState,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, session, sql)
}

fn create_docs_table(core: &EngineCore, session: &mut SessionState, ctx: &PolicyContext) {
    exec(
        core,
        session,
        ctx,
        "CREATE TABLE docs (a TEXT, qty INTEGER)",
    )
    .expect("create table");
}

// --- ADD: 成功系 ----------------------------------------------------------

/// 名前省略の `ADD CHECK` は既定名（`<table>_check`。`CREATE TABLE` の表制約と
/// 同じ既定名アルゴリズム）で確定する（設計 D2）。追加後は違反 INSERT が
/// `23514` で拒否され、適合 INSERT は成功する。
#[test]
fn add_check_without_name_uses_default_name_and_enforces_it() {
    let (core, path) = new_core("alter-check-add-default-name");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    let outcome = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty > 0)",
    )
    .expect("add check without explicit name");
    match outcome {
        SqlOutcome::AlterTable(o) => match o.action {
            engine::sql::ddl::AlterTableAction::AddConstraint { constraint_name } => {
                assert_eq!(constraint_name, "docs_check");
            }
            other => panic!("expected AddConstraint action, got {other:?}"),
        },
        other => panic!("expected AlterTable outcome, got {other:?}"),
    }

    let err = exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'x', -1) USING OPERATION_ID 'op-1'",
    )
    .expect_err("violating row must be rejected");
    assert_eq!(err.wire_code(), "23514");
    assert!(err.client_message().contains("docs_check"));

    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'x', 1) USING OPERATION_ID 'op-2'",
    )
    .expect("satisfying row must be accepted");
}

/// 明示 `CONSTRAINT <name> CHECK` は指定した名前で確定し、`DROP CONSTRAINT`
/// でその名前を指定して削除できる（往復。UNIQUE と同じ名前空間の契約）。
#[test]
fn add_named_check_then_drop_by_name_round_trips() {
    let (core, path) = new_core("alter-check-add-named");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    )
    .expect("add named check");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'x', -1) USING OPERATION_ID 'op-1'",
    )
    .expect_err("violating row must be rejected while the constraint exists");
    assert_eq!(err.wire_code(), "23514");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT qty_ck",
    )
    .expect("drop named check");

    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'x', -1) USING OPERATION_ID 'op-2'",
    )
    .expect("previously violating row is accepted again after DROP CONSTRAINT");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT qty_ck",
    )
    .expect_err("dropping an already-dropped constraint name must fail");
    assert_eq!(err.wire_code(), "42704");
}

/// 既定名の衝突は UNIQUE 実名と CHECK 実名の両方を避けて `_2` 接尾辞へ解決する
/// （設計 D2: `CREATE TABLE` の `validate_and_build` は CHECK 名同士の衝突しか
/// 見ないが、`ALTER TABLE ADD CHECK` は同じ名前空間を共有する既存 UNIQUE 名も
/// 避ける必要がある）。
#[test]
fn add_check_default_name_collision_avoids_both_unique_and_check_names() {
    let (core, path) = new_core("alter-check-default-name-collision");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE docs (a TEXT, qty INTEGER, CONSTRAINT docs_check CHECK (qty > 0))",
    )
    .expect("create table with an explicit docs_check CHECK");

    // 既定名候補 `docs_check` は既存 CHECK と衝突するため `docs_check_2` へ、
    // それも UNIQUE 実名を明示指定して占有しておけば `docs_check_3` へ解決する。
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT docs_check_2 UNIQUE (a)",
    )
    .expect("seed a UNIQUE constraint occupying the first suffix candidate");

    let outcome = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty < 1000)",
    )
    .expect("add check with a colliding default name candidate");
    match outcome {
        SqlOutcome::AlterTable(o) => match o.action {
            engine::sql::ddl::AlterTableAction::AddConstraint { constraint_name } => {
                assert_eq!(constraint_name, "docs_check_3");
            }
            other => panic!("expected AddConstraint action, got {other:?}"),
        },
        other => panic!("expected AlterTable outcome, got {other:?}"),
    }
}

/// 明示名が既存 UNIQUE 名と衝突する場合は `42P07`（逆方向: 明示名が既存 CHECK
/// 名と衝突する場合は `sql_alter_table_unique_constraint.rs` の
/// `add_unique_rejects_name_collision_with_existing_check` が既にカバー済み）。
#[test]
fn add_check_rejects_explicit_name_collision_with_existing_unique() {
    let (core, path) = new_core("alter-check-name-collision-unique");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT uq_a UNIQUE (a)",
    )
    .expect("seed a UNIQUE constraint");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT uq_a CHECK (qty > 0)",
    )
    .expect_err("name shared with a UNIQUE constraint must be rejected");
    assert_eq!(err.wire_code(), "42P07");
}

/// 明示名が既存 CHECK 名と衝突する場合も `42P07`（CHECK 同士）。
#[test]
fn add_check_rejects_explicit_name_collision_with_existing_check() {
    let (core, path) = new_core("alter-check-name-collision-check");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    )
    .expect("seed a CHECK constraint");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty < 1000)",
    )
    .expect_err("name shared with an existing CHECK constraint must be rejected");
    assert_eq!(err.wire_code(), "42P07");
}

/// 参照列が NULL の既存行は違反にしない（三値論理。設計 D6）。
#[test]
fn add_check_null_referenced_column_is_not_a_violation() {
    let (core, path) = new_core("alter-check-null-not-violation");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert a row with NULL qty");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty > 0)",
    )
    .expect(
        "ADD CHECK must succeed: the only existing row has qty = NULL (UNKNOWN, not a violation)",
    );
}

// --- ADD: 既存行の違反（受入基準 1） ----------------------------------------

/// 別テナント・`Private` 可視性を含む既存行が新しい CHECK に違反する場合は
/// `23514` で拒否され、副作用ゼロ（カタログ・世代・行のいずれも不変）を保つ:
/// 同じ違反行の INSERT は以後も成功し、同名で再度 ADD しても `42P07` に
/// ならず、ストレージ再オープン後も制約は存在しない。
#[test]
fn add_check_rejects_existing_cross_tenant_private_violation_with_no_side_effect() {
    let (core, path) = new_core("alter-check-existing-violation");
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    // `other` は `Private` のみを見る `PolicyContext`（`unique_constraint.rs`
    // の `insert_uniqueness_includes_private_rows_not_just_visible_ones` と
    // 同じ手法）で書き込み、実際に `Private` 行として永続化する。owner から
    // 見れば「別テナント・かつ不可視」の行になる。
    let other_private = PolicyContext::with_visibilities("other-tenant", [Visibility::Private])
        .expect("valid tenant");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    // owner 自身の行は述語を満たす（違反しない）ようにし、別テナントの
    // `Private` 行**だけ**が違反する状態を作る。owner の行も違反させると、
    // 走査がテナント境界内に誤って縮退していても owner 自身の行が違反を
    // 検出してしまい、テナント境界を跨いだ走査であることの証明にならない
    // （fail-open の見逃し防止が本テストの主眼。security.md）。DDL は
    // `PolicyContext` を取らない全テナント・全可視性対象の操作であるため、
    // owner から見えない `other-tenant` の `Private` 行も検査対象に入らな
    // ければならない。
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'x', 1) USING OPERATION_ID 'op-owner'",
    )
    .expect("owner's own row satisfies the future CHECK");
    exec(
        &core,
        &mut session,
        &other_private,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'y', -1) USING OPERATION_ID 'op-other-private'",
    )
    .expect("other tenant's Private row (violates the future CHECK, invisible to owner)");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    )
    .expect_err("existing violating rows must reject the ADD CHECK");
    assert_eq!(err.wire_code(), "23514");
    let message = err.client_message();
    assert!(message.contains("qty_ck"));
    assert!(!message.contains("owner"));
    assert!(!message.contains("other-tenant"));

    // 副作用ゼロ: 同じ違反行の INSERT が引き続き成功する。
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (2, 'x2', -1) USING OPERATION_ID 'op-owner-2'",
    )
    .expect("constraint must not have been persisted: violating INSERT still succeeds");

    // 副作用ゼロ: 同名で再度 ADD しても `42P07`（衝突）にはならない
    // （カタログに残っていない）。
    let err2 = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    )
    .expect_err("existing violation must reject again with the same error, not a name conflict");
    assert_eq!(err2.wire_code(), "23514");

    drop(core);
    // 副作用ゼロ: ストレージ再オープン後も制約は存在しない（`checks()` は
    // `pub(crate)` のため schema を直接検査せず、拒否行がなお受理されることで
    // 間接的に確認する。`table16_check_constraint.rs` の永続化テストと同じ
    // 流儀）。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    core.execute_insert_sql(
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (3, 'x3', -1) USING OPERATION_ID 'op-owner-3'",
    )
    .expect("the rejected CHECK constraint must not have survived a reopen either");
}

/// 拒否後に違反行を修正（UPDATE）してから再度 ADD すると成功する。
#[test]
fn add_check_succeeds_after_fixing_the_violating_row() {
    let (core, path) = new_core("alter-check-fix-then-add");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'x', -1) USING OPERATION_ID 'op-1'",
    )
    .expect("insert a violating row");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty > 0)",
    )
    .expect_err("must be rejected while the row still violates");

    exec(
        &core,
        &mut session,
        &owner,
        "UPDATE docs SET qty = 1 WHERE id = 1 USING OPERATION_ID 'op-2'",
    )
    .expect("fix the violating row");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty > 0)",
    )
    .expect("must succeed once the offending row has been fixed");
}

/// 既存行の評価自体が失敗する場合（0 除算）は、制約違反（`23514`）ではなく
/// 通常の式評価エラーと同じ `wire_code`（`22000`）で拒否し、副作用ゼロを保つ
/// （オーナー判断 2026-09-28・Issue #1075 と同じ契約。設計 D4）。
#[test]
fn add_check_existing_row_evaluation_error_returns_22000_with_no_side_effect() {
    let (core, path) = new_core("alter-check-eval-error");
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (1, 'x', 0) USING OPERATION_ID 'op-1'",
    )
    .expect("insert a row that will trigger division by zero");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (100 / qty > 1)",
    )
    .expect_err("division by zero during existing-row evaluation must fail closed");
    assert_eq!(err.wire_code(), "22000");

    drop(core);
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    core.execute_insert_sql(
        &owner,
        "INSERT INTO docs (id, a, qty) VALUES (2, 'x2', 0) USING OPERATION_ID 'op-2'",
    )
    .expect("the rejected CHECK constraint must not have been persisted (qty = 0 accepted)");
}

// --- 名前空間・上限 ----------------------------------------------------------

/// CHECK 制約数の上限（32 件）を超える追加は `54000`。
#[test]
fn add_check_rejects_when_constraint_limit_exceeded() {
    let (core, path) = new_core("alter-check-limit-exceeded");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    for i in 0..32 {
        exec(
            &core,
            &mut session,
            &owner,
            &format!("ALTER TABLE docs ADD CONSTRAINT ck_{i} CHECK (qty < 1000)"),
        )
        .unwrap_or_else(|e| panic!("seed CHECK #{i} must succeed, got {e:?}"));
    }

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty < 2000)",
    )
    .expect_err("the 33rd CHECK constraint must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- DROP ------------------------------------------------------------------

/// UNIQUE 実名の保持回帰（設計 D2 の申し送り）: 既定名衝突で確定した UNIQUE
/// の実名（`t_a_key_2`）は、その後の CHECK 追加・削除・再オープンを経ても
/// 変わらない（`derive_unique_constraint_names` が CHECK 名を使用済み集合に
/// 含むため導出名がずれ得るが、`encode_schema` が実名を保存するため暗黙に
/// 変わらないことを固定する）。
#[test]
fn drop_check_preserves_unique_constraint_real_name_across_reopen() {
    let (core, path) = new_core("alter-check-drop-preserves-unique-name");
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    let mut session = ddl_session();
    // 列制約 UNIQUE の既定名候補は `t_a_key` だが、表制約 CHECK の明示名が
    // 先に占有するため `t_a_key_2` へ確定する（UNIQUE 側の既定名解決は
    // `derive_unique_constraint_names`。CHECK 名を使用済み集合に含む）。
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE t (a TEXT UNIQUE, CONSTRAINT t_a_key CHECK (a = 'x'))",
    )
    .expect("create table");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE t DROP CONSTRAINT t_a_key",
    )
    .expect("dropping the CHECK constraint must succeed");

    drop(core);
    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_drop_constraint("t", "t_a_key_2")
        .expect(
            "the UNIQUE constraint's real name t_a_key_2 must survive the CHECK removal and reopen",
        );
    let err = storage.alter_table_drop_constraint("t", "t_a_key");
    assert!(
        matches!(
            err,
            Err(engine::catalog::CatalogError::ConstraintNotFound(_))
        ),
        "t_a_key must no longer exist after the CHECK was dropped, got {err:?}"
    );
}

/// DROP した CHECK が参照していた列は、以後 `DROP COLUMN` できる
/// （依存が消えたことの確認。`table16_check_constraint.rs` の
/// `drop_column_referenced_by_check_is_rejected` と対になる回帰）。
#[test]
fn drop_check_then_allows_dropping_the_previously_referenced_column() {
    let (core, path) = new_core("alter-check-drop-then-drop-column");
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    )
    .expect("add check referencing qty");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT qty_ck",
    )
    .expect("drop the CHECK constraint");

    drop(core);
    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_drop_column("docs", "qty")
        .expect("dropping qty must now succeed: no CHECK references it any more");
}

// --- 権限・トランザクション --------------------------------------------------

/// DDL 権限が無ければ、対象テーブルの有無に関わらず `42501`（存在オラクルに
/// ならない）。
#[test]
fn add_check_without_ddl_permission_is_42501_for_existing_and_missing_table() {
    let (core, path) = new_core("alter-check-no-ddl-permission");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut ddl = ddl_session();
    create_docs_table(&core, &mut ddl, &owner);

    let mut plain = SessionState::default();
    let err = exec(
        &core,
        &mut plain,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty > 0)",
    )
    .expect_err("no DDL permission on an existing table");
    assert_eq!(err.wire_code(), "42501");

    let err = exec(
        &core,
        &mut plain,
        &owner,
        "ALTER TABLE missing_table ADD CHECK (qty > 0)",
    )
    .expect_err("no DDL permission on a missing table");
    assert_eq!(err.wire_code(), "42501");
}

/// 明示トランザクション内の `ALTER TABLE ADD/DROP CONSTRAINT CHECK` は、他の
/// DDL と同じく `0A000` で拒否され、トランザクションは `Failed` へ遷移し、
/// 制約は変更されない。
#[test]
fn alter_table_add_check_inside_explicit_transaction_is_rejected_with_0a000() {
    let (core, path) = new_core("alter-check-explicit-txn");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "ALTER TABLE docs ADD CHECK (qty > 0)",
        )
        .expect_err("ALTER TABLE ADD CHECK inside an explicit transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

// --- スコープ外構文 ----------------------------------------------------------

/// スコープ外の構文（設計 D1）は `42601` で拒否される: `CHECK (...) NOT VALID`、
/// `CHECK (...) OR ...`。
#[test]
fn out_of_scope_add_check_forms_are_rejected_with_42601() {
    let (core, path) = new_core("alter-check-out-of-scope-forms");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty > 0) NOT VALID",
    )
    .expect_err("NOT VALID is out of scope for this Issue");
    assert_eq!(err.wire_code(), "42601");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CHECK (qty > 0 OR qty < -1000)",
    )
    .expect_err("OR inside a CHECK clause is out of scope (TASK-208・SQL-24)");
    assert_eq!(err.wire_code(), "42601");
}

// --- 永続化 ------------------------------------------------------------------

/// 追加した CHECK はストレージ再オープン後も効く。
#[test]
fn add_check_persists_across_reopen() {
    let (core, path) = new_core("alter-check-persists");
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    )
    .expect("add check");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .execute_insert_sql(
            &owner,
            "INSERT INTO docs (id, a, qty) VALUES (1, 'x', -1) USING OPERATION_ID 'op-1'",
        )
        .expect_err("violating row must still be rejected after reopen");
    assert_eq!(err.wire_code(), "23514");
}
