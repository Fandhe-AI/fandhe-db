//! `wire-server restore` が復元の完了時に変更ログの timeline を分岐させ、元の系統と復元した系統の
//! 履歴の混在を拒否することの外形検証（Issue #3100・RECOVER-14・RECOVER-17 のポインタ）。
//! バイナリを子プロセスとして起動し、終了コード・宛先と一時名の残置・出力の非漏えいを確かめる。
//! 変更ログを有効にした DB は engine の公開 API で作る（サーバーは起動せず、ポート・固定パスに依存しない）。

use crate::common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engine::storage::{chain_point_of, RowInput, Storage, Visibility};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");
const SECRET_NOTE: &[u8] = b"secret-payload-forkz";

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run")
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn code(o: &Output) -> Option<i32> {
    o.status.code()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// 出力先に使える（他者が書けない）ディレクトリを作る。umask に依らず 0755 にする。
fn make_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(path).expect("mkdir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// 変更ログを有効にして `ids` の行を足す（開いて閉じる。続きの書き込みも連続性検査を通る）。
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

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".fandhe-"))
        .collect()
}

fn backup(db: &Path, out: &Path) {
    let o = run(&["backup", "--db", s(db), "--out", s(out)]);
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
}

fn incremental(db: &Path, base: &Path, out: &Path) {
    let o = run(&[
        "backup-incremental",
        "--db",
        s(db),
        "--base",
        s(base),
        "--out",
        s(out),
    ]);
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
}

fn restore(base: &Path, dest: &Path, incs: &[&Path], extra: &[&str]) -> Output {
    let mut args = vec!["restore", "--from", s(base), "--db", s(dest)];
    for p in incs {
        args.push("--incremental");
        args.push(s(p));
    }
    args.extend_from_slice(extra);
    run(&args)
}

struct Lineages {
    _fx: common::TempFixtureDir,
    root: PathBuf,
    base: PathBuf,
    inc_orig: PathBuf,
    /// 復元した系統のフルバックアップと増分。
    base_r: PathBuf,
    inc_r: PathBuf,
}

/// 元の系統（ベース + 増分）と、ベースを復元して書き込みを続けた系統（フルバックアップ + 増分）。
fn build(label: &str) -> Lineages {
    let fx = common::TempFixtureDir::new(label);
    let db = PathBuf::from(fx.db_path_str());
    let root = db.parent().expect("parent").join("out");
    make_private_dir(&root);
    let base = root.join("base.redb");
    let inc_orig = root.join("orig.seg");

    write_rows(&db, 1..=3);
    backup(&db, &base);
    write_rows(&db, 4..=5);
    incremental(&db, &base, &inc_orig);

    // ベースを復元して稼働させ、自分のフルバックアップと増分を取る。
    let restored = root.join("restored.redb");
    let o = restore(&base, &restored, &[], &[]);
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    write_rows(&restored, 100..=101);
    let base_r = root.join("base_r.redb");
    backup(&restored, &base_r);
    write_rows(&restored, 102..=103);
    let inc_r = root.join("r.seg");
    incremental(&restored, &base_r, &inc_r);
    assert!(
        chain_point_of(&base_r).expect("point").timeline
            > chain_point_of(&base).expect("point").timeline,
        "the restored lineage has a new timeline"
    );
    Lineages {
        _fx: fx,
        root,
        base,
        inc_orig,
        base_r,
        inc_r,
    }
}

fn assert_refused(l: &Lineages, dest: &Path, o: &Output) {
    assert_eq!(code(o), Some(1), "stderr: {}", stderr(o));
    assert!(!dest.exists(), "a rejected restore publishes nothing");
    assert!(leftovers(&l.root).is_empty(), "{:?}", leftovers(&l.root));
    let all = format!("{}{}", String::from_utf8_lossy(&o.stdout), stderr(o));
    for needle in ["secret-payload-forkz", "tenant-a"] {
        assert!(!all.contains(needle), "output leaked {needle:?}: {all}");
    }
}

/// 元の系統の増分を復元した系統のベースへ、復元した系統の増分を元のベースへ適用すると拒否される。
#[test]
fn recover14_mixed_lineages_are_refused_in_both_directions() {
    let l = build("fork-cli-mixed");
    let dest = l.root.join("a.redb");
    let o = restore(&l.base_r, &dest, &[&l.inc_orig], &[]);
    assert_refused(&l, &dest, &o);
    let dest = l.root.join("b.redb");
    let o = restore(&l.base, &dest, &[&l.inc_r], &[]);
    assert_refused(&l, &dest, &o);
    // 両系統の増分を 1 本の列に混ぜても拒否される。
    let dest = l.root.join("c.redb");
    let o = restore(&l.base, &dest, &[&l.inc_orig, &l.inc_r], &[]);
    assert_refused(&l, &dest, &o);
}

/// 対照: 各系統は自分自身の連鎖なら復元できる。
#[test]
fn recover14_each_lineage_restores_with_its_own_chain() {
    let l = build("fork-cli-own");
    for (base, inc, name) in [
        (&l.base, &l.inc_orig, "orig.redb"),
        (&l.base_r, &l.inc_r, "r.redb"),
    ] {
        let dest = l.root.join(name);
        let o = restore(base, &dest, &[inc], &[]);
        assert_eq!(code(&o), Some(0), "{name}: {}", stderr(&o));
        let v = run(&["verify", "--db", s(&dest)]);
        assert_eq!(code(&v), Some(0), "{name}: {}", stderr(&v));
    }
}

/// `--generation-state` を共有した同じベースからの兄弟の復元は、別々の timeline になる。
#[test]
fn recover17_sibling_restores_with_a_shared_state_get_distinct_timelines() {
    let fx = common::TempFixtureDir::new("fork-cli-siblings");
    let db = PathBuf::from(fx.db_path_str());
    let root = db.parent().expect("parent").join("out");
    make_private_dir(&root);
    let base = root.join("base.redb");
    write_rows(&db, 1..=2);
    backup(&db, &base);
    let state = root.join("gen-state.bin");
    let a = root.join("a.redb");
    let b = root.join("b.redb");
    for dest in [&a, &b] {
        let o = restore(&base, dest, &[], &["--generation-state", s(&state)]);
        assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    }
    let ta = chain_point_of(&a).expect("point").timeline;
    let tb = chain_point_of(&b).expect("point").timeline;
    assert_ne!(ta, tb, "siblings must not share a timeline");
    assert!(ta > chain_point_of(&base).expect("point").timeline);
}
