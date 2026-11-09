//! `stale_temp` の単体テスト（Issue #3094。名前の形式は本テスト内の簡易判定を使う）。

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use super::*;

fn is_name(n: &str) -> bool {
    n.strip_prefix(".t-")
        .and_then(|r| r.strip_suffix(".tmp"))
        .is_some_and(|r| is_decimal(r, 10))
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
    sweep_stale_temps(dir, is_name, &[]).expect("sweep")
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
    assert!(sweep_stale_temps(&d, is_name, &[]).is_err());
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

/// 異常終了で残った uid 確認用ファイル（probe 形式）は、呼び出し側の一時名の判定に一致しなくても
/// 次の掃除が一時名と同じ安全条件で回収する。
#[test]
fn leftover_probe_files_are_reclaimed_but_unsafe_ones_are_kept() {
    let d = tdir("probe");
    put(&d, ".fandhe-sweep-probe-4242-0123456789abcdef-0.tmp", 0o600);
    put(&d, ".fandhe-sweep-probe-4242-0123456789abcdef-1.tmp", 0o644);
    put(&d, ".fandhe-sweep-probe-bad.tmp", 0o600);
    let s = sweep(&d);
    assert_eq!((s.removed, s.skipped), (1, 1));
    assert!(d
        .join(".fandhe-sweep-probe-4242-0123456789abcdef-1.tmp")
        .exists());
    assert!(d.join(".fandhe-sweep-probe-bad.tmp").exists());
}

/// 予約名の判定は復元・アーカイブ・probe の各形式に一致し、近い別名には一致しない。
#[test]
fn reserved_temp_name_covers_every_swept_format() {
    for n in [
        ".fandhe-restore-4242-7.tmp",
        ".fandhe-restore-4242-0123456789abcdef-7.tmp",
        ".fandhe-seg-4242-0123456789abcdef-7.tmp",
        ".fandhe-sweep-probe-4242-0123456789abcdef-0.tmp",
    ] {
        assert!(is_reserved_temp_name(n), "{n}");
    }
    for n in [
        "backup.redb",
        ".fandhe-restore-decoy.tmp",
        ".fandhe-restore-4242-7.tmp.bak",
        ".fandhe-seg-4242-7.tmp",
        ".fandhe-verify-4242-0123456789abcdef-7.tmp",
    ] {
        assert!(!is_reserved_temp_name(n), "{n}");
    }
}

#[test]
fn protected_paths_are_never_removed() {
    let d = tdir("protected");
    put(&d, ".t-1.tmp", 0o600);
    put(&d, ".t-2.tmp", 0o600);
    put(&d, ".t-3.tmp", 0o600);
    // 別ディレクトリ経由のシンボリックリンク・ハードリンクで指された実体も守る。
    let other = tdir("protected-other");
    symlink(d.join(".t-2.tmp"), other.join("db-link")).expect("symlink");
    std::fs::hard_link(d.join(".t-3.tmp"), other.join("db-hard")).expect("hard link");
    let by_name = d.join(".t-1.tmp");
    let by_link = other.join("db-link");
    let by_hard = other.join("db-hard");
    let s = sweep_stale_temps(
        &d,
        is_name,
        &[by_name.as_path(), by_link.as_path(), by_hard.as_path()],
    )
    .expect("sweep");
    assert_eq!((s.removed, s.skipped), (0, 3));
    for n in [".t-1.tmp", ".t-2.tmp", ".t-3.tmp"] {
        assert!(d.join(n).exists(), "{n} must stay");
    }
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&other);
}

#[test]
fn unprotected_siblings_are_still_removed_and_missing_protected_is_ignored() {
    let d = tdir("protected-mixed");
    put(&d, ".t-1.tmp", 0o600);
    put(&d, ".t-2.tmp", 0o600);
    let keep = d.join(".t-1.tmp");
    let missing = d.join("no-such-file");
    let s = sweep_stale_temps(&d, is_name, &[keep.as_path(), missing.as_path()]).expect("sweep");
    assert_eq!((s.removed, s.skipped), (1, 1));
    assert!(d.join(".t-1.tmp").exists());
    assert!(!d.join(".t-2.tmp").exists());
    let _ = std::fs::remove_dir_all(&d);
}
fn is_wd(n: &str) -> bool {
    n.strip_prefix(".w-")
        .and_then(|r| r.strip_suffix(".d"))
        .is_some_and(|r| is_decimal(r, 10))
}

fn sweep_wd(dir: &Path, protected: &[&Path]) -> StaleTempSweep {
    sweep_stale_temps_with_work_dirs(dir, is_name, is_wd, protected).expect("sweep")
}

fn mk_wd(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let p = dir.join(name);
    std::fs::create_dir(&p).expect("mkdir wd");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    p
}

#[test]
fn snapshot_and_resync_names_are_matched_strictly() {
    for ok in [
        ".fandhe-snapshot-1-0123456789abcdef-7.d",
        ".fandhe-snapshot-4242-0123456789abcdef-18446744073709551615.d",
    ] {
        assert!(is_snapshot_work_dir_name(ok), "{ok}");
        assert!(is_reserved_temp_name(ok), "{ok}");
    }
    for ok in [
        ".fandhe-snapshot-1-0123456789abcdef-7.tmp",
        // 旧形式（親直下に作っていた一時名）も残骸として受理する。
        ".fandhe-snapshot-1-7.tmp",
    ] {
        assert!(is_snapshot_temp_name(ok), "{ok}");
        assert!(is_reserved_temp_name(ok), "{ok}");
    }
    for bad in [
        ".fandhe-snapshot-1-2.d",
        ".fandhe-snapshot-1-0123456789ABCDEF-7.d",
        ".fandhe-snapshot-1-0123456789abcdef-7.d.bak",
        ".fandhe-snapshot-1-0123456789abcdef-7.tmp",
        ".fandhe-snapshot-12345678901-0123456789abcdef-7.d",
        ".fandhe-snapshot-1-0123456789abcdef-.d",
        ".fandhe-restore-1-0123456789abcdef-7.d",
    ] {
        assert!(!is_snapshot_work_dir_name(bad), "{bad}");
    }
    for bad in [
        ".fandhe-snapshot-1-0123456789abcdef-7.d",
        ".fandhe-snapshot-1-xyz.tmp",
        ".fandhe-snapshot-1-0123456789abcdef-7-9.tmp",
        ".fandhe-snapshot-1-0123456789ABCDEF-7.tmp",
        ".fandhe-snapshot-.tmp",
        ".fandhe-snapshot-1-.tmp",
    ] {
        assert!(!is_snapshot_temp_name(bad), "{bad}");
    }
    let resync = ".fandhe-replica-resync-1-0123456789abcdef-7.d";
    assert!(is_replica_resync_work_dir_name(resync));
    assert!(is_reserved_temp_name(resync));
    assert!(!is_replica_resync_work_dir_name(
        ".fandhe-replica-resync-1-0123456789abcdef-7.redb"
    ));
    // 固定名のベースは作業ディレクトリの中だけで使い、予約名にはしない（サーバーの出力先検査に当たらない）。
    assert!(is_replica_resync_base_name("base.redb"));
    assert!(!is_replica_resync_base_name("base.redb.bak"));
    assert!(!is_reserved_temp_name("base.redb"));
}

#[test]
fn generated_names_match_their_predicates() {
    let w = make_temp_name("fandhe-snapshot", 3, "d");
    let t = make_temp_name("fandhe-snapshot", 4, "tmp");
    let r = make_temp_name("fandhe-replica-resync", 5, "d");
    assert!(is_snapshot_work_dir_name(&w), "{w}");
    assert!(is_snapshot_temp_name(&t), "{t}");
    assert!(is_replica_resync_work_dir_name(&r), "{r}");
    assert!(!w.contains('/'));
}

fn is_base(n: &str) -> bool {
    n == "base.bin"
}

fn never_temp(_: &str) -> bool {
    false
}

#[test]
fn work_dir_entries_use_their_own_predicate() {
    let d = tdir("wd-entries");
    let w = d.join(".w-1.d");
    std::fs::create_dir(&w).expect("mkdir");
    std::fs::set_permissions(&w, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    put(&w, "base.bin", 0o600);
    // 親直下の項目は直下の判定（一致なし）に従い、触らない。
    put(&d, "base.bin", 0o600);
    let s = sweep_stale_temps_with_work_dir_entries(&d, never_temp, is_wd, is_base, &[])
        .expect("sweep");
    assert_eq!((s.removed, s.skipped, s.failed), (1, 0, 0));
    assert!(!w.exists());
    assert!(d.join("base.bin").exists());
    // 中身の判定に一致しない項目を含む作業ディレクトリは触らず skipped に数える。
    let w2 = d.join(".w-2.d");
    std::fs::create_dir(&w2).expect("mkdir");
    std::fs::set_permissions(&w2, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    put(&w2, "base.bin", 0o600);
    put(&w2, "unknown", 0o600);
    let s = sweep_stale_temps_with_work_dir_entries(&d, never_temp, is_wd, is_base, &[])
        .expect("sweep");
    assert_eq!((s.removed, s.skipped), (0, 1));
    assert!(w2.join("base.bin").exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn removes_nested_snapshot_work_dirs_left_inside_a_work_dir() {
    let d = tdir("wd-nested");
    let mk = |p: &Path| {
        std::fs::create_dir(p).expect("mkdir");
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    };
    let w = d.join(".w-1.d");
    mk(&w);
    put(&w, "base.bin", 0o600);
    let nested = w.join(".fandhe-snapshot-1-0123456789abcdef-0.d");
    mk(&nested);
    put(&nested, ".fandhe-snapshot-1-0123456789abcdef-0.tmp", 0o600);
    let s = sweep_stale_temps_with_work_dir_entries(&d, never_temp, is_wd, is_base, &[])
        .expect("sweep");
    assert_eq!((s.removed, s.skipped, s.failed), (1, 0, 0));
    assert!(!w.exists());
    // 入れ子の中に想定外の項目があれば、親ごと触らず見送る。
    let w2 = d.join(".w-2.d");
    mk(&w2);
    let n2 = w2.join(".fandhe-snapshot-1-0123456789abcdef-1.d");
    mk(&n2);
    put(&n2, "unknown", 0o600);
    let s = sweep_stale_temps_with_work_dir_entries(&d, never_temp, is_wd, is_base, &[])
        .expect("sweep");
    assert_eq!((s.removed, s.skipped), (0, 1));
    assert!(n2.join("unknown").exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn restore_work_dir_names_are_matched_strictly() {
    for ok in [
        ".fandhe-restore-1-0123456789abcdef-7.d",
        ".fandhe-restore-4242-0123456789abcdef-18446744073709551615.d",
    ] {
        assert!(is_restore_work_dir_name(ok), "{ok}");
        assert!(is_reserved_temp_name(ok), "{ok}");
    }
    for bad in [
        ".fandhe-restore-1-2.d",
        ".fandhe-restore-1-0123456789ABCDEF-7.d",
        ".fandhe-restore-1-0123456789abcdef-7.d.bak",
        ".fandhe-restore-1-0123456789abcdef-7.tmp",
        ".fandhe-restore-12345678901-0123456789abcdef-7.d",
        ".fandhe-restore-1-0123456789abcdef-123456789012345678901.d",
        ".fandhe-verify-1-0123456789abcdef-7.d",
        ".fandhe-restore-1-0123456789abcdef-.d",
    ] {
        assert!(!is_restore_work_dir_name(bad), "{bad}");
    }
}

#[test]
fn removes_work_dirs_with_temp_files_and_empty_ones() {
    let d = tdir("wd-basic");
    let w1 = mk_wd(&d, ".w-1.d", 0o700);
    put(&w1, ".t-1.tmp", 0o600);
    mk_wd(&d, ".w-2.d", 0o700);
    // 一時名の通常ファイルは従来どおり消える。
    put(&d, ".t-3.tmp", 0o600);
    let s = sweep_wd(&d, &[]);
    assert_eq!((s.removed, s.skipped, s.failed), (3, 0, 0));
    assert!(!d.join(".w-1.d").exists() && !d.join(".w-2.d").exists());
    let mut names = s.removed_names.clone();
    names.sort();
    assert_eq!(names, vec![".t-3.tmp", ".w-1.d", ".w-2.d"]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn removes_setgid_work_dirs_like_work_dir_check() {
    // 親が setgid だと mkdir(0700) の作業ディレクトリは 2700 になる。特殊ビットだけの差は掃除対象。
    let d = tdir("wd-setgid");
    let w = mk_wd(&d, ".w-1.d", 0o2700);
    put(&w, ".t-1.tmp", 0o600);
    let s = sweep_wd(&d, &[]);
    assert_eq!((s.removed, s.skipped, s.failed), (1, 0, 0));
    assert!(!d.join(".w-1.d").exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn leaves_unexpected_work_dirs_and_counts_them() {
    let d = tdir("wd-skip");
    // モードが違う。
    let w = mk_wd(&d, ".w-1.d", 0o750);
    put(&w, ".t-1.tmp", 0o600);
    // 未知の項目を含む。
    let w = mk_wd(&d, ".w-2.d", 0o700);
    put(&w, "other.bin", 0o600);
    put(&w, ".t-1.tmp", 0o600);
    // 項目数が上限を超える。
    let w = mk_wd(&d, ".w-3.d", 0o700);
    for i in 0..=MAX_WORK_DIR_ENTRIES {
        put(&w, &format!(".t-{i}.tmp"), 0o600);
    }
    // ファイルのモードが 0644。
    let w = mk_wd(&d, ".w-4.d", 0o700);
    put(&w, ".t-1.tmp", 0o644);
    // シンボリックリンク。
    let target = mk_wd(&d, "victim", 0o700);
    put(&target, ".t-1.tmp", 0o600);
    symlink(&target, d.join(".w-5.d")).expect("ln");
    // 中身がシンボリックリンク。
    let w = mk_wd(&d, ".w-6.d", 0o700);
    symlink(target.join(".t-1.tmp"), w.join(".t-9.tmp")).expect("ln");
    let s = sweep_wd(&d, &[]);
    assert_eq!((s.removed, s.skipped), (0, 6));
    for n in [".w-1.d", ".w-2.d", ".w-3.d", ".w-4.d", "victim", ".w-6.d"] {
        assert!(d.join(n).exists(), "{n}");
    }
    assert!(target.join(".t-1.tmp").exists());
    assert!(std::fs::symlink_metadata(d.join(".w-5.d")).is_ok());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn keeps_work_dirs_holding_protected_files() {
    let d = tdir("wd-prot");
    let w = mk_wd(&d, ".w-1.d", 0o700);
    put(&w, ".t-1.tmp", 0o600);
    let prot = w.join(".t-1.tmp");
    let s = sweep_wd(&d, &[prot.as_path()]);
    assert_eq!((s.removed, s.skipped), (0, 1));
    assert!(prot.exists());
    // 作業ディレクトリ自身が保護対象でも残る。
    let s = sweep_wd(&d, &[w.as_path()]);
    assert_eq!((s.removed, s.skipped), (0, 1));
    assert!(w.exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn work_dir_sweep_is_skipped_for_shared_parent_and_held_lock() {
    let d = tdir("wd-shared");
    mk_wd(&d, ".w-1.d", 0o700);
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o775)).expect("chmod");
    let s = sweep_wd(&d, &[]);
    assert_eq!(s.outcome, SweepOutcome::SharedDirectory);
    assert!(d.join(".w-1.d").exists());
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let guard = TempDirLock::shared(&d).expect("lock");
    let s = sweep_wd(&d, &[]);
    assert_eq!(s.outcome, SweepOutcome::InUse);
    assert!(d.join(".w-1.d").exists());
    drop(guard);
    assert_eq!(sweep_wd(&d, &[]).removed, 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn plain_sweep_never_touches_work_dirs() {
    let d = tdir("wd-plain");
    mk_wd(&d, ".w-1.d", 0o700);
    let s = sweep(&d);
    assert_eq!((s.removed, s.skipped), (0, 0));
    assert!(d.join(".w-1.d").exists());
    let _ = std::fs::remove_dir_all(&d);
}
