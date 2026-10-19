//! `ON COMMIT` フック（`SessionState::apply_temp_on_commit`）の単体テスト
//! （TABLE-44・Issue #1683、#1684 で SQL 経由の観測へ置き換え）。自動コミット・明示 COMMIT・
//! ROLLBACK それぞれでの行の消え方（`DELETE ROWS`）と表の消え方（`DROP`）を、一時表への
//! `INSERT`／`SELECT COUNT(*)` で検証する。

use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const T: &str = "tenant-a";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities(T, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn exec(core: &EngineCore, s: &mut SessionState, sql: &str) -> Result<(), String> {
    core.execute_sql_in_session(&ctx(), s, sql)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
}

/// 一時表へ 1 行置く（自動コミット。`ON COMMIT DELETE ROWS` の表は直後に空になるため、
/// 明示トランザクション内で使う場合以外は `put_in` を使う）。
fn put(core: &EngineCore, s: &mut SessionState, table: &str, row_id: u64) {
    exec(
        core,
        s,
        &format!("INSERT INTO {table} (id, a) VALUES ({row_id}, 'v')"),
    )
    .expect("insert");
}

fn put_in<'e>(
    core: &'e EngineCore,
    s: &mut SessionState,
    txn: &mut crate::sql::transaction::SessionTransaction<'e>,
    table: &str,
    row_id: u64,
) {
    run(
        core,
        s,
        txn,
        &format!("INSERT INTO {table} (id, a) VALUES ({row_id}, 'v')"),
    )
    .expect("insert in txn");
}

fn rows(core: &EngineCore, s: &mut SessionState, table: &str) -> i64 {
    match core
        .execute_sql_in_session(&ctx(), s, &format!("SELECT COUNT(*) FROM {table}"))
        .expect("count")
    {
        SqlOutcome::Query(r) => match r.rows.first().and_then(|row| row.cells.first()) {
            Some(crate::sql::exec::Cell::Integer(n)) => i64::try_from(*n).expect("fits"),
            other => panic!("unexpected count cell: {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

fn run<'e>(
    core: &'e EngineCore,
    s: &mut SessionState,
    txn: &mut crate::sql::transaction::SessionTransaction<'e>,
    sql: &str,
) -> Result<(), String> {
    core.execute_sql_in_txn(&ctx(), s, txn, sql)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
}

fn new_session() -> SessionState {
    SessionState::default()
}

#[test]
fn table_44_delete_rows_clears_rows_after_a_successful_autocommit_statement() {
    let path = unique_db_path("temp-on-commit-auto");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = new_session();
    exec(
        &core,
        &mut s,
        "CREATE TEMP TABLE d (a TEXT) ON COMMIT DELETE ROWS",
    )
    .unwrap();
    exec(&core, &mut s, "CREATE TEMP TABLE p (a TEXT)").unwrap();

    // 自動コミットの INSERT 自体が commit なので、DELETE ROWS の表は同じ文の終わりで空になる。
    put(&core, &mut s, "d", 1);
    put(&core, &mut s, "p", 1);
    assert_eq!((rows(&core, &mut s, "d"), rows(&core, &mut s, "p")), (0, 1));

    // 失敗した文（重複 `42P07`）では適用されない: DELETE ROWS の表へは明示トランザクションで
    // 行を置く必要があるため、ここでは PRESERVE ROWS の行が失敗した文の後も残ることを確かめる。
    assert_eq!(
        exec(&core, &mut s, "CREATE TEMP TABLE p (a TEXT)"),
        Err("42P07".to_string())
    );
    assert_eq!(rows(&core, &mut s, "p"), 1);
    assert!(s.temp_tables().contains("d"));
    assert_eq!(
        exec(&core, &mut s, "CREATE TEMP TABLE d (a TEXT)"),
        Err("42P07".to_string())
    );
}

#[test]
fn table_44_explicit_commit_applies_and_rollback_does_not() {
    let path = unique_db_path("temp-on-commit-explicit");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = new_session();
    exec(
        &core,
        &mut s,
        "CREATE TEMP TABLE d (a TEXT) ON COMMIT DELETE ROWS",
    )
    .unwrap();
    let mut txn = core.new_session_transaction();

    // ROLLBACK では書いた行ごと消え、表は残る。
    run(&core, &mut s, &mut txn, "BEGIN").unwrap();
    put_in(&core, &mut s, &mut txn, "d", 1);
    run(&core, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(rows(&core, &mut s, "d"), 0);

    // トランザクション内では見え、COMMIT で消える。
    run(&core, &mut s, &mut txn, "BEGIN").unwrap();
    put_in(&core, &mut s, &mut txn, "d", 1);
    put_in(&core, &mut s, &mut txn, "d", 2);
    run(&core, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(rows(&core, &mut s, "d"), 0);
    assert!(s.temp_tables().contains("d"));
}

#[test]
fn table_44_drop_removes_the_table_and_its_storage() {
    let path = unique_db_path("temp-on-commit-drop");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = new_session();
    // 自動コミットの CREATE は同じ文の終わりで消える（PG の自動コミット時と同じ）。
    exec(&core, &mut s, "CREATE TEMP TABLE x (a TEXT) ON COMMIT DROP").unwrap();
    assert!(!s.temp_tables().contains("x"));
    exec(&core, &mut s, "CREATE TEMP TABLE x (a TEXT) ON COMMIT DROP").unwrap();

    // 手で登録した DROP の表は、行を置いた後の commit で表も実体も無くなる。
    exec(&core, &mut s, "CREATE TEMP TABLE keep (a TEXT)").unwrap();
    put(&core, &mut s, "keep", 1);
    let store = s.temp_rows_arc_for_test().expect("store");
    let base = store.used_bytes().expect("used");
    store.create_table(&sch("y")).expect("create y in storage");
    s.temp_tables_mut()
        .register(sch("y"), crate::sql::allowlist::TempOnCommit::Drop, 64)
        .expect("register y");
    s.apply_temp_on_commit().expect("hook");
    assert!(!s.temp_tables().contains("y"));
    assert!(s.temp_tables().contains("keep"));
    assert_eq!(rows(&core, &mut s, "keep"), 1);
    // 実体も消えているので、同名を作り直せる。
    exec(&core, &mut s, "CREATE TEMP TABLE y (a TEXT)").unwrap();
    assert!(store.used_bytes().expect("used") >= base);
}

fn sch(name: &str) -> crate::catalog::TableSchema {
    crate::catalog::TableSchema::new(
        name,
        vec![crate::catalog::ColumnDef::new(
            "a",
            crate::catalog::ColumnType::Text,
            true,
        )],
    )
}

#[test]
fn table_44_hook_is_a_noop_without_targets() {
    let path = unique_db_path("temp-on-commit-noop");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = new_session();
    // 一時表が無いセッションは一時 DB も作らない。
    s.apply_temp_on_commit().expect("empty");
    assert!(s.temp_rows_arc_for_test().is_none());
    exec(&core, &mut s, "CREATE TEMP TABLE p (a TEXT)").unwrap();
    put(&core, &mut s, "p", 1);
    s.apply_temp_on_commit().expect("preserve only");
    assert_eq!(rows(&core, &mut s, "p"), 1);
}
