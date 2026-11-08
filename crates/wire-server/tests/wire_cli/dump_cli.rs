//! `wire-server dump`／`dump-restore` サブコマンドの外形検証（Issue #2162・RECOVER-16 ポインタ。
//! 方式は `docs/design/logical-dump.md` の第 12 節）。バイナリを子プロセスとして起動し、終了コード・
//! 出力ファイルの内容と権限・復元後の DB の中身・失敗時に何も残らないことを確かめる。
//! サーバーを起動するテストはポート 0 と一意な一時ディレクトリを使う。

use crate::backup_cli::ChildGuard;
use crate::common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use wire_server::dump_file::{FileWriter, Header, StmtKind};
use wire_server::dump_opt::{DumpFormat, DumpParts, DumpScope};

const BOB_SECRET: &str = "BOBSECRET-row-content";
const BOB_SCHEMA: &str = "bobschema";

pub(crate) fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(args)
        .output()
        .expect("run wire-server")
}

pub(crate) fn code(o: &Output) -> Option<i32> {
    o.status.code()
}

pub(crate) fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

pub(crate) fn dir_of(path: &str) -> PathBuf {
    Path::new(path).parent().expect("parent dir").to_path_buf()
}

pub(crate) fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// 2 テナント（alice=tenant-a・bob=tenant-b）の利用者ストアを書く。
fn write_two_tenant_store(path: &str) {
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
    let phc = phc.trim();
    std::fs::write(path, format!("alice:tenant-a:{phc}\nbob:tenant-b:{phc}\n"))
        .expect("write user store");
}

fn spawn_server(users: &str, db: &str) -> (ChildGuard, std::net::SocketAddr) {
    let mut g = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(["--users", users, "--db", db, "--bind", "127.0.0.1:0"])
            .args(["--ddl-allowed-users", "alice,bob"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server"),
    );
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut g.0, Duration::from_secs(10));
    (g, addr)
}

/// 問い合わせ結果をテキストの行として返す（`NULL` は `None`）。
fn query(stream: &mut std::net::TcpStream, sql: &str) -> Vec<Vec<Option<String>>> {
    common::send_simple_query(stream, sql);
    let mut rows = Vec::new();
    loop {
        let (ty, body) = common::read_message(stream);
        match ty {
            b'D' => {
                let n = i16::from_be_bytes([body[0], body[1]]) as usize;
                let mut pos = 2;
                let mut row = Vec::new();
                for _ in 0..n {
                    let len = i32::from_be_bytes([
                        body[pos],
                        body[pos + 1],
                        body[pos + 2],
                        body[pos + 3],
                    ]);
                    pos += 4;
                    if len < 0 {
                        row.push(None);
                    } else {
                        let end = pos + len as usize;
                        row.push(Some(String::from_utf8_lossy(&body[pos..end]).into_owned()));
                        pos = end;
                    }
                }
                rows.push(row);
            }
            b'Z' => return rows,
            b'E' => panic!("query failed: {}", String::from_utf8_lossy(&body)),
            _ => {}
        }
    }
}

/// 欠かせない値の取りこぼしを避けるため、行全体をタブ区切りの文字列にして比べる。
fn flat(rows: Vec<Vec<Option<String>>>) -> Vec<String> {
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|c| c.unwrap_or_else(|| "<NULL>".into()))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect()
}

struct Fixture {
    _fx: common::TempFixtureDir,
    users: String,
    db: String,
}

impl Fixture {
    fn dir(&self) -> PathBuf {
        dir_of(&self.db)
    }

    fn path(&self, name: &str) -> String {
        s(&self.dir().join(name)).to_string()
    }
}

/// engine を直接開いて文を実行する（サーバー経由だと `--users` の初期ブートストラップ権限が
/// 記録され、権限を書き出せない現状のダンプが拒否するため。設計書の第 12 節を参照）。
pub(crate) struct Direct<'a> {
    core: &'a engine::core::EngineCore,
    ctx: engine::policy::PolicyContext,
    session: engine::sql::mode::SessionState,
}

impl<'a> Direct<'a> {
    pub(crate) fn new(core: &'a engine::core::EngineCore, tenant: &str) -> Self {
        use engine::policy::PolicyContext;
        use engine::storage::Visibility;
        let mut session = engine::sql::mode::SessionState::default();
        session.allow_ddl();
        Self {
            core,
            ctx: PolicyContext::with_visibilities(
                tenant,
                [Visibility::Public, Visibility::Private],
            )
            .expect("tenant"),
            session,
        }
    }

    pub(crate) fn exec(&mut self, sql: &str) {
        if let Err(e) = self
            .core
            .execute_sql_in_session(&self.ctx, &mut self.session, sql)
        {
            panic!("statement failed: {}", e.wire_code());
        }
    }

    /// 読み取りを実行し、各行のセルを `Debug` 表記の文字列として返す（往復後の値比較用）。
    pub(crate) fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        match self
            .core
            .execute_sql_in_session(&self.ctx, &mut self.session, sql)
        {
            Ok(engine::sql::SqlOutcome::Query(r)) => r
                .rows
                .into_iter()
                .map(|row| row.cells.iter().map(|c| format!("{c:?}")).collect())
                .collect(),
            Ok(_) => panic!("expected rows"),
            Err(e) => panic!("query failed: {}", e.wire_code()),
        }
    }
}

/// tenant-a と tenant-b が同じ共有の表 `docs` へ行を書いた DB を作る。
/// tenant-a の行には `;`・改行・`--`・`$$`・タブ・バックスラッシュ・NULL・多バイト文字を含める。
/// tenant-b だけが持つ SCHEMA・行・値は、tenant-a のダンプに現れてはならない。
fn populated(label: &str, extra_alice: &[&str]) -> Fixture {
    let fx = common::TempFixtureDir::new(label);
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_two_tenant_store(&users);
    {
        let core = engine::core::EngineCore::open(&db).expect("open db");
        let mut a = Direct::new(&core, "tenant-a");
        a.exec(
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT, score DOUBLE PRECISION, \
             embedding VECTOR(3))",
        );
        a.exec("CREATE INDEX docs_score_idx ON docs (score)");
        a.exec(
            "INSERT INTO docs (id, body, score, embedding) VALUES \
             (1, 'plain', 1.5, '[0.1,0.2,0.3]'), \
             (2, 'semi;colon -- dash $$ dollar', -2, '[1,2,3]'), \
             (3, 'multi\nline\ttab \\ back', NULL, '[0.5,0.25,0.125]'), \
             (4, NULL, 0, '[0,0,1]'), \
             (5, '日本語', 3.25, '[2,2,2]'), \
             (6, '\\N', 7, '[9,9,9]'), \
             (7, '', 8, '[8,8,8]')",
        );
        // 1 回の COPY がバッチ上限を超える行数（復元が分割して commit できること）。
        for chunk in 0..3 {
            let values: Vec<String> = (0..50)
                .map(|i| {
                    let id = 100 + chunk * 50 + i;
                    format!("({id}, 'bulk-{id}', {id}, '[1,1,1]')")
                })
                .collect();
            a.exec(&format!(
                "INSERT INTO docs (id, body, score, embedding) VALUES {}",
                values.join(", ")
            ));
        }
        for sql in extra_alice {
            a.exec(sql);
        }
        let mut b = Direct::new(&core, "tenant-b");
        b.exec(&format!("CREATE SCHEMA {BOB_SCHEMA}"));
        b.exec(&format!(
            "INSERT INTO docs (id, body, score, embedding) VALUES \
             (1000, '{BOB_SECRET}', 99, '[7,7,7]')"
        ));
    }
    Fixture { _fx: fx, users, db }
}

pub(crate) fn dump_args<'a>(db: &'a str, out: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["dump", "--db", db, "--out", out];
    v.extend_from_slice(extra);
    v
}

/// 復元先のディレクトリに一時名（`.fandhe-` 始まり）が残っていないこと。
pub(crate) fn assert_no_temp_leftovers(dir: &Path) {
    for e in std::fs::read_dir(dir).expect("read dir") {
        let name = e.expect("entry").file_name();
        let name = name.to_string_lossy();
        assert!(!name.starts_with(".fandhe-"), "leftover temp file: {name}");
    }
}

pub(crate) fn read_text(path: &str) -> String {
    String::from_utf8(std::fs::read(path).expect("read dump")).expect("utf-8 dump")
}

/// 既定（COPY）で往復でき、テナント指定が境界を越えず、出力は 0600、復元後の再ダンプが一致する。
#[test]
fn dump_round_trip_copy_respects_tenant_boundary() {
    let f = populated("dump-roundtrip", &[]);
    let out = f.path("a.sql");
    let o = run(&dump_args(&f.db, &out, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("dump written"));

    // 権限は 0600。一時名は残らない。
    let mode = std::fs::metadata(&out).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_no_temp_leftovers(&f.dir());

    // 他テナントの行・値・名前は出力に現れない。
    let text = read_text(&out);
    assert!(text.starts_with("-- fandhe-db logical dump v1\n-- scope: tenant=tenant-a\n"));
    assert!(text.contains("COPY "), "copy format expected");
    for needle in [BOB_SECRET, BOB_SCHEMA, "tenant-b", "bob", "1000"] {
        assert!(!text.contains(needle), "dump leaked {needle:?}");
    }

    // 往復: 復元して、別のサーバーが同じ行を返す（特殊文字・NULL・多バイト文字を含む）。
    let restored = f.path("restored.redb");
    let o = run(&["dump-restore", "--from", &out, "--db", &restored]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert_no_temp_leftovers(&f.dir());
    // 復元結果から取り直したダンプは元のダンプとバイト単位で一致する（サーバーを起動すると
    // `--users` の初期ブートストラップ権限が記録されてダンプできなくなるため、起動前に取る）。
    let again = f.path("again.sql");
    let o = run(&dump_args(&restored, &again, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert_eq!(read_text(&again), text);

    let (_g, addr) = spawn_server(&f.users, &restored);
    let mut a = common::authenticate_to_ready_for_query(addr, "alice", "pw");
    let rows = flat(query(
        &mut a,
        "SELECT id, body, score, embedding FROM docs ORDER BY id",
    ));
    assert_eq!(rows.len(), 7 + 150);
    assert_eq!(rows[0], "1\tplain\t1.5\t[0.1,0.2,0.3]");
    assert_eq!(rows[1], "2\tsemi;colon -- dash $$ dollar\t-2\t[1,2,3]");
    assert_eq!(
        rows[2],
        "3\tmulti\nline\ttab \\ back\t<NULL>\t[0.5,0.25,0.125]"
    );
    assert_eq!(rows[3], "4\t<NULL>\t0\t[0,0,1]");
    assert_eq!(rows[4], "5\t日本語\t3.25\t[2,2,2]");
    assert_eq!(rows[5], "6\t\\N\t7\t[9,9,9]");
    assert_eq!(rows[6], "7\t\t8\t[8,8,8]");
    assert_eq!(rows[7], "100\tbulk-100\t100\t[1,1,1]");
    drop(a);
    // 他テナントの行は復元先に存在せず、tenant-b からは何も見えない。
    let mut b = common::authenticate_to_ready_for_query(addr, "bob", "pw");
    assert!(query(&mut b, "SELECT id FROM docs").is_empty());
    drop(b);
    drop(_g);
}

/// あらゆる種別のオブジェクト（ENUM・SEQUENCE・SERIAL・IDENTITY・VIEW・索引・SCHEMA・関数・トリガ・
/// ポリシー・行セキュリティ・ロール）と行を持つ DB が、往復して同一のダンプに戻る。
/// 各種別の許可表（先頭の語句）が書き手の出力形と一致していることの確認を兼ねる。
#[test]
fn dump_round_trip_covers_every_object_kind() {
    let fx = common::TempFixtureDir::new("dump-kinds");
    let db = fx.db_path_str();
    {
        let core = engine::core::EngineCore::open(&db).expect("open db");
        let mut a = Direct::new(&core, "tenant-a");
        for sql in RICH_SCHEMA {
            a.exec(sql);
        }
    }
    let dir = dir_of(&db);
    let out = s(&dir.join("a.sql")).to_string();
    let o = run(&dump_args(&db, &out, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let text = read_text(&out);
    for kind in [
        "schema",
        "role",
        "enum-type",
        "sequence",
        "sequence-value",
        "table",
        "index",
        "view",
        "function",
        "trigger",
        "row-security",
        "policy",
    ] {
        assert!(
            text.contains(&format!("-- @stmt {kind} ")),
            "dump lacks kind {kind}"
        );
    }
    let restored = s(&dir.join("restored.redb")).to_string();
    let o = run(&["dump-restore", "--from", &out, "--db", &restored]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let again = s(&dir.join("again.sql")).to_string();
    let o = run(&dump_args(&restored, &again, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert_eq!(read_text(&again), text);
}

const RICH_SCHEMA: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'it''s \"fine\"')",
    "CREATE SEQUENCE plain_seq START WITH 10 INCREMENT BY 5",
    "CREATE TABLE parent (id BIGINT PRIMARY KEY, name TEXT NOT NULL DEFAULT 'x''y', \
     score DOUBLE PRECISION DEFAULT 1.5, flag BOOLEAN DEFAULT TRUE, m mood DEFAULT 'ok', \
     created TIMESTAMP DEFAULT now(), tags TEXT[], emb VECTOR(3), \
     UNIQUE (name), CHECK (score >= 0))",
    "CREATE TABLE child (cid SERIAL PRIMARY KEY, \
     ident INTEGER GENERATED BY DEFAULT AS IDENTITY (START WITH 100 INCREMENT BY 10), \
     pid BIGINT, \
     sv BIGINT DEFAULT nextval('plain_seq'), note VARCHAR(20) COLLATE \"C\")",
    "CREATE VIEW v1 AS SELECT id, name FROM parent WHERE name = 'x'",
    "CREATE INDEX p_score ON parent (score) WHERE score > 1",
    "CREATE UNIQUE INDEX p_flag_id ON parent (name, id)",
    "CREATE SCHEMA app",
    "CREATE TABLE app.items (iid SERIAL PRIMARY KEY, label TEXT NOT NULL, UNIQUE (label))",
    "CREATE VIEW app.vi AS SELECT label FROM app.items",
    "INSERT INTO parent (id, name, emb, tags) VALUES (1, 'a', '[1,2,3]', '{x,\"y z\",NULL}')",
    "INSERT INTO child (pid) VALUES (1)",
    "INSERT INTO child (pid) VALUES (1)",
    "CREATE FUNCTION add_one(x INTEGER, tags TEXT[]) RETURNS INTEGER LANGUAGE sql IMMUTABLE STRICT \
     PARALLEL SAFE COST 5 AS $q$ SELECT x + 1; $q$",
    "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $q$ BEGIN RETURN NEW; END $q$",
    "CREATE TRIGGER parent_trg BEFORE INSERT OR UPDATE OF name ON parent \
     FOR EACH ROW EXECUTE FUNCTION trg('a', 'it''s')",
    "CREATE POLICY pol_sel ON parent AS RESTRICTIVE FOR SELECT USING (id > 0)",
    "ALTER TABLE parent ENABLE ROW LEVEL SECURITY",
    "CREATE ROLE reporter LOGIN PASSWORD 'pw' CONNECTION LIMIT 3",
];

/// `--format insert` でも往復でき、リテラル内の改行・`;`・`$$` が壊れない。
#[test]
fn dump_round_trip_insert_format() {
    let f = populated("dump-insert", &[]);
    let out = f.path("a.sql");
    let o = run(&dump_args(
        &f.db,
        &out,
        &["--tenant", "tenant-a", "--format", "insert"],
    ));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let text = read_text(&out);
    assert!(text.contains("INSERT INTO "));
    assert!(!text.contains("\nCOPY "));
    assert!(!text.contains(BOB_SECRET));

    let restored = f.path("restored.redb");
    let o = run(&["dump-restore", "--from", &out, "--db", &restored]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let (_g, addr) = spawn_server(&f.users, &restored);
    let mut a = common::authenticate_to_ready_for_query(addr, "alice", "pw");
    let rows = flat(query(&mut a, "SELECT id, body FROM docs ORDER BY id"));
    assert_eq!(rows.len(), 157);
    assert_eq!(rows[1], "2\tsemi;colon -- dash $$ dollar");
    assert_eq!(rows[2], "3\tmulti\nline\ttab \\ back");
}

/// スキーマのみ・データのみ。スキーマのみは空の表を戻し、データのみは新規 DB へ復元できない。
#[test]
fn dump_schema_only_and_data_only() {
    let f = populated("dump-parts", &[]);
    let schema = f.path("schema.sql");
    let o = run(&dump_args(
        &f.db,
        &schema,
        &["--tenant", "tenant-a", "--schema-only"],
    ));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let text = read_text(&schema);
    assert!(text.contains("-- parts: schema\n"));
    assert!(text.contains("CREATE TABLE "));
    assert!(!text.contains("COPY ") && !text.contains("INSERT INTO "));
    assert!(!text.contains("bulk-100"));

    let data = f.path("data.sql");
    let o = run(&dump_args(
        &f.db,
        &data,
        &["--tenant", "tenant-a", "--data-only"],
    ));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let text = read_text(&data);
    assert!(text.contains("-- parts: data\n"));
    assert!(text.contains("COPY ") && text.contains("bulk-100"));
    assert!(!text.contains("CREATE TABLE "));
    assert!(!text.contains(BOB_SECRET));

    // スキーマのみの復元: 表はあるが行は無い。
    let restored = f.path("restored.redb");
    let o = run(&["dump-restore", "--from", &schema, "--db", &restored]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let (_g, addr) = spawn_server(&f.users, &restored);
    let mut a = common::authenticate_to_ready_for_query(addr, "alice", "pw");
    assert!(query(&mut a, "SELECT id FROM docs").is_empty());
    drop(a);
    drop(_g);

    // データのみは新規 DB へ復元できない（拒否・何も作らない）。
    let nope = f.path("nope.redb");
    let o = run(&["dump-restore", "--from", &data, "--db", &nope]);
    assert_eq!(code(&o), Some(4), "stderr: {}", stderr(&o));
    assert!(!Path::new(&nope).exists());
    assert_no_temp_leftovers(&f.dir());
}

/// `--shared-only` は共有の定義だけで行を含まず、復元後に他テナントの存在は現れない。
#[test]
fn dump_shared_only_has_definitions_without_rows() {
    let f = populated("dump-shared", &[]);
    let out = f.path("shared.sql");
    let o = run(&dump_args(&f.db, &out, &["--shared-only"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let text = read_text(&out);
    assert!(text.contains("-- scope: shared\n"));
    assert!(text.contains("CREATE TABLE "));
    for needle in ["COPY ", "INSERT INTO ", BOB_SECRET, BOB_SCHEMA, "bulk-100"] {
        assert!(
            !text.contains(needle),
            "shared dump must not contain {needle:?}"
        );
    }
    let restored = f.path("restored.redb");
    let o = run(&["dump-restore", "--from", &out, "--db", &restored]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let (_g, addr) = spawn_server(&f.users, &restored);
    let mut b = common::authenticate_to_ready_for_query(addr, "bob", "pw");
    assert!(query(&mut b, "SELECT id FROM docs").is_empty());
}

/// 書き出せないオブジェクトがあるとダンプ全体が失敗し、出力も一時名も残らない。
#[test]
fn dump_failure_publishes_nothing_and_leaves_no_temp_file() {
    let f = populated(
        "dump-unsupported",
        // 所有される SEQUENCE の RESTART は SQL で戻せない（マテリアライズド VIEW は #3097 で対応済み）。
        &[
            "CREATE TABLE plain (pid SERIAL PRIMARY KEY, v TEXT)",
            "ALTER SEQUENCE plain_pid_seq RESTART WITH 5",
        ],
    );
    let out = f.path("a.sql");
    let o = run(&dump_args(&f.db, &out, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("unsupported"), "stderr: {err}");
    assert!(
        !err.contains("plain_pid_seq") && !err.contains("plain"),
        "stderr: {err}"
    );
    assert!(!Path::new(&out).exists());
    assert_no_temp_leftovers(&f.dir());
}

/// 稼働中のサーバーが開いている DB のダンプは拒否される（出力は作られない）。
#[test]
fn dump_rejects_running_db() {
    let f = populated("dump-running", &[]);
    let (_g, _addr) = spawn_server(&f.users, &f.db);
    let out = f.path("a.sql");
    let o = run(&dump_args(&f.db, &out, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("in use"), "stderr: {}", stderr(&o));
    assert!(!Path::new(&out).exists());
    assert_no_temp_leftovers(&f.dir());
}

/// 不正な引数・パスは DB を開く前に終了コード 2 で拒否され、何も作られない。
#[test]
fn dump_and_restore_reject_invalid_arguments() {
    let fx = common::TempFixtureDir::new("dump-invalid");
    let db = fx.db_path_str();
    std::fs::write(&db, b"not a database").expect("write dummy db");
    let dir = dir_of(&db);
    let out = s(&dir.join("o.sql")).to_string();
    let existing = s(&dir.join("exists.sql")).to_string();
    std::fs::write(&existing, b"x").expect("write existing");

    let missing_db = existing.replace("exists", "missing");
    let cases: Vec<Vec<&str>> = vec![
        vec!["dump"],
        vec!["dump", "--db", &db, "--out", &out],
        vec![
            "dump",
            "--db",
            &db,
            "--out",
            &out,
            "--tenant",
            "t",
            "--shared-only",
        ],
        vec!["dump", "--db", &db, "--out", &out, "--tenant", "has space"],
        vec!["dump", "--db", &db, "--out", &out, "--tenant", ""],
        vec![
            "dump",
            "--db",
            &db,
            "--out",
            &out,
            "--tenant",
            "t",
            "--schema-only",
            "--data-only",
        ],
        vec![
            "dump", "--db", &db, "--out", &out, "--tenant", "t", "--format", "csv",
        ],
        vec![
            "dump", "--db", &db, "--out", &out, "--tenant", "t", "--bogus",
        ],
        vec!["dump", "--db", &db, "--out", &existing, "--tenant", "t"],
        vec!["dump", "--db", &db, "--out", "../x", "--tenant", "t"],
        vec!["dump", "--db", &missing_db, "--out", &out, "--tenant", "t"],
        vec!["dump-restore"],
        vec!["dump-restore", "--from", &existing],
        vec!["dump-restore", "--from", &existing, "--db", &db],
        vec!["dump-restore", "--from", &existing, "--db", &existing],
    ];
    for args in &cases {
        let o = run(args);
        assert_eq!(code(&o), Some(2), "args={args:?} stderr: {}", stderr(&o));
    }
    assert!(!Path::new(&out).exists());
    assert_no_temp_leftovers(&dir);
    assert_eq!(std::fs::read(&db).expect("db"), b"not a database");
}

fn header(scope: DumpScope, parts: DumpParts) -> Header {
    Header {
        scope,
        parts,
        format: DumpFormat::Copy,
    }
}

/// 手組みのダンプファイルを書く。
fn write_dump(path: &str, parts: DumpParts, build: impl FnOnce(&mut FileWriter<Vec<u8>>)) {
    let mut w = FileWriter::begin(Vec::new(), &header(DumpScope::Tenant("t1".into()), parts))
        .expect("begin");
    build(&mut w);
    std::fs::write(path, w.finish().expect("finish")).expect("write dump");
}

/// 既存の復元先（空でない DB を含む）への復元は拒否され、内容は変わらない。
#[test]
fn restore_refuses_existing_destination() {
    let f = populated("dump-existing", &[]);
    let out = f.path("a.sql");
    let o = run(&dump_args(&f.db, &out, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let before = std::fs::read(&f.db).expect("db bytes");
    let o = run(&["dump-restore", "--from", &out, "--db", &f.db]);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    assert_eq!(std::fs::read(&f.db).expect("db bytes"), before);
    assert_no_temp_leftovers(&f.dir());
}

/// 途中で失敗した復元は何も公開せず、一時ファイルも残さない。
#[test]
fn restore_failure_midway_publishes_nothing() {
    let fx = common::TempFixtureDir::new("dump-midway");
    let dir = dir_of(&fx.db_path_str());
    let from = s(&dir.join("bad.sql")).to_string();
    let dest = s(&dir.join("dest.redb")).to_string();
    write_dump(&from, DumpParts::All, |w| {
        w.statement(StmtKind::Table, "CREATE TABLE t (id BIGINT PRIMARY KEY)")
            .expect("stmt");
        // 同名の表を再度作る文は実行時に失敗する（1 文目は成功済み）。
        w.statement(StmtKind::Table, "CREATE TABLE t (id BIGINT PRIMARY KEY)")
            .expect("stmt");
    });
    let o = run(&["dump-restore", "--from", &from, "--db", &dest]);
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("dump item 2") && err.contains("nothing was published"),
        "{err}"
    );
    assert!(!Path::new(&dest).exists());
    assert_no_temp_leftovers(&dir);
}

/// 改ざん・破損・許可外の文を含むファイルは拒否され（終了コード 4）、何も作られない。
#[test]
fn restore_rejects_tampered_and_malformed_dumps() {
    let fx = common::TempFixtureDir::new("dump-tampered");
    let dir = dir_of(&fx.db_path_str());
    let good = s(&dir.join("good.sql")).to_string();
    write_dump(&good, DumpParts::All, |w| {
        w.statement(
            StmtKind::Table,
            "CREATE TABLE t (id BIGINT PRIMARY KEY, b TEXT)",
        )
        .expect("stmt");
        w.copy_header("COPY t (id, b) FROM STDIN").expect("copy");
        w.copy_row("1\tx").expect("row");
        w.copy_end().expect("end");
    });
    let text = std::fs::read_to_string(&good).expect("read");
    let n = "CREATE TABLE t (id BIGINT PRIMARY KEY, b TEXT)".len();

    let mut variants: Vec<(&str, Vec<u8>)> = vec![
        ("version", text.replacen("v1", "v9", 1).into_bytes()),
        (
            "scope",
            text.replacen("tenant=t1", "tenant=t 1", 1).into_bytes(),
        ),
        (
            "length",
            text.replacen(
                &format!("@stmt table {n}"),
                &format!("@stmt table {}", n - 1),
                1,
            )
            .into_bytes(),
        ),
        (
            "kind",
            text.replacen("@stmt table", "@stmt index", 1).into_bytes(),
        ),
        ("end-marker", text.replacen("-- end\n", "", 1).into_bytes()),
        ("trailing", format!("{text}DROP TABLE t;\n").into_bytes()),
        ("no-copy-end", text.replacen("\\.\n", "", 1).into_bytes()),
        ("binary", vec![0u8, 1, 2, 3, 255, 254]),
        (
            "oversized-length",
            text.replacen(&format!("@stmt table {n}"), "@stmt table 99999999999", 1)
                .into_bytes(),
        ),
    ];
    // 種別と本文が食い違う（許可表に無い文）・2 文を 1 枠に詰める。
    for (name, kind, body) in [
        ("drop-as-table", "table", "DROP TABLE t"),
        (
            "two-statements",
            "table",
            "CREATE TABLE u (id BIGINT PRIMARY KEY); DROP TABLE u",
        ),
        ("select-as-insert", "insert", "SELECT 1"),
    ] {
        let bad = format!(
            "{}\n-- scope: tenant=t1\n-- parts: all\n-- format: copy\n-- body\n\
             -- @stmt {kind} {}\n{body};\n-- end\n",
            wire_server::dump_file::MAGIC,
            body.len()
        );
        variants.push((name, bad.into_bytes()));
    }
    // 切り詰めは、どの位置で切っても拒否される。
    for cut in (0..text.len()).step_by(7) {
        variants.push(("truncated", text.as_bytes()[..cut].to_vec()));
    }
    for (name, bytes) in variants {
        let from = s(&dir.join("variant.sql")).to_string();
        let dest = s(&dir.join("dest.redb")).to_string();
        std::fs::write(&from, &bytes).expect("write variant");
        let o = run(&["dump-restore", "--from", &from, "--db", &dest]);
        let c = code(&o);
        // 空ファイルは引数検証（2）、枠・ヘッダの拒否は 4、COPY の終端を欠いた本文が次の行を
        // データとして読んで失敗する場合は実行時の失敗（1）。いずれも何も公開してはならない。
        assert!(
            matches!(c, Some(1) | Some(4)) || (bytes.is_empty() && c == Some(2)),
            "variant {name}: code {c:?} stderr: {}",
            stderr(&o)
        );
        assert!(
            !Path::new(&dest).exists(),
            "variant {name} published a database"
        );
        assert_no_temp_leftovers(&dir);
    }
    // 元のファイルは受理される。
    let dest = s(&dir.join("ok.redb")).to_string();
    let o = run(&["dump-restore", "--from", &good, "--db", &dest]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
}

fn write_dump_with(path: &str, h: Header, build: impl FnOnce(&mut FileWriter<Vec<u8>>)) {
    let mut w = FileWriter::begin(Vec::new(), &h).expect("begin");
    build(&mut w);
    std::fs::write(path, w.finish().expect("finish")).expect("write dump");
}

/// 復元を試み、終了コードと「何も公開されない」ことを確かめる。
fn assert_restore_rejected(dir: &Path, from: &str, extra: &[&str], want: i32, why: &str) -> String {
    let dest = s(&dir.join("dest.redb")).to_string();
    let mut args = vec!["dump-restore", "--from", from, "--db", &dest];
    args.extend_from_slice(extra);
    let o = run(&args);
    assert_eq!(code(&o), Some(want), "{why}: stderr: {}", stderr(&o));
    assert!(!Path::new(&dest).exists(), "{why}: published a database");
    assert_no_temp_leftovers(dir);
    stderr(&o)
}

const T_SQL: &str = "CREATE TABLE t (id BIGINT PRIMARY KEY, b TEXT)";

/// 枠の順序（作成 → データ → 後段）を強制する。データの後ろに作成系、後段の後ろにデータは置けない。
/// 関数・トリガ・データの順は、トリガ関数がデータ投入中に走りうるので拒否される。
#[test]
fn restore_enforces_frame_order() {
    let fx = common::TempFixtureDir::new("dump-order");
    let dir = dir_of(&fx.db_path_str());
    let from = s(&dir.join("o.sql")).to_string();
    let h = || header(DumpScope::Tenant("t1".into()), DumpParts::All);
    let trg = "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg()";
    let func =
        "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $q$ BEGIN RETURN NEW; END $q$";
    write_dump_with(&from, h(), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::Function, func).expect("s");
        w.statement(StmtKind::Trigger, trg).expect("s");
        w.statement(StmtKind::Insert, "INSERT INTO t (id, b) VALUES (1, 'x')")
            .expect("s");
    });
    let err = assert_restore_rejected(&dir, &from, &[], 4, "trigger before data");
    assert!(err.contains("dump item 4"), "{err}");
    // COPY も同じ。
    write_dump_with(&from, h(), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::Index, "CREATE INDEX i ON t (b)")
            .expect("s");
        w.copy_header("COPY t (id, b) FROM STDIN").expect("c");
        w.copy_row("1\tx").expect("r");
        w.copy_end().expect("e");
    });
    assert_restore_rejected(&dir, &from, &[], 4, "copy after post-data");
    // データの前に後段（外部キー・索引・採番値）を置かない。作成系をデータの後ろにも置かない。
    write_dump_with(&from, h(), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::Insert, "INSERT INTO t (id, b) VALUES (1, 'x')")
            .expect("s");
        w.statement(StmtKind::Table, "CREATE TABLE u (id BIGINT PRIMARY KEY)")
            .expect("s");
    });
    assert_restore_rejected(&dir, &from, &[], 4, "table after data");
}

/// `setval` は厳密な形だけを受け付け、`ALTER TABLE` は種別ごとの形に絞る。
#[test]
fn restore_rejects_loose_setval_and_alter_table() {
    let fx = common::TempFixtureDir::new("dump-shapes");
    let dir = dir_of(&fx.db_path_str());
    let from = s(&dir.join("o.sql")).to_string();
    let h = || header(DumpScope::Tenant("t1".into()), DumpParts::All);
    for (kind, sql) in [
        (
            StmtKind::SequenceValue,
            "SELECT setval('s', 1, TRUE), now()",
        ),
        (
            StmtKind::SequenceValue,
            "SELECT setval('s', 1, TRUE) FROM t",
        ),
        (
            StmtKind::RowSecurity,
            "ALTER TABLE t DISABLE ROW LEVEL SECURITY",
        ),
        (
            StmtKind::RowSecurity,
            "ALTER TABLE t NO FORCE ROW LEVEL SECURITY",
        ),
        (StmtKind::ForeignKey, "ALTER TABLE t DROP COLUMN b"),
    ] {
        write_dump_with(&from, h(), |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.statement(kind, sql).expect("s");
        });
        assert_restore_rejected(&dir, &from, &[], 4, sql);
    }
}

/// マテリアライズド VIEW の枠（Issue #3097）。`CREATE ... WITH NO DATA` と `REFRESH "<名前>"` の厳密な形、
/// SCHEMA 修飾の `setval` は受理し、実体を作る形・修飾の崩れ・段階の違反・`parts` との不整合は拒否する。
#[test]
fn restore_accepts_matview_frames_and_rejects_loose_shapes() {
    let fx = common::TempFixtureDir::new("dump-matview-shapes");
    let dir = dir_of(&fx.db_path_str());
    let from = s(&dir.join("o.sql")).to_string();
    let h = || header(DumpScope::Tenant("t1".into()), DumpParts::All);
    let create = "CREATE MATERIALIZED VIEW \"mv\" AS SELECT id FROM t WITH NO DATA";
    // 受理: SCHEMA 配下の SEQUENCE の現在値・matview の定義と再計算。
    write_dump_with(&from, h(), |w| {
        w.statement(StmtKind::Schema, "CREATE SCHEMA app")
            .expect("s");
        w.statement(
            StmtKind::Sequence,
            "CREATE SEQUENCE app.q START WITH 5 INCREMENT BY 1 MINVALUE 1 \
             MAXVALUE 100 NO CYCLE",
        )
        .expect("s");
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::MaterializedView, create).expect("s");
        w.statement(StmtKind::SequenceValue, "SELECT setval('app.q', 9, TRUE)")
            .expect("s");
        w.statement(
            StmtKind::MaterializedViewRefresh,
            "REFRESH MATERIALIZED VIEW \"mv\"",
        )
        .expect("s");
    });
    let dest = s(&dir.join("ok.redb")).to_string();
    let o = run(&["dump-restore", "--from", &from, "--db", &dest]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));

    // 拒否される文の形。
    for (kind, sql) in [
        (
            StmtKind::MaterializedView,
            "CREATE MATERIALIZED VIEW \"mv\" AS SELECT id FROM t",
        ),
        (
            StmtKind::MaterializedView,
            "CREATE MATERIALIZED VIEW \"mv\" AS SELECT id FROM t WITH DATA",
        ),
        (
            StmtKind::MaterializedViewRefresh,
            "REFRESH MATERIALIZED VIEW CONCURRENTLY \"mv\"",
        ),
        (
            StmtKind::MaterializedViewRefresh,
            "REFRESH MATERIALIZED VIEW \"mv\" WITH NO DATA",
        ),
        (
            StmtKind::MaterializedViewRefresh,
            "REFRESH MATERIALIZED VIEW \"mv\", \"mv2\"",
        ),
        (
            StmtKind::MaterializedViewRefresh,
            "REFRESH MATERIALIZED VIEW \"mv\" WITH DATA",
        ),
        (StmtKind::SequenceValue, "SELECT setval('a.b.c', 1, TRUE)"),
        (StmtKind::SequenceValue, "SELECT setval('a.\"b\"', 1, TRUE)"),
        (StmtKind::SequenceValue, "SELECT setval('.b', 1, TRUE)"),
        (StmtKind::SequenceValue, "SELECT setval('a.', 1, TRUE)"),
    ] {
        write_dump_with(&from, h(), |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.statement(kind, sql).expect("s");
        });
        assert_restore_rejected(&dir, &from, &[], 4, sql);
    }

    // REFRESH は後段（Post）の枠。その後ろに Pre の枠（matview の作成）は置けない（順序違反）。
    write_dump_with(&from, h(), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::MaterializedView, create).expect("s");
        w.statement(
            StmtKind::MaterializedViewRefresh,
            "REFRESH MATERIALIZED VIEW \"mv\"",
        )
        .expect("s");
        w.statement(
            StmtKind::MaterializedView,
            "CREATE MATERIALIZED VIEW \"mv2\" AS SELECT id FROM t WITH NO DATA",
        )
        .expect("s");
    });
    assert_restore_rejected(&dir, &from, &[], 4, "create after refresh");

    // 再計算は論理ダンプ（parts: all）にしか現れない。schema・data のヘッダでは拒否する。
    for parts in [DumpParts::SchemaOnly, DumpParts::DataOnly] {
        write_dump_with(&from, header(DumpScope::Tenant("t1".into()), parts), |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.statement(StmtKind::MaterializedView, create).expect("s");
            w.statement(
                StmtKind::MaterializedViewRefresh,
                "REFRESH MATERIALIZED VIEW \"mv\"",
            )
            .expect("s");
        });
        assert_restore_rejected(&dir, &from, &[], 4, "refresh outside parts: all");
    }
}

/// ヘッダ（scope・parts・format）と枠の内容が食い違うファイルは拒否する。
#[test]
fn restore_rejects_header_frame_mismatch() {
    let fx = common::TempFixtureDir::new("dump-mismatch");
    let dir = dir_of(&fx.db_path_str());
    let from = s(&dir.join("o.sql")).to_string();
    let ins = "INSERT INTO t (id, b) VALUES (1, 'x')";
    let mk = |scope: DumpScope, parts: DumpParts, format: DumpFormat| Header {
        scope,
        parts,
        format,
    };
    let tenant = || DumpScope::Tenant("t1".into());
    // shared にデータ枠（INSERT／COPY）。
    write_dump_with(
        &from,
        mk(DumpScope::Shared, DumpParts::All, DumpFormat::Copy),
        |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.statement(StmtKind::Insert, ins).expect("s");
        },
    );
    assert_restore_rejected(&dir, &from, &[], 4, "shared insert");
    write_dump_with(
        &from,
        mk(DumpScope::Shared, DumpParts::All, DumpFormat::Copy),
        |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.copy_header("COPY t (id, b) FROM STDIN").expect("c");
            w.copy_row("1\tx").expect("r");
            w.copy_end().expect("e");
        },
    );
    assert_restore_rejected(&dir, &from, &[], 4, "shared copy");
    // parts: schema にデータ枠。
    write_dump_with(
        &from,
        mk(tenant(), DumpParts::SchemaOnly, DumpFormat::Copy),
        |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.statement(StmtKind::Insert, ins).expect("s");
        },
    );
    assert_restore_rejected(&dir, &from, &[], 4, "schema-only with data");
    // format: insert に @copy。
    write_dump_with(
        &from,
        mk(tenant(), DumpParts::All, DumpFormat::Insert),
        |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.copy_header("COPY t (id, b) FROM STDIN").expect("c");
            w.copy_row("1\tx").expect("r");
            w.copy_end().expect("e");
        },
    );
    assert_restore_rejected(&dir, &from, &[], 4, "insert format with copy");
    // COPY の見出しが FROM STDIN の形でない。
    write_dump_with(&from, mk(tenant(), DumpParts::All, DumpFormat::Copy), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.copy_header("COPY t (id, b) FROM STDIN WITH (FORMAT csv)")
            .expect("c");
        w.copy_row("1,x").expect("r");
        w.copy_end().expect("e");
    });
    assert_restore_rejected(&dir, &from, &[], 4, "copy options");
    // 整合するファイルは受理され、--expect-tenant の一致／不一致が効く。
    write_dump_with(&from, mk(tenant(), DumpParts::All, DumpFormat::Copy), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::Insert, ins).expect("s");
    });
    assert_restore_rejected(
        &dir,
        &from,
        &["--expect-tenant", "other"],
        4,
        "tenant mismatch",
    );
    assert_restore_rejected(
        &dir,
        &from,
        &["--expect-tenant", "has space"],
        2,
        "bad expect",
    );
    let dest = s(&dir.join("ok.redb")).to_string();
    let o = run(&[
        "dump-restore",
        "--from",
        &from,
        "--db",
        &dest,
        "--expect-tenant",
        "t1",
    ]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
}

/// 復元先の親にグループ・その他が書き込める（sticky bit つきを含む）と、何も作らず終了コード 2 で
/// 拒否する。0755・0700 の親では成功し、作業ディレクトリ・一時名は成功時も失敗時も残らない。
#[test]
fn restore_requires_a_parent_not_writable_by_others() {
    let fx = common::TempFixtureDir::new("dump-parent-perms");
    let base = dir_of(&fx.db_path_str());
    let h = || header(DumpScope::Tenant("t1".into()), DumpParts::All);
    let src = s(&base.join("good.sql")).to_string();
    write_dump_with(&src, h(), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::Insert, "INSERT INTO t (id, b) VALUES (1, 'x')")
            .expect("s");
    });
    let bad_src = s(&base.join("bad.sql")).to_string();
    write_dump_with(&bad_src, h(), |w| {
        w.statement(StmtKind::Table, T_SQL).expect("s");
        w.statement(StmtKind::Table, T_SQL).expect("s");
    });
    for mode in [0o777, 0o775, 0o757, 0o1777] {
        let dir = base.join(format!("p{mode:o}"));
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
        let dest = s(&dir.join("out.redb")).to_string();
        let o = run(&["dump-restore", "--from", &src, "--db", &dest]);
        assert_eq!(code(&o), Some(2), "mode {mode:o}: stderr: {}", stderr(&o));
        assert_eq!(
            std::fs::read_dir(&dir).expect("read").count(),
            0,
            "mode {mode:o}: something was created"
        );
    }
    for mode in [0o755, 0o700] {
        let dir = base.join(format!("ok{mode:o}"));
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
        // 失敗時
        let dest_bad = s(&dir.join("bad.redb")).to_string();
        let o = run(&["dump-restore", "--from", &bad_src, "--db", &dest_bad]);
        assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
        assert!(!Path::new(&dest_bad).exists());
        assert_no_temp_leftovers(&dir);
        // 成功時
        let dest = s(&dir.join("ok.redb")).to_string();
        let o = run(&["dump-restore", "--from", &src, "--db", &dest]);
        assert_eq!(code(&o), Some(0), "mode {mode:o}: stderr: {}", stderr(&o));
        assert_no_temp_leftovers(&dir);
        assert_eq!(
            std::fs::metadata(&dest).expect("meta").permissions().mode() & 0o777,
            0o600
        );
    }
}

/// engine のバッチ上限（行数・チャンク数・合計バイト）を小さくしても、複数行の COPY を
/// バッチに分けて復元できる。上限は子プロセスの環境変数でだけ渡す（テストプロセスの環境は変えない）。
#[test]
fn restore_splits_copy_by_every_engine_batch_limit() {
    let fx = common::TempFixtureDir::new("dump-batch-limits");
    let dir = dir_of(&fx.db_path_str());
    let from = s(&dir.join("o.sql")).to_string();
    write_dump_with(
        &from,
        header(DumpScope::Tenant("t1".into()), DumpParts::All),
        |w| {
            w.statement(StmtKind::Table, T_SQL).expect("s");
            w.copy_header("COPY t (id, b) FROM STDIN").expect("c");
            for i in 0..12 {
                w.copy_row(&format!("{i}\tvalue-{i}")).expect("r");
            }
            w.copy_end().expect("e");
        },
    );
    let cases: [(&str, &str); 3] = [
        ("FANDHE_DB_BATCH_MAX_CHUNKS", "1"),
        ("FANDHE_DB_BATCH_MAX_CHUNKS", "5"),
        // 1 行は 11〜12 バイト。2 行は入らない大きさにする。
        ("FANDHE_DB_BATCH_MAX_TOTAL_BYTES", "16"),
    ];
    for (n, (key, val)) in cases.iter().enumerate() {
        let dest = s(&dir.join(format!("d{n}.redb"))).to_string();
        let o = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(["dump-restore", "--from", &from, "--db", &dest])
            .env(key, val)
            .output()
            .expect("run");
        assert_eq!(code(&o), Some(0), "{key}={val}: stderr: {}", stderr(&o));
        let again = s(&dir.join(format!("d{n}.sql"))).to_string();
        let o = run(&dump_args(&dest, &again, &["--tenant", "t1"]));
        assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
        let text = read_text(&again);
        for i in 0..12 {
            assert!(
                text.contains(&format!("{i}\tvalue-{i}\n")),
                "{key}={val}: row {i}"
            );
        }
    }
    // 1 行だけで上限を超えるなら fail-closed（何も公開しない）。
    let dest = s(&dir.join("tiny.redb")).to_string();
    let o = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(["dump-restore", "--from", &from, "--db", &dest])
        .env("FANDHE_DB_BATCH_MAX_TOTAL_BYTES", "4")
        .output()
        .expect("run");
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(!Path::new(&dest).exists());
    assert_no_temp_leftovers(&dir);
}
