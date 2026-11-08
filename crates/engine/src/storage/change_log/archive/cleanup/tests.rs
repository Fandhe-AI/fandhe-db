//! アーカイブ済みセグメントの保持と削除の試験（Issue #3092・RECOVER-14 (7) のポインタ）。
//!
//! 保持するフルバックアップから PITR できる区間を消さないこと（上限・境界）、削除の前後で
//! 連続性検査が通ること、途中で失敗しても残りが連続した区間になり再実行で収束すること、
//! 不正な入力（別 DB・別 timeline・増分・改ざん・共有ディレクトリ・差し替え）で何も消さないことを固定する。

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::change_log::archive::{list_segments, verify_archive_dir};
use crate::storage::{ArchiveOptions, Storage, Visibility};
use crate::test_util::temp_db::TempDir;

const TENANT: &str = "tenant-a";

fn private_dir(label: &str) -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let d = TempDir::new(label);
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    d
}

struct Fixture {
    _db_dir: TempDir,
    archive: TempDir,
    out: TempDir,
    core: EngineCore,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let db_dir = TempDir::new(label);
        let archive = private_dir(&format!("{label}-arc"));
        let out = TempDir::new(&format!("{label}-out"));
        let mut storage = Storage::open(db_dir.db_path()).expect("open");
        storage.enable_change_log().expect("enable");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        Self {
            _db_dir: db_dir,
            archive,
            out,
            core,
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

    fn insert(&self, from: u32, to: u32) {
        for i in from..to {
            self.sql(&format!(
                "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
            ));
        }
    }

    /// 1 レコード 1 セグメントで書き出し、水位以下の行をすべて刈り込む。
    fn archive_and_prune(&self) {
        let mut opts = ArchiveOptions::new(self.archive.path());
        opts.segment_bytes = 1;
        opts.retain_records = Some(0);
        let report = self.storage().archive_change_log(&opts).expect("archive");
        assert!(!report.skipped);
    }

    fn backup(&self, name: &str) -> RetentionPoint {
        let p = self.out.path().join(name);
        self.storage().export_snapshot(&p).expect("snapshot");
        retention_point_of(&p).expect("retention point")
    }

    fn lsns(&self) -> Vec<(u64, u64)> {
        list_segments(self.archive.path())
            .expect("list")
            .into_iter()
            .map(|(_, n)| (n.first_lsn, n.last_lsn))
            .collect()
    }

    fn policy(&self, backups: Vec<RetentionPoint>) -> CleanupPolicy {
        CleanupPolicy {
            backups,
            db: None,
            keep_secs: None,
            keep_segments: None,
            now_us: i64::MAX,
        }
    }

    fn verify_from(&self, l: u64) -> Result<crate::storage::ArchiveSummary, ArchiveError> {
        self.storage()
            .verify_change_log_archive_from(self.archive.path(), l)
    }
}

/// 表を作って 3 件挿入 → 書き出し・刈り込み → バックアップ（表は空）→ さらに 3 件 → 書き出し。
/// 戻り値はバックアップの位置。アーカイブは 1 レコード 1 セグメントで `base + 1 ..= 最新`。
fn two_rounds(f: &Fixture) -> RetentionPoint {
    f.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
    f.insert(0, 3);
    f.archive_and_prune();
    let b = f.backup("b1.redb");
    f.insert(3, 6);
    f.archive_and_prune();
    b
}

#[test]
fn deletes_only_up_to_the_backup_end_and_verification_holds_before_and_after() {
    let f = Fixture::new("cl-basic");
    let b = two_rounds(&f);
    assert_eq!(b.first_log_lsn, None, "pruned backup has an empty log");
    let before = f.lsns();
    let last = before.last().expect("segments").1;
    f.verify_from(b.end_lsn + 1).expect("verify before");

    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    assert_eq!(plan.cutoff(), b.end_lsn);
    let first = before.first().expect("first").0;
    assert_eq!(plan.lsn_range(), Some((first, b.end_lsn)));
    assert_eq!(f.lsns(), before, "plan deletes nothing");

    let report = execute_archive_cleanup(f.archive.path(), &plan).expect("execute");
    assert_eq!(report.removed, b.end_lsn - first + 1);
    let after = f.lsns();
    assert_eq!(after.first().expect("kept").0, b.end_lsn + 1);
    assert_eq!(after.last().expect("kept").1, last);
    f.verify_from(b.end_lsn + 1).expect("verify after");
    // 厳密な検査（先頭は有効化時の世代の次）は、意図した削除の後は通らない。
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::LsnGap)
    ));
    // 計画し直しても追加で消すものは無い。
    let again = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    assert_eq!(again.segments(), 0);
}

#[test]
fn a_segment_straddling_the_backup_end_is_kept_and_an_exact_end_is_removed() {
    let f = Fixture::new("cl-straddle");
    f.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
    f.insert(0, 5);
    let mut opts = ArchiveOptions::new(f.archive.path());
    opts.segment_bytes = 1;
    f.storage().archive_change_log(&opts).expect("archive");
    let segs = f.lsns();
    assert!(segs.len() >= 4);
    let base = f.backup("b.redb");
    // 1 つのセグメントの中ほどで終わるバックアップを模す。
    let mut mid = base;
    mid.first_log_lsn = None;
    mid.end_lsn = segs[2].1;
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![mid])).expect("plan");
    assert_eq!(plan.segments(), 3, "segments up to and including the end");
    // 終点がセグメントの途中（先頭 LSN と末尾 LSN の間）なら、そのセグメントは残る。
    let one = RetentionPoint {
        end_lsn: segs[2].0 - 1,
        ..mid
    };
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![one])).expect("plan");
    assert_eq!(plan.segments(), 2);
}

#[test]
fn the_oldest_backup_decides_the_cutoff() {
    let f = Fixture::new("cl-oldest");
    let b = two_rounds(&f);
    let newer = RetentionPoint {
        end_lsn: b.end_lsn + 2,
        ..b
    };
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![newer, b])).expect("plan");
    assert_eq!(plan.cutoff(), b.end_lsn);
}

#[test]
fn the_first_log_row_of_the_database_lowers_the_cutoff() {
    let f = Fixture::new("cl-firstrow");
    let b = two_rounds(&f);
    let first = f.lsns().first().expect("first").0;
    // バックアップの先頭行が（空でなく）ある: 最大の先頭行の手前までしか消さない。
    let held = RetentionPoint {
        first_log_lsn: Some(first + 1),
        ..b
    };
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![held])).expect("plan");
    assert_eq!(plan.cutoff(), first);
    // 先頭行が空のバックアップが 1 つでもあれば、この項は上限を下げない。
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![held, b])).expect("plan");
    assert_eq!(plan.cutoff(), b.end_lsn);
    // 停止中の DB の先頭行が分かるときは、それが優先する。
    let mut policy = f.policy(vec![b]);
    policy.db = Some(RetentionPoint {
        first_log_lsn: Some(first + 2),
        ..b
    });
    let plan = plan_archive_cleanup(f.archive.path(), &policy).expect("plan");
    assert_eq!(plan.cutoff(), first + 1);
}

#[cfg(unix)]
#[test]
fn executing_a_plan_against_another_directory_is_rejected_without_deleting() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("cl-other-dir");
    let b = two_rounds(&f);
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    assert!(plan.segments() >= 1);
    let before = f.lsns();
    let other = f.archive.path().join("other");
    std::fs::create_dir(&other).expect("mkdir");
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let err = execute_archive_cleanup(&other, &plan).expect_err("mismatched directory");
    assert!(matches!(err, CleanupError::Replaced));
    assert_eq!(f.lsns(), before);
}

#[test]
fn a_failure_on_the_second_unlink_reports_one_removed() {
    // 2 件目の削除の失敗で、1 件目の削除済みの件数が Partial に載ることを固定する。
    // （fsync の失敗は実ファイルシステムで注入できないため、unlink 後に数える順序はコードで担保する。）
    let f = Fixture::new("cl-count-after-unlink");
    let b = two_rounds(&f);
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    assert!(plan.segments() >= 2);
    let mut calls = 0;
    let err = execute_with(f.archive.path(), &plan, |p| {
        calls += 1;
        if calls == 2 {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        } else {
            std::fs::remove_file(p)
        }
    })
    .expect_err("injected failure");
    assert!(matches!(err, CleanupError::Partial { removed: 1 }));
}

#[test]
fn a_failure_midway_leaves_a_contiguous_tail_and_a_rerun_converges() {
    let f = Fixture::new("cl-partial");
    let b = two_rounds(&f);
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    assert!(plan.segments() >= 3);
    let mut calls = 0;
    let err = execute_with(f.archive.path(), &plan, |p| {
        calls += 1;
        if calls == 3 {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        } else {
            std::fs::remove_file(p)
        }
    })
    .expect_err("injected failure");
    assert!(matches!(err, CleanupError::Partial { removed: 2 }));
    // 残りは先頭から 2 個消えた連続した区間で、連続性検査は通る。
    let left = f.lsns();
    assert_eq!(left.first().expect("left").0, plan.victims[2].first_lsn);
    verify_archive_dir(f.archive.path(), &ArchiveExpectation::default()).expect("contiguous");
    f.verify_from(b.end_lsn + 1).expect("verify");
    // 再実行で収束する。
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    execute_archive_cleanup(f.archive.path(), &plan).expect("execute");
    assert_eq!(f.lsns().first().expect("kept").0, b.end_lsn + 1);
    f.verify_from(b.end_lsn + 1).expect("verify");
}

#[test]
fn nothing_is_removed_for_bad_inputs() {
    let f = Fixture::new("cl-refuse");
    let b = two_rounds(&f);
    let before = f.lsns();

    assert!(matches!(
        plan_archive_cleanup(f.archive.path(), &f.policy(vec![])),
        Err(CleanupError::NoBackups)
    ));
    let mut other_db = b;
    other_db.db_id[0] ^= 0xff;
    assert!(matches!(
        plan_archive_cleanup(f.archive.path(), &f.policy(vec![b, other_db])),
        Err(CleanupError::ForeignDatabase)
    ));
    // アーカイブと識別子が食い違うバックアップは、アーカイブの検証が拒否する。
    assert!(matches!(
        plan_archive_cleanup(f.archive.path(), &f.policy(vec![other_db])),
        Err(CleanupError::Archive(ArchiveError::ForeignDatabase))
    ));
    let mut other_tl = b;
    other_tl.timeline += 1;
    assert!(matches!(
        plan_archive_cleanup(f.archive.path(), &f.policy(vec![b, other_tl])),
        Err(CleanupError::ForeignTimeline)
    ));
    assert!(matches!(
        plan_archive_cleanup(f.archive.path(), &f.policy(vec![other_tl])),
        Err(CleanupError::Archive(ArchiveError::ForeignTimeline))
    ));
    // 増分（セグメント）ファイルはバックアップの基準にできない。
    let seg = list_segments(f.archive.path()).expect("list")[0].0.clone();
    assert!(matches!(
        retention_point_of(&seg),
        Err(CleanupError::NotFullBackup)
    ));
    // 読めないファイル。
    assert!(matches!(
        retention_point_of(&f.out.path().join("missing")),
        Err(CleanupError::BackupUnreadable)
    ));
    assert_eq!(f.lsns(), before);

    // 改ざんされたアーカイブでは計画しない。
    let victim = list_segments(f.archive.path()).expect("list")[1].0.clone();
    let mut bytes = std::fs::read(&victim).expect("read");
    let n = bytes.len();
    bytes[n - 1] ^= 0xff;
    std::fs::write(&victim, bytes).expect("write");
    assert!(matches!(
        plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])),
        Err(CleanupError::Archive(ArchiveError::ChecksumMismatch))
    ));
    assert_eq!(f.lsns(), before);
}

#[test]
fn a_shared_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("cl-shared");
    let b = two_rounds(&f);
    std::fs::set_permissions(f.archive.path(), std::fs::Permissions::from_mode(0o777))
        .expect("chmod");
    let before = f.lsns();
    assert!(matches!(
        plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])),
        Err(CleanupError::InsecureDirectory)
    ));
    assert_eq!(f.lsns(), before);
}

#[test]
fn keep_segments_and_keep_secs_only_lower_the_deletion() {
    let f = Fixture::new("cl-keep");
    let b = two_rounds(&f);
    let all = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b]))
        .expect("plan")
        .segments();
    let mut p = f.policy(vec![b]);
    p.keep_segments = Some(1);
    assert_eq!(
        plan_archive_cleanup(f.archive.path(), &p)
            .expect("plan")
            .segments(),
        all - 1
    );
    p.keep_segments = Some(u64::MAX);
    assert_eq!(
        plan_archive_cleanup(f.archive.path(), &p)
            .expect("plan")
            .segments(),
        0
    );
    // 保持期間: 現在時刻が過去（全セグメントが新しい）なら 1 つも消さない。遠い未来なら上限まで消す。
    let mut p = f.policy(vec![b]);
    p.keep_secs = Some(3600);
    p.now_us = 0;
    assert_eq!(
        plan_archive_cleanup(f.archive.path(), &p)
            .expect("plan")
            .segments(),
        0
    );
    p.now_us = i64::MAX;
    assert_eq!(
        plan_archive_cleanup(f.archive.path(), &p)
            .expect("plan")
            .segments(),
        all
    );
}

#[test]
fn a_replaced_segment_stops_the_execution_without_removing_anything() {
    let f = Fixture::new("cl-replaced");
    let b = two_rounds(&f);
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    let first = plan.victims[0].path.clone();
    let other = plan.victims[1].path.clone();
    std::fs::remove_file(&first).expect("rm");
    std::os::unix::fs::symlink(&other, &first).expect("symlink");
    let before = f.lsns();
    assert!(matches!(
        execute_archive_cleanup(f.archive.path(), &plan),
        Err(CleanupError::Replaced)
    ));
    assert_eq!(f.lsns(), before);
}

#[test]
fn archiving_continues_after_cleanup_without_stalling_the_prune() {
    let f = Fixture::new("cl-continue");
    let b = two_rounds(&f);
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    execute_archive_cleanup(f.archive.path(), &plan).expect("execute");
    f.insert(6, 9);
    f.archive_and_prune();
    f.verify_from(b.end_lsn + 1).expect("verify");
}

#[test]
fn verify_from_detects_a_lost_head_and_a_lost_tail() {
    let f = Fixture::new("cl-verify-from");
    let b = two_rounds(&f);
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    execute_archive_cleanup(f.archive.path(), &plan).expect("execute");
    let segs = list_segments(f.archive.path()).expect("list");
    // 保持区間の先頭（終点の次）のセグメントの紛失。
    std::fs::remove_file(&segs[0].0).expect("rm head");
    assert!(matches!(
        f.verify_from(b.end_lsn + 1),
        Err(ArchiveError::LsnGap)
    ));
}

#[test]
fn verify_from_detects_a_lost_tail() {
    let f = Fixture::new("cl-verify-tail");
    let b = two_rounds(&f);
    let plan = plan_archive_cleanup(f.archive.path(), &f.policy(vec![b])).expect("plan");
    execute_archive_cleanup(f.archive.path(), &plan).expect("execute");
    let segs = list_segments(f.archive.path()).expect("list");
    std::fs::remove_file(&segs.last().expect("tail").0).expect("rm tail");
    assert!(matches!(
        f.verify_from(b.end_lsn + 1),
        Err(ArchiveError::LsnGap)
    ));
}

#[test]
fn display_messages_are_fixed() {
    for e in [
        CleanupError::NoBackups,
        CleanupError::NotFullBackup,
        CleanupError::Replaced,
        CleanupError::Partial { removed: 7 },
        CleanupError::Io(std::io::ErrorKind::NotFound),
    ] {
        let s = e.to_string();
        assert!(!s.chars().any(|c| c.is_ascii_digit()));
    }
}
