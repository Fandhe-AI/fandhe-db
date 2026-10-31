//! UDF 件数上限の起動時設定と DDL 権限の整合の結合テスト（Issue #1922、
//! 対象ビヘイビア: SQL-66 (5)・関連 SQL-46・SQL-9 のポインタ。設計記録: `docs/design/udf-catalog.md`）。
//!
//! 固定する契約: 設定した上限が権限ありのカタログ経路で効く（既定は 64）／件数はテナントごと／
//! 重複 `42723` が上限 `54000` より先／上限を下げて再オープンしても既存の関数はロードされ呼べる／
//! 権限の無いセッションの `CREATE`（`OR REPLACE`・型付き引数・RETURNS を含む）と `DROP FUNCTION` は
//! カタログにもセッション内レジストリにも触れず `42501`（保存先の振り分けより前に拒否）。

use engine::core::EngineCore;
use engine::function_limits::FunctionLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(path: &std::path::Path, max_functions: usize) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
    .with_function_limits(FunctionLimits::new(max_functions))
}

fn session(ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    if ddl {
        s.allow_ddl();
    }
    s
}

fn run(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, (String, String)> {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .map_err(|e| (e.wire_code().to_string(), e.to_string()))
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    run(core, tenant, s, sql).unwrap_or_else(|(c, m)| panic!("{sql}: {c} {m}"));
}

fn code(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match run(core, tenant, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err((c, _)) => c,
    }
}

fn create(n: usize) -> String {
    format!("CREATE FUNCTION f{n}(x) AS x + {n}")
}

/// T1: 権限ありの経路で設定した上限が効き、件数はテナントごとに数える。
#[test]
fn configured_limit_applies_to_catalog_path_per_tenant() {
    let path = unique_db_path("sql66-limit-catalog");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut a = session(true);
    ok(&core, "alice", &mut a, &create(1));
    ok(&core, "alice", &mut a, &create(2));
    assert_eq!(code(&core, "alice", &mut a, &create(3)), "54000");
    // 他テナントは自分の分を 2 件まで作れる。
    let mut b = session(true);
    ok(&core, "bob", &mut b, &create(1));
    ok(&core, "bob", &mut b, &create(2));
    assert_eq!(code(&core, "bob", &mut b, &create(3)), "54000");
}

/// T2: 既定の上限は 64（設定なしの `EngineCore`）。
#[test]
fn default_limit_is_64() {
    let path = unique_db_path("sql66-limit-default");
    let _g = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let mut s = session(true);
    for n in 0..64 {
        ok(&core, "alice", &mut s, &create(n));
    }
    assert_eq!(code(&core, "alice", &mut s, &create(64)), "54000");
}

/// T3: 上限に達していても同名の再定義は 42723（重複が上限より先に判定される）。
#[test]
fn duplicate_is_reported_before_limit() {
    let path = unique_db_path("sql66-limit-order");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 1);
    let mut s = session(true);
    ok(&core, "alice", &mut s, &create(1));
    assert_eq!(code(&core, "alice", &mut s, &create(1)), "42723");
}

/// T4: 上限を下げて再オープンしても既存の関数はロードされ呼べる。新規作成だけが拒否される。
#[test]
fn lowering_the_limit_keeps_existing_functions_usable() {
    let path = unique_db_path("sql66-limit-lowered");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path, 64);
        let mut s = session(true);
        ok(
            &core,
            "alice",
            &mut s,
            "CREATE TABLE t (id BIGINT PRIMARY KEY, n INTEGER)",
        );
        ok(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, n) VALUES (1, 5)",
        );
        for n in 1..=3 {
            ok(&core, "alice", &mut s, &create(n));
        }
    }
    let core = open(&path, 1);
    let mut s = session(true);
    core.load_session_udfs(&ctx("alice"), &mut s)
        .expect("load session udfs");
    for n in 1..=3 {
        ok(
            &core,
            "alice",
            &mut s,
            &format!("SELECT f{n}(n) AS d FROM t LIMIT 1"),
        );
    }
    assert_eq!(code(&core, "alice", &mut s, &create(4)), "54000");
    for n in 1..=3 {
        ok(&core, "alice", &mut s, &format!("DROP FUNCTION f{n}"));
    }
    ok(&core, "alice", &mut s, &create(4));
}

/// T5: 権限の無いセッションの CREATE／CREATE OR REPLACE／型付き引数／RETURNS／DROP はすべて `42501`
/// で、カタログ・セッション内レジストリ・件数のいずれも変わらない（受け入れ条件「権限なしは拒否」）。
/// 型付き引数と RETURNS の永続化（#2798）が無くても、権限検査は保存先の振り分けより前に効く。
#[test]
fn session_without_permission_is_rejected_and_nothing_changes() {
    let path = unique_db_path("sql66-limit-perm");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut d = session(true);
    ok(&core, "alice", &mut d, &create(1));
    let mut s = session(false);
    core.load_session_udfs(&ctx("alice"), &mut s)
        .expect("load session udfs");
    assert_eq!(s.udfs().len(), 1);
    for sql in [
        create(2),
        "CREATE OR REPLACE FUNCTION f2(x) AS x + 2".to_string(),
        "CREATE OR REPLACE FUNCTION f1(x) AS x + 100".to_string(),
        "CREATE FUNCTION f3(x integer) AS x + 3".to_string(),
        "CREATE OR REPLACE FUNCTION f3(x integer) AS x + 3".to_string(),
        "CREATE FUNCTION f4() RETURNS integer AS 4".to_string(),
        "DROP FUNCTION f1".to_string(),
        "DROP FUNCTION IF EXISTS f1".to_string(),
        "DROP FUNCTION IF EXISTS never_defined".to_string(),
        "DROP FUNCTION never_defined".to_string(),
    ] {
        assert_eq!(code(&core, "alice", &mut s, &sql), "42501", "{sql}");
        assert_eq!(s.udfs().len(), 1, "registry must not change: {sql}");
    }
    // カタログ: f1 は残り、件数は 1 のまま（上限 2 のもとで 1 件だけ追加できる）。
    let mut d2 = session(true);
    core.load_session_udfs(&ctx("alice"), &mut d2)
        .expect("load session udfs");
    assert_eq!(d2.udfs().len(), 1);
    ok(&core, "alice", &mut d2, &create(2));
    assert_eq!(code(&core, "alice", &mut d2, &create(3)), "54000");
    assert_eq!(code(&core, "alice", &mut d2, &create(1)), "42723");
}

/// T5b: 権限の無いセッションは明示トランザクション内でも `42501`（`0A000` より先に権限を検査する）。
#[test]
fn create_and_drop_in_explicit_transaction_without_permission_are_42501() {
    let path = unique_db_path("sql66-limit-perm-txn");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut s = session(false);
    let alice = ctx("alice");
    for sql in [create(1), "DROP FUNCTION f1".to_string()] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&alice, &mut s, &mut txn, &sql)
            .expect_err("rejected");
        assert_eq!(err.wire_code(), "42501", "{sql}");
    }
}

/// T6: 権限ありの明示トランザクション内の CREATE FUNCTION は上限の設定に関係なく 0A000。
#[test]
fn create_in_explicit_transaction_stays_rejected() {
    let path = unique_db_path("sql66-limit-txn");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut s = session(true);
    let alice = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&alice, &mut s, &mut txn, &create(1))
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "0A000");
}
