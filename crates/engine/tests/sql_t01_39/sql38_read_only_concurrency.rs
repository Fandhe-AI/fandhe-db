//! `BEGIN READ ONLY` の並行状態でのスナップショット一貫性・テナント境界・READ ONLY であることによる
//! 差の結合テスト（Issue #1935。親系統 #1932・#1934・#1933）。ポインタ: SQL-38・SQL-31・TABLE-3
//! （設計記録は `docs/design/explicit-transaction.md` の「READ ONLY の読み取りトランザクション」節と 1a-8）。
//!
//! 確認する契約: (1) READ ONLY の Tx が開いている間も別セッションの書き込み・commit が完了する
//! （単一ライタを占有しない）、(2) READ ONLY 側の読み取り（Scan・集計・集合演算・JOIN・ビュー・
//! カーソル・COPY TO）は `BEGIN` 時点のまま変わらない、(3) 他テナントの変更は観測に影響しない、
//! (4) 通常の `BEGIN` が `40001`／`55P03` になる状況でも READ ONLY の書き込み文は `25006` になる。
//!
//! 既存テストとの分担: 書き込み拒否の全種類は `sql38_read_only_write_rejection`、時間計測は
//! `write_lock_wait.rs`、通常 BEGIN の切り替えは `sql31_snapshot_switch`。順序制御は `mpsc` だけで行い、
//! sleep・実時間の閾値は使わない。スレッド数・ラウンド数・行数は定数で上限化する。
//! 例外: `USING PLAN` 付き SELECT と `EXPLAIN` は文ごとに最新の確定状態を読む既知の残り
//! （`explicit-transaction.md` の「既知の残り」）で、一貫性の対象外。成功することだけを確かめる。

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility, MAX_WRITE_LOCK_WAIT_MS};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

/// 並行 writer の本数（上限）。
const WRITERS: usize = 4;
/// writer ごとのラウンド数（上限）。
const ROUNDS: usize = 8;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `wait_ms` は writer 待機上限。並行ケースは最大、`55P03` の対照ケースは短くする。
fn new_core(label: &str, wait_ms: u64) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = if wait_ms == MAX_WRITE_LOCK_WAIT_MS {
        core.with_write_lock_wait(validate_write_lock_wait_ms(wait_ms).expect("valid wait"))
    } else {
        core.with_write_lock_wait(Duration::from_millis(wait_ms))
    };
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE docs (n BIGINT)");
    ok(&core, &sys, "CREATE TABLE docs2 (n BIGINT)");
    ok(&core, &sys, "CREATE VIEW docs_v AS SELECT id, n FROM docs");
    (core, guard)
}

fn ok(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn rows(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Query(r) => {
            let mut v: Vec<String> = r
                .rows
                .iter()
                .map(|row| format!("{:?}", row.cells))
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Query outcome, got {other:?}"),
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
    fn new(core: &'e EngineCore, tenant: &str) -> Self {
        Self {
            core,
            ctx: ctx(tenant),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        }
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, String> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
            .map_err(|e| format!("{}|{e}", e.wire_code()))
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.run(sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed in the transaction, got {e}"))
    }

    fn rows(&mut self, sql: &str) -> Vec<String> {
        rows(self.ok(sql))
    }

    fn begin_read_only(core: &'e EngineCore, tenant: &str) -> Self {
        let mut t = Self::new(core, tenant);
        t.ok("BEGIN READ ONLY");
        t
    }

    fn fetch_all(&mut self, cursor: &str) -> usize {
        match self.ok(&format!("FETCH 100 FROM {cursor}")) {
            SqlOutcome::Fetch(r) => r.rows.len(),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn copy_to_rows(&mut self) -> usize {
        let plan = self
            .core
            .begin_copy_in_txn(
                &self.ctx,
                &self.session,
                &mut self.txn,
                "COPY (SELECT id, n FROM docs LIMIT 100) TO STDOUT",
            )
            .expect("copy to");
        match plan {
            CopyPlan::To(_, result, _) => result.rows.len(),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// READ ONLY 側の観測列（Scan・集計・集合演算・JOIN・ビュー・COPY TO の行数）。
    fn observe(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for sql in [
            "SELECT id, n FROM docs LIMIT 100",
            "SELECT COUNT(*) FROM docs",
            "SELECT id FROM docs UNION ALL SELECT id FROM docs2 LIMIT 100",
            "SELECT a.id, b.id FROM docs a JOIN docs2 b ON a.id = b.id LIMIT 100",
            "SELECT id, n FROM docs_v LIMIT 100",
        ] {
            out.extend(self.rows(sql));
            out.push("--".to_string());
        }
        out.push(format!("copy={}", self.copy_to_rows()));
        out
    }
}

fn seed(core: &EngineCore, tenant: &str) {
    ok(
        core,
        &ctx(tenant),
        "INSERT INTO docs (id, n) VALUES (1, 10), (2, 20), (3, 30) USING OPERATION_ID 'seed'",
    );
    ok(
        core,
        &ctx(tenant),
        "INSERT INTO docs2 (id, n) VALUES (1, 1), (2, 2) USING OPERATION_ID 'seed2'",
    );
}

/// 要件 1・2: 別セッションの各種書き込み（autocommit・明示 Tx・TRUNCATE）が READ ONLY 中も成功し、
/// READ ONLY 側の観測は BEGIN 時点のまま。COMMIT／ROLLBACK の後は最新が見える。
#[test]
fn read_only_reads_keep_begin_time_snapshot_across_other_commits() {
    for end in ["COMMIT", "ROLLBACK"] {
        let (core, _g) = new_core("sql38-ro-conc-snap", MAX_WRITE_LOCK_WAIT_MS);
        seed(&core, "tenant-a");
        let other = ctx("tenant-a");
        let mut a = Tx::begin_read_only(&core, "tenant-a");
        let base = a.observe();
        // BEGIN 後・他セッション commit 前に宣言したカーソル。
        a.ok("DECLARE early CURSOR FOR SELECT id, n FROM docs LIMIT 100");

        let writes = [
            "INSERT INTO docs (id, n) VALUES (4, 40) USING OPERATION_ID 'o-ins'",
            "UPDATE docs SET n = 99 WHERE id = 1 USING OPERATION_ID 'o-up'",
            "DELETE FROM docs WHERE id = 2 USING OPERATION_ID 'o-del'",
            "INSERT INTO docs2 (id, n) VALUES (3, 3) USING OPERATION_ID 'o-ins2'",
        ];
        for sql in writes {
            ok(&core, &other, sql);
            assert_eq!(a.observe(), base, "after {sql}");
        }
        // 別セッションの明示 Tx。
        let mut b = Tx::new(&core, "tenant-a");
        b.ok("BEGIN");
        b.ok("INSERT INTO docs (id, n) VALUES (5, 50) USING OPERATION_ID 'o-tx'");
        b.ok("COMMIT");
        assert_eq!(a.observe(), base, "after explicit tx commit");

        // 他セッション commit の後に宣言したカーソルも BEGIN 時点。
        a.ok("DECLARE late CURSOR FOR SELECT id, n FROM docs LIMIT 100");
        assert_eq!(a.fetch_all("late"), 3);
        assert_eq!(a.fetch_all("early"), 3);

        // EXPLAIN は一貫性の対象外（既知の残り）。成功と Tx 状態だけを確かめる。
        a.ok("EXPLAIN SELECT id FROM docs LIMIT 100");
        assert_eq!(a.txn.status(), TransactionStatus::InTransaction);

        ok(
            &core,
            &other,
            "TRUNCATE TABLE docs USING OPERATION_ID 'o-trunc'",
        );
        assert_eq!(a.observe(), base, "after truncate");

        a.ok(end);
        assert_eq!(a.txn.status(), TransactionStatus::Idle);
        let latest = rows(ok(&core, &other, "SELECT id, n FROM docs LIMIT 100"));
        assert!(latest.is_empty(), "truncate is visible after {end}");
    }
}

/// 要件 4: 通常 BEGIN は読んでから書くと `40001`、READ ONLY の同じ書き込みは `25006`。
#[test]
fn read_only_write_attempt_is_25006_while_plain_begin_gets_40001() {
    let (core, _g) = new_core("sql38-ro-conc-40001", MAX_WRITE_LOCK_WAIT_MS);
    seed(&core, "tenant-a");
    let other = ctx("tenant-a");

    let mut plain = Tx::new(&core, "tenant-a");
    plain.ok("BEGIN");
    plain.rows("SELECT id FROM docs LIMIT 100");
    ok(
        &core,
        &other,
        "INSERT INTO docs (id, n) VALUES (4, 40) USING OPERATION_ID 'p-c'",
    );
    let err = plain
        .run("INSERT INTO docs (id, n) VALUES (9, 9) USING OPERATION_ID 'p-w'")
        .expect_err("40001");
    assert!(err.starts_with("40001|"), "{err}");
    plain.ok("ROLLBACK");

    let mut ro = Tx::begin_read_only(&core, "tenant-a");
    ro.rows("SELECT id FROM docs LIMIT 100");
    ok(
        &core,
        &other,
        "INSERT INTO docs (id, n) VALUES (5, 50) USING OPERATION_ID 'r-c'",
    );
    let err = ro
        .run("INSERT INTO docs (id, n) VALUES (9, 9) USING OPERATION_ID 'r-w'")
        .expect_err("25006");
    assert!(err.starts_with("25006|"), "{err}");
    assert_eq!(ro.txn.status(), TransactionStatus::Failed);
    let e2 = ro.run("SELECT 1").expect_err("25P02");
    assert!(e2.starts_with("25P02|"), "{e2}");
    ro.ok("ROLLBACK");
    assert_eq!(ro.txn.status(), TransactionStatus::Idle);

    // 拒否された INSERT は残らず、同じ operation_id の再送は新規として成功する。
    let all = rows(ok(&core, &other, "SELECT id FROM docs LIMIT 100"));
    assert_eq!(all.len(), 5, "{all:?}");
    ok(
        &core,
        &other,
        "INSERT INTO docs (id, n) VALUES (9, 9) USING OPERATION_ID 'r-w'",
    );
}

/// 時点をずらして開いた 2 つの READ ONLY は互いに干渉せず、その間も別セッションの書き込みは成功する。
#[test]
fn two_read_only_sessions_and_writer_interleave() {
    let (core, _g) = new_core("sql38-ro-conc-two", MAX_WRITE_LOCK_WAIT_MS);
    seed(&core, "tenant-a");
    let other = ctx("tenant-a");
    let mut a1 = Tx::begin_read_only(&core, "tenant-a");
    let base1 = a1.observe();
    ok(
        &core,
        &other,
        "INSERT INTO docs (id, n) VALUES (4, 40) USING OPERATION_ID 'i-1'",
    );
    let mut a2 = Tx::begin_read_only(&core, "tenant-a");
    let base2 = a2.observe();
    assert_ne!(base1, base2);
    ok(
        &core,
        &other,
        "INSERT INTO docs (id, n) VALUES (5, 50) USING OPERATION_ID 'i-2'",
    );
    assert_eq!(a1.observe(), base1);
    assert_eq!(a2.observe(), base2);
    a1.ok("COMMIT");
    assert_eq!(a2.observe(), base2);
    a2.ok("COMMIT");
}

/// 要件 1・2: 別スレッドの writer が実際に動いている並行状態でも READ ONLY の観測は変わらない。
#[test]
fn read_only_snapshot_is_stable_under_concurrent_writers() {
    let (core, _g) = new_core("sql38-ro-conc-threads", MAX_WRITE_LOCK_WAIT_MS);
    seed(&core, "tenant-a");
    let mut a = Tx::begin_read_only(&core, "tenant-a");
    let base = a.observe();
    thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<()>();
        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let tx = tx.clone();
            let core = &core;
            handles.push(s.spawn(move || {
                for r in 0..ROUNDS {
                    let id = 1000 + w * 100 + r;
                    let table = if r % 2 == 0 { "docs" } else { "docs2" };
                    let sql = format!(
                        "INSERT INTO {table} (id, n) VALUES ({id}, 1) \
                         USING OPERATION_ID 'w-{w}-{r}'"
                    );
                    if r % 3 == 0 {
                        // 明示 Tx（読まずに書くので 40001 にならない）。
                        let mut t = Tx::new(core, "tenant-a");
                        t.ok("BEGIN");
                        t.ok(&sql);
                        t.ok("COMMIT");
                    } else {
                        ok(core, &ctx("tenant-a"), &sql);
                    }
                    tx.send(()).expect("notify");
                }
            }));
        }
        drop(tx);
        let mut seen = 0;
        while rx.recv().is_ok() {
            seen += 1;
            assert_eq!(a.observe(), base, "after {seen} commits");
        }
        assert_eq!(seen, WRITERS * ROUNDS);
        for h in handles {
            h.join().expect("writer thread");
        }
    });
    assert_eq!(a.observe(), base);
    a.ok("COMMIT");
    let docs = rows(ok(
        &core,
        &ctx("tenant-a"),
        "SELECT id FROM docs LIMIT 1000",
    ));
    let docs2 = rows(ok(
        &core,
        &ctx("tenant-a"),
        "SELECT id FROM docs2 LIMIT 1000",
    ));
    let per_writer_docs = ROUNDS.div_ceil(2);
    assert_eq!(docs.len(), 3 + WRITERS * per_writer_docs);
    assert_eq!(docs2.len(), 2 + WRITERS * (ROUNDS - per_writer_docs));
}

/// 要件 4: 別セッションが writer を保持していても、通常 BEGIN の書き込みは `55P03`、
/// READ ONLY の書き込みは gate に到達せず `25006`。
#[test]
fn read_only_write_attempt_is_25006_without_touching_the_gate() {
    let (core, _g) = new_core("sql38-ro-conc-gate", 100);
    seed(&core, "tenant-a");
    thread::scope(|s| {
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let (rel_tx, rel_rx) = mpsc::channel::<()>();
        let core_ref = &core;
        let h = s.spawn(move || {
            let mut b = Tx::new(core_ref, "tenant-a");
            b.ok("BEGIN");
            b.ok("INSERT INTO docs2 (id, n) VALUES (70, 1) USING OPERATION_ID 'h-1'");
            held_tx.send(()).expect("held");
            rel_rx.recv().expect("release");
            b.ok("COMMIT");
        });
        held_rx.recv().expect("holder has writer");

        let mut plain = Tx::new(&core, "tenant-a");
        plain.ok("BEGIN");
        let err = plain
            .run("INSERT INTO docs (id, n) VALUES (9, 9) USING OPERATION_ID 'g-p'")
            .expect_err("55P03");
        assert!(err.starts_with("55P03|"), "{err}");
        plain.ok("ROLLBACK");

        let mut ro = Tx::begin_read_only(&core, "tenant-a");
        assert_eq!(ro.rows("SELECT id FROM docs LIMIT 100").len(), 3);
        let err = ro
            .run("INSERT INTO docs (id, n) VALUES (9, 9) USING OPERATION_ID 'g-r'")
            .expect_err("25006");
        assert!(err.starts_with("25006|"), "{err}");
        ro.ok("ROLLBACK");

        rel_tx.send(()).expect("release");
        h.join().expect("holder commit");
    });
}

/// 要件 3: 他テナントの変更（BEGIN 前・最中・読み取りの合間）は READ ONLY の観測に影響しない。
#[test]
fn read_only_observation_is_independent_of_other_tenants() {
    fn observe_world(label: &str, noisy: bool) -> Vec<String> {
        let (core, _g) = new_core(label, MAX_WRITE_LOCK_WAIT_MS);
        seed(&core, "tenant-a");
        // id は表内で一意なので、ノイズごと・テナントごとに重ならない値を割り当てる。
        let noise = |k: usize, tag: &str| {
            if !noisy {
                return;
            }
            for (i, t) in ["tenant-b", "tenant-c"].into_iter().enumerate() {
                let c = ctx(t);
                let base = 1000 + k * 100 + i * 10;
                ok(
                    &core,
                    &c,
                    &format!(
                        "INSERT INTO docs (id, n) VALUES ({base}, 111), ({}, 7), ({}, 8) \
                         USING OPERATION_ID '{t}-{tag}'",
                        base + 1,
                        base + 2
                    ),
                );
                ok(
                    &core,
                    &c,
                    &format!(
                        "UPDATE docs SET n = 5 WHERE id = {} USING OPERATION_ID '{t}-u-{tag}'",
                        base + 1
                    ),
                );
                ok(
                    &core,
                    &c,
                    &format!(
                        "DELETE FROM docs WHERE id = {} USING OPERATION_ID '{t}-d-{tag}'",
                        base + 2
                    ),
                );
            }
        };
        noise(0, "pre");
        let mut a = Tx::begin_read_only(&core, "tenant-a");
        noise(1, "mid");
        let mut out = a.observe();
        noise(2, "between");
        a.ok("DECLARE c CURSOR FOR SELECT id, n FROM docs LIMIT 100");
        out.push(format!("fetch={}", a.fetch_all("c")));
        out.extend(a.rows("SELECT id, n FROM docs WHERE id = 1001 LIMIT 100"));
        out.extend(a.observe());
        a.ok("COMMIT");
        out
    }
    let quiet = observe_world("sql38-ro-conc-tenant-x", false);
    let noisy = observe_world("sql38-ro-conc-tenant-y", true);
    assert_eq!(quiet, noisy);
    // 陽性の対照: 自テナントの BEGIN 前の commit は見える。
    assert!(quiet.iter().any(|r| r.contains("copy=3")), "{quiet:?}");
    assert!(quiet.iter().any(|r| r.contains("fetch=3")), "{quiet:?}");
}

/// 25006 の応答は他テナントの行の有無・名前を含まず、行の有無で変わらない。READ ONLY は
/// 他テナントの書き込みも待たせない。
#[test]
fn read_only_errors_do_not_leak_other_tenants() {
    fn error_of(label: &str, with_other: bool) -> String {
        let (core, _g) = new_core(label, MAX_WRITE_LOCK_WAIT_MS);
        seed(&core, "tenant-a");
        if with_other {
            ok(
                &core,
                &ctx("tenant-b"),
                "INSERT INTO docs (id, n) VALUES (1, 424242) USING OPERATION_ID 'leak-b'",
            );
        }
        let mut a = Tx::begin_read_only(&core, "tenant-a");
        let e1 = a
            .run("UPDATE docs SET n = 1 WHERE id = 1 USING OPERATION_ID 'x-u'")
            .expect_err("25006");
        assert!(e1.starts_with("25006|"), "{e1}");
        a.ok("ROLLBACK");
        let mut a = Tx::begin_read_only(&core, "tenant-a");
        let e2 = a
            .run("DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'x-d'")
            .expect_err("25006");
        assert!(e2.starts_with("25006|"), "{e2}");
        a.ok("ROLLBACK");
        for e in [&e1, &e2] {
            assert!(!e.contains("tenant-b") && !e.contains("424242"), "{e}");
        }
        format!("{e1}#{e2}")
    }
    assert_eq!(
        error_of("sql38-ro-conc-leak-x", false),
        error_of("sql38-ro-conc-leak-y", true)
    );

    // 対称: tenant-b が READ ONLY を開いている間も tenant-a の書き込みは成功する。
    let (core, _g) = new_core("sql38-ro-conc-leak-z", MAX_WRITE_LOCK_WAIT_MS);
    let mut b = Tx::begin_read_only(&core, "tenant-b");
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO docs (id, n) VALUES (1, 1) USING OPERATION_ID 'sym-a'",
    );
    assert!(b.rows("SELECT id FROM docs LIMIT 100").is_empty());
    b.ok("COMMIT");
}
