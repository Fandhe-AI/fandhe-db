//! `CREATE UNIQUE INDEX`（UNIQUE 制約の宣言への展開。Issue #1616・INDEX-9）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/` INDEX-9・TABLE-16・TABLE-17・TABLE-36・RLS-9・
//! RLS-10 (c)・ERR-6。
//!
//! `sql_alter_table_unique_constraint.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として検証する）。検証する契約
//! （詳細は `docs/design/index-ddl-declaration.md` の Issue #1616 追記を参照）:
//!
//! - 作成後の重複 INSERT・UPDATE は `23505`、`DROP INDEX` で強制が外れる
//! - 既存行に重複があれば `23505` で作成が失敗し、索引も制約も残らない
//! - 索引由来の UNIQUE は制約として扱わない（`DROP CONSTRAINT <索引名>` は `42704`）。
//!   `ON CONFLICT (<列>)` の推論と FK の参照先には使える
//! - 制約名前空間の衝突・未対応の形は fail-closed（`42P07`／`0A000`／`42601`）
//! - テナントごとに独立して判定し、`23505` の文言にテナント・値を含めない

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

static OP: AtomicU64 = AtomicU64::new(0);

fn op() -> String {
    format!("uidx-op-{}", OP.fetch_add(1, Ordering::Relaxed))
}

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
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn code(core: &EngineCore, session: &mut SessionState, tenant: &str, sql: &str) -> &'static str {
    exec(core, session, tenant, sql).expect_err(sql).wire_code()
}

fn insert(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    id: u64,
    a: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    exec(
        core,
        session,
        tenant,
        &format!(
            "INSERT INTO docs (id, a, b) VALUES ({id}, '{a}', 'b') USING OPERATION_ID '{}'",
            op()
        ),
    )
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let (core, path) = new_core(label);
    let mut s = ddl_session();
    exec(&core, &mut s, "owner", "CREATE TABLE docs (a TEXT, b TEXT)").expect("create table");
    (core, path, s)
}

#[test]
fn create_unique_index_enforces_insert_and_update_then_drop_releases() {
    let (core, path, mut s) = setup("uidx-basic");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    insert(&core, &mut s, "owner", 1, "x").expect("first");
    assert_eq!(
        insert(&core, &mut s, "owner", 2, "x")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    insert(&core, &mut s, "owner", 3, "y").expect("distinct");
    let upd = format!(
        "UPDATE docs SET a = 'x' WHERE id = 3 USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "owner", &upd), "23505");
    // 複数行 VALUES 内の重複。
    let multi = format!(
        "INSERT INTO docs (id, a, b) VALUES (10, 'm', 'b'), (11, 'm', 'b') USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "owner", &multi), "23505");

    exec(&core, &mut s, "owner", "DROP INDEX docs_a").expect("drop");
    insert(&core, &mut s, "owner", 2, "x").expect("enforcement released");
}

#[test]
fn nulls_are_distinct_and_tenants_are_independent() {
    let (core, path, mut s) = setup("uidx-null-tenant");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    for (i, t) in [(1u64, "alice"), (2, "bob")] {
        insert(&core, &mut s, t, i, "same").expect("tenant-independent value");
    }
    let err = insert(&core, &mut s, "alice", 3, "same").unwrap_err();
    assert_eq!(err.wire_code(), "23505");
    let msg = err.client_message();
    assert!(!msg.contains("alice") && !msg.contains("same"), "{msg}");
    for id in [20u64, 21] {
        let sql = format!(
            "INSERT INTO docs (id, a, b) VALUES ({id}, NULL, 'b') USING OPERATION_ID '{}'",
            op()
        );
        exec(&core, &mut s, "alice", &sql).expect("NULLs never conflict");
    }
}

#[test]
fn create_over_existing_duplicates_fails_and_leaves_nothing() {
    let (core, path, mut s) = setup("uidx-existing-dup");
    let _g = CleanupGuard(path);
    insert(&core, &mut s, "alice", 1, "x").expect("seed");
    insert(&core, &mut s, "alice", 2, "x").expect("seed dup");
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "CREATE UNIQUE INDEX docs_a ON docs (a)"
        ),
        "23505"
    );
    assert_eq!(code(&core, &mut s, "owner", "DROP INDEX docs_a"), "42704");
    // 制約も残っていない: 重複 INSERT が受理される。
    insert(&core, &mut s, "alice", 3, "x").expect("no constraint left behind");

    // 別テナントの同値だけなら作成できる。
    let (core2, path2, mut s2) = setup("uidx-existing-cross-tenant");
    let _g2 = CleanupGuard(path2);
    insert(&core2, &mut s2, "alice", 1, "x").expect("seed");
    insert(&core2, &mut s2, "bob", 2, "x").expect("seed other tenant");
    exec(
        &core2,
        &mut s2,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("cross-tenant duplicates are allowed");
}

#[test]
fn drop_constraint_on_index_name_is_42704_and_drop_index_if_exists_works() {
    let (core, path, mut s) = setup("uidx-drop-constraint");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "ALTER TABLE docs DROP CONSTRAINT docs_a"
        ),
        "42704"
    );
    // 拒否後も強制は残っている。
    insert(&core, &mut s, "owner", 1, "x").expect("seed");
    assert_eq!(
        insert(&core, &mut s, "owner", 2, "x")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    exec(&core, &mut s, "owner", "DROP INDEX IF EXISTS docs_a").expect("drop");
    exec(&core, &mut s, "owner", "DROP INDEX IF EXISTS docs_a").expect("idempotent");
}

#[test]
fn enforcement_survives_reopen() {
    let (core, path, mut s) = setup("uidx-reopen");
    let _g = CleanupGuard(path.clone());
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    insert(&core, &mut s, "owner", 1, "x").expect("seed");
    drop(core);
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    let mut s = ddl_session();
    assert_eq!(
        insert(&core, &mut s, "owner", 2, "x")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    drop(core);
    let idx = Storage::open(&path)
        .expect("reopen")
        .list_indexes()
        .expect("list");
    assert_eq!(idx.len(), 1);
    assert!(idx[0].unique);
}

#[test]
fn on_conflict_inference_and_foreign_key_target_use_the_unique_index() {
    let (core, path, mut s) = setup("uidx-conflict-fk");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    insert(&core, &mut s, "owner", 1, "x").expect("seed");
    let nothing = format!(
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'b') ON CONFLICT (a) DO NOTHING USING OPERATION_ID '{}'",
        op()
    );
    exec(&core, &mut s, "owner", &nothing).expect("inferred by unique index");
    let named = format!(
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'b') ON CONFLICT ON CONSTRAINT docs_a DO NOTHING USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "owner", &named), "42601");

    exec(
        &core,
        &mut s,
        "owner",
        "CREATE TABLE kids (parent_a TEXT REFERENCES docs (a))",
    )
    .expect("FK may reference the unique index columns");
    assert_eq!(code(&core, &mut s, "owner", "DROP INDEX docs_a"), "2BP01");
}

#[test]
fn constraint_namespace_collisions_are_rejected_even_with_if_not_exists() {
    let (core, path, mut s) = setup("uidx-namespace");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs ADD CONSTRAINT uq_b UNIQUE (b)",
    )
    .expect("named unique");
    exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs ADD CONSTRAINT ck CHECK (a = 'z')",
    )
    .expect("check");
    for sql in [
        "CREATE UNIQUE INDEX uq_b ON docs (a)",
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_b ON docs (a)",
        "CREATE UNIQUE INDEX ck ON docs (a)",
    ] {
        assert_eq!(code(&core, &mut s, "owner", sql), "42P07", "{sql}");
    }
    assert_eq!(code(&core, &mut s, "owner", "DROP INDEX uq_b"), "42704");

    // relation 名前空間の衝突は IF NOT EXISTS で no-op、なければ 42P07。
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX IF NOT EXISTS docs_a ON docs (a)",
    )
    .expect("no-op");
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "CREATE UNIQUE INDEX docs_a ON docs (a)"
        ),
        "42P07"
    );
    // 同じ列並びの既存 UNIQUE との重複は 42601。
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "CREATE UNIQUE INDEX docs_a2 ON docs (a)"
        ),
        "42601"
    );
}

#[test]
fn add_unique_without_name_avoids_unique_index_name() {
    let (core, path, mut s) = setup("uidx-default-name");
    let _g = CleanupGuard(path);
    exec(&core, &mut s, "owner", "ALTER TABLE docs ADD COLUMN c TEXT").expect("add column");
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_c_key ON docs (a)",
    )
    .expect("index named like a default constraint name");
    exec(&core, &mut s, "owner", "ALTER TABLE docs ADD UNIQUE (c)")
        .expect("default name avoids the collision");
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "ALTER TABLE docs ADD CONSTRAINT docs_c_key UNIQUE (b)"
        ),
        "42P07"
    );
}

#[test]
fn unsupported_shapes_are_rejected_fail_closed() {
    let (core, path, mut s) = setup("uidx-shapes");
    let _g = CleanupGuard(path);
    let cases = [
        ("CREATE UNIQUE INDEX i1 ON docs USING hnsw (a)", "0A000"),
        ("CREATE UNIQUE INDEX i2 ON docs (a COLLATE \"C\")", "0A000"),
        ("CREATE UNIQUE INDEX i3 ON docs (id)", "0A000"),
        ("CREATE UNIQUE INDEX ON docs (a)", "42601"),
        ("CREATE UNIQUE INDEX i4 ON ghost (a)", "42P01"),
        ("CREATE UNIQUE INDEX i5 ON docs (ghost)", "42703"),
        ("CREATE UNIQUE INDEX i6 ON docs (a, a)", "42601"),
        ("CREATE INDEX UNIQUE i7 ON docs (a)", "42601"),
    ];
    for (sql, expected) in cases {
        assert_eq!(code(&core, &mut s, "owner", sql), expected, "{sql}");
    }
    // DDL 権限が無ければ 42501。
    let mut plain = SessionState::default();
    assert_eq!(
        code(
            &core,
            &mut plain,
            "owner",
            "CREATE UNIQUE INDEX i9 ON docs (b)"
        ),
        "42501"
    );
}

#[test]
fn unsupported_column_types_are_rejected() {
    let (core, path) = new_core("uidx-types");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE TABLE t (f BOOLEAN, j JSONB, arr INTEGER[], n INTEGER)",
    )
    .expect("create table");
    for col in ["f", "j", "arr"] {
        assert_eq!(
            code(
                &core,
                &mut s,
                "owner",
                &format!("CREATE UNIQUE INDEX u_{col} ON t ({col})")
            ),
            "0A000",
            "{col}"
        );
    }
    exec(&core, &mut s, "owner", "CREATE UNIQUE INDEX u_n ON t (n)").expect("integer ok");
    exec(&core, &mut s, "owner", "DROP INDEX u_n").expect("drop");
    assert_eq!(code(&core, &mut s, "owner", "DROP INDEX u_f"), "42704");
}

#[test]
fn rename_column_keeps_enforcement_and_drop_table_clears_declaration() {
    let (core, path, mut s) = setup("uidx-rename-droptable");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    insert(&core, &mut s, "owner", 1, "x").expect("seed");
    exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs RENAME COLUMN a TO a2",
    )
    .expect("rename");
    let sql = format!(
        "INSERT INTO docs (id, a2, b) VALUES (2, 'x', 'b') USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "owner", &sql), "23505");
    exec(&core, &mut s, "owner", "DROP INDEX docs_a").expect("drop after rename");
    let sql = format!(
        "INSERT INTO docs (id, a2, b) VALUES (2, 'x', 'b') USING OPERATION_ID '{}'",
        op()
    );
    exec(&core, &mut s, "owner", &sql).expect("released");
}

#[test]
fn drop_table_clears_declaration_and_constraint() {
    let (core, path, mut s) = setup("uidx-droptable");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_a ON docs (a)",
    )
    .expect("create");
    insert(&core, &mut s, "owner", 1, "x").expect("seed");
    exec(&core, &mut s, "owner", "DROP TABLE docs").expect("drop table");
    exec(&core, &mut s, "owner", "CREATE TABLE docs (a TEXT, b TEXT)").expect("recreate");
    assert_eq!(code(&core, &mut s, "owner", "DROP INDEX docs_a"), "42704");
    insert(&core, &mut s, "owner", 1, "x").expect("seed");
    insert(&core, &mut s, "owner", 2, "x").expect("no residual enforcement");
}
