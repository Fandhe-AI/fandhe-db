//! TRUNCATE（SQL-22）と DROP TABLE（TABLE-15、関連: SQL-23・RECOVER-1/2/10・
//! RLS-9、Issue #1200）の **クラッシュ時単一トランザクション性** の結合テスト。
//!
//! 子プロセス（自己再帰: `std::env::current_exe()` を環境変数付きで再実行する。
//! `recover6_panic_hook.rs` と同型）が「投入 → TRUNCATE／DROP」のサイクルを回し、
//! 親が進捗マーカー（ファイルへの追記。標準出力は使わない）の行数を見て SIGKILL
//! する。親は `Storage` を開き直し、最後のマーカーに応じた **どちらか一方の状態**
//! だけが観測できること（全部反映か何も反映されないか）を確認する:
//!
//! - TRUNCATE: 自テナント行数は `{0, M}` のみ。`operation_id` 台帳・一意索引が行数と
//!   整合し、他テナント（bob）の行は常に無傷。
//! - DROP: 「テーブル・全行・台帳・一意索引が揃って存在」か「`42P01` で、同名再作成後に
//!   旧 `operation_id`・旧 UNIQUE 値を再利用できる」かのどちらか。途中状態は失敗。
//!
//! オラクルは kill のタイミングに依存しない（どの段階で kill が当たっても成立する
//! 不変条件）。`WriteDurability::Immediate`（既定）のため、マーカーを書けた時点の
//! commit は SIGKILL 後も必ず見える。sleep は親のポーリング間隔と kill 位置の
//! ばらつき付与にのみ使い、判定には使わない。

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

const ROLE_ENV: &str = "TABLE15_CRASH_CHILD_ROLE";
const DB_ENV: &str = "TABLE15_CRASH_CHILD_DB";
const LOG_ENV: &str = "TABLE15_CRASH_CHILD_LOG";
/// 親の狙いマーカー（`"<kind> <最小サイクル>"`）。子はこのマーカーを書いた直後に
/// 親の合図（`go` ファイル）を待ってから次の操作へ進む。
const HOLD_ENV: &str = "TABLE15_CRASH_CHILD_HOLD";

/// 子の最大サイクル数（暴走防止。親は必ずこれより先に kill する）。
const MAX_CYCLES: u64 = 1_000;
const ALICE_ROWS: u64 = 60;
const BOB_ROWS: u64 = 30;
const BOB_ID_BASE: u64 = 100_001;
const KILL_ROUNDS: usize = 24;
const POLL_TIMEOUT: Duration = Duration::from_secs(30);

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
        // 巨大な一括 INSERT 文をそのまま出さないよう先頭のみ表示する。
        panic!("{} must succeed, got {code}", &sql[..sql.len().min(80)]);
    }
}

fn err(core: &EngineCore, c: &PolicyContext, sql: &str) -> String {
    match run(core, c, sql) {
        Ok(_) => panic!("{} must fail", &sql[..sql.len().min(80)]),
        Err(code) => code,
    }
}

const CREATE_DOCS: &str = "CREATE TABLE docs (code TEXT UNIQUE, embedding VECTOR(2) NOT NULL)";

/// `ids` の行を 1 文（1 txn）で投入する SQL。`code` は `<prefix><id>`。
fn insert_sql(prefix: &str, first: u64, count: u64, op: &str) -> String {
    let values: Vec<String> = (first..first + count)
        .map(|id| format!("({id}, '{prefix}{id}', '[0.1,0.2]')"))
        .collect();
    format!(
        "INSERT INTO docs (id, code, embedding) VALUES {} USING OPERATION_ID '{op}'",
        values.join(",")
    )
}

/// `COUNT(*)`。テーブル不在は `Err("42P01")`。
fn count(core: &EngineCore, c: &PolicyContext) -> Result<u64, String> {
    match run(core, c, "SELECT COUNT(*) FROM docs")? {
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
    hold_at_target(log, line);
}

/// 狙いマーカーを書いた直後に 1 回だけ、親の合図（`go` ファイルの出現）を待つ。
/// 親のポーリングが負荷で遅れても狙いマーカーが最新行のまま残り、狙いを取り逃して
/// 子が全サイクルを終えてしまうことを防ぐ（kill 位置は合図後の `jitter` でばらつく）。
/// 合図が来ないまま上限を過ぎた場合は進む（親側の `POLL_TIMEOUT` が失敗として扱う）。
fn hold_at_target(log: &Path, line: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HELD: AtomicBool = AtomicBool::new(false);
    let Ok(spec) = std::env::var(HOLD_ENV) else {
        return;
    };
    let mut want = spec.split(' ');
    let (Some(kind), Some(min)) = (want.next(), want.next()) else {
        return;
    };
    let min: u64 = min.parse().expect("child: hold cycle");
    let mut got = line.split(' ');
    let (Some(got_kind), Some(got_k)) = (got.next(), got.next()) else {
        return;
    };
    let got_k: u64 = got_k.parse().expect("child: marker cycle");
    if got_kind != kind || got_k < min || HELD.swap(true, Ordering::SeqCst) {
        return;
    }
    let go = log.with_extension("go");
    let start = Instant::now();
    while !go.exists() && start.elapsed() < POLL_TIMEOUT {
        std::thread::yield_now();
    }
}

fn child_env() -> Option<(String, PathBuf, PathBuf)> {
    let role = std::env::var(ROLE_ENV).ok()?;
    let db = PathBuf::from(std::env::var(DB_ENV).expect("child: db env"));
    let log = PathBuf::from(std::env::var(LOG_ENV).expect("child: log env"));
    Some((role, db, log))
}

// ---------------------------------------------------------------------------
// 子プロセス側の負荷
// ---------------------------------------------------------------------------

fn child_truncate_workload(db: &Path, log: &Path) {
    let storage = Storage::open(db).expect("child: open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    ok(&core, &alice, CREATE_DOCS);
    ok(
        &core,
        &bob,
        &insert_sql("b", BOB_ID_BASE, BOB_ROWS, "seed-bob"),
    );
    for k in 1..=MAX_CYCLES {
        mark(log, &format!("BEGIN_SEED {k}"));
        ok(
            &core,
            &alice,
            &insert_sql("c", 1, ALICE_ROWS, &format!("seed-{k}")),
        );
        mark(log, &format!("SEEDED {k}"));
        mark(log, &format!("BEGIN_TRUNCATE {k}"));
        ok(
            &core,
            &alice,
            &format!("TRUNCATE TABLE docs USING OPERATION_ID 'trunc-{k}'"),
        );
        mark(log, &format!("TRUNCATED {k}"));
    }
}

fn child_drop_workload(db: &Path, log: &Path) {
    let storage = Storage::open(db).expect("child: open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    for k in 1..=MAX_CYCLES {
        mark(log, &format!("BEGIN_CYCLE {k}"));
        ok(&core, &alice, CREATE_DOCS);
        ok(
            &core,
            &alice,
            &insert_sql("c", 1, ALICE_ROWS, &format!("seed-a-{k}")),
        );
        ok(
            &core,
            &bob,
            &insert_sql("b", BOB_ID_BASE, BOB_ROWS, &format!("seed-b-{k}")),
        );
        mark(log, &format!("SEEDED {k}"));
        mark(log, &format!("BEGIN_DROP {k}"));
        ok(&core, &alice, "DROP TABLE docs");
        mark(log, &format!("DROPPED {k}"));
    }
}

// ---------------------------------------------------------------------------
// 親プロセス側の共通部
// ---------------------------------------------------------------------------

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

/// 子を起動し、狙いマーカー（`target`・サイクル `min_cycle` 以上）が最新行になったら
/// 子へ合図（[`hold_at_target`]）して `jitter` 待って SIGKILL する。kill 後の（完全な
/// 行だけの）マーカー列を返す。
fn run_and_kill(
    test_name: &str,
    role: &str,
    db: &Path,
    log: &Path,
    target: &str,
    min_cycle: u64,
    jitter: Duration,
) -> Vec<String> {
    let exe = std::env::current_exe().expect("current_exe");
    let go = log.with_extension("go");
    let _ = std::fs::remove_file(&go);
    let child = Command::new(exe)
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(DB_ENV, db)
        .env(LOG_ENV, log)
        .env(HOLD_ENV, format!("{target} {min_cycle}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");
    let mut guard = ChildGuard(child);
    let start = Instant::now();
    // 目標マーカー（例: BEGIN_TRUNCATE）が最新行になった瞬間を狙って kill する
    // （子のサイクルは投入の fsync が支配的なため、固定行数での kill は投入中に
    // 偏り TRUNCATE／DROP の書き込み窓にほとんど当たらない）。判定自体は時間に
    // 依存せず、待機はスピン＋yield（sleep しない）で kill 位置の解像度を上げる。
    loop {
        let lines = complete_lines(log);
        if !lines.is_empty() {
            let (kind, k) = last_kind(&lines);
            if kind == target && k >= min_cycle {
                break;
            }
        }
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            panic!("child exited before the kill point: {status:?}");
        }
        assert!(
            start.elapsed() < POLL_TIMEOUT,
            "child did not reach marker {target} (cycle >= {min_cycle}) within {POLL_TIMEOUT:?}"
        );
        std::thread::yield_now();
    }
    // 子は狙いマーカーの直後で合図を待っている。合図してから `jitter` 後に kill する
    // （`jitter` 中に子は次の操作〔TRUNCATE／DROP・投入〕を進める）。
    std::fs::write(&go, b"").expect("write go signal");
    if !jitter.is_zero() {
        std::thread::sleep(jitter);
    }
    guard.0.kill().expect("SIGKILL child");
    guard.0.wait().expect("reap child");
    let _ = std::fs::remove_file(&go);
    complete_lines(log)
}

/// kill の狙いマーカー（ラウンドごとに巡回）。TRUNCATE／DROP の書き込み窓の直前
/// （`BEGIN_*`）と直後（`TRUNCATED`／`DROPPED`）を含める。
const TRUNCATE_TARGETS: [&str; 4] = ["BEGIN_TRUNCATE", "TRUNCATED", "SEEDED", "BEGIN_SEED"];
const DROP_TARGETS: [&str; 4] = ["BEGIN_DROP", "DROPPED", "SEEDED", "BEGIN_CYCLE"];

/// 目標マーカー検出後の追加待機（決定的な列。kill 位置をばらつかせる）。
fn jitter_for(round: usize) -> Duration {
    const JITTER_US: [u64; 6] = [0, 30, 100, 300, 800, 2_000];
    Duration::from_micros(JITTER_US[(round / 4) % JITTER_US.len()])
}

fn last_kind(lines: &[String]) -> (String, u64) {
    let last = lines.last().expect("at least one marker");
    let mut it = last.split(' ');
    let kind = it.next().expect("kind").to_string();
    let k = it.next().expect("k").parse().expect("cycle number");
    (kind, k)
}

fn reopen(db: &Path) -> EngineCore {
    let storage = Storage::open(db).expect("reopen storage after SIGKILL");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

// ---------------------------------------------------------------------------
// TRUNCATE
// ---------------------------------------------------------------------------

fn assert_truncate_state(db: &Path, lines: &[String]) {
    let core = reopen(db);
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    let (kind, k) = last_kind(lines);
    let ctx_msg = format!("last marker = {kind} {k}");

    // 他テナントは常に無傷。
    assert_eq!(
        count(&core, &bob).expect("bob count"),
        BOB_ROWS,
        "{ctx_msg}"
    );

    let n = count(&core, &alice).expect("alice count");
    assert!(n == 0 || n == ALICE_ROWS, "partial state n={n}: {ctx_msg}");

    let trunc = format!("TRUNCATE TABLE docs USING OPERATION_ID 'trunc-{k}'");
    match kind.as_str() {
        // マーカー TRUNCATED は commit 後に書くため、必ず反映済みで台帳も残る。
        "TRUNCATED" => {
            assert_eq!(n, 0, "{ctx_msg}");
            assert_eq!(err(&core, &alice, &trunc), "23505", "{ctx_msg}");
        }
        // SEEDED は INSERT の commit 後に書く。TRUNCATE はまだ開始していない。
        "SEEDED" => {
            assert_eq!(n, ALICE_ROWS, "{ctx_msg}");
            ok(&core, &alice, &trunc);
        }
        // TRUNCATE が commit 済みか未 commit のどちらか（全部か何もか）。
        "BEGIN_TRUNCATE" => {
            if n == ALICE_ROWS {
                ok(&core, &alice, &trunc);
            } else {
                assert_eq!(err(&core, &alice, &trunc), "23505", "{ctx_msg}");
            }
        }
        // 投入の commit 前後どちらでもよい（行数は {0, M} を上で確認済み）。
        "BEGIN_SEED" => {
            let seed = insert_sql("c", 1, ALICE_ROWS, &format!("seed-{k}"));
            if n == ALICE_ROWS {
                // 投入が commit 済みなら台帳も残っている。
                assert_eq!(err(&core, &alice, &seed), "23505", "{ctx_msg}");
            } else {
                // 未反映なら台帳も残っていない（同じ operation_id で再投入できる）。
                ok(&core, &alice, &seed);
            }
        }
        other => panic!("unexpected marker {other}"),
    }

    // 一意索引が行と整合している（行が残るなら重複拒否・0 件なら再投入可）。
    let n_now = count(&core, &alice).expect("alice count");
    let dup = "INSERT INTO docs (id, code, embedding) VALUES (900001, 'c1', '[0.1,0.2]') \
               USING OPERATION_ID 'probe-dup'";
    if n_now == 0 {
        ok(&core, &alice, dup);
    } else {
        assert_eq!(err(&core, &alice, dup), "23505", "{ctx_msg}");
    }
    assert_eq!(
        count(&core, &bob).expect("bob count"),
        BOB_ROWS,
        "{ctx_msg}"
    );
}

#[test]
fn truncate_is_atomic_under_sigkill() {
    const NAME: &str = "truncate_is_atomic_under_sigkill";
    if let Some((role, db, log)) = child_env() {
        assert_eq!(role, "truncate");
        child_truncate_workload(&db, &log);
        return;
    }
    let mut kinds = std::collections::BTreeMap::<String, u32>::new();
    for round in 0..KILL_ROUNDS {
        let db = unique_db_path("t15-crash-truncate");
        let _db_guard = CleanupGuard(db.clone());
        let log = db.with_extension("marks");
        let _ = std::fs::remove_file(&log);
        let lines = run_and_kill(
            NAME,
            "truncate",
            &db,
            &log,
            TRUNCATE_TARGETS[round % TRUNCATE_TARGETS.len()],
            1 + (round % 3) as u64,
            jitter_for(round),
        );
        let _ = std::fs::remove_file(&log);
        *kinds.entry(last_kind(&lines).0).or_default() += 1;
        assert_truncate_state(&db, &lines);
    }
    eprintln!("truncate kill distribution (last marker kind): {kinds:?}");
}

// ---------------------------------------------------------------------------
// DROP TABLE
// ---------------------------------------------------------------------------

/// テーブルが完全に存在する状態（行・台帳・一意索引が揃っている）を確認する。
fn assert_table_fully_present(core: &EngineCore, k: u64, msg: &str) {
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    assert_eq!(
        count(core, &alice).expect("alice count"),
        ALICE_ROWS,
        "{msg}"
    );
    assert_eq!(count(core, &bob).expect("bob count"), BOB_ROWS, "{msg}");
    // 台帳が残っている（同一 operation_id の再送は重複）。
    let reseed = insert_sql("c", 1, ALICE_ROWS, &format!("seed-a-{k}"));
    assert_eq!(err(core, &alice, &reseed), "23505", "{msg}");
    // 一意索引が残っている（既存 code の別 id への投入は UNIQUE 違反）。
    assert_eq!(
        err(
            core,
            &alice,
            "INSERT INTO docs (id, code, embedding) VALUES (900001, 'c1', '[0.1,0.2]') \
             USING OPERATION_ID 'probe-dup'"
        ),
        "23505",
        "{msg}"
    );
}

/// テーブルが完全に消えた状態（再作成後に旧 operation_id・旧 UNIQUE 値を再利用できる）。
fn assert_table_fully_gone(core: &EngineCore, k: u64, msg: &str) {
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    assert_eq!(
        count(core, &alice).expect_err("table must be gone"),
        "42P01",
        "{msg}"
    );
    assert_eq!(
        count(core, &bob).expect_err("table must be gone"),
        "42P01",
        "{msg}"
    );
    ok(core, &alice, CREATE_DOCS);
    ok(
        core,
        &alice,
        &insert_sql("c", 1, ALICE_ROWS, &format!("seed-a-{k}")),
    );
    ok(
        core,
        &bob,
        &insert_sql("b", BOB_ID_BASE, BOB_ROWS, &format!("seed-b-{k}")),
    );
    assert_eq!(
        count(core, &alice).expect("alice count"),
        ALICE_ROWS,
        "{msg}"
    );
    assert_eq!(count(core, &bob).expect("bob count"), BOB_ROWS, "{msg}");
}

fn assert_drop_state(db: &Path, lines: &[String]) {
    let core = reopen(db);
    let (kind, k) = last_kind(lines);
    let msg = format!("last marker = {kind} {k}");
    let exists = count(&core, &ctx("alice")).is_ok();

    match kind.as_str() {
        "DROPPED" => assert!(!exists, "dropped table must stay gone: {msg}"),
        // 投入 commit 後・DROP 開始前。テーブルは必ず完全に存在する。
        "SEEDED" => {
            assert!(exists, "{msg}");
            assert_table_fully_present(&core, k, &msg);
            return;
        }
        // DROP は commit 済みか未 commit のどちらか（途中状態は失敗）。
        "BEGIN_DROP" => {}
        // CREATE／投入の途中。存在するなら行数は {0, M} で台帳・一意索引と整合。
        "BEGIN_CYCLE" => {
            if exists {
                let n = count(&core, &ctx("alice")).expect("alice count");
                assert!(n == 0 || n == ALICE_ROWS, "partial state n={n}: {msg}");
                // bob の投入は alice の後段の独立 commit。行数は {0, M'} で、alice が
                // 未投入なら bob も未投入（投入順の逆転・部分反映は失敗）。
                let nb = count(&core, &ctx("bob")).expect("bob count");
                assert!(
                    nb == 0 || nb == BOB_ROWS,
                    "partial bob state nb={nb}: {msg}"
                );
                if n == 0 {
                    assert_eq!(nb, 0, "bob seeded before alice: {msg}");
                }
                let reseed = insert_sql("c", 1, ALICE_ROWS, &format!("seed-a-{k}"));
                if n == ALICE_ROWS {
                    assert_eq!(err(&core, &ctx("alice"), &reseed), "23505", "{msg}");
                } else {
                    // 行が無いなら台帳も無い（再投入できる）。
                    ok(&core, &ctx("alice"), &reseed);
                }
                let reseed_bob = insert_sql("b", BOB_ID_BASE, BOB_ROWS, &format!("seed-b-{k}"));
                if nb == BOB_ROWS {
                    assert_eq!(err(&core, &ctx("bob"), &reseed_bob), "23505", "{msg}");
                } else {
                    ok(&core, &ctx("bob"), &reseed_bob);
                }
                return;
            }
        }
        other => panic!("unexpected marker {other}"),
    }

    if exists {
        assert_table_fully_present(&core, k, &msg);
    } else {
        assert_table_fully_gone(&core, k, &msg);
    }
}

#[test]
fn drop_table_is_atomic_under_sigkill() {
    const NAME: &str = "drop_table_is_atomic_under_sigkill";
    if let Some((role, db, log)) = child_env() {
        assert_eq!(role, "drop");
        child_drop_workload(&db, &log);
        return;
    }
    let mut kinds = std::collections::BTreeMap::<String, u32>::new();
    for round in 0..KILL_ROUNDS {
        let db = unique_db_path("t15-crash-drop");
        let _db_guard = CleanupGuard(db.clone());
        let log = db.with_extension("marks");
        let _ = std::fs::remove_file(&log);
        let lines = run_and_kill(
            NAME,
            "drop",
            &db,
            &log,
            DROP_TARGETS[round % DROP_TARGETS.len()],
            1 + (round % 3) as u64,
            jitter_for(round),
        );
        let _ = std::fs::remove_file(&log);
        *kinds.entry(last_kind(&lines).0).or_default() += 1;
        assert_drop_state(&db, &lines);
    }
    eprintln!("drop kill distribution (last marker kind): {kinds:?}");
}
