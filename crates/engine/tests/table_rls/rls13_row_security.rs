//! `ALTER TABLE ... ENABLE／DISABLE／FORCE／NO FORCE ROW LEVEL SECURITY`（RLS-13・Issue #2275）の
//! SQL 水準の結合テスト。ポインタ: `docs/spec/04-behavior/rls.md` の RLS-13、設計記録は
//! `docs/design/row-security-policy-composition.md`。
//!
//! 本 Issue が入れるのは有効化状態の DDL と永続化だけで、行の見え方への適用（既定拒否・USING の評価）は
//! #2276 以降の担当。そのため本ファイルは (1) 構文・SQLSTATE・権限ゲートの契約、(2) 状態遷移と
//! テナントごとの独立・ライフサイクル・永続化、(3) 状態を切り替えてもテナント境界が変わらないこと
//! を固定し、「ENABLE 後の自テナント内の行の見え方」は PostgreSQL と異なる期待値を主張しない。

use engine::catalog::row_security::RowSecurityState;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::ddl::AlterTableAction;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn table_schema(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("a", ColumnType::Text, true)])
}

fn open_fixture(label: &str) -> (CleanupGuard, std::path::PathBuf, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&table_schema("t")).expect("create t");
    (
        guard,
        path,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> SqlOutcome {
    run_as(core, tenant, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code_as(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> &'static str {
    match run_as(core, tenant, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn state(path: &std::path::Path, tenant: &str, table: &str) -> RowSecurityState {
    // 開いている DB のロックを避けるため、各確認は core を drop してから別 Storage で行う。
    Storage::open(path)
        .expect("reopen")
        .table_row_security(tenant, table)
        .expect("state")
}

const FOUR: [&str; 4] = [
    "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
    "ALTER TABLE t DISABLE ROW LEVEL SECURITY",
    "ALTER TABLE t FORCE ROW LEVEL SECURITY",
    "ALTER TABLE t NO FORCE ROW LEVEL SECURITY",
];

/// 4 文の受理と応答、再指定の成功。
#[test]
fn four_statements_are_accepted_with_row_security_outcome() {
    let (_g, _p, core) = open_fixture("rls13-accept");
    let mut s = allowed_session();
    for sql in FOUR.iter().chain(FOUR.iter()) {
        match ok_as(&core, "alice", &mut s, sql) {
            SqlOutcome::AlterTable(o) => {
                assert_eq!(o.table_name, "t");
                assert!(
                    matches!(o.action, AlterTableAction::RowSecurity { .. }),
                    "{sql}: {:?}",
                    o.action
                );
            }
            other => panic!("{sql}: unexpected {other:?}"),
        }
    }
}

/// 権限の無い主体には、対象表の有無を問わず常に `42501`（存在オラクルにしない）で、状態は変わらない。
#[test]
fn ddl_permission_gate_precedes_every_catalog_lookup() {
    let (_g, path, core) = open_fixture("rls13-perm");
    let mut s = SessionState::default();
    for sql in [
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE nosuch ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE t FORCE ROW LEVEL SECURITY",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "42501", "{sql}");
    }
    drop(core);
    assert_eq!(state(&path, "alice", "t"), RowSecurityState::default());
}

#[test]
fn sqlstate_contract() {
    let (_g, _p, core) = open_fixture("rls13-codes");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE VIEW v AS SELECT a FROM t");
    for (sql, want) in [
        ("ALTER TABLE nosuch ENABLE ROW LEVEL SECURITY", "42P01"),
        ("ALTER TABLE v ENABLE ROW LEVEL SECURITY", "42809"),
        ("ALTER TABLE t ENABLE TRIGGER x", "42601"),
        ("ALTER TABLE t DISABLE RULE x", "42601"),
        ("ALTER TABLE t NO ROW LEVEL SECURITY", "42601"),
        ("ALTER TABLE t ENABLE ROW LEVEL SECURITY extra", "42601"),
        (
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY",
            "0A000",
        ),
        (
            "ALTER TABLE t ADD COLUMN z TEXT, ENABLE ROW LEVEL SECURITY",
            "0A000",
        ),
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), want, "{sql}");
    }
}

/// 明示 Tx 内は `0A000` で、ROLLBACK の後も状態は変わらない。
#[test]
fn explicit_transaction_rejects_and_leaves_state_unchanged() {
    let (_g, path, core) = open_fixture("rls13-tx");
    let mut s = allowed_session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, FOUR[0])
        .expect_err("DDL inside a transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    drop(core);
    assert_eq!(state(&path, "alice", "t"), RowSecurityState::default());
}

/// 一時表が同名の永続表を隠しているとき、永続表の状態を黙って変えない（`0A000`）。
#[test]
fn temp_table_shadowing_does_not_touch_the_persistent_table() {
    let (_g, path, core) = open_fixture("rls13-temp");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE TEMP TABLE t (a TEXT)");
    assert_eq!(code_as(&core, "alice", &mut s, FOUR[0]), "0A000");
    drop(core);
    assert_eq!(state(&path, "alice", "t"), RowSecurityState::default());
}

/// FORCE だけでは適用されず、ENABLE で両方に適用、NO FORCE で所有者が外れ、DISABLE で全員外れる。
#[test]
fn state_transitions_follow_postgres_bits() {
    let (_g, path, core) = open_fixture("rls13-trans");
    let mut s = allowed_session();
    for sql in [FOUR[2], FOUR[0], FOUR[3], FOUR[1]] {
        ok_as(&core, "alice", &mut s, sql);
    }
    drop(core);
    // 最終状態: FORCE → ENABLE → NO FORCE → DISABLE で両方 false。
    assert_eq!(state(&path, "alice", "t"), RowSecurityState::default());

    let (_g2, path2, core2) = open_fixture("rls13-trans2");
    let mut s2 = allowed_session();
    ok_as(&core2, "alice", &mut s2, FOUR[2]);
    ok_as(&core2, "alice", &mut s2, FOUR[0]);
    ok_as(&core2, "alice", &mut s2, FOUR[3]);
    drop(core2);
    let st = state(&path2, "alice", "t");
    assert!(st.user_policies_apply(false) && !st.user_policies_apply(true));
}

/// A の ENABLE／FORCE は B・C に現れず、B の DISABLE は A に影響しない。応答に他テナントの状態は現れない。
#[test]
fn tenants_are_independent() {
    let (_g, path, core) = open_fixture("rls13-tenants");
    let mut a = allowed_session();
    let mut b = allowed_session();
    ok_as(&core, "ta", &mut a, FOUR[0]);
    ok_as(&core, "ta", &mut a, FOUR[2]);
    // 他テナントが ENABLE 済みでも B の応答は変わらない（同じ成功）。
    let before = format!("{:?}", ok_as(&core, "tb", &mut b, FOUR[1]));
    let again = format!("{:?}", ok_as(&core, "tb", &mut b, FOUR[1]));
    assert_eq!(before, again);
    drop(core);
    assert_eq!(
        state(&path, "ta", "t"),
        RowSecurityState {
            enabled: true,
            forced: true
        }
    );
    assert_eq!(state(&path, "tb", "t"), RowSecurityState::default());
    assert_eq!(state(&path, "tc", "t"), RowSecurityState::default());
}

/// DROP TABLE で全テナント分が消え、同名再作成で既定に戻る。RENAME は新しい名前へ移る。
#[test]
fn lifecycle_follows_drop_and_rename() {
    let (_g, path, core) = open_fixture("rls13-life");
    let mut a = allowed_session();
    let mut b = allowed_session();
    ok_as(&core, "ta", &mut a, FOUR[0]);
    ok_as(&core, "tb", &mut b, FOUR[2]);
    ok_as(&core, "ta", &mut a, "ALTER TABLE t RENAME TO t2");
    ok_as(&core, "ta", &mut a, "DROP TABLE t2");
    drop(core);
    {
        let s = Storage::open(&path).expect("reopen");
        s.create_table(&table_schema("t2")).expect("recreate");
        s.create_table(&table_schema("t")).expect("recreate t");
    }
    for tenant in ["ta", "tb"] {
        for table in ["t", "t2"] {
            assert_eq!(
                state(&path, tenant, table),
                RowSecurityState::default(),
                "{tenant}/{table}"
            );
        }
    }
}

#[test]
fn rename_moves_state_to_the_new_name() {
    let (_g, path, core) = open_fixture("rls13-rename");
    let mut a = allowed_session();
    ok_as(&core, "ta", &mut a, FOUR[0]);
    ok_as(&core, "ta", &mut a, "ALTER TABLE t RENAME TO t2");
    drop(core);
    assert!(state(&path, "ta", "t2").enabled);
    assert_eq!(state(&path, "ta", "t"), RowSecurityState::default());
}

/// 永続化: reopen の後も各テナントの状態が保たれる。
#[test]
fn state_persists_across_reopen() {
    let (_g, path, core) = open_fixture("rls13-persist");
    let mut a = allowed_session();
    let mut b = allowed_session();
    ok_as(&core, "ta", &mut a, FOUR[0]);
    ok_as(&core, "tb", &mut b, FOUR[2]);
    drop(core);
    assert_eq!(
        state(&path, "ta", "t"),
        RowSecurityState {
            enabled: true,
            forced: false
        }
    );
    assert_eq!(
        state(&path, "tb", "t"),
        RowSecurityState {
            enabled: false,
            forced: true
        }
    );
}

/// 状態を切り替えてもテナント境界による可視性（他テナントの行が見えない）は変わらない。
#[test]
fn tenant_boundary_is_unchanged_by_any_state() {
    let (_g, _p, core) = open_fixture("rls13-boundary");
    let mut a = allowed_session();
    let mut b = allowed_session();
    ok_as(
        &core,
        "ta",
        &mut a,
        "INSERT INTO t (id, a) VALUES (1, 'from-a') USING OPERATION_ID 'op-a'",
    );
    ok_as(
        &core,
        "tb",
        &mut b,
        "INSERT INTO t (id, a) VALUES (2, 'from-b') USING OPERATION_ID 'op-b'",
    );
    let count = |core: &EngineCore, tenant: &str, s: &mut SessionState| -> Vec<String> {
        match ok_as(core, tenant, s, "SELECT a FROM t") {
            SqlOutcome::Query(r) => r.rows.iter().map(|row| format!("{row:?}")).collect(),
            other => panic!("expected Query, got {other:?}"),
        }
    };
    let base_a = count(&core, "ta", &mut a);
    assert_eq!(base_a.len(), 1);
    assert!(base_a[0].contains("from-a") && !base_a[0].contains("from-b"));
    for sql in FOUR {
        ok_as(&core, "ta", &mut a, sql);
        let rows = count(&core, "ta", &mut a);
        assert_eq!(rows.len(), 1, "{sql}");
        assert!(!rows[0].contains("from-b"), "{sql}");
        let rows_b = count(&core, "tb", &mut b);
        assert_eq!(rows_b.len(), 1, "{sql}");
        assert!(!rows_b[0].contains("from-a"), "{sql}");
    }
}
