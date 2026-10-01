//! テスト一時領域ガードの回帰テスト（Issue #1303）。
//!
//! `tests/common/mod.rs` の `UserStoreFile`／`TempFixtureDir` と、`engine` の
//! `temp_db::unlink_open_db_file` が、正常終了・panic（unwinding）のいずれでも
//! 一時ディレクトリ／ファイルを残さないことを確認する。多数の結合テストが
//! これらのガードに依存しているため、ここが壊れると inode 枯渇（ENOSPC）が再発する。
//! 全体の残置検出は `scripts/check_tmp_leak.sh`（`make tmp-leak-check`）が担う。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

fn fixture_dir_of(store: &common::UserStoreFile) -> PathBuf {
    store
        .parent()
        .expect("user store file has a parent dir")
        .to_path_buf()
}

#[test]
fn user_store_file_is_removed_on_normal_drop() {
    let store = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let dir = fixture_dir_of(&store);
    assert!(store.exists());
    drop(store);
    assert!(!dir.exists(), "fixture dir must be removed on drop");
}

#[test]
fn user_store_file_is_removed_when_test_panics() {
    let mut dir_slot: Option<PathBuf> = None;
    let result = catch_unwind(AssertUnwindSafe(|| {
        let store = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
        dir_slot = Some(fixture_dir_of(&store));
        panic!("intentional panic to exercise unwinding");
    }));
    assert!(result.is_err());
    let dir = dir_slot.expect("dir recorded before panic");
    assert!(!dir.exists(), "fixture dir must be removed on unwinding");
}

#[test]
fn temp_fixture_dir_is_removed_on_drop_even_with_files_inside() {
    let fixture = common::TempFixtureDir::new("tmp-fixture-cleanup");
    let users = PathBuf::from(fixture.users_path_str());
    let dir = users.parent().expect("parent").to_path_buf();
    common::write_empty_user_store(&fixture.users_path_str());
    assert!(users.exists());
    drop(fixture);
    assert!(!dir.exists());
}

#[test]
fn remove_dir_all_logged_tolerates_missing_dir() {
    // 削除失敗（NotFound）はテストを失敗させない（panic しない）契約。
    let missing = std::env::temp_dir().join("wire-server-tmp-fixture-cleanup-missing-dir");
    common::remove_dir_all_logged(&missing);
}

#[cfg(unix)]
#[test]
fn unlink_open_db_file_removes_file_while_core_stays_usable() {
    let path = temp_db::unique_db_path("tmp-fixture-cleanup");
    let core = engine::core::EngineCore::open(&path).expect("open engine core");
    temp_db::unlink_open_db_file(&path);
    assert!(!path.exists(), "db file must be unlinked right after open");
    drop(core);
    assert!(!path.exists());
}
