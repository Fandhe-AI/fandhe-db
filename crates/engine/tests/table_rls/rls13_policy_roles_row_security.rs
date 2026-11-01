//! セッション変数 `row_security` と利用者ポリシー適用の判定（RLS-13・Issue #2278）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/rls.md` RLS-12・RLS-13、設計記録は
//! `docs/design/row-security-policy-composition.md`（#2278 の節）。
//!
//! `table27_search_path.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。検証する契約:
//!
//! - 既定 on・`SET`／`SHOW`／`RESET`・`DEFAULT`・不正値 `22023`・`SET LOCAL` の `0A000`
//! - `row_security = off` でも他テナントの行は読めない（テナント境界は利用者ポリシーと独立）

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger),
        guard,
    )
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
}

fn code(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(tenant), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn show(core: &EngineCore, s: &mut SessionState) -> String {
    match ok(core, "alice", s, "SHOW row_security") {
        SqlOutcome::Show(r) => match &r.rows[0].cells[0] {
            Cell::Text(t) => t.clone(),
            other => panic!("expected text cell, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

/// RLS-13: 既定 on・SET／SHOW／RESET の往復と受理値。
#[test]
fn rls13_row_security_set_show_reset_roundtrip() {
    let (core, _g) = new_core("rls13-rs-roundtrip");
    let mut s = SessionState::default();
    assert_eq!(show(&core, &mut s), "on");
    ok(&core, "alice", &mut s, "SET row_security = off");
    assert_eq!(show(&core, &mut s), "off");
    assert_eq!(
        ok(&core, "alice", &mut s, "RESET row_security"),
        SqlOutcome::SetRowSecurity { reset: true }
    );
    assert_eq!(show(&core, &mut s), "on");
    for (sql, want) in [
        ("SET SESSION row_security TO false", "off"),
        ("SET row_security TO 'on'", "on"),
        ("SET row_security = NO", "off"),
        ("SET row_security = 1", "on"),
        ("SET row_security = 0", "off"),
        ("SET row_security TO DEFAULT", "on"),
    ] {
        ok(&core, "alice", &mut s, sql);
        assert_eq!(show(&core, &mut s), want, "{sql}");
    }
}

/// RLS-13: 不正値は `22023`、`SET LOCAL` は `0A000`。失敗した SET は値を変えない。
#[test]
fn rls13_row_security_rejects_invalid_values() {
    let (core, _g) = new_core("rls13-rs-invalid");
    let mut s = SessionState::default();
    ok(&core, "alice", &mut s, "SET row_security = off");
    assert_eq!(
        code(&core, "alice", &mut s, "SET row_security = maybe"),
        "22023"
    );
    assert_eq!(
        code(&core, "alice", &mut s, "SET row_security = 2"),
        "22023"
    );
    assert_eq!(
        code(&core, "alice", &mut s, "SET LOCAL row_security = on"),
        "0A000"
    );
    assert_eq!(show(&core, &mut s), "off");
}

/// RLS-12・RLS-13: `row_security = off` でも他テナントの行は読めない（利用者ポリシーの適用可否は
/// テナント境界に作用しない）。
#[test]
fn rls13_row_security_off_never_widens_tenant_boundary() {
    let (core, _g) = new_core("rls13-rs-boundary");
    let mut alice = SessionState::default();
    alice.allow_ddl();
    ok(
        &core,
        "alice",
        &mut alice,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        "alice",
        &mut alice,
        "INSERT INTO t (id, name) VALUES (1, 'a')",
    );
    let mut bob = SessionState::default();
    ok(&core, "bob", &mut bob, "SET row_security = off");
    let rows = match core.execute_sql_in_session(&ctx("bob"), &mut bob, "SELECT * FROM t") {
        Ok(SqlOutcome::Query(r)) => r.rows.len(),
        // 表が bob から見えない場合は、不在と区別できない拒否（存在秘匿）でよい。
        Err(_) => 0,
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(rows, 0, "off must not expose another tenant's rows");
}

/// RLS-13: RLS 有効な表では、`WITH CHECK (true)` のポリシーがあっても `row_security = off` の INSERT は
/// 行を書く前に `42501` で拒否され、`on` へ戻すと成功する。RLS 無効な表は off の影響を受けない。
#[test]
fn rls13_row_security_off_rejects_write_to_rls_table_before_writing() {
    let (core, _g) = new_core("rls13-rs-off-write");
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
        "CREATE TABLE plain (id BIGINT PRIMARY KEY, name TEXT)",
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY p ON t FOR ALL USING (true) WITH CHECK (true)",
    ] {
        ok(&core, "alice", &mut s, sql);
    }
    ok(&core, "alice", &mut s, "SET row_security = off");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, name) VALUES (1, 'a')"
        ),
        "42501"
    );
    // RLS 無効な表は影響を受けない。
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO plain (id, name) VALUES (1, 'a')",
    );
    ok(&core, "alice", &mut s, "SET row_security = on");
    // 拒否された文は何も残さない（on の INSERT が主キー衝突しない = 先の文は書かれていない）。
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, name) VALUES (1, 'a')",
    );
}

/// RLS-13: 対象行が 0 件の書き込み（`WHERE false` の UPDATE・0 行の INSERT SELECT）でも、RLS 有効な表への
/// `row_security = off` は文の開始時点で `42501` になる（結果が件数に依存しない）。RLS 無効な表は成功する。
#[test]
fn rls13_row_security_off_rejects_zero_row_writes() {
    let (core, _g) = new_core("rls13-rs-off-empty");
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
        "CREATE TABLE plain (id BIGINT PRIMARY KEY, name TEXT)",
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY p ON t FOR ALL USING (true) WITH CHECK (true)",
    ] {
        ok(&core, "alice", &mut s, sql);
    }
    ok(&core, "alice", &mut s, "SET row_security = off");
    for sql in [
        "UPDATE t SET name = 'x' WHERE false",
        "UPDATE t SET name = 'x' WHERE id = 99",
        "INSERT INTO t (id, name) SELECT id, name FROM plain WHERE false",
    ] {
        assert_eq!(code(&core, "alice", &mut s, sql), "42501", "{sql}");
    }
    ok(
        &core,
        "alice",
        &mut s,
        "UPDATE plain SET name = 'x' WHERE false",
    );
}
