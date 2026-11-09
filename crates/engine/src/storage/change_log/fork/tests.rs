//! 復元の完了時の timeline 分岐の試験（Issue #3100・RECOVER-14・RECOVER-17 のポインタ）。
//!
//! 分岐後の状態・fail-closed の拒否・変更ログを有効にしていない DB への無影響・分岐後の系統が
//! 自分自身の増分連鎖を保つことを固定する。元の系統との混在の拒否は incremental / pitr の試験が持つ。

use redb::ReadableDatabase as _;

use super::*;
use crate::storage::change_log::incremental::{apply_incremental_segments, chain_point_of};
use crate::storage::change_log::INITIAL_TIMELINE;
use crate::storage::{RowInput, Storage, Visibility};
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

/// 変更ログを有効にして `rows` 行を書き、閉じた DB のパスを返す。
fn enabled_db(label: &str, rows: u64) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new(label);
    let path = dir.db_path();
    {
        let mut s = Storage::open(&path).expect("open");
        s.enable_change_log().expect("enable");
        for i in 0..rows {
            put(&s, i + 1);
        }
    }
    (dir, path)
}

fn meta_u64(db: &redb::Database, key: &str) -> Option<u64> {
    let read = db.begin_read().expect("read");
    let t = read.open_table(META_TABLE).expect("meta");
    t.get(key)
        .expect("get")
        .map(|v| u64::from_le_bytes(v.value().try_into().expect("8 bytes")))
}

fn log_len(db: &redb::Database) -> usize {
    let read = db.begin_read().expect("read");
    match read.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => t.iter().expect("iter").count(),
        Err(redb::TableError::TableDoesNotExist(_)) => 0,
        Err(e) => panic!("{e}"),
    }
}

fn set_meta(path: &std::path::Path, key: &str, value: Option<&[u8]>) {
    let db = redb::Database::open(path).expect("open raw");
    let w = db.begin_write().expect("begin");
    {
        #[allow(clippy::disallowed_methods)]
        let mut t = w.open_table(META_TABLE).expect("meta");
        match value {
            Some(v) => {
                t.insert(key, v).expect("insert");
            }
            None => {
                t.remove(key).expect("remove");
            }
        }
    }
    w.commit().expect("commit");
}

#[test]
fn next_timeline_is_the_larger_of_the_generation_and_old_plus_one() {
    assert_eq!(next_timeline(1, 1).expect("floor 0 case"), 2);
    assert_eq!(next_timeline(1, 1_000).expect("clock"), 1_000);
    assert_eq!(next_timeline(5_000, 1_000).expect("monotonic"), 5_001);
    assert!(next_timeline(u64::MAX, 1).is_err());
}

#[test]
fn a_database_without_the_change_log_is_left_alone() {
    let dir = TempDir::new("fork-never");
    let mut s = Storage::open(dir.db_path()).expect("open");
    put(&s, 1);
    s.advance_restore_generation_with_floor(0).expect("advance");
    assert!(!s.change_log_required(), "no change log is forced on");
    drop(s);
    let s = Storage::open(dir.db_path()).expect("reopen");
    assert!(!s.change_log_required());
}

#[test]
fn advancing_forks_the_timeline_and_restarts_the_log_at_the_branch_point() {
    let (_d, path) = enabled_db("fork-state", 3);
    let before = chain_point_of(&path).expect("point");
    assert_eq!(before.timeline, INITIAL_TIMELINE);
    {
        let mut s = Storage::open(&path).expect("open");
        assert_eq!(s.advance_restore_generation_with_floor(0).expect("adv"), 1);
        assert!(s.change_log_required());
    }
    let after = chain_point_of(&path).expect("point");
    assert_eq!(after.db_id, before.db_id, "the DB id is kept");
    assert_eq!(after.end_lsn, before.end_lsn, "the LSN is kept");
    assert_eq!(after.timeline, 2, "floor 0: old + 1");
    let db = redb::Database::open(&path).expect("raw");
    assert_eq!(meta_u64(&db, META_KEY_TIMELINE), Some(2));
    assert_eq!(
        meta_u64(&db, META_KEY_BASE_GENERATION),
        Some(before.end_lsn)
    );
    assert_eq!(meta_u64(&db, META_KEY_ARCHIVED_LSN), Some(before.end_lsn));
    assert_eq!(log_len(&db), 0);
    drop(db);

    // 再有効化の連続性検査が通り、新しいレコードは新しい timeline を持つ。
    let mut s = Storage::open(&path).expect("reopen");
    s.enable_change_log().expect("continuity");
    assert_eq!(s.change_log_identity().map(|(_, t)| t), Some(2));
    put(&s, 100);
    drop(s);
    assert_eq!(chain_point_of(&path).expect("point").timeline, 2);
}

#[test]
fn a_clock_based_generation_becomes_the_timeline() {
    let (_d, path) = enabled_db("fork-clock", 1);
    let mut s = Storage::open(&path).expect("open");
    s.advance_restore_generation_with_floor(1_000).expect("adv");
    drop(s);
    assert_eq!(chain_point_of(&path).expect("point").timeline, 1_000);
}

#[test]
fn repeated_restores_increase_the_timeline_strictly() {
    let (_d, path) = enabled_db("fork-twice", 2);
    let mut last = INITIAL_TIMELINE;
    for _ in 0..3 {
        let mut s = Storage::open(&path).expect("open");
        s.advance_restore_generation_with_floor(0).expect("adv");
        drop(s);
        let t = chain_point_of(&path).expect("point").timeline;
        assert!(t > last, "{t} must exceed {last}");
        last = t;
    }
}

#[test]
fn a_discontinuous_log_is_refused_and_nothing_is_written() {
    let (_d, path) = enabled_db("fork-gap", 3);
    // 起点を過去へずらして「記録の欠け」を作る（最後のレコードを消して連続性を壊す）。
    {
        let db = redb::Database::open(&path).expect("raw");
        let w = db.begin_write().expect("begin");
        {
            #[allow(clippy::disallowed_methods)]
            let mut t = w.open_table(CHANGE_LOG_TABLE).expect("log");
            t.pop_last().expect("pop");
        }
        w.commit().expect("commit");
    }
    let mut s = Storage::open(&path).expect("open");
    let err = s.advance_restore_generation_with_floor(0).expect_err("gap");
    assert!(err.to_string().contains("continuity"), "{err}");
    assert_eq!(s.restore_generation().expect("gen"), 0, "nothing written");
    drop(s);
    let db = redb::Database::open(&path).expect("raw");
    assert_eq!(meta_u64(&db, META_KEY_TIMELINE), Some(INITIAL_TIMELINE));
}

#[test]
fn corrupt_or_missing_metadata_is_refused_and_nothing_is_written() {
    type Corrupt = fn(&std::path::Path);
    let cases: [(&str, Corrupt); 3] = [
        ("timeline-missing", |p| set_meta(p, META_KEY_TIMELINE, None)),
        ("timeline-short", |p| {
            set_meta(p, META_KEY_TIMELINE, Some(&[1, 2, 3]))
        }),
        ("base-missing", |p| {
            set_meta(p, META_KEY_BASE_GENERATION, None)
        }),
    ];
    for (label, corrupt) in cases {
        let (_d, path) = enabled_db(&format!("fork-{label}"), 2);
        corrupt(&path);
        let mut s = Storage::open(&path).expect("open");
        assert!(
            s.advance_restore_generation_with_floor(0).is_err(),
            "{label}"
        );
        assert_eq!(s.restore_generation().expect("gen"), 0, "{label}");
    }
}

#[test]
fn an_overflowing_timeline_is_refused() {
    let (_d, path) = enabled_db("fork-overflow", 1);
    set_meta(&path, META_KEY_TIMELINE, Some(&u64::MAX.to_le_bytes()));
    let mut s = Storage::open(&path).expect("open");
    assert!(s.advance_restore_generation_with_floor(0).is_err());
    assert_eq!(s.restore_generation().expect("gen"), 0);
}

#[test]
fn a_forked_lineage_keeps_its_own_incremental_chain() {
    let (_d, path) = enabled_db("fork-chain", 2);
    {
        let mut s = Storage::open(&path).expect("open");
        s.advance_restore_generation_with_floor(0).expect("adv");
    }
    // 復元した DB を稼働させ、自分のフルバックアップ（複製）と増分を取る。
    let mut s = Storage::open(&path).expect("reopen");
    s.enable_change_log().expect("enable");
    // 書き出し先は他者が書けない権限が要る（umask に依らず 0755 にする）。
    let out = TempDir::new("fork-chain-out");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(out.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    let base_copy = out.path().join("base.redb");
    s.export_snapshot(&base_copy).expect("snapshot");
    let base_point = chain_point_of(&base_copy).expect("point");
    assert_eq!(base_point.timeline, 2);
    put(&s, 10);
    put(&s, 11);
    let inc = out.path().join("inc.seg");
    s.export_incremental(&base_point, &inc).expect("export");
    drop(s);
    let sum = apply_incremental_segments(&base_copy, &[inc]).expect("apply");
    assert_eq!(sum.last_lsn, base_point.end_lsn + 2);
}
