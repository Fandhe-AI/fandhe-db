//! `wire-server backup-incremental --socket`（稼働中のサーバーからの増分バックアップ）の外形検証
//! （Issue #3098・親 #2164・RECOVER-13・RECOVER-14 ポインタ。設計は `docs/design/change-log-pitr.md` の D7）。
//! バイナリを子プロセスとして起動し、変更ログ有効のサーバーへ運用者専用ソケット経由で依頼して、
//! 受け入れ条件「稼働中に取った増分をベースへ適用すると取得時点の状態になる」・連鎖・拒否系・不正な要求を
//! 固定する。サーバーはポート 0・一意な一時ディレクトリで独立させる。

use crate::backup_cli::{
    insert, parent_of, private_socket_path, select_ids, spawn_server_with,
    write_user_store_with_alice, ChildGuard,
};
use crate::common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");

fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(BIN).args(args).output().expect("run")
}

fn code(o: &Output) -> Option<i32> {
    o.status.code()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn backup_full(sock: &Path, out: &Path) -> Output {
    run(&[
        "backup".as_ref(),
        "--socket".as_ref(),
        sock.as_os_str(),
        "--out".as_ref(),
        out.as_os_str(),
    ])
}

fn backup_inc(sock: &Path, base: &Path, out: &Path) -> Output {
    run(&[
        "backup-incremental".as_ref(),
        "--socket".as_ref(),
        sock.as_os_str(),
        "--base".as_ref(),
        base.as_os_str(),
        "--out".as_ref(),
        out.as_os_str(),
    ])
}

fn restore(base: &Path, dest: &Path, incs: &[&Path]) -> Output {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "restore".as_ref(),
        "--from".as_ref(),
        base.as_os_str(),
        "--db".as_ref(),
        dest.as_os_str(),
    ];
    for p in incs {
        args.push("--incremental".as_ref());
        args.push(p.as_os_str());
    }
    run(&args)
}

/// 変更ログ有効＋運用者ソケット付きで起動したサーバー（認証済みの接続 1 本つき）。
struct Live {
    _guard: ChildGuard,
    addr: std::net::SocketAddr,
    stream: std::net::TcpStream,
    sock: PathBuf,
    users: String,
    dir: PathBuf,
}

fn start(fx: &common::TempFixtureDir, extra: &[&str]) -> Live {
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let (_d, sock) = private_socket_path(&db);
    let sock_s = sock.to_str().expect("utf-8").to_string();
    let mut args = vec!["--backup-socket", sock_s.as_str()];
    args.extend_from_slice(extra);
    let mut child = spawn_server_with(&users, &db, &args);
    let (addr, _) =
        common::wait_for_listening_addr_and_lines(&mut child.0, Duration::from_secs(10));
    let mut stream = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    common::send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
    );
    let _ = common::read_command_complete(&mut stream);
    common::read_ready_for_query(&mut stream);
    Live {
        _guard: child,
        addr,
        stream,
        sock,
        users,
        dir: parent_of(&db).to_path_buf(),
    }
}

/// 停止後の出力 DB をサーバーで開いて id 一覧を返す。
fn ids_of(users: &str, db: &Path) -> Vec<String> {
    let mut b = spawn_server_with(users, db.to_str().expect("utf-8"), &["--change-log"]);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut b.0, Duration::from_secs(10));
    let mut s = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    select_ids(&mut s)
}

/// 受け入れ条件: 書き込みを流しながら稼働中に取った増分をベースへ適用すると、取得開始前に完了した
/// 書き込みをすべて含む欠番なしの接頭辞になる。静止時の増分と連鎖は最新と厳密に一致する。
#[test]
fn live_incremental_restores_to_a_consistent_state_and_chains() {
    let fx = common::TempFixtureDir::new("ibs");
    let mut live = start(&fx, &["--change-log"]);
    for id in 1..=3 {
        insert(&mut live.stream, id);
    }
    let base = live.dir.join("base.redb");
    let o = backup_full(&live.sock, &base);
    assert_eq!(code(&o), Some(0), "{}", text(&o));

    // 取得中も別接続から書き込みを続ける。
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (stop_w, done_w, addr) = (stop.clone(), done.clone(), live.addr);
    let writer = std::thread::spawn(move || {
        let mut w = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        let mut n = 0u32;
        while !stop_w.load(std::sync::atomic::Ordering::Relaxed) {
            insert(&mut w, 100 + n);
            n += 1;
            done_w.store(n, std::sync::atomic::Ordering::Release);
        }
    });
    while done.load(std::sync::atomic::Ordering::Acquire) < 3 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = done.load(std::sync::atomic::Ordering::Acquire);
    let inc1 = live.dir.join("inc1.seg");
    let o = backup_inc(&live.sock, &base, &inc1);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().expect("writer");
    let after = done.load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(code(&o), Some(0), "{}", text(&o));
    assert!(text(&o).contains("backup-incremental written: records="));
    assert!(inc1.is_file());

    let restored = live.dir.join("restored.redb");
    let o = restore(&base, &restored, &[&inc1]);
    assert_eq!(code(&o), Some(0), "{}", text(&o));
    let ids = ids_of(&live.users, &restored);
    // 取得開始前に完了した書き込み（1..=3 と 100..100+before）をすべて含み、欠番がない。
    let n_restored = ids.len();
    assert!(n_restored >= 3 + before as usize, "{ids:?}");
    assert!(n_restored <= 3 + after as usize, "{ids:?}");
    let expect: Vec<String> = (1..=3u32)
        .chain(100..100 + (n_restored as u32 - 3))
        .map(|i| i.to_string())
        .collect();
    let mut got = ids.clone();
    got.sort_by_key(|i| i.parse::<u32>().expect("numeric id"));
    assert_eq!(got, expect);

    // 静止時: さらに書き込み、2 本目の増分（inc1 をベースにした連鎖）で最新と厳密に一致する。
    insert(&mut live.stream, 900);
    insert(&mut live.stream, 901);
    let inc2 = live.dir.join("inc2.seg");
    let o = backup_inc(&live.sock, &inc1, &inc2);
    assert_eq!(code(&o), Some(0), "{}", text(&o));
    let restored2 = live.dir.join("restored2.redb");
    let o = restore(&base, &restored2, &[&inc1, &inc2]);
    assert_eq!(code(&o), Some(0), "{}", text(&o));
    let live_ids = select_ids(&mut live.stream);
    assert_eq!(ids_of(&live.users, &restored2), live_ids);

    // 新しい commit が無い（空の区間）は 1。何も作られず、サーバーは無傷。
    let inc3 = live.dir.join("inc3.seg");
    let o = backup_inc(&live.sock, &inc2, &inc3);
    assert_eq!(code(&o), Some(1), "{}", text(&o));
    assert!(!inc3.exists());
    // 既存の出力先は 2。
    let o = backup_inc(&live.sock, &inc2, &inc2);
    assert_eq!(code(&o), Some(2), "{}", text(&o));
    assert_eq!(select_ids(&mut live.stream), live_ids);
}

/// 拒否系: 変更ログなしのサーバー・別 DB のベースは 1、引数の不備は 2。出力は作られず、出力にパスを含めない。
#[test]
fn live_incremental_rejections_leave_nothing_and_server_survives() {
    // 変更ログ有効のサーバー（別 DB のベースの供給元）。
    let fx_a = common::TempFixtureDir::new("ibs-a");
    let mut a = start(&fx_a, &["--change-log"]);
    insert(&mut a.stream, 1);
    let foreign_base = a.dir.join("foreign.redb");
    assert_eq!(code(&backup_full(&a.sock, &foreign_base)), Some(0));

    // 変更ログなしのサーバー。
    let fx_b = common::TempFixtureDir::new("ibs-b");
    let mut b = start(&fx_b, &[]);
    insert(&mut b.stream, 1);
    let inc = b.dir.join("x.seg");
    let o = backup_inc(&b.sock, &foreign_base, &inc);
    assert_eq!(code(&o), Some(1), "{}", text(&o));
    assert!(!inc.exists());

    // 別 DB のベースは連鎖が合わず 1（変更ログ有効のサーバーに対して）。
    insert(&mut a.stream, 2);
    let fx_c = common::TempFixtureDir::new("ibs-c");
    let mut c = start(&fx_c, &["--change-log"]);
    insert(&mut c.stream, 1);
    let inc = c.dir.join("y.seg");
    let o = backup_inc(&c.sock, &foreign_base, &inc);
    assert_eq!(code(&o), Some(1), "{}", text(&o));
    assert!(!inc.exists());
    assert!(!text(&o).contains(c.dir.to_str().expect("utf-8")));

    // 引数: --db と --socket の併用・どちらも無し。
    let sock_s = c.sock.to_str().expect("utf-8");
    let o = run(&[
        "backup-incremental".as_ref(),
        "--db".as_ref(),
        "x".as_ref(),
        "--socket".as_ref(),
        sock_s.as_ref(),
        "--base".as_ref(),
        foreign_base.as_os_str(),
        "--out".as_ref(),
        inc.as_os_str(),
    ]);
    assert_eq!(code(&o), Some(2), "{}", text(&o));
    let o = run(&[
        "backup-incremental".as_ref(),
        "--base".as_ref(),
        foreign_base.as_os_str(),
        "--out".as_ref(),
        inc.as_os_str(),
    ]);
    assert_eq!(code(&o), Some(2), "{}", text(&o));
    assert_eq!(select_ids(&mut c.stream), vec!["1".to_string()]);
}

fn raw_request(sock: &Path, line: &[u8]) -> String {
    let mut s = UnixStream::connect(sock).expect("connect");
    s.write_all(line).expect("write");
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("read");
    resp
}

/// 不正な増分要求は固定文言の 2 で拒否され、サーバーは生存して次の正しい要求に応える。
#[test]
fn malformed_incremental_requests_are_rejected_and_server_survives() {
    let fx = common::TempFixtureDir::new("ibs-m");
    let mut live = start(&fx, &["--change-log"]);
    insert(&mut live.stream, 1);
    let base = live.dir.join("base.redb");
    assert_eq!(code(&backup_full(&live.sock, &base)), Some(0));

    let id = "00112233445566778899aabbccddeeff";
    let out = live.dir.join("never.seg");
    let out_s = out.to_str().expect("utf-8");
    let bad: Vec<String> = vec![
        format!("incremental zz 1 1 {out_s}\n"),
        format!("incremental {} 1 1 {out_s}\n", &id[..31]),
        format!("incremental {id} +1 1 {out_s}\n"),
        format!("incremental {id} 1 1 relative/path\n"),
        format!("incremental {id} 1 1 /a/../b\n"),
        format!("incremental {id} 1\n"),
        "incremental\n".to_string(),
    ];
    for line in &bad {
        let r = raw_request(&live.sock, line.as_bytes());
        assert!(r.starts_with("2 "), "{line:?} -> {r:?}");
        assert!(!r.contains(out_s));
    }
    assert!(!out.exists());

    insert(&mut live.stream, 2);
    let inc = live.dir.join("ok.seg");
    let o = backup_inc(&live.sock, &base, &inc);
    assert_eq!(code(&o), Some(0), "{}", text(&o));
    assert!(inc.is_file());
}
