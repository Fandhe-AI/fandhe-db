//! `ALTER TABLE ... RENAME TO`（TABLE-38 のポインタ、Issue #1623）の **クラッシュ時単一
//! トランザクション性** の結合テスト（`table15_crash_atomicity.rs` と同型の自己再帰子プロセス）。
//!
//! 子プロセス（`std::env::current_exe()` を環境変数付きで再実行する）が alice・bob の行を
//! 投入した後 `docs` と `docs2` の間で改名を繰り返し、commit のたびに進捗マーカーを
//! ファイルへ追記する。親は jitter を付けて SIGKILL し、`Storage` を開き直して次の
//! 不変条件（kill のタイミングに依存しない）を確認する:
//!
//! - `docs`・`docs2` のちょうど一方だけが存在し、他方は `42P01`（旧名・新名の中間状態なし）
//! - 存在する側で alice・bob の全行が揃っている（行ストアがカタログと同時に移る）
//! - 存在する名前は、最後のマーカーが示す名前か、その次の名前のどちらか
//! - 存在しない側の名前で `CREATE TABLE` すると 0 件（孤児の行テーブルを引き継がない）
//!
//! `WriteDurability::Immediate`（既定）のため、マーカーを書けた時点の commit は SIGKILL 後も
//! 必ず見える。sleep は親のポーリングで kill 位置にばらつきを与えるためにのみ使う。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ROLE_ENV: &str = "TABLE38_CRASH_CHILD_ROLE";
const DB_ENV: &str = "TABLE38_CRASH_CHILD_DB";
const LOG_ENV: &str = "TABLE38_CRASH_CHILD_LOG";

/// 子の最大改名回数（暴走防止。親は必ずこれより先に kill する）。
const MAX_RENAMES: u64 = 2_000;
const ALICE_ROWS: u64 = 40;
const BOB_ROWS: u64 = 20;
const BOB_ID_BASE: u64 = 100_001;
const KILL_ROUNDS: usize = 16;
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
const NAMES: [&str; 2] = ["docs", "docs2"];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, c: &PolicyContext, sql: &str) -> Result<SqlOutcome, String> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(c, &mut s, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn ok(core: &EngineCore, c: &PolicyContext, sql: &str) {
    if let Err(code) = run(core, c, sql) {
        panic!("{} must succeed, got {code}", &sql[..sql.len().min(80)]);
    }
}

fn insert_sql(table: &str, first: u64, count: u64, op: &str) -> String {
    let values: Vec<String> = (first..first + count)
        .map(|id| format!("({id}, 'c{id}', '[0.1,0.2]')"))
        .collect();
    format!(
        "INSERT INTO {table} (id, code, embedding) VALUES {} USING OPERATION_ID '{op}'",
        values.join(",")
    )
}

/// `COUNT(*)`。テーブル不在は `Err("42P01")`。
fn count(core: &EngineCore, c: &PolicyContext, table: &str) -> Result<u64, String> {
    match run(core, c, &format!("SELECT COUNT(*) FROM {table}"))? {
        SqlOutcome::Query(q) => match &q.rows[0].cells[0] {
            Cell::Integer(v) => Ok(*v),
            other => panic!("expected Integer, got {other:?}"),
        },
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn mark(log: &Path, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .expect("child: open log");
    f.write_all(format!("{line}\n").as_bytes())
        .expect("child: write marker");
}

fn child_env() -> Option<(String, PathBuf, PathBuf)> {
    let role = std::env::var(ROLE_ENV).ok()?;
    let db = PathBuf::from(std::env::var(DB_ENV).expect("child: db env"));
    let log = PathBuf::from(std::env::var(LOG_ENV).expect("child: log env"));
    Some((role, db, log))
}

/// 子の負荷。投入完了後に `SEEDED` を書き、以降は改名の直前に `BEGIN <n>`、commit 後に
/// `RENAMED <n>`（`n` 回目の改名後の名前は `NAMES[n % 2]`）を書く。
fn child_workload(db: &Path, log: &Path) {
    let storage = Storage::open(db).expect("child: open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    ok(
        &core,
        &alice,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, code TEXT, embedding VECTOR(2) NOT NULL)",
    );
    ok(&core, &alice, &insert_sql("docs", 1, ALICE_ROWS, "seed-a"));
    ok(
        &core,
        &bob,
        &insert_sql("docs", BOB_ID_BASE, BOB_ROWS, "seed-b"),
    );
    mark(log, "SEEDED 0");
    for n in 1..=MAX_RENAMES {
        let from = NAMES[((n - 1) % 2) as usize];
        let to = NAMES[(n % 2) as usize];
        mark(log, &format!("BEGIN {n}"));
        ok(&core, &alice, &format!("ALTER TABLE {from} RENAME TO {to}"));
        mark(log, &format!("RENAMED {n}"));
    }
}

/// panic 時にも子を必ず kill して回収する RAII ガード。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn complete_lines(log: &Path) -> Vec<String> {
    match std::fs::read_to_string(log) {
        Ok(s) => s
            .split_inclusive('\n')
            .filter(|l| l.ends_with('\n'))
            .map(|l| l.trim_end().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn last_marker(lines: &[String]) -> (String, u64) {
    let last = lines.last().expect("at least one marker");
    let mut it = last.split(' ');
    let kind = it.next().expect("kind").to_string();
    let n = it.next().expect("n").parse().expect("rename number");
    (kind, n)
}

/// 子を起動し、`min_renames` 回以上の改名が済んだら `jitter` 待って SIGKILL する。
fn run_and_kill(db: &Path, log: &Path, min_renames: u64, jitter: Duration) -> Vec<String> {
    let exe = std::env::current_exe().expect("current_exe");
    let child = Command::new(exe)
        .args([
            "--exact",
            "rename_table_is_atomic_under_sigkill",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_ENV, "rename")
        .env(DB_ENV, db)
        .env(LOG_ENV, log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");
    let mut guard = ChildGuard(child);
    let start = Instant::now();
    loop {
        let lines = complete_lines(log);
        if let Some(last) = lines.last() {
            if last.starts_with("RENAMED ") && last_marker(&lines).1 >= min_renames {
                break;
            }
        }
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            panic!("child exited before the kill point: {status:?}");
        }
        assert!(
            start.elapsed() < POLL_TIMEOUT,
            "child did not reach {min_renames} renames within {POLL_TIMEOUT:?}"
        );
        std::thread::yield_now();
    }
    if !jitter.is_zero() {
        std::thread::sleep(jitter);
    }
    guard.0.kill().expect("SIGKILL child");
    guard.0.wait().expect("reap child");
    complete_lines(log)
}

fn assert_rename_state(db: &Path, lines: &[String]) {
    let core = EngineCore::from_storage(
        Storage::open(db).expect("reopen storage after SIGKILL"),
        Box::new(CpuScalarProvider),
    );
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    let (kind, n) = last_marker(lines);
    let msg = format!("last marker = {kind} {n}");

    // 改名後の名前は NAMES[n % 2]。マーカー BEGIN n は commit の前後どちらでもよい
    // （その場合は改名前の名前 NAMES[(n-1) % 2] か改名後の NAMES[n % 2]）。
    let committed = NAMES[(n % 2) as usize];
    let previous = NAMES[((n + 1) % 2) as usize];
    let present: Vec<&str> = NAMES
        .iter()
        .copied()
        .filter(|t| count(&core, &alice, t).is_ok())
        .collect();
    assert_eq!(present.len(), 1, "exactly one name must exist: {msg}");
    let live = present[0];
    let missing = if live == NAMES[0] { NAMES[1] } else { NAMES[0] };
    assert_eq!(
        count(&core, &alice, missing).expect_err("missing name"),
        "42P01",
        "{msg}"
    );
    match kind.as_str() {
        "RENAMED" | "SEEDED" => assert_eq!(live, committed, "{msg}"),
        "BEGIN" => assert!(live == committed || live == previous, "{msg}"),
        other => panic!("unexpected marker {other}"),
    }
    // 全行が存在する側にあり、テナント境界は保たれる。
    assert_eq!(
        count(&core, &alice, live).expect("alice"),
        ALICE_ROWS,
        "{msg}"
    );
    assert_eq!(count(&core, &bob, live).expect("bob"), BOB_ROWS, "{msg}");
    // 存在しない側の名前を新規作成すると空（孤児の行テーブルを引き継がない）。
    ok(
        &core,
        &alice,
        &format!("CREATE TABLE {missing} (id BIGINT PRIMARY KEY, code TEXT, embedding VECTOR(2) NOT NULL)"),
    );
    assert_eq!(count(&core, &alice, missing).expect("fresh"), 0, "{msg}");
    assert_eq!(count(&core, &bob, missing).expect("fresh bob"), 0, "{msg}");
}

#[test]
fn rename_table_is_atomic_under_sigkill() {
    if let Some((role, db, log)) = child_env() {
        assert_eq!(role, "rename");
        child_workload(&db, &log);
        return;
    }
    const JITTER_US: [u64; 6] = [0, 30, 100, 300, 800, 2_000];
    for round in 0..KILL_ROUNDS {
        let db = unique_db_path("t38-crash-rename");
        let _db_guard = CleanupGuard(db.clone());
        let log = db.with_extension("marks");
        let _ = std::fs::remove_file(&log);
        let lines = run_and_kill(
            &db,
            &log,
            1 + (round % 5) as u64,
            Duration::from_micros(JITTER_US[round % JITTER_US.len()]),
        );
        assert_rename_state(&db, &lines);
        let _ = std::fs::remove_file(&log);
    }
}
