//! `GRANT`／`REVOKE`（SQL-40・Issue #1965）の SQL 水準の結合テスト。設計記録:
//! `docs/design/role-privilege-model.md` §8。
//!
//! 公開 API だけで固定できる契約を扱う: 現在ロールが未束縛（現状の全セッション）なら対象・ロールの有無を
//! 問わず常に同じ `42501`（存在オラクルの防止）、構文エラーは `42601`、範囲外の形は `0A000`、明示トランザクション内は
//! `0A000`（未束縛は `42501` が先）、束縛ロールが実在しなければ `42501`。ロールを作る `Storage::create_role` は
//! crate 内限定のため、付与者の判定・REVOKE の連鎖は `sql::grant_ddl` の単体テストが固定する。

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

fn open_fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![ColumnDef::new("a", ColumnType::Text, true)],
        ))
        .expect("create table");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn err_of(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> SqlSurfaceError {
    match run(core, tenant, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

/// 受入: 権限の無い GRANT は `42501`。未束縛のセッションには、存在する表・存在しない表・存在しない
/// ロールのどれでも同じ SQLSTATE・同じ文言（対象の有無を示さない）。DDL 許可だけでも変わらない。
#[test]
fn unbound_session_gets_identical_42501_regardless_of_target() {
    let (_g, core) = open_fixture("sql40_unbound");
    for with_ddl in [false, true] {
        let mut s = SessionState::default();
        if with_ddl {
            s.allow_ddl();
        }
        let mut seen = Vec::new();
        for sql in [
            "GRANT SELECT ON t TO alice",
            "GRANT SELECT ON missing TO alice",
            "GRANT SELECT (zzz) ON t TO alice",
            "REVOKE SELECT ON t FROM alice",
            "REVOKE ALL ON missing FROM PUBLIC CASCADE",
            "GRANT r1 TO r2",
        ] {
            let e = err_of(&core, "t1", &mut s, sql);
            assert_eq!(e.wire_code(), "42501", "{sql}");
            seen.push(e.client_message());
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    }
}

/// 他テナントのスキーマで修飾した表は、存在しないスキーマと同じ応答（解決段で `3F000`。権限の有無に
/// よらず同一で、他テナントのスキーマの存在を示さない）。
#[test]
fn other_tenant_schema_is_indistinguishable_from_missing() {
    let (_g, core) = open_fixture("sql40_schema_oracle");
    let mut owner = SessionState::default();
    owner.allow_ddl();
    run(&core, "t2", &mut owner, "CREATE SCHEMA s_other").expect("schema");
    let mut s = SessionState::default();
    let a = err_of(&core, "t1", &mut s, "GRANT SELECT ON s_other.t TO alice");
    let b = err_of(&core, "t1", &mut s, "GRANT SELECT ON s_nowhere.t TO alice");
    assert_eq!(a.wire_code(), b.wire_code());
    assert_eq!(
        a.client_message().replace("s_other", "X"),
        b.client_message().replace("s_nowhere", "X")
    );
}

/// 構文エラーは `42601`、範囲外の形は `0A000`（束縛済みのセッションでは構文の判定が先）。
#[test]
fn syntax_and_scope_errors_surface_through_the_session_api() {
    let (_g, core) = open_fixture("sql40_syntax");
    let mut s = SessionState::default();
    for sql in [
        "GRANT SELECT ON t",
        "GRANT FROBNICATE ON t TO a",
        "REVOKE SELECT ON t TO a",
    ] {
        assert_eq!(
            err_of(&core, "t1", &mut s, sql).wire_code(),
            "42601",
            "{sql}"
        );
    }
    assert_eq!(
        err_of(&core, "t1", &mut s, "GRANT USAGE ON TABLE t TO a").wire_code(),
        "0LP01"
    );
    // 範囲外の形は状態に依存しない拒否（権限の有無・対象の有無に関わらず同じ `0A000`）。
    for sql in [
        "GRANT USAGE ON SCHEMA public TO a",
        "GRANT SELECT ON ALL TABLES IN SCHEMA public TO a",
        "GRANT SELECT ON t TO a GRANTED BY b",
    ] {
        assert_eq!(
            err_of(&core, "t1", &mut s, sql).wire_code(),
            "0A000",
            "{sql}"
        );
    }
    assert_eq!(
        err_of(
            &core,
            "t1",
            &mut s,
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO a"
        )
        .wire_code(),
        "0A000"
    );
}

/// 束縛ロールが実在しない（DROP 済み・偽の ID）なら対象の有無を問わず `42501`。
#[test]
fn bound_but_nonexistent_role_is_denied() {
    let (_g, core) = open_fixture("sql40_bound_missing");
    let mut s = SessionState::default();
    s.bind_role_ids(4242, 4242);
    s.allow_ddl();
    for sql in [
        "GRANT SELECT ON t TO PUBLIC",
        "GRANT SELECT ON missing TO PUBLIC",
    ] {
        let e = err_of(&core, "t1", &mut s, sql);
        // 対象の解決より前に付与者を確認するため、存在しない表でも `42501`。
        assert_eq!(e.wire_code(), "42501", "{sql}");
    }
}

/// 明示トランザクション内は `0A000`（ACL の書き込みは ROLLBACK で取り消せない）。未束縛は `42501` が先。
#[test]
fn explicit_transaction_is_rejected_after_the_principal_gate() {
    let (_g, core) = open_fixture("sql40_txn");
    for (bound, expected) in [(false, "42501"), (true, "0A000")] {
        for sql in [
            "GRANT SELECT ON t TO alice",
            "REVOKE SELECT ON missing FROM alice",
        ] {
            // 失敗したトランザクションは `25P02` になるため、文ごとに新しい Tx を使う。
            let mut s = SessionState::default();
            if bound {
                s.bind_role_ids(1, 1);
            }
            let mut txn = core.new_session_transaction();
            core.execute_sql_in_txn(&ctx("t1"), &mut s, &mut txn, "BEGIN")
                .expect("begin");
            let e = core
                .execute_sql_in_txn(&ctx("t1"), &mut s, &mut txn, sql)
                .expect_err("rejected");
            assert_eq!(e.wire_code(), expected, "{sql}");
            core.execute_sql_in_txn(&ctx("t1"), &mut s, &mut txn, "ROLLBACK")
                .expect("rollback");
        }
    }
}
