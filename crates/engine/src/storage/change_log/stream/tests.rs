//! 変更ログのストリーミング読み出しと保持位置の試験（Issue #2173・RECOVER-15 のポインタ）。
//!
//! 開始位置からの連続した取り出し、上限、追いついた状態、刈り込み済み・先走りの拒否、レプリカの保持位置が
//! 刈り込みを止め、切断（`Drop`）で解けることを固定する。

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::{ArchiveOptions, Visibility};
use crate::test_util::temp_db::TempDir;

const TENANT: &str = "tenant-a";

struct Fixture {
    _db_dir: TempDir,
    archive: TempDir,
    core: EngineCore,
}

impl Fixture {
    fn new(label: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let db_dir = TempDir::new(label);
        let archive = TempDir::new(&format!("{label}-arc"));
        std::fs::set_permissions(archive.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let mut storage = Storage::open(db_dir.db_path()).expect("open");
        storage.enable_change_log().expect("enable");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let f = Self {
            _db_dir: db_dir,
            archive,
            core,
        };
        f.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        f
    }

    fn storage(&self) -> &Storage {
        self.core.storage_for_test()
    }

    fn sql(&self, sql: &str) {
        let ctx =
            PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
                .expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        self.core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {:?}", e.wire_code()));
    }

    fn insert(&self, n: u32) {
        for i in 0..n {
            self.sql(&format!(
                "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
            ));
        }
    }

    /// 全レコードを書き出して水位を進め、新しい側 `retain` 件を残して刈り込む。
    fn archive_and_prune(&self, retain: u64) {
        let mut opts = ArchiveOptions::new(self.archive.path());
        opts.retain_records = Some(retain);
        self.storage().archive_change_log(&opts).expect("archive");
    }

    fn current(&self) -> u64 {
        self.storage().current_generation().expect("generation")
    }
}

#[test]
fn reads_contiguous_records_from_start() {
    let f = Fixture::new("stream-read");
    f.insert(4);
    let cur = f.current();
    let first = cur - 4; // CREATE の LSN
    let b = f
        .storage()
        .read_change_log_from(first, 100, usize::MAX)
        .expect("read");
    assert_eq!(b.current_lsn, cur);
    let lsns: Vec<u64> = b.records.iter().map(|(l, _)| *l).collect();
    assert_eq!(lsns, (first..=cur).collect::<Vec<_>>());
}

#[test]
fn limits_apply_but_one_record_is_always_returned() {
    let f = Fixture::new("stream-limit");
    f.insert(3);
    let first = f.current() - 3;
    let b = f
        .storage()
        .read_change_log_from(first, 2, usize::MAX)
        .unwrap();
    assert_eq!(b.records.len(), 2);
    let b = f.storage().read_change_log_from(first, 100, 1).unwrap();
    assert_eq!(b.records.len(), 1);
    let b = f.storage().read_change_log_from(first, 0, 1).unwrap();
    assert!(b.records.is_empty());
}

#[test]
fn caught_up_is_empty_and_ahead_is_rejected() {
    let f = Fixture::new("stream-ahead");
    f.insert(1);
    let cur = f.current();
    let b = f
        .storage()
        .read_change_log_from(cur + 1, 10, 1 << 20)
        .unwrap();
    assert!(b.records.is_empty());
    assert_eq!(b.current_lsn, cur);
    assert!(matches!(
        f.storage().read_change_log_from(cur + 2, 10, 1 << 20),
        Err(StreamError::Ahead)
    ));
}

#[test]
fn pruned_start_is_rejected() {
    let f = Fixture::new("stream-pruned");
    f.insert(5);
    let cur = f.current();
    f.archive_and_prune(0);
    assert!(matches!(
        f.storage().read_change_log_from(cur - 3, 10, 1 << 20),
        Err(StreamError::Pruned)
    ));
    assert!(matches!(
        f.storage().pin_change_log_for_replica(cur - 4),
        Err(StreamError::Pruned)
    ));
    assert!(matches!(
        f.storage().read_change_log_from(0, 10, 1 << 20),
        Err(StreamError::Pruned)
    ));
    // 追いついた位置からの開始は刈り込み後でも受け付ける。
    assert!(f.storage().pin_change_log_for_replica(cur).is_ok());
}

#[test]
fn pin_holds_records_until_dropped() {
    let f = Fixture::new("stream-pin");
    f.insert(5);
    let cur = f.current();
    let applied = cur - 3;
    let pin = f
        .storage()
        .pin_change_log_for_replica(applied)
        .expect("pin");
    f.archive_and_prune(0);
    // 確認応答済みの位置（applied）より後は残る。
    let b = f
        .storage()
        .read_change_log_from(applied + 1, 100, usize::MAX)
        .expect("kept");
    assert_eq!(b.records.len(), 3);
    pin.advance(cur - 1);
    f.archive_and_prune(0);
    assert!(f
        .storage()
        .read_change_log_from(cur, 100, usize::MAX)
        .is_ok());
    assert!(matches!(
        f.storage().read_change_log_from(cur - 1, 100, usize::MAX),
        Err(StreamError::Pruned)
    ));
    // advance は後退しない。
    pin.advance(applied);
    drop(pin);
    f.archive_and_prune(0);
    assert!(matches!(
        f.storage().read_change_log_from(cur, 100, usize::MAX),
        Err(StreamError::Pruned)
    ));
}

#[test]
fn identity_is_exposed_only_when_enabled() {
    let f = Fixture::new("stream-id");
    assert!(f.storage().change_log_stream_identity().is_some());
    let d = TempDir::new("stream-id-off");
    let s = Storage::open(d.db_path()).expect("open");
    assert!(s.change_log_stream_identity().is_none());
    assert!(matches!(
        s.read_change_log_from(1, 1, 1),
        Err(StreamError::NotEnabled)
    ));
}
