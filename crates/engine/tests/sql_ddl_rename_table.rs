//! `ALTER TABLE <t> RENAME TO <u>`（TABLE-38 のポインタ、Issue #1623）の SQL 表層結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → `require_ddl_permission` →
//! `sql::ddl::execute_alter_table_rename_table`）を production 経路として検証する
//! （`sql_ddl_rename_column.rs` と同じ流儀）。PK・UNIQUE・CHECK・FK（参照元・参照先）・
//! 索引宣言は改名に追随すること（Issue #1624）、単純形ビューの定義と `operation_id` 台帳の
//! 記録が改名に追随すること・評価後射影形ビューに依存される表の改名は `0A000` で拒否される
//! こと（Issue #1625）を固定する。

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

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("note", ColumnType::Text, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-ddl-rename-table");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create table");
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

/// 書き込み文（INSERT／UPDATE／DELETE）に未指定の `USING OPERATION_ID` を一意に補う。
fn with_op(sql: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let head = sql.trim_start().to_ascii_uppercase();
    let is_dml = ["INSERT", "UPDATE", "DELETE"]
        .iter()
        .any(|k| head.starts_with(k));
    if is_dml && !head.contains("USING OPERATION_ID") {
        format!(
            "{sql} USING OPERATION_ID 'auto-{}'",
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    } else {
        sql.to_string()
    }
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, &with_op(sql))
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    run_as(core, "alice", session, sql)
}

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    run(core, session, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn insert(core: &EngineCore, tenant: &str, table: &str, id: u64, note: &str) {
    let mut s = SessionState::default();
    run_as(
        core,
        tenant,
        &mut s,
        &format!(
            "INSERT INTO {table} (id, embedding, note) VALUES ({id}, '[0.1,0.2]', '{note}') \
             USING OPERATION_ID 'op-{tenant}-{table}-{id}'"
        ),
    )
    .expect("insert");
}

fn notes(core: &EngineCore, tenant: &str, table: &str) -> Result<Vec<String>, SqlSurfaceError> {
    let out = core.execute_sql(
        &ctx(tenant),
        &format!("SELECT note FROM {table} ORDER BY id LIMIT 100"),
    )?;
    Ok(out
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("one cell") {
            Cell::Text(s) => s,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect())
}

/// ビュー経由の SELECT（ビューは `id` を公開しないことがあるため `note` で並べる）。
fn view_notes(core: &EngineCore, tenant: &str, view: &str) -> Result<Vec<String>, SqlSurfaceError> {
    let out = core.execute_sql(
        &ctx(tenant),
        &format!("SELECT note FROM {view} ORDER BY note LIMIT 100"),
    )?;
    Ok(out
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("one cell") {
            Cell::Text(s) => s,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect())
}

#[test]
fn rename_moves_rows_per_tenant_and_old_name_is_gone() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, "alice", "docs", 1, "a1");
    insert(&core, "alice", "docs", 2, "a2");
    insert(&core, "bob", "docs", 3, "b3");
    // キャッシュを温める。
    assert_eq!(notes(&core, "alice", "docs").expect("warm").len(), 2);

    let out = run(&core, &mut session, "ALTER TABLE docs RENAME TO papers;").expect("rename");
    match out {
        SqlOutcome::AlterTable(o) => {
            assert_eq!(o.table_name, "docs");
            assert_eq!(
                o.action,
                AlterTableAction::RenameTable {
                    new_table_name: "papers".to_string()
                }
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }

    // テナント境界は物理キーとともに移る。
    assert_eq!(
        notes(&core, "alice", "papers").expect("alice"),
        ["a1", "a2"]
    );
    assert_eq!(notes(&core, "bob", "papers").expect("bob"), ["b3"]);
    assert_eq!(
        notes(&core, "carol", "papers").expect("carol"),
        Vec::<String>::new()
    );
    // 旧名は存在しない。
    assert_eq!(
        notes(&core, "alice", "docs").expect_err("old").wire_code(),
        "42P01"
    );
    assert_eq!(
        code_of(
            &core,
            &mut ddl_session(),
            "INSERT INTO docs (id, embedding, note) VALUES (9, '[0.1,0.2]', 'n') USING OPERATION_ID 'op-old'"
        ),
        "42P01"
    );

    // 改名後の DML と KNN 検索。
    insert(&core, "alice", "papers", 4, "a4");
    let mut s = SessionState::default();
    run(
        &core,
        &mut s,
        "UPDATE papers SET note = 'a1x' WHERE id = 1 USING OPERATION_ID 'op-u1'",
    )
    .expect("update");
    run(
        &core,
        &mut s,
        "DELETE FROM papers WHERE id = 2 USING OPERATION_ID 'op-d2'",
    )
    .expect("delete");
    assert_eq!(
        notes(&core, "alice", "papers").expect("alice"),
        ["a1x", "a4"]
    );
    let knn = core
        .execute_sql(
            &ctx("alice"),
            "SELECT id FROM papers ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10",
        )
        .expect("knn");
    assert_eq!(knn.rows.len(), 2);
}

#[test]
fn rename_of_never_written_table_succeeds() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(&core, &mut session, "ALTER TABLE docs RENAME TO papers").expect("rename");
    assert!(notes(&core, "alice", "papers").expect("empty").is_empty());
    insert(&core, "alice", "papers", 1, "x");
    assert_eq!(notes(&core, "alice", "papers").expect("one"), ["x"]);
}

#[test]
fn old_name_recreated_is_empty_and_round_trip_keeps_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, "alice", "docs", 1, "a1");
    assert_eq!(notes(&core, "alice", "docs").expect("warm"), ["a1"]);
    run(&core, &mut session, "ALTER TABLE docs RENAME TO papers").expect("rename");
    run(
        &core,
        &mut session,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, note TEXT)",
    )
    .expect("recreate");
    assert!(notes(&core, "alice", "docs").expect("fresh").is_empty());
    assert_eq!(notes(&core, "alice", "papers").expect("moved"), ["a1"]);
    // 往復（papers -> docs は衝突するので別名を経由する）。
    run(&core, &mut session, "DROP TABLE docs").expect("drop");
    run(&core, &mut session, "ALTER TABLE papers RENAME TO tmp").expect("to tmp");
    run(&core, &mut session, "ALTER TABLE tmp RENAME TO docs").expect("back");
    assert_eq!(notes(&core, "alice", "docs").expect("back"), ["a1"]);
}

#[test]
fn rename_survives_reopen() {
    let path = unique_db_path("sql-ddl-rename-table-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage.create_table(&schema("docs")).expect("create");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        insert(&core, "alice", "docs", 1, "a1");
        insert(&core, "bob", "docs", 2, "b2");
        run(
            &core,
            &mut ddl_session(),
            "ALTER TABLE docs RENAME TO papers",
        )
        .expect("rename");
    }
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    assert_eq!(notes(&core, "alice", "papers").expect("alice"), ["a1"]);
    assert_eq!(notes(&core, "bob", "papers").expect("bob"), ["b2"]);
    assert_eq!(
        notes(&core, "alice", "docs").expect_err("old").wire_code(),
        "42P01"
    );
}

#[test]
fn rename_error_codes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE TABLE other (id BIGINT PRIMARY KEY, note TEXT)",
    )
    .expect("other");
    run(
        &core,
        &mut session,
        "CREATE VIEW v_other AS SELECT note FROM other",
    )
    .expect("view");
    run(
        &core,
        &mut session,
        "CREATE INDEX idx_other ON other (note)",
    )
    .expect("index");
    let cases = [
        // 既存の表・ビュー・索引名、同名への改名
        ("ALTER TABLE docs RENAME TO other", "42P07"),
        ("ALTER TABLE docs RENAME TO v_other", "42P07"),
        ("ALTER TABLE docs RENAME TO idx_other", "42P07"),
        ("ALTER TABLE docs RENAME TO docs", "42P07"),
        // 不在の表、改名元がビュー・索引
        ("ALTER TABLE nope RENAME TO x", "42P01"),
        ("ALTER TABLE v_other RENAME TO x", "42809"),
        ("ALTER TABLE idx_other RENAME TO x", "42809"),
        // 構文
        ("ALTER TABLE docs RENAME TO \"bad name\"", "42601"),
        ("ALTER TABLE docs RENAME TO s.u", "42601"),
        ("ALTER TABLE docs RENAME TO", "42601"),
        ("ALTER TABLE docs RENAME TO x, ADD COLUMN z TEXT", "42601"),
        ("ALTER TABLE docs ADD COLUMN z TEXT, RENAME TO x", "42601"),
        ("ALTER TABLE docs RENAME CONSTRAINT a TO b", "42601"),
    ];
    for (sql, code) in cases {
        assert_eq!(code_of(&core, &mut session, sql), code, "{sql}");
    }
    // 拒否の後も表は旧名のまま。
    assert!(notes(&core, "alice", "docs")
        .expect("still docs")
        .is_empty());
}

#[test]
fn rename_requires_ddl_permission_without_existence_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut denied = SessionState::default();
    let existing = code_of(&core, &mut denied, "ALTER TABLE docs RENAME TO x");
    let missing = code_of(&core, &mut denied, "ALTER TABLE nope RENAME TO x");
    assert_eq!(existing, "42501");
    assert_eq!(existing, missing);
    assert!(notes(&core, "alice", "docs").is_ok());
}

#[test]
fn rename_inside_explicit_transaction_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let owner = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "ALTER TABLE docs RENAME TO x",
        )
        .expect_err("DDL inside an explicit transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert!(notes(&core, "alice", "docs").is_ok());
}

/// 評価後射影形（集計）ビューが読む表の改名は `0A000` で拒否され、何も変わらない。
#[test]
fn rename_with_buffered_view_is_refused_without_changes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for ddl in [
        "CREATE TABLE view_t (id BIGINT PRIMARY KEY, note TEXT)",
        "CREATE VIEW v_view_t AS SELECT note FROM view_t ORDER BY note LIMIT 5",
    ] {
        run(&core, &mut session, ddl).unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    let sql = "ALTER TABLE view_t RENAME TO renamed_x";
    assert_eq!(code_of(&core, &mut session, sql), "0A000", "{sql}");
    // 何も変わっていない: 旧名は存在し、新名は存在しない。
    assert!(notes(&core, "alice", "view_t").is_ok());
    assert!(view_notes(&core, "alice", "v_view_t").is_ok());
    assert_eq!(
        notes(&core, "alice", "renamed_x")
            .expect_err("no new name")
            .wire_code(),
        "42P01"
    );
}

/// 単純形ビュー（`*`・列リスト・`WHERE` 述語つき・ビューの連鎖）は改名に追随し、
/// ビュー経由の SELECT が改名前と同じ結果を返す。他テナントの行は見えない。
#[test]
fn rename_follows_simple_views_and_select_through_view_works() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for ddl in [
        "CREATE VIEW v_all AS SELECT * FROM docs",
        "CREATE VIEW v_note AS SELECT note FROM docs",
        "CREATE VIEW v_pred AS SELECT note FROM docs WHERE note = 'x'",
        "CREATE VIEW v_chain AS SELECT note FROM v_note",
    ] {
        exec(&core, &mut session, ddl);
    }
    insert(&core, "alice", "docs", 1, "x");
    insert(&core, "alice", "docs", 2, "y");
    insert(&core, "bob", "docs", 3, "bob-x");
    let before: Vec<_> = ["v_all", "v_note", "v_pred", "v_chain"]
        .iter()
        .map(|v| view_notes(&core, "alice", v).expect("before"))
        .collect();
    exec(&core, &mut session, "ALTER TABLE docs RENAME TO docs2");
    let after: Vec<_> = ["v_all", "v_note", "v_pred", "v_chain"]
        .iter()
        .map(|v| view_notes(&core, "alice", v).expect("after"))
        .collect();
    assert_eq!(before, after);
    assert_eq!(view_notes(&core, "alice", "v_pred").unwrap(), vec!["x"]);
    assert_eq!(view_notes(&core, "bob", "v_note").unwrap(), vec!["bob-x"]);
    // 往復改名でも追随する。
    exec(&core, &mut session, "ALTER TABLE docs2 RENAME TO docs");
    assert_eq!(
        view_notes(&core, "alice", "v_chain").unwrap(),
        vec!["x", "y"]
    );
}

/// 別の表を読むビューは改名の影響を受けない。
#[test]
fn rename_unrelated_views_untouched() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        "CREATE TABLE other (id BIGINT PRIMARY KEY, note TEXT)",
    );
    exec(
        &core,
        &mut session,
        "CREATE VIEW v_other AS SELECT note FROM other ORDER BY note LIMIT 5",
    );
    exec(&core, &mut session, "ALTER TABLE docs RENAME TO docs2");
    assert!(view_notes(&core, "alice", "v_other").is_ok());
}

/// 改名前に記録した `operation_id` を新名で再送すると、改名前の台帳で判定される。
/// 旧名への文は `42P01`。行は二重に挿入されない。
#[test]
fn resend_after_rename_is_judged_by_pre_rename_ledger() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let ins = |table: &str, note: &str| {
        format!(
            "INSERT INTO {table} (id, embedding, note) VALUES (1, '[0.1,0.2]', '{note}') \
             USING OPERATION_ID 'op-a'"
        )
    };
    exec(&core, &mut session, &ins("docs", "first"));
    exec(&core, &mut session, "ALTER TABLE docs RENAME TO docs2");
    assert_eq!(
        code_of(&core, &mut session, &ins("docs2", "first")),
        "23505"
    );
    assert_eq!(
        code_of(&core, &mut session, &ins("docs2", "other")),
        "22023"
    );
    assert_eq!(code_of(&core, &mut session, &ins("docs", "first")), "42P01");
    assert_eq!(notes(&core, "alice", "docs2").unwrap(), vec!["first"]);
}

/// 表名をハッシュに含む述語つき UPDATE の再送は、改名後は fail-closed で `22023`
/// （二重適用は起きない）。
#[test]
fn resend_of_predicate_update_after_rename_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, "alice", "docs", 1, "a");
    exec(
        &core,
        &mut session,
        "UPDATE docs SET note = 'b' WHERE note = 'a' USING OPERATION_ID 'op-u'",
    );
    exec(&core, &mut session, "ALTER TABLE docs RENAME TO docs2");
    let sql = "UPDATE docs2 SET note = 'b' WHERE note = 'a' USING OPERATION_ID 'op-u'";
    assert_eq!(code_of(&core, &mut session, sql), "22023");
    assert_eq!(notes(&core, "alice", "docs2").unwrap(), vec!["b"]);
}

/// `last_operation_id` は改名前の最後の `operation_id` を新名で返し、旧名では `NotFound`。
/// 他テナントの記録は見えない。改名後に旧名で再作成した表は旧記録を引き継がない。
#[test]
fn last_operation_id_follows_rename() {
    use engine::recovery::ledger::LastOperationLookup;
    use engine::recovery::required_op_id::OperationId;
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, "alice", "docs", 1, "a");
    exec(&core, &mut session, "ALTER TABLE docs RENAME TO docs2");
    let op = OperationId::parse("op-alice-docs-1").expect("op");
    assert_eq!(
        core.last_operation_id(&ctx("alice"), "docs2")
            .expect("lookup"),
        LastOperationLookup::Committed(op)
    );
    assert_eq!(
        core.last_operation_id(&ctx("alice"), "docs")
            .expect("lookup"),
        LastOperationLookup::NotFound
    );
    assert_eq!(
        core.last_operation_id(&ctx("bob"), "docs2")
            .expect("lookup"),
        LastOperationLookup::NotFound
    );
    // 旧名で再作成した表は旧記録を引き継がない（同じ operation_id の INSERT が成功する）。
    exec(
        &core,
        &mut session,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, note TEXT)",
    );
    exec(
        &core,
        &mut session,
        "INSERT INTO docs (id, note) VALUES (1, 'n') USING OPERATION_ID 'op-alice-docs-1'",
    );
}

fn exec(core: &EngineCore, session: &mut SessionState, sql: &str) {
    run(core, session, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// 参照先の表の改名: 子の FK 参照アクションが新しい名前で働き、旧名は消える。
#[test]
fn rename_parent_table_keeps_foreign_key_actions() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for ddl in [
        "CREATE TABLE par (id BIGINT NOT NULL UNIQUE, code TEXT PRIMARY KEY, note TEXT)",
        "CREATE TABLE kid_c (id BIGINT PRIMARY KEY, p TEXT, note TEXT, \
         FOREIGN KEY (p) REFERENCES par (code) ON DELETE CASCADE)",
        "CREATE TABLE kid_n (id BIGINT PRIMARY KEY, p TEXT, note TEXT, \
         FOREIGN KEY (p) REFERENCES par (code) ON DELETE SET NULL)",
        "CREATE TABLE kid_r (id BIGINT PRIMARY KEY, p TEXT, note TEXT, FOREIGN KEY (p) REFERENCES par (code))",
        "INSERT INTO par (id, code, note) VALUES (1, 'a', 'pa')",
        "INSERT INTO par (id, code, note) VALUES (2, 'b', 'pb')",
        "INSERT INTO kid_c (id, p, note) VALUES (1, 'a', 'c1')",
        "INSERT INTO kid_n (id, p, note) VALUES (1, 'a', 'n1')",
        "INSERT INTO kid_r (id, p, note) VALUES (1, 'b', 'r1')",
    ] {
        exec(&core, &mut session, ddl);
    }
    exec(&core, &mut session, "ALTER TABLE par RENAME TO par2");
    assert_eq!(
        code_of(&core, &mut session, "DELETE FROM par WHERE code = 'a'"),
        "42P01"
    );
    // NO ACTION の子が参照している行は消せない。
    assert_eq!(
        code_of(&core, &mut session, "DELETE FROM par2 WHERE code = 'b'"),
        "23503"
    );
    // 新規子行も新しい親を参照して検査される。
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "INSERT INTO kid_r (id, p, note) VALUES (2, 'zzz', 'bad')"
        ),
        "23503"
    );
    exec(&core, &mut session, "DELETE FROM par2 WHERE code = 'a'");
    assert_eq!(
        notes(&core, "alice", "kid_c").expect("kid_c"),
        Vec::<String>::new()
    );
    assert_eq!(notes(&core, "alice", "kid_n").expect("kid_n"), vec!["n1"]);
    let out = core
        .execute_sql(&ctx("alice"), "SELECT p FROM kid_n ORDER BY id LIMIT 10")
        .expect("select p");
    assert!(matches!(out.rows[0].cells[0], Cell::Null));
    // 親表を DROP しようとしても子が居れば拒否される（依存が新しい名前で追随している）。
    assert_eq!(code_of(&core, &mut session, "DROP TABLE par2"), "2BP01");
}

/// 参照元（子）・自己参照 FK・PK・UNIQUE・CHECK を持つ表の改名。制約名は変わらない。
#[test]
fn rename_keeps_constraints_and_their_names() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for ddl in [
        "CREATE TABLE par (id BIGINT NOT NULL UNIQUE, code TEXT PRIMARY KEY, note TEXT)",
        "CREATE TABLE tree (id BIGINT NOT NULL UNIQUE, code TEXT PRIMARY KEY, up TEXT, n INTEGER, note TEXT, \
         UNIQUE (up, n), CHECK (n > 0), \
         FOREIGN KEY (up) REFERENCES tree (code), \
         FOREIGN KEY (note) REFERENCES par (code))",
        "INSERT INTO par (id, code, note) VALUES (1, 'p', 'p')",
        "INSERT INTO tree (id, code, up, n, note) VALUES (1, 'root', NULL, 1, 'p')",
    ] {
        exec(&core, &mut session, ddl);
    }
    exec(&core, &mut session, "ALTER TABLE tree RENAME TO tree2");
    let ins = |id: u64, code: &str, up: &str, n: u32, note: &str| {
        format!(
            "INSERT INTO tree2 (id, code, up, n, note) VALUES ({id}, '{code}', '{up}', {n}, '{note}')"
        )
    };
    assert_eq!(
        code_of(&core, &mut session, &ins(2, "root", "root", 2, "p")),
        "23505"
    );
    assert_eq!(
        code_of(&core, &mut session, &ins(2, "c1", "ghost", 2, "p")),
        "23503"
    );
    assert_eq!(
        code_of(&core, &mut session, &ins(2, "c1", "root", 2, "ghost")),
        "23503"
    );
    assert_eq!(
        code_of(&core, &mut session, &ins(2, "c1", "root", 0, "p")),
        "23514"
    );
    exec(&core, &mut session, &ins(2, "c1", "root", 2, "p"));
    assert_eq!(
        code_of(&core, &mut session, &ins(3, "c2", "root", 2, "p")),
        "23505"
    );
    // 親 `par` の削除は改名後の子（tree2）が参照しているため拒否される。
    assert_eq!(code_of(&core, &mut session, "DROP TABLE par"), "2BP01");
    // 制約名は改名前のまま（`tree_*`）。
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE tree2 DROP CONSTRAINT tree2_pkey"
        ),
        "42704"
    );
    // FK を先に外す（PK は自己参照 FK の参照先のため）。
    for name in ["tree_up_fkey", "tree_pkey", "tree_up_n_key", "tree_check"] {
        exec(
            &core,
            &mut session,
            &format!("ALTER TABLE tree2 DROP CONSTRAINT {name}"),
        );
    }
}

/// 索引宣言は所有テーブルが新しい名前へ移り、索引名は変わらない。
#[test]
fn rename_moves_index_declarations_and_keeps_index_names() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for ddl in [
        "CREATE TABLE idx_t (id BIGINT PRIMARY KEY, code TEXT, note TEXT)",
        "CREATE INDEX idx_t_note ON idx_t (note)",
        "CREATE UNIQUE INDEX idx_t_code ON idx_t (code)",
        "INSERT INTO idx_t (id, code, note) VALUES (1, 'a', 'n')",
    ] {
        exec(&core, &mut session, ddl);
    }
    exec(&core, &mut session, "ALTER TABLE idx_t RENAME TO idx_u");
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "INSERT INTO idx_u (id, code, note) VALUES (2, 'a', 'x')"
        ),
        "23505"
    );
    // 宣言を読み戻す（core を閉じて同じ DB を開き直す。reopen 後も残ることの確認を兼ねる）。
    drop(session);
    drop(core);
    let storage = Storage::open(&_guard.0).expect("reopen storage");
    let defs = storage.list_indexes().expect("list indexes");
    for name in ["idx_t_note", "idx_t_code"] {
        let def = defs.iter().find(|d| d.name == name).expect(name);
        assert_eq!(def.table, "idx_u", "{name}");
    }
}
