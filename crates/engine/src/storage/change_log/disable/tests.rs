//! 変更ログの無効化の試験（Issue #3093・RECOVER-14 のポインタ）。
//!
//! 受け入れ条件「無効化後も DB が整合検査を通る」「無効化後の増分・PITR の要求が拒否される」に加え、
//! 未アーカイブ分の拒否と明示破棄、無効化の原子性（拒否時に何も書かない）、再有効化での識別子の
//! 作り直しを固定する。

use super::*;
use crate::storage::change_log::INITIAL_TIMELINE;
use crate::storage::{chain_point_of, ArchiveOptions, IntegrityOptions, RowInput, Visibility};
use crate::test_util::temp_db::TempDir;

fn put(storage: &Storage, id: u64) {
    storage
        .put(
            id,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            },
        )
        .expect("put");
}

fn enabled(label: &str, rows: u64) -> (TempDir, Storage) {
    let dir = TempDir::new(label);
    let mut storage = Storage::open(dir.db_path()).expect("open");
    storage.enable_change_log().expect("enable");
    for i in 0..rows {
        put(&storage, i + 1);
    }
    (dir, storage)
}

fn private_dir(label: &str) -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let d = TempDir::new(label);
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    d
}

fn archive_all(storage: &Storage, dir: &TempDir) {
    storage
        .archive_change_log(&ArchiveOptions::new(dir.path()))
        .expect("archive");
}

fn tables_present(storage: &Storage) -> (bool, bool) {
    use redb::ReadableDatabase as _;
    let read = storage.db.begin_read().expect("read");
    let log = !matches!(
        read.open_table(CHANGE_LOG_TABLE),
        Err(redb::TableError::TableDoesNotExist(_))
    );
    let meta = !matches!(
        read.open_table(META_TABLE),
        Err(redb::TableError::TableDoesNotExist(_))
    );
    (log, meta)
}

#[test]
fn disable_after_full_archive_removes_both_tables_and_keeps_generation() {
    let (dir, mut storage) = enabled("dis-ok", 3);
    let arc = private_dir("dis-ok-arc");
    archive_all(&storage, &arc);
    let generation = storage.current_generation().expect("gen");
    assert_eq!(tables_present(&storage), (true, true));

    storage
        .disable_change_log(&DisableOptions::default())
        .expect("disable");

    assert_eq!(tables_present(&storage), (false, false));
    assert_eq!(storage.current_generation().expect("gen"), generation);
    assert!(!storage.change_log_required());
    assert!(storage.change_log_identity().is_none());
    // 有効化せずに書き込める。
    put(&storage, 100);
    drop(storage);

    // 再オープンしても「一度も有効にしていない DB」として開く。
    let reopened = Storage::open(dir.db_path()).expect("reopen");
    assert!(!reopened.change_log_required());
    assert!(!crate::storage::change_log::identity_present(&reopened.db).expect("identity"));
    put(&reopened, 101);
}

#[test]
fn disable_is_refused_with_unarchived_records_and_writes_nothing() {
    let (_dir, mut storage) = enabled("dis-unarch", 3);
    let generation = storage.current_generation().expect("gen");

    let err = storage
        .disable_change_log(&DisableOptions::default())
        .expect_err("must refuse");
    assert!(matches!(err, DisableError::Unarchived));
    assert_eq!(
        err.to_string(),
        "change log has records that are not archived"
    );

    // 何も書かず、有効状態も変わらない。
    assert_eq!(tables_present(&storage), (true, true));
    assert_eq!(storage.current_generation().expect("gen"), generation);
    assert!(storage.change_log_identity().is_some());
    put(&storage, 50);
}

#[test]
fn disable_with_discard_drops_unarchived_records() {
    let (_dir, mut storage) = enabled("dis-discard", 3);
    storage
        .disable_change_log(&DisableOptions {
            discard_unarchived: true,
        })
        .expect("disable");
    assert_eq!(tables_present(&storage), (false, false));
    assert!(!storage.change_log_required());
    // 表のデータは失われない。
    assert!(storage.get("tenant-a", 1).is_ok());
}

#[test]
fn disable_right_after_enable_needs_no_archive() {
    let (_dir, mut storage) = enabled("dis-fresh", 0);
    storage
        .disable_change_log(&DisableOptions::default())
        .expect("disable");
    assert_eq!(tables_present(&storage), (false, false));
}

#[test]
fn disable_requires_an_enabled_database() {
    let dir = TempDir::new("dis-never");
    let mut storage = Storage::open(dir.db_path()).expect("open");
    let err = storage
        .disable_change_log(&DisableOptions::default())
        .expect_err("never enabled");
    assert!(matches!(err, DisableError::NotEnabled));
    // 失敗しても何も作らない。
    assert_eq!(tables_present(&storage), (false, false));
}

#[test]
fn disable_is_refused_for_temporary_storage() {
    let dir = TempDir::new("dis-temp");
    let mut storage = Storage::open(dir.db_path()).expect("open");
    storage.temp_scoped = true;
    let err = storage
        .disable_change_log(&DisableOptions::default())
        .expect_err("temp");
    assert!(matches!(err, DisableError::NotEnabled));
}

#[test]
fn disabled_database_passes_integrity_check() {
    let (_dir, mut storage) = enabled("dis-integrity", 2);
    storage
        .disable_change_log(&DisableOptions {
            discard_unarchived: true,
        })
        .expect("disable");
    let report = storage
        .verify_integrity(&IntegrityOptions::default())
        .expect("verify");
    assert!(report.is_consistent());
}

#[test]
fn incremental_and_chain_point_are_refused_after_disable() {
    let (dir, mut storage) = enabled("dis-incr", 2);
    let out = private_dir("dis-incr-out");
    let before = out.path().join("before.redb");
    storage.export_snapshot(&before).expect("snapshot");
    let base = chain_point_of(&before).expect("chain point");

    storage
        .disable_change_log(&DisableOptions {
            discard_unarchived: true,
        })
        .expect("disable");

    // 無効化前のベースからの増分は、出力元の識別子が無いため拒否される。
    let err = storage
        .export_incremental(&base, out.path().join("inc.seg"))
        .expect_err("incremental refused");
    assert!(matches!(err, crate::storage::IncrementalError::NotEnabled));
    assert!(!out.path().join("inc.seg").exists());

    // 無効化後のフルバックアップは識別子を持たず、連鎖の起点にならない。
    let after = out.path().join("after.redb");
    storage.export_snapshot(&after).expect("snapshot");
    assert!(matches!(
        chain_point_of(&after),
        Err(crate::storage::IncrementalError::NotEnabled)
    ));
    drop(dir);
}

#[test]
fn reenable_creates_a_new_identity_and_does_not_touch_old_segments() {
    let (dir, mut storage) = enabled("dis-reenable", 2);
    let old_id = storage.change_log_identity().expect("identity").0;
    let arc = private_dir("dis-reenable-arc");
    archive_all(&storage, &arc);
    let segments_before: Vec<_> = std::fs::read_dir(arc.path())
        .expect("dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(!segments_before.is_empty());

    storage
        .disable_change_log(&DisableOptions::default())
        .expect("disable");
    storage.enable_change_log().expect("re-enable");
    let (new_id, timeline) = storage.change_log_identity().expect("identity");
    assert_ne!(new_id, old_id);
    assert_eq!(timeline, INITIAL_TIMELINE);
    put(&storage, 77);

    // 旧 DB 識別子のセグメントを上書き・削除しない。
    archive_all(&storage, &arc);
    for name in &segments_before {
        assert!(arc.path().join(name).exists());
    }
    drop(dir);
}

#[test]
fn enabled_storage_with_live_state_can_be_disabled_and_stops_recording() {
    // `enable_change_log` 済み（`change_log` が `Some`）の Storage からでも無効化できる。
    let (_dir, mut storage) = enabled("dis-live", 1);
    assert!(storage.change_log_identity().is_some());
    storage
        .disable_change_log(&DisableOptions {
            discard_unarchived: true,
        })
        .expect("disable");
    put(&storage, 9);
    assert_eq!(tables_present(&storage), (false, false));
}
