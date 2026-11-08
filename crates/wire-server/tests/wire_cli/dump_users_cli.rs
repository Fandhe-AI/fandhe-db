//! `--users` で起動したサーバーが作った DB の論理ダンプの往復と、ロール・権限の復元指示の改ざん拒否の外形
//! 検証（Issue #3096・RECOVER-16 ポインタ。設計は `docs/design/logical-dump.md` の第 14 節）。
//! バイナリを子プロセスとして起動し、終了コード・出力ファイルの権限・復元後に同じ users ファイルで
//! 起動したサーバーの認証と権限を確かめる。サーバーはポート 0 と一意な一時ディレクトリで起動する。

use crate::backup_cli::ChildGuard;
use crate::common;
use crate::dump_cli::{
    assert_no_temp_leftovers, assert_restore_rejected, code, dir_of, dump_args, header, read_text,
    run, s, stderr, write_dump_with,
};

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use wire_server::dump_file::StmtKind;
use wire_server::dump_opt::{DumpParts, DumpScope};

/// `hash-password` で `pw` の PHC を作り、与えた (利用者, テナント) の行から users ファイルを書く。
/// 書いた PHC を返す（出力に漏れないことの確認に使う）。
fn write_store(path: &str, rows: &[(&str, &str)]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-password");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"pw\n")
        .expect("write password");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success(), "hash-password must succeed");
    let phc = String::from_utf8(output.stdout).expect("utf-8 phc");
    let phc = phc.trim().to_string();
    let body: String = rows
        .iter()
        .map(|(u, t)| format!("{u}:{t}:{phc}\n"))
        .collect();
    std::fs::write(path, body).expect("write user store");
    phc
}

fn spawn_server(users: &str, db: &str) -> (ChildGuard, std::net::SocketAddr) {
    let mut g = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(["--users", users, "--db", db, "--bind", "127.0.0.1:0"])
            .args(["--ddl-allowed-users", "alice"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server"),
    );
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut g.0, Duration::from_secs(10));
    (g, addr)
}

/// 文を実行し、成功なら `Ok(行数)`、失敗なら `Err(SQLSTATE)` を返す。
fn try_exec(stream: &mut std::net::TcpStream, sql: &str) -> Result<usize, String> {
    common::send_simple_query(stream, sql);
    let mut rows = 0usize;
    let mut failed: Option<String> = None;
    loop {
        let (ty, body) = common::read_message(stream);
        match ty {
            b'D' => rows += 1,
            b'E' => {
                let mut state = String::new();
                for field in body.split(|b| *b == 0) {
                    if let Some((b'C', rest)) = field.split_first() {
                        state = String::from_utf8_lossy(rest).into_owned();
                    }
                }
                failed = Some(state);
            }
            b'Z' => return failed.map_or(Ok(rows), Err),
            _ => {}
        }
    }
}

fn exec_ok(stream: &mut std::net::TcpStream, sql: &str) {
    if let Err(state) = try_exec(stream, sql) {
        panic!("{sql} failed with sqlstate {state}");
    }
}

/// 各ロールの権限の観測結果（元の DB と復元先で同じになることを確かめる）。
fn probe(addr: std::net::SocketAddr) -> Vec<Result<usize, String>> {
    let mut out = Vec::new();
    for (user, sqls) in [
        ("alice", &["SELECT id FROM tab", "SELECT f(1)"][..]),
        (
            "carol",
            &[
                "SELECT id FROM tab",
                "DELETE FROM tab WHERE id = 99",
                "SELECT f(1)",
                "CREATE TABLE carol_t (id BIGINT PRIMARY KEY)",
            ][..],
        ),
    ] {
        let mut c = common::authenticate_to_ready_for_query(addr, user, "pw");
        for sql in sqls {
            out.push(try_exec(&mut c, sql));
        }
    }
    out
}

/// `--users` で起動したサーバーが作った DB（alice・carol=tenant-a、dave=tenant-b）を
/// `dump` → `dump-restore` で往復し、同じ users ファイルで復元先を起動しても認証・権限が元と同じ。
#[test]
fn users_file_database_round_trips_with_roles_and_privileges() {
    let fx = common::TempFixtureDir::new("dump-users-rt");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    let dir = dir_of(&db);
    let phc = write_store(
        &users,
        &[
            ("alice", "tenant-a"),
            ("carol", "tenant-a"),
            ("dave", "tenant-b"),
        ],
    );
    let before;
    {
        let (g, addr) = spawn_server(&users, &db);
        let mut a = common::authenticate_to_ready_for_query(addr, "alice", "pw");
        for sql in [
            "CREATE TABLE tab (id BIGINT PRIMARY KEY, v TEXT)",
            "INSERT INTO tab (id, v) VALUES (1, 'one'), (2, 'two')",
            "CREATE FUNCTION f(x INTEGER) RETURNS INTEGER LANGUAGE sql AS $q$ SELECT x $q$",
            "GRANT SELECT ON tab TO carol",
            "REVOKE EXECUTE ON FUNCTION f FROM PUBLIC",
            "ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC",
            "CREATE ROLE helper NOLOGIN",
            "GRANT helper TO carol",
        ] {
            exec_ok(&mut a, sql);
        }
        drop(a);
        before = probe(addr);
        // 他テナントにも別の状態を作る（ダンプに混ざらないこと）。dave は DDL 権限が無いので失敗してよい。
        let mut d = common::authenticate_to_ready_for_query(addr, "dave", "pw");
        let _ = try_exec(&mut d, "CREATE ROLE secret_b NOLOGIN");
        drop(d);
        drop(g);
    }

    let out = s(&dir.join("a.sql")).to_string();
    let o = run(&dump_args(&db, &out, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert_eq!(
        std::fs::metadata(&out).expect("meta").permissions().mode() & 0o777,
        0o600
    );
    let text = read_text(&out);
    assert!(text.contains("RESTORE BOOTSTRAP MARKER"));
    assert!(text.contains("RESTORE ROLE \"alice\" LEGACY PASSWORD"));
    assert!(text.contains("RESTORE ROLE \"alice\" BOOTSTRAP PRIVILEGES"));
    for leaked in ["dave", "tenant-b", "secret_b"] {
        assert!(!text.contains(leaked), "dump leaked {leaked:?}");
    }
    // 標準出力・標準エラーにパスワード検証子は出ない。
    assert!(!String::from_utf8_lossy(&o.stdout).contains(&phc));
    assert!(!stderr(&o).contains(&phc));

    let restored = s(&dir.join("restored.redb")).to_string();
    let o = run(&["dump-restore", "--from", &out, "--db", &restored]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(!String::from_utf8_lossy(&o.stdout).contains(&phc));
    assert!(!stderr(&o).contains(&phc));
    assert_no_temp_leftovers(&dir);
    // サーバーを起動する前に再ダンプを取る（起動するとブートストラップの結果で状態が変わりうる）。
    let again = s(&dir.join("again.sql")).to_string();
    let o = run(&dump_args(&restored, &again, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert_eq!(read_text(&again), text, "re-dump must match the original");

    // 同じ users ファイルで復元先を起動する。ブートストラップは再投入せず、認証と権限が元と同じ。
    let (g, addr) = spawn_server(&users, &restored);
    let after = probe(addr);
    assert_eq!(before, after, "privileges must match the original database");
    // 観測が空振りでないこと（alice も carol も `tab` を読める）。状態そのものの一致は再ダンプの一致が担う。
    assert_eq!(after[0], Ok(2));
    assert_eq!(after[2], Ok(2));
    let mut a = common::authenticate_to_ready_for_query(addr, "alice", "pw");
    exec_ok(&mut a, "CREATE TABLE after_restore (id BIGINT PRIMARY KEY)");
    drop(a);
    drop(g);
}

/// 復元指示の改ざんは、適用前（許可表・段階・範囲）か適用時（名前の解決・文法）に拒否され、何も公開されず、
/// 標準エラーにパスワード検証子・名前が出ない。
#[test]
fn restore_rejects_tampered_role_and_privilege_directives() {
    let fx = common::TempFixtureDir::new("dump-directive-tamper");
    let dir = dir_of(&fx.db_path_str());
    let from = s(&dir.join("d.sql")).to_string();
    let tenant = || header(DumpScope::Tenant("t1".into()), DumpParts::All);
    let role = "CREATE ROLE r1 NOLOGIN";
    let secret = "$argon2id$v=19$m=1,t=1,p=1$c2FsdA$SECRET-HASH";

    // 存在しないロールへの指示: 適用に失敗（終了コード 1・SQLSTATE 42704）し、検証子・名前は出ない。
    write_dump_with(&from, tenant(), |w| {
        w.statement(StmtKind::Role, role).expect("s");
        w.statement(
            StmtKind::RoleBootstrap,
            &format!("RESTORE ROLE \"ghost\" LEGACY PASSWORD '{secret}'"),
        )
        .expect("s");
    });
    let err = assert_restore_rejected(&dir, &from, &[], 1, "unknown role");
    assert!(err.contains("42704"), "{err}");
    assert!(
        !err.contains("SECRET-HASH") && !err.contains("ghost"),
        "{err}"
    );

    // 文法に合わない指示（余分な語・SQL の混入）は 42601。
    for bad in [
        "RESTORE BOOTSTRAP MARKER; DROP TABLE t",
        "RESTORE ROLE \"r1\" BOOTSTRAP PRIVILEGES SUPERUSER",
    ] {
        write_dump_with(&from, tenant(), |w| {
            w.statement(StmtKind::Role, role).expect("s");
            w.statement(StmtKind::RoleBootstrap, bad).expect("s");
        });
        let err = assert_restore_rejected(&dir, &from, &[], 1, bad);
        assert!(err.contains("42601"), "{bad}: {err}");
    }

    // 種別の許可表に合わない本文（GRANT・別の指示の接頭辞）は適用前に拒否（終了コード 4）。
    for (kind, body) in [
        (StmtKind::Privilege, "GRANT SELECT ON t TO r1"),
        (
            StmtKind::Owner,
            "RESTORE ACL TABLE \"t\" TO PUBLIC BY \"r1\" PRIVILEGES SELECT",
        ),
        (
            StmtKind::RoleBootstrap,
            "RESTORE OWNER TABLE \"t\" TO \"r1\"",
        ),
    ] {
        write_dump_with(&from, tenant(), |w| {
            w.statement(StmtKind::Role, role).expect("s");
            w.statement(kind, body).expect("s");
        });
        assert_restore_rejected(&dir, &from, &[], 4, body);
    }

    // 指示は後段の枠で、データの前には置けない。
    write_dump_with(&from, tenant(), |w| {
        w.statement(StmtKind::Table, "CREATE TABLE t (id BIGINT PRIMARY KEY)")
            .expect("s");
        w.statement(StmtKind::Role, role).expect("s");
        w.statement(StmtKind::RoleBootstrap, "RESTORE BOOTSTRAP MARKER")
            .expect("s");
        w.statement(StmtKind::Insert, "INSERT INTO t (id) VALUES (1)")
            .expect("s");
    });
    assert_restore_rejected(&dir, &from, &[], 4, "directive before data");

    // 共有のみ（scope: shared）のダンプに指示は現れない。
    write_dump_with(
        &from,
        header(DumpScope::Shared, DumpParts::SchemaOnly),
        |w| {
            w.statement(StmtKind::RoleBootstrap, "RESTORE BOOTSTRAP MARKER")
                .expect("s");
        },
    );
    assert_restore_rejected(&dir, &from, &[], 4, "directive in a shared dump");
    assert!(Path::new(&from).exists());
}
