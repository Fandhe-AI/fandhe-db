//! 変更ログ（RECOVER-14 のポインタ。Issue #3069。設計は `docs/design/change-log-pitr.md` の D1・D8・D10）の
//! **クラッシュ時の原子性** の結合テスト。レコードの追記と世代（commit 番号）の更新が同じ redb
//! トランザクションに載ることを、SIGKILL で確かめる。
//!
//! 子プロセス（自己再帰: `std::env::current_exe()` を環境変数付きで再実行する。`table15_crash_atomicity.rs`
//! と同型）が変更ログを有効にして DDL・DML の commit を回し続け、親が進捗マーカー（ファイルへの追記）を
//! 見て、ばらつかせた時刻に SIGKILL する。親は redb を読み取り専用で開き直し、**どの時点で kill が当たっても
//! 成立する不変条件**を確かめる:
//!
//! 1. `change_log` の LSN は欠落なく連続する（片方だけ残らない）
//! 2. 最後のレコードの LSN は `storage_generation` と一致する（世代だけ進んでレコードが無い、
//!    レコードだけあって世代が進んでいない、のどちらも無い）
//! 3. 最初のレコードの LSN は有効化時の世代（`base_generation`）の次である
//!
//! 同じ DB に対して kill と再起動（再有効化）を繰り返すため、再有効化時の連続性検査
//! （`Storage::enable_change_log`）が、クラッシュ後の DB を誤って拒否しないことも確かめる。
//! `WriteDurability::Immediate`（既定）のため、マーカーを書けた時点の commit は SIGKILL 後も必ず見える。
//! sleep は kill 位置のばらつき付与にのみ使い、判定には使わない。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ROLE_ENV: &str = "CHANGE_LOG_CRASH_CHILD_ROLE";
const DB_ENV: &str = "CHANGE_LOG_CRASH_CHILD_DB";
const LOG_ENV: &str = "CHANGE_LOG_CRASH_CHILD_LOG";

const MAX_CYCLES: u64 = 5_000;
const KILL_ROUNDS: usize = 24;
const POLL_TIMEOUT: Duration = Duration::from_secs(60);

const CHANGE_LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("change_log");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("change_log_meta");
const GENERATION: TableDefinition<&str, u64> = TableDefinition::new("storage_generation");

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, sql: &str) -> Result<(), String> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
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

/// 子プロセスの負荷。DDL・DML・TRUNCATE・rename を混ぜた commit を、1 文 1 commit で回し続ける。
fn child_workload(db: &Path, log: &Path) {
    let mut storage = Storage::open(db).expect("child: open storage");
    storage
        .enable_change_log()
        .expect("child: a crashed database must be re-enabled without refusal");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // 既存なら `42P07`。クラッシュ後の再起動では既にある。
    let _ = run(
        &core,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, n INTEGER, embedding VECTOR(2) NOT NULL)",
    );
    // rename の途中で kill されると退避名の表が残るため、再起動のたびに片付ける。
    let _ = run(&core, "DROP TABLE docs_tmp");
    mark(log, "READY 0");
    for k in 1..=MAX_CYCLES {
        let base = (k * 7) % 300;
        let values: Vec<String> = (0..20)
            .map(|i| format!("({}, {k}, '[0.1,0.2]')", base + i))
            .collect();
        run(
            &core,
            &format!(
                "INSERT INTO docs (id, n, embedding) VALUES {} \
                 ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n",
                values.join(",")
            ),
        )
        .expect("child: upsert");
        mark(log, &format!("OP {k}"));
        run(
            &core,
            &format!("UPDATE docs SET n = n + 1 WHERE id = {}", base + 3),
        )
        .expect("child: update");
        run(&core, &format!("DELETE FROM docs WHERE id = {}", base + 5)).expect("child: delete");
        if k % 5 == 0 {
            run(&core, "TRUNCATE TABLE docs").expect("child: truncate");
        }
        if k % 7 == 0 {
            run(&core, "ALTER TABLE docs RENAME TO docs_tmp").expect("child: rename");
            mark(log, &format!("RENAMED {k}"));
            run(&core, "ALTER TABLE docs_tmp RENAME TO docs").expect("child: rename back");
        }
        mark(log, &format!("OP {k}"));
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn complete_lines(log: &Path) -> usize {
    match std::fs::read_to_string(log) {
        Ok(s) => s
            .split_inclusive('\n')
            .filter(|l| l.ends_with('\n'))
            .count(),
        Err(_) => 0,
    }
}

/// 子を起動し、マーカーが `min_lines` 行に達したら `jitter` 待って SIGKILL する。
fn run_and_kill(test_name: &str, db: &Path, log: &Path, min_lines: usize, jitter: Duration) {
    let exe = std::env::current_exe().expect("current_exe");
    let child = Command::new(exe)
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, "workload")
        .env(DB_ENV, db)
        .env(LOG_ENV, log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");
    let mut guard = ChildGuard(child);
    let start = Instant::now();
    let already = complete_lines(log);
    while complete_lines(log) < already + min_lines {
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            panic!("child exited before the kill point: {status:?}");
        }
        assert!(
            start.elapsed() < POLL_TIMEOUT,
            "child did not reach {min_lines} markers within {POLL_TIMEOUT:?}"
        );
        std::thread::yield_now();
    }
    if !jitter.is_zero() {
        std::thread::sleep(jitter);
    }
    guard.0.kill().expect("SIGKILL child");
    guard.0.wait().expect("reap child");
}

/// kill 後の DB が満たす不変条件。`(最初の LSN, 最後の LSN, 世代)` を返す。
fn check_invariants(db: &Path) -> (Option<u64>, Option<u64>, u64) {
    let database = redb::Database::open(db).expect("reopen redb after SIGKILL");
    let read = database.begin_read().expect("begin_read");
    let generation = match read.open_table(GENERATION) {
        Ok(t) => t
            .get("generation")
            .expect("get generation")
            .map_or(0, |v| v.value()),
        Err(redb::TableError::TableDoesNotExist(_)) => 0,
        Err(e) => panic!("open generation: {e}"),
    };
    let base = {
        let meta = read
            .open_table(META)
            .expect("the log must have been enabled");
        let v = meta
            .get("base_generation")
            .expect("get base")
            .expect("base_generation is written with the identity");
        u64::from_le_bytes(v.value().try_into().expect("8 bytes"))
    };
    let lsns: Vec<u64> = match read.open_table(CHANGE_LOG) {
        Ok(t) => t
            .iter()
            .expect("iter")
            .map(|e| e.expect("entry").0.value())
            .collect(),
        Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
        Err(e) => panic!("open change_log: {e}"),
    };
    // 1. 連続（欠落なし）。
    for pair in lsns.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "LSN holes after a crash");
    }
    // 2. 最後の LSN = 世代（レコードのない世代も、世代のないレコードも無い）。
    //    3. 最初の LSN = 有効化時の世代の次。
    match (lsns.first(), lsns.last()) {
        (Some(first), Some(last)) => {
            assert_eq!(
                *first,
                base + 1,
                "the first record follows the base generation"
            );
            assert_eq!(*last, generation, "the last record matches the generation");
        }
        _ => assert_eq!(generation, base, "no record: the generation is the base"),
    }
    (lsns.first().copied(), lsns.last().copied(), generation)
}

/// 子プロセス側のエントリ（親からの再実行時だけ動く）。
fn run_child_if_requested() -> bool {
    if std::env::var(ROLE_ENV).is_err() {
        return false;
    }
    let db = PathBuf::from(std::env::var(DB_ENV).expect("child: db env"));
    let log = PathBuf::from(std::env::var(LOG_ENV).expect("child: log env"));
    child_workload(&db, &log);
    true
}

/// RECOVER-14: SIGKILL をどの時点で受けても、レコードと世代は同じ commit に載ったままで、
/// クラッシュ後の DB は連続性検査に拒否されずに再び有効化できる。
#[test]
fn change_log_records_and_generation_stay_together_across_sigkill() {
    if run_child_if_requested() {
        return;
    }
    let path = unique_db_path("change-log-crash");
    let _guard = CleanupGuard(path.clone());
    let log = path.with_extension("markers");
    let _ = std::fs::remove_file(&log);
    let test_name = "change_log_records_and_generation_stay_together_across_sigkill";

    const JITTER_US: [u64; 6] = [0, 30, 100, 300, 800, 2_000];
    let mut last_generation = 0;
    for round in 0..KILL_ROUNDS {
        // 開始直後から数十 commit 後まで、kill 位置をばらつかせる。
        let min_lines = 1 + (round * 5) % 23;
        let jitter = Duration::from_micros(JITTER_US[(round / 4) % JITTER_US.len()]);
        run_and_kill(test_name, &path, &log, min_lines, jitter);
        let (_, last, generation) = check_invariants(&path);
        assert!(
            generation >= last_generation,
            "the generation never goes back"
        );
        last_generation = generation;
        assert!(last.is_some() || round == 0, "records accumulate");
    }
    let _ = std::fs::remove_file(&log);
    println!("change_log_crash: rounds={KILL_ROUNDS} final_generation={last_generation}");
    assert!(
        last_generation >= KILL_ROUNDS as u64,
        "the workload committed in every round"
    );
}
