//! 既定の invoker（本番の PL/pgSQL トリガ実行）で INSERT トリガが発火することの、公開 API だけを使う
//! 結合テスト（TABLE-45・SQL-68・RLS-10。Issue #3185）。注入 invoker を使わず、SQL の
//! `CREATE FUNCTION ... RETURNS trigger LANGUAGE plpgsql` と `CREATE TRIGGER`・`INSERT` だけで通す。
//! 設計記録は `docs/design/plpgsql-subset.md` の「Issue #3185 追記」。網羅的な再検証（RLS 越境・crash）は #3188。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

fn open(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
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

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn cells(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Cell> {
    match ok(core, tenant, sql) {
        SqlOutcome::Query(q) => q
            .rows
            .into_iter()
            .filter_map(|r| r.cells.into_iter().next())
            .collect(),
        other => panic!("expected query, got {other:?}"),
    }
}

fn ints(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = cells(core, tenant, sql)
        .into_iter()
        .map(|c| match c {
            Cell::SignedInteger(v) => v,
            Cell::Integer(v) => i64::try_from(v).expect("fits"),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn texts(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    cells(core, tenant, sql)
        .into_iter()
        .map(|c| match c {
            Cell::Text(s) => s,
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

fn affected(out: SqlOutcome) -> u64 {
    match out {
        SqlOutcome::Insert(o) => o.rows_affected,
        other => panic!("expected insert, got {other:?}"),
    }
}

/// 対象表はテナントの SCHEMA `s` の `s.tt`（共有 public 表へのトリガは定義時に拒否される。Issue #3361）。
/// 監査表 `audit` は public のまま（本体の INSERT は SCHEMA 修飾の書き込み先を受けない）。
fn tables(core: &EngineCore, tenant: &str) {
    ok(core, tenant, "CREATE SCHEMA s");
    ok(core, tenant, "CREATE TABLE s.tt (n INTEGER)");
    ok(
        core,
        tenant,
        "CREATE TABLE audit (n INTEGER, lv TEXT, op TEXT)",
    );
}

fn func(core: &EngineCore, tenant: &str, name: &str, body: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn trigger(core: &EngineCore, tenant: &str, name: &str, spec: &str, f: &str) {
    let (when, level) = spec.split_once(" FOR ").expect("spec has FOR");
    let level = format!("FOR {level}");
    ok(
        core,
        tenant,
        &format!("CREATE TRIGGER {name} {when} ON s.tt {level} EXECUTE FUNCTION {f}()"),
    );
}

const BUMP: &str = "BEGIN NEW.n := NEW.n + 100; RETURN NEW; END";
const STMT_AUDIT: &str =
    "BEGIN INSERT INTO audit (n, lv, op) VALUES (0, TG_LEVEL, TG_OP); RETURN NULL; END";

// BEFORE ROW の NEW 書き換えが複数行 VALUES の全行に効く。末尾 `;` 付きでも同じ。
#[test]
fn before_row_rewrite_fires_with_and_without_semicolon() {
    let (_g, core) = open("t45fire-rewrite");
    tables(&core, "t1");
    func(&core, "t1", "bump", BUMP);
    trigger(&core, "t1", "tb", "BEFORE INSERT FOR EACH ROW", "bump");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (3);");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![101, 102, 103]);
}

// RETURN NULL は行を書かず、影響行数にも含めず、AFTER ROW にも渡さない。
#[test]
fn before_row_null_skips_row_and_after_row() {
    let (_g, core) = open("t45fire-skip");
    tables(&core, "t1");
    func(
        &core,
        "t1",
        "skip2",
        "BEGIN IF NEW.n = 2 THEN RETURN NULL; END IF; RETURN NEW; END",
    );
    func(
        &core,
        "t1",
        "log_after",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NULL; END",
    );
    trigger(&core, "t1", "tb", "BEFORE INSERT FOR EACH ROW", "skip2");
    trigger(&core, "t1", "ta", "AFTER INSERT FOR EACH ROW", "log_after");
    let out = ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2), (3)");
    assert_eq!(affected(out), 2);
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![1, 3]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 3]);
}

// AFTER STATEMENT は文ごとに 1 回だけ、`TG_LEVEL`・`TG_OP` つきで発火する。
#[test]
fn after_statement_fires_once_per_statement() {
    let (_g, core) = open("t45fire-stmt");
    tables(&core, "t1");
    func(&core, "t1", "stmt_audit", STMT_AUDIT);
    trigger(
        &core,
        "t1",
        "ts",
        "AFTER INSERT FOR EACH STATEMENT",
        "stmt_audit",
    );
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2)");
    assert_eq!(
        texts(&core, "t1", "SELECT lv FROM audit"),
        vec!["STATEMENT"]
    );
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (3);");
    assert_eq!(
        texts(&core, "t1", "SELECT op FROM audit"),
        vec!["INSERT", "INSERT"]
    );
}

// 明示 Tx: COMMIT で本表・監査とも残り、ROLLBACK では両方残らない。
#[test]
fn explicit_transaction_commit_and_rollback() {
    let (_g, core) = open("t45fire-txn");
    tables(&core, "t1");
    func(&core, "t1", "stmt_audit", STMT_AUDIT);
    trigger(
        &core,
        "t1",
        "ts",
        "AFTER INSERT FOR EACH STATEMENT",
        "stmt_audit",
    );
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    for sql in ["BEGIN", "INSERT INTO s.tt (n) VALUES (1)", "COMMIT"] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in ["BEGIN", "INSERT INTO s.tt (n) VALUES (2)", "ROLLBACK"] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![1]);
    assert_eq!(texts(&core, "t1", "SELECT lv FROM audit").len(), 1);
}

// 台帳: operation_id つき INSERT の再送は `23505` で、トリガ本体は再実行されない。
// 網羅的な再検証（Disabled 整合・crash）は #3188。
#[test]
fn replay_with_operation_id_does_not_refire() {
    let (_g, core) = open("t45fire-ledger");
    tables(&core, "t1");
    func(&core, "t1", "stmt_audit", STMT_AUDIT);
    trigger(
        &core,
        "t1",
        "ts",
        "AFTER INSERT FOR EACH STATEMENT",
        "stmt_audit",
    );
    let sql = "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-1'";
    ok(&core, "t1", sql);
    assert_eq!(code(&core, "t1", sql), "23505");
    assert_eq!(texts(&core, "t1", "SELECT lv FROM audit").len(), 1);
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![1]);
}

// テナント境界: t1 のトリガは t2 の INSERT で発火せず、t1 の監査行は t2 から見えない。
#[test]
fn trigger_does_not_fire_across_tenants() {
    let (_g, core) = open("t45fire-tenant");
    // `s.tt` はテナントごとの表。t2 は同名・同形の表を持つが、t1 のトリガは t2 の書き込みで発火しない。
    tables(&core, "t1");
    ok(&core, "t2", "CREATE SCHEMA s");
    ok(&core, "t2", "CREATE TABLE s.tt (n INTEGER)");
    func(&core, "t1", "bump", BUMP);
    trigger(&core, "t1", "tb", "BEFORE INSERT FOR EACH ROW", "bump");
    func(&core, "t1", "stmt_audit", STMT_AUDIT);
    trigger(
        &core,
        "t1",
        "ts",
        "AFTER INSERT FOR EACH STATEMENT",
        "stmt_audit",
    );
    ok(&core, "t2", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(ints(&core, "t2", "SELECT n FROM s.tt"), vec![1]);
    assert!(texts(&core, "t1", "SELECT lv FROM audit").is_empty());
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(texts(&core, "t1", "SELECT lv FROM audit").len(), 1);
    assert!(texts(&core, "t2", "SELECT lv FROM audit").is_empty());
}

// 窓口の許可リストに無い本体（`INSERT ... SELECT`）は `0A000` で、本表・監査に残渣を残さない。
// 本体の `DELETE`・表を読む `SELECT` は Issue #3187 で対応済み（`core::plpgsql_trigger_delete_select_tests`）。
#[test]
fn unsupported_body_is_0a000_without_residue() {
    let (_g, core) = open("t45fire-unsupported");
    tables(&core, "t1");
    func(
        &core,
        "t1",
        "upd",
        "BEGIN INSERT INTO audit (n) VALUES (50); INSERT INTO audit (n) SELECT 51; RETURN NEW; END",
    );
    trigger(&core, "t1", "tb", "BEFORE INSERT FOR EACH ROW", "upd");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
        "0A000"
    );
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
}
