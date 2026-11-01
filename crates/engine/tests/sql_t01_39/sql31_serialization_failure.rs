//! writer 取得時の `40001` 判定（Issue #2405。親 #2396・TASK-333）の公開 API 結合テスト。
//! ポインタ: SQL-31・SQL-38（設計記録は `docs/design/explicit-transaction.md` 1a-3・1a-6・1a-11）。
//!
//! 確認する契約: (1) 読んでから書く形で、読んだ表に可視な commit が `BEGIN` 後に入っていれば
//! 最初の書き込みで `40001`、以降は拒否され `ROLLBACK` で復帰する、(2) 読まずに書く形・読み取り
//! だけの形・無関係な表への commit では `40001` にならない、(3) 他テナントの Private 行だけの
//! commit は原因にならず、エラー文にテナント・表名を含めない。
//! 同一スレッドで 2 セッションを順に操作する（並行は `sql31_serialization_failure_concurrent`〔#2406〕、3 テナントの網羅は #2407）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for t in ["sf1", "sf2"] {
        auto(&core, "sys", &format!("CREATE TABLE {t} (n BIGINT)"));
    }
    (core, guard)
}

fn auto(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
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
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
            .map(|_| ())
            .map_err(|e| format!("{}|{e}", e.wire_code()))
    }
}

#[test]
fn read_then_write_fails_with_40001_then_25p02_then_rollback_recovers() {
    let (core, _g) = new_core("sf-basic");
    let mut a = Tx::begin(&core, "tenant-a");
    a.run("SELECT n FROM sf1").expect("read");
    auto(&core, "tenant-a", "INSERT INTO sf1 (id, n) VALUES (1, 1)");

    let err = a
        .run("INSERT INTO sf2 (id, n) VALUES (2, 2)")
        .expect_err("40001");
    assert!(err.starts_with("40001|"), "{err}");
    assert!(!err.contains("tenant") && !err.contains("sf1") && !err.contains("sf2"));
    assert_eq!(a.txn.status(), TransactionStatus::Failed);
    let err = a.run("SELECT 1").expect_err("25P02");
    assert!(err.starts_with("25P02|"), "{err}");
    a.run("ROLLBACK").expect("rollback");
    assert_eq!(a.txn.status(), TransactionStatus::Idle);
    // 書き込みは残らず、再試行は成功する。
    let mut a = Tx::begin(&core, "tenant-a");
    a.run("SELECT n FROM sf1").expect("read");
    a.run("INSERT INTO sf2 (id, n) VALUES (2, 2)")
        .expect("retry");
    a.run("COMMIT").expect("commit");
}

#[test]
fn no_conflict_for_write_only_read_only_and_unrelated_tables() {
    let (core, _g) = new_core("sf-noconflict");
    let mut a = Tx::begin(&core, "tenant-a");
    auto(&core, "tenant-a", "INSERT INTO sf1 (id, n) VALUES (1, 1)");
    a.run("INSERT INTO sf1 (id, n) VALUES (2, 2)")
        .expect("write only");
    a.run("COMMIT").expect("commit");

    let mut a = Tx::begin(&core, "tenant-a");
    a.run("SELECT n FROM sf1").expect("read");
    auto(&core, "tenant-a", "INSERT INTO sf1 (id, n) VALUES (3, 3)");
    a.run("COMMIT").expect("read only");

    let mut a = Tx::begin(&core, "tenant-a");
    a.run("SELECT n FROM sf1").expect("read");
    auto(&core, "tenant-a", "INSERT INTO sf2 (id, n) VALUES (4, 4)");
    a.run("INSERT INTO sf2 (id, n) VALUES (5, 5)")
        .expect("unrelated");
    a.run("COMMIT").expect("commit");
}

#[test]
fn other_tenants_private_commit_is_not_a_cause() {
    let (core, _g) = new_core("sf-tenant");
    let mut a = Tx::begin(&core, "tenant-a");
    a.run("SELECT n FROM sf1").expect("read");
    auto(&core, "tenant-b", "INSERT INTO sf1 (id, n) VALUES (1, 1)");
    a.run("INSERT INTO sf2 (id, n) VALUES (2, 2)")
        .expect("invisible commit is not a cause");
    a.run("COMMIT").expect("commit");
}
