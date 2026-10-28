//! `DROP TABLE a, b`（複数テーブル指定。Issue #1639・親 #1638、対象ビヘイビア:
//! TABLE-37 (1)・SQL-23）の結合テスト。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-37・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → DDL 権限ゲート →
//! `sql::ddl::execute_drop_table` → `catalog::Storage::drop_tables`）を production 経路として
//! 検証する。契約: 文全体が単一 write txn で原子的（不在・種類違い・依存のどれか 1 つでも
//! あれば何も削除しない）、`IF EXISTS` はリスト全体に掛かる、同じ文に並べた名前どうしの
//! 依存は数えない、権限の無いセッションは存否に関わらず `42501`、`CASCADE`／`RESTRICT` 単独は
//! 受理（Issue #1641。`sql_drop_table_cascade.rs`）で、2 語の併記は引き続き `42601`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
use crate::legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn granted() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = granted();
    core.execute_sql_in_session_legacy(&ctx("sys"), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    run(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, sql: &str) -> String {
    run(core, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

/// 表が存在するか（`SELECT` が `42P01` 以外で成功するか）。
fn exists(core: &EngineCore, table: &str) -> bool {
    match run(core, &format!("SELECT id FROM {table} LIMIT 1")) {
        Ok(_) => true,
        Err(e) => {
            assert_eq!(e.wire_code(), "42P01", "unexpected error probing {table}");
            false
        }
    }
}

fn mk(core: &EngineCore, names: &[&str]) {
    for n in names {
        ok(core, &format!("CREATE TABLE {n} (name TEXT)"));
    }
}

fn skipped(out: SqlOutcome) -> bool {
    match out {
        SqlOutcome::DropTable(o) => o.skipped(),
        other => panic!("expected DropTable outcome, got {other:?}"),
    }
}

#[test]
fn drops_all_named_tables_in_one_statement() {
    let (core, _g) = new_core("drop-multi-ok");
    mk(&core, &["a", "b", "c"]);
    let out = ok(&core, "DROP TABLE a, b");
    assert!(!skipped(out));
    assert!(!exists(&core, "a"));
    assert!(!exists(&core, "b"));
    assert!(exists(&core, "c"));
    // 権限ゲートは `;` 終端や大文字小文字でも変わらない。
    mk(&core, &["a", "b"]);
    ok(&core, "drop table a ,b;");
    assert!(!exists(&core, "a") && !exists(&core, "b"));
}

fn vec_schema(n: &str) -> TableSchema {
    TableSchema::new(
        n,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn seed(core: &EngineCore, tenant: &PolicyContext, table: &str, id: u64) {
    core.insert_row(
        tenant,
        table,
        id,
        &RowInput {
            tenant_id: tenant.tenant_id(),
            visibility: Visibility::Public,
            embedding: &[0.1f32, 0.2f32],
            metadata: &[],
        },
        Some(&OperationId::parse(&format!("seed-{table}-{id}")).expect("op id")),
    )
    .expect("insert row (operation_id must be unused: ledger removed with the table)");
}

/// 両表の全テナントの行と台帳が消え、同名再作成後に旧 operation_id を再利用できる。
#[test]
fn rows_and_ledger_of_every_table_are_removed() {
    let path = unique_db_path("drop-multi-data");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for n in ["ma", "mb"] {
        storage.create_table(&vec_schema(n)).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    for n in ["ma", "mb"] {
        seed(&core, &alice, n, 1);
        seed(&core, &bob, n, 2);
    }
    let out = ok(&core, "DROP TABLE ma, mb");
    assert!(!skipped(out));
    assert!(!exists(&core, "ma") && !exists(&core, "mb"));
    for n in ["ma", "mb"] {
        ok(&core, &format!("CREATE TABLE {n} (embedding VECTOR(2))"));
        seed(&core, &alice, n, 1);
    }
}

#[test]
fn missing_name_aborts_whole_statement() {
    let (core, _g) = new_core("drop-multi-missing");
    mk(&core, &["a"]);
    assert_eq!(code(&core, "DROP TABLE a, ghost"), "42P01");
    assert!(exists(&core, "a"));
    assert_eq!(code(&core, "DROP TABLE ghost, a"), "42P01");
    assert!(exists(&core, "a"));
}

#[test]
fn wrong_object_kind_aborts_whole_statement() {
    let (core, _g) = new_core("drop-multi-kind");
    mk(&core, &["a"]);
    ok(&core, "CREATE VIEW v AS SELECT id FROM a LIMIT 5");
    ok(&core, "CREATE INDEX idx ON a (name)");
    assert_eq!(code(&core, "DROP TABLE a, v"), "42809");
    assert_eq!(code(&core, "DROP TABLE a, idx"), "42809");
    assert!(exists(&core, "a"));
}

#[test]
fn error_priority_follows_listing_order() {
    let (core, _g) = new_core("drop-multi-order");
    mk(&core, &["a"]);
    ok(&core, "CREATE VIEW v AS SELECT id FROM a LIMIT 5");
    assert_eq!(code(&core, "DROP TABLE ghost, v"), "42P01");
    assert_eq!(code(&core, "DROP TABLE v, ghost"), "42809");
}

#[test]
fn external_foreign_key_dependent_aborts_whole_statement() {
    let (core, _g) = new_core("drop-multi-fk-ext");
    mk(&core, &["p", "x"]);
    ok(&core, "CREATE TABLE d (pid BIGINT REFERENCES p, note TEXT)");
    assert_eq!(code(&core, "DROP TABLE x, p"), "2BP01");
    assert!(exists(&core, "p") && exists(&core, "x") && exists(&core, "d"));
}

#[test]
fn view_dependent_aborts_whole_statement() {
    let (core, _g) = new_core("drop-multi-view-dep");
    mk(&core, &["a", "b"]);
    ok(&core, "CREATE VIEW v AS SELECT id FROM b LIMIT 5");
    assert_eq!(code(&core, "DROP TABLE a, b"), "2BP01");
    assert!(exists(&core, "a") && exists(&core, "b"));
}

#[test]
fn dependencies_among_listed_names_are_not_counted() {
    let (core, _g) = new_core("drop-multi-fk-internal");
    for (parent, child, first, second) in [("p1", "c1", "p1", "c1"), ("p2", "c2", "c2", "p2")] {
        mk(&core, &[parent]);
        ok(
            &core,
            &format!("CREATE TABLE {child} (pid BIGINT REFERENCES {parent}, note TEXT)"),
        );
        ok(&core, &format!("DROP TABLE {first}, {second}"));
        assert!(!exists(&core, parent) && !exists(&core, child));
    }
    // 自己参照を含む表と、それを参照する表を同時に落とせる。
    ok(&core, "CREATE TABLE s (name TEXT)");
    ok(&core, "CREATE TABLE t (sid BIGINT REFERENCES s, note TEXT)");
    ok(&core, "DROP TABLE s, t");
    assert!(!exists(&core, "s") && !exists(&core, "t"));
}

#[test]
fn if_exists_applies_to_the_whole_list() {
    let (core, _g) = new_core("drop-multi-ie");
    mk(&core, &["a", "base"]);
    ok(&core, "CREATE VIEW v AS SELECT id FROM base LIMIT 5");
    assert!(!skipped(ok(&core, "DROP TABLE IF EXISTS a, ghost")));
    assert!(!exists(&core, "a"));
    assert!(skipped(ok(&core, "DROP TABLE IF EXISTS g1, g2")));
    // 種類違いは読み替えない。
    mk(&core, &["b"]);
    assert_eq!(code(&core, "DROP TABLE IF EXISTS b, v"), "42809");
    assert!(exists(&core, "b"));
}

#[test]
fn duplicate_names_count_once() {
    let (core, _g) = new_core("drop-multi-dup");
    mk(&core, &["a"]);
    ok(&core, "DROP TABLE a, a");
    assert!(!exists(&core, "a"));
}

#[test]
fn without_permission_is_42501_regardless_of_existence() {
    let (core, _g) = new_core("drop-multi-perm");
    mk(&core, &["a", "b"]);
    let mut denied = SessionState::default();
    for sql in [
        "DROP TABLE a, b",
        "DROP TABLE ghost1, ghost2",
        "DROP TABLE a, ghost",
    ] {
        let err = core
            .execute_sql_in_session_legacy(&ctx("alice"), &mut denied, sql)
            .expect_err("denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
    }
    assert!(exists(&core, "a") && exists(&core, "b"));
}

#[test]
fn malformed_and_unsupported_shapes_stay_42601() {
    let (core, _g) = new_core("drop-multi-syntax");
    mk(&core, &["a", "b"]);
    let mut denied = SessionState::default();
    for sql in [
        "DROP TABLE a,",
        "DROP TABLE , a",
        "DROP TABLE a,,b",
        "DROP TABLE a b",
        // `CASCADE`／`RESTRICT` 単独は TABLE-37 (2)(3)（Issue #1641）で受理に変わった。
        "DROP TABLE a, b CASCADE RESTRICT",
        "DROP TABLE IF EXISTS a, b CASCADE CASCADE",
    ] {
        assert_eq!(code(&core, sql), "42601", "granted: {sql}");
        let err = core
            .execute_sql_in_session_legacy(&ctx("alice"), &mut denied, sql)
            .expect_err("denied");
        assert_eq!(err.wire_code(), "42601", "denied: {sql}");
    }
    assert!(exists(&core, "a") && exists(&core, "b"));
}

#[test]
fn explicit_transaction_rejects_multi_drop() {
    let (core, _g) = new_core("drop-multi-txn");
    mk(&core, &["a", "b"]);
    let mut s = granted();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("sys"), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&ctx("sys"), &mut s, &mut txn, "DROP TABLE a, b")
        .expect_err("DDL in txn");
    assert_eq!(err.wire_code(), "0A000");
    core.execute_sql_in_txn(&ctx("sys"), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(exists(&core, "a") && exists(&core, "b"));
}
