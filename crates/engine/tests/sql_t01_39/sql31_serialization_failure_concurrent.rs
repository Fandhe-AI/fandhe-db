//! writer 取得時の `40001` 判定（Issue #2406。親 #2396・TASK-333）の並行セッション結合テスト。
//! ポインタ: SQL-31・SQL-38（設計記録は `docs/design/explicit-transaction.md` 1a-3・1a-6・1a-11）。
//!
//! 既存の `sql31_serialization_failure` は 2 セッションを同一スレッドで順に操作する。本モジュールは
//! 別スレッドのセッションが writer を実際に保持・競合している状態でも、(1) 読んでから書く形は
//! `40001`、(2) 無関係な表・読まずに書く形・読み取りだけの形は `40001` にならない、ことを確かめる。
//!
//! 方針: 順序制御は `mpsc` と `Barrier` だけで行い、sleep・実時間の閾値は使わない。どの順序で
//! 競合しても結果が決定的になる構成にし、「実際に待ったか」は確かめない（それには
//! `test-sync-points` が要り束ねられない）。待機上限は 60 s に設定し、許容する `wire_code` は
//! 期待値だけ（`55P03` などは fail-closed でテスト失敗）。スレッド数・ラウンド数は定数で上限化する。
//! 3 テナントでの非漏えい（#2407）・待機上限と `55P03`（#2408）は対象外。

use std::sync::{mpsc, Barrier};
use std::thread;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility, MAX_WRITE_LOCK_WAIT_MS};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

/// 並行 writer の本数（上限）。
const WRITERS: usize = 4;
/// write skew を繰り返すラウンド数（上限）。
const ROUNDS: usize = 8;
const TENANT: &str = "tenant-a";

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
    for t in ["sf1", "sf2"] {
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session_legacy(
            &ctx("sys"),
            &mut session,
            &format!("CREATE TABLE {t} (n BIGINT)"),
        )
        .expect("create table");
    }
    (core, guard)
}

fn auto(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session_legacy(&ctx(TENANT), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
    /// 呼び出したスレッドの中だけで使う（writer gate は `ThreadId` で保持者を見る）。
    fn begin(core: &'e EngineCore) -> Self {
        let mut t = Self {
            core,
            ctx: ctx(TENANT),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        };
        t.run("BEGIN").expect("begin");
        t
    }

    fn run(&mut self, sql: &str) -> Result<(), String> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
            .map(|_| ())
            .map_err(|e| format!("{}|{e}", e.wire_code()))
    }

    /// `40001` を期待する。以降は `25P02`、`ROLLBACK` で `Idle` へ戻る。
    fn expect_40001_then_recover(&mut self, sql: &str) {
        let err = self.run(sql).expect_err("40001");
        self.assert_failed_then_recover(&err);
    }

    fn assert_failed_then_recover(&mut self, err: &str) {
        assert!(err.starts_with("40001|"), "{err}");
        assert!(!err.contains("tenant") && !err.contains("sf1") && !err.contains("sf2"));
        assert_eq!(self.txn.status(), TransactionStatus::Failed);
        let e2 = self.run("SELECT 1").expect_err("25P02");
        assert!(e2.starts_with("25P02|"), "{e2}");
        self.run("ROLLBACK").expect("rollback");
        assert_eq!(self.txn.status(), TransactionStatus::Idle);
    }
}

/// 別スレッドで明示 Tx を開始して `table` へ書き込み（writer 取得）、`held` を送ってから commit する。
fn spawn_holder<'scope, 'env>(
    s: &'scope thread::Scope<'scope, 'env>,
    core: &'env EngineCore,
    table: &'static str,
    id: usize,
    held: mpsc::Sender<()>,
) -> thread::ScopedJoinHandle<'scope, ()> {
    s.spawn(move || {
        let mut b = Tx::begin(core);
        b.run(&format!("INSERT INTO {table} (id, n) VALUES ({id}, 1)"))
            .expect("holder write");
        held.send(()).expect("signal");
        b.run("COMMIT").expect("holder commit");
    })
}

#[test]
fn read_then_write_fails_with_40001_while_other_session_holds_writer() {
    let (core, _g) = new_core("sf-c-hold");
    let mut a = Tx::begin(&core);
    a.run("SELECT n FROM sf1").expect("read");
    thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        let h = spawn_holder(s, &core, "sf1", 1, tx);
        rx.recv().expect("holder has writer");
        a.expect_40001_then_recover("INSERT INTO sf2 (id, n) VALUES (2, 2)");
        h.join().expect("join");
    });
    // 書き込みは残らず（同じ id で入る）、再試行は成功する。
    let mut a = Tx::begin(&core);
    a.run("SELECT n FROM sf1").expect("read");
    a.run("INSERT INTO sf2 (id, n) VALUES (2, 2)")
        .expect("retry");
    a.run("COMMIT").expect("commit");
}

#[test]
fn read_then_write_fails_with_40001_after_concurrent_autocommit_writers() {
    let (core, _g) = new_core("sf-c-auto");
    let mut a = Tx::begin(&core);
    a.run("SELECT n FROM sf1").expect("read");
    let barrier = Barrier::new(WRITERS);
    thread::scope(|s| {
        for k in 0..WRITERS {
            let (core, barrier) = (&core, &barrier);
            s.spawn(move || {
                barrier.wait();
                auto(core, &format!("INSERT INTO sf1 (id, n) VALUES ({k}, 1)"));
            });
        }
    });
    a.expect_40001_then_recover("INSERT INTO sf2 (id, n) VALUES (100, 1)");
}

/// 2 本の明示 Tx が互いに読んだ表へ書く write skew は、ちょうど 1 本だけが `40001` になる。
fn write_skew_round(core: &EngineCore, sides: [(&str, &str); 2], id: usize) {
    let barrier = Barrier::new(2);
    let results: Vec<Result<(), String>> = thread::scope(|s| {
        let handles: Vec<_> = sides
            .iter()
            .enumerate()
            .map(|(i, &(read, write))| {
                let barrier = &barrier;
                s.spawn(move || -> Result<(), String> {
                    let mut t = Tx::begin(core);
                    t.run(&format!("SELECT n FROM {read}")).expect("read");
                    barrier.wait();
                    match t.run(&format!(
                        "INSERT INTO {write} (id, n) VALUES ({}, 1)",
                        id + i
                    )) {
                        Ok(()) => t.run("COMMIT"),
                        Err(e) => {
                            t.assert_failed_then_recover(&e);
                            Err(e)
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "exactly one side commits: {results:?}");
    let failed = results.iter().find_map(|r| r.as_ref().err()).expect("one");
    assert!(failed.starts_with("40001|"), "{failed}");
}

#[test]
fn write_skew_between_two_explicit_txns_yields_exactly_one_40001() {
    let (core, _g) = new_core("sf-c-skew");
    for r in 0..ROUNDS {
        // 別の表を読んで互いの表へ書く形。
        write_skew_round(&core, [("sf1", "sf2"), ("sf2", "sf1")], r * 100);
        // 同じ表を読んで同じ表へ書く形。
        write_skew_round(&core, [("sf1", "sf1"), ("sf1", "sf1")], r * 100 + 10);
    }
}

#[test]
fn unrelated_table_commit_under_contention_does_not_cause_40001() {
    let (core, _g) = new_core("sf-c-unrel");
    let mut a = Tx::begin(&core);
    a.run("SELECT n FROM sf1").expect("read");
    thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        let h = spawn_holder(s, &core, "sf2", 1, tx);
        rx.recv().expect("holder has writer");
        // sf2 への commit は A が読んだ sf1 とは無関係。gate の解放後に成功する。
        a.run("INSERT INTO sf2 (id, n) VALUES (2, 2)")
            .expect("unrelated");
        a.run("COMMIT").expect("commit");
        h.join().expect("join");
    });
}

#[test]
fn write_without_prior_read_succeeds_despite_concurrent_commits() {
    let (core, _g) = new_core("sf-c-wonly");
    let mut a = Tx::begin(&core);
    thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        let h = spawn_holder(s, &core, "sf1", 1, tx);
        rx.recv().expect("holder has writer");
        a.run("INSERT INTO sf1 (id, n) VALUES (2, 2)")
            .expect("write only");
        a.run("COMMIT").expect("commit");
        h.join().expect("join");
    });
    // 読まずに書く明示 Tx が並行しても、gate で直列化されて全員成功する。
    let barrier = Barrier::new(WRITERS);
    thread::scope(|s| {
        for k in 0..WRITERS {
            let (core, barrier) = (&core, &barrier);
            s.spawn(move || {
                let mut t = Tx::begin(core);
                barrier.wait();
                t.run(&format!("INSERT INTO sf1 (id, n) VALUES ({}, 1)", 100 + k))
                    .expect("write");
                t.run("COMMIT").expect("commit");
            });
        }
    });
}

#[test]
fn read_only_txns_commit_while_concurrent_writers_commit() {
    let (core, _g) = new_core("sf-c-ro");
    let barrier = Barrier::new(WRITERS * 2);
    thread::scope(|s| {
        for k in 0..WRITERS {
            let (core, barrier) = (&core, &barrier);
            s.spawn(move || {
                let mut t = Tx::begin(core);
                t.run("SELECT n FROM sf1").expect("read");
                barrier.wait();
                t.run("COMMIT").expect("read only commit");
            });
            s.spawn(move || {
                barrier.wait();
                auto(core, &format!("INSERT INTO sf1 (id, n) VALUES ({k}, 1)"));
            });
        }
    });
}
