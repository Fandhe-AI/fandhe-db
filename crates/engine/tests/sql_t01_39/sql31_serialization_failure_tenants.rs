//! writer 取得時の `40001` とテナント境界の非漏えい（Issue #2407。親 #2396・TASK-333）の
//! 公開 API 結合テスト。ポインタ: ERR-7・SQL-38・TABLE-3（設計記録は
//! `docs/design/explicit-transaction.md` 1a-6・1a-7・1a-11）。
//!
//! 役割: `40001` が「他テナントの不可視な行の有無・件数・commit 時点」を観測させないこと
//! （3 テナントの差分比較・役割の巡回・並行実行）と、`40001` の後に副作用が残らないこと（ERR-7）を
//! 固定する。判定そのもの・2 テナントの順次ケースは `sql31_serialization_failure`、1 テナントの
//! 並行ケースは `sql31_serialization_failure_concurrent` が担う。Public 行・内部状態（世代・台帳）
//! の観測は crate 内の `core::serialization_failure_tenant_tests` に置く。
//!
//! 方針: 差分比較は「読み手の観測列が、他テナントの活動によらず等しい」ことを `assert_eq!` で
//! 確かめる。陽性の対照（本当に `40001` になる世界）を併置し、判定が働いていることも確かめる。
//! 並行は `mpsc`・`Barrier` だけで順序を制御し、sleep は使わない。スレッド数・行数・ラウンド数は
//! 定数で上限化する。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Barrier};
use std::thread;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility, MAX_WRITE_LOCK_WAIT_MS};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
/// 他テナントが一度に入れる行数の上限（多数 commit の世界）。
const MANY_ROWS: u64 = 16;
/// 並行ケースの読み手のラウンド数（上限）。
const ROUNDS: u64 = 8;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let wait = validate_write_lock_wait_ms(MAX_WRITE_LOCK_WAIT_MS).expect("valid wait");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_write_lock_wait(wait);
    for t in ["t1", "t2"] {
        auto_ddl(&core, "sys", &format!("CREATE TABLE {t} (n BIGINT)"));
    }
    (core, guard)
}

fn auto(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn auto_ddl(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

/// 自テナントから見える `table` の行 id 列（autocommit で観測する）。
fn visible_ids(core: &EngineCore, tenant: &str, table: &str) -> Vec<u64> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session_legacy(
            &ctx(tenant),
            &mut session,
            &format!("SELECT n FROM {table}"),
        )
        .expect("select")
    {
        SqlOutcome::Query(q) => {
            let mut ids: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
            ids.sort_unstable();
            ids
        }
        other => panic!("unexpected {other:?}"),
    }
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
    /// 呼び出したスレッドの中だけで使う（writer gate は `ThreadId` で保持者を見る）。
    fn begin(core: &'e EngineCore, tenant: &str) -> Self {
        let mut t = Self {
            core,
            ctx: ctx(tenant),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        };
        t.run("BEGIN").expect("begin");
        t
    }

    fn run(&mut self, sql: &str) -> Result<(), String> {
        self.exec(sql).map(|_| ())
    }

    /// 成功なら返った行 id 列（SELECT 以外は空）、失敗なら `"{wire_code}|{msg}"`。
    fn exec(&mut self, sql: &str) -> Result<Vec<u64>, String> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
            .map(|o| match o {
                SqlOutcome::Query(q) => {
                    let mut ids: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
                    ids.sort_unstable();
                    ids
                }
                _ => Vec::new(),
            })
            .map_err(|e| format!("{}|{e}", e.wire_code()))
    }

    /// `40001` を期待する。文言にテナント ID・表名を含めず、以降は `25P02`、`ROLLBACK` で `Idle`。
    fn expect_40001_then_recover(&mut self, sql: &str) {
        let err = self.run(sql).expect_err("40001");
        self.assert_failed_then_recover(&err);
    }

    fn assert_failed_then_recover(&mut self, err: &str) {
        assert!(err.starts_with("40001|"), "{err}");
        assert_no_identifiers(err);
        assert_eq!(self.txn.status(), TransactionStatus::Failed);
        let e2 = self.run("SELECT 1").expect_err("25P02");
        assert!(e2.starts_with("25P02|"), "{e2}");
        self.run("ROLLBACK").expect("rollback");
        assert_eq!(self.txn.status(), TransactionStatus::Idle);
    }
}

/// エラー文に 3 テナントの ID も表名も含まれない。
fn assert_no_identifiers(err: &str) {
    for needle in ["tenant", "t1", "t2"] {
        assert!(!err.contains(needle), "leaks `{needle}`: {err}");
    }
}

/// 読み手の各文の観測（結果の種別・返った行 id 列・Tx 状態）。
type Trace = Vec<(String, TransactionStatus)>;

fn observe(t: &mut Tx<'_>, sql: &str, trace: &mut Trace) {
    let r = match t.exec(sql) {
        Ok(ids) => format!("OK{ids:?}"),
        Err(e) => e.split('|').next().unwrap_or("").to_string(),
    };
    trace.push((r, t.txn.status()));
}

/// 他テナントの活動（実行する時点、所有者の役割 0=W1 1=W2、SQL）。
type Act = (usize, usize, String);

fn ins(phase: usize, role: usize, base: u64, n: u64) -> Vec<Act> {
    (0..n)
        .map(|k| {
            (
                phase,
                role,
                format!("INSERT INTO t1 (id, n) VALUES ({}, 1)", base + k),
            )
        })
        .collect()
}

/// 世界（他テナントの活動の列）。`needs_pre` は事前データの存在が前提のもの。
struct World {
    name: &'static str,
    needs_pre: bool,
    acts: Vec<Act>,
}

fn worlds(phase: usize) -> Vec<World> {
    let w = |name, needs_pre, acts| World {
        name,
        needs_pre,
        acts,
    };
    let mut both = ins(phase, 0, 3000, 1);
    both.extend(ins(phase, 1, 4000, 3));
    vec![
        w("baseline", false, vec![]),
        w("w1 insert 1", false, ins(phase, 0, 3000, 1)),
        w("w1 insert many", false, ins(phase, 0, 3000, MANY_ROWS)),
        w("w1 and w2 insert", false, both),
        w(
            "w1 update own row",
            true,
            vec![(phase, 0, "UPDATE t1 SET n = 9 WHERE id = 1000".into())],
        ),
        w(
            "w1 delete own row",
            true,
            vec![(phase, 0, "DELETE FROM t1 WHERE id = 1000".into())],
        ),
        w(
            "w1 zero-row update",
            false,
            vec![(phase, 0, "UPDATE t1 SET n = 9 WHERE id = 99999".into())],
        ),
        w(
            "w1 truncate",
            true,
            vec![(phase, 0, "TRUNCATE TABLE t1".into())],
        ),
    ]
}

/// 読み手 `reader` の台本を流して観測列を返す。他テナントの活動は `acts`（時点 0=最初の SELECT 前、
/// 1=SELECT の間、2=書き込み直前）。事前データ `pre` が真なら BEGIN 前に W1・W2 の行を入れる。
fn run_world(label: &str, reader: usize, pre: bool, acts: &[Act]) -> Trace {
    let (core, _g) = new_core(label);
    let owners = [TENANTS[(reader + 1) % 3], TENANTS[(reader + 2) % 3]];
    auto(
        &core,
        TENANTS[reader],
        "INSERT INTO t1 (id, n) VALUES (1, 1)",
    );
    if pre {
        auto(&core, owners[0], "INSERT INTO t1 (id, n) VALUES (1000, 1)");
        auto(&core, owners[1], "INSERT INTO t1 (id, n) VALUES (2000, 1)");
    }
    let mut r = Tx::begin(&core, TENANTS[reader]);
    let mut trace = Trace::new();
    let fire = |phase: usize| {
        for (p, role, sql) in acts {
            if *p == phase {
                auto_ddl(&core, owners[*role], sql);
            }
        }
    };
    fire(0);
    observe(&mut r, "SELECT n FROM t1", &mut trace);
    fire(1);
    observe(&mut r, "SELECT n FROM t1", &mut trace);
    fire(2);
    observe(&mut r, "INSERT INTO t2 (id, n) VALUES (7, 7)", &mut trace);
    observe(&mut r, "COMMIT", &mut trace);
    trace
}

/// 他テナントの活動（有無・件数・種類・時点）が読み手の観測列を変えない。3 テナントが順に読み手になる。
#[test]
fn other_tenants_activity_does_not_change_reader_observation() {
    for reader in 0..TENANTS.len() {
        for pre in [false, true] {
            for phase in 0..3 {
                let base = run_world(
                    &format!("sft-base-{reader}-{pre}-{phase}"),
                    reader,
                    pre,
                    &[],
                );
                assert!(
                    base.iter().all(|(r, _)| r.starts_with("OK")),
                    "baseline must not fail: {base:?}"
                );
                for w in worlds(phase) {
                    if w.needs_pre && !pre {
                        continue;
                    }
                    let got = run_world(
                        &format!("sft-w-{reader}-{pre}-{phase}"),
                        reader,
                        pre,
                        &w.acts,
                    );
                    assert_eq!(
                        got, base,
                        "reader={reader} pre={pre} phase={phase} world={}",
                        w.name
                    );
                }
            }
        }
    }
}

/// 陽性の対照: 読み手自身の別セッションの commit・共有カタログの DDL は原因になる（判定が働いている）。
#[test]
fn positive_control_own_commit_and_ddl_cause_40001() {
    for (reader, me) in TENANTS.into_iter().enumerate() {
        let (core, _g) = new_core(&format!("sft-pos-{reader}"));
        let mut r = Tx::begin(&core, me);
        r.run("SELECT n FROM t1").expect("read");
        auto(&core, me, "INSERT INTO t1 (id, n) VALUES (1, 1)");
        r.expect_40001_then_recover("INSERT INTO t2 (id, n) VALUES (7, 7)");

        let mut readers: Vec<Tx<'_>> = TENANTS.iter().map(|t| Tx::begin(&core, t)).collect();
        for t in readers.iter_mut() {
            t.run("SELECT n FROM t1").expect("read");
        }
        auto_ddl(&core, "sys", "ALTER TABLE t1 ADD COLUMN extra BIGINT");
        for t in readers.iter_mut() {
            t.expect_40001_then_recover("INSERT INTO t2 (id, n) VALUES (7, 7)");
        }
    }
}

/// 並行: 他テナントが writer を保持・競合している間も、読み手は `40001` にならず commit できる。
/// writer は読み手が書き込みを始める直前の合図（`go`）まで commit しないため、読み手の `BEGIN`
/// 以降に他テナントの commit が必ず入り、読み手の writer 取得はその commit と交差する。
#[test]
fn other_tenants_concurrent_writers_do_not_cause_40001() {
    for reader in 0..TENANTS.len() {
        let (core, _g) = new_core(&format!("sft-conc-{reader}"));
        let owners = [TENANTS[(reader + 1) % 3], TENANTS[(reader + 2) % 3]];
        let mut r = Tx::begin(&core, TENANTS[reader]);
        r.run("SELECT n FROM t1").expect("read");
        thread::scope(|s| {
            let (held_tx, held_rx) = mpsc::channel();
            let mut gos = Vec::new();
            let hs: Vec<_> = owners
                .iter()
                .enumerate()
                .map(|(i, owner)| {
                    let (go_tx, go_rx) = mpsc::channel::<()>();
                    gos.push(go_tx);
                    let (core, held_tx) = (&core, held_tx.clone());
                    s.spawn(move || {
                        let mut b = Tx::begin(core, owner);
                        b.run(&format!("INSERT INTO t1 (id, n) VALUES ({}, 1)", 100 + i))
                            .expect("holder write");
                        held_tx.send(()).expect("signal");
                        go_rx.recv().expect("go");
                        b.run("COMMIT").expect("holder commit");
                    })
                })
                .collect();
            drop(held_tx);
            // 先に writer を取った 1 本が保持している状態で、読み手が書き込みへ進む。
            held_rx.recv().expect("a holder has writer");
            for go in &gos {
                go.send(()).expect("go");
            }
            r.run("INSERT INTO t2 (id, n) VALUES (7, 7)")
                .expect("invisible commits are not a cause");
            r.run("COMMIT").expect("commit");
            for h in hs {
                h.join().expect("join");
            }
        });
    }
}

/// 並行 autocommit writer（他の 2 テナント）が書き続けても、読み手の各ラウンドは `40001` にならない。
/// 読み手は各ラウンドで `SELECT` の後、他テナントの commit が 1 件以上進むのを待ってから書き込む
/// （`BEGIN` 以降の commit と writer 取得の交差を各ラウンドで保証する。sleep は使わない）。
#[test]
fn other_tenants_autocommit_storm_never_causes_40001() {
    /// writer 1 本あたりの書き込み回数の上限（読み手が終われば早く止まる）。
    const STORM_CAP: u64 = 4096;
    let (core, _g) = new_core("sft-storm");
    let barrier = Barrier::new(3);
    let commits = AtomicU64::new(0);
    let reader_done = AtomicBool::new(false);
    let live_writers = AtomicU64::new(2);
    let crossed = AtomicU64::new(0);
    thread::scope(|s| {
        for (i, owner) in [TENANTS[1], TENANTS[2]].into_iter().enumerate() {
            let (core, barrier, commits, done, live) =
                (&core, &barrier, &commits, &reader_done, &live_writers);
            s.spawn(move || {
                barrier.wait();
                for k in 0..STORM_CAP {
                    if done.load(Ordering::SeqCst) {
                        break;
                    }
                    auto(
                        core,
                        owner,
                        &format!(
                            "INSERT INTO t1 (id, n) VALUES ({}, 1)",
                            10_000 * (i as u64 + 1) + k
                        ),
                    );
                    commits.fetch_add(1, Ordering::SeqCst);
                }
                live.fetch_sub(1, Ordering::SeqCst);
            });
        }
        let (core, barrier, commits, done, live, crossed) = (
            &core,
            &barrier,
            &commits,
            &reader_done,
            &live_writers,
            &crossed,
        );
        s.spawn(move || {
            barrier.wait();
            for round in 0..ROUNDS {
                let mut r = Tx::begin(core, TENANTS[0]);
                r.run("SELECT n FROM t1").expect("read");
                let seen = commits.load(Ordering::SeqCst);
                while commits.load(Ordering::SeqCst) == seen && live.load(Ordering::SeqCst) > 0 {
                    thread::yield_now();
                }
                if commits.load(Ordering::SeqCst) > seen {
                    crossed.fetch_add(1, Ordering::SeqCst);
                }
                r.run(&format!("INSERT INTO t2 (id, n) VALUES ({round}, 1)"))
                    .expect("never 40001 from invisible commits");
                r.run("COMMIT").expect("commit");
            }
            done.store(true, Ordering::SeqCst);
        });
    });
    assert!(
        crossed.load(Ordering::SeqCst) >= 1,
        "at least one round must cross another tenant's commit"
    );
}

/// ERR-7: `40001` の後に副作用が残らない（行）。観測は 3 テナントの文脈から行い、再試行は成功する。
#[test]
fn no_side_effects_remain_after_40001() {
    let stmts = [
        "INSERT INTO t2 (id, n) VALUES (50, 5)",
        "UPDATE t2 SET n = 99 WHERE id = 1",
        "DELETE FROM t2 WHERE id = 1",
        "TRUNCATE TABLE t2",
    ];
    for stmt in stmts {
        let (core, _g) = new_core("sft-side");
        for t in TENANTS {
            auto(&core, t, "INSERT INTO t2 (id, n) VALUES (1, 1)");
        }
        let snapshot = |core: &EngineCore| -> Vec<Vec<u64>> {
            TENANTS.iter().map(|t| visible_ids(core, t, "t2")).collect()
        };
        let before = snapshot(&core);
        let mut r = Tx::begin(&core, TENANTS[0]);
        r.run("SELECT n FROM t1").expect("read");
        auto(&core, TENANTS[0], "INSERT INTO t1 (id, n) VALUES (1, 1)");
        r.expect_40001_then_recover(stmt);
        assert_eq!(before, snapshot(&core), "{stmt}");
        let mut s = SessionState::default();
        let out = core
            .execute_sql_in_session_legacy(
                &ctx(TENANTS[0]),
                &mut s,
                "SELECT n FROM t2 WHERE n = 99",
            )
            .expect("select");
        match out {
            SqlOutcome::Query(q) => assert!(q.rows.is_empty(), "{stmt}"),
            other => panic!("unexpected {other:?}"),
        }
        let mut r = Tx::begin(&core, TENANTS[0]);
        r.run("SELECT n FROM t1").expect("read");
        r.run(stmt).expect("retry from the start");
        r.run("COMMIT").expect("commit");
    }
}

/// ERR-7: `40001` になった `nextval` 付き INSERT は採番状態を進めない（次の採番が 1 のまま）。
#[test]
fn nextval_does_not_advance_after_40001() {
    let (core, _g) = new_core("sft-seq");
    auto_ddl(&core, "sys", "CREATE SEQUENCE s");
    let mut r = Tx::begin(&core, TENANTS[0]);
    r.run("SELECT n FROM t1").expect("read");
    auto(&core, TENANTS[0], "INSERT INTO t1 (id, n) VALUES (1, 1)");
    r.expect_40001_then_recover(
        "INSERT INTO t2 (id, n) VALUES (7, nextval('s')) USING OPERATION_ID 'sft-op-1'",
    );
    // 失敗したのと同じテナントで同じ operation_id を再送する。台帳に残っていれば再送扱いで
    // 採番も行も増えず、残っていなければ新規実行になる。どちらでも次の採番は 1 から始まる。
    auto(
        &core,
        TENANTS[0],
        "INSERT INTO t2 (id, n) VALUES (7, nextval('s')) USING OPERATION_ID 'sft-op-1'",
    );
    let mut s = SessionState::default();
    let out = core
        .execute_sql_in_session_legacy(&ctx(TENANTS[0]), &mut s, "SELECT n FROM t2")
        .expect("select");
    let SqlOutcome::Query(q) = out else {
        panic!("unexpected {out:?}");
    };
    let ns: Vec<&Cell> = q.rows.iter().map(|r| &r.cells[0]).collect();
    assert_eq!(ns.len(), 1, "exactly one row after retry: {ns:?}");
    assert!(
        matches!(ns[0], Cell::SignedInteger(1)),
        "must start at 1: {:?}",
        ns[0]
    );
}
