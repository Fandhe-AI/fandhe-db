//! `ALTER TABLE ... ADD [CONSTRAINT <name>] UNIQUE (...)` ／
//! `ALTER TABLE ... DROP CONSTRAINT <name>`（TABLE-16・TASK-204、Issue #1067）の
//! 結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-204・TASK-205・
//! `docs/spec/04-behavior/data-model.md` TABLE-16・TABLE-17・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・SQL-31・
//! `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
//! `docs/spec/04-behavior/error-format.md` ERR-1・ERR-2・ERR-4・ERR-6。
//!
//! `sql_ddl_add_column.rs`・`unique_constraint.rs`・`table17_foreign_key.rs` と
//! 同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／
//! `CleanupGuard`、`EngineCore::execute_sql_in_session` を production 経路として
//! 検証する）。一意性検査は既存の単一検査点
//! `constraint::table_has_duplicate_unique_key`／
//! `constraint::enforce_unique_keys_in_txn` を再利用するため、ここでは SQL 表層
//! （構文・DDL 権限ゲート・名前解決・エラー分類）の観点のみを検証する。

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
    exec(core, session, ctx, "CREATE TABLE docs (a TEXT, b TEXT)").expect("create table");
}

// --- ADD: 成功系 ----------------------------------------------------------

/// 名前省略の `ADD UNIQUE` は既定名（`<table>_<col>_key`）で確定する
/// （設計 D2）。追加後は重複 INSERT が `23505` で拒否される。
#[test]
fn add_unique_without_name_uses_default_name_and_enforces_uniqueness() {
    let (core, path) = new_core("alter-unique-add-default-name");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD UNIQUE (a)",
    )
    .expect("add unique");

    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert succeeds");
    let err = exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect_err("duplicate value for the new unique constraint must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 別列値なら成功する（制約が `a` 単独に閉じていることの確認）。
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (3, 'x2', 'y') USING OPERATION_ID 'op-3'",
    )
    .expect("distinct value succeeds");
}

/// 明示 `CONSTRAINT <name> UNIQUE` は指定した名前で確定し、`DROP CONSTRAINT`
/// でその名前を指定して削除できる。
#[test]
fn add_named_unique_then_drop_by_name_round_trips() {
    let (core, path) = new_core("alter-unique-add-named");
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
    .expect("add named unique");

    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert succeeds");
    let err = exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect_err("duplicate must be rejected while constraint exists");
    assert_eq!(err.wire_code(), "23505");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT uq_a",
    )
    .expect("drop named unique");

    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2b'",
    )
    .expect("duplicate is allowed again after DROP CONSTRAINT");
}

/// 複合列の `ADD UNIQUE (b, c)` も既定名で確定し強制される。
#[test]
fn add_composite_unique_enforces_column_tuple() {
    let (core, path) = new_core("alter-unique-add-composite");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE docs (a TEXT, b TEXT, c TEXT)",
    )
    .expect("create table");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD UNIQUE (b, c)",
    )
    .expect("add composite unique");

    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b, c) VALUES (1, 'a1', 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert succeeds");
    // `b` は同じでも `c` が違えば許容される。
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b, c) VALUES (2, 'a2', 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect("distinct tuple succeeds");
    let err = exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b, c) VALUES (3, 'a3', 'x', 'y') USING OPERATION_ID 'op-3'",
    )
    .expect_err("same tuple must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

// --- ADD: 既存行の重複 ------------------------------------------------------

/// 追加前に既存行の中で重複がある場合は `23505` で拒否され、副作用ゼロ
/// （制約は追加されない。以後も重複 INSERT が成功する）。
#[test]
fn add_unique_rejects_when_existing_rows_already_duplicate() {
    let (core, path) = new_core("alter-unique-add-existing-dup");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (1, 'dup', 'y1') USING OPERATION_ID 'op-1'",
    )
    .expect("seed row 1");
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (2, 'dup', 'y2') USING OPERATION_ID 'op-2'",
    )
    .expect("seed row 2");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD UNIQUE (a)",
    )
    .expect_err("existing duplicate rows must reject the ADD UNIQUE");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: 制約は追加されておらず、以後も重複 INSERT が成功する。
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (3, 'dup', 'y3') USING OPERATION_ID 'op-3'",
    )
    .expect("constraint was not added; duplicate insert still succeeds");
}

// --- 名前衝突 ---------------------------------------------------------------

/// 明示名が既存の UNIQUE 制約名と衝突すれば `42P07`。
#[test]
fn add_unique_rejects_name_collision_with_existing_unique() {
    let (core, path) = new_core("alter-unique-name-collision-unique");
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
    .expect("first add");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT uq_a UNIQUE (b)",
    )
    .expect_err("duplicate constraint name must be rejected");
    assert_eq!(err.wire_code(), "42P07");
}

/// 明示名が既存の CHECK 制約名と衝突すれば `42P07`（テーブル単位で UNIQUE・
/// CHECK が名前空間を共有する。設計 D1）。
#[test]
fn add_unique_rejects_name_collision_with_existing_check() {
    let (core, path) = new_core("alter-unique-name-collision-check");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE docs (a TEXT, CONSTRAINT ck_a CHECK (a = 'x'))",
    )
    .expect("create table with CHECK");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT ck_a UNIQUE (a)",
    )
    .expect_err("name shared with a CHECK constraint must be rejected");
    assert_eq!(err.wire_code(), "42P07");
}

/// 明示名が既存の FOREIGN KEY 制約名と衝突すれば `42P07`（設計 F1。UNIQUE・
/// CHECK・FOREIGN KEY はテーブル単位の名前空間を共有する。Cursor Bugbot 指摘・
/// PR #1156: `alter_table_add_named_unique_constraint` の衝突検査が UNIQUE・
/// CHECK のみを見ており FOREIGN KEY 名を占有名として扱っていなかったため、
/// 衝突を検出できず後段の `validate_schema` で `Invalid`（`42601`）に丸められ
/// 誤ったエラーコードが返っていた）。
#[test]
fn add_unique_rejects_name_collision_with_existing_foreign_key() {
    let (core, path) = new_core("alter-unique-name-collision-fk");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    )
    .expect("create parents");
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE docs (a TEXT, parent_id BIGINT)",
    )
    .expect("create docs");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT dup_name FOREIGN KEY (parent_id) REFERENCES parents",
    )
    .expect("add foreign key");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT dup_name UNIQUE (a)",
    )
    .expect_err("name shared with a FOREIGN KEY constraint must be rejected");
    assert_eq!(err.wire_code(), "42P07");
}

// --- DROP: エラー系 ----------------------------------------------------------

/// 存在しない制約名の `DROP CONSTRAINT` は `42704`。
#[test]
fn drop_constraint_not_found_is_42704() {
    let (core, path) = new_core("alter-unique-drop-not-found");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT nope",
    )
    .expect_err("dropping an unknown constraint name must fail");
    assert_eq!(err.wire_code(), "42704");
}

/// CHECK 制約名を指定した `DROP CONSTRAINT` は Issue #1068 以降成功する
/// （UNIQUE・CHECK はテーブル単位で名前空間を共有する。設計 D1・D5）。
/// DROP 後は、その CHECK が拒否していた行の INSERT が即座に成功するように
/// なる（同一セッション内で反映。世代 bump による即時反映は既存の
/// `add_then_drop_constraint_take_effect_immediately_in_the_same_session`
/// と同じ契約）。
#[test]
fn drop_constraint_naming_a_check_constraint_succeeds() {
    let (core, path) = new_core("alter-unique-drop-check");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE docs (a TEXT, CONSTRAINT ck_a CHECK (a = 'x'))",
    )
    .expect("create table with CHECK");

    // CHECK が有効な間は違反行の INSERT が拒否される。
    let err = exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a) VALUES (1, 'y') USING OPERATION_ID 'op-1'",
    )
    .expect_err("violating row must be rejected while the CHECK is active");
    assert_eq!(err.wire_code(), "23514");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT ck_a",
    )
    .expect("dropping a CHECK constraint by name must succeed");

    // DROP 後は同じ行が成功する（世代 bump による即時反映）。
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a) VALUES (1, 'y') USING OPERATION_ID 'op-2'",
    )
    .expect("row must be accepted once the CHECK constraint is dropped");

    // 削除済みの名前は以後 `42704`。
    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT ck_a",
    )
    .expect_err("dropping an already-dropped constraint name must fail");
    assert_eq!(err.wire_code(), "42704");
}

/// 主キーは導出擬似名（`<table>_pkey`）で `DROP CONSTRAINT` できる（TABLE-22 (d)、
/// Issue #1196）。削除後の再 DROP は `42704`（PRIMARY KEY 自体は無名。設計 D1）。
#[test]
fn drop_constraint_naming_primary_key_pseudo_name_succeeds_once() {
    let (core, path) = new_core("alter-unique-drop-pkey-name");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE docs (a TEXT, PRIMARY KEY (a))",
    )
    .expect("create table with primary key");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT docs_pkey",
    )
    .expect("primary key is droppable by its derived name");
    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT docs_pkey",
    )
    .expect_err("no primary key is left to drop");
    assert_eq!(err.wire_code(), "42704");
}

// --- FK 依存 ------------------------------------------------------------

/// 子テーブルの `FOREIGN KEY` が参照する UNIQUE 制約は `DROP CONSTRAINT` で
/// 削除できない（`2BP01`）。参照されていない別の UNIQUE は削除できる。
#[test]
fn drop_constraint_referenced_by_foreign_key_is_2bp01() {
    let (core, path) = new_core("alter-unique-drop-fk-dependency");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (code TEXT, other TEXT)",
    )
    .expect("create parents");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents ADD CONSTRAINT uq_code UNIQUE (code)",
    )
    .expect("add uq_code");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents ADD CONSTRAINT uq_other UNIQUE (other)",
    )
    .expect("add uq_other");
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_code TEXT REFERENCES parents (code))",
    )
    .expect("create children with FK to uq_code");

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents DROP CONSTRAINT uq_code",
    )
    .expect_err("dropping a UNIQUE referenced by a FOREIGN KEY must fail");
    assert_eq!(err.wire_code(), "2BP01");

    // 参照されていない `uq_other` は削除できる。
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents DROP CONSTRAINT uq_other",
    )
    .expect("dropping an unreferenced UNIQUE constraint succeeds");
}

// --- 権限・構文 --------------------------------------------------------------

/// DDL 権限の無いセッションは、対象テーブルの存在有無に関わらず常に
/// `42501`（存在オラクルにならない。security.md 対応）。
#[test]
fn add_unique_without_ddl_permission_is_42501_for_existing_and_missing_table() {
    let (core, path) = new_core("alter-unique-no-permission");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut ddl = ddl_session();
    create_docs_table(&core, &mut ddl, &owner);

    let mut plain_session = SessionState::default();
    let err = exec(
        &core,
        &mut plain_session,
        &owner,
        "ALTER TABLE docs ADD UNIQUE (a)",
    )
    .expect_err("no DDL permission on existing table");
    assert_eq!(err.wire_code(), "42501");

    let err = exec(
        &core,
        &mut plain_session,
        &owner,
        "ALTER TABLE missing_table ADD UNIQUE (a)",
    )
    .expect_err("no DDL permission on missing table");
    assert_eq!(err.wire_code(), "42501");
}

/// スコープ外の構文（設計 D6。Issue #1068 以降 `ADD CONSTRAINT ... CHECK`
/// 自体は受理するようになったが、`NOT VALID` を付けた形は既存行の検証を
/// 飛ばす経路であり黙って受理しない）は `42601` で拒否される:
/// `CHECK (...) NOT VALID`、`DROP CONSTRAINT IF EXISTS`。
#[test]
fn out_of_scope_alter_table_forms_are_rejected_with_42601() {
    let (core, path) = new_core("alter-unique-out-of-scope-forms");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT ck_a CHECK (a = 'x') NOT VALID",
    )
    .expect_err("ADD CONSTRAINT ... CHECK ... NOT VALID is out of scope for this Issue");
    assert_eq!(err.wire_code(), "42601");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT uq_a UNIQUE (a)",
    )
    .expect("seed a real constraint to drop with IF EXISTS");
    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT IF EXISTS uq_a",
    )
    .expect_err("DROP CONSTRAINT IF EXISTS is out of scope for this Issue");
    assert_eq!(err.wire_code(), "42601");

    // PRIMARY KEY は無名（Issue #1196）。名前付き形・暗黙の `id` 主キーの再宣言は拒否する。
    for sql in [
        "ALTER TABLE docs ADD CONSTRAINT x PRIMARY KEY (a)",
        "ALTER TABLE docs ADD PRIMARY KEY (id)",
    ] {
        let err = exec(&core, &mut session, &owner, sql).expect_err("out of scope form");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

// --- 明示トランザクション内 --------------------------------------------------

/// 明示トランザクション内の `ALTER TABLE ADD/DROP CONSTRAINT` は、他の DDL と
/// 同じく DDL 権限の有無に関わらず `0A000` で拒否され、トランザクションは
/// `Failed` へ遷移し、制約は変更されない。
#[test]
fn alter_table_add_unique_inside_explicit_transaction_is_rejected_with_0a000() {
    let (core, path) = new_core("alter-unique-explicit-txn");
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
            "ALTER TABLE docs ADD UNIQUE (a)",
        )
        .expect_err("ALTER TABLE ADD UNIQUE inside an explicit transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);

    // 制約が追加されていないことを、重複 INSERT が成功することで確認する。
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 succeeds because no unique constraint was added");
}

// --- 世代（キャッシュ失効）------------------------------------------------

/// ADD/DROP CONSTRAINT 後、既存セッションの次の文から新しい制約が効く
/// （テーブル世代 bump によるキャッシュ失効。他の DDL と同じ契約）。
#[test]
fn add_then_drop_constraint_take_effect_immediately_in_the_same_session() {
    let (core, path) = new_core("alter-unique-generation");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    create_docs_table(&core, &mut session, &owner);

    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("seed row");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD UNIQUE (a)",
    )
    .expect("add unique");
    let err = exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect_err("unique constraint takes effect immediately");
    assert_eq!(err.wire_code(), "23505");

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT docs_a_key",
    )
    .expect("drop the default-named constraint");
    exec(
        &core,
        &mut session,
        &owner,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2b'",
    )
    .expect("drop takes effect immediately");
}
