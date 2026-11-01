//! `CREATE POLICY` / `ALTER POLICY` / `DROP POLICY`（RLS-13・Issue #2274）の SQL 水準の結合テスト。
//! ポインタ: `docs/spec/04-behavior/rls.md` の RLS-13、設計記録は
//! `docs/design/row-security-policy-composition.md`。
//!
//! 固定する契約: 権限ゲートがカタログ照会より前にあること（`42501`）、PG 17 と同じ SQLSTATE
//! （`42710`・`42704`・`42P01`・`42809`・`42703`・`42804`・`42601`・`0A000`）、テナント境界（同名のポリシーを
//! テナントごとに独立に持てる・他テナントのポリシーは不在と同じ応答）、表のライフサイクルへの追随、
//! 再起動後も定義が残ること。ポリシーの適用（行の可視性への反映）は後続 Issue の担当で、ここでは扱わない。

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

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn fixture_storage(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("b", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        ))
        .expect("create table");
    storage
}

fn open_fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = fixture_storage(&path);
    (
        guard,
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

/// 権限の無い主体には、対象表・ポリシーの有無を問わず常に `42501`（存在オラクルにしない）。
#[test]
fn ddl_permission_gate_precedes_every_catalog_lookup() {
    let (_g, core) = open_fixture("pol-perm");
    let mut owner = allowed_session();
    ok_as(
        &core,
        "alice",
        &mut owner,
        "CREATE POLICY p ON t USING (a = 'x')",
    );
    let mut s = SessionState::default();
    for sql in [
        "CREATE POLICY p ON t USING (a = 'x')",
        "CREATE POLICY q ON t",
        "CREATE POLICY p ON nosuch",
        "ALTER POLICY p ON t RENAME TO q",
        "ALTER POLICY nosuch ON t USING (a = 'x')",
        "ALTER POLICY p ON nosuch RENAME TO q",
        "DROP POLICY p ON t",
        "DROP POLICY IF EXISTS nosuch ON t",
        "DROP POLICY IF EXISTS p ON nosuch",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "42501", "{sql}");
        assert_eq!(code_as(&core, "bob", &mut s, sql), "42501", "{sql}");
    }
}

#[test]
fn create_alter_drop_succeed_and_alter_keeps_unspecified_clauses() {
    let (_g, core) = open_fixture("pol-ok");
    let mut s = allowed_session();
    assert!(matches!(
        ok_as(
            &core,
            "alice",
            &mut s,
            "CREATE POLICY p ON t AS RESTRICTIVE FOR UPDATE TO PUBLIC \
             USING (a = 'x') WITH CHECK (n > 1)"
        ),
        SqlOutcome::CreatePolicy(_)
    ));
    for sql in [
        "CREATE POLICY q1 ON t",
        "CREATE POLICY q2 ON t FOR SELECT USING (flag)",
        "CREATE POLICY q3 ON t FOR INSERT WITH CHECK (b IS NOT NULL)",
        "CREATE POLICY q4 ON t FOR DELETE USING (n > 1 OR a = 'y')",
    ] {
        ok_as(&core, "alice", &mut s, sql);
    }
    for sql in [
        "ALTER POLICY p ON t USING (b = 'z')",
        "ALTER POLICY p ON t WITH CHECK (n > 5)",
        "ALTER POLICY p ON t TO PUBLIC",
        "ALTER POLICY p ON t",
        "ALTER POLICY p ON t RENAME TO p2",
    ] {
        assert!(matches!(
            ok_as(&core, "alice", &mut s, sql),
            SqlOutcome::AlterPolicy(_)
        ));
    }
    // 改名で旧名は不在、新名は存在する。
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER POLICY p ON t TO PUBLIC"),
        "42704"
    );
    ok_as(&core, "alice", &mut s, "ALTER POLICY p2 ON t TO PUBLIC");
    match ok_as(&core, "alice", &mut s, "DROP POLICY p2 ON t") {
        SqlOutcome::DropPolicy(o) => assert!(!o.skipped()),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        code_as(&core, "alice", &mut s, "DROP POLICY p2 ON t"),
        "42704"
    );
}

#[test]
fn sqlstates_follow_postgresql() {
    let (_g, core) = open_fixture("pol-sqlstate");
    let mut s = allowed_session();
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY p ON t USING (a = 'x')",
    );
    ok_as(&core, "alice", &mut s, "CREATE POLICY q ON t");
    ok_as(&core, "alice", &mut s, "CREATE VIEW v AS SELECT a FROM t");
    for (sql, want) in [
        // 重複・不在。
        ("CREATE POLICY p ON t", "42710"),
        ("ALTER POLICY p ON t RENAME TO q", "42710"),
        ("ALTER POLICY nosuch ON t RENAME TO r", "42704"),
        ("ALTER POLICY nosuch ON t USING (a = 'x')", "42704"),
        ("DROP POLICY nosuch ON t", "42704"),
        // 表の不在・種別違い。
        ("CREATE POLICY r ON nosuch", "42P01"),
        ("ALTER POLICY p ON nosuch RENAME TO r", "42P01"),
        ("DROP POLICY p ON nosuch", "42P01"),
        ("CREATE POLICY r ON v", "42809"),
        ("DROP POLICY p ON v", "42809"),
        ("DROP POLICY IF EXISTS p ON v", "42809"),
        // 式の束縛。
        ("CREATE POLICY r ON t USING (nope = 'x')", "42703"),
        ("CREATE POLICY r ON t USING (n)", "42804"),
        ("CREATE POLICY r ON t USING (a)", "42804"),
        ("CREATE POLICY r ON t USING (1)", "42804"),
        ("CREATE POLICY r ON t USING ('x')", "42804"),
        ("CREATE POLICY r ON t USING (n + 1)", "42804"),
        ("ALTER POLICY p ON t USING (n)", "42804"),
        ("CREATE POLICY r ON t USING (tenant_id = 'x')", "42601"),
        ("CREATE POLICY r ON t USING (visibility = 'x')", "42601"),
        ("CREATE POLICY r ON t USING (visible())", "42601"),
        // FOR と句の矛盾。
        (
            "CREATE POLICY r ON t FOR SELECT WITH CHECK (n > 1)",
            "42601",
        ),
        (
            "CREATE POLICY r ON t FOR DELETE WITH CHECK (n > 1)",
            "42601",
        ),
        ("CREATE POLICY r ON t FOR INSERT USING (n > 1)", "42601"),
        // 未対応の形（ロール機構は後続）。
        ("CREATE POLICY r ON t TO alice", "0A000"),
        ("CREATE POLICY r ON t TO CURRENT_USER", "0A000"),
        ("ALTER POLICY p ON t TO alice", "0A000"),
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), want, "{sql}");
    }
    // 部分クエリは拒否される（SQLSTATE は構文段の 42601 か束縛段の 0A000）。
    let c = code_as(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY r ON t USING (n IN (SELECT n FROM t))",
    );
    assert!(c == "0A000" || c == "42601", "{c}");
    // 失敗した作成は何も残さない。
    ok_as(&core, "alice", &mut s, "CREATE POLICY r ON t");
    // IF EXISTS は不在でも成功（表の不在を含む）。
    for sql in [
        "DROP POLICY IF EXISTS nosuch ON t",
        "DROP POLICY IF EXISTS p ON nosuch",
    ] {
        match ok_as(&core, "alice", &mut s, sql) {
            SqlOutcome::DropPolicy(o) => assert!(o.skipped(), "{sql}"),
            other => panic!("{other:?}"),
        }
    }
}

/// ALTER で `FOR` と矛盾する句を足す形は、既存ポリシーの `FOR` を読んだ実行段で `42601`。
#[test]
fn alter_clause_must_agree_with_the_policy_command() {
    let (_g, core) = open_fixture("pol-alter-for");
    let mut s = allowed_session();
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY sel ON t FOR SELECT USING (a = 'x')",
    );
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY ins ON t FOR INSERT WITH CHECK (a = 'x')",
    );
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "ALTER POLICY sel ON t WITH CHECK (a = 'y')"
        ),
        "42601"
    );
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "ALTER POLICY ins ON t USING (a = 'y')"
        ),
        "42601"
    );
    ok_as(
        &core,
        "alice",
        &mut s,
        "ALTER POLICY sel ON t USING (a = 'y')",
    );
    ok_as(
        &core,
        "alice",
        &mut s,
        "ALTER POLICY ins ON t WITH CHECK (a = 'y')",
    );
}

/// テナント境界: 同じ共有表に同名のポリシーを各テナントが独立に持て、他テナントのポリシーは
/// 「本当に無い」場合と同一の応答になる。
#[test]
fn policies_are_scoped_per_tenant() {
    let (_g, core) = open_fixture("pol-tenant");
    let mut s = allowed_session();
    for tenant in ["alice", "bob", "carol"] {
        ok_as(
            &core,
            tenant,
            &mut s,
            "CREATE POLICY p ON t USING (a = 'x')",
        );
    }
    ok_as(&core, "alice", &mut s, "CREATE POLICY only_alice ON t");
    // bob から alice 専用のポリシーは「無い」。ない場合の応答と同一。
    for sql in [
        "ALTER POLICY only_alice ON t RENAME TO z",
        "ALTER POLICY only_alice ON t USING (a = 'y')",
        "DROP POLICY only_alice ON t",
    ] {
        assert_eq!(code_as(&core, "bob", &mut s, sql), "42704", "{sql}");
        let absent = sql.replace("only_alice", "never_existed");
        assert_eq!(code_as(&core, "bob", &mut s, &absent), "42704", "{absent}");
    }
    // bob の IF EXISTS は alice のポリシーを消さず、no-op として成功する。
    match ok_as(
        &core,
        "bob",
        &mut s,
        "DROP POLICY IF EXISTS only_alice ON t",
    ) {
        SqlOutcome::DropPolicy(o) => assert!(o.skipped()),
        other => panic!("{other:?}"),
    }
    // alice のポリシーは健在（重複作成が 42710）。
    assert_eq!(
        code_as(&core, "alice", &mut s, "CREATE POLICY only_alice ON t"),
        "42710"
    );
    // bob の p を消しても alice・carol の p は残る。
    ok_as(&core, "bob", &mut s, "DROP POLICY p ON t");
    for tenant in ["alice", "carol"] {
        assert_eq!(
            code_as(&core, tenant, &mut s, "CREATE POLICY p ON t"),
            "42710"
        );
    }
}

#[test]
fn table_lifecycle_is_followed() {
    let (_g, core) = open_fixture("pol-lifecycle");
    let mut s = allowed_session();
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY p ON t USING (a = 'x')",
    );
    // 参照列の DROP は 2BP01、改名・型変更は 0A000。参照されない列は操作できる。
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE t DROP COLUMN a"),
        "2BP01"
    );
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE t RENAME COLUMN a TO c"),
        "0A000"
    );
    // 型変更は、変換自体は成立する列（INTEGER → BIGINT）でもポリシーの参照があれば拒否する。
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY pn ON t USING (n > 1)",
    );
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "ALTER TABLE t ALTER COLUMN n TYPE BIGINT"
        ),
        "0A000"
    );
    ok_as(&core, "alice", &mut s, "DROP POLICY pn ON t");
    ok_as(&core, "alice", &mut s, "ALTER TABLE t DROP COLUMN b");
    // 表の改名にポリシーが追随する。
    ok_as(&core, "alice", &mut s, "ALTER TABLE t RENAME TO t2");
    assert_eq!(
        code_as(&core, "alice", &mut s, "CREATE POLICY p ON t2"),
        "42710"
    );
    ok_as(&core, "alice", &mut s, "DROP POLICY p ON t2");
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE POLICY p ON t2 USING (a = 'x')",
    );
    // DROP TABLE の後に同名の表を作り直しても旧ポリシーは復活しない。
    ok_as(&core, "alice", &mut s, "DROP TABLE t2");
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE TABLE t2 (a TEXT, n INTEGER)",
    );
    ok_as(&core, "alice", &mut s, "CREATE POLICY p ON t2");
}

#[test]
fn explicit_transaction_rejects_policy_ddl() {
    let (_g, core) = open_fixture("pol-tx");
    let mut allowed = allowed_session();
    for sql in [
        "CREATE POLICY p ON t",
        "ALTER POLICY p ON t RENAME TO q",
        "DROP POLICY p ON t",
    ] {
        // 権限ありは 0A000。
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut allowed, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut allowed, &mut txn, sql)
            .expect_err("policy DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        core.execute_sql_in_txn(&ctx("alice"), &mut allowed, &mut txn, "ROLLBACK")
            .expect("rollback");
        // 権限なしは 42501（対象の有無を問わない）。
        let mut denied = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut denied, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut denied, &mut txn, sql)
            .expect_err("denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
        core.execute_sql_in_txn(&ctx("alice"), &mut denied, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

/// 受入: 定義・変更・削除が再起動後も保たれる（同じ DB パスを開き直して観測する）。
#[test]
fn definitions_survive_restart() {
    let path = unique_db_path("pol-restart");
    let _guard = CleanupGuard(path.clone());
    let mut s = allowed_session();
    {
        let core = EngineCore::from_storage(fixture_storage(&path), Box::new(CpuScalarProvider));
        ok_as(
            &core,
            "alice",
            &mut s,
            "CREATE POLICY keep ON t USING (a = 'x')",
        );
        ok_as(&core, "alice", &mut s, "CREATE POLICY old ON t");
        ok_as(&core, "alice", &mut s, "CREATE POLICY gone ON t");
        ok_as(
            &core,
            "alice",
            &mut s,
            "ALTER POLICY old ON t RENAME TO renamed",
        );
        ok_as(
            &core,
            "alice",
            &mut s,
            "ALTER POLICY keep ON t USING (b = 'y')",
        );
        ok_as(&core, "alice", &mut s, "DROP POLICY gone ON t");
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for name in ["keep", "renamed"] {
        assert_eq!(
            code_as(
                &core,
                "alice",
                &mut s,
                &format!("CREATE POLICY {name} ON t")
            ),
            "42710",
            "{name}"
        );
    }
    for name in ["old", "gone"] {
        assert_eq!(
            code_as(&core, "alice", &mut s, &format!("DROP POLICY {name} ON t")),
            "42704",
            "{name}"
        );
    }
    // 他テナントには見えない。
    ok_as(&core, "bob", &mut s, "CREATE POLICY keep ON t");
}
