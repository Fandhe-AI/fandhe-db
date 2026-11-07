//! 稼働中バックアップ（`EngineCore::export_snapshot`）が並行書き込みの下でも単一 commit 点の
//! 内容になることの結合テスト（RECOVER-13・Issue #2153。ポインタ: `docs/spec/04-behavior/recovery.md`
//! の RECOVER-13、方式は `docs/design/backup-snapshot.md`）。
//!
//! 書き込みスレッドは 1 回の明示トランザクションで複数の表を同時に更新する（採番カウンタ・口座残高の
//! 付け替え・全口座の世代番号・親子表への 1 行ずつの追加）。途中の状態が書き出しに混ざれば、
//! 次の不変条件のどれかが必ず崩れる。期待値は commit 番号 K だけから独立に計算する（production の
//! 判定を使わない）。
//!
//! - カウンタ == K、全口座の世代 == K、親の id 集合 == 子の id 集合 == {1..K}
//! - 各口座の残高が K から決まる値と一致する（総和一定を含む）
//! - 子の親参照がすべて親に存在する（FK）、主キーに重複がない
//! - 取得前に commit 済みの書き込みは含まれ（K >= 取得前の commit 数）、取得の完了後に始まった
//!   書き込みは含まれない（K <= 取得後の開始数）
//!
//! 時間・sleep には依存しない。書き込みが実際に並行していること、スナップショット間で内容が進む
//! ことは、書き込みの進捗カウンタを待つ（yield のみ）ことで保証する。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const BALLAST_CHUNKS: i64 = 80;
const BALLAST_ROWS_PER_CHUNK: i64 = 64;
const ACCOUNTS: i64 = 8;
const INITIAL_BAL: i64 = 1000;
/// 取得回数。
const EXPORTS: usize = 6;
/// 書き込みスレッドの周回数の上限。SELECT の LIMIT 上限（10000）に収め、stop が立たない異常時にも止まる。
const MAX_ITERATIONS: u64 = 5_000;

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(core: &EngineCore, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

/// 指定列の整数セルを (主キー, 値) の組で返す。
fn pairs(core: &EngineCore, sql: &str) -> Vec<(i64, i64)> {
    match run(core, sql) {
        SqlOutcome::Query(q) => q
            .rows
            .iter()
            .map(|r| match (&r.cells[0], &r.cells[1]) {
                (Cell::SignedInteger(a), Cell::SignedInteger(b)) => (*a, *b),
                other => panic!("unexpected cells {other:?}"),
            })
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn seed(core: &EngineCore) {
    run(core, "CREATE TABLE ctr (id BIGINT PRIMARY KEY, n BIGINT)");
    run(
        core,
        "CREATE TABLE accts (id BIGINT PRIMARY KEY, bal BIGINT, gen BIGINT)",
    );
    run(
        core,
        "CREATE TABLE parents (pid BIGINT PRIMARY KEY, gen BIGINT)",
    );
    run(
        core,
        "CREATE TABLE children (cid BIGINT PRIMARY KEY, pid BIGINT REFERENCES parents, gen BIGINT)",
    );
    run(core, "INSERT INTO ctr (id, n) VALUES (1, 0)");
    // 書き出しに時間がかかるよう、検査対象外の表に行を積んでおく（書き込みと重なる窓を広げる）。
    run(core, "CREATE TABLE ballast (id BIGINT PRIMARY KEY, s TEXT)");
    for chunk in 0..BALLAST_CHUNKS {
        let values: Vec<String> = (0..BALLAST_ROWS_PER_CHUNK)
            .map(|i| {
                let id = chunk * BALLAST_ROWS_PER_CHUNK + i;
                format!("({id}, 'ballast-row-{id}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')")
            })
            .collect();
        run(
            core,
            &format!("INSERT INTO ballast (id, s) VALUES {}", values.join(", ")),
        );
    }
    let values: Vec<String> = (0..ACCOUNTS)
        .map(|i| format!("({i}, {INITIAL_BAL}, 0)"))
        .collect();
    run(
        core,
        &format!(
            "INSERT INTO accts (id, bal, gen) VALUES {}",
            values.join(", ")
        ),
    );
}

/// commit 番号 K の書き込みが行う更新（1 トランザクション）。
fn write_commit(core: &EngineCore, k: u64, started: &AtomicU64) -> Result<(), String> {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut step = |sql: &str| {
        core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, sql)
            .map(|_| ())
            .map_err(|e| format!("{sql}: {}", e.wire_code()))
    };
    step("BEGIN")?;
    step(&format!("UPDATE ctr SET n = {k} WHERE id = 1"))?;
    step(&format!(
        "UPDATE accts SET bal = bal - 1 WHERE id = {}",
        k % ACCOUNTS as u64
    ))?;
    step(&format!(
        "UPDATE accts SET bal = bal + 1 WHERE id = {}",
        (k + 1) % ACCOUNTS as u64
    ))?;
    step(&format!("UPDATE accts SET gen = {k}"))?;
    step(&format!("INSERT INTO parents (pid, gen) VALUES ({k}, {k})"))?;
    step(&format!(
        "INSERT INTO children (cid, pid, gen) VALUES ({k}, {k}, {k})"
    ))?;
    // この store の後に始まる書き込みだけが「取得より後」になりうる。COMMIT 直前に公開する。
    started.store(k, Ordering::SeqCst);
    step("COMMIT")
}

/// commit 番号 K の時点の口座 `id` の残高（独立オラクル）。
fn expected_bal(id: i64, k: i64) -> i64 {
    let a = ACCOUNTS;
    let out = (1..=k).filter(|j| j % a == id).count() as i64;
    let inn = (1..=k).filter(|j| (j + 1) % a == id).count() as i64;
    INITIAL_BAL - out + inn
}

/// 書き出されたファイルを開いて不変条件を検査し、含まれていた commit 番号 K を返す。
fn verify_snapshot(path: &std::path::Path) -> u64 {
    let copy = open_core(path);
    let ctr = pairs(&copy, "SELECT id, n FROM ctr LIMIT 10");
    assert_eq!(ctr.len(), 1, "counter row must exist exactly once");
    let k = ctr[0].1;
    assert!(k >= 0);

    let accts = pairs(&copy, "SELECT id, bal FROM accts LIMIT 100");
    assert_eq!(accts.len() as i64, ACCOUNTS);
    let ids: BTreeSet<i64> = accts.iter().map(|(i, _)| *i).collect();
    assert_eq!(ids.len() as i64, ACCOUNTS, "primary key must be unique");
    for (id, bal) in &accts {
        assert_eq!(*bal, expected_bal(*id, k), "balance of {id} at commit {k}");
    }
    assert_eq!(
        accts.iter().map(|(_, b)| b).sum::<i64>(),
        ACCOUNTS * INITIAL_BAL
    );
    for (id, gen) in pairs(&copy, "SELECT id, gen FROM accts LIMIT 100") {
        assert_eq!(gen, k, "generation of account {id}");
    }

    let parents = pairs(&copy, "SELECT pid, gen FROM parents LIMIT 10000");
    let children = pairs(&copy, "SELECT cid, pid FROM children LIMIT 10000");
    let want: BTreeSet<i64> = (1..=k).collect();
    let parent_ids: BTreeSet<i64> = parents.iter().map(|(p, _)| *p).collect();
    let child_ids: BTreeSet<i64> = children.iter().map(|(c, _)| *c).collect();
    assert_eq!(parents.len() as i64, k, "parent row count");
    assert_eq!(children.len() as i64, k, "child row count");
    assert_eq!(parent_ids, want);
    assert_eq!(child_ids, want);
    for (cid, pid) in &children {
        assert!(parent_ids.contains(pid), "child {cid} has no parent {pid}");
        assert_eq!(cid, pid);
    }
    for (pid, gen) in &parents {
        assert_eq!(pid, gen);
    }

    let ballast = pairs(&copy, "SELECT id, id FROM ballast LIMIT 10000");
    assert_eq!(
        ballast.len() as i64,
        BALLAST_CHUNKS * BALLAST_ROWS_PER_CHUNK
    );

    // 制約そのものが出力に残っている（書き出し後の書き込みでも主キー・FK が効く）。
    let mut session = SessionState::default();
    session.allow_ddl();
    let code = |sql: &str, s: &mut SessionState| {
        copy.execute_sql_in_session(&ctx(), s, sql)
            .expect_err("must be rejected")
            .wire_code()
    };
    assert_eq!(
        code("INSERT INTO ctr (id, n) VALUES (1, 0)", &mut session),
        "23505"
    );
    assert_eq!(
        code(
            "INSERT INTO children (cid, pid, gen) VALUES (999999999, 999999998, 0)",
            &mut session
        ),
        "23503"
    );
    u64::try_from(k).expect("non-negative")
}

#[test]
fn snapshot_under_concurrent_writes_is_a_single_commit_point() {
    let path = unique_db_path("recover13-concurrent");
    let _g = CleanupGuard(path.clone());
    let core = Arc::new(open_core(&path));
    seed(&core);

    let started = Arc::new(AtomicU64::new(0));
    let committed = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let writer_failed = Arc::new(AtomicBool::new(false));

    let writer = {
        let (core, started, committed, stop, failed) = (
            Arc::clone(&core),
            Arc::clone(&started),
            Arc::clone(&committed),
            Arc::clone(&stop),
            Arc::clone(&writer_failed),
        );
        std::thread::spawn(move || {
            let mut errors = Vec::new();
            for k in 1..=MAX_ITERATIONS {
                if let Err(e) = write_commit(&core, k, &started) {
                    errors.push(format!("commit {k}: {e}"));
                    failed.store(true, Ordering::SeqCst);
                    break;
                }
                committed.store(k, Ordering::SeqCst);
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
            errors
        })
    };

    let mut seen: Vec<u64> = Vec::new();
    let mut outputs = Vec::new();
    for i in 0..EXPORTS {
        // 書き込みが 1 件以上進むまで待つ（i 回目の取得の前に必ず前進させ、取得間で内容が進むことを保証）。
        let floor = committed
            .load(Ordering::SeqCst)
            .max(seen.last().copied().unwrap_or(0));
        while committed.load(Ordering::SeqCst) <= floor
            && !writer_failed.load(Ordering::SeqCst)
            && floor < MAX_ITERATIONS
        {
            std::thread::yield_now();
        }
        assert!(
            !writer_failed.load(Ordering::SeqCst) && committed.load(Ordering::SeqCst) > floor,
            "writer must keep committing during exports"
        );
        let before_committed = committed.load(Ordering::SeqCst);
        let out = unique_db_path(&format!("recover13-concurrent-out{i}"));
        outputs.push(CleanupGuard(out.clone()));
        // 取得ファイルが出力先として未存在であること。
        let _ = std::fs::remove_file(&out);
        core.export_snapshot(&out).expect("export snapshot");
        let after_started = started.load(Ordering::SeqCst);

        let k = verify_snapshot(&out);
        assert!(
            k >= before_committed,
            "commit {before_committed} finished before export started but snapshot has {k}"
        );
        assert!(
            k <= after_started,
            "snapshot has commit {k} that started after export finished ({after_started})"
        );
        seen.push(k);
    }
    stop.store(true, Ordering::SeqCst);
    let errors = writer.join().expect("writer thread");
    assert!(errors.is_empty(), "writer errors: {errors:?}");

    // 書き込みは実際に並行して進み、スナップショットは単調に新しくなる。
    assert!(committed.load(Ordering::SeqCst) >= EXPORTS as u64);
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "progress: {seen:?}");
    assert!(seen[0] >= 1, "first snapshot must include writer commits");
}
