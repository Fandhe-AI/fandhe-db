//! レプリカの昇格の試験（Issue #3132・RECOVER-15 (4)(5) のポインタ）。
//!
//! 成功時の報告（適用済みの LSN・分岐後の timeline）、昇格後にプライマリとして有効化できて LSN が続くこと、
//! 拒否（レプリカのベースでない・連続しない・timeline の溢れ）が何も書かないこと、`restore_generation` を
//! 動かさないこと、旧系統のレコードが昇格後の DB へ入らないことを固定する。

use super::*;
use crate::storage::change_log::incremental::chain_point_of;
use crate::storage::change_log::{
    CHANGE_LOG_TABLE, INITIAL_TIMELINE, META_KEY_ARCHIVED_LSN, META_KEY_BASE_GENERATION,
    META_KEY_TIMELINE, META_TABLE,
};
use crate::storage::{ReplicaApplyError, RowInput, Visibility};
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

/// 変更ログを有効にして `rows` 行を書き、閉じた DB のパスを返す（変更ログなしで開き直すとレプリカのベース）。
fn base_db(label: &str, rows: u64) -> (TempDir, std::path::PathBuf) {
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

fn meta_u64(path: &std::path::Path, key: &str) -> Option<u64> {
    let db = redb::Database::open(path).expect("raw");
    let read = db.begin_read().expect("read");
    let t = read.open_table(META_TABLE).expect("meta");
    t.get(key)
        .expect("get")
        .map(|v| u64::from_le_bytes(v.value().try_into().expect("8 bytes")))
}

fn set_meta(path: &std::path::Path, key: &str, value: &[u8]) {
    let db = redb::Database::open(path).expect("raw");
    let w = db.begin_write().expect("begin");
    {
        #[allow(clippy::disallowed_methods)]
        let mut t = w.open_table(META_TABLE).expect("meta");
        t.insert(key, value).expect("insert");
    }
    w.commit().expect("commit");
}

fn log_len(path: &std::path::Path) -> usize {
    use redb::ReadableTable as _;
    let db = redb::Database::open(path).expect("raw");
    let read = db.begin_read().expect("read");
    let t = read.open_table(CHANGE_LOG_TABLE).expect("log");
    t.iter().expect("iter").count()
}

#[test]
fn promotion_reports_the_applied_lsn_and_forks_the_timeline() {
    let (_d, path) = base_db("promote-ok", 3);
    let before = chain_point_of(&path).expect("point");
    let mut s = Storage::open(&path).expect("open");
    let report = s.promote_replica_with_floor(0).expect("promote");
    assert_eq!(report.applied_lsn, before.end_lsn);
    assert_eq!(report.timeline, INITIAL_TIMELINE + 1, "floor 0: old + 1");
    drop(s);
    let after = chain_point_of(&path).expect("point");
    assert_eq!(after.db_id, before.db_id, "the DB id is kept");
    assert_eq!(after.end_lsn, before.end_lsn);
    assert_eq!(after.timeline, report.timeline);
    assert_eq!(
        meta_u64(&path, META_KEY_BASE_GENERATION),
        Some(before.end_lsn)
    );
    assert_eq!(meta_u64(&path, META_KEY_ARCHIVED_LSN), Some(before.end_lsn));
    assert_eq!(log_len(&path), 0);
}

#[test]
fn a_clock_floor_becomes_the_timeline() {
    let (_d, path) = base_db("promote-floor", 1);
    let mut s = Storage::open(&path).expect("open");
    let report = s.promote_replica_with_floor(5_000).expect("promote");
    assert_eq!(report.timeline, 5_000);
}

#[test]
fn the_promoted_database_starts_as_a_primary_and_the_lsn_continues() {
    let (_d, path) = base_db("promote-primary", 2);
    let applied = {
        let mut s = Storage::open(&path).expect("open");
        s.promote_replica_with_floor(0)
            .expect("promote")
            .applied_lsn
    };
    let mut s = Storage::open(&path).expect("reopen");
    s.enable_change_log().expect("continuity holds");
    put(&s, 100);
    assert_eq!(s.current_generation().expect("generation"), applied + 1);
    assert_eq!(
        s.change_log_stream_identity().map(|i| i.timeline),
        Some(INITIAL_TIMELINE + 1)
    );
}

#[test]
fn promotion_does_not_advance_the_restore_generation() {
    let (_d, path) = base_db("promote-restore-gen", 2);
    let mut s = Storage::open(&path).expect("open");
    let before = s.restore_generation().expect("gen");
    s.promote_replica_with_floor(7).expect("promote");
    assert_eq!(s.restore_generation().expect("gen"), before);
}

#[test]
fn promoting_two_copies_with_different_floors_gives_different_timelines() {
    let (_d, path) = base_db("promote-twice", 2);
    let copy = path.with_file_name("copy.redb");
    std::fs::copy(&path, &copy).expect("copy");
    let a = Storage::open(&path)
        .expect("open")
        .promote_replica_with_floor(1_000)
        .expect("a");
    let b = Storage::open(&copy)
        .expect("open")
        .promote_replica_with_floor(2_000)
        .expect("b");
    assert_ne!(a.timeline, b.timeline);
    assert_eq!(a.applied_lsn, b.applied_lsn);
}

#[test]
fn a_database_that_never_enabled_the_change_log_is_refused_untouched() {
    let dir = TempDir::new("promote-plain");
    let path = dir.db_path();
    drop(Storage::open(&path).expect("open"));
    let mut s = Storage::open(&path).expect("open");
    assert!(matches!(
        s.promote_replica_with_floor(0),
        Err(PromoteError::NotReplicaBase)
    ));
    drop(s);
    // メタ表を作って DB を「変更ログ必須」に変えていない。
    assert!(matches!(
        chain_point_of(&path),
        Err(IncrementalError::NotEnabled)
    ));
}

#[test]
fn a_storage_opened_with_the_change_log_is_refused() {
    let (_d, path) = base_db("promote-primary-refused", 2);
    let before = chain_point_of(&path).expect("point");
    let mut s = Storage::open(&path).expect("open");
    s.enable_change_log().expect("enable");
    assert!(matches!(
        s.promote_replica_with_floor(0),
        Err(PromoteError::NotReplicaBase)
    ));
    drop(s);
    assert_eq!(chain_point_of(&path).expect("point"), before);
}

#[test]
fn a_discontinuous_log_is_refused_and_nothing_is_written() {
    let (_d, path) = base_db("promote-gap", 3);
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
    let len = log_len(&path);
    let mut s = Storage::open(&path).expect("open");
    assert!(matches!(
        s.promote_replica_with_floor(0),
        Err(PromoteError::BaseDiscontinuous)
    ));
    drop(s);
    assert_eq!(meta_u64(&path, META_KEY_TIMELINE), Some(INITIAL_TIMELINE));
    assert_eq!(log_len(&path), len);
}

#[test]
fn an_overflowing_timeline_is_refused_and_nothing_is_written() {
    let (_d, path) = base_db("promote-overflow", 1);
    set_meta(&path, META_KEY_TIMELINE, &u64::MAX.to_le_bytes());
    let len = log_len(&path);
    let mut s = Storage::open(&path).expect("open");
    assert!(matches!(
        s.promote_replica_with_floor(0),
        Err(PromoteError::TimelineLimit)
    ));
    drop(s);
    assert_eq!(meta_u64(&path, META_KEY_TIMELINE), Some(u64::MAX));
    assert_eq!(log_len(&path), len);
}

#[test]
fn records_of_the_former_lineage_are_refused_after_promotion() {
    let (_d, primary_path) = base_db("promote-lineage", 2);
    let replica_dir = TempDir::new("promote-lineage-replica");
    let replica_path = replica_dir.db_path();
    std::fs::copy(&primary_path, &replica_path).expect("copy");
    // 旧プライマリが昇格後もコミットを続け、そのレコードを昇格した DB へ渡す。
    let mut primary = Storage::open(&primary_path).expect("open primary");
    primary.enable_change_log().expect("enable");
    let base = primary.current_generation().expect("generation");
    put(&primary, 50);
    let records = primary
        .read_change_log_from(base + 1, 100, usize::MAX)
        .expect("read")
        .records;
    assert!(!records.is_empty());

    Storage::open(&replica_path)
        .expect("open")
        .promote_replica_with_floor(0)
        .expect("promote");
    // 変更ログなしで開くと別系統として拒否される。
    let plain = Storage::open(&replica_path).expect("open");
    assert!(matches!(
        plain.apply_replicated_records(&records),
        Err(ReplicaApplyError::Foreign)
    ));
    drop(plain);
    // 有効にして開いた（プライマリとしての）DB はレプリカのベースではない。
    let mut as_primary = Storage::open(&replica_path).expect("open");
    as_primary.enable_change_log().expect("enable");
    assert!(matches!(
        as_primary.apply_replicated_records(&records),
        Err(ReplicaApplyError::NotReplicaBase)
    ));
}
