//! 一意の式索引（`CREATE UNIQUE INDEX ... ((式))`。Issue #1651・INDEX-9・INDEX-10）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-9・INDEX-10、TABLE-16、RLS-9・RLS-10 (c)、
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_unique_index.rs`・`sql_expression_index.rs` と同じ流儀（実 `Storage` ＋
//! `EngineCore::execute_sql_in_session` を production 経路として検証する）。検証する契約
//! （詳細は `docs/design/expression-index.md` の Issue #1651 節）:
//!
//! - 式の値（列要素との複合を含む）の重複は INSERT・UPDATE・UPSERT・複数行 VALUES のどれでも
//!   `23505` で拒否され、`DROP INDEX` で強制が外れる。NULL は重複しない（NULLS DISTINCT）。
//!   テナントごとに独立して判定し、不可視行とも衝突する
//! - 既存行に重複があれば `23505` で作成が失敗し、索引・世代が残らない（応答に値・テナントを含めない）
//! - 参照列の `DROP COLUMN` は索引を連鎖削除し、`RENAME COLUMN` は式テキストを追随させ、
//!   `ALTER COLUMN TYPE` は再束縛・再検証して強制を継続する（失敗は各 SQLSTATE で無変化）

use engine::catalog::IndexDef;
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
    format!("xuidx-op-{}", OP.fetch_add(1, Ordering::Relaxed))
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
    insert_ab(core, session, tenant, id, a, "b")
}

fn insert_ab(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    id: u64,
    a: &str,
    b: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    exec(
        core,
        session,
        tenant,
        &format!(
            "INSERT INTO docs (id, a, b) VALUES ({id}, '{a}', '{b}') USING OPERATION_ID '{}'",
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

fn count(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> usize {
    match exec(core, s, tenant, sql).expect("select") {
        SqlOutcome::Query(q) => q.rows.len(),
        other => panic!("unexpected outcome {other:?}"),
    }
}

fn list(path: &std::path::Path) -> Vec<IndexDef> {
    Storage::open(path)
        .expect("reopen storage")
        .list_indexes()
        .expect("list indexes")
}

#[test]
fn unique_expression_index_enforces_every_write_path_then_drop_releases() {
    let (core, path, mut s) = setup("xuidx-basic");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect("create");
    insert(&core, &mut s, "owner", 1, "EN").expect("first");
    // 式の値が衝突する（大文字小文字違い）。
    assert_eq!(
        insert(&core, &mut s, "owner", 2, "en")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    insert(&core, &mut s, "owner", 3, "fr").expect("distinct");
    // UPDATE で衝突させる。
    let upd = format!(
        "UPDATE docs SET a = 'En' WHERE id = 3 USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "owner", &upd), "23505");
    // 同一文の複数行 VALUES 内の衝突。
    let multi = format!(
        "INSERT INTO docs (id, a, b) VALUES (10, 'M', 'b'), (11, 'm', 'b') USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "owner", &multi), "23505");
    // UPSERT（列ターゲットは id）の DO UPDATE が式の値を衝突させる。
    let upsert = format!(
        "INSERT INTO docs (id, a, b) VALUES (3, 'EN', 'b') ON CONFLICT (id) DO UPDATE \
         SET a = EXCLUDED.a USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "owner", &upsert), "23505");
    // 拒否された文は行を残さない。
    assert_eq!(
        count(&core, &mut s, "owner", "SELECT id FROM docs LIMIT 100"),
        2
    );

    // 自分自身の更新（値不変）は衝突しない。
    let own = format!(
        "UPDATE docs SET b = 'z' WHERE id = 1 USING OPERATION_ID '{}'",
        op()
    );
    exec(&core, &mut s, "owner", &own).expect("own update");

    exec(&core, &mut s, "owner", "DROP INDEX docs_lower_a").expect("drop");
    insert(&core, &mut s, "owner", 2, "en").expect("enforcement released");
}

#[test]
fn null_results_are_distinct_and_tenants_are_independent() {
    let (core, path, mut s) = setup("xuidx-null-tenant");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect("create");
    // 同じ値でもテナントが違えば衝突しない。
    insert(&core, &mut s, "alice", 1, "same").expect("alice");
    insert(&core, &mut s, "bob", 2, "SAME").expect("bob");
    // NULL の結果は何件あってもよい（NULLS DISTINCT）。
    for id in [10u64, 11, 12] {
        let sql = format!(
            "INSERT INTO docs (id, b) VALUES ({id}, 'b') USING OPERATION_ID '{}'",
            op()
        );
        exec(&core, &mut s, "carol", &sql).expect("NULL result is not unique-checked");
    }
    // 同一テナント内の衝突の応答は固定で、値・テナントを含まない。
    let err = insert(&core, &mut s, "alice", 5, "SAME").unwrap_err();
    assert_eq!(err.wire_code(), "23505");
    let text = err.to_string();
    assert!(
        !text.contains("same") && !text.contains("SAME") && !text.contains("alice"),
        "must not echo the value or tenant: {text}"
    );
}

#[test]
fn private_rows_collide_with_other_visibility_of_the_same_tenant() {
    let (core, path, mut s) = setup("xuidx-private");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect("create");
    let private_only =
        PolicyContext::with_visibilities("alice", [Visibility::Private]).expect("valid tenant");
    let public_only =
        PolicyContext::with_visibilities("alice", [Visibility::Public]).expect("valid tenant");
    core.execute_sql_in_session(
        &private_only,
        &mut s,
        &format!(
            "INSERT INTO docs (id, a, b) VALUES (1, 'Secret', 'b') USING OPERATION_ID '{}'",
            op()
        ),
    )
    .expect("private row");
    // 不可視（Private）行とも衝突する（可視性で判定を狭めない）。
    let err = core
        .execute_sql_in_session(
            &public_only,
            &mut s,
            &format!(
                "INSERT INTO docs (id, a, b) VALUES (2, 'secret', 'b') USING OPERATION_ID '{}'",
                op()
            ),
        )
        .expect_err("collision with a row the caller cannot see");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn composite_column_and_expression_key_is_enforced() {
    let (core, path, mut s) = setup("xuidx-composite");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_ab ON docs ((lower(a)), b)",
    )
    .expect("create");
    insert_ab(&core, &mut s, "owner", 1, "X", "k1").expect("first");
    insert_ab(&core, &mut s, "owner", 2, "x", "k2").expect("same expression, other column");
    insert_ab(&core, &mut s, "owner", 3, "y", "k1").expect("other expression, same column");
    assert_eq!(
        insert_ab(&core, &mut s, "owner", 4, "x", "k1")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
}

#[test]
fn explicit_transaction_rollback_leaves_no_unique_trace() {
    let (core, path, mut s) = setup("xuidx-txn");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect("create");
    let caller = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut s,
        &mut txn,
        &format!(
            "INSERT INTO docs (id, a, b) VALUES (1, 'Dup', 'b') USING OPERATION_ID '{}'",
            op()
        ),
    )
    .expect("insert in txn");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            &format!(
                "INSERT INTO docs (id, a, b) VALUES (2, 'dup', 'b') USING OPERATION_ID '{}'",
                op()
            ),
        )
        .expect_err("collision inside the transaction");
    assert_eq!(err.wire_code(), "23505");
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    // ロールバック後は同じ値を入れられる（正引きエントリの痕跡が残らない）。
    insert(&core, &mut s, "alice", 3, "DUP").expect("no trace after rollback");
}

#[test]
fn create_rejects_existing_duplicates_and_leaves_no_declaration() {
    let (core, path, mut s) = setup("xuidx-create-dup");
    let _g = CleanupGuard(path.clone());
    insert(&core, &mut s, "alice", 1, "Same").expect("row");
    insert(&core, &mut s, "alice", 2, "SAME").expect("row");
    // 別テナントの同値はテナントごとの判定で衝突しない。
    insert(&core, &mut s, "bob", 3, "same").expect("other tenant");
    let err = exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect_err("duplicates exist");
    assert_eq!(err.wire_code(), "23505");
    let text = err.to_string();
    assert!(
        !text.contains("alice") && !text.to_lowercase().contains("same"),
        "must not echo tenant or value: {text}"
    );
    assert_eq!(
        code(&core, &mut s, "owner", "DROP INDEX docs_lower_a"),
        "42704"
    );
    drop(core);
    assert!(list(&path).is_empty());

    // 重複を解消すれば同名で作れ、以後は強制される。再作成でマーカーも作り直される。
    let (core, _) = (core_at(&path), ());
    let mut s = ddl_session();
    let del = format!(
        "DELETE FROM docs WHERE id = 2 USING OPERATION_ID '{}'",
        op()
    );
    exec(&core, &mut s, "alice", &del).expect("delete duplicate");
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect("create after cleanup");
    assert_eq!(
        insert(&core, &mut s, "alice", 4, "SAME")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    exec(&core, &mut s, "owner", "DROP INDEX docs_lower_a").expect("drop");
    insert(&core, &mut s, "alice", 4, "SAME").expect("released");
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

#[test]
fn create_variants_if_not_exists_name_collision_and_type_gate() {
    let (core, path, mut s) = setup("xuidx-create-variants");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u1 ON docs ((lower(a)))",
    )
    .expect("create");
    // IF NOT EXISTS は同名があれば何もしない。
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX IF NOT EXISTS u1 ON docs ((upper(a)))",
    )
    .expect("no-op");
    // 非 IF NOT EXISTS は 42P07。
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "CREATE UNIQUE INDEX u1 ON docs ((lower(b)))"
        ),
        "42P07"
    );
    // 制約名前空間との衝突（UNIQUE 制約と同名）は 42P07。
    exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs ADD CONSTRAINT docs_uq UNIQUE (b)",
    )
    .expect("add constraint");
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "CREATE UNIQUE INDEX docs_uq ON docs ((lower(a)))"
        ),
        "42P07"
    );
    // 暗黙の id 列は一意キー許可型外（列要素の型ゲート）。
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "CREATE UNIQUE INDEX u2 ON docs ((lower(a)), id)"
        ),
        "0A000"
    );
    // 非 owner（DDL 不可）は拒否される。
    let mut plain = SessionState::default();
    assert_eq!(
        code(
            &core,
            &mut plain,
            "alice",
            "CREATE UNIQUE INDEX u3 ON docs ((lower(a)))"
        ),
        "42501"
    );
}

#[test]
fn unique_expression_index_is_persisted_and_enforced_after_reopen() {
    let (core, path, mut s) = setup("xuidx-reopen");
    let _g = CleanupGuard(path.clone());
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect("create");
    insert(&core, &mut s, "alice", 1, "Persist").expect("row");
    drop(core);
    let defs = list(&path);
    assert_eq!(defs.len(), 1);
    assert!(defs[0].unique && defs[0].has_expressions());
    let core = core_at(&path);
    let mut s = ddl_session();
    assert_eq!(
        insert(&core, &mut s, "alice", 2, "persist")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    // DROP → 再作成でも、古い正引きエントリを流用せず作り直して強制する。
    exec(&core, &mut s, "owner", "DROP INDEX docs_lower_a").expect("drop");
    insert(&core, &mut s, "alice", 2, "PERSIST").expect("released");
    let del = format!(
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID '{}'",
        op()
    );
    exec(&core, &mut s, "alice", &del).expect("delete");
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX docs_lower_a ON docs ((lower(a)))",
    )
    .expect("recreate");
    assert_eq!(
        insert(&core, &mut s, "alice", 3, "Persist")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
}
// ---------------------------------------------------------------------------
// 列変更との関係（Issue #1651）。DROP COLUMN は連鎖削除、RENAME COLUMN は式テキストの追随、
// ALTER COLUMN TYPE は再束縛・再検証（失敗は各 SQLSTATE で表・索引とも無変化）。
// ---------------------------------------------------------------------------

fn setup_n(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let (core, path) = new_core(label);
    let mut s = ddl_session();
    exec(&core, &mut s, "owner", "CREATE TABLE nt (n BIGINT, c TEXT)").expect("create table");
    (core, path, s)
}

fn insert_n(
    core: &EngineCore,
    s: &mut SessionState,
    tenant: &str,
    id: u64,
    n: i64,
) -> Result<SqlOutcome, SqlSurfaceError> {
    exec(
        core,
        s,
        tenant,
        &format!(
            "INSERT INTO nt (id, n, c) VALUES ({id}, {n}, 'c') USING OPERATION_ID '{}'",
            op()
        ),
    )
}

#[test]
fn drop_column_removes_unique_expression_indexes_and_releases_enforcement() {
    for (label, create, victim) in [
        (
            "xuidx-dropcol-expr",
            "CREATE UNIQUE INDEX u1 ON docs ((lower(a)))",
            "a",
        ),
        (
            "xuidx-dropcol-plain-element",
            "CREATE UNIQUE INDEX u1 ON docs ((lower(a)), b)",
            "b",
        ),
    ] {
        let (core, path, mut s) = setup(label);
        let _g = CleanupGuard(path.clone());
        exec(&core, &mut s, "owner", create).expect("create");
        // 参照しない列の索引は残る。
        exec(
            &core,
            &mut s,
            "owner",
            "CREATE INDEX keep ON docs ((upper(b)))",
        )
        .expect("keep");
        insert_ab(&core, &mut s, "alice", 1, "X", "k").expect("row");
        exec(
            &core,
            &mut s,
            "owner",
            &format!("ALTER TABLE docs DROP COLUMN {victim}"),
        )
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        drop(core);
        let names: Vec<String> = list(&path).into_iter().map(|d| d.name).collect();
        assert!(!names.contains(&"u1".to_string()), "{label}: {names:?}");
        // 列 b を落としたケースでは upper(b) の索引も消え、a を落としたケースでは残る。
        assert_eq!(
            names.contains(&"keep".to_string()),
            victim == "a",
            "{label}"
        );
    }
    // DROP 後は強制が外れ、同名列を再追加しても旧索引は復活しない。
    let (core, path, mut s) = setup("xuidx-dropcol-release");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u1 ON docs ((lower(a)))",
    )
    .expect("create");
    insert(&core, &mut s, "alice", 1, "X").expect("row");
    exec(&core, &mut s, "owner", "ALTER TABLE docs DROP COLUMN a").expect("drop column");
    exec(&core, &mut s, "owner", "ALTER TABLE docs ADD COLUMN a TEXT").expect("re-add column");
    let sql = format!(
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'b'), (3, 'X', 'b') USING OPERATION_ID '{}'",
        op()
    );
    exec(&core, &mut s, "alice", &sql).expect("no stale enforcement");
}

#[test]
fn rename_column_rewrites_the_expression_and_keeps_enforcing() {
    let (core, path, mut s) = setup("xuidx-rename");
    let _g = CleanupGuard(path.clone());
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u1 ON docs ((lower(a)), b)",
    )
    .expect("create");
    insert_ab(&core, &mut s, "alice", 1, "Ab", "k").expect("row");
    exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs RENAME COLUMN a TO a2",
    )
    .expect("rename expr");
    exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs RENAME COLUMN b TO b2",
    )
    .expect("rename plain");
    let dup = format!(
        "INSERT INTO docs (id, a2, b2) VALUES (2, 'aB', 'k') USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "alice", &dup), "23505");
    let ok = format!(
        "INSERT INTO docs (id, a2, b2) VALUES (2, 'aB', 'other') USING OPERATION_ID '{}'",
        op()
    );
    exec(&core, &mut s, "alice", &ok).expect("different second element");
    drop(core);
    let defs = list(&path);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].columns, vec![String::new(), "b2".to_string()]);
    let e = defs[0].expressions[0].as_ref().expect("expression");
    assert_eq!(e.referenced_columns, vec!["a2".to_string()]);
    assert!(e.sql.contains("a2"), "{}", e.sql);
    // 再オープン後も新しい名前で強制される。
    let core = core_at(&path);
    let mut s = ddl_session();
    let dup = format!(
        "INSERT INTO docs (id, a2, b2) VALUES (3, 'AB', 'k') USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "alice", &dup), "23505");
}

#[test]
fn rename_swap_of_referenced_columns_does_not_chain_substitute() {
    let (core, path, mut s) = setup("xuidx-rename-swap");
    let _g = CleanupGuard(path.clone());
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX ua ON docs ((lower(a)))",
    )
    .expect("create ua");
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX ub ON docs ((upper(b)))",
    )
    .expect("create ub");
    for sql in [
        "ALTER TABLE docs RENAME COLUMN a TO tmp",
        "ALTER TABLE docs RENAME COLUMN b TO a",
        "ALTER TABLE docs RENAME COLUMN tmp TO b",
    ] {
        exec(&core, &mut s, "owner", sql).expect(sql);
    }
    drop(core);
    let defs = list(&path);
    let refs_of = |name: &str| {
        defs.iter()
            .find(|d| d.name == name)
            .and_then(|d| d.expressions[0].as_ref())
            .map(|e| e.referenced_columns.clone())
            .expect("index")
    };
    // 元の `a` を参照していた索引は今の `b`、元の `b` を参照していた索引は今の `a`。
    assert_eq!(refs_of("ua"), vec!["b".to_string()]);
    assert_eq!(refs_of("ub"), vec!["a".to_string()]);
}

#[test]
fn alter_type_rebinds_and_keeps_enforcing() {
    let (core, path, mut s) = setup("xuidx-alter-type");
    let _g = CleanupGuard(path.clone());
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u1 ON docs ((lower(a)))",
    )
    .expect("create");
    insert(&core, &mut s, "alice", 1, "Ab").expect("row");
    exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs ALTER COLUMN a TYPE varchar(10)",
    )
    .expect("alter type");
    assert_eq!(
        insert(&core, &mut s, "alice", 2, "aB")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    insert(&core, &mut s, "alice", 2, "cd").expect("distinct");
    drop(core);
    assert_eq!(list(&path).len(), 1);
}

#[test]
fn alter_type_with_unbindable_expression_is_rejected_without_changes() {
    let (core, path, mut s) = setup("xuidx-alter-type-bind");
    let _g = CleanupGuard(path.clone());
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u1 ON docs ((lower(a)))",
    )
    .expect("create");
    insert(&core, &mut s, "alice", 1, "7").expect("row");
    let err = exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE docs ALTER COLUMN a TYPE integer USING length(a)",
    )
    .expect_err("lower(integer) cannot bind");
    // 束縛側の SQLSTATE（引数の型不一致）をそのまま透過する。
    assert_eq!(err.wire_code(), "42804", "{err}");
    drop(core);
    assert_eq!(list(&path).len(), 1);
    // 表も無変化（a は TEXT のまま。強制も継続する）。
    let core = core_at(&path);
    let mut s = ddl_session();
    assert_eq!(
        insert(&core, &mut s, "alice", 2, "7")
            .unwrap_err()
            .wire_code(),
        "23505"
    );
}

#[test]
fn alter_type_that_creates_duplicates_is_rejected_with_23505() {
    let (core, path, mut s) = setup_n("xuidx-alter-type-dup");
    let _g = CleanupGuard(path.clone());
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u1 ON nt ((n + 0))",
    )
    .expect("create");
    insert_n(&core, &mut s, "alice", 1, 100).expect("row");
    insert_n(&core, &mut s, "alice", 2, 150).expect("row");
    // 他テナントの行は別のキー空間（衝突しない）。
    insert_n(&core, &mut s, "bob", 3, 1).expect("other tenant");
    let err = exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE nt ALTER COLUMN n TYPE bigint USING n / 100",
    )
    .expect_err("100 and 150 collapse to the same key");
    assert_eq!(err.wire_code(), "23505", "{err}");
    // 無変化: 元の値で強制が継続する。
    assert_eq!(
        insert_n(&core, &mut s, "alice", 4, 100)
            .unwrap_err()
            .wire_code(),
        "23505"
    );
    insert_n(&core, &mut s, "alice", 4, 200).expect("distinct under the old values");
}

#[test]
fn alter_type_evaluation_error_is_rejected_with_the_sqlstate() {
    let (core, path, mut s) = setup_n("xuidx-alter-type-eval");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u1 ON nt ((100 / n))",
    )
    .expect("create");
    insert_n(&core, &mut s, "alice", 1, 5).expect("row");
    // n - 5 は 0 になり、式 100 / n が評価できない（`22012`）。
    let err = exec(
        &core,
        &mut s,
        "owner",
        "ALTER TABLE nt ALTER COLUMN n TYPE bigint USING n - 5",
    )
    .expect_err("division by zero after the conversion");
    assert_eq!(err.wire_code(), "22012", "{err}");
    insert_n(&core, &mut s, "alice", 2, 4).expect("table is unchanged");
}

#[test]
fn constraint_additions_collide_with_unique_index_name() {
    let (core, path, mut s) = setup("xuidx-name-collision");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u ON docs ((lower(a)))",
    )
    .expect("create");
    for (sql, expected) in [
        ("ALTER TABLE docs ADD CONSTRAINT u UNIQUE (b)", "42P07"),
        (
            "ALTER TABLE docs ADD CONSTRAINT u CHECK (b <> 'x')",
            "42710",
        ),
    ] {
        assert_eq!(code(&core, &mut s, "owner", sql), expected, "{sql}");
    }
}

#[test]
fn alter_type_of_bare_unique_element_to_disallowed_type_is_rejected() {
    let (core, path, mut s) = setup("xuidx-alter-bare-type");
    let _g = CleanupGuard(path);
    exec(
        &core,
        &mut s,
        "owner",
        "CREATE UNIQUE INDEX u ON docs ((lower(a)), b)",
    )
    .expect("create");
    assert_eq!(
        code(
            &core,
            &mut s,
            "owner",
            "ALTER TABLE docs ALTER COLUMN b TYPE time USING NULL"
        ),
        "0A000"
    );
    insert_ab(&core, &mut s, "alice", 1, "x", "y").expect("table is unchanged");
}
