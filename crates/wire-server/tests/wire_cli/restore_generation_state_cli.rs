//! `wire-server restore --generation-state <path>` の外形検証（Issue #3105・RECOVER-17 のポインタ）。
//! 復元をまたぐ世代の高水位が、同じ復元元からの繰り返し復元でも世代を単調に増やし、復元元を書き換えず、
//! 状態ファイルを保持できないときは何も公開しない（fail-closed）ことを、バイナリを子プロセスとして起動して
//! 確かめる。サーバーは起動せず、状態ファイルは fixture ディレクトリ内の `.fandhe-` 以外の名前に置く。

use crate::common;

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engine::storage::{decode_high_water, encode_next_slot, HIGH_WATER_FILE_LEN};

use super::verify_restore_cli::{code, generation_of, leftovers, stderr, write_clean_db};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");
const MSG_REJECTED: &str =
    "restore generation state file is not a private regular file with a valid state";

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn restore_with(src: &Path, dest: &Path, state: &Path) -> Output {
    Command::new(BIN)
        .args([
            "restore",
            "--from",
            s(src),
            "--db",
            s(dest),
            "--generation-state",
            s(state),
        ])
        .output()
        .expect("run restore")
}

/// 状態ファイルの高水位を engine のコーデックで読む。
fn high_water_of(state: &Path) -> u64 {
    decode_high_water(&std::fs::read(state).expect("read state"))
        .expect("decode state")
        .value
}

/// 状態ファイルへ高水位 `v` を書く（0600。実運用の書き込みと同じスロット形式）。
fn write_high_water(state: &Path, v: u64) {
    let st = decode_high_water(&[]).expect("fresh");
    let (off, slot) = encode_next_slot(&st, v).expect("encode");
    let mut bytes = vec![0u8; HIGH_WATER_FILE_LEN];
    bytes[off as usize..off as usize + slot.len()].copy_from_slice(&slot);
    std::fs::write(state, bytes).expect("write state");
    std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

struct Fx {
    _fx: common::TempFixtureDir,
    dir: PathBuf,
    src: PathBuf,
    state: PathBuf,
}

fn fixture(label: &str) -> Fx {
    let fx = common::TempFixtureDir::new(label);
    let dir = PathBuf::from(fx.db_path_str())
        .parent()
        .expect("parent")
        .to_path_buf();
    let src = dir.join("src.redb");
    write_clean_db(&src);
    let state = dir.join("gen-state.bin");
    Fx {
        _fx: fx,
        dir,
        src,
        state,
    }
}

/// 復元が拒否され、宛先も残骸も無く、復元元と状態ファイルが変わらないこと。
fn assert_rejected(f: &Fx, dest: &Path, o: &Output, exit: i32, state_before: Option<&Vec<u8>>) {
    assert_eq!(code(o), Some(exit), "stderr: {}", stderr(o));
    assert!(!dest.exists(), "nothing is published");
    assert!(leftovers(&f.dir).is_empty(), "{:?}", leftovers(&f.dir));
    if let Some(before) = state_before {
        assert_eq!(&std::fs::read(&f.state).expect("state"), before);
    }
}

/// 同じ復元元から状態ファイルを共有して繰り返し復元すると世代が厳密に増え、状態ファイルが最後の世代を持つ。
/// 復元元は変わらない。
#[test]
fn recover17_shared_state_makes_repeated_restores_strictly_increase() {
    let f = fixture("restore-gen-state-repeat");
    let src_before = std::fs::read(&f.src).expect("read src");
    let mut previous = 0;
    for i in 0..3 {
        let dest = f.dir.join(format!("restored-{i}.redb"));
        let o = restore_with(&f.src, &dest, &f.state);
        assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
        let g = generation_of(&dest);
        assert!(g > previous, "restore {i}: {g} must exceed {previous}");
        assert_eq!(high_water_of(&f.state), g, "the state holds the latest");
        previous = g;
        let v = Command::new(BIN)
            .args(["verify", "--db", s(&dest)])
            .output()
            .expect("verify");
        assert_eq!(code(&v), Some(0), "stderr: {}", stderr(&v));
    }
    assert_eq!(std::fs::read(&f.src).expect("read src"), src_before);
    assert!(leftovers(&f.dir).is_empty(), "{:?}", leftovers(&f.dir));
    let mode = std::fs::metadata(&f.state)
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "the state file is private");
}

/// 高水位が時計より十分大きいとき、世代は時計ではなく高水位を超える（状態ファイルが単調性を担う）。
#[test]
fn recover17_a_high_water_above_the_clock_is_exceeded() {
    let f = fixture("restore-gen-state-above-clock");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_micros() as u64;
    let hw = now + 1_000_000_000_000;
    write_high_water(&f.state, hw);
    let dest = f.dir.join("restored.redb");
    let o = restore_with(&f.src, &dest, &f.state);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert_eq!(generation_of(&dest), hw + 1);
    assert_eq!(high_water_of(&f.state), hw + 1);
}

/// 状態ファイルが壊れている・別のファイル・長さ不正・上限超過のとき、終了コード 1・固定の文言で
/// 何も公開せず、状態ファイルは書き換えない。
#[test]
fn recover17_an_invalid_state_file_publishes_nothing_and_is_left_untouched() {
    let f = fixture("restore-gen-state-invalid");
    let src_before = std::fs::read(&f.src).expect("read src");
    let mut valid = Vec::new();
    {
        write_high_water(&f.state, 7);
        valid.extend(std::fs::read(&f.state).expect("read"));
    }
    let mut both_corrupt = valid.clone();
    both_corrupt[10] ^= 0xff;
    both_corrupt[40] ^= 0xff;
    let mut one_corrupt = valid.clone();
    one_corrupt[10] ^= 0xff;
    let mut over_limit = vec![0u8; HIGH_WATER_FILE_LEN];
    over_limit[..8].copy_from_slice(b"FDRGHW01");
    over_limit[8..16].copy_from_slice(&1u64.to_le_bytes());
    over_limit[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("both-corrupt", both_corrupt),
        ("one-corrupt", one_corrupt),
        ("bad-length", vec![1u8; 10]),
        (
            "foreign",
            b"not a state file, some other content that is 64 bytes long.....".to_vec(),
        ),
        ("over-limit", over_limit),
    ];
    for (name, bytes) in cases {
        std::fs::write(&f.state, &bytes).expect("write state");
        std::fs::set_permissions(&f.state, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let dest = f.dir.join(format!("restored-{name}.redb"));
        let o = restore_with(&f.src, &dest, &f.state);
        assert!(stderr(&o).contains(MSG_REJECTED), "{name}: {}", stderr(&o));
        assert_rejected(&f, &dest, &o, 1, Some(&bytes));
    }
    assert_eq!(std::fs::read(&f.src).expect("read src"), src_before);
}

/// シンボリックリンク・ディレクトリ・`--from`／`--db` と同じパスは、何も開かずに終了コード 2 で拒否する。
#[test]
fn recover17_unsafe_state_paths_are_rejected_before_anything_is_opened() {
    let f = fixture("restore-gen-state-paths");
    let src_before = std::fs::read(&f.src).expect("read src");
    let dest = f.dir.join("restored.redb");

    let target = f.dir.join("target.bin");
    write_high_water(&target, 5);
    let link = f.dir.join("link.bin");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let o = restore_with(&f.src, &dest, &link);
    assert_rejected(&f, &dest, &o, 2, None);
    assert_eq!(high_water_of(&target), 5);

    let as_dir = f.dir.join("a-dir");
    std::fs::create_dir(&as_dir).expect("mkdir");
    let o = restore_with(&f.src, &dest, &as_dir);
    assert_rejected(&f, &dest, &o, 2, None);

    // 復元元そのもの・復元先と同じパス（別の綴りを含む）。
    let o = restore_with(&f.src, &dest, &f.src);
    assert_rejected(&f, &dest, &o, 2, None);
    let alias = f.dir.join(".").join("restored.redb");
    let o = restore_with(&f.src, &dest, &alias);
    assert_rejected(&f, &dest, &o, 2, None);

    // 値の欠落と重複。
    let o = Command::new(BIN)
        .args([
            "restore",
            "--from",
            s(&f.src),
            "--db",
            s(&dest),
            "--generation-state",
        ])
        .output()
        .expect("run");
    assert_rejected(&f, &dest, &o, 2, None);
    let o = Command::new(BIN)
        .args([
            "restore",
            "--from",
            s(&f.src),
            "--db",
            s(&dest),
            "--generation-state",
            s(&f.state),
            "--generation-state",
            s(&f.state),
        ])
        .output()
        .expect("run");
    assert_rejected(&f, &dest, &o, 2, None);
    assert!(!f.state.exists(), "an argument error creates no state file");
    assert_eq!(std::fs::read(&f.src).expect("read src"), src_before);
}

/// 親ディレクトリがグループから書き込める場合は、状態ファイルを作らず何も公開しない。
#[test]
fn recover17_a_shared_state_parent_publishes_nothing() {
    let f = fixture("restore-gen-state-shared-parent");
    let shared = f.dir.join("shared");
    std::fs::create_dir(&shared).expect("mkdir");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).expect("chmod");
    let state = shared.join("gen-state.bin");
    let dest = f.dir.join("restored.redb");
    let o = restore_with(&f.src, &dest, &state);
    assert!(stderr(&o).contains(MSG_REJECTED), "{}", stderr(&o));
    assert_rejected(&f, &dest, &o, 1, None);
    assert!(!state.exists());
}

/// 状態ファイルの権限が緩い（他者が読み書きできる）場合も拒否する。
#[test]
fn recover17_a_permissive_state_file_publishes_nothing() {
    let f = fixture("restore-gen-state-permissive");
    write_high_water(&f.state, 3);
    std::fs::set_permissions(&f.state, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let before = std::fs::read(&f.state).expect("read");
    let dest = f.dir.join("restored.redb");
    let o = restore_with(&f.src, &dest, &f.state);
    assert_rejected(&f, &dest, &o, 1, Some(&before));
}

/// 別のプロセスが状態ファイルのロックを握っている間は、終了コード 1（`in use`）で何も公開しない。
#[test]
fn recover17_a_locked_state_file_publishes_nothing() {
    let f = fixture("restore-gen-state-locked");
    write_high_water(&f.state, 3);
    let holder = File::options()
        .read(true)
        .write(true)
        .open(&f.state)
        .expect("open");
    holder.lock().expect("lock");
    let before = std::fs::read(&f.state).expect("read");
    let dest = f.dir.join("restored.redb");
    let o = restore_with(&f.src, &dest, &f.state);
    assert!(
        stderr(&o).contains("is in use by another restore"),
        "{}",
        stderr(&o)
    );
    assert_rejected(&f, &dest, &o, 1, Some(&before));
    drop(holder);
    // ロックを解放すれば復元できる。
    let o = restore_with(&f.src, &dest, &f.state);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
}
