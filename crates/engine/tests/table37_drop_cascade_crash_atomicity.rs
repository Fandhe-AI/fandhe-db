//! `DROP TABLE ... CASCADE`（TABLE-37 (3)(5) のポインタ、Issue #1641）の **クラッシュ時
//! 単一トランザクション性** の結合テスト（`table38_rename_table_crash_atomicity.rs` と同型の
//! 自己再帰子プロセス）。
//!
//! 子プロセス（`std::env::current_exe()` を環境変数付きで再実行する）が「親表 `p`・子表 `c`
//! （`p` への FK）・`p` 上のビュー `v` を作り、alice・bob の行を投入 → `DROP TABLE p CASCADE`
//! → 子表 `c` を落として初期化」を繰り返し、各段階でマーカーをファイルへ追記する。親は
//! jitter を付けて SIGKILL し、`Storage` を開き直して次の不変条件（kill のタイミングに依存
//! しない）を確認する:
//!
//! - `p`・`v`・`c` の FK 制約は「すべて在る」か「すべて無い」かのどちらか（中間状態なし）
//! - `c` が存在するなら alice・bob の全行が揃っている（連鎖削除は子表の行に触れない）
//! - `SETUP_DONE n` が最後のマーカーなら全部が在る。`DROPPED n` が最後なら `p`・`v` は無く、
//!   `c` の FK も無い（`c` が既に初期化で落とされていてもよい）
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

const ROLE_ENV: &str = "TABLE37_CRASH_CHILD_ROLE";
const DB_ENV: &str = "TABLE37_CRASH_CHILD_DB";
const LOG_ENV: &str = "TABLE37_CRASH_CHILD_LOG";

/// 子の最大反復回数（暴走防止。親は必ずこれより先に kill する）。
const MAX_ROUNDS: u64 = 2_000;
const ALICE_ROWS: u64 = 40;
const BOB_ROWS: u64 = 20;
const BOB_ID_BASE: u64 = 100_001;
/// 親表 `p` の行 id（FK の参照先。テナントごとに自分の行だけが参照できる）。
const ALICE_PARENT: u64 = 1;
const BOB_PARENT: u64 = 100_001;
const KILL_ROUNDS: usize = 16;
const POLL_TIMEOUT: Duration = Duration::from_secs(60);

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

fn insert_children_sql(first: u64, count: u64, parent: u64, op: &str) -> String {
    let values: Vec<String> = (first..first + count)
        .map(|id| format!("({id}, {parent})"))
        .collect();
    format!(
        "INSERT INTO c (id, pid) VALUES {} USING OPERATION_ID '{op}'",
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

/// 子の負荷。反復 `n` ごとに `p`・`c`（FK）・`v` を作って投入し、`SETUP_DONE n` を書く。
/// `DROP TABLE p CASCADE` の直前に `BEGIN n`、commit 後に `DROPPED n`、子表 `c` を落として
/// 初期化した後に `RESET n` を書く。
fn child_workload(db: &Path, log: &Path) {
    let storage = Storage::open(db).expect("child: open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    for n in 1..=MAX_ROUNDS {
        ok(&core, &alice, "CREATE TABLE p (name TEXT)");
        ok(
            &core,
            &alice,
            "CREATE TABLE c (pid BIGINT REFERENCES p, note TEXT)",
        );
        ok(&core, &alice, "CREATE VIEW v AS SELECT id FROM p LIMIT 5");
        ok(
            &core,
            &alice,
            &format!(
                "INSERT INTO p (id, name) VALUES ({ALICE_PARENT}, 'a') USING OPERATION_ID 'pa-{n}'"
            ),
        );
        ok(
            &core,
            &bob,
            &format!(
                "INSERT INTO p (id, name) VALUES ({BOB_PARENT}, 'b') USING OPERATION_ID 'pb-{n}'"
            ),
        );
        ok(
            &core,
            &alice,
            &insert_children_sql(1, ALICE_ROWS, ALICE_PARENT, &format!("ca-{n}")),
        );
        ok(
            &core,
            &bob,
            &insert_children_sql(BOB_ID_BASE, BOB_ROWS, BOB_PARENT, &format!("cb-{n}")),
        );
        mark(log, &format!("SETUP_DONE {n}"));
        mark(log, &format!("BEGIN {n}"));
        ok(&core, &alice, "DROP TABLE p CASCADE");
        mark(log, &format!("DROPPED {n}"));
        ok(&core, &alice, "DROP TABLE c");
        mark(log, &format!("RESET {n}"));
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
    let n = it.next().expect("n").parse().expect("round number");
    (kind, n)
}

/// 子を起動し、`min_round` 回目以降の `BEGIN` が書かれたら `jitter` 待って SIGKILL する。
fn run_and_kill(db: &Path, log: &Path, min_round: u64, jitter: Duration) -> Vec<String> {
    let exe = std::env::current_exe().expect("current_exe");
    let child = Command::new(exe)
        .args([
            "--exact",
            "drop_cascade_is_atomic_under_sigkill",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_ENV, "cascade")
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
            if last.starts_with("BEGIN ") && last_marker(&lines).1 >= min_round {
                break;
            }
        }
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            panic!("child exited before the kill point: {status:?}");
        }
        assert!(
            start.elapsed() < POLL_TIMEOUT,
            "child did not reach round {min_round} within {POLL_TIMEOUT:?}"
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

fn relation_exists(core: &EngineCore, c: &PolicyContext, rel: &str) -> bool {
    match run(core, c, &format!("SELECT id FROM {rel} LIMIT 1")) {
        Ok(_) => true,
        Err(code) => {
            assert_eq!(code, "42P01", "unexpected error probing {rel}");
            false
        }
    }
}

/// FK 制約が効いているか（存在しない親を指す行の INSERT が `23503` になるか）。成功した場合は
/// 行が増えるため、行数の検査より後に呼ぶ。
fn fk_enforced(core: &EngineCore, c: &PolicyContext) -> bool {
    match run(
        core,
        c,
        "INSERT INTO c (id, pid) VALUES (900000, 777777) USING OPERATION_ID 'probe-fk'",
    ) {
        Ok(_) => false,
        Err(code) => {
            assert_eq!(code, "23503", "unexpected error probing FK");
            true
        }
    }
}

fn assert_cascade_state(db: &Path, lines: &[String]) {
    let core = EngineCore::from_storage(
        Storage::open(db).expect("reopen storage after SIGKILL"),
        Box::new(CpuScalarProvider),
    );
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    let (kind, n) = last_marker(lines);
    let msg = format!("last marker = {kind} {n}");
    // 初期化（`c` の DROP）以降は次反復の途中状態で不変条件の対象外。
    if kind == "RESET" {
        return;
    }
    let p = relation_exists(&core, &alice, "p");
    let v = relation_exists(&core, &alice, "v");
    let c = relation_exists(&core, &alice, "c");
    assert_eq!(p, v, "p and v must disappear together: {msg}");
    match kind.as_str() {
        "SETUP_DONE" => assert!(p && c, "{msg}"),
        "BEGIN" => {}
        "DROPPED" => assert!(!p, "{msg}"),
        other => panic!("unexpected marker {other}"),
    }
    if c {
        // 連鎖削除は子表の行に触れない（全テナント分が揃っている）。
        assert_eq!(
            count(&core, &alice, "c").expect("alice"),
            ALICE_ROWS,
            "{msg}"
        );
        assert_eq!(count(&core, &bob, "c").expect("bob"), BOB_ROWS, "{msg}");
        // FK は p と運命を共にする（p が在るなら効き、無ければ外れている）。
        assert_eq!(fk_enforced(&core, &alice), p, "FK must follow p: {msg}");
    } else {
        assert_eq!(kind, "DROPPED", "c may only vanish after DROPPED: {msg}");
        assert!(!p, "{msg}");
    }
}

#[test]
fn drop_cascade_is_atomic_under_sigkill() {
    if let Some((role, db, log)) = child_env() {
        assert_eq!(role, "cascade");
        child_workload(&db, &log);
        return;
    }
    const JITTER_US: [u64; 6] = [0, 30, 100, 300, 800, 2_000];
    for round in 0..KILL_ROUNDS {
        let db = unique_db_path("t37-crash-cascade");
        let _db_guard = CleanupGuard(db.clone());
        let log = db.with_extension("marks");
        let _ = std::fs::remove_file(&log);
        let lines = run_and_kill(
            &db,
            &log,
            1 + (round % 5) as u64,
            Duration::from_micros(JITTER_US[round % JITTER_US.len()]),
        );
        assert_cascade_state(&db, &lines);
        let _ = std::fs::remove_file(&log);
    }
}
