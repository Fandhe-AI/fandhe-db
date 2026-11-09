//! `wire-server replica-resync`（レプリカの再同期。Issue #2176・親 #2171・RECOVER-15・RECOVER-13 ポインタ。
//! 方式の記録は `docs/design/replication-streaming.md`）をバイナリ子プロセスで外形検証する。
//!
//! 遅れが上限を超えたレプリカが「再同期が必要」で止まり（飛ばして適用しない）、`replica-resync` で作り直した
//! DB がプライマリの先端まで追いつくこと、プライマリの拒否（別系統・`RESYNC_REQUIRED`）や引数の不正では
//! 最終名へ何も公開されないこと、パスワードが出力に出ないことを固定する。サーバーはポート 0 と一意な
//! 一時ディレクトリで起動する（`replica_cli` の起動ヘルパーを共有）。

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engine::storage::{chain_point_of, Storage};

use super::replica_cli::{
    count_rows, frame, read_frame, run, s, spawn_replica, write_credentials, ChildGuard, Primary,
};

const ROLE_PW: &str = "pw-replica";
const EXIT_USAGE: i32 = 2;
const EXIT_PRIMARY_REJECTED: i32 = 11;

/// 他者が書けない新しいディレクトリを作る（サーバー側のバックアップ出力先の検査を満たす）。
fn private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(path).expect("mkdir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// 子プロセスが終了するまで待つ（上限つき）。
fn wait_exit(guard: &mut ChildGuard) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(st) = guard.0.try_wait().expect("try_wait") {
            return st.code();
        }
        assert!(Instant::now() < deadline, "process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `dir` に再同期の一時名が残っていないこと。
fn assert_no_temp(dir: &Path) {
    let leftovers: Vec<_> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("replica-resync"))
        .collect();
    assert!(leftovers.is_empty(), "temp files remain: {leftovers:?}");
}

fn resync_args<'a>(sock: &'a Path, out: &'a Path, repl: &'a str, cred: &'a Path) -> Vec<&'a str> {
    vec![
        "replica-resync",
        "--socket",
        s(sock),
        "--out",
        s(out),
        "--primary",
        repl,
        "--credentials",
        s(cred),
    ]
}

/// 遅れが上限を超えたレプリカは「再同期が必要」で止まり、`replica-resync` で作り直した DB が
/// プライマリの先端まで追いつく。復元の世代番号は進まない。
#[test]
fn recover15_resync_rebuilds_a_replica_that_fell_behind() {
    let primary = Primary::start_with("resync-lag", &["--replication-max-lag-records", "5"]);
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    for id in 1..=3 {
        t.insert(id);
    }
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let base_lsn = chain_point_of(&base).expect("chain point").end_lsn;
    let rdir = primary.dir.join("replica");
    private_dir(&rdir);
    let rdb = rdir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy base");
    let cred = write_credentials(&rdir, 0o600);

    // 1 件追従してから止め、上限を超える件数をプライマリへ書く。
    t.insert(4);
    let (mut g, lines) = spawn_replica(&rdb, primary.repl, &cred, &[]);
    lines.wait_applied(base_lsn + 1);
    let _ = g.0.kill();
    let _ = g.0.wait();
    for id in 5..=24 {
        t.insert(id);
    }

    // 遅れすぎたレプリカは適用を止めて報告する（終了コード 11・固有の文言）。何も飛ばして適用しない。
    for _ in 0..2 {
        let (mut g2, lines2) = spawn_replica(&rdb, primary.repl, &cred, &[]);
        assert_eq!(wait_exit(&mut g2), Some(EXIT_PRIMARY_REJECTED));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            lines2
                .snapshot()
                .iter()
                .any(|l| l.contains("resync required")),
            "{:?}",
            lines2.snapshot()
        );
        assert_eq!(
            chain_point_of(&rdb).expect("point").end_lsn,
            base_lsn + 1,
            "the replica must not skip the missing changes"
        );
    }

    // 再同期: 新しいレプリカ DB を作る。古い DB には触れない。
    let ndir = primary.dir.join("rebuilt");
    private_dir(&ndir);
    let new_db = ndir.join("replica.redb");
    let repl = primary.repl.to_string();
    let o = run(&resync_args(&primary.sock, &new_db, &repl, &cred));
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let new_lsn: u64 = out
        .trim()
        .strip_prefix("replica-resync written: base_lsn=")
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unexpected stdout: {out}"));
    assert!(new_lsn >= base_lsn + 21);
    assert_eq!(chain_point_of(&new_db).expect("point").end_lsn, new_lsn);
    assert_eq!(chain_point_of(&rdb).expect("old").end_lsn, base_lsn + 1);
    assert_no_temp(&ndir);
    let combined = format!("{out}{}", String::from_utf8_lossy(&o.stderr));
    assert!(!combined.contains(ROLE_PW), "password leaked");

    // 作り直したレプリカはプライマリの先端まで追いつく。
    t.insert(100);
    let (mut g3, lines3) = spawn_replica(&new_db, primary.repl, &cred, &[]);
    lines3.wait_applied(new_lsn + 1);
    let _ = g3.0.kill();
    let _ = g3.0.wait();
    assert_eq!(count_rows(&new_db, "SELECT id FROM docs LIMIT 1000"), 25);

    // 復元の世代番号はプライマリの値のまま進めない（`restore` は通さない）。
    let reference = primary.dir.join("reference.redb");
    let o = run(&[
        "backup",
        "--socket",
        s(&primary.sock),
        "--out",
        s(&reference),
    ]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let new_gen = Storage::open(&new_db)
        .expect("open")
        .restore_generation()
        .expect("generation");
    let ref_gen = Storage::open(&reference)
        .expect("open")
        .restore_generation()
        .expect("generation");
    assert_eq!(new_gen, ref_gen);
}

/// 偽プライマリがどう拒否しても、最終名にも一時名にも何も残らない。
fn run_against_fake_primary(label: &str, foreign: bool) {
    let primary = Primary::start(label);
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    t.insert(1);
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let point = chain_point_of(&base).expect("point");

    let rdir = primary.dir.join("cred");
    std::fs::create_dir(&rdir).expect("mkdir");
    let cred = write_credentials(&rdir, 0o600);
    let ndir = primary.dir.join("out");
    private_dir(&ndir);
    let new_db = ndir.join("replica.redb");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut ident_db = point.db_id;
    if foreign {
        ident_db[0] ^= 0xff;
    }
    let timeline = point.timeline;
    let latest = point.end_lsn;
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(20))).ok();
        let (ty, _) = read_frame(&mut conn).expect("hello");
        assert_eq!(ty, b'H');
        let mut ident = ident_db.to_vec();
        ident.extend_from_slice(&timeline.to_be_bytes());
        ident.extend_from_slice(&latest.to_be_bytes());
        conn.write_all(&frame(b'I', &ident)).expect("identity");
        if foreign {
            // Start を送らずに閉じられる。
            assert!(
                read_frame(&mut conn).is_none(),
                "no Start for a foreign primary"
            );
            return;
        }
        let (ty, _) = read_frame(&mut conn).expect("start");
        assert_eq!(ty, b'S');
        conn.write_all(&frame(b'E', &[5])).expect("error");
    });

    let repl = addr.to_string();
    let o = run(&resync_args(&primary.sock, &new_db, &repl, &cred));
    server.join().expect("fake primary");
    assert_eq!(o.status.code(), Some(EXIT_PRIMARY_REJECTED), "{:?}", o);
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(!err.contains(ROLE_PW), "password leaked: {err}");
    if foreign {
        assert!(err.contains("another database"), "{err}");
    } else {
        assert!(err.contains("outside the range"), "{err}");
    }
    assert!(!new_db.exists(), "nothing may be published");
    assert_no_temp(&ndir);
}

#[test]
fn recover15_resync_publishes_nothing_when_the_switch_point_is_not_retained() {
    run_against_fake_primary("resync-pruned", false);
}

#[test]
fn recover15_resync_publishes_nothing_for_a_foreign_primary() {
    run_against_fake_primary("resync-foreign", true);
}

/// 引数・パスの不正は、どこにも接続せず終了コード 2 で拒否される。
#[test]
fn recover15_resync_rejects_bad_arguments_before_doing_anything() {
    use std::os::unix::fs::PermissionsExt;
    let primary = Primary::start("resync-args");
    let rdir = primary.dir.join("cred");
    std::fs::create_dir(&rdir).expect("mkdir");
    let good = write_credentials(&rdir, 0o600);
    let loose_dir = primary.dir.join("loose");
    std::fs::create_dir(&loose_dir).expect("mkdir");
    let loose = write_credentials(&loose_dir, 0o644);
    let ndir = primary.dir.join("out");
    private_dir(&ndir);
    let existing = ndir.join("exists.redb");
    std::fs::write(&existing, b"x").expect("existing");
    let fresh = ndir.join("fresh.redb");
    let repl = primary.repl.to_string();
    let sock = primary.sock.clone();

    let with = |out: &Path, sock: &Path, repl: &str, cred: &Path| -> std::process::Output {
        run(&resync_args(sock, out, repl, cred))
    };
    let dotdot: PathBuf = ndir.join("..").join("out").join("x.redb");
    let missing_parent = ndir.join("nope").join("x.redb");
    let loose_sock_dir = primary.dir.join("ls");
    std::fs::create_dir(&loose_sock_dir).expect("mkdir");
    let loose_sock = loose_sock_dir.join("b.sock");
    {
        let _l = std::os::unix::net::UnixListener::bind(&loose_sock).expect("bind");
        std::fs::set_permissions(&loose_sock, std::fs::Permissions::from_mode(0o666))
            .expect("chmod");
        let o = with(&fresh, &loose_sock, &repl, &good);
        assert_eq!(o.status.code(), Some(EXIT_USAGE), "{:?}", o);
    }
    let mut cases = vec![
        with(&existing, &sock, &repl, &good),
        with(&dotdot, &sock, &repl, &good),
        with(&missing_parent, &sock, &repl, &good),
        with(&fresh, &sock, &repl, &loose),
        with(&fresh, &sock, "10.0.0.5:9", &good),
        run(&[
            "replica-resync",
            "--socket",
            s(&sock),
            "--socket",
            s(&sock),
            "--out",
            s(&fresh),
            "--primary",
            &repl,
            "--credentials",
            s(&good),
        ]),
        run(&["replica-resync", "--bogus", "x"]),
        run(&["replica-resync"]),
    ];
    for o in cases.drain(..) {
        assert_eq!(o.status.code(), Some(EXIT_USAGE), "{:?}", o);
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        assert!(!err.contains(ROLE_PW), "password leaked: {err}");
    }
    assert!(!fresh.exists());
    assert_eq!(std::fs::read(&existing).expect("read"), b"x");
    assert_no_temp(&ndir);
}

/// Issue #3135: 前回の異常終了が `--out` の親に残した作業ディレクトリ（0700・自分の所有・中身が `base.redb` だけ）
/// を、次の `replica-resync` が掃除して報告する。モードが違うもの・未知の項目を含むもの・シンボリックリンクは
/// 残る。今回の実行（プライマリに届かず失敗）も作業ディレクトリを残さない。
#[test]
fn recover15_resync_sweeps_stale_work_dirs_only_when_safe() {
    use std::os::unix::fs::PermissionsExt;
    let primary = Primary::start("resync-sweep");
    let rdir = primary.dir.join("cred");
    std::fs::create_dir(&rdir).expect("mkdir");
    let cred = write_credentials(&rdir, 0o600);
    let ndir = primary.dir.join("out");
    std::fs::create_dir(&ndir).expect("mkdir");
    std::fs::set_permissions(&ndir, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    let mk = |name: &str, mode: u32, files: &[(&str, u32)]| {
        let p = ndir.join(name);
        std::fs::create_dir(&p).expect("mkdir");
        for (f, fm) in files {
            let fp = p.join(f);
            std::fs::write(&fp, b"x").expect("write");
            std::fs::set_permissions(&fp, std::fs::Permissions::from_mode(*fm)).expect("chmod");
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let nonce = "0123456789abcdef";
    let stale = format!(".fandhe-replica-resync-4242-{nonce}-7.d");
    let empty = format!(".fandhe-replica-resync-4242-{nonce}-8.d");
    let wide = format!(".fandhe-replica-resync-4242-{nonce}-9.d");
    let extra = format!(".fandhe-replica-resync-4242-{nonce}-10.d");
    let link = format!(".fandhe-replica-resync-4242-{nonce}-11.d");
    mk(&stale, 0o700, &[("base.redb", 0o600)]);
    mk(&empty, 0o700, &[]);
    mk(&wide, 0o750, &[("base.redb", 0o600)]);
    mk(&extra, 0o700, &[("other.bin", 0o600)]);
    std::os::unix::fs::symlink(ndir.join(&stale), ndir.join(&link)).expect("symlink");

    // 届かない endpoint（閉じたポート）。ベースの取得までは進み、確認接続で失敗する。
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").to_string()
    };
    let new_db = ndir.join("replica.redb");
    let o = run(&resync_args(&primary.sock, &new_db, &closed, &cred));
    assert_ne!(o.status.code(), Some(0), "{:?}", o);
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(err.contains("removed 2 stale temporary file(s)"), "{err}");
    assert!(err.contains(&stale) && err.contains(&empty), "{err}");
    assert!(!ndir.join(&stale).exists() && !ndir.join(&empty).exists());
    assert!(ndir.join(&wide).is_dir() && ndir.join(&extra).is_dir());
    assert!(std::fs::symlink_metadata(ndir.join(&link)).is_ok());
    assert!(!new_db.exists(), "nothing may be published");
    // 今回の作業ディレクトリは残らない（残るのは意図して残した 3 つの項目だけ）。
    let mut left: Vec<String> = std::fs::read_dir(&ndir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    let mut want = vec![wide, extra, link];
    want.sort();
    assert_eq!(left, want);
}
