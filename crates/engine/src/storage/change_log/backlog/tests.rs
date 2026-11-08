//! 未アーカイブの変更ログ量の上限の試験（RECOVER-14 (2)・Issue #3091）。
//!
//! 上限超過で書き込みが `54000` で拒否され（世代・レコードが確定しない）、アーカイブで水位が進めば
//! 再開すること、バイトの数え値が数え直しと一致すること、不整合な数え値・再オープンでも正しい値で
//! 判定されること、上限なしではメタ表に新しいキーを作らないことを固定する。`cfg(test)` では
//! 全 commit がオラクルで検証されるため、再開後の LSN の連鎖も間接的に確かめられる。

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::change_log::archive::ArchiveOptions;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::TempDir;
use redb::ReadableDatabase as _;

const TENANT: &str = "tenant-a";

struct Fixture {
    db_dir: TempDir,
    archive: TempDir,
    core: EngineCore,
}

fn open_core(db_dir: &TempDir, limit: UnarchivedLimit) -> EngineCore {
    let mut storage = Storage::open(db_dir.db_path()).expect("open");
    storage
        .enable_change_log_with_unarchived_limit(limit)
        .expect("enable");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

impl Fixture {
    fn new(label: &str, limit: UnarchivedLimit) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let db_dir = TempDir::new(label);
        let archive = TempDir::new(&format!("{label}-arc"));
        std::fs::set_permissions(archive.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let core = open_core(&db_dir, limit);
        Self {
            db_dir,
            archive,
            core,
        }
    }

    fn reopen(self, limit: UnarchivedLimit) -> Self {
        let Self {
            db_dir,
            archive,
            core,
        } = self;
        drop(core);
        let core = open_core(&db_dir, limit);
        Self {
            db_dir,
            archive,
            core,
        }
    }

    fn storage(&self) -> &Storage {
        self.core.storage_for_test()
    }

    /// 失敗したときは `wire_code` を返す。
    fn try_sql(&self, sql: &str) -> std::result::Result<(), String> {
        let ctx =
            PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
                .expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        self.core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .map(|_| ())
            .map_err(|e| e.wire_code().to_string())
    }

    fn sql(&self, sql: &str) {
        self.try_sql(sql).unwrap_or_else(|c| panic!("{sql}: {c}"));
    }

    fn insert(&self, id: u32) -> std::result::Result<(), String> {
        self.try_sql(&format!(
            "INSERT INTO ev (id, embedding) VALUES ({id}, '[0.1,0.2]')"
        ))
    }

    fn create(&self) {
        self.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
    }

    fn archive(&self) {
        let mut opts = ArchiveOptions::new(self.archive.path());
        opts.segment_bytes = 1;
        let report = self.storage().archive_change_log(&opts).expect("archive");
        assert!(!report.skipped);
    }

    fn generation(&self) -> u64 {
        self.storage().current_generation().expect("generation")
    }

    fn row_count(&self) -> usize {
        let read = self.storage().db.begin_read().expect("read");
        match read.open_table(CHANGE_LOG_TABLE) {
            Ok(t) => t.iter().expect("iter").count(),
            Err(_) => 0,
        }
    }

    /// 水位より後の行の値の長さの合計（数え直し）。
    fn recount(&self) -> u64 {
        let read = self.storage().db.begin_read().expect("read");
        let meta = read.open_table(META_TABLE).expect("meta");
        let w = watermark(&meta).expect("w");
        let log = read.open_table(CHANGE_LOG_TABLE).expect("log");
        let last = log.last().expect("last").map_or(w, |(k, _)| k.value());
        sum_bytes(&log, w, last).expect("sum")
    }

    fn meta_has(&self, key: &str) -> bool {
        let read = self.storage().db.begin_read().expect("read");
        let meta = read.open_table(META_TABLE).expect("meta");
        meta.get(key).expect("get").is_some()
    }

    fn drop_meta(&self, key: &str) {
        let txn = self.storage().db.begin_write().expect("write");
        {
            #[allow(clippy::disallowed_methods)]
            let mut meta = txn.open_table(META_TABLE).expect("meta");
            meta.remove(key).expect("remove");
        }
        txn.commit().expect("commit");
    }

    fn counter(&self) -> Option<u64> {
        let read = self.storage().db.begin_read().expect("read");
        let meta = read.open_table(META_TABLE).expect("meta");
        read_u64(&meta, META_KEY_UNARCHIVED_BYTES).expect("read")
    }
}

fn records(n: u64) -> UnarchivedLimit {
    UnarchivedLimit {
        max_records: Some(n),
        max_bytes: None,
    }
}

fn bytes(n: u64) -> UnarchivedLimit {
    UnarchivedLimit {
        max_records: None,
        max_bytes: Some(n),
    }
}

#[test]
fn recover14_records_limit_refuses_then_resumes_after_archive() {
    let f = Fixture::new("bl-records", records(5));
    f.create();
    for i in 0..4 {
        f.insert(i).expect("under the limit");
    }
    assert_eq!(
        f.storage()
            .change_log_unarchived_status()
            .unwrap()
            .unwrap()
            .records,
        5
    );
    let generation = f.generation();
    let rows = f.row_count();
    // 上限に達したので拒否される。世代・レコードは変わらない（AC2）。
    let err = f.insert(100).expect_err("refused at the limit");
    assert_eq!(err, "54000");
    assert_eq!(f.generation(), generation);
    assert_eq!(f.row_count(), rows);
    assert!(
        f.storage()
            .change_log_unarchived_status()
            .unwrap()
            .unwrap()
            .at_limit
    );
    // アーカイブで水位が進めば再開する（AC1）。
    f.archive();
    assert!(
        !f.storage()
            .change_log_unarchived_status()
            .unwrap()
            .unwrap()
            .at_limit
    );
    f.insert(100).expect("resumes");
    f.storage()
        .verify_change_log_archive(f.archive.path())
        .expect("archive verifies");
}

#[test]
fn bytes_limit_allows_one_oversized_record_then_refuses_and_resumes() {
    let f = Fixture::new("bl-bytes", bytes(1));
    f.create(); // 空の backlog なら上限より大きい 1 レコードも通る
    f.insert(1).expect_err("the next one is refused");
    f.archive();
    f.insert(1).expect("resumes after the archive");
}

#[test]
fn byte_counter_matches_a_full_recount() {
    let f = Fixture::new("bl-counter", bytes(u64::MAX));
    f.create();
    for i in 0..6 {
        f.insert(i).expect("insert");
    }
    assert_eq!(f.counter(), Some(f.recount()));
    f.archive(); // 1 レコードごとのセグメント（Write）
    assert_eq!(f.counter(), Some(0));
    for i in 10..13 {
        f.insert(i).expect("insert");
    }
    assert_eq!(f.counter(), Some(f.recount()));
    // 同じ区間のセグメントが既にある（Adopt）周期でも合計が合う。
    f.archive();
    assert_eq!(f.counter(), Some(f.recount()));
    assert_eq!(f.counter(), Some(0));
}

#[test]
fn stale_or_missing_stamp_is_recounted() {
    let f = Fixture::new("bl-stale", bytes(1));
    f.create();
    f.archive();
    f.insert(1).expect("first goes through an empty backlog");
    // 数え値を失っても、数え直して正しく拒否する。
    f.drop_meta(META_KEY_UNARCHIVED_BYTES_LSN);
    f.insert(2).expect_err("recounted and refused");
    f.drop_meta(META_KEY_UNARCHIVED_BYTES);
    f.insert(2).expect_err("recounted and refused");
    f.archive();
    f.insert(2).expect("resumes");
    assert_eq!(f.counter(), Some(f.recount()));
}

#[test]
fn reopen_recounts_the_backlog_and_applies_the_limit() {
    let f = Fixture::new("bl-reopen", UnarchivedLimit::default());
    f.create();
    for i in 0..3 {
        f.insert(i).expect("insert");
    }
    assert!(
        !f.meta_has(META_KEY_UNARCHIVED_BYTES),
        "no limit, no counter"
    );
    let f = f.reopen(bytes(1));
    assert_eq!(f.counter(), Some(f.recount()));
    f.insert(9).expect_err("existing backlog is over the limit");
    let f = f.reopen(records(2));
    f.insert(9).expect_err("records over the limit");
    f.archive();
    f.insert(9).expect("resumes");
}

#[test]
fn archived_but_retained_rows_are_not_counted() {
    let f = Fixture::new("bl-retained", records(3));
    f.create();
    f.insert(1).expect("insert");
    f.archive(); // 刈り込まずに残す
    assert!(f.row_count() >= 2);
    f.insert(2).expect("archived rows are not counted");
    f.insert(3).expect("archived rows are not counted");
    f.insert(4)
        .expect("two unarchived rows are under the limit");
    f.insert(5)
        .expect_err("three unarchived rows reach the limit");
}

#[test]
fn archive_at_the_limit_is_not_skipped_and_advances_the_watermark() {
    let f = Fixture::new("bl-gate", records(2));
    f.create();
    f.insert(1).expect("insert");
    f.insert(2).expect_err("refused");
    let mut opts = ArchiveOptions::new(f.archive.path());
    opts.segment_bytes = 1;
    let report = f.storage().archive_change_log(&opts).expect("archive");
    assert!(!report.skipped);
    assert_eq!(report.archived_through, f.generation());
}

#[test]
fn enable_with_a_backlog_over_the_limit_still_succeeds() {
    let f = Fixture::new("bl-over", UnarchivedLimit::default());
    f.create();
    for i in 0..4 {
        f.insert(i).expect("insert");
    }
    let f = f.reopen(records(1));
    assert!(
        f.storage()
            .change_log_unarchived_status()
            .unwrap()
            .unwrap()
            .at_limit
    );
}

#[test]
fn no_limit_touches_no_meta_keys() {
    let f = Fixture::new("bl-none", UnarchivedLimit::default());
    f.create();
    f.insert(1).expect("insert");
    f.archive();
    assert!(!f.meta_has(META_KEY_UNARCHIVED_BYTES));
    assert!(!f.meta_has(META_KEY_UNARCHIVED_BYTES_LSN));
    let status = f.storage().change_log_unarchived_status().unwrap().unwrap();
    assert!(!status.at_limit);
    assert_eq!(status.bytes, None);
    let _ = &f.db_dir;
}

#[test]
fn zero_limits_are_rejected_and_the_message_is_fixed() {
    let dir = TempDir::new("bl-zero");
    for limit in [records(0), bytes(0)] {
        let mut storage = Storage::open(dir.db_path()).expect("open");
        assert!(storage
            .enable_change_log_with_unarchived_limit(limit)
            .is_err());
    }
    let text = Unrecordable::UnarchivedLimit.text();
    assert!(!text.chars().any(|c| c.is_ascii_digit()));
}
