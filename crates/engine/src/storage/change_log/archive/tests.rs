//! 変更ログのアーカイブ・保持管理の試験（Issue #2167・RECOVER-14 のポインタ）。
//!
//! 受け入れ条件「アーカイブの欠落が検出できる」（LSN の飛び・チェックサム不一致・別 DB／別 timeline・
//! 末尾の紛失）、再試行の収束、未アーカイブ行を刈り込まないこと、保守 commit が世代を進めず後続の
//! 利用者 commit の記録を壊さないこと、刈り込み後の再有効化が連続性検査に拒否されないことを固定する。
//! `cfg(test)` では変更ログが有効な全 commit がオラクルで検証されるため、刈り込み後の利用者 commit
//! （`prev == storage_generation` の連鎖）もここで間接的に確かめられる。

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::{Visibility, GENERATION_KEY, GENERATION_TABLE};
use crate::test_util::temp_db::TempDir;

const TENANT: &str = "tenant-a";

struct Fixture {
    _db_dir: TempDir,
    archive: TempDir,
    core: EngineCore,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let db_dir = TempDir::new(label);
        let archive = TempDir::new(&format!("{label}-arc"));
        let mut storage = Storage::open(db_dir.db_path()).expect("open");
        storage.enable_change_log().expect("enable");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        Self {
            _db_dir: db_dir,
            archive,
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

    /// 表を作って `n` 件を 1 件ずつ挿入する（`n + 1` commit = `n + 1` レコード）。
    fn populate(&self, n: u32) {
        self.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        for i in 0..n {
            self.sql(&format!(
                "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
            ));
        }
    }

    fn opts(&self) -> ArchiveOptions {
        ArchiveOptions::new(self.archive.path())
    }

    fn generation(&self) -> u64 {
        self.storage().current_generation().expect("generation")
    }

    fn row_lsns(&self) -> Vec<u64> {
        let read = self.storage().db.begin_read().expect("read");
        let t = match read.open_table(CHANGE_LOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Vec::new(),
            Err(e) => panic!("open: {e}"),
        };
        t.iter()
            .expect("iter")
            .map(|e| e.expect("entry").0.value())
            .collect()
    }

    fn watermark(&self) -> u64 {
        watermark(&self.storage().db.begin_read().expect("read")).expect("watermark")
    }

    fn segment_files(&self) -> Vec<PathBuf> {
        list_segments(self.archive.path())
            .expect("list")
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    }
}

fn flip_byte(path: &Path, offset_from_end: usize) {
    let mut bytes = std::fs::read(path).expect("read");
    let i = bytes.len() - offset_from_end;
    bytes[i] ^= 0xff;
    std::fs::write(path, bytes).expect("write");
}

#[test]
fn archive_writes_segments_and_verifies_against_the_database() {
    let f = Fixture::new("arc-basic");
    f.populate(5);
    let gen_before = f.generation();
    let base_rows = f.row_lsns();
    let mut opts = f.opts();
    opts.segment_bytes = 1; // 1 レコードごとに閉じる
    let report = f.storage().archive_change_log(&opts).expect("archive");
    assert_eq!(report.segments_written, 6);
    assert_eq!(report.segments_adopted, 0);
    assert_eq!(report.pruned_records, 0, "retention is off by default");
    assert_eq!(report.archived_through, gen_before);
    assert!(!report.skipped);
    // 保守 commit は世代を進めず、行も消さない。
    assert_eq!(f.generation(), gen_before);
    assert_eq!(f.row_lsns(), base_rows);
    assert_eq!(f.watermark(), gen_before);

    let summary = f
        .storage()
        .verify_change_log_archive(f.archive.path())
        .expect("verify");
    assert_eq!(summary.segments, 6);
    assert_eq!(summary.records, 6);
    assert_eq!(summary.last_lsn, Some(gen_before));

    // 再実行は何もしない（冪等）。
    let again = f.storage().archive_change_log(&opts).expect("again");
    assert_eq!(again.segments_written, 0);
    assert_eq!(again.archived_through, gen_before);
}

#[test]
fn archive_after_more_writes_continues_the_chain() {
    let f = Fixture::new("arc-continue");
    f.populate(2);
    f.storage().archive_change_log(&f.opts()).expect("first");
    f.sql("INSERT INTO ev (id, embedding) VALUES (100, '[0.3,0.4]')");
    let r = f.storage().archive_change_log(&f.opts()).expect("second");
    assert_eq!(r.segments_written, 1);
    let s = f
        .storage()
        .verify_change_log_archive(f.archive.path())
        .expect("verify");
    assert_eq!(s.segments, 2);
    assert_eq!(s.last_lsn, Some(f.generation()));
}

#[test]
fn segment_files_are_private_and_never_overwritten() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("arc-perm");
    f.populate(1);
    f.storage().archive_change_log(&f.opts()).expect("archive");
    for p in f.segment_files() {
        let mode = std::fs::metadata(&p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    // 一時名の残骸は残らない。
    let leftovers = std::fs::read_dir(f.archive.path())
        .expect("dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX))
        .count();
    assert_eq!(leftovers, 0);
}

#[test]
fn invalid_directory_and_options_are_rejected() {
    let f = Fixture::new("arc-badopts");
    f.populate(1);
    let mut opts = ArchiveOptions::new(f.archive.path().join("missing"));
    assert!(matches!(
        f.storage().archive_change_log(&opts),
        Err(ArchiveError::InvalidDirectory)
    ));
    opts = f.opts();
    opts.segment_bytes = 0;
    assert!(matches!(
        f.storage().archive_change_log(&opts),
        Err(ArchiveError::InvalidOptions)
    ));
    // 失敗したアーカイブは水位を進めない。
    assert!(f.watermark() < f.generation());
}

#[test]
fn archive_requires_an_enabled_change_log() {
    let db_dir = TempDir::new("arc-disabled");
    let arc = TempDir::new("arc-disabled-dir");
    let storage = Storage::open(db_dir.db_path()).expect("open");
    assert!(matches!(
        storage.archive_change_log(&ArchiveOptions::new(arc.path())),
        Err(ArchiveError::NotEnabled)
    ));
}

#[test]
fn missing_middle_segment_is_detected_as_a_gap() {
    let f = Fixture::new("arc-gap");
    f.populate(4);
    let mut opts = f.opts();
    opts.segment_bytes = 1;
    f.storage().archive_change_log(&opts).expect("archive");
    let files = f.segment_files();
    std::fs::remove_file(files.get(2).expect("segment")).expect("remove");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::LsnGap)
    ));
    assert!(matches!(
        verify_archive_dir(f.archive.path(), &ArchiveExpectation::default()),
        Err(ArchiveError::LsnGap)
    ));
}

#[test]
fn missing_first_or_last_segment_is_detected_against_the_database() {
    let f = Fixture::new("arc-ends");
    f.populate(3);
    let mut opts = f.opts();
    opts.segment_bytes = 1;
    f.storage().archive_change_log(&opts).expect("archive");
    let files = f.segment_files();
    // 末尾の紛失は、ディレクトリ単体では連鎖が保たれるが、DB の水位との突き合わせで検出される。
    let last = files.last().expect("last").clone();
    let saved = std::fs::read(&last).expect("read");
    std::fs::remove_file(&last).expect("remove");
    assert!(verify_archive_dir(f.archive.path(), &ArchiveExpectation::default()).is_ok());
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::LsnGap)
    ));
    std::fs::write(&last, saved).expect("restore");
    // 先頭の紛失。
    let first = files.first().expect("first");
    std::fs::remove_file(first).expect("remove");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::LsnGap)
    ));
}

#[test]
fn tampered_segment_is_detected_by_checksum() {
    let f = Fixture::new("arc-tamper");
    f.populate(2);
    f.storage().archive_change_log(&f.opts()).expect("archive");
    let file = f.segment_files().pop().expect("segment");
    // 本体（レコードのペイロード）の 1 バイト、トレーラ、ヘッダをそれぞれ壊す。
    for (offset, label) in [(CHECKSUM_LEN + 3, "body"), (1, "trailer")] {
        let original = std::fs::read(&file).expect("read");
        flip_byte(&file, offset);
        assert!(
            matches!(
                f.storage().verify_change_log_archive(f.archive.path()),
                Err(ArchiveError::ChecksumMismatch)
            ),
            "{label}"
        );
        std::fs::write(&file, original).expect("restore");
    }
    let original = std::fs::read(&file).expect("read");
    let mut bytes = original.clone();
    bytes[10] ^= 0x01; // ヘッダ内（DB 識別子）
    std::fs::write(&file, bytes).expect("write");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::ChecksumMismatch)
    ));
    std::fs::write(&file, original).expect("restore");
    assert!(f
        .storage()
        .verify_change_log_archive(f.archive.path())
        .is_ok());
}

#[test]
fn truncated_and_extended_segments_are_rejected() {
    let f = Fixture::new("arc-trunc");
    f.populate(2);
    f.storage().archive_change_log(&f.opts()).expect("archive");
    let file = f.segment_files().pop().expect("segment");
    let original = std::fs::read(&file).expect("read");
    std::fs::write(&file, original.get(..original.len() - 5).expect("cut")).expect("write");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::Truncated)
    ));
    let mut longer = original.clone();
    longer.push(0);
    std::fs::write(&file, longer).expect("write");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::BadFormat)
    ));
    // ヘッダだけ（本体が無い）。
    std::fs::write(&file, original.get(..10).expect("cut")).expect("write");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::Truncated)
    ));
}

#[test]
fn segment_from_another_database_or_timeline_is_rejected() {
    let a = Fixture::new("arc-foreign-a");
    a.populate(2);
    a.storage()
        .archive_change_log(&a.opts())
        .expect("archive a");
    let b = Fixture::new("arc-foreign-b");
    b.populate(2);
    b.storage()
        .archive_change_log(&b.opts())
        .expect("archive b");

    // 別 DB の同じ LSN 範囲のセグメントを混ぜる: 期待する DB 識別子と一致しない。
    let foreign = b.segment_files().pop().expect("b segment");
    let name = foreign.file_name().expect("name").to_owned();
    let dest = a.archive.path().join(&name);
    assert!(!dest.exists());
    // a 側の本来のセグメントを退避し、b のセグメントに置き換える。
    for p in a.segment_files() {
        std::fs::remove_file(p).expect("remove");
    }
    std::fs::copy(&foreign, &dest).expect("copy");
    assert!(matches!(
        a.storage().verify_change_log_archive(a.archive.path()),
        Err(ArchiveError::ForeignDatabase)
    ));
    // 別 timeline: 同じ DB 識別子で timeline だけ違う状態のレコードで組み立てる。
    let state_a = a.storage().change_log.clone().expect("state");
    let mut other = (*state_a).clone();
    other.timeline = state_a.timeline + 1;
    let mut builder = SegmentBuilder::new(&other, 1);
    let read = a.storage().db.begin_read().expect("read");
    let table = read.open_table(CHANGE_LOG_TABLE).expect("table");
    let (k, v) = table.first().expect("first").expect("row");
    // 表のレコードの timeline と builder の timeline が違うので、組み立て段階で拒否される。
    assert!(matches!(
        builder.push(k.value(), v.value()),
        Err(ArchiveError::LogCorrupt)
    ));
}

#[test]
fn mixed_timelines_in_one_directory_are_rejected() {
    // timeline の異なる 2 つのセグメントを手で作って並べる。
    let dir = TempDir::new("arc-timeline");
    let f = Fixture::new("arc-timeline-src");
    f.populate(1);
    let state = f.storage().change_log.clone().expect("state");
    let read = f.storage().db.begin_read().expect("read");
    let table = read.open_table(CHANGE_LOG_TABLE).expect("table");
    let rows: Vec<(u64, Vec<u8>)> = table
        .iter()
        .expect("iter")
        .map(|e| {
            let (k, v) = e.expect("row");
            (k.value(), v.value().to_vec())
        })
        .collect();
    assert_eq!(rows.len(), 2);
    let write = |first_idx: usize, timeline: u64| {
        let (key, value) = rows.get(first_idx).expect("row");
        let mut st = (*state).clone();
        st.timeline = timeline;
        // 表のレコードの timeline は state の値なので、検査を通すため header を直接組む。
        let header = SegmentHeader {
            db_id: st.db_id,
            timeline,
            first_lsn: *key,
            last_lsn: *key,
            record_count: 1,
            storage_format: peek_record(value, usize::MAX).expect("peek").storage_format,
            body_len: value.len() as u64,
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(value);
        let mut h = Sha256::new();
        h.update(&bytes);
        bytes.extend_from_slice(&h.finalize());
        let name = segment_name(&header.db_id, timeline, *key, *key);
        std::fs::write(dir.path().join(name), bytes).expect("write");
    };
    write(0, state.timeline);
    write(1, state.timeline + 1);
    // 2 つ目のセグメントのレコードは state の timeline を持つため、ヘッダの timeline と食い違い、
    // 別 timeline として拒否される。
    assert!(matches!(
        verify_archive_dir(dir.path(), &ArchiveExpectation::default()),
        Err(ArchiveError::ForeignTimeline)
    ));
}

#[test]
fn file_name_and_header_disagreement_is_rejected() {
    let f = Fixture::new("arc-rename");
    f.populate(2);
    f.storage().archive_change_log(&f.opts()).expect("archive");
    let file = f.segment_files().pop().expect("segment");
    let name = parse_segment_name(file.file_name().expect("n").to_str().expect("utf8"))
        .expect("parse")
        .expect("ours");
    let renamed = f.archive.path().join(segment_name(
        &name.db_id,
        name.timeline,
        name.first_lsn + 1,
        name.last_lsn + 1,
    ));
    std::fs::rename(&file, renamed).expect("rename");
    assert!(verify_archive_dir(f.archive.path(), &ArchiveExpectation::default()).is_err());
}

#[test]
fn unrelated_files_are_ignored_but_malformed_segment_names_are_not() {
    let f = Fixture::new("arc-names");
    f.populate(1);
    f.storage().archive_change_log(&f.opts()).expect("archive");
    std::fs::write(f.archive.path().join("README.txt"), b"x").expect("write");
    assert!(f
        .storage()
        .verify_change_log_archive(f.archive.path())
        .is_ok());
    std::fs::write(f.archive.path().join("fandhe-cl-zz.seg"), b"x").expect("write");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::BadFormat)
    ));
}

#[test]
fn symlinked_segment_is_rejected() {
    let f = Fixture::new("arc-symlink");
    f.populate(1);
    f.storage().archive_change_log(&f.opts()).expect("archive");
    let file = f.segment_files().pop().expect("segment");
    let moved = f.archive.path().join("moved.bin");
    std::fs::rename(&file, &moved).expect("rename");
    std::os::unix::fs::symlink(&moved, &file).expect("symlink");
    assert!(matches!(
        f.storage().verify_change_log_archive(f.archive.path()),
        Err(ArchiveError::BadFormat)
    ));
}

#[test]
fn retry_after_a_lost_watermark_adopts_the_identical_segment() {
    let f = Fixture::new("arc-retry");
    f.populate(3);
    let base = f.watermark();
    f.storage().archive_change_log(&f.opts()).expect("archive");
    let files_before = f.segment_files();
    let bytes_before = std::fs::read(files_before.first().expect("seg")).expect("read");
    // 公開後・水位更新前のクラッシュを再現する: 水位を元へ戻す。
    set_watermark(f.storage(), base);
    // 設定を変えた再起動（セグメントのバイト上限が違う）でも、同じ先頭 LSN のセグメントを採用する。
    let mut opts = f.opts();
    opts.segment_bytes = 1;
    let r = f.storage().archive_change_log(&opts).expect("retry");
    assert_eq!(r.segments_written, 0);
    assert_eq!(r.segments_adopted, 1);
    assert_eq!(r.archived_through, f.generation());
    assert_eq!(
        f.segment_files(),
        files_before,
        "no duplicate range is created"
    );
    assert_eq!(
        std::fs::read(files_before.first().expect("seg")).expect("read"),
        bytes_before,
        "the existing file is untouched"
    );
}

#[test]
fn a_conflicting_segment_of_the_same_name_stops_the_archive() {
    let f = Fixture::new("arc-conflict");
    f.populate(2);
    let base = f.watermark();
    f.storage().archive_change_log(&f.opts()).expect("archive");
    let file = f.segment_files().pop().expect("segment");
    let original = std::fs::read(&file).expect("read");
    set_watermark(f.storage(), base);
    // 中身が壊れている（読めない）。上書きも削除もしない。
    flip_byte(&file, 5);
    let damaged = std::fs::read(&file).expect("read");
    assert!(matches!(
        f.storage().archive_change_log(&f.opts()),
        Err(ArchiveError::Conflict)
    ));
    assert_eq!(std::fs::read(&file).expect("read"), damaged);
    assert_eq!(f.watermark(), base, "the watermark does not advance");
    // 検証に通る別内容（短いセグメント）が同じ先頭 LSN で在る場合も、表と一致しなければ止める。
    std::fs::write(&file, &original).expect("restore");
    let (h, short) = {
        let state = f.storage().change_log.clone().expect("state");
        let read = f.storage().db.begin_read().expect("read");
        let table = read.open_table(CHANGE_LOG_TABLE).expect("table");
        let mut b = SegmentBuilder::new(&state, base + 1);
        let (k, v) = table.first().expect("first").expect("row");
        b.push(k.value(), v.value()).expect("push");
        b.finish().expect("finish")
    };
    std::fs::remove_file(&file).expect("remove");
    std::fs::write(
        f.archive
            .path()
            .join(segment_name(&h.db_id, h.timeline, h.first_lsn, h.last_lsn)),
        short,
    )
    .expect("write");
    // 短いセグメントは表の先頭 1 件と一致するので採用され、残りは新しいセグメントに続く。
    let r = f
        .storage()
        .archive_change_log(&f.opts())
        .expect("adopt prefix");
    assert_eq!(r.segments_adopted, 1);
    assert!(f
        .storage()
        .verify_change_log_archive(f.archive.path())
        .is_ok());
}

fn set_watermark(storage: &Storage, w: u64) {
    let txn = storage.db.begin_write().expect("begin");
    {
        let mut meta = txn.open_table(META_TABLE).expect("meta");
        meta.insert(META_KEY_ARCHIVED_LSN, w.to_le_bytes().as_slice())
            .expect("insert");
    }
    txn.commit().expect("commit");
}

#[test]
fn retention_prunes_only_archived_rows_and_keeps_the_newest() {
    let f = Fixture::new("arc-prune");
    f.populate(6);
    let generation = f.generation();
    let mut opts = f.opts();
    opts.retain_records = Some(2);
    let r = f.storage().archive_change_log(&opts).expect("archive");
    assert_eq!(r.pruned_records, generation - 2 - f.watermark_base_offset());
    assert_eq!(
        f.generation(),
        generation,
        "pruning does not advance the generation"
    );
    let rows = f.row_lsns();
    assert_eq!(rows, vec![generation - 1, generation]);
    // 後続の利用者 commit は連鎖を保って記録される（オラクルが検証する）。
    f.sql("INSERT INTO ev (id, embedding) VALUES (200, '[0.5,0.5]')");
    assert_eq!(f.row_lsns().last().copied(), Some(generation + 1));
    // アーカイブ側は全レコードを持つ。
    f.storage().archive_change_log(&opts).expect("archive 2");
    let s = f
        .storage()
        .verify_change_log_archive(f.archive.path())
        .expect("verify");
    assert_eq!(s.records, generation + 1);
}

impl Fixture {
    /// 有効化時の世代（空の DB では 0）。刈り込み件数の期待値の計算に使う。
    fn watermark_base_offset(&self) -> u64 {
        let read = self.storage().db.begin_read().expect("read");
        read_meta_u64(&read, META_KEY_BASE_GENERATION)
            .expect("meta")
            .expect("base")
    }
}

#[test]
fn unarchived_rows_are_never_pruned() {
    let f = Fixture::new("arc-unarchived");
    f.populate(5);
    let generation = f.generation();
    let base = f.watermark();
    let mut opts = f.opts();
    opts.retain_records = Some(0);
    // 水位を途中（base + 3）までしか進めない保守 commit。以降は未アーカイブで、保持 0 でも消えない。
    let step = maintenance_commit(f.storage(), base, base + 3, &opts).expect("maintenance");
    assert!(matches!(step, Step::Done { pruned: 3 }));
    assert_eq!(f.row_lsns(), (base + 4..=generation).collect::<Vec<_>>());
    // 水位を超えて消すことは構造的にできない: 続けて刈り込みだけを実行しても何も消えない。
    let again = maintenance_commit(f.storage(), base + 3, base + 3, &opts).expect("again");
    assert!(matches!(again, Step::Done { pruned: 0 }));
    assert_eq!(f.row_lsns().len() as u64, generation - base - 3);
    // 古い水位を渡した（並行した別の更新を見ていない）保守 commit は拒否する。
    assert!(matches!(
        maintenance_commit(f.storage(), base, base + 1, &opts),
        Err(ArchiveError::WatermarkMoved)
    ));
    // アーカイブ先が使えない間は何も消えない。
    let bad = ArchiveOptions {
        retain_records: Some(0),
        ..ArchiveOptions::new(f.archive.path().join("missing"))
    };
    assert!(f.storage().archive_change_log(&bad).is_err());
    assert_eq!(f.row_lsns().len() as u64, generation - base - 3);
}

#[test]
fn prune_is_bounded_per_maintenance_commit() {
    let f = Fixture::new("arc-bound");
    f.populate(9);
    let generation = f.generation();
    let base = f.watermark();
    let mut opts = f.opts();
    opts.retain_records = Some(0);
    opts.max_prune_records = 3;
    let step = maintenance_commit(f.storage(), base, generation, &opts).expect("m1");
    assert!(
        matches!(step, Step::Done { pruned: 3 }),
        "bounded by records"
    );
    opts.max_prune_records = 1000;
    opts.max_prune_bytes = 1;
    let step = maintenance_commit(f.storage(), generation, generation, &opts).expect("m2");
    assert!(matches!(step, Step::Done { pruned: 1 }), "bounded by bytes");
    // 全体の呼び出しは残りを引き継いで完了する。
    opts.max_prune_bytes = DEFAULT_PRUNE_BYTES;
    opts.max_prune_records = 2;
    let r = f.storage().archive_change_log(&opts).expect("cycle");
    assert_eq!(r.pruned_records, generation - base - 4);
    assert!(f.row_lsns().is_empty());
}

#[test]
fn reenabling_after_pruning_every_row_passes_the_continuity_check() {
    let db_dir = TempDir::new("arc-reenable");
    let archive = TempDir::new("arc-reenable-dir");
    let generation;
    {
        let mut storage = Storage::open(db_dir.db_path()).expect("open");
        storage.enable_change_log().expect("enable");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let ctx = PolicyContext::with_visibilities(TENANT, [Visibility::Public]).expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        for sql in [
            "CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))",
            "INSERT INTO ev (id, embedding) VALUES (1, '[0.1,0.2]')",
        ] {
            core.execute_sql_in_session(&ctx, &mut session, sql)
                .expect("sql");
        }
        let mut opts = ArchiveOptions::new(archive.path());
        opts.retain_records = Some(0);
        let r = core
            .storage_for_test()
            .archive_change_log(&opts)
            .expect("archive");
        assert!(r.pruned_records >= 2);
        generation = core.storage_for_test().current_generation().expect("gen");
        let read = core.storage_for_test().db.begin_read().expect("read");
        assert!(read
            .open_table(CHANGE_LOG_TABLE)
            .expect("table")
            .iter()
            .expect("iter")
            .next()
            .is_none());
    }
    let mut reopened = Storage::open(db_dir.db_path()).expect("reopen");
    assert!(reopened.change_log_required());
    reopened
        .enable_change_log()
        .expect("continuity holds through the archived watermark");
    assert_eq!(reopened.current_generation().expect("gen"), generation);
    // 以降の書き込みも続けられ、アーカイブも続く。
    let core = EngineCore::from_storage(reopened, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::with_visibilities(TENANT, [Visibility::Public]).expect("tenant");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO ev (id, embedding) VALUES (2, '[0.1,0.2]')",
    )
    .expect("insert");
    let r = core
        .storage_for_test()
        .archive_change_log(&ArchiveOptions::new(archive.path()))
        .expect("archive again");
    assert_eq!(r.segments_written, 1);
    core.storage_for_test()
        .verify_change_log_archive(archive.path())
        .expect("verify");
}

#[test]
fn continuity_still_rejects_a_gap_after_pruning() {
    let db_dir = TempDir::new("arc-gap-reenable");
    let archive = TempDir::new("arc-gap-reenable-dir");
    {
        let mut storage = Storage::open(db_dir.db_path()).expect("open");
        storage.enable_change_log().expect("enable");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let ctx = PolicyContext::with_visibilities(TENANT, [Visibility::Public]).expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE TABLE ev (id BIGINT PRIMARY KEY)",
        )
        .expect("sql");
        let mut opts = ArchiveOptions::new(archive.path());
        opts.retain_records = Some(0);
        core.storage_for_test()
            .archive_change_log(&opts)
            .expect("archive");
        // 有効化せずに世代だけを進める書き込み（LSN の欠落）を直接作る。
        let storage = core.storage_for_test();
        let txn = storage.db.begin_write().expect("begin");
        let g = storage.current_generation().expect("gen");
        {
            let mut t = txn.open_table(GENERATION_TABLE).expect("table");
            t.insert(GENERATION_KEY, g + 1).expect("insert");
        }
        txn.commit().expect("commit");
    }
    let mut reopened = Storage::open(db_dir.db_path()).expect("reopen");
    assert!(reopened.enable_change_log().is_err());
}

#[test]
fn maintenance_commit_does_not_disturb_following_user_commits() {
    let f = Fixture::new("arc-after-maint");
    f.populate(2);
    let mut opts = f.opts();
    opts.retain_records = Some(0);
    for i in 0..5 {
        f.storage().archive_change_log(&opts).expect("archive");
        f.sql(&format!(
            "INSERT INTO ev (id, embedding) VALUES ({}, '[0.1,0.2]')",
            50 + i
        ));
    }
    f.storage().archive_change_log(&opts).expect("archive");
    let s = f
        .storage()
        .verify_change_log_archive(f.archive.path())
        .expect("verify");
    assert_eq!(s.last_lsn, Some(f.generation()));
    assert_eq!(s.records, f.generation());
    assert!(f.row_lsns().is_empty());
}

#[test]
fn maintenance_yields_the_writer_gate_instead_of_failing() {
    // 別のスレッドが書き込みトランザクションを保持している間、保守 commit はゲートの待機上限を超えて
    // 見送り（`None`）になる。エラーにはしない（次の呼び出しが再試行する）。
    let db_dir = TempDir::new("arc-skip");
    let mut storage = Storage::open(db_dir.db_path())
        .expect("open")
        .with_write_lock_wait(std::time::Duration::from_millis(50));
    storage.enable_change_log().expect("enable");
    let storage = std::sync::Arc::new(storage);
    let held = storage.begin_write_txn().expect("held writer");
    let other = std::sync::Arc::clone(&storage);
    let skipped = std::thread::spawn(move || other.begin_maintenance_txn().map(|t| t.is_none()))
        .join()
        .expect("join")
        .expect("txn");
    assert!(skipped, "a busy gate is skipped, not an error");
    drop(held);
    assert!(storage.begin_maintenance_txn().expect("txn").is_some());
}

#[test]
fn segment_name_roundtrip_and_validation() {
    let id = [0xab; DB_ID_LEN];
    let name = segment_name(&id, 3, 10, 20);
    let parsed = parse_segment_name(&name).expect("ok").expect("ours");
    assert_eq!(
        parsed,
        SegmentName {
            db_id: id,
            timeline: 3,
            first_lsn: 10,
            last_lsn: 20
        }
    );
    assert!(parse_segment_name("other.txt").expect("ok").is_none());
    for bad in [
        "fandhe-cl-.seg",
        &format!("{FILE_PREFIX}{}-0000000000000003-00000000000000000020-00000000000000000010{FILE_SUFFIX}", hex(&id)),
        &format!("{FILE_PREFIX}{}-0000000000000003-00000000000000000000-00000000000000000010{FILE_SUFFIX}", hex(&id)),
    ] {
        assert!(parse_segment_name(bad).is_err(), "{bad}");
    }
}

#[test]
fn segment_header_rejects_inconsistent_fields() {
    let h = SegmentHeader {
        db_id: [1; DB_ID_LEN],
        timeline: 1,
        first_lsn: 5,
        last_lsn: 6,
        record_count: 2,
        storage_format: 1,
        body_len: 0,
    };
    assert_eq!(SegmentHeader::decode(&h.encode()).expect("decode"), h);
    for bad in [
        SegmentHeader {
            record_count: 3,
            ..h
        },
        SegmentHeader { last_lsn: 4, ..h },
        SegmentHeader {
            first_lsn: 0,
            last_lsn: 1,
            record_count: 2,
            ..h
        },
    ] {
        assert!(SegmentHeader::decode(&bad.encode()).is_err());
    }
    // 版数の違いは拒否する（ヘッダのチェックサムを合わせ直しても）。
    let mut raw = h.encode();
    raw[4] = 9;
    let sum = crate::crypto::sha256::digest(raw.get(..SEG_HEADER_LEN).expect("head"));
    raw[SEG_HEADER_LEN..].copy_from_slice(&sum);
    assert!(matches!(
        SegmentHeader::decode(&raw),
        Err(ArchiveError::BadFormat)
    ));
}

#[test]
fn errors_carry_no_variable_content() {
    for e in [
        ArchiveError::NotEnabled,
        ArchiveError::InvalidOptions,
        ArchiveError::InvalidDirectory,
        ArchiveError::Io(std::io::ErrorKind::NotFound),
        ArchiveError::Unsupported,
        ArchiveError::Truncated,
        ArchiveError::BadFormat,
        ArchiveError::ChecksumMismatch,
        ArchiveError::LsnGap,
        ArchiveError::ForeignDatabase,
        ArchiveError::ForeignTimeline,
        ArchiveError::LogCorrupt,
        ArchiveError::Conflict,
        ArchiveError::WatermarkMoved,
    ] {
        let text = e.to_string();
        assert!(
            !text.replace("i/o", "").contains('/') && !text.chars().any(|c| c.is_ascii_digit()),
            "{text}"
        );
    }
}

#[test]
fn segment_is_cut_at_a_storage_format_boundary() {
    use super::super::{encode_record, RecordBody, RecordHeader, STORAGE_FORMAT_VERSION};
    let f = Fixture::new("arc-format");
    let state = f.storage().change_log.clone().expect("state");
    let rec = |lsn: u64, format: u32| {
        encode_record(
            &RecordHeader {
                db_id: state.db_id,
                timeline: state.timeline,
                lsn,
                prev_lsn: lsn - 1,
                commit_time_us: 0,
                storage_format: format,
            },
            &RecordBody::default(),
            1024,
        )
        .expect("encode")
    };
    let (a, b) = (
        rec(1, STORAGE_FORMAT_VERSION),
        rec(2, STORAGE_FORMAT_VERSION + 1),
    );
    let mut builder = SegmentBuilder::new(&state, 1);
    assert!(
        builder.accepts_format(&a),
        "the first record is always accepted"
    );
    builder.push(1, &a).expect("push");
    assert!(
        !builder.accepts_format(&b),
        "a different version starts a new segment"
    );
    assert!(builder.accepts_format(&rec(2, STORAGE_FORMAT_VERSION)));
}

#[test]
fn a_racing_non_regular_file_is_a_conflict_and_is_never_read() {
    let dir = TempDir::new("arc-fifo");
    let path = dir.path().join("x");
    std::os::unix::fs::symlink("/dev/zero", &path).expect("symlink");
    assert!(!existing_equals(&path, b"abc").expect("no read"));
    std::fs::remove_file(&path).expect("rm");
    std::fs::write(&path, b"abc").expect("write");
    assert!(existing_equals(&path, b"abc").expect("eq"));
    assert!(!existing_equals(&path, b"abd").expect("ne"));
    assert!(!existing_equals(&path, b"ab").expect("len"));
}
