//! 増分バックアップの出力・連鎖検証・適用の試験（Issue #2168・RECOVER-13・RECOVER-14 のポインタ）。
//!
//! 受け入れ条件「ベース＋増分を復元すると最新と一致する」（全表の実内容を `oracle::dump_writer` で比べる）、
//! 連鎖の切断・順序違反・別 DB・別 timeline・改ざん・刈り込み済み区間・上書き・権限の拒否を固定する。
//! `cfg(test)` では変更ログが有効な全 commit がオラクルで検証されるため、適用の入力となるログ自体の
//! 正しさもここで間接的に確かめられる。

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::change_log::oracle;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::TempDir;

const TENANT: &str = "tenant-a";

/// 手で組んだセグメントの権限を 0600 にする（読み取り器は他者が書けるファイルを拒否する。umask に依らせない）。
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

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

    fn path(&self, name: &str) -> PathBuf {
        self.out.path().join(name)
    }

    /// フルバックアップを `name` へ書き出し、その終点を返す。
    fn full_backup(&self, name: &str) -> (PathBuf, ChainPoint) {
        let p = self.path(name);
        self.storage().export_snapshot(&p).expect("snapshot");
        let point = chain_point_of(&p).expect("chain point");
        (p, point)
    }

    /// `base` の終点からの増分を `name` へ書き出す。
    fn increment(&self, base: &ChainPoint, name: &str) -> PathBuf {
        let p = self.path(name);
        self.storage().export_incremental(base, &p).expect("export");
        p
    }

    fn generation(&self) -> u64 {
        self.storage().current_generation().expect("generation")
    }

    /// 最初の書き込み群（表・行）。
    fn write_round_1(&self) {
        self.sql("CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        for i in 0..5 {
            self.sql(&format!(
                "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
            ));
        }
    }

    /// 2 番目の書き込み群（別の表の作成・更新・削除）。
    fn write_round_2(&self) {
        self.sql("CREATE TABLE ev2 (id BIGINT PRIMARY KEY, embedding VECTOR(2))");
        self.sql("INSERT INTO ev2 (id, embedding) VALUES (1, '[0.3,0.4]')");
        self.sql("INSERT INTO ev (id, embedding) VALUES (10, '[0.5,0.6]')");
        self.sql("DELETE FROM ev WHERE id = 2");
    }

    /// 3 番目の書き込み群（表の削除を含む）。
    fn write_round_3(&self) {
        self.sql("DROP TABLE ev2");
        self.sql("INSERT INTO ev (id, embedding) VALUES (20, '[0.7,0.8]')");
    }
}

/// 全表（変更ログの内部表を除く）の内容。
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

/// 適用して、最新の DB と全表が一致することを確かめる。
fn assert_matches_latest(fx: &Fixture, restored: &Path) {
    let db = redb::Database::open(restored).expect("open restored");
    let restored_gen = {
        let read = db.begin_read().expect("read");
        crate::storage::current_generation_in_txn(&read).expect("gen")
    };
    assert_eq!(restored_gen, fx.generation());
    let live = dump_db(fx.storage().db());
    let got = dump_db(&db);
    assert_eq!(oracle::diff_models(&live, &got), None);
    assert_eq!(oracle::diff_models(&got, &live), None);
}

#[test]
fn base_plus_increments_reproduce_the_latest_database() {
    let fx = Fixture::new("inc-chain");
    fx.write_round_1();
    let (base, base_point) = fx.full_backup("base.redb");
    assert_eq!(base_point.end_lsn, fx.generation());

    fx.write_round_2();
    let inc1 = fx.increment(&base_point, "inc1.seg");
    let p1 = chain_point_of(&inc1).expect("inc1 point");
    assert_eq!(p1.db_id, base_point.db_id);
    assert_eq!(p1.timeline, base_point.timeline);
    assert_eq!(p1.end_lsn, fx.generation());

    fx.write_round_3();
    // 前回の増分をベースにした次の増分（連鎖）。
    let inc2 = fx.increment(&p1, "inc2.seg");

    let work = TempDir::new("inc-chain-work");
    let restored = copy_to(&base, &work, "restored.redb");
    let summary =
        apply_incremental_segments(&restored, &[inc1.clone(), inc2.clone()]).expect("apply");
    assert_eq!(summary.segments, 2);
    assert_eq!(summary.last_lsn, fx.generation());
    assert_matches_latest(&fx, &restored);

    // 1 つの増分にまとめた場合（ベース直後から最新まで）も同じ結果になる。
    let whole = fx.increment(&base_point, "whole.seg");
    let restored2 = copy_to(&base, &work, "restored2.redb");
    apply_incremental_segments(&restored2, &[whole]).expect("apply whole");
    assert_matches_latest(&fx, &restored2);
}

#[test]
fn restored_database_continues_with_the_change_log_enabled() {
    let fx = Fixture::new("inc-reopen");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let inc = fx.increment(&point, "inc.seg");

    let work = private_dir("inc-reopen-work");
    let restored = copy_to(&base, &work, "restored.redb");
    apply_incremental_segments(&restored, &[inc]).expect("apply");

    // 適用した `change_log` 行により、再有効化の連続性検査（最後の LSN == 世代）が通る。
    let mut s = Storage::open(&restored).expect("reopen");
    s.enable_change_log().expect("continuity holds");
    assert_eq!(s.current_generation().expect("gen"), fx.generation());
    // 続きの書き込みもレコードに記録され、さらに次の増分を取れる。
    let core = EngineCore::from_storage(s, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::with_visibilities(TENANT, [Visibility::Public]).expect("tenant");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO ev (id, embedding) VALUES (99, '[0.1,0.1]')",
    )
    .expect("insert after restore");
    let after = chain_point_of(&inc_path(&fx, "inc.seg")).expect("point");
    let next = work.path().join("next.seg");
    core.storage_for_test()
        .export_incremental(&after, &next)
        .expect("next increment after restore");
}

fn inc_path(fx: &Fixture, name: &str) -> PathBuf {
    fx.path(name)
}

#[test]
fn export_rejects_foreign_database_timeline_and_ahead_base() {
    let fx = Fixture::new("inc-reject-src");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();

    let other = Fixture::new("inc-reject-other");
    other.write_round_1();
    let (_, other_point) = other.full_backup("base.redb");
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage().export_incremental(&other_point, &dest),
        Err(IncrementalError::ForeignDatabase)
    ));
    let wrong_timeline = ChainPoint {
        timeline: point.timeline + 1,
        ..point
    };
    assert!(matches!(
        fx.storage().export_incremental(&wrong_timeline, &dest),
        Err(IncrementalError::ForeignTimeline)
    ));
    let ahead = ChainPoint {
        end_lsn: fx.generation() + 1,
        ..point
    };
    assert!(matches!(
        fx.storage().export_incremental(&ahead, &dest),
        Err(IncrementalError::ChainBroken)
    ));
    let latest = ChainPoint {
        end_lsn: fx.generation(),
        ..point
    };
    assert!(matches!(
        fx.storage().export_incremental(&latest, &dest),
        Err(IncrementalError::EmptyRange)
    ));
    assert!(!dest.exists(), "a rejected export publishes nothing");
}

#[test]
fn export_rejects_range_pruned_from_the_log() {
    let fx = Fixture::new("inc-pruned");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    // ベースの終点の次のレコードを失わせる（刈り込み済みを模す）。
    {
        let txn = fx.storage().db().begin_write().expect("write");
        {
            let mut t = txn.open_table(CHANGE_LOG_TABLE).expect("table");
            assert!(t.remove(point.end_lsn + 1).expect("remove").is_some());
        }
        txn.commit().expect("commit");
    }
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage().export_incremental(&point, &dest),
        Err(IncrementalError::RangeUnavailable)
    ));
    assert!(!dest.exists());
}

#[test]
fn export_never_overwrites_and_rejects_shared_directories() {
    let fx = Fixture::new("inc-dest");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let dest = fx.path("inc.seg");
    fx.storage()
        .export_incremental(&point, &dest)
        .expect("first");
    let before = std::fs::read(&dest).expect("read");
    assert!(matches!(
        fx.storage().export_incremental(&point, &dest),
        Err(IncrementalError::DestinationExists)
    ));
    assert_eq!(std::fs::read(&dest).expect("read"), before);
    // 先行する別名のシンボリックリンクにも書かない。
    let link = fx.path("link.seg");
    std::os::unix::fs::symlink(fx.path("nowhere"), &link).expect("symlink");
    assert!(matches!(
        fx.storage().export_incremental(&point, &link),
        Err(IncrementalError::DestinationExists)
    ));

    let shared = TempDir::new("inc-shared");
    std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o775)).expect("chmod");
    assert!(matches!(
        fx.storage()
            .export_incremental(&point, shared.path().join("x.seg")),
        Err(IncrementalError::InsecureDirectory)
    ));
    assert!(matches!(
        fx.storage().export_incremental(&point, "/"),
        Err(IncrementalError::InvalidDestination)
    ));
}

#[test]
fn apply_rejects_broken_chains_without_touching_the_base() {
    let fx = Fixture::new("inc-apply-reject");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let inc1 = fx.increment(&point, "inc1.seg");
    let p1 = chain_point_of(&inc1).expect("p1");
    fx.write_round_3();
    let inc2 = fx.increment(&p1, "inc2.seg");

    let work = TempDir::new("inc-apply-reject-work");
    let restored = copy_to(&base, &work, "restored.redb");

    // 途中の増分の欠落（ベースの終点の次から始まらない）。
    assert!(matches!(
        apply_incremental_segments(&restored, std::slice::from_ref(&inc2)),
        Err(IncrementalError::ChainBroken)
    ));
    // 順序違反。
    assert!(matches!(
        apply_incremental_segments(&restored, &[inc2.clone(), inc1.clone()]),
        Err(IncrementalError::ChainBroken)
    ));
    // 重複。
    assert!(matches!(
        apply_incremental_segments(&restored, &[inc1.clone(), inc1.clone()]),
        Err(IncrementalError::ChainBroken)
    ));
    // 空の列。
    assert!(matches!(
        apply_incremental_segments(&restored, &[]),
        Err(IncrementalError::BadSegmentList)
    ));
    // 拒否された適用は何も反映しない（終点はベースのまま）。元のバックアップは複製にしか触れないので不変。
    assert_eq!(chain_point_of(&restored).expect("point"), point);

    // 別 DB のベースへは適用できない。
    let other = Fixture::new("inc-apply-other");
    other.write_round_1();
    let (other_base, _) = other.full_backup("base.redb");
    let other_copy = copy_to(&other_base, &work, "other.redb");
    assert!(matches!(
        apply_incremental_segments(&other_copy, std::slice::from_ref(&inc1)),
        Err(IncrementalError::ForeignDatabase)
    ));

    // 別 timeline（メタ表の timeline を書き換えたベース）。
    let forked = copy_to(&base, &work, "forked.redb");
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
    assert!(matches!(
        apply_incremental_segments(&forked, std::slice::from_ref(&inc1)),
        Err(IncrementalError::ForeignTimeline)
    ));

    // 正しい連鎖は通る。
    apply_incremental_segments(&restored, &[inc1, inc2]).expect("apply");
    assert_matches_latest(&fx, &restored);
}

#[test]
fn apply_rejects_tampered_truncated_and_foreign_segments() {
    let fx = Fixture::new("inc-tamper");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let inc = fx.increment(&point, "inc.seg");
    let work = TempDir::new("inc-tamper-work");
    let restored = copy_to(&base, &work, "restored.redb");

    // 本体・トレーラ・ヘッダの改ざん。
    for offset in [1usize, 40, 200] {
        let bad = copy_to(&inc, &work, &format!("bad{offset}.seg"));
        flip_byte(&bad, offset);
        assert!(
            matches!(
                apply_incremental_segments(&restored, std::slice::from_ref(&bad)),
                Err(IncrementalError::Segment(_))
            ),
            "tamper at {offset}"
        );
    }
    // 途中切れ・余り。
    let truncated = copy_to(&inc, &work, "trunc.seg");
    let len = std::fs::metadata(&truncated).expect("meta").len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&truncated)
        .expect("open")
        .set_len(len - 5)
        .expect("truncate");
    assert!(matches!(
        apply_incremental_segments(&restored, std::slice::from_ref(&truncated)),
        Err(IncrementalError::Segment(ArchiveError::Truncated))
    ));
    let extended = copy_to(&inc, &work, "ext.seg");
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&extended)
            .expect("open");
        f.write_all(b"x").expect("append");
    }
    assert!(matches!(
        apply_incremental_segments(&restored, std::slice::from_ref(&extended)),
        Err(IncrementalError::Segment(ArchiveError::BadFormat))
    ));
    // 通常ファイルでないもの（シンボリックリンク）。
    let link = work.path().join("link.seg");
    std::os::unix::fs::symlink(&inc, &link).expect("symlink");
    assert!(matches!(
        apply_incremental_segments(&restored, &[link]),
        Err(IncrementalError::Segment(ArchiveError::BadFormat))
    ));
}

/// 検証用のセグメントファイルを手で組み立てる（チェックサムは正しいが内容が不正なレコードを作る）。
fn forge_segment(
    path: &Path,
    base: &ChainPoint,
    bodies: Vec<RecordBody>,
    storage_format: u32,
) -> PathBuf {
    let mut body_bytes = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        let lsn = base.end_lsn + 1 + i as u64;
        let header = RecordHeader {
            db_id: base.db_id,
            timeline: base.timeline,
            lsn,
            prev_lsn: lsn - 1,
            commit_time_us: 0,
            storage_format,
        };
        body_bytes
            .extend(super::super::encode_record(&header, body, usize::MAX >> 8).expect("encode"));
    }
    let header = SegmentHeader {
        db_id: base.db_id,
        timeline: base.timeline,
        first_lsn: base.end_lsn + 1,
        last_lsn: base.end_lsn + bodies.len() as u64,
        record_count: bodies.len() as u64,
        storage_format,
        body_len: body_bytes.len() as u64,
    };
    let mut file = header.encode().to_vec();
    file.extend(body_bytes);
    let sum = crate::crypto::sha256::digest(&file);
    file.extend(sum);
    std::fs::write(path, file).expect("write forged");
    set_owner_only(path);
    path.to_path_buf()
}

use super::super::{RecordBody, RecordHeader, TableChange, CHANGE_LOG_META_TABLE_NAME};

#[test]
fn apply_rejects_records_that_cannot_be_applied() {
    let fx = Fixture::new("inc-forged");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    let work = TempDir::new("inc-forged-work");
    let change = |name: &str, shape: u32| TableChange {
        name: name.to_string(),
        shape,
        upserts: vec![(b"k".to_vec(), b"v".to_vec())],
        removes: Vec::new(),
    };
    let cases: Vec<(&str, RecordBody, u32, &str)> = vec![
        // 記録範囲外のメタ表（DB 識別子の書き換え）を指す。
        (
            "meta",
            RecordBody {
                tables: vec![change(CHANGE_LOG_META_TABLE_NAME, 1)],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        // 存在しない表の変更。
        (
            "missing",
            RecordBody {
                tables: vec![change("no_such_table", 1)],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        // 存在しない表の削除。
        (
            "drop-missing",
            RecordBody {
                dropped: vec!["no_such_table".to_string()],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        // 範囲外の shape 番号。
        (
            "shape",
            RecordBody {
                created: vec![("fresh".to_string(), 9999)],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        // キー・値のバイト列が shape の型に合わない(panic させない)。shape 11 は (u64, u64)、shape 0 は (&str, u64)。
        (
            "bad-fixed-key",
            RecordBody {
                created: vec![("fresh".to_string(), 11)],
                tables: vec![change("fresh", 11)],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        (
            "bad-utf8-key",
            RecordBody {
                created: vec![("fresh".to_string(), 0)],
                tables: vec![TableChange {
                    name: "fresh".to_string(),
                    shape: 0,
                    upserts: vec![(vec![0xff], 1u64.to_le_bytes().to_vec())],
                    removes: Vec::new(),
                }],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        (
            "bad-value",
            RecordBody {
                created: vec![("fresh".to_string(), 0)],
                tables: vec![TableChange {
                    name: "fresh".to_string(),
                    shape: 0,
                    upserts: vec![(b"k".to_vec(), vec![1, 2, 3])],
                    removes: Vec::new(),
                }],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        (
            "bad-remove-key",
            RecordBody {
                created: vec![("fresh".to_string(), 3)],
                tables: vec![TableChange {
                    name: "fresh".to_string(),
                    shape: 3,
                    upserts: Vec::new(),
                    removes: vec![vec![0xff]],
                }],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        // 空の表名(redb の `TableDefinition::new` が panic する)。作成・変更・削除のどの経路でも拒否する。
        (
            "empty-created",
            RecordBody {
                created: vec![(String::new(), 0)],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        (
            "empty-changed",
            RecordBody {
                tables: vec![change("", 0)],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        (
            "empty-dropped",
            RecordBody {
                dropped: vec![String::new()],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "BadRecord",
        ),
        // 世代の表を含まないレコード（末尾に世代が届かない）。
        (
            "no-generation",
            RecordBody {
                created: vec![("fresh".to_string(), 1)],
                ..RecordBody::default()
            },
            STORAGE_FORMAT_VERSION,
            "ChainBroken",
        ),
        // ストレージ形式の版数が違う。
        (
            "format",
            RecordBody::default(),
            STORAGE_FORMAT_VERSION + 1,
            "UnsupportedFormat",
        ),
    ];
    for (label, body, format, want) in cases {
        let restored = copy_to(&base, &work, &format!("{label}.redb"));
        let seg = forge_segment(
            &work.path().join(format!("{label}.seg")),
            &point,
            vec![body],
            format,
        );
        let r = apply_incremental_segments(&restored, &[seg]);
        let got = format!("{:?}", r.expect_err("must be rejected"));
        assert!(got.starts_with(want), "case {label}: {got}");
    }
}

#[test]
fn chain_point_of_rejects_unreadable_and_disabled_bases() {
    let fx = Fixture::new("inc-point");
    fx.write_round_1();
    let junk = fx.path("junk");
    std::fs::write(&junk, vec![0xAB; 8192]).expect("write");
    assert!(matches!(
        chain_point_of(&junk),
        Err(IncrementalError::BaseUnreadable)
    ));
    assert!(matches!(
        chain_point_of(&fx.path("missing")),
        Err(IncrementalError::BaseUnreadable)
    ));
    // 変更ログを有効にしていない DB のバックアップは連鎖の起点にできない。
    let dir = TempDir::new("inc-point-disabled");
    let s = Storage::open(dir.db_path()).expect("open");
    drop(s);
    assert!(matches!(
        chain_point_of(&dir.db_path()),
        Err(IncrementalError::NotEnabled)
    ));
}

#[test]
fn read_only_export_of_a_stopped_database_matches_the_live_export() {
    let fx = Fixture::new("inc-readonly");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let live = fx.increment(&point, "live.seg");
    let Fixture {
        _db_dir: db_dir,
        out,
        core,
    } = fx;
    drop(core);

    // 停止中の DB を読み取り専用で開いて同じ区間を出力する。DB のバイト列は変わらない。
    let before = std::fs::read(db_dir.db_path()).expect("read");
    let ro = crate::storage::ReadOnlyStorage::open(db_dir.db_path()).expect("open read-only");
    let stopped = out.path().join("stopped.seg");
    ro.export_incremental(&point, &stopped).expect("export");
    drop(ro);
    assert_eq!(std::fs::read(db_dir.db_path()).expect("read"), before);
    // 同じ区間・同じ内容（全体チェックサムを含めてバイト単位で一致）。
    assert_eq!(
        std::fs::read(&stopped).expect("read"),
        std::fs::read(&live).expect("read")
    );
}

/// 増分（`overlap_ok` でない経路）の先頭が、適用済みの区間に重なる場合は拒否する。読み飛ばしも
/// 黙った受け入れも再適用もしない(PITR のアーカイブだけが重なりを許し、内容を照合する)。
#[test]
fn apply_rejects_an_increment_that_overlaps_the_applied_range() {
    let fx = Fixture::new("inc-apply-overlap");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let inc1 = fx.increment(&point, "inc1.seg");
    let p1 = chain_point_of(&inc1).expect("p1");
    fx.write_round_3();
    let inc2 = fx.increment(&p1, "inc2.seg");
    // ベースの直後から最新までを 1 つにした増分（inc1 の範囲を含む）。
    let whole = fx.increment(&point, "whole.seg");

    let work = TempDir::new("inc-apply-overlap-work");
    let restored = copy_to(&base, &work, "restored.redb");
    apply_incremental_segments(&restored, std::slice::from_ref(&inc1)).expect("apply inc1");
    let after_inc1 = chain_point_of(&restored).expect("point");
    let model = dump_db(&redb::Database::open(&restored).expect("open"));

    // 同じ増分の再適用(先頭が適用済み)、適用済みの区間を含む増分、それらと続きの組み合わせ。
    for segs in [
        vec![inc1.clone()],
        vec![whole.clone()],
        vec![inc1.clone(), inc2.clone()],
    ] {
        assert!(matches!(
            apply_incremental_segments(&restored, &segs),
            Err(IncrementalError::ChainBroken)
        ));
    }
    // 拒否は何も反映しない(世代も内容も変わらない)。
    assert_eq!(chain_point_of(&restored).expect("point"), after_inc1);
    let got = dump_db(&redb::Database::open(&restored).expect("open"));
    assert_eq!(oracle::diff_models(&model, &got), None);
    assert_eq!(oracle::diff_models(&got, &model), None);
    // 正しい続きは通る。
    apply_incremental_segments(&restored, &[inc2]).expect("apply inc2");
    assert_matches_latest(&fx, &restored);
}

/// 復元の世代の表を指すレコード(変更・削除・作成)は、表が存在する適用先でも拒否され、値は変わらない
/// (増分・PITR の再生がこの表を書き換え・削除できない。RECOVER-17・Issue #2448)。
#[test]
fn apply_never_touches_the_restore_generation_table() {
    use super::super::super::restore_generation::RESTORE_GENERATION_TABLE_NAME as NAME;
    let fx = Fixture::new("inc-restore-gen");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    let work = TempDir::new("inc-restore-gen-work");
    let change = TableChange {
        name: NAME.to_string(),
        shape: 1,
        upserts: vec![(b"generation".to_vec(), 7u64.to_le_bytes().to_vec())],
        removes: Vec::new(),
    };
    let cases: Vec<(&str, RecordBody)> = vec![
        (
            "changed",
            RecordBody {
                tables: vec![change],
                ..RecordBody::default()
            },
        ),
        (
            "dropped",
            RecordBody {
                dropped: vec![NAME.to_string()],
                ..RecordBody::default()
            },
        ),
        (
            "created",
            RecordBody {
                created: vec![(NAME.to_string(), 1)],
                ..RecordBody::default()
            },
        ),
    ];
    for (label, body) in cases {
        let restored = copy_to(&base, &work, &format!("{label}.redb"));
        let advanced = {
            let mut s = Storage::open(&restored).expect("open copy");
            s.advance_restore_generation().expect("advance")
        };
        let seg = forge_segment(
            &work.path().join(format!("{label}.seg")),
            &point,
            vec![body],
            STORAGE_FORMAT_VERSION,
        );
        let r = apply_incremental_segments(&restored, &[seg]);
        let got = format!("{:?}", r.expect_err("must be rejected"));
        assert!(got.starts_with("BadRecord"), "case {label}: {got}");
        let s = Storage::open(&restored).expect("reopen");
        assert_eq!(s.restore_generation().expect("read"), advanced, "{label}");
    }
}
// ---------------------------------------------------------------------------
// 刈り込み済みの区間をアーカイブから補う増分（Issue #3099・RECOVER-14）
// ---------------------------------------------------------------------------

use crate::storage::ArchiveOptions;

/// 表の行の LSN 一覧（昇順）。
fn log_lsns(fx: &Fixture) -> Vec<u64> {
    let read = fx.storage().db().begin_read().expect("read");
    let t = read.open_table(CHANGE_LOG_TABLE).expect("table");
    t.iter()
        .expect("iter")
        .map(|e| e.expect("entry").0.value())
        .collect()
}

/// 未アーカイブの分を `dir` へ書き出し、`retain` 件を残して刈り込む（実際の保守の経路）。
fn archive_and_prune(fx: &Fixture, dir: &Path, segment_bytes: usize, retain: u64) {
    let mut opts = ArchiveOptions::new(dir);
    opts.segment_bytes = segment_bytes;
    opts.retain_records = Some(retain);
    for _ in 0..4 {
        fx.storage().archive_change_log(&opts).expect("archive");
    }
}

/// ベース取得後に更新し、アーカイブして全件を刈り込み、さらに更新した状態を作る。
/// 戻り値は（フィクスチャ、ベースのファイル、ベースの終点、アーカイブ先）。
fn pruned_fixture(label: &str, segment_bytes: usize) -> (Fixture, PathBuf, ChainPoint, TempDir) {
    let fx = Fixture::new(label);
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let archive = private_dir(&format!("{label}-arc"));
    archive_and_prune(&fx, archive.path(), segment_bytes, 0);
    fx.write_round_3();
    (fx, base, point, archive)
}

fn archive_segments(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").path())
        .collect();
    v.sort();
    v
}

/// 1 レコード 1 セグメントのアーカイブから、先頭が `lsn` のセグメントのパス。
fn needed_segment(dir: &Path, lsn: u64) -> PathBuf {
    let suffix = format!("-{lsn:020}-{lsn:020}.seg");
    archive_segments(dir)
        .into_iter()
        .find(|p| p.to_string_lossy().ends_with(&suffix))
        .expect("segment")
}

#[test]
fn export_fills_the_pruned_range_from_the_archive() {
    // セグメントの大きさを変えて、ベースの終点がセグメントの途中・境界に落ちる配置を通す。
    for segment_bytes in [1usize, 300, 700, 100_000] {
        let (fx, base, point, archive) = pruned_fixture("inc-fill", segment_bytes);
        let dest = fx.path("with-archive.seg");
        assert!(
            log_lsns(&fx)[0] > point.end_lsn + 1,
            "the head of the range must be pruned"
        );
        // アーカイブを指定しなければ従来どおり拒否する。
        assert!(matches!(
            fx.storage()
                .export_incremental(&point, fx.path("plain.seg")),
            Err(IncrementalError::RangeUnavailable)
        ));
        let summary = fx
            .storage()
            .export_incremental_with_archive(&point, archive.path(), &dest)
            .expect("export with archive");
        assert_eq!(summary.first_lsn, point.end_lsn + 1);
        assert_eq!(summary.last_lsn, fx.generation());
        let info = verify_segment_file(&dest, usize::MAX >> 8).expect("verify");
        assert_eq!(info.header.record_count, summary.records);

        let work = TempDir::new("inc-fill-work");
        let restored = copy_to(&base, &work, "restored.redb");
        apply_incremental_segments(&restored, &[dest]).expect("apply");
        assert_matches_latest(&fx, &restored);
    }
}

#[test]
fn export_works_when_every_row_is_pruned_and_chains_across_prunes() {
    let fx = Fixture::new("inc-all-pruned");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let archive = private_dir("inc-all-pruned-arc");
    archive_and_prune(&fx, archive.path(), 400, 0);
    assert!(log_lsns(&fx).is_empty(), "the whole log is pruned");
    let inc1 = fx.path("inc1.seg");
    fx.storage()
        .export_incremental_with_archive(&point, archive.path(), &inc1)
        .expect("all pruned");
    let p1 = chain_point_of(&inc1).expect("point");

    // 前回の増分をベースにした次の増分も、刈り込みをまたいで作れる。
    fx.write_round_3();
    archive_and_prune(&fx, archive.path(), 400, 0);
    let inc2 = fx.path("inc2.seg");
    fx.storage()
        .export_incremental_with_archive(&p1, archive.path(), &inc2)
        .expect("second");

    let work = TempDir::new("inc-all-pruned-work");
    let restored = copy_to(&base, &work, "restored.redb");
    apply_incremental_segments(&restored, &[inc1, inc2]).expect("apply");
    assert_matches_latest(&fx, &restored);
}

#[test]
fn export_with_archive_matches_the_plain_export_when_nothing_is_pruned() {
    let fx = Fixture::new("inc-not-pruned");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let archive = private_dir("inc-not-pruned-arc");
    let plain = fx.increment(&point, "plain.seg");
    // 存在しないアーカイブ先でも、補う必要が無ければ開かない。
    let with = fx.path("with.seg");
    fx.storage()
        .export_incremental_with_archive(&point, archive.path().join("missing"), &with)
        .expect("export");
    assert_eq!(
        std::fs::read(&plain).expect("plain"),
        std::fs::read(&with).expect("with")
    );
}

#[test]
fn export_uses_archive_segments_that_overlap_the_retained_rows() {
    let fx = Fixture::new("inc-overlap");
    fx.write_round_1();
    let (base, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    fx.write_round_3();
    let archive = private_dir("inc-overlap-arc");
    // 直近 3 件を DB に残す（アーカイブのセグメントは、DB の残りと重なる区間も持つ）。
    archive_and_prune(&fx, archive.path(), 100_000, 3);
    assert_eq!(log_lsns(&fx).len(), 3);
    let dest = fx.path("inc.seg");
    fx.storage()
        .export_incremental_with_archive(&point, archive.path(), &dest)
        .expect("export");
    let work = TempDir::new("inc-overlap-work");
    let restored = copy_to(&base, &work, "restored.redb");
    apply_incremental_segments(&restored, &[dest]).expect("apply");
    assert_matches_latest(&fx, &restored);

    // 重なる区間の内容が DB の行と食い違う（分岐した系列）ときは拒否する。
    let lsns = log_lsns(&fx);
    let other = {
        let read = fx.storage().db().begin_read().expect("read");
        let t = read.open_table(CHANGE_LOG_TABLE).expect("table");
        let v = t.get(lsns[1]).expect("get").expect("row").value().to_vec();
        v
    };
    {
        let txn = fx.storage().db().begin_write().expect("write");
        {
            let mut t = txn.open_table(CHANGE_LOG_TABLE).expect("table");
            t.insert(lsns[0], other.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");
    }
    let bad = fx.path("diverged.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &bad),
        Err(IncrementalError::ChainBroken)
    ));
    assert!(!bad.exists());
}

#[test]
fn export_rejects_missing_tampered_foreign_and_shared_archives() {
    // 必要な区間の先頭のセグメントが欠けている。
    let (fx, _, point, archive) = pruned_fixture("inc-missing", 1);
    let dest = fx.path("x.seg");
    std::fs::remove_file(needed_segment(archive.path(), point.end_lsn + 1)).expect("remove first");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::RangeUnavailable)
    ));
    assert!(!dest.exists());

    // 途中のセグメントが欠けている。
    let (fx, _, point, archive) = pruned_fixture("inc-hole", 1);
    std::fs::remove_file(needed_segment(archive.path(), point.end_lsn + 2)).expect("remove middle");
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::RangeUnavailable)
    ));
    assert!(!dest.exists());

    // 末尾側のセグメントが欠けている（刈り込み済みの区間の末尾まで届かない）。
    let (fx, _, point, archive) = pruned_fixture("inc-tail", 1);
    let segs = archive_segments(archive.path());
    std::fs::remove_file(segs.last().expect("last")).expect("remove last");
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::RangeUnavailable)
    ));
    assert!(!dest.exists());

    // 1 バイトの改ざん。
    let (fx, _, point, archive) = pruned_fixture("inc-tamper", 1);
    flip_byte(&needed_segment(archive.path(), point.end_lsn + 1), 1);
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::Segment(ArchiveError::ChecksumMismatch))
    ));
    assert!(!dest.exists());

    // 別の DB のアーカイブ（名前の DB 識別子が違うので、区間を覆うものが無い）。
    let (fx, _, point, _archive) = pruned_fixture("inc-foreign", 300);
    let (_other_fx, _, _, other_archive) = pruned_fixture("inc-foreign-other", 300);
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, other_archive.path(), &dest),
        Err(IncrementalError::RangeUnavailable)
    ));
    assert!(!dest.exists());

    // 他者が書けるアーカイブ先は読み取りでも拒否する。
    let (fx, _, point, archive) = pruned_fixture("inc-shared-arc", 300);
    std::fs::set_permissions(archive.path(), std::fs::Permissions::from_mode(0o777))
        .expect("chmod");
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::InsecureDirectory)
    ));
    assert!(!dest.exists());
    std::fs::set_permissions(archive.path(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod back");
}

#[test]
fn export_with_archive_rejects_a_hole_in_the_database_side() {
    let (fx, _, point, archive) = pruned_fixture("inc-db-hole", 300);
    fx.sql("INSERT INTO ev (id, embedding) VALUES (30, '[0.1,0.2]')");
    fx.sql("INSERT INTO ev (id, embedding) VALUES (31, '[0.1,0.2]')");
    let lsns = log_lsns(&fx);
    assert!(lsns.len() >= 3, "round 3 leaves rows in the log");
    {
        let txn = fx.storage().db().begin_write().expect("write");
        {
            let mut t = txn.open_table(CHANGE_LOG_TABLE).expect("table");
            assert!(t.remove(lsns[1]).expect("remove").is_some());
        }
        txn.commit().expect("commit");
    }
    let dest = fx.path("x.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::RangeUnavailable)
    ));
    assert!(!dest.exists());
}

/// 稼働中の書き込みを止めずに増分を取れ、取得時点（末尾 LSN）の状態になること（Issue #3098）。
/// 書き込みを流し続けながら取った増分をベースへ適用し、その終点から取り直した次の増分を重ねると
/// 最新と一致する（先頭から欠けなく連続した接頭辞であることの確認）。
#[test]
fn live_export_under_concurrent_writes_is_a_consistent_prefix() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let fx = Fixture::new("inc-live");
    fx.write_round_1();
    let (base, base_point) = fx.full_backup("base.redb");

    let stop = AtomicBool::new(false);
    let done = AtomicU64::new(0);
    let (inc1, last_lsn) = std::thread::scope(|s| {
        s.spawn(|| {
            let mut i = 100;
            while !stop.load(Ordering::Acquire) {
                fx.sql(&format!(
                    "INSERT INTO ev (id, embedding) VALUES ({i}, '[0.1,0.2]')"
                ));
                i += 1;
                done.fetch_add(1, Ordering::Release);
            }
        });
        while done.load(Ordering::Acquire) < 3 {
            std::thread::yield_now();
        }
        let p = fx.path("inc1.seg");
        let r = fx.core.export_incremental(&base_point, &p);
        stop.store(true, Ordering::Release);
        (p, r.expect("live export").last_lsn)
    });
    assert!(last_lsn > base_point.end_lsn);
    assert!(last_lsn <= fx.generation());

    let work = TempDir::new("inc-live-work");
    let restored = copy_to(&base, &work, "restored.redb");
    let summary =
        apply_incremental_segments(&restored, std::slice::from_ref(&inc1)).expect("apply");
    assert_eq!(summary.last_lsn, last_lsn);

    // 取得時点の終点から最新までを重ねると最新と一致する。
    let point1 = chain_point_of(&inc1).expect("inc1 point");
    assert_eq!(point1.end_lsn, last_lsn);
    if point1.end_lsn < fx.generation() {
        let inc2 = fx.increment(&point1, "inc2.seg");
        apply_incremental_segments(&restored, &[inc2]).expect("apply inc2");
    }
    assert_matches_latest(&fx, &restored);
}

/// 書き出しはフル・増分を通じて同時 1 本まで。実行中は `InProgress` で何も作らず、終われば再び取れる。
#[test]
fn live_export_is_limited_to_one_at_a_time_shared_with_full_backups() {
    use std::sync::atomic::Ordering;

    let fx = Fixture::new("inc-busy");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();

    fx.storage()
        .snapshot_export_busy
        .store(true, Ordering::Release);
    let dest = fx.path("busy.seg");
    assert!(matches!(
        fx.storage().export_incremental(&point, &dest),
        Err(IncrementalError::InProgress)
    ));
    assert!(std::fs::symlink_metadata(&dest).is_err());
    // 拒否された側は、他方の実行中フラグを解放しない。
    assert!(fx.storage().snapshot_export_busy.load(Ordering::Acquire));
    fx.storage()
        .snapshot_export_busy
        .store(false, Ordering::Release);

    fx.storage()
        .export_incremental(&point, &dest)
        .expect("export");
    assert!(!fx.storage().snapshot_export_busy.load(Ordering::Acquire));
}

/// アーカイブ補完付きの書き出しも、フル・増分と同じ実行中フラグで同時 1 本に制限する（補完が不要でも取る）。
#[test]
fn live_export_with_archive_shares_the_busy_flag() {
    use std::sync::atomic::Ordering;

    let fx = Fixture::new("inc-busy-arc");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    let archive = private_dir("inc-busy-arc-dir");

    fx.storage()
        .snapshot_export_busy
        .store(true, Ordering::Release);
    let dest = fx.path("busy.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::InProgress)
    ));
    assert!(std::fs::symlink_metadata(&dest).is_err());
    assert!(fx.storage().snapshot_export_busy.load(Ordering::Acquire));
    fx.storage()
        .snapshot_export_busy
        .store(false, Ordering::Release);

    fx.storage()
        .export_incremental_with_archive(&point, archive.path(), &dest)
        .expect("export");
    assert!(!fx.storage().snapshot_export_busy.load(Ordering::Acquire));
}

/// 要求区間より前の行が DB に残っているとき、区間内の欠けは刈り込みではなく破損なので、アーカイブが
/// 覆っていても補わず `RangeUnavailable` で拒否する。
#[test]
fn export_rejects_an_interior_gap_even_when_the_archive_covers_it() {
    let fx = Fixture::new("inc-interior-gap");
    fx.write_round_1();
    let (_, point) = fx.full_backup("base.redb");
    fx.write_round_2();
    fx.write_round_3();
    let archive = private_dir("inc-interior-gap-arc");
    // 全件を DB に残したままアーカイブして、水位を最新まで進める。
    archive_and_prune(&fx, archive.path(), 100_000, 100_000);
    let lsns = log_lsns(&fx);
    assert!(lsns[0] <= point.end_lsn, "rows before the range remain");
    let hole = point.end_lsn + 1;
    assert!(lsns.contains(&hole) && lsns.iter().any(|l| *l > hole));
    {
        let txn = fx.storage().db().begin_write().expect("write");
        {
            let mut t = txn.open_table(CHANGE_LOG_TABLE).expect("table");
            t.remove(hole).expect("remove");
        }
        txn.commit().expect("commit");
    }
    let dest = fx.path("gap.seg");
    assert!(matches!(
        fx.storage()
            .export_incremental_with_archive(&point, archive.path(), &dest),
        Err(IncrementalError::RangeUnavailable)
    ));
    assert!(!dest.exists());
}
