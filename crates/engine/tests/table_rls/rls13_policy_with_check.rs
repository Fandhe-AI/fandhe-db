//! 利用者定義ポリシーの `WITH CHECK` の書き込みへの適用（RLS-13・Issue #2277）の SQL 水準の結合テスト。
//! ポインタ: `docs/spec/04-behavior/rls.md` の RLS-13、設計記録は
//! `docs/design/row-security-policy-composition.md`。
//!
//! 対象は `INSERT`・`UPDATE`・`INSERT ... ON CONFLICT DO UPDATE`（挿入側・更新側・既存行）・
//! `INSERT ... SELECT`。違反は `42501` で文全体を拒否し、何も書かれないこと（原子性）を固定する。
//! 所有者・`BYPASSRLS`（#2278）と対象行の選択への `USING`（#2276）は本 Issue の対象外のため、
//! 各テストは `ENABLE` と `FORCE` を両方掛け、`UPDATE` の対象行が `USING` を満たす条件にして、
//! それらが入っても期待値が変わらないようにしている。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn open_fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("owner", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create t");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    run(core, tenant, s, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

/// 失敗する文の (SQLSTATE, 文言) を返す。
fn fail(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> (&'static str, String) {
    match run(core, tenant, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code(), e.to_string()),
    }
}

/// 自テナントの `owner` 列の値を id 昇順で返す。
fn row_ids(core: &EngineCore, tenant: &str) -> Vec<String> {
    let mut s = session();
    match run(
        core,
        tenant,
        &mut s,
        "SELECT id, owner FROM t ORDER BY id LIMIT 100",
    )
    .expect("select")
    {
        SqlOutcome::Query(q) => q
            .rows
            .iter()
            .map(|r| format!("{}", r.id))
            .collect::<Vec<_>>(),
        other => panic!("unexpected {other:?}"),
    }
}

/// ENABLE と FORCE を掛け、`owner = 'a'` を要求する INSERT・UPDATE 用の PERMISSIVE を 1 つ作る。
fn setup_with_policy(label: &str, policy_sql: &str) -> (CleanupGuard, EngineCore, SessionState) {
    let (g, core) = open_fixture(label);
    let mut s = session();
    ok(
        &core,
        "alice",
        &mut s,
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "ALTER TABLE t FORCE ROW LEVEL SECURITY",
    );
    ok(&core, "alice", &mut s, policy_sql);
    (g, core, s)
}

#[test]
fn insert_violation_is_42501_and_writes_nothing() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-insert",
        "CREATE POLICY p ON t FOR INSERT WITH CHECK (owner = 'a')",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 'i1'",
    );
    let (code, msg) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (2, 'b', 2) USING OPERATION_ID 'i2'",
    );
    assert_eq!(code, "42501");
    assert_eq!(
        msg,
        "new row violates row-level security policy for table \"t\""
    );
    assert_eq!(row_ids(&core, "alice"), vec!["1"]);
}

#[test]
fn multi_row_insert_with_one_violation_writes_no_rows() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-multi",
        "CREATE POLICY p ON t FOR INSERT WITH CHECK (owner = 'a')",
    );
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1), (2, 'b', 2) USING OPERATION_ID 'm1'",
    );
    assert_eq!(code, "42501");
    assert!(row_ids(&core, "alice").is_empty());
}

#[test]
fn enabled_table_without_insert_policy_denies_by_default() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-default-deny",
        "CREATE POLICY p ON t FOR SELECT USING (owner = 'a')",
    );
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 'd1'",
    );
    assert_eq!(code, "42501");
    assert!(row_ids(&core, "alice").is_empty());
}

#[test]
fn update_with_check_violation_leaves_row_unchanged() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-update",
        "CREATE POLICY p ON t FOR ALL USING (true) WITH CHECK (n < 10)",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 'u-i1'",
    );
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "UPDATE t SET n = 50 WHERE id = 1 USING OPERATION_ID 'u-u1'",
    );
    assert_eq!(code, "42501");
    ok(
        &core,
        "alice",
        &mut s,
        "UPDATE t SET n = 5 WHERE id = 1 USING OPERATION_ID 'u-u2'",
    );
}

#[test]
fn using_substitutes_for_missing_with_check_on_all_policy() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-substitute",
        "CREATE POLICY p ON t FOR ALL USING (n < 10)",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 's-i1'",
    );
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (2, 'a', 99) USING OPERATION_ID 's-i2'",
    );
    assert_eq!(code, "42501");
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "UPDATE t SET n = 99 WHERE id = 1 USING OPERATION_ID 's-u1'",
    );
    assert_eq!(code, "42501");
}

#[test]
fn restrictive_violation_names_the_policy() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-restrictive",
        "CREATE POLICY p ON t FOR INSERT WITH CHECK (true)",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY r ON t AS RESTRICTIVE FOR INSERT WITH CHECK (n < 10)",
    );
    let (code, msg) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 99) USING OPERATION_ID 'r1'",
    );
    assert_eq!(code, "42501");
    assert_eq!(
        msg,
        "new row violates row-level security policy \"r\" for table \"t\""
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 'r2'",
    );
}

#[test]
fn on_conflict_checks_insert_side_update_side_and_existing_row() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-conflict",
        "CREATE POLICY p ON t FOR ALL USING (owner = 'a') WITH CHECK (n < 10)",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 'c-i1'",
    );
    // 挿入側（新規行）の違反。
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (2, 'a', 99) ON CONFLICT (id) DO UPDATE SET n = 1 \
         USING OPERATION_ID 'c-i2'",
    );
    assert_eq!(code, "42501");
    // 更新側の新しい行の違反（提案行は WITH CHECK を満たし、更新後の行が満たさない）。
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 5) ON CONFLICT (id) DO UPDATE SET n = 99 \
         USING OPERATION_ID 'c-i3'",
    );
    assert_eq!(code, "42501");
    // 提案行の違反は衝突して更新へ回る行でも拒否される。
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 99) ON CONFLICT (id) DO UPDATE SET n = 1 \
         USING OPERATION_ID 'c-i4'",
    );
    assert_eq!(code, "42501");
    assert_eq!(row_ids(&core, "alice"), vec!["1"]);
    // 全て満たせば成功する。
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 5) ON CONFLICT (id) DO UPDATE SET n = 7 \
         USING OPERATION_ID 'c-i5'",
    );
}

#[test]
fn on_conflict_existing_row_must_satisfy_using() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-conflict-using",
        "CREATE POLICY ins ON t FOR INSERT WITH CHECK (true)",
    );
    // UPDATE・SELECT の USING を持つポリシーが無いため、既存行の検査は既定拒否になる。
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) USING OPERATION_ID 'cu-i1'",
    );
    let (code, msg) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'a', 1) ON CONFLICT (id) DO UPDATE SET n = 2 \
         USING OPERATION_ID 'cu-i2'",
    );
    assert_eq!(code, "42501");
    assert_eq!(
        msg,
        "new row violates row-level security policy (USING expression) for table \"t\""
    );
}

#[test]
fn disabling_row_security_stops_checks_and_drop_policy_restores_default_deny() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-state",
        "CREATE POLICY p ON t FOR INSERT WITH CHECK (owner = 'a')",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "ALTER TABLE t DISABLE ROW LEVEL SECURITY",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'b', 1) USING OPERATION_ID 'st-i1'",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
    );
    ok(&core, "alice", &mut s, "DROP POLICY p ON t");
    let (code, _) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (2, 'a', 1) USING OPERATION_ID 'st-i2'",
    );
    assert_eq!(code, "42501");
}

#[test]
fn policies_and_state_are_independent_per_tenant() {
    let (_g, core, mut s) = setup_with_policy(
        "rls13wc-tenants",
        "CREATE POLICY p ON t FOR INSERT WITH CHECK (owner = 'a')",
    );
    // bob のテーブルには ENABLE もポリシーも無い。alice の設定は bob の書き込みに影響しない。
    let mut sb = session();
    ok(
        &core,
        "bob",
        &mut sb,
        "INSERT INTO t (id, owner, n) VALUES (1, 'zzz', 1) USING OPERATION_ID 'b-i1'",
    );
    let (code, msg) = fail(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, owner, n) VALUES (1, 'zzz', 1) USING OPERATION_ID 'a-i1'",
    );
    assert_eq!(code, "42501");
    assert!(!msg.contains("zzz") && !msg.contains("bob"), "{msg}");
    assert!(row_ids(&core, "alice").is_empty());
    assert_eq!(row_ids(&core, "bob"), vec!["1"]);
}
