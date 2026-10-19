//! 一時表の実体ストア `temp_rows` のテスト（Issue #1682、#1684 で再設計。対象ビヘイビア: TABLE-44）。
//!
//! 役割: 一時コアでの DML の往復・総バイト数上限（`54000`）・書き込みトランザクションの
//! 再入拒否（デッドロックしない）・テナント固定・切断（drop）での破棄・永続側に触れないこと・
//! 物理上限の backstop を検証する。

use std::sync::Arc;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::temp_rows::{TempRowStore, DEFAULT_MAX_TEMP_BYTES_PER_SESSION};
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::tempdir;

const T: &str = "tenant-a";

/// 設定の引き継ぎ元になる永続コア（一時コアはこのコアのストレージを使わない）。
fn template() -> (EngineCore, crate::test_util::temp_db::TempDir) {
    let dir = tempdir();
    let storage = Storage::open(dir.db_path()).expect("open");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(
            crate::recovery::required_op_id::LedgerMode::CompareOnlyWithoutLedger,
        ),
        dir,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn store(max: u64, tpl: &EngineCore) -> TempRowStore {
    TempRowStore::new(T, max, tpl).expect("store")
}

fn code(e: &SqlSurfaceError) -> &'static str {
    e.wire_code()
}

fn schema(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("x", ColumnType::Text, true)])
}

fn insert(core: &EngineCore, table: &str, id: u64, body: &str) -> Result<(), String> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(
        &ctx(T),
        &mut s,
        &format!("INSERT INTO {table} (id, x) VALUES ({id}, '{body}')"),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code().to_string())
}

#[test]
fn table_44_store_runs_dml_on_its_own_storage() {
    let (tpl, _dir) = template();
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION, &tpl);
    s.create_table(&schema("a")).unwrap();
    let core = s.core().unwrap();
    insert(core, "a", 1, "aa").unwrap();
    assert_eq!(insert(core, "a", 1, "zz"), Err("23505".to_string()));
    s.truncate_tables(&["a".to_string()]).unwrap();
    insert(core, "a", 1, "again").unwrap();
    s.drop_tables(&["a".to_string()]).unwrap();
    // 実体ごと消えたので、同名の表は存在しない。
    assert_eq!(insert(core, "a", 1, "x"), Err("42P01".to_string()));
    // 永続コア（テンプレート）にはこの表が無い。
    assert_eq!(insert(&tpl, "a", 1, "x"), Err("42P01".to_string()));
}

#[test]
fn table_44_budget_is_enforced_with_54000_and_released_by_delete() {
    let (tpl, _dir) = template();
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION, &tpl);
    s.create_table(&schema("a")).unwrap();
    insert(s.core().unwrap(), "a", 1, &"x".repeat(2000)).unwrap();
    let used = s.used_bytes().unwrap();
    assert!(used > 0);

    // 現在の使用量より小さい上限のストアでは、文の後・commit の前の判定が 54000 になる。
    // DDL（CREATE TEMP TABLE）も上限を迂回できない。超過なら作った表は戻る。
    let small = store(1, &tpl);
    let err = small
        .create_table(&schema("a"))
        .expect_err("DDL over budget");
    assert_eq!(code(&err), "54000");
    assert_eq!(
        insert(small.core().unwrap(), "a", 1, "x"),
        Err("42P01".to_string())
    );

    let ample = store(used.saturating_mul(4), &tpl);
    ample.create_table(&schema("a")).unwrap();
    let w = ample.begin_write().unwrap();
    ample.enforce_budget(&w).expect("within budget");
}

#[test]
fn table_44_second_begin_write_on_the_same_thread_errors_instead_of_blocking() {
    let (tpl, _dir) = template();
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION, &tpl);
    let first = s.begin_write().unwrap();
    let err = s.begin_write().err().expect("re-entrant begin_write");
    assert_eq!(code(&err), "55P03");
    drop(first);
    // 手放した後は再び取れる。
    s.begin_write().expect("after release");
}

#[test]
fn table_44_tenant_is_pinned_and_errors_do_not_leak() {
    let (tpl, _dir) = template();
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION, &tpl);
    let other = "tenant-secret-b";
    let err = s.check_tenant(other).expect_err("other tenant");
    assert_eq!(code(&err), "XX000");
    assert!(!format!("{err:?}{err}").contains(other));
    s.check_tenant(T).expect("owner");

    let mut session = SessionState::default();
    session.temp_rows_or_init(T, 1024, &tpl).expect("init");
    assert!(session.temp_rows_or_init(other, 1024, &tpl).is_err());
}

/// テスト専用: `SessionState` が保持する `Arc` の弱参照を取る。
fn weak_of(s: &SessionState) -> std::sync::Weak<TempRowStore> {
    Arc::downgrade(&s.temp_rows_arc_for_test().expect("store"))
}

#[test]
fn table_44_store_is_dropped_with_session_and_clones() {
    let (tpl, _dir) = template();
    let mut session = SessionState::default();
    assert!(session.temp_rows_arc_for_test().is_none());
    // 遅延生成: clone しても生成されない。
    assert!(session.clone().temp_rows_arc_for_test().is_none());

    session
        .temp_rows_or_init(T, 1024 * 1024, &tpl)
        .expect("init");
    let rollback_copy = session.clone();
    let weak = weak_of(&session);
    drop(session);
    assert!(weak.upgrade().is_some(), "clone still holds the store");
    drop(rollback_copy);
    assert!(
        weak.upgrade().is_none(),
        "store must be freed on disconnect"
    );
}

#[test]
fn table_44_persistent_db_and_data_dir_are_untouched() {
    // 同じ回数だけ永続 DB を開閉する 2 つのディレクトリを用意し、片方でだけ一時表へ書く。
    // 再オープン自体のファイル変化（ヘッダ更新等）は両者に共通なので、差が出れば一時表の影響。
    fn cycle(with_temp: bool) -> (u64, Vec<std::ffi::OsString>) {
        let dir = tempdir();
        let db = dir.db_path();
        for i in 0..3 {
            let storage = Storage::open(&db).expect("open");
            if with_temp && i == 2 {
                let tpl_dir = tempdir();
                let tpl = EngineCore::from_storage(
                    Storage::open(tpl_dir.db_path()).expect("open tpl"),
                    Box::new(CpuScalarProvider),
                );
                let s = store(1024 * 1024, &tpl);
                s.create_table(&schema("a")).unwrap();
                insert(s.core().unwrap(), "a", 1, "temp-only").unwrap();
            }
            drop(storage);
        }
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        files.sort();
        (std::fs::metadata(&db).unwrap().len(), files)
    }
    assert_eq!(cycle(false), cycle(true));
}

#[test]
fn table_44_physical_backstop_fails_closed() {
    let (tpl, _dir) = template();
    // 初期確保（約 1 MiB）の直後に数 MiB の物理上限を置き、論理上限は十分に大きくする。
    let s = TempRowStore::with_limits(T, u64::MAX, 3 * 1024 * 1024, &tpl).expect("store");
    s.create_table(&schema("a")).unwrap();
    let chunk = "y".repeat(8 * 1024);
    let mut failed = false;
    for i in 0..1024u64 {
        match s.core() {
            Ok(core) => {
                if insert(core, "a", i, &chunk).is_err() {
                    failed = true;
                    break;
                }
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed, "physical growth must be refused");
    // 以後の操作はすべて fail-closed。
    let err = s.core().err().expect("exhausted store");
    assert_eq!(code(&err), "54000");
    assert!(s.begin_write().is_err());
}

#[test]
fn table_44_apply_on_commit_truncates_and_drops_in_one_txn() {
    let (tpl, _dir) = template();
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION, &tpl);
    s.create_table(&schema("a")).unwrap();
    s.create_table(&schema("b")).unwrap();
    let core = s.core().unwrap();
    insert(core, "a", 1, "aa").unwrap();
    insert(core, "b", 1, "bb").unwrap();
    s.apply_on_commit(&["a".to_string()], &["b".to_string()])
        .unwrap();
    // a は行だけ消え（同じ id を再挿入できる）、b は実体ごと消える。
    insert(core, "a", 1, "again").unwrap();
    assert_eq!(insert(core, "b", 1, "x"), Err("42P01".to_string()));
}
