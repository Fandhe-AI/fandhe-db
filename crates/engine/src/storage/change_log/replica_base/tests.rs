//! レプリカのベースの連続性検査の試験（Issue #2176・RECOVER-15 のポインタ）。
//!
//! `export_snapshot` で作ったベースが切り替え点（終点 LSN = 世代）を返すこと、記録のない区間・変更ログ未有効・
//! 書き込み中の DB が拒否されること、検査が DB を書き換えないこと、`verify_continuity` と判定が一致すること、
//! 適用後も連続が保たれることを固定する。

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::change_log::incremental::chain_point_of;
use crate::storage::change_log::{verify_continuity, META_TABLE};
use crate::storage::{Visibility, GENERATION_KEY, GENERATION_TABLE};
use crate::test_util::temp_db::TempDir;

struct Fixture {
    db_dir: TempDir,
    out: TempDir,
    core: EngineCore,
}

impl Fixture {
    fn new(label: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let db_dir = TempDir::new(label);
        let out = TempDir::new(&format!("{label}-out"));
        std::fs::set_permissions(out.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let mut storage = Storage::open(db_dir.db_path()).expect("open");
        storage.enable_change_log().expect("enable");
        Self {
            db_dir,
            out,
            core: EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        }
    }

    fn storage(&self) -> &Storage {
        self.core.storage_for_test()
    }

    fn sql(&self, sql: &str) {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        self.core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {:?}", e.wire_code()));
    }

    fn write_rows(&self) {
        self.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        for i in 0..3 {
            self.sql(&format!(
                "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
            ));
        }
    }

    fn base(&self, name: &str) -> std::path::PathBuf {
        let p = self.out.path().join(name);
        self.storage().export_snapshot(&p).expect("snapshot");
        p
    }
}

/// 世代表を `generation` に書き換える（記録のない区間を作る細工）。
fn set_generation(path: &std::path::Path, generation: u64) {
    let db = redb::Database::open(path).expect("open");
    let txn = db.begin_write().expect("write");
    {
        #[allow(clippy::disallowed_methods)]
        let mut g = txn.open_table(GENERATION_TABLE).expect("generation");
        g.insert(GENERATION_KEY, generation).expect("insert");
    }
    txn.commit().expect("commit");
}

#[test]
fn a_full_backup_yields_the_switch_point() {
    let f = Fixture::new("rb-ok");
    f.write_rows();
    let p = f.base("b.redb");
    let point = replica_base_point_of(&p).expect("point");
    assert_eq!(point.end_lsn, f.storage().current_generation().unwrap());
    assert_eq!(point, chain_point_of(&p).expect("chain point"));
}

#[test]
fn the_check_does_not_modify_the_file() {
    let f = Fixture::new("rb-ro");
    f.write_rows();
    let p = f.base("b.redb");
    let before = std::fs::read(&p).expect("read");
    replica_base_point_of(&p).expect("point");
    assert_eq!(std::fs::read(&p).expect("read"), before);
}

#[test]
fn a_gap_between_the_log_and_the_generation_is_rejected() {
    let f = Fixture::new("rb-gap");
    f.write_rows();
    let p = f.base("b.redb");
    set_generation(&p, f.storage().current_generation().unwrap() + 5);
    assert!(matches!(
        replica_base_point_of(&p),
        Err(IncrementalError::ChainBroken)
    ));
    // 変更ログの連続性検査（verify_continuity）と判定が一致する。
    let db = redb::Database::open(&p).expect("open");
    let generation = {
        let read = db.begin_read().expect("read");
        crate::storage::current_generation_in_txn(&read).expect("generation")
    };
    assert!(verify_continuity(&db, generation).is_err());
}

#[test]
fn an_empty_log_table_uses_the_archive_watermark() {
    let f = Fixture::new("rb-prune");
    f.write_rows();
    let p = f.base("b.redb");
    let generation = f.storage().current_generation().unwrap();
    {
        let db = redb::Database::open(&p).expect("open");
        let txn = db.begin_write().expect("write");
        {
            #[allow(clippy::disallowed_methods)]
            let mut log = txn.open_table(CHANGE_LOG_TABLE).expect("log");
            log.retain(|_, _| false).expect("prune");
            #[allow(clippy::disallowed_methods)]
            let mut m = txn.open_table(META_TABLE).expect("meta");
            m.insert(META_KEY_ARCHIVED_LSN, generation.to_le_bytes().as_slice())
                .expect("insert");
        }
        txn.commit().expect("commit");
    }
    assert_eq!(
        replica_base_point_of(&p).expect("point").end_lsn,
        generation
    );
}

#[test]
fn a_database_without_a_change_log_is_not_enabled() {
    let dir = TempDir::new("rb-disabled");
    drop(Storage::open(dir.db_path()).expect("open"));
    assert!(matches!(
        replica_base_point_of(&dir.db_path()),
        Err(IncrementalError::NotEnabled)
    ));
}

#[test]
fn a_database_open_for_writing_is_in_use() {
    let f = Fixture::new("rb-inuse");
    f.write_rows();
    assert!(matches!(
        replica_base_point_of(&f.db_dir.db_path()),
        Err(IncrementalError::InUse)
    ));
}

#[test]
fn an_applied_replica_stays_continuous() {
    let f = Fixture::new("rb-apply");
    f.write_rows();
    let p = f.base("r.redb");
    let base = f.storage().current_generation().unwrap();
    f.sql("INSERT INTO ev (id, embedding) VALUES (50, '[0.5,0.6]')");
    f.sql("INSERT INTO ev (id, embedding) VALUES (51, '[0.5,0.6]')");
    let recs = f
        .storage()
        .read_change_log_from(base + 1, 100_000, usize::MAX)
        .expect("read")
        .records;
    let replica = Storage::open(&p).expect("open replica");
    assert_eq!(replica.check_replica_base().expect("check").end_lsn, base);
    replica.apply_replicated_records(&recs).expect("apply");
    let after = replica.check_replica_base().expect("check");
    assert_eq!(after.end_lsn, f.storage().current_generation().unwrap());
}

#[test]
fn the_open_replica_check_maps_errors() {
    let f = Fixture::new("rb-open");
    f.write_rows();
    let p = f.base("r.redb");
    set_generation(&p, 9_999);
    let replica = Storage::open(&p).expect("open replica");
    assert!(matches!(
        replica.check_replica_base(),
        Err(ReplicaApplyError::BaseDiscontinuous)
    ));
    // 変更ログを有効にして開いているプライマリは、レプリカのベースではない。
    assert!(matches!(
        f.storage().check_replica_base(),
        Err(ReplicaApplyError::NotReplicaBase)
    ));
}
