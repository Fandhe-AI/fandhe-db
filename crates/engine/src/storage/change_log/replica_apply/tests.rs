//! レプリカ適用の試験（Issue #2174・RECOVER-15 のポインタ）。
//!
//! プライマリ（変更ログ有効）の `read_change_log_from` で取ったレコードを、`export_snapshot` で作った
//! ベースのコピー（変更ログ無効のまま開く）へ適用し、全表の一致・再開・冪等・順序違反の拒否と
//! バッチ原子性・書き込み拒否を固定する。

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::change_log::oracle;
use crate::storage::Visibility;
use crate::test_util::temp_db::TempDir;

const TENANT: &str = "tenant-a";

struct Fixture {
    _db_dir: TempDir,
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
            _db_dir: db_dir,
            out,
            core: EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        }
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

    fn generation(&self) -> u64 {
        self.storage().current_generation().expect("generation")
    }

    /// ベース（フルバックアップ）を作って、変更ログ無効のまま開いたレプリカを返す。
    fn replica(&self, name: &str) -> (std::path::PathBuf, Storage) {
        let p = self.out.path().join(name);
        self.storage().export_snapshot(&p).expect("snapshot");
        let s = Storage::open(&p).expect("open replica");
        (p, s)
    }

    /// `from` 以降のレコードをすべて読む。
    fn records_from(&self, from: u64) -> Vec<(u64, Vec<u8>)> {
        self.storage()
            .read_change_log_from(from, 100_000, usize::MAX)
            .expect("read")
            .records
    }

    fn write_round_1(&self) {
        self.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        for i in 0..4 {
            self.sql(&format!(
                "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
            ));
        }
    }

    fn write_round_2(&self) {
        self.sql("CREATE TABLE ev2 (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        self.sql("INSERT INTO ev2 (id, embedding) VALUES (1, '[0.3,0.4]')");
        self.sql("DELETE FROM ev WHERE id = 2");
        self.sql("DROP TABLE ev2");
        self.sql("INSERT INTO ev (id, embedding) VALUES (10, '[0.5,0.6]')");
    }
}

fn dump(s: &Storage) -> oracle::Model {
    let txn = s.db.begin_write().expect("begin_write");
    let m = oracle::dump_writer(&txn).expect("dump");
    txn.abort().expect("abort");
    m
}

fn assert_same(primary: &Storage, replica: &Storage) {
    assert_eq!(dump(primary), dump(replica));
    assert_eq!(
        primary.current_generation().unwrap(),
        replica.replica_applied_lsn().unwrap()
    );
}

#[test]
fn applies_records_and_matches_primary() {
    let f = Fixture::new("rep-apply");
    f.write_round_1();
    let (_p, replica) = f.replica("r1.redb");
    let base = replica.replica_applied_lsn().unwrap();
    assert_eq!(base, f.generation());
    f.write_round_2();
    let recs = f.records_from(base + 1);
    assert!(recs.len() >= 5);
    let rep = replica.apply_replicated_records(&recs).expect("apply");
    assert_eq!(rep.applied, recs.len() as u64);
    assert_eq!(rep.skipped_duplicates, 0);
    assert_eq!(rep.last_lsn, f.generation());
    assert_same(f.storage(), &replica);
}

#[test]
fn identity_matches_primary() {
    let f = Fixture::new("rep-ident");
    f.write_round_1();
    let (_p, replica) = f.replica("r.redb");
    let id = replica.replica_identity().unwrap();
    assert_eq!(Some(id), f.storage().change_log_stream_identity());
}

#[test]
fn resumes_after_reopen_and_between_batches() {
    let f = Fixture::new("rep-resume");
    f.write_round_1();
    let (path, replica) = f.replica("r.redb");
    let base = replica.replica_applied_lsn().unwrap();
    f.write_round_2();
    let recs = f.records_from(base + 1);
    let (head, tail) = recs.split_at(2);
    replica.apply_replicated_records(head).expect("head");
    drop(replica);
    let replica = Storage::open(&path).expect("reopen");
    let at = replica.replica_applied_lsn().unwrap();
    assert_eq!(at, base + 2);
    replica.apply_replicated_records(tail).expect("tail");
    assert_same(f.storage(), &replica);
}

#[test]
fn duplicates_are_skipped_and_diverged_is_rejected() {
    let f = Fixture::new("rep-dup");
    f.write_round_1();
    let (_p, replica) = f.replica("r.redb");
    let base = replica.replica_applied_lsn().unwrap();
    f.write_round_2();
    let recs = f.records_from(base + 1);
    replica.apply_replicated_records(&recs).unwrap();
    // 同じ列の再送は無害。
    let rep = replica.apply_replicated_records(&recs).expect("resend");
    assert_eq!(rep.applied, 0);
    assert_eq!(rep.skipped_duplicates, recs.len() as u64);
    // 重複を先頭に含み、続きがある再送。
    f.sql("INSERT INTO ev (id, embedding) VALUES (99, '[0.9,0.9]')");
    let more = f.records_from(base + 1);
    let rep = replica.apply_replicated_records(&more).expect("overlap");
    assert_eq!(rep.applied, 1);
    assert_same(f.storage(), &replica);
    // 同じ LSN でフレームとヘッダが食い違うものは拒否する。
    let mut bad = more[0].clone();
    bad.1 = more[1].1.clone();
    assert!(replica.apply_replicated_records(&[bad]).is_err());
    // 適用先の同じ LSN の行が受信内容と異なる（分岐した履歴）なら Diverged。
    let txn = replica.db.begin_write().unwrap();
    {
        #[allow(clippy::disallowed_methods)]
        let mut log = txn.open_table(CHANGE_LOG_TABLE).unwrap();
        log.insert(more[0].0, more[1].1.as_slice()).unwrap();
    }
    txn.commit().unwrap();
    assert!(matches!(
        replica.apply_replicated_records(&[more[0].clone()]),
        Err(ReplicaApplyError::Diverged)
    ));
}

/// 拒否後も DB は直前の整合状態のままで、正しい続きを渡せば適用を再開できる。
fn assert_rejected_then_resumes(
    f: &Fixture,
    replica: &Storage,
    base: u64,
    bad: Vec<(u64, Vec<u8>)>,
    check: impl Fn(&ReplicaApplyError) -> bool,
) {
    let before = dump(replica);
    let err = replica.apply_replicated_records(&bad).unwrap_err();
    assert!(check(&err), "unexpected error: {err}");
    assert_eq!(replica.replica_applied_lsn().unwrap(), base);
    assert_eq!(dump(replica), before);
    let good = f.records_from(base + 1);
    replica.apply_replicated_records(&good).expect("resume");
    assert_same(f.storage(), replica);
}

type Setup = (
    Fixture,
    std::path::PathBuf,
    Storage,
    u64,
    Vec<(u64, Vec<u8>)>,
);

fn setup(label: &str) -> Setup {
    let f = Fixture::new(label);
    f.write_round_1();
    let (p, replica) = f.replica("r.redb");
    let base = replica.replica_applied_lsn().unwrap();
    f.write_round_2();
    let recs = f.records_from(base + 1);
    (f, p, replica, base, recs)
}

#[test]
fn rejects_gap_and_reorder() {
    let (f, _p, r, base, recs) = setup("rep-gap");
    let gap = vec![recs[1].clone(), recs[2].clone()];
    assert_rejected_then_resumes(&f, &r, base, gap, |e| {
        matches!(e, ReplicaApplyError::OutOfOrder)
    });
    let (f, _p, r, base, recs) = setup("rep-reorder");
    let reordered = vec![recs[1].clone(), recs[0].clone()];
    assert_rejected_then_resumes(&f, &r, base, reordered, |e| {
        matches!(e, ReplicaApplyError::OutOfOrder)
    });
}

#[test]
fn rejects_partial_batch_atomically() {
    let (f, _p, r, base, recs) = setup("rep-atomic");
    // 先頭 2 件は正しく、3 件目が欠番。全体が適用されない。
    let bad = vec![recs[0].clone(), recs[1].clone(), recs[3].clone()];
    assert_rejected_then_resumes(&f, &r, base, bad, |e| {
        matches!(e, ReplicaApplyError::OutOfOrder)
    });
}

#[test]
fn rejects_frame_lsn_mismatch() {
    let (f, _p, r, base, recs) = setup("rep-frame");
    let bad = vec![(recs[0].0 + 7, recs[0].1.clone())];
    assert_rejected_then_resumes(&f, &r, base, bad, |e| {
        matches!(e, ReplicaApplyError::OutOfOrder)
    });
}

#[test]
fn rejects_corrupt_checksum_and_truncation() {
    let (f, _p, r, base, recs) = setup("rep-corrupt");
    let mut flipped = recs[0].clone();
    let n = flipped.1.len();
    flipped.1[n - 1] ^= 0xff;
    assert_rejected_then_resumes(&f, &r, base, vec![flipped], |e| {
        matches!(e, ReplicaApplyError::Corrupt)
    });
    let (f, _p, r, base, recs) = setup("rep-trunc");
    let mut cut = recs[0].clone();
    cut.1.truncate(cut.1.len() / 2);
    assert_rejected_then_resumes(&f, &r, base, vec![cut], |e| {
        matches!(e, ReplicaApplyError::Corrupt)
    });
}

#[test]
fn rejects_foreign_database() {
    let (f, _p, r, base, _recs) = setup("rep-foreign");
    let other = Fixture::new("rep-foreign-other");
    other.write_round_1();
    other.write_round_2();
    let theirs = other.records_from(base + 1);
    assert_rejected_then_resumes(&f, &r, base, vec![theirs[0].clone()], |e| {
        matches!(e, ReplicaApplyError::Foreign)
    });
}

#[test]
fn rejects_oversized_record_and_batch() {
    let (_f, _p, r, _base, recs) = setup("rep-limits");
    let big = (recs[0].0, vec![0u8; MAX_RECORD_BYTES + 1]);
    assert!(matches!(
        r.apply_replicated_records(&[big]),
        Err(ReplicaApplyError::Corrupt)
    ));
    let many: Vec<(u64, Vec<u8>)> = (0..=MAX_APPLY_RECORDS).map(|_| recs[0].clone()).collect();
    assert!(matches!(
        r.apply_replicated_records(&many),
        Err(ReplicaApplyError::BatchTooLarge)
    ));
}

#[test]
fn ordinary_writes_on_replica_are_refused() {
    let (_f, _p, r, _base, _recs) = setup("rep-write");
    let before = r.replica_applied_lsn().unwrap();
    let err = match r.begin_write_txn() {
        Err(e) => e,
        Ok(_) => panic!("write must be refused"),
    };
    assert!(
        matches!(err, StorageError::CommitFootprintUnrecordable),
        "unexpected: {err:?}"
    );
    assert_eq!(r.replica_applied_lsn().unwrap(), before);
}

#[test]
fn primary_and_plain_db_are_not_replica_bases() {
    let f = Fixture::new("rep-notbase");
    f.write_round_1();
    assert!(matches!(
        f.storage().apply_replicated_records(&[]),
        Err(ReplicaApplyError::NotReplicaBase)
    ));
    let dir = TempDir::new("rep-plain");
    let plain = Storage::open(dir.db_path()).unwrap();
    assert!(matches!(
        plain.apply_replicated_records(&[]),
        Err(ReplicaApplyError::NotReplicaBase)
    ));
}

#[test]
fn rejects_reversed_or_gapped_duplicates_before_new_record() {
    let (f, _p, r, base, recs) = setup("rep-dupseq");
    r.apply_replicated_records(&recs).unwrap();
    let applied = r.replica_applied_lsn().unwrap();
    assert!(applied > base);
    f.sql("INSERT INTO ev (id, embedding) VALUES (77, '[0.7,0.7]')");
    let new = f.records_from(applied + 1);
    assert_eq!(new.len(), 1);
    let before = dump(&r);
    // 逆順の重複列の後に新規レコード。
    let mut reversed: Vec<_> = recs.iter().rev().cloned().collect();
    reversed.extend(new.iter().cloned());
    assert!(matches!(
        r.apply_replicated_records(&reversed),
        Err(ReplicaApplyError::OutOfOrder)
    ));
    // 欠落のある重複列の後に新規レコード。
    let mut gapped = vec![recs[0].clone(), recs[2].clone()];
    gapped.extend(new.iter().cloned());
    assert!(matches!(
        r.apply_replicated_records(&gapped),
        Err(ReplicaApplyError::OutOfOrder)
    ));
    assert_eq!(r.replica_applied_lsn().unwrap(), applied);
    assert_eq!(dump(&r), before);
    // 正しい連続列なら適用できる。
    let mut ok = recs.clone();
    ok.extend(new);
    r.apply_replicated_records(&ok).expect("contiguous");
    assert_same(f.storage(), &r);
}

#[test]
fn applies_on_non_immediate_storage() {
    let f = Fixture::new("rep-nodur");
    f.write_round_1();
    let p = f.out.path().join("nd.redb");
    f.storage().export_snapshot(&p).expect("snapshot");
    let r = Storage::open_with_durability(&p, crate::storage::WriteDurability::None)
        .expect("open replica");
    let base = r.replica_applied_lsn().unwrap();
    f.write_round_2();
    let recs = f.records_from(base + 1);
    let rep = r.apply_replicated_records(&recs).expect("apply");
    assert_eq!(rep.applied, recs.len() as u64);
    assert_same(f.storage(), &r);
    drop(r);
    // 再オープン後も適用位置が残る（Immediate で確定している）。
    let r2 = Storage::open(&p).expect("reopen");
    assert_eq!(r2.replica_applied_lsn().unwrap(), f.generation());
}
