//! 一時表の行ストア `temp_rows` のテスト（Issue #1682、対象ビヘイビア: TABLE-44）。
//!
//! 役割: 行の往復・トランザクション原子性・総バイト数上限（`54000`）・関係の分離・テナント固定・
//! 切断（drop）での破棄・永続側に触れないこと・キャッシュのゲート・物理上限の backstop を検証する。

use std::sync::Arc;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::allowlist::{SqlSurfaceError, TempOnCommit};
use crate::sql::mode::SessionState;
use crate::sql::temp_catalog::{SessionTempCatalog, TempRelationId};
use crate::sql::temp_rows::{TempRowStore, DEFAULT_MAX_TEMP_BYTES_PER_SESSION};
use crate::storage::read_source::ReadSource;
use crate::storage::Storage;
use crate::test_util::temp_db::tempdir;

const T: &str = "tenant-a";

fn rel(n: u64) -> TempRelationId {
    TempRelationId::from_raw(n)
}

fn store(max: u64) -> TempRowStore {
    TempRowStore::new(T, max).expect("store")
}

fn code(e: &SqlSurfaceError) -> &'static str {
    e.wire_code()
}

fn schema(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("x", ColumnType::Text, true)])
}

/// 1 行あたりの論理コスト（tenant 長 + 8 + 値長）。
fn cost(len: usize) -> u64 {
    (T.len() + 8 + len) as u64
}

#[test]
fn table_44_row_roundtrip() {
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION);
    let w = s.begin_write().unwrap();
    assert!(w.insert_row(rel(1), T, 1, b"aa").unwrap());
    assert!(w.insert_row(rel(1), T, 2, b"bb").unwrap());
    assert!(!w.insert_row(rel(1), T, 1, b"zz").unwrap(), "duplicate id");
    assert_eq!(w.get_row(rel(1), T, 1).unwrap(), Some(b"aa".to_vec()));
    w.put_row(rel(1), T, 2, b"BBB").unwrap();
    assert!(w.delete_row(rel(1), T, 1).unwrap());
    assert!(!w.delete_row(rel(1), T, 1).unwrap());
    w.commit().unwrap();

    let r = s.begin_read().unwrap();
    assert_eq!(r.get_row(rel(1), T, 1).unwrap(), None);
    assert_eq!(
        r.scan_relation(rel(1), T).unwrap(),
        vec![(2, b"BBB".to_vec())]
    );
    assert!(r.scan_relation(rel(9), T).unwrap().is_empty());
    assert_eq!(s.used_bytes().unwrap(), cost(3));
}

#[test]
fn table_44_abort_restores_rows_and_accounting() {
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION);
    let w = s.begin_write().unwrap();
    w.insert_row(rel(1), T, 1, b"keep").unwrap();
    w.commit().unwrap();
    let before = s.used_bytes().unwrap();

    let w = s.begin_write().unwrap();
    w.insert_row(rel(1), T, 2, b"drop-me").unwrap();
    w.abort().unwrap();
    assert_eq!(s.used_bytes().unwrap(), before);
    let r = s.begin_read().unwrap();
    assert_eq!(r.scan_relation(rel(1), T).unwrap().len(), 1);
}

#[test]
fn table_44_limit_is_enforced_with_54000() {
    let max = cost(10) * 2;
    let s = store(max);
    let w = s.begin_write().unwrap();
    w.insert_row(rel(1), T, 1, &[0u8; 10]).unwrap();
    w.insert_row(rel(1), T, 2, &[0u8; 10]).unwrap();
    let err = w.insert_row(rel(1), T, 3, &[0u8; 1]).expect_err("over");
    assert_eq!(code(&err), "54000");
    assert!(w.get_row(rel(1), T, 3).unwrap().is_none());
    // 置換で 1 バイト増えても超過。
    let err = w.put_row(rel(1), T, 1, &[0u8; 11]).expect_err("over");
    assert_eq!(code(&err), "54000");
    w.commit().unwrap();
    assert_eq!(s.used_bytes().unwrap(), max);

    // 削除・truncate で空きができれば再び入る。
    let w = s.begin_write().unwrap();
    assert!(w.delete_row(rel(1), T, 1).unwrap());
    w.insert_row(rel(1), T, 3, &[0u8; 10]).unwrap();
    w.truncate_relation(rel(1)).unwrap();
    w.insert_row(rel(1), T, 4, &[0u8; 10]).unwrap();
    w.commit().unwrap();
    assert_eq!(s.used_bytes().unwrap(), cost(10));
}

#[test]
fn table_44_oversized_value_is_rejected_before_allocation() {
    let s = store(u64::MAX);
    let w = s.begin_write().unwrap();
    let big = vec![0u8; 4 * 1024 * 1024 + 4096 + 1];
    let err = w.insert_row(rel(1), T, 1, &big).expect_err("too big");
    assert_eq!(code(&err), "54000");
}

#[test]
fn table_44_relations_are_isolated_and_orphans_are_swept() {
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION);
    let mut cat = SessionTempCatalog::default();
    let a = cat
        .register(schema("a"), TempOnCommit::PreserveRows, 8)
        .expect("register a");
    let b = cat
        .register(schema("b"), TempOnCommit::PreserveRows, 8)
        .expect("register b");
    let w = s.begin_write().unwrap();
    w.insert_row(a, T, 1, b"a1").unwrap();
    w.insert_row(b, T, 1, b"b1").unwrap();
    assert_eq!(w.get_row(a, T, 1).unwrap(), Some(b"a1".to_vec()));
    assert_eq!(w.get_row(b, T, 1).unwrap(), Some(b"b1".to_vec()));

    // DROP 相当: 同名を新しい id で作り直しても古い行は見えない。
    let removed = cat.remove("a").expect("removed");
    w.drop_relation(removed).unwrap();
    let a2 = cat
        .register(schema("a"), TempOnCommit::PreserveRows, 8)
        .expect("re-register");
    assert_ne!(a2, removed);
    assert!(w.get_row(a2, T, 1).unwrap().is_none());

    // カタログから消えた b の行領域は retain_relations が掃除する。
    cat.remove("b");
    w.retain_relations(&cat).unwrap();
    assert!(w.get_row(b, T, 1).unwrap().is_none());
    w.commit().unwrap();
    assert_eq!(s.used_bytes().unwrap(), 0);
}

#[test]
fn table_44_tenant_is_pinned_and_errors_do_not_leak() {
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION);
    let w = s.begin_write().unwrap();
    let other = "tenant-secret-b";
    for err in [
        w.insert_row(rel(1), other, 1, b"x")
            .map(|_| ())
            .unwrap_err(),
        w.put_row(rel(1), other, 1, b"x").unwrap_err(),
        w.delete_row(rel(1), other, 1).map(|_| ()).unwrap_err(),
        w.get_row(rel(1), other, 1).map(|_| ()).unwrap_err(),
    ] {
        assert_eq!(code(&err), "XX000");
        assert!(!format!("{err:?}{err}").contains(other));
    }
    w.commit().unwrap();
    let r = s.begin_read().unwrap();
    assert!(r.get_row(rel(1), other, 1).is_err());
    assert!(r.scan_relation(rel(1), other).is_err());

    let mut session = SessionState::default();
    session.temp_rows_or_init(T, 1024).expect("init");
    assert!(session.temp_rows_or_init(other, 1024).is_err());
}

/// テスト専用: `SessionState` が保持する `Arc` の弱参照を取る。
fn weak_of(s: &SessionState) -> std::sync::Weak<TempRowStore> {
    Arc::downgrade(&s.temp_rows_arc_for_test().expect("store"))
}

#[test]
fn table_44_store_is_dropped_with_session_and_clones() {
    let mut session = SessionState::default();
    assert!(session.temp_rows().is_none());
    // 遅延生成: clone しても生成されない。
    assert!(session.clone().temp_rows().is_none());

    session.temp_rows_or_init(T, 1024 * 1024).expect("init");
    {
        let w = session.temp_rows().unwrap().begin_write().unwrap();
        w.insert_row(rel(1), T, 1, b"row").unwrap();
        w.commit().unwrap();
    }
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
                let s = store(1024 * 1024);
                let w = s.begin_write().unwrap();
                w.insert_row(rel(1), T, 1, b"temp-only").unwrap();
                w.commit().unwrap();
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
fn table_44_read_txn_never_exposes_a_cache_snapshot() {
    let s = store(1024);
    let r = s.begin_read().unwrap();
    assert!(ReadSource::snapshot(&r).is_none());
}

#[test]
fn table_44_physical_backstop_fails_closed() {
    // 初期確保（約 1 MiB）の直後に数 MiB の物理上限を置き、論理上限は十分に大きくする。
    let s = TempRowStore::with_limits(T, u64::MAX, 3 * 1024 * 1024).expect("store");
    let chunk = vec![7u8; 64 * 1024];
    let mut failed = false;
    {
        let w = s.begin_write().unwrap();
        for i in 0..256u64 {
            if w.insert_row(rel(1), T, i, &chunk).is_err() {
                failed = true;
                break;
            }
        }
        failed |= w.commit().is_err();
    }
    assert!(failed, "physical growth must be refused");
    // 以後の操作はすべて fail-closed。
    assert!(s.begin_write().is_err());
    assert!(s.begin_read().is_err());
}

#[test]
fn table_44_memory_stays_bounded_by_churn() {
    let s = store(DEFAULT_MAX_TEMP_BYTES_PER_SESSION);
    let chunk = vec![1u8; 32 * 1024];
    for _ in 0..20 {
        let w = s.begin_write().unwrap();
        for i in 0..32u64 {
            w.put_row(rel(1), T, i, &chunk).unwrap();
        }
        w.commit().unwrap();
    }
    // 論理 1 MiB を 20 回書き換えても、解放ページが再利用されて物理長は数倍に収まる。
    assert!(
        s.physical_len() < 16 * 1024 * 1024,
        "len={}",
        s.physical_len()
    );
}
