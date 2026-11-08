//! `stale_temp` の単体テスト（Issue #3094。名前の形式は本テスト内の簡易判定を使う）。

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use super::*;

fn is_name(n: &str) -> bool {
    n.strip_prefix(".t-")
        .and_then(|r| r.strip_suffix(".tmp"))
        .is_some_and(|r| is_decimal(r, 10))
}

fn probe(i: u64) -> String {
    format!(".t-9{}{i}.tmp", std::process::id())
}

fn tdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("stale-temp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    d
}

fn put(dir: &Path, name: &str, mode: u32) {
    let p = dir.join(name);
    std::fs::write(&p, b"x").expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn sweep(dir: &Path) -> StaleTempSweep {
    sweep_stale_temps(dir, is_name, &probe).expect("sweep")
}

#[test]
fn removes_matching_regular_files_only() {
    let d = tdir("basic");
    put(&d, ".t-1.tmp", 0o600);
    put(&d, ".t-22.tmp", 0o600);
    for n in [
        ".t-x.tmp",
        ".t-1.tmp.bak",
        "t-1.tmp",
        ".t-.tmp",
        "other.seg",
    ] {
        put(&d, n, 0o600);
    }
    let s = sweep(&d);
    assert_eq!(s.outcome, SweepOutcome::Swept);
    assert_eq!((s.removed, s.skipped, s.failed), (2, 0, 0));
    let mut names = s.removed_names.clone();
    names.sort();
    assert_eq!(names, vec![".t-1.tmp", ".t-22.tmp"]);
    for n in [
        ".t-x.tmp",
        ".t-1.tmp.bak",
        "t-1.tmp",
        ".t-.tmp",
        "other.seg",
    ] {
        assert!(d.join(n).exists(), "{n} must stay");
    }
    // probe は残らない。
    let left = std::fs::read_dir(&d)
        .expect("rd")
        .filter(|e| is_name(e.as_ref().expect("e").file_name().to_str().unwrap_or("")))
        .count();
    assert_eq!(left, 0);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn leaves_unexpected_type_or_mode_and_counts_them() {
    let d = tdir("skip");
    put(&d, ".t-1.tmp", 0o644);
    std::fs::create_dir(d.join(".t-2.tmp")).expect("dir");
    let target = d.join("target.bin");
    std::fs::write(&target, b"keep").expect("w");
    symlink(&target, d.join(".t-3.tmp")).expect("ln");
    let s = sweep(&d);
    assert_eq!((s.removed, s.skipped), (0, 3));
    assert!(d.join(".t-1.tmp").exists() && d.join(".t-2.tmp").is_dir());
    assert!(std::fs::symlink_metadata(d.join(".t-3.tmp")).is_ok());
    assert!(target.exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn skips_when_a_writer_holds_the_shared_lock() {
    let d = tdir("inuse");
    put(&d, ".t-1.tmp", 0o600);
    let guard = TempDirLock::shared(&d).expect("lock");
    let s = sweep(&d);
    assert_eq!(s.outcome, SweepOutcome::InUse);
    assert!(d.join(".t-1.tmp").exists());
    drop(guard);
    let s = sweep(&d);
    assert_eq!(s.removed, 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn shared_lock_failure_is_an_error_not_a_silent_none() {
    let d = tdir("lockfail");
    let missing = d.join("no-such-dir");
    assert!(TempDirLock::shared(&missing).is_err());
    assert!(TempDirLock::shared(&d).is_ok());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn skips_shared_directory() {
    let d = tdir("shared");
    put(&d, ".t-1.tmp", 0o600);
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o770)).expect("chmod");
    let s = sweep(&d);
    assert_eq!(s.outcome, SweepOutcome::SharedDirectory);
    assert!(d.join(".t-1.tmp").exists());
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn empty_directory_and_missing_directory() {
    let d = tdir("empty");
    let s = sweep(&d);
    assert_eq!((s.outcome, s.removed), (SweepOutcome::Swept, 0));
    let _ = std::fs::remove_dir_all(&d);
    assert!(sweep_stale_temps(&d, is_name, &probe).is_err());
}

#[test]
fn reported_names_are_capped_but_count_is_total() {
    let d = tdir("cap");
    let n = MAX_REPORTED_NAMES + 5;
    for i in 0..n {
        put(&d, &format!(".t-{i}.tmp"), 0o600);
    }
    let s = sweep(&d);
    assert_eq!(s.removed, n);
    assert_eq!(s.removed_names.len(), MAX_REPORTED_NAMES);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn name_helpers_are_strict() {
    assert!(is_decimal("123", 3) && !is_decimal("1234", 3) && !is_decimal("", 3));
    assert!(!is_decimal("-1", 3) && !is_decimal("１", 3));
    assert!(is_lower_hex("0a9f", 4) && !is_lower_hex("0A9f", 4) && !is_lower_hex("0a9", 4));
}
