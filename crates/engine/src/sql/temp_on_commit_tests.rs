//! `ON COMMIT` フック（`SessionState::apply_temp_on_commit`）の単体テスト
//! （TABLE-44・Issue #1683）。一時表への DML は #1684 までまだ SQL から書けないため、
//! 行ストアへ直接行を置いて、自動コミット・明示 COMMIT・ROLLBACK それぞれでの
//! 行の消え方（`DELETE ROWS`）と表の消え方（`DROP`）を検証する。

use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::sql::temp_catalog::TempRelationId;
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

fn id_of(s: &SessionState, name: &str) -> TempRelationId {
    s.temp_tables().get(name).expect("registered").id
}

/// 関係へ行を 1 行置く（DML が SQL から使えるようになるまでの代替）。
fn put(s: &SessionState, rel: TempRelationId, row_id: u64) {
    let store = s.temp_rows_arc_for_test().expect("row store");
    let w = store.begin_write().expect("begin_write");
    w.insert_row(rel, T, row_id, b"v").expect("insert");
    w.commit().expect("commit");
}

fn rows(s: &SessionState, rel: TempRelationId) -> usize {
    let store = s.temp_rows_arc_for_test().expect("row store");
    let r = store.begin_read().expect("begin_read");
    r.scan_relation(rel, T).expect("scan").len()
}

fn used(s: &SessionState) -> u64 {
    s.temp_rows_arc_for_test()
        .expect("row store")
        .used_bytes()
        .expect("used")
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
    let (d, p) = (id_of(&s, "d"), id_of(&s, "p"));

    put(&s, d, 1);
    put(&s, p, 1);
    assert_eq!((rows(&s, d), rows(&s, p)), (1, 1));

    // 失敗した文（重複 `42P07`）の後は消えない。
    assert_eq!(
        exec(&core, &mut s, "CREATE TEMP TABLE p (a TEXT)"),
        Err("42P07".to_string())
    );
    assert_eq!((rows(&s, d), rows(&s, p)), (1, 1));

    // 成功した自動コミットの文の後は DELETE ROWS だけが消える。表は残る。
    exec(&core, &mut s, "CREATE TEMP TABLE q (a TEXT)").unwrap();
    assert_eq!((rows(&s, d), rows(&s, p)), (0, 1));
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
    let d = id_of(&s, "d");
    let mut txn = core.new_session_transaction();

    // ROLLBACK では消えない。
    run(&core, &mut s, &mut txn, "BEGIN").unwrap();
    put(&s, d, 1);
    run(&core, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(rows(&s, d), 1);

    // COMMIT で消える。
    run(&core, &mut s, &mut txn, "BEGIN").unwrap();
    assert_eq!(rows(&s, d), 1, "BEGIN alone must not clear rows");
    run(&core, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(rows(&s, d), 0);
    assert!(s.temp_tables().contains("d"));
}

#[test]
fn table_44_drop_removes_the_table_and_its_row_area() {
    let path = unique_db_path("temp-on-commit-drop");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = new_session();
    // 自動コミットの CREATE は同じ文の終わりで消える（PG の自動コミット時と同じ）。
    exec(&core, &mut s, "CREATE TEMP TABLE x (a TEXT) ON COMMIT DROP").unwrap();
    assert!(!s.temp_tables().contains("x"));
    exec(&core, &mut s, "CREATE TEMP TABLE x (a TEXT) ON COMMIT DROP").unwrap();

    // 手で登録した DROP の表は、行を置いた後の commit で表も行領域も無くなる。
    exec(&core, &mut s, "CREATE TEMP TABLE keep (a TEXT)").unwrap();
    let keep = id_of(&s, "keep");
    put(&s, keep, 1);
    let base = used(&s);
    let dropped_id = s
        .temp_tables_mut()
        .register(sch("y"), crate::sql::allowlist::TempOnCommit::Drop, 64)
        .expect("register y");
    put(&s, dropped_id, 1);
    assert!(used(&s) > base);
    s.apply_temp_on_commit().expect("hook");
    assert!(!s.temp_tables().contains("y"));
    assert!(s.temp_tables().contains("keep"));
    assert_eq!(used(&s), base, "dropped relation's bytes are released");
    assert_eq!(rows(&s, keep), 1);
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
    // 一時表が無いセッションは行ストアも作らない。
    s.apply_temp_on_commit().expect("empty");
    assert!(s.temp_rows_arc_for_test().is_none());
    exec(&core, &mut s, "CREATE TEMP TABLE p (a TEXT)").unwrap();
    let p = id_of(&s, "p");
    put(&s, p, 1);
    s.apply_temp_on_commit().expect("preserve only");
    assert_eq!(rows(&s, p), 1);
}
