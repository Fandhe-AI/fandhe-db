//! SQL-38 (1)(2)・TABLE-3・PERSIST-4 のポインタ・Issue #1940: 要求された分離レベルによらず、
//! 実行は常に単一ライタの直列化（SERIALIZABLE 相当）になることを 2 セッションの並行操作で固定する
//! 結合テスト（前提の writer 取得時 `40001` 判定は #2396・TASK-333。設計記録は
//! `docs/design/explicit-transaction.md` の「分離レベルの受理方針」節と 1a 節）。
//!
//! 分担: 単一スレッドでの受理は `sql38_begin_isolation_level`、`40001` の網羅は
//! `sql31_serialization_failure*`、READ ONLY は `sql38_read_only_concurrency`。本モジュールは
//! 「弱いレベルを要求しても保証が弱まらない」「要求レベルが記録どおりで実際より強い保証を装わない」
//! ことを、レベル別・レベル混在で確かめる。`SET TRANSACTION` 経路は `sql38_set_transaction.rs`（#1939）、`SHOW` は対象外。
//!
//! 方針: 順序制御は `mpsc` と `Barrier` だけで行い、sleep・実時間の閾値は使わない。許容する
//! `wire_code` は期待値（`40001`）だけで、`55P03` などは fail-closed でテスト失敗。スレッド数・
//! ラウンド数は定数で上限化する。書き込み後の読み取りは取得時点のスナップショットへ切り替わる
//! ため、書き込み前の読み取りだけを主張する。

use std::sync::{mpsc, Barrier};
use std::thread;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{RequestedIsolationLevel, SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility, MAX_WRITE_LOCK_WAIT_MS};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

const WRITERS: usize = 4;
const ROUNDS: usize = 3;
const TENANT: &str = "tenant-a";

type Level = (&'static str, RequestedIsolationLevel);

const LEVELS: [Level; 4] = [
    ("SERIALIZABLE", RequestedIsolationLevel::Serializable),
    ("REPEATABLE READ", RequestedIsolationLevel::RepeatableRead),
    ("READ COMMITTED", RequestedIsolationLevel::ReadCommitted),
    ("READ UNCOMMITTED", RequestedIsolationLevel::ReadUncommitted),
];

/// 修飾子付きで受理される開始文の形（`START TRANSACTION` への修飾子は現状 42601 のため含めない）。
const HEADS: [&str; 2] = ["BEGIN", "BEGIN TRANSACTION"];

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
    for t in ["iso1", "iso2"] {
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
    /// `<head> ISOLATION LEVEL <レベル>` で開始し、記録されるレベルが要求どおりであることを確かめる。
    /// 呼び出したスレッドの中だけで使う（writer gate は `ThreadId` で保持者を見る）。
    fn begin_with(core: &'e EngineCore, head: &str, level: Level) -> Self {
        let mut t = Self {
            core,
            ctx: ctx(TENANT),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        };
        t.run(&format!("{head} ISOLATION LEVEL {}", level.0))
            .expect("begin with isolation level");
        assert_eq!(t.txn.status(), TransactionStatus::InTransaction);
        assert_eq!(t.txn.isolation_level(), Some(level.1), "{}", level.0);
        t
    }

    fn run(&mut self, sql: &str) -> Result<(), String> {
        self.exec(sql).map(|_| ())
    }

    fn exec(&mut self, sql: &str) -> Result<SqlOutcome, String> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
            .map_err(|e| format!("{}|{e}", e.wire_code()))
    }

    /// SELECT の返却行数。
    fn count(&mut self, table: &str) -> usize {
        match self
            .exec(&format!("SELECT n FROM {table}"))
            .expect("select")
        {
            SqlOutcome::Query(r) => r.rows.len(),
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// SELECT の返却行の `n` 列を行 id 順の文字列表現で返す（値の変化を検出する用途。
    /// 具体的な `Cell` の型に依存せず、開始時点との同一性だけを比べる）。
    fn values(&mut self, table: &str) -> Vec<(u64, String)> {
        match self
            .exec(&format!("SELECT n FROM {table}"))
            .expect("select")
        {
            SqlOutcome::Query(r) => {
                let mut v: Vec<_> = r
                    .rows
                    .iter()
                    .map(|row| (row.id, format!("{:?}", row.cells)))
                    .collect();
                v.sort();
                v
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    fn expect_40001_then_recover(&mut self, sql: &str) {
        let err = self.run(sql).expect_err("40001");
        self.assert_failed_then_recover(&err);
    }

    /// `40001` → 以降の文は `25P02` → `ROLLBACK` で `Idle`。エラー文に表名・テナント名を含まない。
    fn assert_failed_then_recover(&mut self, err: &str) {
        assert!(err.starts_with("40001|"), "{err}");
        assert!(!err.contains("tenant") && !err.contains("iso1") && !err.contains("iso2"));
        assert_eq!(self.txn.status(), TransactionStatus::Failed);
        let e2 = self.run("SELECT 1").expect_err("25P02");
        assert!(e2.starts_with("25P02|"), "{e2}");
        self.run("ROLLBACK").expect("rollback");
        assert_eq!(self.txn.status(), TransactionStatus::Idle);
    }
}

/// 別スレッドで明示 Tx（最強レベル）として `table` へ書き込み（writer 取得）、`held` を送り、
/// `release` があればそれを受けてから commit する。
fn spawn_holder<'scope, 'env>(
    s: &'scope thread::Scope<'scope, 'env>,
    core: &'env EngineCore,
    table: &'static str,
    id: usize,
    held: mpsc::Sender<()>,
    release: Option<mpsc::Receiver<()>>,
) -> thread::ScopedJoinHandle<'scope, ()> {
    s.spawn(move || {
        let mut b = Tx::begin_with(core, "BEGIN", LEVELS[0]);
        b.run(&format!("INSERT INTO {table} (id, n) VALUES ({id}, 1)"))
            .expect("holder write");
        held.send(()).expect("signal");
        if let Some(r) = release {
            r.recv().expect("release");
        }
        b.run("COMMIT").expect("holder commit");
    })
}

/// 4 レベル × 2 形（`BEGIN`／`BEGIN TRANSACTION`）の開始が受理され、要求レベルが記録され、他セッションが writer を持つ中でも
/// 書き込み・COMMIT が通る。終了後の要求レベルは `None`。
#[test]
fn every_level_and_head_is_accepted_and_records_requested_level() {
    let (core, _g) = new_core("iso-c-accept");
    let mut id = 0;
    for head in HEADS {
        for level in LEVELS {
            let mut a = Tx::begin_with(&core, head, level);
            id += 10;
            thread::scope(|s| {
                let (tx, rx) = mpsc::channel();
                let h = spawn_holder(s, &core, "iso2", id, tx, None);
                rx.recv().expect("holder has writer");
                a.run(&format!("INSERT INTO iso1 (id, n) VALUES ({id}, 1)"))
                    .expect("write");
                h.join().expect("join");
            });
            a.run("COMMIT").expect("commit");
            assert_eq!(a.txn.isolation_level(), None);
        }
    }
    assert_eq!(Tx::begin_with(&core, "BEGIN", LEVELS[0]).count("iso1"), 8);
}

/// どのレベルでも、読んだ表へ他セッションの commit が入っていれば最初の書き込みは `40001`。
/// READ COMMITTED／READ UNCOMMITTED でも素通りしない。再試行は成功する。
#[test]
fn read_then_write_yields_40001_at_every_level() {
    for level in LEVELS {
        let (core, _g) = new_core("iso-c-40001");
        let mut a = Tx::begin_with(&core, "BEGIN", level);
        a.run("SELECT n FROM iso1").expect("read");
        thread::scope(|s| {
            let (tx, rx) = mpsc::channel();
            let h = spawn_holder(s, &core, "iso1", 1, tx, None);
            rx.recv().expect("holder has writer");
            a.expect_40001_then_recover("INSERT INTO iso2 (id, n) VALUES (2, 2)");
            h.join().expect("join");
        });
        let mut a = Tx::begin_with(&core, "BEGIN", level);
        a.run("SELECT n FROM iso1").expect("read");
        a.run("INSERT INTO iso2 (id, n) VALUES (2, 2)")
            .expect("retry");
        a.run("COMMIT").expect("commit");
    }
}

/// 他セッションの未 commit の行は、READ UNCOMMITTED を含むどのレベルでも見えない。
#[test]
fn no_dirty_read_at_any_level() {
    for level in LEVELS {
        let (core, _g) = new_core("iso-c-dirty");
        let mut a = Tx::begin_with(&core, "BEGIN", level);
        thread::scope(|s| {
            let (tx, rx) = mpsc::channel();
            let (rel_tx, rel_rx) = mpsc::channel();
            let h = spawn_holder(s, &core, "iso1", 1, tx, Some(rel_rx));
            rx.recv().expect("holder has writer, not committed");
            assert_eq!(a.count("iso1"), 0, "{}", level.0);
            rel_tx.send(()).expect("release");
            h.join().expect("join");
        });
        a.run("ROLLBACK").expect("rollback");
    }
}

/// 書き込み前の再読み取りは、他セッションの INSERT・UPDATE の commit が入っても開始時点と同じ。
#[test]
fn no_non_repeatable_or_phantom_read_before_write_at_any_level() {
    for level in LEVELS {
        let (core, _g) = new_core("iso-c-repeat");
        auto(&core, "INSERT INTO iso1 (id, n) VALUES (1, 1)");
        let mut a = Tx::begin_with(&core, "BEGIN", level);
        let before = a.values("iso1");
        assert_eq!(before.len(), 1);
        auto(&core, "INSERT INTO iso1 (id, n) VALUES (2, 2)");
        auto(&core, "UPDATE iso1 SET n = 9 WHERE id = 1");
        // 行数（ファントム）と行の値（non-repeatable read）の両方が開始時点と同じ。
        assert_eq!(a.values("iso1"), before, "{}", level.0);
        a.run("ROLLBACK").expect("rollback");
    }
}

/// レベルを混在させた 2 本の write skew は、組み合わせによらずちょうど 1 本だけが `40001`。
fn write_skew_round(core: &EngineCore, levels: [Level; 2], sides: [(&str, &str); 2], id: usize) {
    let barrier = Barrier::new(2);
    let results: Vec<Result<(), String>> = thread::scope(|s| {
        let handles: Vec<_> = sides
            .iter()
            .zip(levels)
            .enumerate()
            .map(|(i, (&(read, write), level))| {
                let barrier = &barrier;
                s.spawn(move || -> Result<(), String> {
                    let mut t = Tx::begin_with(core, "BEGIN", level);
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
fn write_skew_across_mixed_levels_yields_exactly_one_40001() {
    let (core, _g) = new_core("iso-c-skew");
    let [ser, rr, rc, ru] = LEVELS;
    let pairs = [[rc, rr], [ru, ser], [rc, rc], [ser, ru], [ru, ru]];
    for (p, pair) in pairs.iter().enumerate() {
        for r in 0..ROUNDS {
            let id = (p * ROUNDS + r) * 100;
            write_skew_round(&core, *pair, [("iso1", "iso2"), ("iso2", "iso1")], id);
            write_skew_round(&core, *pair, [("iso1", "iso1"), ("iso1", "iso1")], id + 10);
        }
    }
}

/// 読まずに書くだけの明示 Tx は、弱いレベルでも gate で直列化されて全員成功する。
#[test]
fn write_only_txns_at_weak_levels_serialize_and_all_commit() {
    let (core, _g) = new_core("iso-c-wonly");
    let barrier = Barrier::new(WRITERS);
    thread::scope(|s| {
        for k in 0..WRITERS {
            let (core, barrier) = (&core, &barrier);
            s.spawn(move || {
                let mut t = Tx::begin_with(core, "BEGIN", LEVELS[k % LEVELS.len()]);
                barrier.wait();
                t.run(&format!("INSERT INTO iso1 (id, n) VALUES ({k}, 1)"))
                    .expect("write");
                t.run("COMMIT").expect("commit");
            });
        }
    });
    let mut v = Tx::begin_with(&core, "BEGIN", LEVELS[0]);
    assert_eq!(v.count("iso1"), WRITERS);
}

/// 無関係な表への commit は、弱いレベルでも `40001` の判定粒度を変えない。
#[test]
fn unrelated_table_commit_does_not_cause_40001_at_any_level() {
    for level in LEVELS {
        let (core, _g) = new_core("iso-c-unrel");
        let mut a = Tx::begin_with(&core, "BEGIN", level);
        a.run("SELECT n FROM iso1").expect("read");
        thread::scope(|s| {
            let (tx, rx) = mpsc::channel();
            let h = spawn_holder(s, &core, "iso2", 1, tx, None);
            rx.recv().expect("holder has writer");
            a.run("INSERT INTO iso2 (id, n) VALUES (2, 2)")
                .expect("unrelated");
            a.run("COMMIT").expect("commit");
            h.join().expect("join");
        });
    }
}
