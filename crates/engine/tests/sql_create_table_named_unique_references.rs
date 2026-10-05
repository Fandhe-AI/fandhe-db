//! `CREATE TABLE` の名前付き `UNIQUE`（表制約・列制約）と列制約の名前付き
//! `REFERENCES`（TABLE-22・Issue #1428）の結合テスト。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-22・
//! `docs/spec/04-behavior/error-format.md` ERR-4・ERR-6。
//!
//! `sql_create_table_named_primary_key.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`EngineCore::execute_sql_in_session` を production 経路として
//! 検証する）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
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

#[allow(clippy::too_many_arguments)]
fn insert(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    table: &str,
    col: &str,
    id: u32,
    val: &str,
    op: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    exec(
        core,
        session,
        tenant,
        &format!(
            "INSERT INTO {table} (id, {col}) VALUES ({id}, '{val}') USING OPERATION_ID '{op}'"
        ),
    )
}

fn code(r: Result<SqlOutcome, SqlSurfaceError>) -> String {
    r.expect_err("must fail").wire_code().to_string()
}

/// 表制約・列制約いずれの名前付き UNIQUE も、テナント内一意性が効き、明示名で
/// DROP でき、既定名は `42704`。
#[test]
fn named_unique_table_and_column_forms_are_enforced_and_droppable_by_name() {
    for (label, ddl, default_name) in [
        (
            "create-named-unique-table",
            "CREATE TABLE docs (a TEXT, CONSTRAINT u_docs UNIQUE (a))",
            "docs_a_key",
        ),
        (
            "create-named-unique-column",
            "CREATE TABLE docs (a TEXT CONSTRAINT u_docs UNIQUE)",
            "docs_a_key",
        ),
    ] {
        let (core, path, mut session) = setup(label);
        let _guard = CleanupGuard(path);
        run(&core, &mut session, ddl);
        insert(&core, &mut session, "owner", "docs", "a", 1, "x", "op-1").expect("first");
        assert_eq!(
            code(insert(
                &core,
                &mut session,
                "owner",
                "docs",
                "a",
                2,
                "x",
                "op-2"
            )),
            "23505"
        );
        insert(&core, &mut session, "other", "docs", "a", 1, "x", "op-3")
            .expect("same value in another tenant is allowed");
        assert_eq!(
            fail(
                &core,
                &mut session,
                &format!("ALTER TABLE docs DROP CONSTRAINT {default_name}")
            ),
            "42704"
        );
        run(
            &core,
            &mut session,
            "ALTER TABLE docs DROP CONSTRAINT u_docs",
        );
        insert(&core, &mut session, "owner", "docs", "a", 3, "x", "op-4")
            .expect("dup allowed after drop");
    }
}

/// 列制約の名前付き REFERENCES: 親の存在が要求され、他テナントの親では満たされず、
/// 明示名で DROP でき、既定名は `42704`。
#[test]
fn named_column_references_is_enforced_and_droppable_by_name() {
    let (core, path, mut session) = setup("create-named-fk-column");
    let _guard = CleanupGuard(path);
    run(&core, &mut session, "CREATE TABLE p (k TEXT UNIQUE)");
    run(
        &core,
        &mut session,
        "CREATE TABLE c (v TEXT CONSTRAINT fk_c REFERENCES p (k))",
    );
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            "c",
            "v",
            1,
            "x",
            "op-1"
        )),
        "23503"
    );
    insert(&core, &mut session, "other", "p", "k", 1, "x", "op-2").expect("other parent");
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            "c",
            "v",
            2,
            "x",
            "op-3"
        )),
        "23503",
        "another tenant's parent must not satisfy the reference"
    );
    insert(&core, &mut session, "owner", "p", "k", 1, "x", "op-4").expect("own parent");
    insert(&core, &mut session, "owner", "c", "v", 3, "x", "op-5").expect("child");
    assert_eq!(
        fail(
            &core,
            &mut session,
            "ALTER TABLE c DROP CONSTRAINT c_v_fkey"
        ),
        "42704"
    );
    run(&core, &mut session, "ALTER TABLE c DROP CONSTRAINT fk_c");
    insert(&core, &mut session, "owner", "c", "v", 4, "orphan", "op-6")
        .expect("orphan allowed after drop");
}

/// 再オープン後も名前付き UNIQUE・名前付き列 FK の名前が保たれ、明示名で DROP できる。
#[test]
fn named_unique_and_column_references_survive_reopen() {
    let (core, path, mut session) = setup("create-named-unique-fk-reopen");
    let _guard = CleanupGuard(path.clone());
    run(&core, &mut session, "CREATE TABLE p (k TEXT UNIQUE)");
    run(
        &core,
        &mut session,
        "CREATE TABLE c (v TEXT CONSTRAINT fk_c REFERENCES p (k), \
         w TEXT CONSTRAINT u_c UNIQUE)",
    );
    drop(session);
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("c").expect("schema");
    assert_eq!(schema.foreign_keys()[0].name(), "fk_c");
    assert_eq!(schema.unique_constraints()[0].name(), "u_c");
    drop(storage);
    let storage = Storage::open(&path).expect("reopen again");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    // 再オープン後も制約が効いている。
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            "c",
            "v",
            1,
            "x",
            "op-1"
        )),
        "23503"
    );
    run(&core, &mut session, "ALTER TABLE c DROP CONSTRAINT fk_c");
    run(&core, &mut session, "ALTER TABLE c DROP CONSTRAINT u_c");
}

/// 名前の重複: UNIQUE 系は `42P07`、FOREIGN KEY を含むものは `42710`。副作用なし
/// （同名テーブルを後続で作成できる）。
#[test]
fn duplicate_names_return_42p07_or_42710_without_side_effects() {
    let (core, path, mut session) = setup("create-named-dup");
    let _guard = CleanupGuard(path);
    run(&core, &mut session, "CREATE TABLE p (k TEXT UNIQUE)");
    for (sql, expected) in [
        (
            "CREATE TABLE t (a TEXT, b TEXT, CONSTRAINT x UNIQUE (a), CONSTRAINT x UNIQUE (b))",
            "42P07",
        ),
        (
            "CREATE TABLE t (a TEXT CONSTRAINT x UNIQUE, b TEXT CONSTRAINT x UNIQUE)",
            "42P07",
        ),
        (
            "CREATE TABLE t (a TEXT, CONSTRAINT x UNIQUE (a), CONSTRAINT x CHECK (a = 'v'))",
            "42P07",
        ),
        (
            "CREATE TABLE t (a TEXT PRIMARY KEY, b TEXT, CONSTRAINT t_pkey UNIQUE (b))",
            "42P07",
        ),
        (
            "CREATE TABLE t (a TEXT CONSTRAINT x UNIQUE, b TEXT CONSTRAINT x REFERENCES p (k))",
            "42710",
        ),
        (
            "CREATE TABLE t (a TEXT CONSTRAINT x REFERENCES p (k), b TEXT, \
             CONSTRAINT x FOREIGN KEY (b) REFERENCES p (k))",
            "42710",
        ),
        (
            "CREATE TABLE t (a TEXT PRIMARY KEY, b TEXT CONSTRAINT t_pkey REFERENCES p (k))",
            "42710",
        ),
    ] {
        assert_eq!(fail(&core, &mut session, sql), expected, "{sql}");
    }
    // どれも副作用を残さない。
    run(&core, &mut session, "CREATE TABLE t (a TEXT)");
}

/// 既定名は明示した UNIQUE 名を避ける。
#[test]
fn default_unique_name_avoids_explicit_names() {
    let (core, path, mut session) = setup("create-named-unique-default");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "CREATE TABLE t (a TEXT UNIQUE, b TEXT, CONSTRAINT t_a_key UNIQUE (b))",
    );
    run(&core, &mut session, "ALTER TABLE t DROP CONSTRAINT t_a_key");
}

/// 明示 UNIQUE 名と既定 CHECK 名の衝突は `42601` で固定する（`XX000` にも黙って受理にも
/// ならない）。
#[test]
fn explicit_unique_name_colliding_with_default_check_name_is_42601() {
    let (core, path, mut session) = setup("create-named-unique-check-default");
    let _guard = CleanupGuard(path);
    assert_eq!(
        fail(
            &core,
            &mut session,
            "CREATE TABLE t (a INT CHECK (a > 0), CONSTRAINT t_a_check UNIQUE (a))"
        ),
        "42601"
    );
}

/// 名前付き UNIQUE を参照先にした FK がある間、その UNIQUE の DROP は `2BP01`。
#[test]
fn dropping_named_unique_referenced_by_foreign_key_is_2bp01() {
    let (core, path, mut session) = setup("create-named-unique-referenced");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "CREATE TABLE p (k TEXT, CONSTRAINT u_p UNIQUE (k))",
    );
    run(
        &core,
        &mut session,
        "CREATE TABLE c (v TEXT CONSTRAINT fk_c REFERENCES p (k))",
    );
    assert_eq!(
        fail(&core, &mut session, "ALTER TABLE p DROP CONSTRAINT u_p"),
        "2BP01"
    );
}

/// 構文の fail-closed: 型キーワード名・未知列・同一列の UNIQUE 重複は `42601`。
#[test]
fn structural_rejections_are_42601() {
    let (core, path, mut session) = setup("create-named-unique-structural");
    let _guard = CleanupGuard(path);
    for sql in [
        "CREATE TABLE t (a TEXT, CONSTRAINT text UNIQUE (a))",
        "CREATE TABLE t (a TEXT, CONSTRAINT u UNIQUE (missing))",
        "CREATE TABLE t (a TEXT UNIQUE CONSTRAINT u UNIQUE)",
        "CREATE TABLE t (a TEXT CONSTRAINT u NOT NULL)",
    ] {
        assert_eq!(fail(&core, &mut session, sql), "42601", "{sql}");
    }
}
