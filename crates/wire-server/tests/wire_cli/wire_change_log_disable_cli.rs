//! `wire-server change-log-disable` の外形検証（Issue #3093・親 #2166・RECOVER-14 ポインタ。設計は
//! `docs/design/change-log-pitr.md` の D13）。バイナリを子プロセスとして起動し、終了コード・出力・
//! 無効化後の DB の状態を確かめる。変更ログを有効にした DB は engine の公開 API で作る（サーバーを
//! 起動しない。ポート・固定パスに依存しない）。受け入れ条件「無効化後も整合検査を通る」「無効化後の
//! 増分・PITR の要求が拒否される」と、未アーカイブ分の拒否・明示破棄・使用中 DB の拒否を固定する。

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

fn put_rows(storage: &Storage, ids: std::ops::RangeInclusive<u64>) {
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

struct Fx {
    _fx: common::TempFixtureDir,
    root: PathBuf,
    db: PathBuf,
    archive: PathBuf,
}

/// 変更ログを有効にした DB を作り、`archived` が真ならアーカイブを追いつかせる。
fn enabled_db(label: &str, archived: bool) -> Fx {
    let fx = common::TempFixtureDir::new(label);
    let db = PathBuf::from(fx.db_path_str());
    let root = db.parent().expect("parent").join("out");
    make_private_dir(&root);
    let archive = root.join("archive");
    make_private_dir(&archive);
    let mut storage = Storage::open(&db).expect("open");
    storage.enable_change_log().expect("enable");
    put_rows(&storage, 1..=3);
    if archived {
        storage
            .archive_change_log(&ArchiveOptions::new(&archive))
            .expect("archive");
    }
    drop(storage);
    Fx {
        _fx: fx,
        root,
        db,
        archive,
    }
}

#[test]
fn recover14_disable_rejects_bad_usage_with_exit_2() {
    for args in [
        vec!["change-log-disable"],
        vec!["change-log-disable", "--bogus"],
        vec!["change-log-disable", "--db"],
        vec!["change-log-disable", "--db", "a", "--db", "b"],
    ] {
        let o = run(&args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
    }
}

#[test]
fn recover14_disable_refuses_unarchived_records_and_changes_nothing() {
    let f = enabled_db("disable-unarchived", false);
    let o = run(&["change-log-disable", "--db", s(&f.db)]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("not archived"), "{err}");
    assert!(!err.contains(s(&f.db)), "{err}");
    // 拒否後も有効のまま（フラグなしの Storage は書き込めない）。
    let storage = Storage::open(&f.db).expect("open");
    assert!(storage.change_log_required());
}

#[test]
fn recover14_disable_with_discard_succeeds_and_database_passes_verify() {
    let f = enabled_db("disable-discard", false);
    let o = run(&[
        "change-log-disable",
        "--db",
        s(&f.db),
        "--discard-unarchived",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("change log disabled"), "{}", stdout(&o));
    assert!(!stdout(&o).contains(s(&f.db)));

    // 受け入れ条件 A: 無効化後も整合検査を通る。
    let v = run(&["verify", "--db", s(&f.db)]);
    assert_eq!(v.status.code(), Some(0), "{}{}", stdout(&v), stderr(&v));

    // フラグなしで書き込める（有効にしていない DB と同じ）。
    let storage = Storage::open(&f.db).expect("open");
    assert!(!storage.change_log_required());
    put_rows(&storage, 10..=10);
}

#[test]
fn recover14_disable_after_archive_needs_no_discard() {
    let f = enabled_db("disable-archived", true);
    let o = run(&["change-log-disable", "--db", s(&f.db)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
}

#[test]
fn recover14_disable_refuses_a_database_that_never_had_a_change_log() {
    let fx = common::TempFixtureDir::new("disable-never");
    let db = PathBuf::from(fx.db_path_str());
    drop(Storage::open(&db).expect("open"));
    let o = run(&["change-log-disable", "--db", s(&db)]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("not enabled"), "{}", stderr(&o));
}

#[test]
fn recover14_disable_refuses_a_database_in_use() {
    let f = enabled_db("disable-in-use", true);
    let holder = Storage::open(&f.db).expect("open holder");
    let o = run(&["change-log-disable", "--db", s(&f.db)]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("in use"), "{}", stderr(&o));
    drop(holder);
}

#[test]
fn recover14_incremental_pitr_and_verify_are_refused_after_disable() {
    let f = enabled_db("disable-refusals", true);
    let before = f.root.join("before.redb");
    let o = run(&["backup", "--db", s(&f.db), "--out", s(&before)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    let o = run(&["change-log-disable", "--db", s(&f.db)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    // 無効化前のベースからの増分は拒否される。
    let inc = f.root.join("inc.seg");
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&f.db),
        "--base",
        s(&before),
        "--out",
        s(&inc),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(!inc.exists());

    // 無効化後のフルバックアップを起点にした PITR は拒否され、何も公開されない。
    let after = f.root.join("after.redb");
    let o = run(&["backup", "--db", s(&f.db), "--out", s(&after)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let dest = f.root.join("restored.redb");
    let o = run(&[
        "restore",
        "--from",
        s(&after),
        "--db",
        s(&dest),
        "--change-log-archive-dir",
        s(&f.archive),
        "--until-lsn",
        "1",
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(!dest.exists());

    // アーカイブの DB 突き合わせも拒否される。
    let o = run(&[
        "change-log-verify",
        "--archive-dir",
        s(&f.archive),
        "--db",
        s(&f.db),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
}

#[test]
fn recover14_server_restart_hint_points_at_change_log_disable() {
    let f = enabled_db("disable-hint", true);
    let users = f.root.join("users.json");
    common::write_empty_user_store(s(&users));
    let out = Command::new(BIN)
        .args([
            "--users",
            s(&users),
            "--db",
            s(&f.db),
            "--bind",
            "127.0.0.1:0",
        ])
        .output()
        .expect("run");
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("change-log-disable"),
        "{}",
        stderr(&out)
    );

    // 無効化後はフラグなしで起動できる。
    let o = run(&["change-log-disable", "--db", s(&f.db)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let mut child = Command::new(BIN)
        .args([
            "--users",
            s(&users),
            "--db",
            s(&f.db),
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let listening = common::wait_for_listening(&mut child, std::time::Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    assert!(listening, "a disabled database starts without the flag");
}
