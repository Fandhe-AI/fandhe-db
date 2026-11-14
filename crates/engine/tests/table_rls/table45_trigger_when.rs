//! `CREATE TRIGGER ... WHEN (<条件>)` の定義時検査と発火時評価の結合テスト（TABLE-45・SQL-68・RLS-10 の
//! ポインタ。Issue #3190。設計は `docs/design/trigger-execution-model.md` の D7）。
//!
//! 注入 invoker を使わず、SQL の `CREATE FUNCTION ... RETURNS trigger` と `CREATE TRIGGER` だけで定義した
//! 本物のトリガを公開 API から発火させる。対象表は SCHEMA 配下の `s.tt`（共有 public 表へのトリガは拒否）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
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

fn core_at(path: &std::path::Path, mode: LedgerMode) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode)
}

fn open(label: &str, mode: LedgerMode) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path, mode);
    (guard, core)
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), &mut session(), sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    err(core, tenant, sql).wire_code().to_string()
}

fn ints(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    let SqlOutcome::Query(q) = ok(core, tenant, sql) else {
        panic!("expected query for {sql}");
    };
    let mut v: Vec<i64> = q
        .rows
        .into_iter()
        .filter_map(|r| r.cells.into_iter().next())
        .map(|c| match c {
            Cell::SignedInteger(v) => v,
            Cell::Integer(v) => i64::try_from(v).expect("fits"),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

/// 対象表 `s.tt`・監査表 `audit`・カウンタ表 `ctr`（種行 1 件）。
fn tables(core: &EngineCore, tenant: &str) {
    ok(core, tenant, "CREATE SCHEMA s");
    ok(core, tenant, "CREATE TABLE s.tt (n INTEGER)");
    // public 表は名前空間を共有し、行だけがテナントごとに分かれる。2 テナント目以降は作成済みを許す。
    for ddl in [
        "CREATE TABLE audit (n INTEGER)",
        "CREATE TABLE ctr (c INTEGER)",
    ] {
        if let Err(e) = run(core, tenant, ddl) {
            assert_eq!(e.wire_code(), "42P07", "{ddl}: {e}");
        }
    }
    ok(core, tenant, "INSERT INTO ctr (c) VALUES (0)");
}

fn func(core: &EngineCore, tenant: &str, name: &str, body: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn audit_vals(core: &EngineCore, tenant: &str) -> Vec<i64> {
    ints(core, tenant, "SELECT n FROM audit")
}

fn setup(core: &EngineCore, tenant: &str) {
    tables(core, tenant);
    func(
        core,
        tenant,
        "rec",
        "BEGIN INSERT INTO audit (n) VALUES (1); RETURN NEW; END",
    );
}

#[test]
fn after_insert_when_fires_only_for_true_rows() {
    let (_g, core) = open("trgwhen-ai", LedgerMode::Optional);
    setup(&core, "t1");
    ok(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH ROW WHEN (NEW.n > 1) EXECUTE FUNCTION rec()");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2), (3)");
    assert_eq!(audit_vals(&core, "t1").len(), 2);
    // NULL は不発火。
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (NULL)");
    assert_eq!(audit_vals(&core, "t1").len(), 2);
}

#[test]
fn trailing_semicolon_single_statement_form() {
    let (_g, core) = open("trgwhen-semi", LedgerMode::Optional);
    setup(&core, "t1");
    ok(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH ROW WHEN (NEW.n >= 5 AND NEW.n < 9) EXECUTE FUNCTION rec();");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (4), (5), (9);");
    assert_eq!(audit_vals(&core, "t1").len(), 1);
}

#[test]
fn update_and_delete_use_old_and_new() {
    let (_g, core) = open("trgwhen-ud", LedgerMode::Optional);
    setup(&core, "t1");
    func(
        &core,
        "t1",
        "recd",
        "BEGIN INSERT INTO audit (n) VALUES (OLD.n); RETURN OLD; END",
    );
    ok(&core, "t1", "CREATE TRIGGER gu AFTER UPDATE ON s.tt FOR EACH ROW WHEN (NEW.n <> OLD.n) EXECUTE FUNCTION rec()");
    ok(&core, "t1", "CREATE TRIGGER gd AFTER DELETE ON s.tt FOR EACH ROW WHEN (OLD.n = 20) EXECUTE FUNCTION recd()");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (10), (20)");
    ok(&core, "t1", "UPDATE s.tt SET n = n + 1 WHERE n = 10");
    assert_eq!(audit_vals(&core, "t1"), vec![1]);
    ok(&core, "t1", "UPDATE s.tt SET n = n WHERE n = 20");
    assert_eq!(audit_vals(&core, "t1"), vec![1]);
    ok(&core, "t1", "DELETE FROM s.tt");
    assert_eq!(audit_vals(&core, "t1"), vec![1, 20]);
}

#[test]
fn before_when_false_keeps_row_and_true_null_skips() {
    let (_g, core) = open("trgwhen-before", LedgerMode::Optional);
    setup(&core, "t1");
    func(&core, "t1", "skip", "BEGIN RETURN NULL; END");
    ok(&core, "t1", "CREATE TRIGGER g BEFORE INSERT ON s.tt FOR EACH ROW WHEN (NEW.n < 0) EXECUTE FUNCTION skip()");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (-1), (1), (2)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![1, 2]);
}

#[test]
fn before_chain_evaluates_on_rewritten_new() {
    let (_g, core) = open("trgwhen-chain", LedgerMode::Optional);
    setup(&core, "t1");
    func(
        &core,
        "t1",
        "bump",
        "BEGIN NEW.n := NEW.n + 100; RETURN NEW; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER a BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    // 名前順で a の後。書き換え後の NEW（>= 100）で評価される。
    ok(&core, "t1", "CREATE TRIGGER b AFTER INSERT ON s.tt FOR EACH ROW WHEN (NEW.n >= 100) EXECUTE FUNCTION rec()");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(audit_vals(&core, "t1").len(), 1);
}

#[test]
fn update_of_and_when_combine() {
    let (_g, core) = open("trgwhen-updof", LedgerMode::Optional);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (n INTEGER, m INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit (n INTEGER)");
    func(
        &core,
        "t1",
        "rec",
        "BEGIN INSERT INTO audit (n) VALUES (1); RETURN NEW; END",
    );
    ok(&core, "t1", "CREATE TRIGGER g AFTER UPDATE OF n ON s.tt FOR EACH ROW WHEN (NEW.n > 5) EXECUTE FUNCTION rec()");
    ok(&core, "t1", "INSERT INTO s.tt (n, m) VALUES (1, 1)");
    ok(&core, "t1", "UPDATE s.tt SET m = 9");
    assert_eq!(audit_vals(&core, "t1").len(), 0);
    ok(&core, "t1", "UPDATE s.tt SET n = 3");
    assert_eq!(audit_vals(&core, "t1").len(), 0);
    ok(&core, "t1", "UPDATE s.tt SET n = 6");
    assert_eq!(audit_vals(&core, "t1").len(), 1);
}

// 定義時の拒否の固定（サブクエリ・非組み込み関数・揮発性関数・裸の列名・文トリガの WHEN）。
//
// intentional: docs/design/trigger-execution-model.md
// D7 の実装記録のとおり、宣言的な式に限り文トリガの WHEN は受理しない（fail-closed）。
#[test]
fn definition_time_rejections() {
    let (_g, core) = open("trgwhen-rej", LedgerMode::Optional);
    setup(&core, "t1");
    let head = "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH ROW WHEN";
    let tail = "EXECUTE FUNCTION rec()";
    for (cond, want) in [
        ("(NEW.n > (SELECT 1))", "0A000"),
        ("(myudf(NEW.n))", "0A000"),
        ("(now() > NEW.n)", "0A000"),
        ("(nextval('x') > 0)", "0A000"),
        ("(EXISTS (SELECT 1))", "0A000"),
        ("(OLD.n > 1)", "42P17"),
        ("(NEW.nope > 1)", "42703"),
        ("(n > 1)", "0A000"),
        ("()", "42601"),
        ("(NEW.n > 1", "42601"),
        ("(NEW.n > 1; )", "42601"),
        ("(other.n > 1)", "0A000"),
    ] {
        let sql = format!("{head} {cond} {tail}");
        let c = code(&core, "t1", &sql);
        if want == "?" {
            eprintln!("bare column code: {c}");
            assert_ne!(c, "");
        } else {
            assert_eq!(c, want, "{sql}");
        }
    }
    assert_eq!(
        code(&core, "t1", "CREATE TRIGGER g AFTER DELETE ON s.tt FOR EACH ROW WHEN (NEW.n > 1) EXECUTE FUNCTION rec()"),
        "42P17"
    );
    assert_eq!(
        code(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH STATEMENT WHEN (NEW.n > 1) EXECUTE FUNCTION rec()"),
        "42P17"
    );
    assert_eq!(
        code(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH STATEMENT WHEN (true) EXECUTE FUNCTION rec()"),
        "0A000"
    );
    assert_eq!(
        code(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON audit FOR EACH ROW WHEN (NEW.n > 1) EXECUTE FUNCTION rec()"),
        "0A000"
    );
}

#[test]
fn tenant_isolation_of_when_triggers() {
    let (_g, core) = open("trgwhen-tenant", LedgerMode::Optional);
    setup(&core, "t1");
    setup(&core, "t2");
    ok(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH ROW WHEN (NEW.n > 0) EXECUTE FUNCTION rec()");
    ok(&core, "t2", "INSERT INTO s.tt (n) VALUES (5)");
    assert_eq!(audit_vals(&core, "t2").len(), 0);
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (5)");
    assert_eq!(audit_vals(&core, "t1").len(), 1);
}

#[test]
fn when_evaluation_error_aborts_the_statement() {
    let (_g, core) = open("trgwhen-err", LedgerMode::Optional);
    setup(&core, "t1");
    ok(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH ROW WHEN (1 / NEW.n > 0) EXECUTE FUNCTION rec()");
    let c = code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (0)");
    assert_eq!(c, "22012");
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
}

/// 評価器が扱えない形（`IS DISTINCT FROM`）は発火時に `0A000` で文を取り消す（fail-closed。黙って不発火にしない）。
//
// intentional: docs/design/trigger-execution-model.md
// 評価器が扱えない形は黙って不発火にせず、文を取り消す（D7 の実装記録の既知の制限）。
#[test]
fn unsupported_condition_form_fails_closed_at_fire_time() {
    let (_g, core) = open("trgwhen-unsup", LedgerMode::Optional);
    setup(&core, "t1");
    ok(&core, "t1", "CREATE TRIGGER g AFTER INSERT ON s.tt FOR EACH ROW WHEN (NEW.n IS DISTINCT FROM 1) EXECUTE FUNCTION rec()");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)"),
        "0A000"
    );
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
}
