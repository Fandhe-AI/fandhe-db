//! `ALTER TABLE <t> RENAME [COLUMN] <old> TO <new>`（TABLE-38 のポインタ、Issue #1619）の
//! SQL 表層結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → `require_ddl_permission` →
//! `sql::ddl::execute_alter_table_rename_column`）を production 経路として検証する
//! （`sql_ddl_drop_alter_column.rs` と同じ流儀）。制約・索引の追随は Issue #1620 で、
//! 改名後も一意性・CHECK・参照整合性・索引宣言が新しい名前で効くことを固定する。ビュー定義の
//! 追随は Issue #1621 の範囲で、ビューを持つ表の改名は `0A000` で拒否する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::ddl::AlterTableAction;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("note", ColumnType::Text, true),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("label", ColumnType::Text, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-ddl-rename-column");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("owner", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    run(core, session, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn insert(core: &EngineCore, id: u64, cols: &str, vals: &str) {
    let mut s = SessionState::default();
    run(
        core,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, {cols}) VALUES ({id}, '[0.1,0.2]', {vals}) USING OPERATION_ID 'op-{id}'"
        ),
    )
    .expect("insert");
}

fn select_col(core: &EngineCore, col: &str) -> Vec<Cell> {
    core.execute_sql(&ctx(), &format!("SELECT {col} FROM {TABLE} LIMIT 100"))
        .expect("select")
        .rows
        .into_iter()
        .map(|r| r.cells.into_iter().next().expect("one cell"))
        .collect()
}

#[test]
fn rename_column_keeps_existing_rows_and_works_for_dml() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note, qty", "'hello', 7");

    let out = run(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} RENAME COLUMN note TO memo;"),
    )
    .expect("rename");
    match out {
        SqlOutcome::AlterTable(o) => {
            assert_eq!(o.table_name, TABLE);
            assert_eq!(
                o.action,
                AlterTableAction::RenameColumn {
                    column_name: "note".to_string(),
                    new_column_name: "memo".to_string()
                }
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    // 既存行の値が新しい名前で読める。古い名前は 42703。
    let cells = select_col(&core, "memo");
    assert_eq!(cells.len(), 1);
    assert!(matches!(&cells[0], Cell::Text(t) if t == "hello"));
    let err = core
        .execute_sql(&ctx(), &format!("SELECT note FROM {TABLE} LIMIT 1"))
        .expect_err("old name must be unknown");
    assert_eq!(err.wire_code(), "42703");

    // 新しい名前での INSERT・UPDATE・WHERE・DELETE。
    insert(&core, 2, "memo, qty", "'world', 9");
    let mut w = SessionState::default();
    run(
        &core,
        &mut w,
        &format!("UPDATE {TABLE} SET memo = 'changed' WHERE id = 2 USING OPERATION_ID 'op-upd-2'"),
    )
    .expect("update");
    let rows = core
        .execute_sql(
            &ctx(),
            &format!("SELECT qty FROM {TABLE} WHERE memo = 'changed' LIMIT 10"),
        )
        .expect("where on new name");
    assert_eq!(rows.rows.len(), 1);
    assert!(matches!(rows.rows[0].cells[0], Cell::SignedInteger(9)));
    run(
        &core,
        &mut w,
        &format!("DELETE FROM {TABLE} WHERE memo = 'hello' USING OPERATION_ID 'op-del-1'"),
    )
    .expect("delete");
    assert_eq!(select_col(&core, "memo").len(), 1);
}

#[test]
fn column_keyword_is_optional() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note", "'x'");
    run(&core, &mut session, "ALTER TABLE docs RENAME note TO memo").expect("rename");
    assert_eq!(select_col(&core, "memo").len(), 1);
}

#[test]
fn renamed_column_survives_reopen() {
    let path = unique_db_path("sql-ddl-rename-column-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage.create_table(&schema()).expect("create");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = ddl_session();
        insert(&core, 1, "note", "'persist'");
        run(
            &core,
            &mut session,
            "ALTER TABLE docs RENAME COLUMN note TO memo",
        )
        .expect("rename");
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let cells = select_col(&core, "memo");
    assert!(matches!(&cells[0], Cell::Text(t) if t == "persist"));
}

#[test]
fn error_contract_and_decision_order() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let cases = [
        // 存在しない表
        ("ALTER TABLE nope RENAME COLUMN a TO b", "42P01"),
        // 存在しない改名元
        ("ALTER TABLE docs RENAME COLUMN missing TO b", "42703"),
        // 既存列との衝突・同名への変更
        ("ALTER TABLE docs RENAME COLUMN note TO qty", "42701"),
        ("ALTER TABLE docs RENAME COLUMN note TO note", "42701"),
        // 内部列は TABLE-38 に従い 42703、キーワード衝突名は 42601
        // （構文段。表の存在より先に判定される）
        ("ALTER TABLE docs RENAME COLUMN id TO x", "42703"),
        ("ALTER TABLE docs RENAME COLUMN tenant_id TO x", "42703"),
        ("ALTER TABLE docs RENAME COLUMN note TO id", "42703"),
        ("ALTER TABLE docs RENAME COLUMN note TO visibility", "42703"),
        ("ALTER TABLE docs RENAME COLUMN note TO \"check\"", "42601"),
        ("ALTER TABLE nope RENAME COLUMN id TO x", "42703"),
        // 未対応の形
        ("ALTER TABLE docs RENAME TO other", "42601"),
        ("ALTER TABLE docs RENAME CONSTRAINT a TO b", "42601"),
        (
            "ALTER TABLE IF EXISTS docs RENAME COLUMN note TO memo",
            "42601",
        ),
        (
            "ALTER TABLE docs RENAME COLUMN note TO memo, ADD COLUMN z TEXT",
            "42601",
        ),
        ("ALTER TABLE docs RENAME COLUMN note", "42601"),
    ];
    for (sql, code) in cases {
        assert_eq!(code_of(&core, &mut session, sql), code, "{sql}");
    }
    // 拒否後もスキーマは不変。
    core.execute_sql(&ctx(), "SELECT note FROM docs LIMIT 1")
        .expect("note still exists");
}

#[test]
fn view_name_is_42809() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE VIEW v_docs AS SELECT * FROM docs",
    )
    .expect("view");
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE v_docs RENAME COLUMN note TO m"
        ),
        "42809"
    );
}

#[test]
fn permission_denial_is_identical_regardless_of_existence() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = SessionState::default();
    let mut messages = Vec::new();
    for sql in [
        "ALTER TABLE docs RENAME COLUMN note TO memo",
        "ALTER TABLE nope RENAME COLUMN note TO memo",
        "ALTER TABLE docs RENAME COLUMN missing TO memo",
        "ALTER TABLE docs RENAME COLUMN note TO qty",
    ] {
        let err = run(&core, &mut session, sql).expect_err("must be denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
        messages.push(err.to_string());
    }
    assert!(messages.windows(2).all(|w| w[0] == w[1]));
    core.execute_sql(&ctx(), "SELECT note FROM docs LIMIT 1")
        .expect("note still exists");
}

/// 制約・索引つきの表群を作る（`parents`: PK・UNIQUE・CHECK、`children`: 親への FK、
/// `tree`: 自己参照 FK、`docs`: scalar 索引）。
fn setup_constrained(core: &EngineCore, session: &mut SessionState) {
    for sql in [
        "CREATE TABLE parents (code TEXT PRIMARY KEY, u TEXT UNIQUE, c INTEGER, free TEXT, CHECK (c > 0))",
        "CREATE TABLE children (pcode TEXT, y TEXT, FOREIGN KEY (pcode) REFERENCES parents (code))",
        "CREATE TABLE tree (nid TEXT PRIMARY KEY, up TEXT, FOREIGN KEY (up) REFERENCES tree (nid))",
        "CREATE INDEX idx_note ON docs (note)",
    ] {
        run(core, session, sql).expect(sql);
    }
    seed_constrained(core, session);
}

fn seed_constrained(core: &EngineCore, session: &mut SessionState) {
    for sql in [
        "INSERT INTO parents (id, code, u, c, free) VALUES (101, 'p1', 'u1', 5, 'f') USING OPERATION_ID 'op-p1'",
        "INSERT INTO children (id, pcode, y) VALUES (102, 'p1', 'y1') USING OPERATION_ID 'op-c1'",
        "INSERT INTO tree (id, nid, up) VALUES (103, 't1', NULL) USING OPERATION_ID 'op-t1'",
    ] {
        run(core, session, sql).expect(sql);
    }
}

/// 改名後の新しい名前で一意性・CHECK・参照整合性が効くことを確かめる。
fn assert_constraints_effective(core: &EngineCore, session: &mut SessionState) {
    // PK（旧 code → k）。
    assert_eq!(
        code_of(
            core,
            session,
            "INSERT INTO parents (id, k, uu, cc, free) VALUES (104, 'p1', 'u2', 5, 'f') USING OPERATION_ID 'op-d1'"
        ),
        "23505"
    );
    // UNIQUE（旧 u → uu）。
    assert_eq!(
        code_of(
            core,
            session,
            "INSERT INTO parents (id, k, uu, cc, free) VALUES (105, 'p2', 'u1', 5, 'f') USING OPERATION_ID 'op-d2'"
        ),
        "23505"
    );
    // CHECK（旧 c → cc）。
    assert_eq!(
        code_of(
            core,
            session,
            "INSERT INTO parents (id, k, uu, cc, free) VALUES (106, 'p3', 'u3', -1, 'f') USING OPERATION_ID 'op-d3'"
        ),
        "23514"
    );
    assert_eq!(
        code_of(
            core,
            session,
            "UPDATE parents SET cc = -2 WHERE id = 101 USING OPERATION_ID 'op-u1'"
        ),
        "23514"
    );
    // 子側 FK（旧 pcode → pk2）・親側 FK（parents.k）。
    assert_eq!(
        code_of(
            core,
            session,
            "INSERT INTO children (id, pk2, y) VALUES (107, 'nope', 'y') USING OPERATION_ID 'op-d4'"
        ),
        "23503"
    );
    assert_eq!(
        code_of(
            core,
            session,
            "DELETE FROM parents WHERE id = 101 USING OPERATION_ID 'op-x1'"
        ),
        "23503"
    );
    // 自己参照 FK（旧 up → upper_id・旧 nid → node_id）。
    assert_eq!(
        code_of(
            core,
            session,
            "INSERT INTO tree (id, node_id, upper_id) VALUES (108, 't2', 'zz') USING OPERATION_ID 'op-d5'"
        ),
        "23503"
    );
    // 正しい行は通る。
    run(
        core,
        session,
        "INSERT INTO parents (id, k, uu, cc, free) VALUES (109, 'p4', 'u4', 7, 'f') USING OPERATION_ID 'op-ok1'",
    )
    .expect("valid parent");
    run(
        core,
        session,
        "INSERT INTO children (id, pk2, y) VALUES (110, 'p4', 'y') USING OPERATION_ID 'op-ok2'",
    )
    .expect("valid child");
    run(
        core,
        session,
        "INSERT INTO tree (id, node_id, upper_id) VALUES (111, 't3', 't1') USING OPERATION_ID 'op-ok3'",
    )
    .expect("valid self reference");
}

fn rename_all(core: &EngineCore, session: &mut SessionState) {
    for sql in [
        "ALTER TABLE parents RENAME COLUMN code TO k",
        "ALTER TABLE parents RENAME COLUMN u TO uu",
        "ALTER TABLE parents RENAME COLUMN c TO cc",
        "ALTER TABLE children RENAME COLUMN pcode TO pk2",
        "ALTER TABLE tree RENAME COLUMN nid TO node_id",
        "ALTER TABLE tree RENAME COLUMN up TO upper_id",
        "ALTER TABLE docs RENAME COLUMN note TO memo",
    ] {
        run(core, session, sql).expect(sql);
    }
}

#[test]
fn dependent_columns_follow_rename_and_constraints_stay_effective() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    setup_constrained(&core, &mut session);
    rename_all(&core, &mut session);
    assert_constraints_effective(&core, &mut session);
    // 旧名は存在しない。
    assert_eq!(
        code_of(&core, &mut session, "SELECT code FROM parents LIMIT 1"),
        "42703"
    );
    // 索引宣言は新しい名前を指す（再宣言は同名で 42P07、DROP INDEX は成功、
    // 新しい列名での DROP COLUMN は宣言を連鎖削除する）。
    assert_eq!(
        code_of(&core, &mut session, "CREATE INDEX idx_note ON docs (memo)"),
        "42P07"
    );
    run(&core, &mut session, "ALTER TABLE docs DROP COLUMN memo").expect("drop renamed column");
    run(&core, &mut session, "CREATE INDEX idx_note ON docs (label)")
        .expect("declaration was cascaded away");
    // 依存のない列は引き続き改名できる。
    run(
        &core,
        &mut session,
        "ALTER TABLE parents RENAME COLUMN free TO free2",
    )
    .expect("independent column");
}

#[test]
fn constraints_stay_effective_after_reopen() {
    let path = unique_db_path("sql-ddl-rename-column-dep-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage.create_table(&schema()).expect("create");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = ddl_session();
        setup_constrained(&core, &mut session);
        rename_all(&core, &mut session);
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = ddl_session();
    assert_constraints_effective(&core, &mut session);
}

#[test]
fn rename_there_and_back_keeps_key_checks_correct() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    setup_constrained(&core, &mut session);
    for sql in [
        "ALTER TABLE parents RENAME COLUMN code TO k",
        "ALTER TABLE parents RENAME COLUMN k TO code",
    ] {
        run(&core, &mut session, sql).expect(sql);
    }
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "INSERT INTO parents (id, code, u, c, free) VALUES (112, 'p1', 'u9', 5, 'f') USING OPERATION_ID 'op-b1'"
        ),
        "23505"
    );
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "INSERT INTO children (id, pcode, y) VALUES (113, 'nope', 'y') USING OPERATION_ID 'op-b2'"
        ),
        "23503"
    );
}

#[test]
fn table_with_view_rejects_rename_conservatively() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE VIEW v_docs AS SELECT note FROM docs",
    )
    .expect("view");
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE docs RENAME COLUMN qty TO q"
        ),
        "0A000"
    );
}

#[test]
fn vector_column_can_be_renamed_and_searched() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note", "'x'");
    run(
        &core,
        &mut session,
        "ALTER TABLE docs RENAME COLUMN embedding TO vec",
    )
    .expect("rename vector column");
    let rows = core
        .execute_sql(
            &ctx(),
            "SELECT id FROM docs ORDER BY vec <=> '[0.1,0.2]' LIMIT 5",
        )
        .expect("search on renamed vector column");
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn rename_to_dropped_column_name_and_readd_old_name() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note, qty", "'keep', 3");
    run(&core, &mut session, "ALTER TABLE docs DROP COLUMN label").expect("drop");
    // 墓標と同じ名前への改名は許可され、墓標の値は復活しない。
    run(
        &core,
        &mut session,
        "ALTER TABLE docs RENAME COLUMN note TO label",
    )
    .expect("rename");
    let cells = select_col(&core, "label");
    assert!(matches!(&cells[0], Cell::Text(t) if t == "keep"));
    // 改名後、新しい名前での ADD は 42701、古い名前での ADD は成功し NULL。
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE docs ADD COLUMN label TEXT"
        ),
        "42701"
    );
    run(&core, &mut session, "ALTER TABLE docs ADD COLUMN note TEXT").expect("re-add old name");
    assert!(matches!(select_col(&core, "note")[0], Cell::Null));
}
