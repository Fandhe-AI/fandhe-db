//! `wire-server change-log-cleanup` と `change-log-verify --base` の外形検証（Issue #3092・親 #2164・
//! RECOVER-14 (7) のポインタ）。バイナリを子プロセスとして起動し、終了コード・出力・アーカイブ先のファイルを
//! 確かめる。変更ログを有効にした DB は engine の公開 API で作る（サーバーを起動しない）。
//! 受け入れ条件「保持しているフルバックアップから PITR できる区間は消さない」を、削除の後の復元で
//! 実地に確かめ、dry-run が既定であること・引数の不備が終了コード 2 で何も消さないことを固定する。

use crate::common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engine::storage::{ArchiveOptions, RowInput, Storage, Visibility};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");

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

fn make_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(path).expect("mkdir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

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
                    metadata: b"note",
                },
            )
            .expect("put");
    }
}

/// 未アーカイブのレコードを 1 レコード 1 セグメントで `dir` へ書き、水位以下の行を刈り込む。
fn archive_and_prune(db: &Path, dir: &Path) {
    let mut storage = Storage::open(db).expect("open");
    storage.enable_change_log().expect("enable change log");
    let mut opts = ArchiveOptions::new(dir);
    opts.segment_bytes = 1;
    opts.retain_records = Some(0);
    let report = storage.archive_change_log(&opts).expect("archive");
    assert!(!report.skipped);
}

fn row_ids(db: &Path) -> Vec<u64> {
    let storage = Storage::open(db).expect("open");
    let mut ids: Vec<u64> = storage.scan().expect("scan").iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read archive dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "seg"))
        .collect();
    v.sort();
    v
}

struct Setup {
    _fx: common::TempFixtureDir,
    root: PathBuf,
    db: PathBuf,
    base: PathBuf,
    archive: PathBuf,
    base_lsn: u64,
}

/// ids 1..=3 → 書き出し・刈り込み → フルバックアップ → ids 4..=6 → 書き出し・刈り込み。
fn build(label: &str) -> Setup {
    let fx = common::TempFixtureDir::new(label);
    let db = PathBuf::from(fx.db_path_str());
    let root = db.parent().expect("parent").join("out");
    make_private_dir(&root);
    let base = root.join("base.redb");
    let archive = root.join("archive");
    make_private_dir(&archive);
    write_rows(&db, 1..=3);
    archive_and_prune(&db, &archive);
    let o = run(&["backup", "--db", s(&db), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let base_lsn = engine::storage::chain_point_of(&base)
        .expect("chain point")
        .end_lsn;
    write_rows(&db, 4..=6);
    archive_and_prune(&db, &archive);
    Setup {
        _fx: fx,
        root,
        db,
        base,
        archive,
        base_lsn,
    }
}

/// dry-run は何も消さず、`--execute` で保持するバックアップの終点までだけが消え、削除の後も
/// `--base` 付きの検証が成功し（なしは厳密な検査のため失敗）、そのバックアップから PITR できる。
#[test]
fn recover14_cleanup_removes_only_what_the_retained_backup_does_not_need() {
    let p = build("cl-cleanup-e2e");
    let before = segments(&p.archive);
    assert!(before.len() >= 6, "{before:?}");

    let dry = run(&[
        "change-log-cleanup",
        "--archive-dir",
        s(&p.archive),
        "--backup",
        s(&p.base),
    ]);
    assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
    assert!(stdout(&dry).contains("(dry-run)"), "{}", stdout(&dry));
    assert!(!stdout(&dry).contains(s(&p.root)), "no paths in the output");
    assert_eq!(segments(&p.archive), before, "dry-run deletes nothing");

    let done = run(&[
        "change-log-cleanup",
        "--archive-dir",
        s(&p.archive),
        "--backup",
        s(&p.base),
        "--db",
        s(&p.db),
        "--execute",
    ]);
    assert_eq!(done.status.code(), Some(0), "{}", stderr(&done));
    assert!(stdout(&done).contains("(removed)"), "{}", stdout(&done));
    let after = segments(&p.archive);
    assert!(after.len() < before.len() && !after.is_empty());

    let verify = run(&[
        "change-log-verify",
        "--archive-dir",
        s(&p.archive),
        "--db",
        s(&p.db),
        "--base",
        s(&p.base),
    ]);
    assert_eq!(verify.status.code(), Some(0), "{}", stderr(&verify));
    let strict = run(&[
        "change-log-verify",
        "--archive-dir",
        s(&p.archive),
        "--db",
        s(&p.db),
    ]);
    assert_eq!(strict.status.code(), Some(1), "{}", stderr(&strict));

    // 保持しているバックアップからの PITR は、削除の後も成り立つ。
    let dest = p.root.join("restored.redb");
    let lsn = (p.base_lsn + 2).to_string();
    let o = run(&[
        "restore",
        "--from",
        s(&p.base),
        "--db",
        s(&dest),
        "--change-log-archive-dir",
        s(&p.archive),
        "--until-lsn",
        &lsn,
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&dest), vec![1, 2, 3, 4, 5]);
}

/// 引数の不備は終了コード 2 で、何も消さず、値を echo しない。
#[test]
fn recover14_cleanup_argument_errors_exit_2_and_remove_nothing() {
    let p = build("cl-cleanup-args");
    let before = segments(&p.archive);
    let arc = s(&p.archive);
    let base = s(&p.base);
    for args in [
        vec!["change-log-cleanup"],
        vec!["change-log-cleanup", "--archive-dir", arc],
        vec!["change-log-cleanup", "--backup", base],
        vec![
            "change-log-cleanup",
            "--archive-dir",
            arc,
            "--backup",
            base,
            "--keep-secs",
            "abc",
        ],
        vec![
            "change-log-cleanup",
            "--archive-dir",
            arc,
            "--backup",
            base,
            "--execute",
            "--execute",
        ],
        vec![
            "change-log-cleanup",
            "--archive-dir",
            arc,
            "--backup",
            "/nonexistent-backup-for-cleanup",
            "--execute",
        ],
        vec!["change-log-cleanup", "--bogus"],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(!stderr(&out).contains("/nonexistent-backup-for-cleanup"));
    }
    assert_eq!(segments(&p.archive), before);
}

/// 実行時の拒否（別 DB のバックアップ・増分ファイル）は終了コード 1 で、何も消さない。
#[test]
fn recover14_cleanup_runtime_rejections_exit_1_and_remove_nothing() {
    let p = build("cl-cleanup-reject");
    let before = segments(&p.archive);
    let seg = before.first().expect("segment");
    let out = run(&[
        "change-log-cleanup",
        "--archive-dir",
        s(&p.archive),
        "--backup",
        s(seg),
        "--execute",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not a full backup"),
        "{}",
        stderr(&out)
    );

    let other = build("cl-cleanup-reject-other");
    let out = run(&[
        "change-log-cleanup",
        "--archive-dir",
        s(&p.archive),
        "--backup",
        s(&other.base),
        "--execute",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(segments(&p.archive), before);
}
