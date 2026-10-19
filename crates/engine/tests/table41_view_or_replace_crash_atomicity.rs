//! `CREATE OR REPLACE VIEW` の依存ビュー再検証つき置換（TABLE-41 (3) のポインタ、Issue #1661）の
//! **クラッシュ時単一トランザクション性** の結合テスト（`table38_rename_table_crash_atomicity.rs`
//! と同型の自己再帰子プロセス）。
//!
//! 子プロセス（`std::env::current_exe()` を環境変数付きで再実行する）が `docs` に行を投入し、
//! ビュー `v1`（と、それを読む依存ビュー `v2`）を作った後、`v1` を 2 つの互換な定義
//! （`WHERE kind = 'a'`／`WHERE kind = 'b'`。列は同一で行数が異なる）の間で交互に置換する。
//! 置換のたびに進捗マーカーをファイルへ追記し、親は jitter を付けて SIGKILL して `Storage` を
//! 開き直し、次の不変条件（kill のタイミングに依存しない）を確認する:
//!
//! - `v1` の観測行数は 2 定義のどちらか一方に一致する（中間状態がない）
//! - その定義は、最後のマーカーが示す定義か、その次の定義のどちらか
//! - 依存ビュー `v2` は問い合わせ可能で、`v1` と同じ行集合を返す（依存ビューが宙に浮かない）
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
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ROLE_ENV: &str = "TABLE41_CRASH_CHILD_ROLE";
const DB_ENV: &str = "TABLE41_CRASH_CHILD_DB";
const LOG_ENV: &str = "TABLE41_CRASH_CHILD_LOG";

/// 子の最大置換回数（暴走防止。親は必ずこれより先に kill する）。
const MAX_REPLACES: u64 = 2_000;
const A_ROWS: u64 = 10;
const B_ROWS: u64 = 30;
const KILL_ROUNDS: usize = 16;
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
/// 置換 n 回後の `v1` の定義（`n % 2`）と、そのとき観測される行数。
const VARIANTS: [(&str, u64); 2] = [("a", A_ROWS), ("b", B_ROWS)];

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

fn insert_sql() -> String {
    let values: Vec<String> = (1..=A_ROWS + B_ROWS)
        .map(|id| {
            let kind = if id <= A_ROWS { "a" } else { "b" };
            format!("({id}, 'c{id}', '{kind}', '[0.1,0.2]')")
        })
        .collect();
    format!(
        "INSERT INTO docs (id, code, kind, embedding) VALUES {} USING OPERATION_ID 'seed-a'",
        values.join(",")
    )
}

fn replace_sql(n: u64) -> String {
    let kind = VARIANTS[(n % 2) as usize].0;
    format!("CREATE OR REPLACE VIEW v1 AS SELECT code, kind FROM docs WHERE kind = '{kind}'")
}

/// ビューの行 id 集合（昇順）。不在・失敗は `Err(wire_code)`。
fn view_ids(core: &EngineCore, c: &PolicyContext, view: &str) -> Result<Vec<u64>, String> {
    match run(core, c, &format!("SELECT * FROM {view} LIMIT 1000"))? {
        SqlOutcome::Query(q) => {
            let mut ids: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
            ids.sort_unstable();
            Ok(ids)
        }
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

/// 子の負荷。準備完了後に `SEEDED 0` を書き、以降は置換の直前に `BEGIN <n>`、commit 後に
/// `REPLACED <n>`（`n` 回目の置換後の定義は `VARIANTS[n % 2]`）を書く。
fn child_workload(db: &Path, log: &Path) {
    let storage = Storage::open(db).expect("child: open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "CREATE TABLE docs (code TEXT, kind TEXT, embedding VECTOR(2) NOT NULL)",
    );
    ok(&core, &alice, &insert_sql());
    ok(&core, &alice, &replace_sql(0).replace(" OR REPLACE", ""));
    ok(&core, &alice, "CREATE VIEW v2 AS SELECT code FROM v1");
    mark(log, "SEEDED 0");
    for n in 1..=MAX_REPLACES {
        mark(log, &format!("BEGIN {n}"));
        ok(&core, &alice, &replace_sql(n));
        mark(log, &format!("REPLACED {n}"));
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
    let n = it.next().expect("n").parse().expect("replace number");
    (kind, n)
}

/// 子を起動し、`min_replaces` 回以上の置換が済んだら `jitter` 待って SIGKILL する。
fn run_and_kill(db: &Path, log: &Path, min_replaces: u64, jitter: Duration) -> Vec<String> {
    let exe = std::env::current_exe().expect("current_exe");
    let child = Command::new(exe)
        .args([
            "--exact",
            "replace_view_with_dependents_is_atomic_under_sigkill",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_ENV, "replace")
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
            if last.starts_with("REPLACED ") && last_marker(&lines).1 >= min_replaces {
                break;
            }
        }
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            panic!("child exited before the kill point: {status:?}");
        }
        assert!(
            start.elapsed() < POLL_TIMEOUT,
            "child did not reach {min_replaces} replaces within {POLL_TIMEOUT:?}"
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

fn assert_replace_state(db: &Path, lines: &[String]) {
    let core = EngineCore::from_storage(
        Storage::open(db).expect("reopen storage after SIGKILL"),
        Box::new(CpuScalarProvider),
    );
    let alice = ctx("alice");
    let (kind, n) = last_marker(lines);
    let msg = format!("last marker = {kind} {n}");

    let v1 = view_ids(&core, &alice, "v1").unwrap_or_else(|c| panic!("v1 query {c}: {msg}"));
    let v2 = view_ids(&core, &alice, "v2").unwrap_or_else(|c| panic!("v2 query {c}: {msg}"));
    // 依存ビューは v1 と同じ行集合を返す（v1 が中間状態・宙に浮いた状態にならない）。
    assert_eq!(v1, v2, "{msg}");

    // 置換 n 回後の定義は VARIANTS[n % 2]。BEGIN n は commit の前後どちらでもよい。
    let committed = VARIANTS[(n % 2) as usize].1;
    let previous = VARIANTS[((n + 1) % 2) as usize].1;
    let observed = v1.len() as u64;
    match kind.as_str() {
        "REPLACED" | "SEEDED" => assert_eq!(observed, committed, "{msg}"),
        "BEGIN" => assert!(observed == committed || observed == previous, "{msg}"),
        other => panic!("unexpected marker {other}"),
    }
}

#[test]
fn replace_view_with_dependents_is_atomic_under_sigkill() {
    if let Some((role, db, log)) = child_env() {
        assert_eq!(role, "replace");
        child_workload(&db, &log);
        return;
    }
    const JITTER_US: [u64; 6] = [0, 30, 100, 300, 800, 2_000];
    for round in 0..KILL_ROUNDS {
        let db = unique_db_path("t41-crash-replace");
        let _db_guard = CleanupGuard(db.clone());
        let log = db.with_extension("marks");
        let _ = std::fs::remove_file(&log);
        let lines = run_and_kill(
            &db,
            &log,
            1 + (round % 5) as u64,
            Duration::from_micros(JITTER_US[round % JITTER_US.len()]),
        );
        assert_replace_state(&db, &lines);
        let _ = std::fs::remove_file(&log);
    }
}
