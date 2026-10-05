//! `CREATE TABLE` の名前付き主キー `CONSTRAINT <name> PRIMARY KEY`・主キー重複宣言の
//! `42P16`（TABLE-22 (a)(d)・Issue #1412）の結合テスト。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-22・
//! `docs/spec/04-behavior/error-format.md` ERR-4・ERR-6。
//!
//! `sql_alter_table_primary_key.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として検証する）。

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
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

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    (core, path, session)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn run(core: &EngineCore, session: &mut SessionState, sql: &str) {
    exec(core, session, "owner", sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn fail(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    exec(core, session, "owner", sql)
        .expect_err("statement must fail")
        .wire_code()
        .to_string()
}

fn insert(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    table: &str,
    id: u32,
    a: &str,
    op: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    exec(
        core,
        session,
        tenant,
        &format!("INSERT INTO {table} (id, a) VALUES ({id}, '{a}') USING OPERATION_ID '{op}'"),
    )
}

/// 表制約形: 名前付き PK でテナント内一意性が効き、明示名で DROP でき、導出名は `42704`。
#[test]
fn table_constraint_named_primary_key_is_enforced_and_droppable_by_name() {
    let (core, path, mut session) = setup("create-named-pk-table");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "CREATE TABLE docs (a TEXT, b TEXT, CONSTRAINT pk_docs PRIMARY KEY (a))",
    );
    insert(&core, &mut session, "owner", "docs", 1, "x", "op-1").expect("first");
    assert_eq!(
        insert(&core, &mut session, "owner", "docs", 2, "x", "op-2")
            .expect_err("dup")
            .wire_code(),
        "23505"
    );
    insert(&core, &mut session, "other", "docs", 1, "x", "op-3")
        .expect("same value in another tenant is allowed");
    assert_eq!(
        fail(
            &core,
            &mut session,
            "ALTER TABLE docs DROP CONSTRAINT docs_pkey"
        ),
        "42704"
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE docs DROP CONSTRAINT pk_docs",
    );
    insert(&core, &mut session, "owner", "docs", 3, "x", "op-4").expect("dup allowed after drop");
}

/// 列制約形でも名前付き PK を受け付け、その名前で DROP できる。
#[test]
fn column_constraint_named_primary_key_is_droppable_by_name() {
    let (core, path, mut session) = setup("create-named-pk-column");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "CREATE TABLE docs (a TEXT CONSTRAINT pk_docs PRIMARY KEY, b TEXT)",
    );
    insert(&core, &mut session, "owner", "docs", 1, "x", "op-1").expect("first");
    assert_eq!(
        insert(&core, &mut session, "owner", "docs", 2, "x", "op-2")
            .expect_err("dup")
            .wire_code(),
        "23505"
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE docs DROP CONSTRAINT pk_docs",
    );
}

/// 導出名 `<table>_pkey` と同じ明示名は無名と同じ扱い。
#[test]
fn named_primary_key_equal_to_derived_name_behaves_as_unnamed() {
    let (core, path, mut session) = setup("create-named-pk-derived");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "CREATE TABLE docs (a TEXT, CONSTRAINT docs_pkey PRIMARY KEY (a))",
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE docs DROP CONSTRAINT docs_pkey",
    );
}

/// 重複宣言は `42P16`（InvalidTableDefinition）で、テーブルは作られない（副作用ゼロ）。
#[test]
fn duplicate_primary_key_is_42p16_without_side_effects() {
    let (core, path, mut session) = setup("create-named-pk-dup");
    let _guard = CleanupGuard(path);
    for sql in [
        "CREATE TABLE docs (a TEXT, b TEXT, CONSTRAINT x PRIMARY KEY (a), CONSTRAINT y PRIMARY KEY (b))",
        "CREATE TABLE docs (a TEXT CONSTRAINT x PRIMARY KEY, b TEXT, PRIMARY KEY (b))",
    ] {
        let err = exec(&core, &mut session, "owner", sql).expect_err("must fail");
        assert_eq!(err.wire_code(), "42P16", "{sql}");
        assert_eq!(
            ClassifiedError::error_class(&err),
            ErrorClass::InvalidTableDefinition
        );
    }
    run(
        &core,
        &mut session,
        "CREATE TABLE docs (a TEXT PRIMARY KEY, b TEXT)",
    );
}

/// 既定 UNIQUE 名は明示 PK 名を避け、PK 名は明示名のまま保持される。
#[test]
fn default_unique_name_avoids_explicit_primary_key_name() {
    let (core, path, mut session) = setup("create-named-pk-unique");
    let _guard = CleanupGuard(path.clone());
    run(
        &core,
        &mut session,
        "CREATE TABLE t (a TEXT, b TEXT, CONSTRAINT t_a_key PRIMARY KEY (b), UNIQUE (a))",
    );
    drop(session);
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("t").expect("schema");
    assert_eq!(
        schema.primary_key_constraint_name_effective().as_deref(),
        Some("t_a_key")
    );
    assert_eq!(schema.unique_constraints().len(), 1);
    assert_ne!(schema.unique_constraints()[0].name(), "t_a_key");
}

/// 名前付き PK と自己参照 FK の併用（v13 エンコード経路）・再オープン後も名前が残り、
/// FK 参照中の DROP は `2BP01`。
#[test]
fn named_primary_key_with_foreign_key_survives_reopen_and_blocks_drop() {
    let (core, path, mut session) = setup("create-named-pk-fk");
    let _guard = CleanupGuard(path.clone());
    run(
        &core,
        &mut session,
        "CREATE TABLE t (a TEXT, p TEXT, CONSTRAINT pk_t PRIMARY KEY (a), \
         CONSTRAINT fk_t FOREIGN KEY (p) REFERENCES t (a))",
    );
    assert_eq!(
        fail(&core, &mut session, "ALTER TABLE t DROP CONSTRAINT pk_t"),
        "2BP01"
    );
    drop(session);
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("t").expect("schema");
    assert_eq!(
        schema.primary_key_constraint_name_effective().as_deref(),
        Some("pk_t")
    );
    drop(storage);
    let storage = Storage::open(&path).expect("reopen again");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    assert_eq!(
        fail(&core, &mut session, "ALTER TABLE t DROP CONSTRAINT pk_t"),
        "2BP01"
    );
}

/// 権限ゲート（`42501`）・明示トランザクション（`0A000`）は既存の DDL と同じ。
#[test]
fn permission_and_explicit_transaction_gates() {
    let (core, path, mut session) = setup("create-named-pk-gates");
    let _guard = CleanupGuard(path);
    let mut plain = SessionState::default();
    assert_eq!(
        exec(
            &core,
            &mut plain,
            "owner",
            "CREATE TABLE docs (a TEXT, CONSTRAINT pk PRIMARY KEY (a))"
        )
        .expect_err("no ddl permission")
        .wire_code(),
        "42501"
    );
    let owner = ctx("owner");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "CREATE TABLE docs (a TEXT, CONSTRAINT pk PRIMARY KEY (a))",
        )
        .expect_err("explicit transaction must reject DDL");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

/// 構造段の拒否（名前付き `(id)`・名前衝突）は `42601`。
#[test]
fn structural_rejections_are_42601() {
    let (core, path, mut session) = setup("create-named-pk-reject");
    let _guard = CleanupGuard(path);
    for sql in [
        "CREATE TABLE docs (a TEXT, CONSTRAINT pk PRIMARY KEY (id))",
        "CREATE TABLE docs (a TEXT, CONSTRAINT pk PRIMARY KEY (a), CONSTRAINT pk CHECK (a = 'v'))",
    ] {
        assert_eq!(fail(&core, &mut session, sql), "42601", "{sql}");
    }
}

/// 明示 PK 名と既定 CHECK 名の衝突は `42601`（`XX000`／黙って受理にならない）。
#[test]
fn explicit_primary_key_name_colliding_with_default_check_name_is_42601() {
    let (core, path, mut session) = setup("create-named-pk-check-collide");
    let _guard = CleanupGuard(path);
    assert_eq!(
        fail(
            &core,
            &mut session,
            "CREATE TABLE docs (a TEXT CHECK (a <> ''), CONSTRAINT docs_a_check PRIMARY KEY (a))"
        ),
        "42601"
    );
}

/// 既定 FK 名は明示 PK 名を避ける（FK の実効名が PK 名と異なる）。
#[test]
fn default_foreign_key_name_avoids_explicit_primary_key_name() {
    let (core, path, mut session) = setup("create-named-pk-fk-default");
    let _guard = CleanupGuard(path.clone());
    run(
        &core,
        &mut session,
        "CREATE TABLE t (a TEXT, p TEXT, CONSTRAINT t_p_fkey PRIMARY KEY (a), \
         FOREIGN KEY (p) REFERENCES t (a))",
    );
    drop(session);
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("t").expect("schema");
    assert_eq!(
        schema.primary_key_constraint_name_effective().as_deref(),
        Some("t_p_fkey")
    );
    assert_eq!(schema.foreign_keys().len(), 1);
    assert_ne!(schema.foreign_keys()[0].name(), "t_p_fkey");
}
