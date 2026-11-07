//! `wire-server backup-incremental` と `restore --incremental` の外形検証（Issue #2168・親 #2164・
//! RECOVER-13・RECOVER-14 ポインタ）。バイナリを子プロセスとして起動し、終了コード・出力・ファイルの状態を
//! 確かめる。変更ログを有効にした DB は engine の公開 API で作る（サーバーを起動しない。ポート・固定パスに
//! 依存しない）。受け入れ条件「ベース＋増分を復元すると最新と一致する」を、フルバックアップ → 書き込み →
//! 増分 → 書き込み → 増分 → 復元の順で確かめ、連鎖の切断・順序違反・別 DB・改ざん・上書きの拒否と、
//! 拒否時に何も公開されず一時ファイルも残らないことを固定する。

use crate::common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engine::storage::{RowInput, Storage, Visibility};

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

struct Chain {
    _fx: common::TempFixtureDir,
    root: PathBuf,
    db: PathBuf,
    base: PathBuf,
    inc1: PathBuf,
    inc2: PathBuf,
}

/// フルバックアップ（ids 1..=3 の時点）→ 書き込み → 増分 1 → 書き込み → 増分 2（増分 1 をベースにした連鎖）。
fn build_chain(label: &str) -> Chain {
    let fx = common::TempFixtureDir::new(label);
    let root = PathBuf::from(fx.db_path_str())
        .parent()
        .expect("parent")
        .join("out");
    make_private_dir(&root);
    let db = PathBuf::from(fx.db_path_str());
    let base = root.join("base.redb");
    let inc1 = root.join("inc1.seg");
    let inc2 = root.join("inc2.seg");

    write_rows(&db, 1..=3);
    let o = run(&["backup", "--db", s(&db), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    write_rows(&db, 4..=5);
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&db),
        "--base",
        s(&base),
        "--out",
        s(&inc1),
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("backup-incremental written: records=2"),
        "{}",
        stdout(&o)
    );
    assert_no_leak(&o, &[]);

    write_rows(&db, 6..=7);
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&db),
        "--base",
        s(&inc1),
        "--out",
        s(&inc2),
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("records=2"),
        "chained increments start after the previous increment: {}",
        stdout(&o)
    );
    Chain {
        _fx: fx,
        root,
        db,
        base,
        inc1,
        inc2,
    }
}

fn restore(chain: &Chain, dest: &Path, incrementals: &[&Path]) -> Output {
    let mut args = vec!["restore", "--from", s(&chain.base), "--db", s(dest)];
    for p in incrementals {
        args.push("--incremental");
        args.push(s(p));
    }
    run(&args)
}

/// 受け入れ条件: フルバックアップ＋増分 2 つを復元すると最新の DB と一致し、整合検査にも通る。
#[test]
fn recover13_base_plus_increments_restore_to_the_latest_state() {
    let c = build_chain("inc-cli-ok");
    let before: Vec<Vec<u8>> = [&c.base, &c.inc1, &c.inc2]
        .iter()
        .map(|p| std::fs::read(p).expect("read"))
        .collect();

    let dest = c.root.join("restored.redb");
    let o = restore(&c, &dest, &[&c.inc1, &c.inc2]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("restore written"), "{}", stdout(&o));
    assert_no_leak(&o, &[]);
    assert!(leftovers(&c.root).is_empty(), "{:?}", leftovers(&c.root));

    // ベースのときは 3 行、復元後は最新（7 行）と一致する。
    assert_eq!(row_ids(&c.db), (1..=7).collect::<Vec<_>>());
    assert_eq!(row_ids(&dest), (1..=7).collect::<Vec<_>>());
    let o = run(&["verify", "--db", s(&dest)]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));

    // 元のバックアップと増分は変わらない。
    for (p, b) in [&c.base, &c.inc1, &c.inc2].iter().zip(before) {
        assert_eq!(std::fs::read(p).expect("read"), b);
    }

    // 増分を 1 つだけ適用した場合は、その時点の状態になる。
    let mid = c.root.join("mid.redb");
    let o = restore(&c, &mid, &[&c.inc1]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&mid), (1..=5).collect::<Vec<_>>());

    // 増分を指定しない復元は従来どおりベースの状態。
    let plain = c.root.join("plain.redb");
    let o = restore(&c, &plain, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(row_ids(&plain), vec![1, 2, 3]);
}

/// 連鎖の切断・順序違反・重複・別 DB・改ざんは何も公開せず 1 で終了する。
#[test]
fn recover14_broken_chains_are_refused_without_publishing() {
    let c = build_chain("inc-cli-reject");
    let dest = c.root.join("restored.redb");

    let cases: Vec<(&str, Vec<&Path>, &str)> = vec![
        // 途中の増分の欠落。
        (
            "gap",
            vec![c.inc2.as_path()],
            "does not continue from the base",
        ),
        // 順序違反。
        (
            "order",
            vec![c.inc2.as_path(), c.inc1.as_path()],
            "does not continue from the base",
        ),
        // 重複。
        (
            "duplicate",
            vec![c.inc1.as_path(), c.inc1.as_path()],
            "does not continue from the base",
        ),
    ];
    for (label, incs, expect) in cases {
        let o = restore(&c, &dest, &incs);
        assert_eq!(o.status.code(), Some(1), "{label}: {}", stderr(&o));
        assert!(stderr(&o).contains(expect), "{label}: {}", stderr(&o));
        assert!(stderr(&o).contains("nothing was published"), "{label}");
        assert_no_leak(&o, &[s(&c.root)]);
        assert!(!dest.exists(), "{label}: nothing is published");
        assert!(leftovers(&c.root).is_empty(), "{label}: no temporary files");
    }

    // 改ざん（本体の 1 バイト）。
    let bad = c.root.join("tampered.seg");
    let mut bytes = std::fs::read(&c.inc1).expect("read");
    let n = bytes.len();
    bytes[n - 40] ^= 0xff;
    std::fs::write(&bad, bytes).expect("write");
    let o = restore(&c, &dest, &[&bad]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("checksum mismatch"), "{}", stderr(&o));
    assert!(!dest.exists());
    assert!(leftovers(&c.root).is_empty());

    // 別 DB の増分。
    let other = common::TempFixtureDir::new("inc-cli-other");
    let other_db = PathBuf::from(other.db_path_str());
    let other_base = c.root.join("other-base.redb");
    let other_inc = c.root.join("other.seg");
    write_rows(&other_db, 1..=2);
    assert_eq!(
        run(&["backup", "--db", s(&other_db), "--out", s(&other_base)])
            .status
            .code(),
        Some(0)
    );
    write_rows(&other_db, 3..=4);
    assert_eq!(
        run(&[
            "backup-incremental",
            "--db",
            s(&other_db),
            "--base",
            s(&other_base),
            "--out",
            s(&other_inc)
        ])
        .status
        .code(),
        Some(0)
    );
    let o = restore(&c, &dest, &[&other_inc]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("another database"), "{}", stderr(&o));
    assert!(!dest.exists());

    // 増分でないファイル（DB ファイル）を増分として渡す。
    let o = restore(&c, &dest, &[&c.base]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(!dest.exists());
    assert!(leftovers(&c.root).is_empty());
}

/// 出力側の拒否: 連鎖の切断（別 DB のベース・区間が空）は 1、出力先の既存・引数の不備は 2。
#[test]
fn backup_incremental_refuses_bad_bases_destinations_and_arguments() {
    let c = build_chain("inc-cli-backup-reject");
    let out = c.root.join("x.seg");

    // 新しい commit が無い（ベースが最新）。
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&c.db),
        "--base",
        s(&c.inc2),
        "--out",
        s(&out),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("no commits after the base"),
        "{}",
        stderr(&o)
    );
    assert!(!out.exists());

    // 別 DB のベース。
    let other = common::TempFixtureDir::new("inc-cli-backup-other");
    let other_db = PathBuf::from(other.db_path_str());
    let other_base = c.root.join("other-base.redb");
    write_rows(&other_db, 1..=2);
    assert_eq!(
        run(&["backup", "--db", s(&other_db), "--out", s(&other_base)])
            .status
            .code(),
        Some(0)
    );
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&c.db),
        "--base",
        s(&other_base),
        "--out",
        s(&out),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("another database"), "{}", stderr(&o));
    assert!(!out.exists());

    // 既存の出力先は上書きしない（引数の検証で 2）。
    let before = std::fs::read(&c.inc1).expect("read");
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&c.db),
        "--base",
        s(&c.base),
        "--out",
        s(&c.inc1),
    ]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert_eq!(std::fs::read(&c.inc1).expect("read"), before);

    // 他者が書けるディレクトリへは書かない。
    {
        use std::os::unix::fs::PermissionsExt;
        let shared = c.root.join("shared");
        std::fs::create_dir(&shared).expect("mkdir");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775)).expect("chmod");
        let dest = shared.join("x.seg");
        let o = run(&[
            "backup-incremental",
            "--db",
            s(&c.db),
            "--base",
            s(&c.base),
            "--out",
            s(&dest),
        ]);
        assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
        assert!(
            stderr(&o).contains("writable by other users"),
            "{}",
            stderr(&o)
        );
        assert!(!dest.exists());
    }

    // 引数の不備（未知・値欠落・重複・必須欠落・存在しないベース・`..`）。
    let (db, base) = (s(&c.db), s(&c.base));
    let bad_args: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--db"],
        vec!["--bogus", "x"],
        vec!["--db", db, "--base", base],
        vec!["--db", db, "--db", db, "--base", base, "--out", "x.seg"],
        vec!["--db", db, "--base", "/nonexistent-base", "--out", "x.seg"],
        vec!["--db", db, "--base", base, "--out", "../x.seg"],
    ];
    for args in bad_args {
        let mut full = vec!["backup-incremental"];
        full.extend(args.iter().copied());
        let o = run(&full);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
        assert_no_leak(&o, &[s(&c.root)]);
    }
    assert!(leftovers(&c.root).is_empty());
}

/// 変更ログを有効にしていない DB・ベースは連鎖の起点にできない（1）。
#[test]
fn backup_incremental_refuses_databases_without_a_change_log() {
    let c = build_chain("inc-cli-disabled");
    let plain = common::TempFixtureDir::new("inc-cli-plain");
    let plain_db = PathBuf::from(plain.db_path_str());
    {
        let storage = Storage::open(&plain_db).expect("open");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.5],
                    metadata: b"m",
                },
            )
            .expect("put");
    }
    let out = c.root.join("y.seg");
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&plain_db),
        "--base",
        s(&c.base),
        "--out",
        s(&out),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(!out.exists());
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&c.db),
        "--base",
        s(&plain_db),
        "--out",
        s(&out),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("change log is not enabled"),
        "{}",
        stderr(&o)
    );
    assert!(!out.exists());
}

/// 別のプロセス（ここではテストプロセス）が書き込み用に開いている DB は、読み取り専用 open がロックで
/// 拒否し、終了コード 1 で何も出力しない。
#[test]
fn backup_incremental_refuses_a_database_in_use() {
    let c = build_chain("inc-cli-in-use");
    let out = c.root.join("busy.seg");
    let held = Storage::open(&c.db).expect("hold the database open");
    let o = run(&[
        "backup-incremental",
        "--db",
        s(&c.db),
        "--base",
        s(&c.base),
        "--out",
        s(&out),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("open for writing by another process"),
        "{}",
        stderr(&o)
    );
    assert!(!out.exists());
    drop(held);
    assert!(leftovers(&c.root).is_empty());
}
