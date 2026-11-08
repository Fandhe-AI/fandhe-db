//! PITR 復元の試験（Issue #2169・RECOVER-14 のポインタ）。
//!
//! 受け入れ条件「指定点までの状態が再現される」を、各 commit 直後に取ったスナップショット（全表の実内容。
//! `oracle::dump_writer`）との両向きの比較で固定する。コミット境界での停止、時刻指定の接頭辞規則
//! （壁時計の逆行を含む）、範囲外・欠落・順序違反・別 DB・別 timeline・改ざんの拒否も確かめる。
//! `cfg(test)` では変更ログが有効な全 commit がオラクルで検証されるため、入力となるログ自体の正しさも
//! ここで間接的に確かめられる。

use std::os::unix::fs::PermissionsExt;

use redb::{ReadableDatabase, ReadableTable};

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::change_log::archive::{ArchiveOptions, SegmentHeader};
use crate::storage::change_log::incremental::ChainPoint;
use crate::storage::change_log::{
    decode_record, encode_record, oracle, CHANGE_LOG_TABLE, META_KEY_TIMELINE, META_TABLE,
};
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::TempDir;

const TENANT: &str = "tenant-a";

/// 書き出し先に使える（他者が書けない）一時ディレクトリ。umask に依らず 0755 にする。
fn private_dir(label: &str) -> TempDir {
    let d = TempDir::new(label);
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    d
}

struct Fixture {
    _db_dir: TempDir,
    out: TempDir,
    core: EngineCore,
    /// 各 commit 直後の (LSN, 全表の内容)。
    snaps: Vec<(u64, oracle::Model)>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let db_dir = TempDir::new(label);
        let mut storage = Storage::open(db_dir.db_path()).expect("open");
        storage.enable_change_log().expect("enable");
        Self {
            _db_dir: db_dir,
            out: private_dir(&format!("{label}-out")),
            core: EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
            snaps: Vec::new(),
        }
    }

    fn storage(&self) -> &Storage {
        self.core.storage_for_test()
    }

    fn generation(&self) -> u64 {
        self.storage().current_generation().expect("generation")
    }

    /// SQL を 1 文実行し、その commit 直後のスナップショットを残す。
    fn step(&mut self, sql: &str) {
        let ctx =
            PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
                .expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        self.core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {:?}", e.wire_code()));
        let model = dump_db(self.storage().db());
        self.snaps.push((self.generation(), model));
    }

    /// 表の作成と 5 件の挿入。
    fn round_1(&mut self) {
        self.step("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        for i in 0..5 {
            self.step(&format!(
                "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
            ));
        }
    }

    /// 別の表の作成・更新・削除・表の削除を含む書き込み群。
    fn round_2(&mut self) {
        self.step("CREATE TABLE ev2 (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        self.step("INSERT INTO ev2 (id, embedding) VALUES (1, '[0.3,0.4]')");
        self.step("INSERT INTO ev (id, embedding) VALUES (10, '[0.5,0.6]')");
        self.step("DELETE FROM ev WHERE id = 2");
        self.step("DROP TABLE ev2");
        self.step("INSERT INTO ev (id, embedding) VALUES (20, '[0.7,0.8]')");
    }

    fn path(&self, name: &str) -> PathBuf {
        self.out.path().join(name)
    }

    fn full_backup(&self, name: &str) -> (PathBuf, ChainPoint) {
        let p = self.path(name);
        self.storage().export_snapshot(&p).expect("snapshot");
        let point = chain_point_of(&p).expect("chain point");
        (p, point)
    }

    /// 未アーカイブの全レコードを、本体 `segment_bytes` ごとのセグメントとして `dir` へ書く。
    fn archive_into(&self, dir: &Path, segment_bytes: usize) {
        let mut opts = ArchiveOptions::new(dir);
        opts.segment_bytes = segment_bytes;
        let report = self.storage().archive_change_log(&opts).expect("archive");
        assert!(!report.skipped);
    }

    /// 各レコードの (LSN, コミット時刻)。
    fn commit_times(&self) -> Vec<(u64, i64)> {
        let read = self.storage().db().begin_read().expect("read");
        let t = read.open_table(CHANGE_LOG_TABLE).expect("table");
        t.iter()
            .expect("iter")
            .map(|e| {
                let (k, v) = e.expect("row");
                let h = peek_record(v.value(), usize::MAX >> 8).expect("peek");
                (k.value(), h.commit_time_us)
            })
            .collect()
    }

    fn snapshot_at(&self, lsn: u64) -> &oracle::Model {
        &self
            .snaps
            .iter()
            .find(|(l, _)| *l == lsn)
            .unwrap_or_else(|| panic!("no snapshot at {lsn}"))
            .1
    }
}

fn dump_db(db: &redb::Database) -> oracle::Model {
    let txn = db.begin_write().expect("begin_write");
    let m = oracle::dump_writer(&txn).expect("dump");
    txn.abort().expect("abort");
    m
}

fn copy_to(src: &Path, dir: &TempDir, name: &str) -> PathBuf {
    let dst = dir.path().join(name);
    std::fs::copy(src, &dst).expect("copy");
    dst
}

fn flip_byte(path: &Path, offset_from_end: usize) {
    let mut bytes = std::fs::read(path).expect("read");
    let i = bytes.len() - offset_from_end;
    bytes[i] ^= 0xff;
    std::fs::write(path, bytes).expect("write");
}

/// ベースの複製へ PITR を適用する。成功時は復元後のパスと結果を返す。
fn restore(
    base: &Path,
    incs: &[PathBuf],
    archive: Option<&Path>,
    target: PitrTarget,
) -> (TempDir, PathBuf, Result<ApplySummary>) {
    let work = TempDir::new("pitr-work");
    let db = copy_to(base, &work, "restored.redb");
    let r = restore_to_point(&db, incs, archive, target);
    (work, db, r)
}

/// 復元後の DB が、世代 `lsn` の時点のスナップショットと全表一致し、`change_log` の連続性も保つこと。
fn assert_state_at(fx: &Fixture, restored: &Path, lsn: u64) {
    let db = redb::Database::open(restored).expect("open restored");
    let gen = {
        let read = db.begin_read().expect("read");
        crate::storage::current_generation_in_txn(&read).expect("gen")
    };
    assert_eq!(gen, lsn);
    let got = dump_db(&db);
    let want = fx.snapshot_at(lsn);
    assert_eq!(oracle::diff_models(want, &got), None, "state at {lsn}");
    assert_eq!(oracle::diff_models(&got, want), None, "state at {lsn}");
    let read = db.begin_read().expect("read");
    let t = read.open_table(CHANGE_LOG_TABLE).expect("log");
    assert_eq!(t.last().expect("last").map(|(k, _)| k.value()), Some(lsn));
}

/// ベース（round_1 の後）→ round_2 → アーカイブ、の定型。ベースの終点はセグメントの途中に落ちる。
struct Scenario {
    fx: Fixture,
    base: PathBuf,
    base_point: ChainPoint,
    archive: TempDir,
}

fn scenario(label: &str, segment_bytes: usize) -> Scenario {
    let mut fx = Fixture::new(label);
    fx.round_1();
    let (base, base_point) = fx.full_backup("base.redb");
    fx.round_2();
    let archive = private_dir(&format!("{label}-arc"));
    fx.archive_into(archive.path(), segment_bytes);
    Scenario {
        fx,
        base,
        base_point,
        archive,
    }
}

#[test]
fn every_lsn_after_the_base_reproduces_its_snapshot() {
    // 1 つの大きいセグメント（ベースの終点が途中）と、1 レコード 1 セグメントの両方。
    for (label, seg_bytes) in [("pitr-lsn-one", usize::MAX >> 8), ("pitr-lsn-many", 1)] {
        let s = scenario(label, seg_bytes);
        let end = s.base_point.end_lsn;
        let mut checked = 0;
        for (lsn, _) in s.fx.snaps.iter().filter(|(l, _)| *l > end) {
            let (_w, db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(*lsn));
            let sum = r.unwrap_or_else(|e| panic!("{label} lsn {lsn}: {e}"));
            assert_eq!(sum.last_lsn, *lsn);
            assert_eq!(sum.records, lsn - end);
            assert_state_at(&s.fx, &db, *lsn);
            checked += 1;
        }
        assert_eq!(checked, 6);
    }
}

#[test]
fn restored_database_continues_with_the_change_log_enabled() {
    let s = scenario("pitr-reopen", usize::MAX >> 8);
    let stop = s.base_point.end_lsn + 3;
    let (_w, db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(stop));
    r.expect("restore");
    let mut st = Storage::open(&db).expect("reopen");
    st.enable_change_log().expect("continuity holds");
    assert_eq!(st.current_generation().expect("gen"), stop);
}

/// RECOVER-17（Issue #2450）・Issue #3100: PITR の後に復元の世代を進めても、全表の状態・LSN は変わらない。
/// 世代は単調に増え、変更ログの timeline は分岐する（本体表は空になり起点が分岐点へ移る）。
#[test]
fn recover17_advancing_the_generation_after_pitr_keeps_the_state_and_lsn() {
    let s = scenario("pitr-generation", usize::MAX >> 8);
    let stop = s.base_point.end_lsn + 3;
    let mut previous = 0;
    for _ in 0..2 {
        let (_w, db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(stop));
        r.expect("restore");
        // 状態の比較は世代を進める前に行う（進めた後は本体表が空になり、末尾 == stop を満たさない）。
        assert_state_at(&s.fx, &db, stop);
        let old_timeline = chain_point_of(&db).expect("point").timeline;
        {
            let mut st = Storage::open(&db).expect("open");
            let g = st.advance_restore_generation().expect("advance");
            assert!(g > 0);
            assert_eq!(st.restore_generation().expect("read"), g);
        }
        let forked = chain_point_of(&db).expect("forked point");
        assert!(forked.timeline > old_timeline, "the timeline must fork");
        assert_eq!(forked.end_lsn, stop, "the LSN is unchanged");
        let mut st = Storage::open(&db).expect("reopen");
        st.enable_change_log().expect("continuity holds");
        assert_eq!(st.current_generation().expect("gen"), stop);
        let g = st.restore_generation().expect("read");
        assert!(g > previous, "{g} must exceed {previous}");
        previous = g;
    }
}

#[test]
fn time_target_stops_before_the_first_later_commit() {
    let s = scenario("pitr-time", 1);
    let times = s.fx.commit_times();
    let end = s.base_point.end_lsn;
    for (lsn, t) in times.iter().filter(|(l, _)| *l > end) {
        // 期待: T を超える最初のレコードの直前（無ければ範囲外）。
        let first_later = times.iter().find(|(l, ct)| *l > end && *ct > *t);
        let (_w, db, r) = restore(
            &s.base,
            &[],
            Some(s.archive.path()),
            PitrTarget::TimeMicros(*t),
        );
        match first_later {
            Some((l, _)) => {
                let sum = r.unwrap_or_else(|e| panic!("time of {lsn}: {e}"));
                assert_eq!(sum.last_lsn, l - 1);
                assert!(sum.last_lsn >= *lsn);
                assert_state_at(&s.fx, &db, l - 1);
            }
            None => assert!(matches!(r, Err(IncrementalError::TargetOutOfRange))),
        }
    }
    // 最初の適用候補の直前の時刻は、ベース以前になるので範囲外。
    let first = times.iter().find(|(l, _)| *l > end).expect("first").1;
    let (_w, _db, r) = restore(
        &s.base,
        &[],
        Some(s.archive.path()),
        PitrTarget::TimeMicros(first - 1),
    );
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
    let (_w, _db, r) = restore(
        &s.base,
        &[],
        Some(s.archive.path()),
        PitrTarget::TimeMicros(i64::MIN),
    );
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
    // 最後のレコードの時刻以降は、末尾より後（超えるレコードが無い）なので範囲外。
    let (_w, _db, r) = restore(
        &s.base,
        &[],
        Some(s.archive.path()),
        PitrTarget::TimeMicros(i64::MAX),
    );
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
}

/// `src` のセグメントのレコードのコミット時刻を `times`（LSN 順。先頭から順に）へ付け替えた
/// セグメントを `dst` に書く（レコード・全体のチェックサムは計算し直す）。
fn retime_segment(src: &Path, dst: &Path, times: &[i64]) {
    let max = CaptureLimits::default().max_payload_bytes;
    let mut reader = SegmentReader::open(src, max).expect("open");
    let mut header: SegmentHeader = *reader.header();
    let mut body = Vec::new();
    let mut i = 0usize;
    while let Some(raw) = reader.next_record().expect("next") {
        let mut rec = decode_record(&raw, max).expect("decode");
        rec.header.commit_time_us = times[i];
        i += 1;
        body.extend(encode_record(&rec.header, &rec.body, usize::MAX >> 8).expect("encode"));
    }
    header.body_len = body.len() as u64;
    let mut file = header.encode().to_vec();
    file.extend(body);
    let sum = crate::crypto::sha256::digest(&file);
    file.extend(sum);
    std::fs::write(dst, file).expect("write");
}

#[test]
fn time_target_is_a_prefix_even_when_the_wall_clock_goes_backwards() {
    let s = scenario("pitr-clock", usize::MAX >> 8);
    let seg_name = std::fs::read_dir(s.archive.path())
        .expect("dir")
        .next()
        .expect("one segment")
        .expect("entry")
        .file_name();
    let seg = s.archive.path().join(&seg_name);
    let total = s.fx.generation() as usize;
    // LSN 1.. の時刻: 単調増加だが、ベースの次から 3 番目のレコードだけ巻き戻る。
    let end = s.base_point.end_lsn as usize;
    let mut times: Vec<i64> = (0..total as i64).map(|i| 1_000 + i * 100).collect();
    times[end + 2] = 1_000; // LSN end+3 が過去の時刻
                            // ベースと重なる区間は本物の時刻のまま
                            // （重なり区間の内容がベースの履歴と食い違えば拒否されるため）。
    for (i, (_, t)) in s.fx.commit_times().iter().take(end).enumerate() {
        times[i] = *t;
    }
    let forged_dir = private_dir("pitr-clock-forged");
    let forged = forged_dir.path().join(&seg_name);
    retime_segment(&seg, &forged, &times);

    // T = 時刻 times[end+1] 以上 times[end+3] 未満: 巻き戻った end+3 は T 以下だが、
    // end+2 が既に T を超えているので、end+1 の直後で止まる（接頭辞）。
    let t = times[end] + 50; // end+1 の時刻 (times[end]) は T 以下、end+2 は T 超
    let (_w, db, r) = restore(
        &s.base,
        &[],
        Some(forged_dir.path()),
        PitrTarget::TimeMicros(t),
    );
    let sum = r.expect("restore");
    assert_eq!(sum.last_lsn, s.base_point.end_lsn + 1);
    assert_state_at(&s.fx, &db, s.base_point.end_lsn + 1);
}

#[test]
fn targets_outside_the_covered_range_are_rejected() {
    let s = scenario("pitr-range", 1);
    let end = s.base_point.end_lsn;
    let last = s.fx.generation();
    for lsn in [1, end - 1, end, last + 1, u64::MAX] {
        let (_w, _db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(lsn));
        assert!(
            matches!(r, Err(IncrementalError::TargetOutOfRange)),
            "lsn {lsn}"
        );
    }
    let (_w, _db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(0));
    assert!(matches!(r, Err(IncrementalError::InvalidTarget)));
    // 入力が無い。
    let (_w, _db, r) = restore(&s.base, &[], None, PitrTarget::Lsn(last));
    assert!(matches!(r, Err(IncrementalError::BadSegmentList)));
    // 空のアーカイブ。
    let empty = private_dir("pitr-range-empty");
    let (_w, _db, r) = restore(&s.base, &[], Some(empty.path()), PitrTarget::Lsn(last));
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
    // 末尾まで（最後の LSN）は通る。
    let (_w, db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(last));
    r.expect("to the end");
    assert_state_at(&s.fx, &db, last);
}

#[test]
fn incrementals_and_archive_combine_and_the_target_may_fall_inside_an_incremental() {
    let mut fx = Fixture::new("pitr-combo");
    fx.round_1();
    let (base, base_point) = fx.full_backup("base.redb");
    fx.step("INSERT INTO ev (id, embedding) VALUES (30, '[0.1,0.1]')");
    fx.step("INSERT INTO ev (id, embedding) VALUES (31, '[0.1,0.1]')");
    let inc = fx.path("inc.seg");
    fx.storage()
        .export_incremental(&base_point, &inc)
        .expect("incremental");
    let inc_end = fx.generation();
    fx.round_2();
    let archive = private_dir("pitr-combo-arc");
    fx.archive_into(archive.path(), 1);
    let last = fx.generation();

    // 増分の内側、増分の末尾、アーカイブの内側、末尾。
    for lsn in [inc_end - 1, inc_end, inc_end + 2, last] {
        let (_w, db, r) = restore(
            &base,
            std::slice::from_ref(&inc),
            Some(archive.path()),
            PitrTarget::Lsn(lsn),
        );
        let sum = r.unwrap_or_else(|e| panic!("lsn {lsn}: {e}"));
        assert_eq!(sum.last_lsn, lsn);
        assert_state_at(&fx, &db, lsn);
    }
    // 増分だけ（アーカイブなし）。内側は通り、末尾より後ろは拒否。
    let (_w, db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        None,
        PitrTarget::Lsn(inc_end - 1),
    );
    r.expect("inside the incremental");
    assert_state_at(&fx, &db, inc_end - 1);
    let (_w, _db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        None,
        PitrTarget::Lsn(inc_end + 1),
    );
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
    // ベースの終点以前は、増分があっても範囲外。
    let (_w, _db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        Some(archive.path()),
        PitrTarget::Lsn(base_point.end_lsn),
    );
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
}

#[test]
fn missing_or_misordered_segments_are_rejected() {
    let s = scenario("pitr-gap", 1);
    let end = s.base_point.end_lsn;
    let last = s.fx.generation();
    let mut files: Vec<PathBuf> = std::fs::read_dir(s.archive.path())
        .expect("dir")
        .map(|e| e.expect("entry").path())
        .collect();
    files.sort();

    // 途中のセグメントの欠落（飛び）。停止点より後ろの欠落でも拒否される。
    let mid = files[files.len() / 2].clone();
    let backup = s.fx.path("moved.seg");
    std::fs::rename(&mid, &backup).expect("move");
    for target in [last, end + 1] {
        let (_w, _db, r) = restore(
            &s.base,
            &[],
            Some(s.archive.path()),
            PitrTarget::Lsn(target),
        );
        assert!(
            matches!(r, Err(IncrementalError::Segment(ArchiveError::LsnGap))),
            "target {target}"
        );
    }
    std::fs::rename(&backup, &mid).expect("restore file");

    // ベースの次（LSN end+1）までを覆うセグメントが無い（先頭側の欠落。アーカイブの先頭が後ろにずれる）。
    // 1 レコード 1 セグメントなので、ファイル名順の先頭 end+1 個が LSN 1..=end+1。
    let held = private_dir("pitr-gap-held");
    let head: Vec<PathBuf> = files[..=end as usize].to_vec();
    for p in &head {
        std::fs::rename(p, held.path().join(p.file_name().expect("name"))).expect("move");
    }
    let (_w, _db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(last));
    assert!(matches!(r, Err(IncrementalError::ChainBroken)));
    for p in &head {
        std::fs::rename(held.path().join(p.file_name().expect("name")), p).expect("restore file");
    }

    // 増分の順序違反（逆順）。
    let mut fx2 = Fixture::new("pitr-order");
    fx2.round_1();
    let (base2, p2) = fx2.full_backup("base.redb");
    fx2.step("INSERT INTO ev (id, embedding) VALUES (40, '[0.1,0.1]')");
    let inc1 = fx2.path("inc1.seg");
    fx2.storage().export_incremental(&p2, &inc1).expect("inc1");
    let p2b = chain_point_of(&inc1).expect("point");
    fx2.step("INSERT INTO ev (id, embedding) VALUES (41, '[0.1,0.1]')");
    let inc2 = fx2.path("inc2.seg");
    fx2.storage().export_incremental(&p2b, &inc2).expect("inc2");
    let (_w, _db, r) = restore(
        &base2,
        &[inc2.clone(), inc1.clone()],
        None,
        PitrTarget::Lsn(fx2.generation()),
    );
    assert!(matches!(r, Err(IncrementalError::ChainBroken)));
    // 増分の欠落（inc1 を飛ばす）。
    let (_w, _db, r) = restore(&base2, &[inc2], None, PitrTarget::Lsn(fx2.generation()));
    assert!(matches!(r, Err(IncrementalError::ChainBroken)));
}

#[test]
fn foreign_database_foreign_timeline_and_tampering_are_rejected() {
    let s = scenario("pitr-foreign", 1);
    let last = s.fx.generation();

    // 別 DB のベース。
    let mut other = Fixture::new("pitr-foreign-other");
    other.round_1();
    let (other_base, _) = other.full_backup("base.redb");
    let (_w, _db, r) = restore(
        &other_base,
        &[],
        Some(s.archive.path()),
        PitrTarget::Lsn(last),
    );
    assert!(matches!(r, Err(IncrementalError::ForeignDatabase)));

    // 別 timeline のベース。
    let work = TempDir::new("pitr-foreign-tl");
    let forked = copy_to(&s.base, &work, "forked.redb");
    {
        let db = redb::Database::open(&forked).expect("open");
        let txn = db.begin_write().expect("write");
        {
            let mut m = txn.open_table(META_TABLE).expect("meta");
            m.insert(META_KEY_TIMELINE, 7u64.to_le_bytes().as_slice())
                .expect("insert");
        }
        txn.commit().expect("commit");
    }
    let r = restore_to_point(&forked, &[], Some(s.archive.path()), PitrTarget::Lsn(last));
    assert!(matches!(r, Err(IncrementalError::ForeignTimeline)));

    // 改ざん: 使うセグメントと、停止点より後ろのセグメントのどちらも拒否される。
    let mut files: Vec<PathBuf> = std::fs::read_dir(s.archive.path())
        .expect("dir")
        .map(|e| e.expect("entry").path())
        .collect();
    files.sort();
    for (idx, target) in [(1usize, last), (files.len() - 1, s.base_point.end_lsn + 1)] {
        let victim = files[idx].clone();
        let original = std::fs::read(&victim).expect("read");
        for offset in [1usize, 40, 200] {
            flip_byte(&victim, offset);
            let (_w, _db, r) = restore(
                &s.base,
                &[],
                Some(s.archive.path()),
                PitrTarget::Lsn(target),
            );
            assert!(
                matches!(r, Err(IncrementalError::Segment(_))),
                "segment {idx} offset {offset}"
            );
            std::fs::write(&victim, &original).expect("restore bytes");
        }
    }
    // 元に戻せば通る（改ざん以外に原因がない）。
    let (_w, db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(last));
    r.expect("untampered");
    assert_state_at(&s.fx, &db, last);
}

#[test]
fn shared_archive_directory_is_rejected() {
    let s = scenario("pitr-shared", usize::MAX >> 8);
    std::fs::set_permissions(s.archive.path(), std::fs::Permissions::from_mode(0o777))
        .expect("chmod");
    let (_w, _db, r) = restore(
        &s.base,
        &[],
        Some(s.archive.path()),
        PitrTarget::Lsn(s.fx.generation()),
    );
    assert!(matches!(r, Err(IncrementalError::InsecureDirectory)));
}

#[test]
fn explicit_transaction_is_applied_or_skipped_as_a_whole() {
    // 1 つの明示 Tx（複数文）は 1 commit = 1 レコード。直前の LSN では全く見えず、その LSN では全部見える。
    let mut fx = Fixture::new("pitr-tx");
    fx.round_1();
    let (base, base_point) = fx.full_backup("base.redb");
    let ctx = PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
        .expect("tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    let mut txn = fx.core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO ev (id, embedding) VALUES (50, '[0.1,0.1]')",
        "INSERT INTO ev (id, embedding) VALUES (51, '[0.1,0.1]')",
        "COMMIT",
    ] {
        fx.core
            .execute_sql_in_txn(&ctx, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {:?}", e.wire_code()));
    }
    let after_tx = fx.generation();
    assert_eq!(
        after_tx,
        base_point.end_lsn + 1,
        "one commit for the whole transaction"
    );
    let tx_model = dump_db(fx.storage().db());
    fx.snaps.push((after_tx, tx_model));
    fx.step("INSERT INTO ev (id, embedding) VALUES (52, '[0.1,0.1]')");
    let archive = private_dir("pitr-tx-arc");
    fx.archive_into(archive.path(), usize::MAX >> 8);

    let (_w, db, r) = restore(&base, &[], Some(archive.path()), PitrTarget::Lsn(after_tx));
    r.expect("restore");
    assert_state_at(&fx, &db, after_tx);
    // Tx の前（= ベースそのもの）は範囲外。
    let (_w, _db, r) = restore(
        &base,
        &[],
        Some(archive.path()),
        PitrTarget::Lsn(after_tx - 1),
    );
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
}

/// 唯一のアーカイブのセグメントの全レコードのコミット時刻を `time` に付け替えた別ディレクトリを作る。
/// チェックサムは正しいまま、重なり区間の内容だけが適用済みの履歴と食い違う（分岐した履歴の混在）。
fn diverged_archive(archive: &Path, label: &str, time: i64) -> TempDir {
    let seg = std::fs::read_dir(archive)
        .expect("dir")
        .next()
        .expect("one segment")
        .expect("entry")
        .path();
    let count = {
        let r = SegmentReader::open(&seg, usize::MAX >> 8).expect("open");
        r.header().record_count as usize
    };
    let dir = private_dir(label);
    retime_segment(
        &seg,
        &dir.path().join(seg.file_name().expect("name")),
        &vec![time; count],
    );
    dir
}

#[test]
fn overlap_with_the_base_must_match_the_applied_history() {
    let s = scenario("pitr-overlap-base", usize::MAX >> 8);
    let last = s.fx.generation();
    // 同じ内容のアーカイブは通る（対照）。
    let (_w, _db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(last));
    r.expect("same history");
    // ベースの終点以下のレコードの内容が食い違うアーカイブは、成功として公開しない。
    let bad = diverged_archive(s.archive.path(), "pitr-overlap-base-bad", 5);
    let (_w, _db, r) = restore(&s.base, &[], Some(bad.path()), PitrTarget::Lsn(last));
    assert!(matches!(r, Err(IncrementalError::ChainBroken)));
    let (_w, _db, r) = restore(
        &s.base,
        &[],
        Some(bad.path()),
        PitrTarget::TimeMicros(i64::MAX),
    );
    assert!(matches!(r, Err(IncrementalError::TargetOutOfRange)));
}

#[test]
fn overlap_with_an_incremental_must_match_the_applied_history() {
    let mut fx = Fixture::new("pitr-overlap-inc");
    fx.round_1();
    let (base, base_point) = fx.full_backup("base.redb");
    fx.step("INSERT INTO ev (id, embedding) VALUES (30, '[0.1,0.1]')");
    fx.step("INSERT INTO ev (id, embedding) VALUES (31, '[0.1,0.1]')");
    let inc = fx.path("inc.seg");
    fx.storage()
        .export_incremental(&base_point, &inc)
        .expect("incremental");
    fx.round_2();
    let archive = private_dir("pitr-overlap-inc-arc");
    fx.archive_into(archive.path(), usize::MAX >> 8);
    let last = fx.generation();

    let (_w, db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        Some(archive.path()),
        PitrTarget::Lsn(last),
    );
    r.expect("same history");
    assert_state_at(&fx, &db, last);

    let bad = diverged_archive(archive.path(), "pitr-overlap-inc-bad", 5);
    let (_w, _db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        Some(bad.path()),
        PitrTarget::Lsn(last),
    );
    assert!(matches!(r, Err(IncrementalError::ChainBroken)));
}

#[test]
fn time_resolution_rejects_segments_rewritten_after_verification() {
    // 1 レコード 1 セグメント。検証（ヘッダ・トレーラの取得）の後に、ヘッダを変えずにコミット時刻と
    // チェックサムを書き換えたセグメントは、時刻解決で読み切って全体チェックサムを照合し、拒否する。
    let s = scenario("pitr-resolve-swap", 1);
    let end = s.base_point.end_lsn;
    let expect = ArchiveExpectation {
        db_id: Some(s.base_point.db_id),
        timeline: Some(s.base_point.timeline),
        start_lsn: None,
    };
    let (_, verified) =
        crate::storage::change_log::archive::verified_segments(s.archive.path(), &expect)
            .expect("verified");
    let segs: Vec<ReplaySegment> = verified
        .into_iter()
        .filter(|(_, i)| i.header.last_lsn > end)
        .map(|(path, info)| ReplaySegment {
            path,
            info,
            overlap_ok: true,
        })
        .collect();
    let refs: Vec<&ReplaySegment> = segs.iter().collect();
    let max = CaptureLimits::default().max_payload_bytes;
    let times = s.fx.commit_times();
    let t_of = |lsn: u64| times.iter().find(|(l, _)| *l == lsn).expect("lsn").1;

    // 書き換え前は、LSN end+2 の時刻を超える最初のレコード（end+3）の直前で止まる。
    let before = resolve_time(&refs, end, t_of(end + 2), max).expect("resolve");
    assert_eq!(before, end + 2);

    // LSN end+2 のセグメントの時刻を、ヘッダ・チェックサムを整えたまま極端に大きくする。判定上は「このレコードが
    // 最初に T を超える」ことになり、停止位置は 1 つ前のセグメントの末尾（end+1）になる。
    // 書き換え後の内容は検証時のトレーラと一致しないので、読み切って照合し、拒否する。
    let victim = &segs[1];
    let forged_dir = private_dir("pitr-resolve-swap-forged");
    let forged = forged_dir.path().join("forged.seg");
    retime_segment(&victim.path, &forged, &[i64::MAX]);
    std::fs::copy(&forged, &victim.path).expect("overwrite");
    let r = resolve_time(&refs, end, t_of(end + 2), max);
    assert!(
        matches!(
            r,
            Err(IncrementalError::Segment(ArchiveError::ChecksumMismatch))
        ),
        "{r:?}"
    );
}

/// アーカイブのディレクトリのうち、LSN `lsn` だけを含む（1 レコード 1 セグメント）セグメントの内容を、
/// 時刻だけ付け替えたもの（チェックサムは正しい）に置き換える。
fn diverge_one_segment(archive: &Path, lsn: u64) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(archive)
        .expect("dir")
        .map(|e| e.expect("entry").path())
        .collect();
    files.sort();
    let victim = files[(lsn - 1) as usize].clone();
    let tmp = private_dir("pitr-diverge-one");
    let forged = tmp.path().join("forged.seg");
    retime_segment(&victim, &forged, &[5]);
    std::fs::copy(&forged, &victim).expect("overwrite");
}

#[test]
fn segments_fully_inside_the_base_are_compared_regardless_of_the_split() {
    let s = scenario("pitr-split-base", 1);
    let last = s.fx.generation();
    // 対照: 同じ履歴なら通る。
    let (_w, _db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(last));
    r.expect("same history");
    // ベースの内側(LSN 2)だけを含むセグメントの内容を食い違わせると、1 つの大きなセグメントのときと
    // 同じく拒否される。
    diverge_one_segment(s.archive.path(), 2);
    let (_w, _db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(last));
    assert!(matches!(r, Err(IncrementalError::ChainBroken)), "{r:?}");
    // 時刻指定でも同じ（時刻の解決は通っても、再生が拒否する）。
    let (_w, _db, r) = restore(
        &s.base,
        &[],
        Some(s.archive.path()),
        PitrTarget::TimeMicros(i64::MAX),
    );
    assert!(r.is_err());
}

#[test]
fn segments_fully_inside_an_incremental_are_compared_regardless_of_the_split() {
    let mut fx = Fixture::new("pitr-split-inc");
    fx.round_1();
    let (base, base_point) = fx.full_backup("base.redb");
    fx.step("INSERT INTO ev (id, embedding) VALUES (30, '[0.1,0.1]')");
    fx.step("INSERT INTO ev (id, embedding) VALUES (31, '[0.1,0.1]')");
    let inc = fx.path("inc.seg");
    fx.storage()
        .export_incremental(&base_point, &inc)
        .expect("incremental");
    fx.round_2();
    let archive = private_dir("pitr-split-inc-arc");
    fx.archive_into(archive.path(), 1);
    let last = fx.generation();
    let (_w, db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        Some(archive.path()),
        PitrTarget::Lsn(last),
    );
    r.expect("same history");
    assert_state_at(&fx, &db, last);
    // 増分の範囲(LSN base+1)に完全に収まるアーカイブのセグメントを食い違わせる。
    diverge_one_segment(archive.path(), base_point.end_lsn + 1);
    let (_w, _db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        Some(archive.path()),
        PitrTarget::Lsn(last),
    );
    assert!(matches!(r, Err(IncrementalError::ChainBroken)), "{r:?}");
}

/// ベースの終点 B、増分が B+1..B+8、アーカイブが 1..B+4(増分より短い)の構成。
fn short_archive_with_longer_incremental(
    label: &str,
) -> (Fixture, PathBuf, ChainPoint, PathBuf, TempDir) {
    let mut fx = Fixture::new(label);
    fx.round_1();
    let (base, base_point) = fx.full_backup("base.redb");
    for i in 0..4 {
        fx.step(&format!(
            "INSERT INTO ev (id, embedding) VALUES ({}, '[0.1,0.1]')",
            40 + i
        ));
    }
    let archive = private_dir(&format!("{label}-arc"));
    fx.archive_into(archive.path(), 1);
    for i in 4..8 {
        fx.step(&format!(
            "INSERT INTO ev (id, embedding) VALUES ({}, '[0.1,0.1]')",
            40 + i
        ));
    }
    let inc = fx.path("inc.seg");
    fx.storage()
        .export_incremental(&base_point, &inc)
        .expect("incremental");
    (fx, base, base_point, inc, archive)
}

#[test]
fn target_inside_a_longer_incremental_is_not_limited_by_a_shorter_archive() {
    let (fx, base, p, inc, archive) = short_archive_with_longer_incremental("pitr-longer-inc");
    let target = p.end_lsn + 7;
    // 増分だけで復元できる目標は、アーカイブを併用しても復元できる(入力を足して契約が変わらない)。
    let (_w, db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        None,
        PitrTarget::Lsn(target),
    );
    r.expect("incremental only");
    assert_state_at(&fx, &db, target);
    let (_w, db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        Some(archive.path()),
        PitrTarget::Lsn(target),
    );
    r.unwrap_or_else(|e| panic!("with archive: {e}"));
    assert_state_at(&fx, &db, target);
    // 時刻指定でも同じ。
    let times = fx.commit_times();
    let t = times
        .iter()
        .find(|(l, _)| *l == target)
        .expect("target time")
        .1;
    let next = times.iter().find(|(_, ct)| *ct > t);
    if let Some((l, _)) = next {
        let (_w, db, r) = restore(
            &base,
            std::slice::from_ref(&inc),
            Some(archive.path()),
            PitrTarget::TimeMicros(t),
        );
        r.expect("time");
        assert_state_at(&fx, &db, l - 1);
    }
}

#[test]
fn archive_inside_an_incremental_is_compared_even_when_the_target_stops_in_the_incremental() {
    let (fx, base, p, inc, archive) = short_archive_with_longer_incremental("pitr-longer-inc-bad");
    // 増分の内側(B+2)に収まるアーカイブのセグメントを食い違わせる。目標は増分の途中(B+7)。
    diverge_one_segment(archive.path(), p.end_lsn + 2);
    let (_w, _db, r) = restore(
        &base,
        std::slice::from_ref(&inc),
        Some(archive.path()),
        PitrTarget::Lsn(p.end_lsn + 7),
    );
    assert!(matches!(r, Err(IncrementalError::ChainBroken)), "{r:?}");
    let _ = fx;
}
/// Issue #3100（RECOVER-14）: 復元して世代を進めた DB（timeline が分岐済み）を基準に、元の系統の
/// アーカイブで PITR を進めると、別系統の履歴が混ざるため拒否され、DB の内容は変わらない。
#[test]
fn fork_restored_base_rejects_the_original_archive_without_touching_the_db() {
    let s = scenario("pitr-fork", usize::MAX >> 8);
    let stop = s.base_point.end_lsn + 2;
    let (w, db, r) = restore(&s.base, &[], Some(s.archive.path()), PitrTarget::Lsn(stop));
    r.expect("restore");
    {
        let mut st = Storage::open(&db).expect("open");
        st.advance_restore_generation().expect("advance");
    }
    let before = dump_db(&redb::Database::open(&db).expect("open raw"));
    let r = restore_to_point(&db, &[], Some(s.archive.path()), PitrTarget::Lsn(stop + 1));
    assert!(r.is_err(), "mixed lineages must be refused: {r:?}");
    let after = dump_db(&redb::Database::open(&db).expect("open raw"));
    assert_eq!(oracle::diff_models(&before, &after), None);
    drop(w);
}
