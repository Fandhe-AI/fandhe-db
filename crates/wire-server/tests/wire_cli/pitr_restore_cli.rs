//! `wire-server restore --change-log-archive-dir ... --until-lsn|--until-time` の外形検証（Issue #2169・
//! 親 #2164・RECOVER-14 ポインタ）。バイナリを子プロセスとして起動し、終了コード・出力・復元先の行集合を
//! 確かめる。変更ログを有効にした DB は engine の公開 API で作る（サーバーを起動しない。ポート・固定パスに
//! 依存しない）。受け入れ条件「指定点までの状態が再現される」を LSN・時刻の両方で確かめ、範囲外・欠落・
//! 別 DB・改ざん・引数の不正の拒否と、拒否時に何も公開されず一時ファイルも残らないことを固定する。

use crate::common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engine::storage::{ArchiveOptions, RowInput, Storage, Visibility};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");
const SECRET_NOTE: &[u8] = b"secret-payload-qwxz";

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// 出力先に使える（他者が書けない）ディレクトリを作る。umask に依らず 0755 にする。
fn make_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(path).expect("mkdir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// 変更ログを有効にして `ids` の行を 1 件ずつ足す（開いて閉じる。続きの書き込みも連続性検査を通る）。
fn write_rows(db: &Path, ids: std::ops::RangeInclusive<u64>) {
    let mut storage = Storage::open(db).expect("open");
    storage.enable_change_log().expect("enable change log");
    for id in ids {
        storage
            .put(
                id,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.5, id as f32],
                    metadata: SECRET_NOTE,
                },
            )
            .expect("put");
    }
}

/// 未アーカイブのレコードを 1 レコード 1 セグメントで `dir` へ書く。
fn archive(db: &Path, dir: &Path, segment_bytes: usize) {
    let mut storage = Storage::open(db).expect("open");
    storage.enable_change_log().expect("enable change log");
    let mut opts = ArchiveOptions::new(dir);
    opts.segment_bytes = segment_bytes;
    let report = storage.archive_change_log(&opts).expect("archive");
    assert!(!report.skipped);
}

fn row_ids(db: &Path) -> Vec<u64> {
    let storage = Storage::open(db).expect("open");
    let mut ids: Vec<u64> = storage.scan().expect("scan").iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".fandhe-"))
        .collect()
}

/// 標準出力・標準エラーに行の値・テナント名・パスが現れないこと。
fn assert_no_leak(o: &Output, extra: &[&str]) {
    let all = format!("{}{}", stdout(o), stderr(o));
    for needle in ["secret-payload-qwxz", "tenant-a"]
        .iter()
        .chain(extra.iter())
    {
        assert!(!all.contains(needle), "output leaked {needle:?}: {all}");
    }
}

/// UNIX エポックからのマイクロ秒を RFC 3339（UTC・`Z`）に整形する（日付は民間暦への変換）。
fn rfc3339(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn now_micros() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    i64::try_from(d.as_micros()).expect("fits")
}

struct Pitr {
    _fx: common::TempFixtureDir,
    root: PathBuf,
    base: PathBuf,
    archive: PathBuf,
    /// ベースの終点の LSN（ids 1..=3 の時点）。
    base_lsn: u64,
    /// ids 6 を書いた直後の壁時計（マイクロ秒）。ids 7 以降はこれより後にコミットされる。
    mid_micros: i64,
}

/// フルバックアップ（ids 1..=3）→ ids 4..=6 → 待機 → ids 7..=9 → アーカイブ。
fn build(label: &str, segment_bytes: usize) -> Pitr {
    let fx = common::TempFixtureDir::new(label);
    let db = PathBuf::from(fx.db_path_str());
    let root = db.parent().expect("parent").join("out");
    make_private_dir(&root);
    let base = root.join("base.redb");
    let archive_dir = root.join("archive");
    make_private_dir(&archive_dir);

    write_rows(&db, 1..=3);
    let o = run(&["backup", "--db", s(&db), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let base_lsn = engine::storage::chain_point_of(&base)
        .expect("chain point")
        .end_lsn;

    write_rows(&db, 4..=6);
    let mid_micros = now_micros();
    std::thread::sleep(std::time::Duration::from_millis(30));
    write_rows(&db, 7..=9);
    archive(&db, &archive_dir, segment_bytes);
    Pitr {
        _fx: fx,
        root,
        base,
        archive: archive_dir,
        base_lsn,
        mid_micros,
    }
}

impl Pitr {
    fn restore(&self, dest: &Path, extra: &[&str]) -> Output {
        let mut args = vec!["restore", "--from", s(&self.base), "--db", s(dest)];
        args.extend_from_slice(extra);
        run(&args)
    }

    fn restore_archive(&self, dest: &Path, until: &[&str]) -> Output {
        let mut extra = vec!["--change-log-archive-dir", s(&self.archive)];
        extra.extend_from_slice(until);
        self.restore(dest, &extra)
    }

    /// 拒否された復元が、復元先も一時ファイルも残さないこと。
    fn assert_nothing_published(&self, dest: &Path) {
        assert!(!dest.exists(), "a rejected restore publishes nothing");
        assert!(
            leftovers(&self.root).is_empty(),
            "{:?}",
            leftovers(&self.root)
        );
    }
}

/// 受け入れ条件: 指定した LSN までの状態が再現される（コミット境界。1 件ごとに 1 commit）。
#[test]
fn recover14_until_lsn_restores_the_prefix_up_to_the_point() {
    for (label, seg_bytes) in [
        ("pitr-cli-lsn-one", usize::MAX >> 8),
        ("pitr-cli-lsn-many", 1),
    ] {
        let p = build(label, seg_bytes);
        for k in [1u64, 3, 6] {
            let dest = p.root.join(format!("restored-{k}.redb"));
            let lsn = (p.base_lsn + k).to_string();
            let o = p.restore_archive(&dest, &["--until-lsn", &lsn]);
            assert_eq!(o.status.code(), Some(0), "{label} k={k}: {}", stderr(&o));
            assert!(stdout(&o).contains("restore written"), "{}", stdout(&o));
            assert_no_leak(&o, &[s(&p.root)]);
            assert_eq!(
                row_ids(&dest),
                (1..=3 + k).collect::<Vec<_>>(),
                "{label} k={k}"
            );
            assert!(leftovers(&p.root).is_empty());
        }
    }
}

/// 時刻指定: コミット時刻が T を超える最初のコミットの直前で止まる。
#[test]
fn recover14_until_time_stops_before_the_first_later_commit() {
    let p = build("pitr-cli-time", usize::MAX >> 8);
    let dest = p.root.join("restored.redb");
    let t = rfc3339(p.mid_micros);
    let o = p.restore_archive(&dest, &["--until-time", &t]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&dest), (1..=6).collect::<Vec<_>>());

    // オフセット表記でも同じ時刻になる（UTC へ正規化）。
    let dest2 = p.root.join("restored2.redb");
    let shifted = rfc3339(p.mid_micros + 9 * 3_600 * 1_000_000).replace('Z', "+09:00");
    let o = p.restore_archive(&dest2, &["--until-time", &shifted]);
    // 「+9h した現地時刻 +09:00」は同じ瞬間（mid）を指す。
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&dest2), (1..=6).collect::<Vec<_>>());
}

/// 増分バックアップとアーカイブの併用。停止点が増分の外（アーカイブ側）でも内でも再現される。
#[test]
fn recover14_incrementals_and_archive_combine() {
    let p = build("pitr-cli-combo", 1);
    // 増分はベースの終点から最新（ids 9）まで。増分だけで内側の LSN を指定できることと、
    // 増分 + アーカイブの併用を確かめる。
    let db = PathBuf::from(p._fx.db_path_str());
    let inc = p.root.join("inc.seg");
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&db),
        "--base",
        s(&p.base),
        "--out",
        s(&inc),
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let dest = p.root.join("inside.redb");
    let lsn = (p.base_lsn + 2).to_string();
    let o = p.restore(&dest, &["--incremental", s(&inc), "--until-lsn", &lsn]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&dest), (1..=5).collect::<Vec<_>>());
    // 増分 + アーカイブ（重なる区間は読み飛ばされる）。
    let dest = p.root.join("both.redb");
    let lsn = (p.base_lsn + 5).to_string();
    let o = p.restore_archive(&dest, &["--incremental", s(&inc), "--until-lsn", &lsn]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&dest), (1..=8).collect::<Vec<_>>());
}

/// 引数の不正は DB を開く前に 2 で拒否し、何も作らない。
#[test]
fn recover14_pitr_argument_errors_exit_2_and_publish_nothing() {
    let p = build("pitr-cli-usage", usize::MAX >> 8);
    let dest = p.root.join("restored.redb");
    let arc = s(&p.archive).to_string();
    let t = rfc3339(p.mid_micros);
    let cases: Vec<Vec<&str>> = vec![
        // 併用
        vec![
            "--change-log-archive-dir",
            &arc,
            "--until-lsn",
            "9",
            "--until-time",
            &t,
        ],
        // アーカイブだけ（目標なし）
        vec!["--change-log-archive-dir", &arc],
        // 目標だけ（ログ源なし）
        vec!["--until-lsn", "9"],
        vec!["--until-time", &t],
        // 不正な LSN
        vec!["--change-log-archive-dir", &arc, "--until-lsn", "0"],
        vec!["--change-log-archive-dir", &arc, "--until-lsn", "-1"],
        vec!["--change-log-archive-dir", &arc, "--until-lsn", "abc"],
        vec!["--change-log-archive-dir", &arc, "--until-lsn", ""],
        // ゾーンなし・空白区切り・名前のゾーン・日付だけ
        vec![
            "--change-log-archive-dir",
            &arc,
            "--until-time",
            "2030-01-02T03:04:05",
        ],
        vec![
            "--change-log-archive-dir",
            &arc,
            "--until-time",
            "2030-01-02 03:04:05Z",
        ],
        vec![
            "--change-log-archive-dir",
            &arc,
            "--until-time",
            "2030-01-02T03:04:05 UTC",
        ],
        vec![
            "--change-log-archive-dir",
            &arc,
            "--until-time",
            "2030-01-02",
        ],
        // 存在しないアーカイブ、値欠落、重複
        vec![
            "--change-log-archive-dir",
            "/nonexistent-pitr-archive",
            "--until-lsn",
            "9",
        ],
        vec!["--change-log-archive-dir", &arc, "--until-lsn"],
        vec![
            "--change-log-archive-dir",
            &arc,
            "--until-lsn",
            "9",
            "--until-lsn",
            "9",
        ],
    ];
    for extra in cases {
        let o = p.restore(&dest, &extra);
        assert_eq!(o.status.code(), Some(2), "{extra:?}: {}", stderr(&o));
        assert_no_leak(&o, &[&arc, s(&p.root)]);
        p.assert_nothing_published(&dest);
    }
}

/// 範囲外・欠落・別 DB・改ざんは 1 で拒否し、何も公開せず一時ファイルも残さない。
#[test]
fn recover14_pitr_runtime_rejections_exit_1_and_publish_nothing() {
    let p = build("pitr-cli-reject", 1);
    let dest = p.root.join("restored.redb");
    let last = p.base_lsn + 6;
    let arc = s(&p.archive).to_string();
    let check = |o: Output, what: &str| {
        assert_eq!(o.status.code(), Some(1), "{what}: {}", stderr(&o));
        assert_no_leak(&o, &[&arc, s(&p.root)]);
        // LSN・時刻の値は出力に出さない。
        assert!(
            !stderr(&o).contains(&last.to_string()),
            "{what}: {}",
            stderr(&o)
        );
        p.assert_nothing_published(&dest);
    };

    // 末尾より後・ベースの終点以前・時刻が範囲外。黙って末尾まで適用しない。
    check(
        p.restore_archive(&dest, &["--until-lsn", &(last + 1).to_string()]),
        "after the end",
    );
    check(
        p.restore_archive(&dest, &["--until-lsn", &p.base_lsn.to_string()]),
        "at the base",
    );
    check(
        p.restore_archive(&dest, &["--until-lsn", "1"]),
        "before the base",
    );
    check(
        p.restore_archive(&dest, &["--until-time", "2999-01-01T00:00:00Z"]),
        "time after the end",
    );
    check(
        p.restore_archive(&dest, &["--until-time", "2000-01-01T00:00:00Z"]),
        "time before the base",
    );

    // 別 DB のアーカイブ。
    let other = common::TempFixtureDir::new("pitr-cli-reject-other");
    let other_db = PathBuf::from(other.db_path_str());
    let other_arc = p.root.join("other-archive");
    make_private_dir(&other_arc);
    write_rows(&other_db, 1..=9);
    archive(&other_db, &other_arc, 1);
    check(
        p.restore(
            &dest,
            &[
                "--change-log-archive-dir",
                s(&other_arc),
                "--until-lsn",
                &last.to_string(),
            ],
        ),
        "foreign archive",
    );

    // 改ざん（停止点より後ろのセグメントでも拒否される）と欠落。
    let mut files: Vec<PathBuf> = std::fs::read_dir(&p.archive)
        .expect("dir")
        .map(|e| e.expect("entry").path())
        .collect();
    files.sort();
    let tail = files.last().expect("segment").clone();
    let original = std::fs::read(&tail).expect("read");
    let mut bad = original.clone();
    let n = bad.len();
    bad[n - 1] ^= 0xff;
    std::fs::write(&tail, &bad).expect("tamper");
    check(
        p.restore_archive(&dest, &["--until-lsn", &(p.base_lsn + 1).to_string()]),
        "tampered tail",
    );
    std::fs::write(&tail, &original).expect("restore bytes");

    let middle = files[files.len() / 2].clone();
    let held = p.root.join("held.seg");
    std::fs::rename(&middle, &held).expect("move");
    check(
        p.restore_archive(&dest, &["--until-lsn", &last.to_string()]),
        "missing segment",
    );
    std::fs::rename(&held, &middle).expect("restore file");

    // 戻せば通る（拒否の原因が上の操作だけである）。
    let o = p.restore_archive(&dest, &["--until-lsn", &last.to_string()]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&dest), (1..=9).collect::<Vec<_>>());
}
