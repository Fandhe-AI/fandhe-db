//! ビューへの INSTEAD OF ROW トリガの結合テスト（TABLE-45・TABLE-43・Issue #3193。設計記録:
//! `docs/design/trigger-instead-of.md`）。
//!
//! 既定の本番 invoker のコアで、SQL の `CREATE FUNCTION ... RETURNS trigger LANGUAGE plpgsql`・
//! `CREATE TRIGGER ... INSTEAD OF`・ビューへの `INSERT`／`UPDATE`／`DELETE` だけで発火を通す。
//! 期待値は PostgreSQL 17 の INSTEAD OF トリガの挙動（`NULL` 返却は処理済みに数えない・複数トリガは
//! 名前順に連鎖・エラーで文全体を取り消す）に、本実装の fail-closed な拒否を加えたもの。

use super::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

pub(super) fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

pub(super) fn core_with(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::Optional);
    (core, CleanupGuard(path))
}

pub(super) fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

pub(super) fn run(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = session();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

pub(super) fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

pub(super) fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

pub(super) fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(q) => q.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("expected query, got {other:?}"),
    }
}

pub(super) fn ints(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = rows(core, tenant, sql)
        .into_iter()
        .map(|r| match &r[0] {
            Cell::SignedInteger(v) => *v,
            Cell::Integer(v) => i64::try_from(*v).expect("fits"),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

/// 影響行数（`INSERT`／`UPDATE`／`DELETE` の応答）。
pub(super) fn affected(out: SqlOutcome) -> u64 {
    match out {
        SqlOutcome::Insert(o) => o.rows_affected,
        SqlOutcome::Update(o) => o.rows_affected,
        SqlOutcome::Delete(o) => o.rows_affected,
        other => panic!("expected a DML outcome, got {other:?}"),
    }
}

pub(super) fn count(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    affected(run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")))
}

/// SCHEMA `s` の表 `s.tt (n)`・public の監査表 `audit (tag, n)`、`s.tt` の非更新ビュー `s.nv`
/// （`LIMIT` 付き）と更新可能ビュー `s.uv` を作る。
pub(super) fn setup(core: &EngineCore) {
    ok(core, "t1", "CREATE SCHEMA s");
    ok(core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(core, "t1", "CREATE TABLE audit (tag TEXT, n INTEGER)");
    ok(
        core,
        "t1",
        "CREATE VIEW s.nv AS SELECT n FROM s.tt LIMIT 1000",
    );
    ok(core, "t1", "CREATE VIEW s.uv AS SELECT n FROM s.tt");
}

pub(super) fn create_fn(core: &EngineCore, tenant: &str, name: &str, body: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

/// ビュー `view` の INSERT・UPDATE・DELETE 用に、監査表へ書く INSTEAD OF トリガ 3 本を作る。
pub(super) fn audit_triggers(core: &EngineCore, tenant: &str, view: &str) {
    create_fn(
        core,
        tenant,
        "f_ins",
        "BEGIN INSERT INTO audit (tag, n) VALUES ('ins', NEW.n); RETURN NEW; END",
    );
    create_fn(
        core,
        tenant,
        "f_upd",
        "BEGIN INSERT INTO audit (tag, n) VALUES ('upd', NEW.n); RETURN NEW; END",
    );
    create_fn(
        core,
        tenant,
        "f_del",
        "BEGIN INSERT INTO audit (tag, n) VALUES ('del', OLD.n); RETURN OLD; END",
    );
    for (name, ev, f) in [
        ("g_ins", "INSERT", "f_ins"),
        ("g_upd", "UPDATE", "f_upd"),
        ("g_del", "DELETE", "f_del"),
    ] {
        ok(
            core,
            tenant,
            &format!(
                "CREATE TRIGGER {name} INSTEAD OF {ev} ON {view} FOR EACH ROW EXECUTE FUNCTION {f}()"
            ),
        );
    }
}

pub(super) fn audit(core: &EngineCore, tag: &str) -> Vec<i64> {
    ints(
        core,
        "t1",
        &format!("SELECT n FROM audit WHERE tag = '{tag}'"),
    )
}

// (1) 自動更新不可のビュー（LIMIT 付き）でも、INSERT・UPDATE・DELETE がトリガ関数で処理される。
#[test]
fn non_updatable_view_writes_go_through_triggers() {
    let (core, _g) = core_with("instof-basic");
    setup(&core);
    // トリガが無い間は従来どおり `55000`。
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.nv (n) VALUES (1)"),
        "55000"
    );
    audit_triggers(&core, "t1", "s.nv");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (10), (20), (30)");

    assert_eq!(
        count(&core, "t1", "INSERT INTO s.nv (n) VALUES (1), (2)"),
        2
    );
    assert_eq!(audit(&core, "ins"), vec![1, 2]);

    assert_eq!(
        count(&core, "t1", "UPDATE s.nv SET n = n + 1 WHERE n >= 20"),
        2
    );
    assert_eq!(audit(&core, "upd"), vec![21, 31]);

    assert_eq!(count(&core, "t1", "DELETE FROM s.nv WHERE n = 10"), 1);
    assert_eq!(audit(&core, "del"), vec![10]);

    // WHERE なしは全行。
    assert_eq!(count(&core, "t1", "DELETE FROM s.nv"), 3);
    assert_eq!(audit(&core, "del"), vec![10, 10, 20, 30]);

    // 基表はトリガ本体が触れない限り不変。
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![10, 20, 30]);
}
